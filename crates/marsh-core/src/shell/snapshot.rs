//! Stage 1: clean work generation, immutable baseline, and owned evaluation resources.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use marsh_instrument::{InvocationId, RootId, Syscall, TraceRun};

use super::access::{Access, Effects};
use super::builtins::gitcmd::GitAction;
use super::execution::Run;
use super::session::{Session, fresh_principal};
use super::{Principal, ShellError, ShellErrorKind};

#[derive(Default)]
pub(super) struct CommandEvidence {
    pub records: Vec<Syscall>,
    pub effects: Vec<(u64, Option<InvocationId>, Effects)>,
    pub git: Vec<GitEffectRecord>,
    pub failure: Option<String>,
    git_live: usize,
    git_exclusive: bool,
}

pub(super) struct Snapshot {
    pub session: Arc<Session>,
    pub uid: Principal,
    path: PathBuf,
    root: OnceLock<RootId>,
    serial: AtomicU64,
    pub retained: AtomicBool,
    closed: AtomicBool,
    reclaimed: AtomicBool,
    deferred: Mutex<Vec<PathBuf>>,
    pub current: Mutex<Weak<Run>>,
    pub state: Mutex<SnapshotState>,
}

pub(super) struct SnapshotState {
    pub tree_seq: super::session::TreeVersion,
    pub dirty: bool,
    access: Access,
    pub evidence: Option<(TraceRun, CommandEvidence)>,
}

impl Snapshot {
    pub fn new(session: Arc<Session>) -> Result<Arc<Self>, ShellError> {
        session.check()?;
        let uid = fresh_principal()?;
        let path = session.persistence.work(uid.as_str());
        let authority = session
            .authority
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let tree_seq = authority.tree_seq;
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
            serial: AtomicU64::new(1),
            retained: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            current: Mutex::new(Weak::new()),
            reclaimed: AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
            state: Mutex::new(SnapshotState {
                tree_seq,
                dirty: false,
                access: Access::default(),
                evidence: None,
            }),
        });
        let weak = Arc::downgrade(&snapshot);
        let root = snapshot.session.tracing.register_root(
            snapshot.path(),
            Arc::new(move |run, builtin, info| {
                if let Some(snapshot) = weak.upgrade() {
                    snapshot.observe(run, builtin, info);
                }
                Ok(())
            }),
        )?;
        snapshot
            .root
            .set(root)
            .map_err(|_| ShellError::infrastructure("snapshot registered twice"))?;
        snapshot
            .session
            .snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
        let mut pending =
            std::mem::take(&mut *self.deferred.lock().unwrap_or_else(PoisonError::into_inner));
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
        *self.deferred.lock().unwrap_or_else(PoisonError::into_inner) = failed;
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
            let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
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
    pub fn active(&self) -> Result<Arc<Run>, ShellError> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))
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
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
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
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
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
            if builtin.is_none()
                && matches!(
                    info.info.syscall,
                    syscalls::Sysno::execve | syscalls::Sysno::execveat
                )
            {
                let index = usize::from(info.info.syscall == syscalls::Sysno::execveat);
                if let Some(path) = info.path(index) {
                    use std::os::unix::ffi::OsStrExt;
                    if Path::new(std::ffi::OsStr::from_bytes(path))
                        .file_name()
                        .is_some_and(|name| name == "git")
                    {
                        evidence.failure.get_or_insert_with(|| {
                            "external git must use the managed builtin".into()
                        });
                    }
                }
            }
            evidence.records.push(info);
        }
    }

    pub fn take_evidence(&self, run: TraceRun) -> Result<CommandEvidence, ShellError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((active, mut evidence)) = state.evidence.take() else {
            return Err(ShellError::infrastructure("missing command evidence"));
        };
        if active != run {
            return Err(ShellError::infrastructure(
                "command evidence identity mismatch",
            ));
        }
        if evidence.git_live != 0 {
            evidence
                .failure
                .get_or_insert_with(|| "git invocation has not completed".into());
        }
        drop(state);
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

    pub fn drain_trace(&self) -> Result<(), ShellError> {
        self.session.tracing.drain(self.active()?.trace)?;
        Ok(())
    }
    pub fn writes_for(&self, invocation: InvocationId) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
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
    pub fn begin_git(self: &Arc<Self>, kind: GitCohortKind) -> Result<GitGuard, String> {
        let context = super::execution::current_context()
            .ok_or_else(|| "git has no owning command context".to_string())?;
        let invocation = context
            .invocation()
            .ok_or_else(|| "git has no builtin invocation".to_string())?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, evidence) = state
            .evidence
            .as_mut()
            .ok_or_else(|| "git has no owning run".to_string())?;
        if evidence.git_live != 0 && (evidence.git_exclusive || kind == GitCohortKind::Exclusive) {
            evidence
                .failure
                .get_or_insert_with(|| "unordered overlapping git invocations".into());
            return Err("git operation overlaps another invocation".into());
        }
        evidence.git_exclusive = kind == GitCohortKind::Exclusive;
        evidence.git_live += 1;
        drop(state);
        Ok(GitGuard {
            snapshot: Arc::clone(self),
            invocation,
            completed: false,
        })
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Clone, Copy)]
pub(super) struct CommandNumber(u64);
impl std::fmt::Display for CommandNumber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Owned command baseline; its Drop never publishes and leaves refused work dirty for reset.
pub(super) struct PreparedCommand {
    pub snapshot: Arc<Snapshot>,
    pub baseline: PathBuf,
    pub run: Arc<Run>,
    pub tree_seq: super::session::TreeVersion,
    pub number: CommandNumber,
    pub settled: bool,
}

pub(super) fn prepare(
    session: &Arc<Session>,
    snapshot: &Arc<Snapshot>,
    runtime: tokio::runtime::Handle,
) -> Result<PreparedCommand, ShellError> {
    session.check()?;
    let scope = session.tracing.internal_scope()?;
    let _guard = scope.enter();
    let authority = session
        .authority
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    let mut state = snapshot
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let retake = state.dirty || state.tree_seq != authority.tree_seq;
    if retake {
        session.fs.delete_subvolume(snapshot.path())?;
        session
            .fs
            .snapshot(&session.persistence.seed, snapshot.path())?;
        state.tree_seq = authority.tree_seq;
        state.dirty = false;
    }
    state.access.prepare(snapshot.path(), retake)?;
    let number = CommandNumber(
        snapshot
            .serial
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |number| {
                number.checked_add(1)
            })
            .map_err(|_| ShellError::infrastructure("command identity exhaustion"))?,
    );
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
    let run = Arc::new(Run::new(runtime, Arc::clone(&session.tracing), trace));
    *snapshot
        .current
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&run);
    Ok(PreparedCommand {
        snapshot: Arc::clone(snapshot),
        baseline,
        run,
        tree_seq,
        number,
        settled: false,
    })
}

impl PreparedCommand {
    pub fn reclaim(&mut self) -> Result<(), ShellError> {
        self.settled = true;
        self.snapshot.reclaim(&self.baseline)
    }
}
impl Drop for PreparedCommand {
    fn drop(&mut self) {
        if !self.settled {
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

pub(super) struct GitGuard {
    snapshot: Arc<Snapshot>,
    pub invocation: InvocationId,
    completed: bool,
}
impl GitGuard {
    pub fn record(mut self, record: GitEffectRecord) {
        if let Some((_, evidence)) = &mut self
            .snapshot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .evidence
        {
            evidence.git.push(record);
        }
        self.completed = true;
    }
    pub fn finish(mut self) {
        self.completed = true;
    }
    pub fn fail(&self, message: String) {
        if let Some((_, evidence)) = &mut self
            .snapshot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .evidence
        {
            evidence.failure.get_or_insert(message);
        }
    }
}
impl Drop for GitGuard {
    fn drop(&mut self) {
        let mut state = self
            .snapshot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((_, evidence)) = &mut state.evidence {
            evidence.git_live = evidence.git_live.saturating_sub(1);
            if !self.completed {
                evidence
                    .failure
                    .get_or_insert_with(|| "git invocation did not finish observation".into());
            }
        }
    }
}
