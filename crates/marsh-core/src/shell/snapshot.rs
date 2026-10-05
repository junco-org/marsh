//! Stage 1: clean work generation, immutable baseline, and owned evaluation resources.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use marsh_instrument::{
    ExecCommand, ExecDecision, ExecHooks, InvocationId, RootId, Syscall, TraceRun,
};
use marsh_lib::RecoverPoison as _;

use super::access::{Access, Effects};
use super::builtins::gitcmd::GitAction;
use super::builtins::gitexec::{self, Runner};
use super::completion::{Completion, Finalize};
use super::execution::Run;
use super::session::Session;
use super::{Principal, ShellError};

#[derive(Default)]
pub(super) struct CommandEvidence {
    pub records: Vec<Syscall>,
    pub effects: Vec<(u64, Option<InvocationId>, Effects)>,
    pub git: Vec<GitEffectRecord>,
    pub failure: Option<String>,
    /// Git invocations admitted and not yet finalized.
    git_active: HashSet<InvocationId>,
    git_exclusive: bool,
}

pub(super) struct Snapshot {
    pub session: Arc<Session>,
    pub uid: Principal,
    path: PathBuf,
    root: OnceLock<RootId>,
    pub retained: AtomicBool,
    closed: AtomicBool,
    reclaimed: AtomicBool,
    deferred: Mutex<Vec<PathBuf>>,
    pub state: Mutex<SnapshotState>,
}

pub(super) struct SnapshotState {
    pub tree_seq: super::session::TreeVersion,
    /// The source's direct epoch this generation was taken at.
    epoch: u64,
    pub dirty: bool,
    access: Access,
    pub evidence: Option<(TraceRun, CommandEvidence)>,
    /// External git invocations of the active command, between their exec and their end.
    runners: HashMap<InvocationId, Runner>,
}

impl Snapshot {
    pub fn new(session: Arc<Session>, uid: Principal) -> Result<Arc<Self>, ShellError> {
        session.check()?;
        let path = session.persistence.work(uid.as_str());
        let authority = session.validator.read();
        let tree_seq = authority.tree_seq;
        let epoch = session.epoch.load(Ordering::Acquire);
        let internal = session.tracing.internal_scope()?;
        let _guard = internal.enter();
        session.fs.snapshot(&session.persistence.seed, &path)?;
        let path = path.canonicalize()?;
        drop(authority);
        let snapshot = Arc::new(Self {
            session,
            uid,
            path,
            root: OnceLock::new(),
            retained: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
            state: Mutex::new(SnapshotState {
                tree_seq,
                epoch,
                dirty: false,
                access: Access::default(),
                evidence: None,
                runners: HashMap::new(),
            }),
        });
        let weak = Arc::downgrade(&snapshot);
        let (select, begin, end) = (weak.clone(), weak.clone(), weak.clone());
        let exec = ExecHooks {
            select: Arc::new(move |run, owner, path| {
                select
                    .upgrade()
                    .is_some_and(|snapshot| snapshot.selects_git(run, owner, path))
            }),
            begin: Arc::new(move |run, _, command| match begin.upgrade() {
                Some(snapshot) => snapshot.begin_external(run, command),
                None => Ok(ExecDecision::Continue),
            }),
            end: Arc::new(move |run, invocation, status| {
                if let Some(snapshot) = end.upgrade() {
                    snapshot.end_external(run, invocation, status);
                }
                Ok(())
            }),
        };
        let root = snapshot.session.tracing.register_root(
            snapshot.path(),
            Arc::new(move |run, builtin, info| {
                if let Some(snapshot) = weak.upgrade() {
                    snapshot.observe(run, builtin, info);
                }
                Ok(())
            }),
            Some(exec),
        )?;
        snapshot
            .root
            .set(root)
            .map_err(|_| ShellError::infrastructure("snapshot registered twice"))?;
        snapshot
            .session
            .snapshots
            .lock()
            .recover()
            .insert(snapshot.path.clone(), Arc::downgrade(&snapshot));
        Ok(snapshot)
    }

    pub fn close(&self) -> Result<(), ShellError> {
        if self.reclaimed.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self.closed.load(Ordering::Acquire)
            && let Some(root) = self.root.get()
        {
            if let Err(error) = self.session.tracing.unregister_root(*root) {
                self.retained.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
        self.closed.store(true, Ordering::Release);
        if self.retained.load(Ordering::Acquire) {
            return Err(ShellError::infrastructure(
                "command resources retained because recovery or quiescence is required",
            ));
        }
        let internal = self.session.tracing.internal_scope().ok();
        let _scope = internal.as_ref().map(marsh_instrument::TraceScope::enter);
        let mut pending = std::mem::take(&mut *self.deferred.lock().recover());
        if !pending.contains(&self.path) {
            pending.push(self.path.clone());
        }
        let mut failed = Vec::new();
        let mut failure = None;
        for path in pending {
            if let Err(error) = self.session.fs.delete_subvolume(&path) {
                failed.push(path);
                failure.get_or_insert(error);
            }
        }
        *self.deferred.lock().recover() = failed;
        if let Some(error) = failure {
            return Err(error.into());
        }
        self.reclaimed.store(true, Ordering::Release);
        Ok(())
    }
    pub fn reclaim(&self, path: &Path) -> Result<(), ShellError> {
        let internal = self.session.tracing.internal_scope().ok();
        let _scope = internal.as_ref().map(marsh_instrument::TraceScope::enter);
        if let Err(error) = self.session.fs.delete_subvolume(path) {
            let mut deferred = self.deferred.lock().recover();
            if !deferred.iter().any(|held| held == path) {
                deferred.push(path.to_path_buf());
            }
            drop(deferred);
            return Err(error.into());
        }
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn root(&self) -> Result<RootId, ShellError> {
        self.root
            .get()
            .copied()
            .ok_or_else(|| ShellError::infrastructure("snapshot registration incomplete"))
    }
    pub fn logical(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.path).map_or_else(
            |_| path.to_path_buf(),
            |relative| self.session.persistence.seed.join(relative),
        )
    }
    pub fn physical(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.session.persistence.seed)
            .map_or_else(|_| path.to_path_buf(), |relative| self.path.join(relative))
    }

    fn observe(&self, run: TraceRun, builtin: Option<InvocationId>, info: Syscall) {
        let mut state = self.state.lock().recover();
        if state
            .evidence
            .as_ref()
            .is_none_or(|(active, _)| *active != run)
        {
            return;
        }
        let effect = state.access.observe(&info, &self.path);
        drop(state);
        let foreign = effect
            .as_ref()
            .is_ok_and(|effect| Session::invalidate_foreign(&self.path, &effect.outside_writes));
        let mut state = self.state.lock().recover();
        if let Some((_, evidence)) = &mut state.evidence {
            if foreign {
                evidence.failure.get_or_insert_with(|| {
                    "command wrote into another shell's private work view".into()
                });
            }
            match effect {
                Ok(effect) => evidence.effects.push((info.entry_order, builtin, effect)),
                Err(error) => {
                    evidence.failure.get_or_insert(error);
                }
            }
            evidence.records.push(info);
        }
    }

    /// Marks the active command's evidence incomplete, so it is never published.
    pub fn fail_evidence(&self, cause: String) {
        if let Some((_, evidence)) = &mut self.state.lock().recover().evidence {
            evidence.failure.get_or_insert(cause);
        }
    }

    pub fn take_evidence(&self, run: TraceRun) -> Result<CommandEvidence, ShellError> {
        // Declared first so they drop after the lock on every path: their guards relock it.
        let stale;
        let mut state = self.state.lock().recover();
        stale = std::mem::take(&mut state.runners);
        let Some((active, mut evidence)) = state.evidence.take() else {
            return Err(ShellError::infrastructure("missing command evidence"));
        };
        if active != run {
            return Err(ShellError::infrastructure(
                "command evidence identity mismatch",
            ));
        }
        if !evidence.git_active.is_empty() || !stale.is_empty() {
            evidence
                .failure
                .get_or_insert_with(|| "git invocation has not completed".into());
        }
        drop(state);
        // Their guards settle against the snapshot's state, which is no longer locked here.
        drop(stale);
        for git in &mut evidence.git {
            let (first, last) = self
                .session
                .tracing
                .invocation_orders(run, git.invocation)?;
            git.started_order = first;
            git.finished_order = last;
        }
        Ok(evidence)
    }

    pub fn writes_for(&self, invocation: InvocationId) -> Vec<PathBuf> {
        let state = self.state.lock().recover();
        let mut paths = std::collections::BTreeSet::new();
        if let Some((_, evidence)) = &state.evidence {
            for (_, owner, effects) in &evidence.effects {
                if *owner == Some(invocation) {
                    paths.extend(effects.writes.iter().cloned());
                }
            }
        }
        drop(state);
        paths.into_iter().collect()
    }
    /// Admits `invocation` of `run` as a git invocation of `kind`; an exclusive one runs alone.
    pub(crate) fn begin_git(
        self: &Arc<Self>,
        run: TraceRun,
        invocation: InvocationId,
        kind: GitCohortKind,
    ) -> Result<Completion<GitGuard>, String> {
        let mut state = self.state.lock().recover();
        let (_, evidence) = state
            .evidence
            .as_mut()
            .filter(|(active, _)| *active == run)
            .ok_or_else(|| "git has no owning run".to_string())?;
        if !evidence.git_active.is_empty()
            && (evidence.git_exclusive || kind == GitCohortKind::Exclusive)
        {
            evidence
                .failure
                .get_or_insert_with(|| "unordered overlapping git invocations".into());
            return Err("git operation overlaps another invocation".into());
        }
        evidence.git_exclusive = kind == GitCohortKind::Exclusive;
        evidence.git_active.insert(invocation);
        drop(state);
        Ok(Completion::new(GitGuard {
            snapshot: Arc::downgrade(self),
            run,
            invocation,
        }))
    }

    /// Runs `update` on the evidence of `run`, when it is still the active command's.
    fn with_evidence(&self, run: TraceRun, update: impl FnOnce(&mut CommandEvidence)) {
        if let Some((active, evidence)) = &mut self.state.lock().recover().evidence
            && *active == run
        {
            update(evidence);
        }
    }

    /// Whether an exec of `path` by `owner` in `run` is a git invocation the active command must
    /// observe: any executable named `git`, unless it already belongs to an observed git
    /// invocation — its own helpers and probes.
    fn selects_git(&self, run: TraceRun, owner: Option<InvocationId>, path: &Path) -> bool {
        if path.file_name().is_none_or(|name| name != "git") {
            return false;
        }
        let state = self.state.lock().recover();
        state.evidence.as_ref().is_some_and(|(active, evidence)| {
            *active == run && owner.is_none_or(|owner| !evidence.git_active.contains(&owner))
        })
    }

    /// Admits an external git exactly as the builtin is admitted, before its image runs: its
    /// probes reproduce its own program, directory and environment. A refusal latches its
    /// diagnostic, so nothing of the command publishes, and the process is ended.
    fn begin_external(
        self: &Arc<Self>,
        run: TraceRun,
        command: ExecCommand,
    ) -> std::io::Result<ExecDecision> {
        let tracing = Arc::clone(&self.session.tracing);
        let invocation = tracing.invocation(run)?;
        let (argv, template) = match gitexec::external(command) {
            Ok(external) => external,
            Err(refusal) => {
                self.with_evidence(run, |evidence| {
                    evidence.failure.get_or_insert_with(|| refusal.into());
                });
                return Ok(ExecDecision::Refuse);
            }
        };
        let runner = match Runner::new(argv).prepare(template, self, tracing, run, invocation) {
            Ok(runner) => runner,
            Err(refusal) => {
                self.with_evidence(run, |evidence| {
                    evidence.failure.get_or_insert(refusal);
                });
                return Ok(ExecDecision::Refuse);
            }
        };
        // A displaced runner settles only after the lock is released.
        let displaced = self
            .state
            .lock()
            .recover()
            .runners
            .insert(invocation, runner);
        drop(displaced);
        Ok(ExecDecision::Track(invocation))
    }

    /// Attributes an external git whose last process ended, with its own status; without one,
    /// its observation is incomplete and it never publishes.
    fn end_external(&self, run: TraceRun, invocation: InvocationId, status: Option<ExitStatus>) {
        let runner = self.state.lock().recover().runners.remove(&invocation);
        let Some(runner) = runner else {
            self.with_evidence(run, |evidence| {
                evidence
                    .failure
                    .get_or_insert_with(|| "git invocation did not finish observation".into());
            });
            return;
        };
        match status {
            Some(status) => {
                if let Err(failure) = runner.finish(status.success(), false) {
                    self.with_evidence(run, |evidence| {
                        evidence.failure.get_or_insert(failure);
                    });
                }
            }
            None => drop(runner),
        }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Clone, Copy)]
pub(super) struct CommandNumber(pub(super) u64);
impl std::fmt::Display for CommandNumber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Owned command baseline; its finalizer never publishes and leaves refused work dirty for reset.
pub(super) struct PreparedCommand {
    pub snapshot: Arc<Snapshot>,
    pub baseline: PathBuf,
    pub run: Arc<Run>,
    pub tree_seq: super::session::TreeVersion,
    pub number: CommandNumber,
}

/// Refreshes a stale work generation, freezes its baseline and begins the traced run. `cwd` is
/// the span's logical initial directory.
pub(super) fn prepare(
    snapshot: &Arc<Snapshot>,
    runtime: tokio::runtime::Handle,
    number: CommandNumber,
    cwd: Arc<PathBuf>,
) -> Result<Completion<PreparedCommand>, ShellError> {
    let session = &snapshot.session;
    session.check()?;
    let scope = session.tracing.internal_scope()?;
    let _guard = scope.enter();
    let authority = session.validator.read();
    let epoch = session.epoch.load(Ordering::Acquire);
    let mut state = snapshot.state.lock().recover();
    let retake = state.dirty || state.tree_seq != authority.tree_seq || state.epoch != epoch;
    if retake {
        session.fs.delete_subvolume(snapshot.path())?;
        session
            .fs
            .snapshot(&session.persistence.seed, snapshot.path())?;
        state.tree_seq = authority.tree_seq;
        state.epoch = epoch;
        state.dirty = false;
    }
    state.access.prepare(snapshot.path(), retake)?;
    let baseline = session
        .persistence
        .snap()
        .join(format!("{}-base-{number}", snapshot.uid));
    session.fs.snapshot_readonly(snapshot.path(), &baseline)?;
    let tree_seq = authority.tree_seq;
    let trace = match session.tracing.begin_run(snapshot.root()?) {
        Ok(run) => run,
        Err(error) => {
            session.fs.delete_subvolume(&baseline)?;
            return Err(error.into());
        }
    };
    state.evidence = Some((trace, CommandEvidence::default()));
    state.dirty = true;
    drop(state);
    drop(authority);
    let run = Arc::new(Run::managed(
        runtime,
        Arc::clone(&session.tracing),
        trace,
        Arc::downgrade(snapshot),
        cwd,
    ));
    Ok(Completion::new(PreparedCommand {
        snapshot: Arc::clone(snapshot),
        baseline,
        run,
        tree_seq,
        number,
    }))
}

impl Completion<PreparedCommand> {
    /// Reclaims the baseline once; a refused reclaim is not retried by the finalizer.
    pub fn reclaim(&mut self) -> Result<(), ShellError> {
        self.completed = true;
        self.payload.snapshot.reclaim(&self.payload.baseline)
    }
}
impl Finalize for PreparedCommand {
    fn finalize(&mut self, completed: bool) {
        if !completed {
            let _ = self.snapshot.reclaim(&self.baseline);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitEffectRecord {
    pub invocation: InvocationId,
    pub started_order: u64,
    pub finished_order: u64,
    pub requests: Vec<(GitAction, PathBuf)>,
    pub metadata: Vec<PathBuf>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitCohortKind {
    Inspect,
    Exclusive,
}

/// One admitted git invocation of one run. It holds its snapshot weakly, and settles nothing
/// once that run's evidence has been taken.
pub(super) struct GitGuard {
    snapshot: Weak<Snapshot>,
    run: TraceRun,
    invocation: InvocationId,
}
impl Completion<GitGuard> {
    pub fn record(mut self, record: GitEffectRecord) {
        if let Some(snapshot) = self.payload.snapshot.upgrade() {
            snapshot.with_evidence(self.payload.run, |evidence| evidence.git.push(record));
        }
        self.completed = true;
    }
}
impl GitGuard {
    pub fn fail(&self, message: String) {
        if let Some(snapshot) = self.snapshot.upgrade() {
            snapshot.with_evidence(self.run, |evidence| {
                evidence.failure.get_or_insert(message);
            });
        }
    }
}
impl Finalize for GitGuard {
    fn finalize(&mut self, completed: bool) {
        let Some(snapshot) = self.snapshot.upgrade() else {
            return;
        };
        snapshot.with_evidence(self.run, |evidence| {
            evidence.git_active.remove(&self.invocation);
            if !completed {
                evidence
                    .failure
                    .get_or_insert_with(|| "git invocation did not finish observation".into());
            }
        });
    }
}
