//! Stage 1: clean work generation, immutable baseline, and owned evaluation resources.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use marsh_instrument::{RootId, Syscall, TraceRun};
use marsh_lib::RecoverPoison as _;

use super::access::{Access, Effects};
use super::completion::{Completion, Finalize};
use super::execution::Run;
use super::session::Session;
use super::{Principal, ShellError};

#[derive(Default)]
pub(super) struct CommandEvidence {
    pub records: Vec<Syscall>,
    /// Every classified effect with its entry order in the run's sequence.
    pub effects: Vec<(u64, Effects)>,
    pub failure: Option<String>,
}

pub(super) struct Snapshot {
    pub session: Arc<Session>,
    /// The shell instance this view belongs to: its storage, ledger and trace identity.
    pub uid: Principal,
    /// The policy principal its commands act as; the uid unless a trusted owner was supplied.
    pub owner: Principal,
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
}

impl Snapshot {
    pub fn new(
        session: Arc<Session>,
        uid: Principal,
        owner: Principal,
    ) -> Result<Arc<Self>, ShellError> {
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
            owner,
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
            }),
        });
        let weak = Arc::downgrade(&snapshot);
        let root = snapshot.session.tracing.register_root(
            snapshot.path(),
            Arc::new(move |run, info| {
                if let Some(snapshot) = weak.upgrade() {
                    snapshot.observe(run, info);
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

    fn observe(&self, run: TraceRun, info: Syscall) {
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
                Ok(effect) => evidence.effects.push((info.entry_order, effect)),
                Err(error) => {
                    evidence.failure.get_or_insert(error);
                }
            }
            evidence.records.push(info);
        }
    }

    /// Records the explicit release of `path` (physical, in this view) at `order` of `run`, the
    /// active command's run. Nothing is read or written; publication orders it among the run's
    /// other effects.
    pub fn release(&self, run: TraceRun, path: &Path, order: u64) -> Result<(), ShellError> {
        let mut state = self.state.lock().recover();
        let SnapshotState {
            access, evidence, ..
        } = &mut *state;
        let Some((_, evidence)) = evidence.as_mut().filter(|(active, _)| *active == run) else {
            return Err(ShellError::infrastructure("release has no active command"));
        };
        let effects = access
            .release(path, &self.path)
            .map_err(ShellError::unsupported)?;
        evidence.effects.push((order, effects));
        drop(state);
        Ok(())
    }

    /// Marks the active command's evidence incomplete, so it is never published.
    pub fn fail_evidence(&self, cause: String) {
        if let Some((_, evidence)) = &mut self.state.lock().recover().evidence {
            evidence.failure.get_or_insert(cause);
        }
    }

    pub fn take_evidence(&self, run: TraceRun) -> Result<CommandEvidence, ShellError> {
        let Some((active, evidence)) = self.state.lock().recover().evidence.take() else {
            return Err(ShellError::infrastructure("missing command evidence"));
        };
        if active != run {
            return Err(ShellError::infrastructure(
                "command evidence identity mismatch",
            ));
        }
        Ok(evidence)
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
