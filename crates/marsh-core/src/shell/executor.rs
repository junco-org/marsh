//! The spawner itself: what the shell hands every external command to.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExternalCommandSpawner, ShellExtensions,
    ShellExtensionsImpl,
};
use brush_core::{Shell, ShellVariable};
use marsh_btrfs::{LibBtrfs, Subvolumes};
use marsh_instrument::{BuiltinHook, BuiltinRecord, SpawnRecord, SpawnRequest};

use super::MarshError;
use super::builtins::SNAPSHOT_ROOT_VAR;
use super::policy::{Principal, escaped_live_principal};
use super::session::{Session, Snapshot};

/// Shell extensions selecting [`MarshExecutor`].
pub type MarshShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter, MarshExecutor>;

/// The shell's spawner, and its handle on the session.
///
/// An [`ExternalCommandSpawner`] that records every external command the shell asks it to start.
/// Attached, it also carries the snapshot — and through it the seed, log and records — that
/// [`Shell`](super::Shell) stages every line in and publishes through.
///
/// There are three states.
///
/// `Default` is the *detached* executor: it delegates to [`DefaultExternalCommandSpawner`] and
/// records nothing, so a shell built with it behaves exactly like a stock one.
///
/// [`Self::open`] is the *seed-level* executor: it holds the seed's lease, its recovered log and
/// nothing else — no snapshot. A `ShellMux` is built over one of these.
///
/// [`Self::snapshot`] is the *attached* executor: the seed-level one plus a snapshot of its own for
/// one principal. A [`Shell`](super::Shell) is built over one of these, and any number of them can
/// run concurrently over a single seed.
///
/// # What it sees
///
/// Only external commands reach a spawner — builtins and shell functions never do. Those are
/// observed instead through the instrumentation [`Shell::attach`](super::Shell::attach) installs,
/// whose records are [`Self::builtin_records`]; the spawn attempts are [`Self::spawn_records`]. The
/// command is spawned exactly as the shell composed it: working directory, environment, file
/// descriptors and process group are untouched. This executor observes, it does not confine.
///
/// # What it publishes, and when
///
/// Publication is [`Shell`](super::Shell)'s: after every line it ran or concluded, once the
/// capability policy granted the line's requests; a denied line is discarded instead, and a line
/// that read something another principal published while it ran is evaluated again against the new
/// seed rather than judged. The snapshot publishes on its own only when it is
/// dropped — finding nothing, normally, because the `Shell`'s own drop concluded the last
/// boundary; whatever a failed final boundary left, otherwise. A publication in the middle of a
/// line is not possible from here, and deliberately so: every stage of a pipeline runs in its own
/// owned shell clone, so a diff taken from inside one could capture another stage's half-written
/// file.
///
/// # What isolation there is
///
/// The shell's working directory: [`Shell::attach`](super::Shell::attach) starts it at
/// [`snapshot_root`](Self::snapshot_root) and exports
/// [`SNAPSHOT_ROOT_VAR`] so the `git` builtin stops its repository search there. A script that
/// `cd`s to an absolute path outside the snapshot writes wherever it went.
#[derive(Clone, Default)]
pub struct MarshExecutor {
    /// The seed's lease and log, or `None` for a detached executor.
    session: Option<Arc<Session>>,
    /// This shell's snapshot, or `None` for a detached or seed-level executor.
    snapshot: Option<Arc<Snapshot>>,
}

impl std::fmt::Debug for MarshExecutor {
    /// Names what the executor is attached to, which is all a caller can act on; the session's
    /// internals are not part of its interface.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MarshExecutor")
            .field("seed", &self.seed())
            .field("uid", &self.uid())
            .field("principal", &self.principal())
            .finish()
    }
}

impl MarshExecutor {
    /// Attaches to the btrfs seed containing `seed`, over real btrfs.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::open_with`] does.
    pub fn open(seed: &Path) -> Result<Self, MarshError> {
        Self::open_with(seed, Arc::new(LibBtrfs))
    }

    /// Attaches to the seed containing `seed`, taking snapshots through `fs`.
    ///
    /// Discovers the seed, takes its exclusive lease and recovers its log; takes no snapshot. Call
    /// [`Self::snapshot`] once per shell for that.
    ///
    /// # Errors
    ///
    /// Fails when no subvolume contains `seed`, when another process holds the seed's lease
    /// ([`marsh_btrfs::Error::SessionBusy`]), or when the log cannot be recovered.
    pub fn open_with(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Self, MarshError> {
        // The canonical starting directory is discovery's other half; a standalone executor is
        // asked only for the seed, and its shell starts at the snapshot root regardless.
        let (persistence, _) = marsh_btrfs::PersistenceLayer::discover(seed, fs.as_ref())?;
        Self::open_discovered(persistence, fs)
    }

    /// The same, for a seed a caller has already discovered.
    ///
    /// A host serving several seeds discovers each one from its own shell's starting directory.
    /// Every session of this process reports to the one shared recorder
    /// ([`marsh_instrument::RecordingHook::shared`]): instrumentation is installed process-wide,
    /// so the last hook installed is the only one that receives anything, and the tracer behind it
    /// is attached to the host rather than to a seed. Obtaining it here rather than taking it as
    /// an argument is what keeps that ownership below every caller — a multiplexer schedules
    /// commands, it does not own the instrumentation they are observed through.
    ///
    /// # Errors
    ///
    /// Fails when another process holds the seed's lease ([`marsh_btrfs::Error::SessionBusy`]),
    /// or when the state directory cannot be materialized or the log recovered.
    pub(crate) fn open_discovered(
        persistence: marsh_btrfs::PersistenceLayer,
        fs: Arc<dyn Subvolumes>,
    ) -> Result<Self, MarshError> {
        Ok(Self {
            session: Some(Session::open(
                persistence,
                fs,
                marsh_instrument::RecordingHook::shared(),
            )?),
            snapshot: None,
        })
    }

    /// A fresh snapshot of the seed for `principal`, as an attached executor.
    ///
    /// Every snapshot runs concurrently with every other one's lines: they share the seed's
    /// authority, which serializes only the boundaries. A detached executor stays detached.
    /// Names here are session-local, not recovery credentials. To retain agent ownership across
    /// reopening, use [`crate::shellmux::ShellId::durable`] when spawning a mux job.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken.
    pub fn snapshot(&self, principal: Principal) -> Result<Self, MarshError> {
        let principal = escaped_live_principal(&principal).unwrap_or(principal);
        self.snapshot_for(Some(principal), None)
    }

    /// The same, with the snapshot's own uid for a principal: what a lone [`Shell`](super::Shell)
    /// gets when nobody named one.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken.
    pub(crate) fn snapshot_as_uid(&self) -> Result<Self, MarshError> {
        self.snapshot_for(None, None)
    }

    /// Shared construction for anonymous shells and already-scoped mux identities.
    pub(crate) fn snapshot_for(
        &self,
        principal: Option<Principal>,
        durable_name: Option<Principal>,
    ) -> Result<Self, MarshError> {
        match &self.session {
            None => Ok(Self::default()),
            Some(session) => Ok(Self {
                session: Some(Arc::clone(session)),
                snapshot: Some(session.snapshot(principal, durable_name)?),
            }),
        }
    }

    /// The tree the session publishes into, when it has one.
    pub fn seed(&self) -> Option<&Path> {
        self.session
            .as_ref()
            .map(|session| session.persistence.seed.as_path())
    }

    /// The snapshot the shell runs inside, when attached.
    ///
    /// One shell's own tree. A seed-level executor has none — see [`Self::snapshot_dir`] for the
    /// directory they all live in.
    pub fn snapshot_root(&self) -> Option<&Path> {
        self.snapshot.as_ref().map(|snapshot| snapshot.path())
    }

    /// The directory every snapshot of this seed is taken into: `<state>/snap`.
    ///
    /// The session's, not one shell's, so a *seed-level* executor has it and
    /// [`Self::snapshot_root`] is `None` — which is exactly the pair a daemon needs. It holds no
    /// snapshot of its own and still has to recognize a path that lies inside one of its jobs',
    /// because a client whose shell has already moved into a pane's snapshot hands over paths
    /// spelled that way.
    ///
    /// Canonicalized to match [`Self::snapshot_root`], which is canonicalized when the snapshot is
    /// taken: a prefix that differed by a symlink would not strip.
    pub fn snapshot_dir(&self) -> Option<PathBuf> {
        self.session.as_ref().map(|session| {
            let snap = session.persistence.snap();
            snap.canonicalize().unwrap_or(snap)
        })
    }

    /// This snapshot's id, when attached.
    pub fn uid(&self) -> Option<&str> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.uid().as_str())
    }

    /// Who the shell over this executor acts as, when attached.
    pub fn principal(&self) -> Option<&Principal> {
        self.snapshot.as_ref().map(|snapshot| snapshot.principal())
    }

    /// Makes a built shell this executor's: it starts at the snapshot root, gains the `git` and
    /// `exec` builtins (over stock `exec`), has every builtin it holds instrumented, and exports
    /// [`SNAPSHOT_ROOT_VAR`]. A detached or seed-level executor leaves the shell exactly as built.
    ///
    /// Call once per shell, after the last builtin has been registered — a registration added
    /// afterwards runs uninstrumented. Any number of attached shells may exist per process over
    /// one seed, on one condition instrumentation imposes: every attached shell must be built with
    /// the same builtin names and the same `SE` — each attach replaces the process-wide
    /// installation, and a shell whose builtin the latest installation does not know fails that
    /// builtin with `marsh-instrument: builtin … is not instrumented`. A shell's records are told
    /// apart by its working directory lying inside its own snapshot.
    ///
    /// Reached only through [`Shell::attach`](super::Shell::attach), so an attached shell is
    /// always a gated one.
    ///
    /// # Errors
    ///
    /// Fails when the shell refuses the working directory or the variable.
    pub(crate) fn attach<SE: ShellExtensions<ExternalCommandSpawner = Self>>(
        &self,
        shell: &mut Shell<SE>,
    ) -> Result<(), brush_core::Error> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        shell.set_working_dir(snapshot.path())?;
        let mut builtins = shell.builtins().clone();
        builtins.extend(super::builtins::all());
        // Before the instrumentation wraps it: the wrapper records the invocation, and what it
        // wraps is the `git` that knows which snapshot it runs in.
        builtins.insert("git".to_string(), super::builtins::managed_registration());
        let hook = Arc::clone(&snapshot.session().hook) as Arc<dyn BuiltinHook>;
        for (name, registration) in marsh_instrument::instrument(builtins, hook) {
            shell.register_builtin(name, registration);
        }
        let mut var = ShellVariable::new(snapshot.path().to_string_lossy().as_ref());
        var.export();
        shell.set_env_global(SNAPSHOT_ROOT_VAR, var)
    }

    /// The snapshot the lines are staged in, when attached.
    pub(crate) const fn attached(&self) -> Option<&Arc<Snapshot>> {
        self.snapshot.as_ref()
    }

    /// Installs this seed's durable capability history into `validator`, once per seed.
    ///
    /// Called wherever an executor is bound to the history its lines will be judged against —
    /// [`Shell::attach`](super::Shell::attach) and `ShellMux::new` — which is strictly before any
    /// of those lines can reach a gate. A detached executor has no seed and therefore no history
    /// to install; it never publishes and never requests.
    pub(crate) fn rehydrate(&self, validator: &std::sync::Mutex<super::PolicyValidator>) {
        if let Some(session) = &self.session {
            session.adopt_into(validator);
        }
    }

    /// Whether an approved publication failed and the seed's log still has to be replayed.
    ///
    /// `false` for a detached executor, which never publishes anything. A seed-level executor
    /// answers for the whole session it leases, not only for a snapshot of its own.
    #[must_use]
    pub fn recovery_required(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.recovery_required())
    }

    /// Every external command this executor was asked to spawn; empty when detached or seed-level.
    #[must_use]
    pub fn spawn_records(&self) -> Vec<SpawnRecord> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.spawns.records())
            .unwrap_or_default()
    }

    /// How many spawn attempts this executor has recorded; 0 when detached or seed-level.
    ///
    /// The mark [`Self::spawned_pids_since`] is taken against, and the reason it exists: a caller
    /// that only needs the count would otherwise clone every record — program, argument vector,
    /// working directory and all — to measure the log.
    pub(crate) fn spawn_record_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.spawns.record_count())
    }

    /// The pids of the processes started since `start`, in the order they were recorded.
    ///
    /// `start` is a [`Self::spawn_record_count`] taken earlier: it indexes every appended record,
    /// failures included. Attempts that started no process contribute nothing, and a repeated pid
    /// is repeated here — this is the spawn log, not a set.
    pub(crate) fn spawned_pids_since(&self, start: usize) -> Vec<u32> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.spawns.spawned_pids_since(start))
            .unwrap_or_default()
    }

    /// Every builtin this shell ran; empty when detached or seed-level.
    #[must_use]
    pub fn builtin_records(&self) -> Vec<BuiltinRecord> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.builtin_records())
            .unwrap_or_default()
    }

    /// A bracket attributing the work inside it to this shell's snapshot.
    ///
    /// `None` when the executor is detached or the host is not tracing: an untraced shell emits
    /// no markers rather than markers nobody can attribute.
    pub(crate) fn scope(&self) -> Option<marsh_instrument::TraceScope> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot.session().hook.scope(Some(snapshot.path()))
    }

    /// Waits until every syscall this shell has issued so far has been decoded.
    ///
    /// Taken before a boundary acquires the seed's authority, never under it: a publication that
    /// waited on the decoder while holding the seed would stall every other principal on work the
    /// decoder may itself be blocked behind.
    ///
    /// # Errors
    ///
    /// Fails when the evidence stream is broken or the decoder cannot keep up.
    pub(crate) fn drain(&self) -> Result<(), MarshError> {
        match &self.snapshot {
            None => Ok(()),
            Some(snapshot) => snapshot.drain_trace(),
        }
    }

    /// Whether this shell's evaluation was told to stop and start over.
    pub(crate) fn interrupted(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.interrupted())
    }

    /// Resolves once this shell's evaluation is told to stop and start over.
    ///
    /// Already-requested interruption resolves immediately, so there is no lost-wakeup window
    /// between a check and a wait; a detached shell never resolves at all.
    pub(crate) async fn interruption(&self) {
        let Some(snapshot) = self.snapshot.clone() else {
            std::future::pending::<()>().await;
            return;
        };
        loop {
            // Registered before the check, so a request landing between them still wakes this.
            let notified = snapshot.resume.notified();
            if snapshot.interrupted() {
                return;
            }
            notified.await;
        }
    }
}

impl ExternalCommandSpawner for MarshExecutor {
    /// Spawns `command` exactly as composed, recording the attempt when attached.
    ///
    /// Refused outright once this evaluation has been told to start over: the line is unwinding,
    /// and a process launched now would do its work into a tree that is about to be retaken and
    /// would then have to be signalled back out of existence.
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        let Some(snapshot) = &self.snapshot else {
            return DefaultExternalCommandSpawner.spawn(command, kill_on_drop);
        };
        if snapshot.interrupted() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the line is being evaluated again",
            ));
        }
        let request = SpawnRequest::of(&command);
        match DefaultExternalCommandSpawner.spawn(command, kill_on_drop) {
            Ok(child) => {
                snapshot.spawns.spawned(request, child.id());
                Ok(child)
            }
            Err(error) => {
                snapshot.spawns.failed(request, &error);
                Err(error)
            }
        }
    }
}

/// The cooperative cancellation flag one logical command's native workers share.
///
/// A *forced stop*, and nothing else. It is set once and never cleared, because a caller that
/// asked for a command to end did not ask for it to be evaluated again — which is what makes it a
/// different thing from the per-evaluation interruption flag on the snapshot.
#[derive(Debug, Default)]
struct Cancellation {
    /// Set once, never cleared.
    requested: AtomicBool,
    /// Wakes every future waiting on the flag.
    signal: tokio::sync::Notify,
}

/// The registered workers of one evaluation, and whether it still admits new ones.
#[derive(Debug, Default)]
struct Registered {
    /// Which evaluation of the logical command these belong to.
    ///
    /// A retained handle from an abandoned evaluation carries the old number, and is refused: its
    /// work would land in a tree the replay has already retaken.
    generation: u64,
    /// Cleared before finalization: a retained handle cannot start work after its verdict.
    open: bool,
    /// Every registered worker, joined before this evaluation ends.
    handles: Vec<tokio::task::JoinHandle<()>>,
    /// Whether this generation's single join has been started.
    joining: bool,
}

/// The native workers one logical command owns, across every evaluation of it.
///
/// A builtin with real blocking work to do — reading a file, draining a FIFO, walking a
/// directory — registers it here, and the run loop joins every registered worker before it
/// concludes, discards or retakes anything. A worker that outlived that would be writing into a
/// tree that has already been reset, which is the whole reason this type exists.
///
/// Cancellation is cooperative: a forced stop *asks*, and a worker that never looks is waited for
/// rather than pre-empted. Nothing here claims to interrupt arbitrary Rust or arbitrary syscalls.
///
/// It lives beside the spawner rather than in the multiplexer above it because a line is
/// evaluated where it is run: a replay has to close admission, join everything the abandoned
/// evaluation started, and only then open a new generation, and none of that is a scheduling
/// decision.
pub(crate) struct Workers {
    /// The runtime the command was admitted on, so a worker started from a foreign thread still
    /// lands on the daemon's own runtime.
    runtime: tokio::runtime::Handle,
    /// The root of this command's own snapshot, and what its workers are attributed to.
    snapshot_root: Option<PathBuf>,
    /// The host's recorder, when this command has a snapshot the tracer knows.
    hook: Option<Arc<marsh_instrument::RecordingHook>>,
    /// The cooperative forced-stop flag.
    cancellation: Cancellation,
    /// The current generation's registrations.
    registered: std::sync::Mutex<Registered>,
    /// The highest generation whose join has completed, and how every finisher learns it.
    ///
    /// Finalization cannot be "drain the list and await it here". Two things break that:
    /// concurrency — a second caller arriving mid-drain finds the list empty and concludes there
    /// is nothing to wait for — and cancellation — the first caller's stack owns the handles, so
    /// dropping that future detaches the very workers the next caller needs to wait for. Either
    /// way someone reclaims a snapshot while a worker is still writing into it.
    ///
    /// So the join happens exactly once per generation, on a task the *runtime* owns, and every
    /// finisher waits on this. The value is the generation that finished rather than a reusable
    /// boolean, so a late finisher of an abandoned evaluation cannot mistake the replay's join for
    /// its own.
    joined: tokio::sync::watch::Sender<Option<u64>>,
}

impl std::fmt::Debug for Workers {
    /// Names what a caller can act on: which tree the workers write into and whether they have
    /// been asked to stop. The runtime handle, the recorder and the registration table are
    /// implementation and are summarized rather than printed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Workers")
            .field("snapshot_root", &self.snapshot_root)
            .field("generation", &self.generation())
            .field("cancelled", &self.cancellation_requested())
            .finish_non_exhaustive()
    }
}

impl Workers {
    /// The workers of one logical command, admitting its first evaluation's.
    pub(crate) fn new(runtime: tokio::runtime::Handle, snapshot_root: Option<PathBuf>) -> Self {
        Self {
            runtime,
            hook: snapshot_root
                .is_some()
                .then(marsh_instrument::RecordingHook::shared),
            snapshot_root,
            cancellation: Cancellation::default(),
            registered: std::sync::Mutex::new(Registered {
                generation: 0,
                open: true,
                handles: Vec::new(),
                joining: false,
            }),
            joined: tokio::sync::watch::channel(None).0,
        }
    }

    /// Which evaluation is currently admitting work.
    pub(crate) fn generation(&self) -> u64 {
        self.locked().generation
    }

    /// Whether `generation` has been superseded by a replay.
    pub(crate) fn superseded(&self, generation: u64) -> bool {
        self.locked().generation != generation
    }

    /// Whether a forced stop has asked this command's work to end.
    pub(crate) fn cancellation_requested(&self) -> bool {
        self.cancellation.requested.load(Ordering::Acquire)
    }

    /// Asks every worker of this command to stop.
    pub(crate) fn request_cancellation(&self) {
        self.cancellation.requested.store(true, Ordering::Release);
        self.cancellation.signal.notify_waiters();
    }

    /// Resolves once a forced stop has been requested.
    pub(crate) async fn cancelled(&self) {
        loop {
            // Registered before the check, so a request landing between them still wakes this.
            let notified = self.cancellation.signal.notified();
            if self.cancellation_requested() {
                return;
            }
            notified.await;
        }
    }

    /// Runs `operation` on a blocking worker this evaluation owns.
    ///
    /// `generation` is the one the caller's handle was taken at. A handle retained across a replay
    /// names the abandoned evaluation and is refused here, because its work would land in a tree
    /// the replay has already retaken.
    ///
    /// # Errors
    ///
    /// Fails with `()` — the caller has the context to name itself in a diagnostic — once this
    /// evaluation has stopped admitting work or has been superseded.
    pub(crate) fn spawn_blocking<F, T>(
        &self,
        generation: u64,
        operation: F,
    ) -> Result<tokio::sync::oneshot::Receiver<T>, ()>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut registered = self.locked();
        if !registered.open || registered.generation != generation {
            return Err(());
        }
        // Allocated here rather than inside the worker: the scope has to name this command's root
        // while the registration still proves that root is live.
        let scope = self
            .hook
            .as_ref()
            .and_then(|hook| hook.scope(self.snapshot_root.as_deref()));
        let handle = self.runtime.spawn_blocking(move || {
            let guard = scope.as_ref().map(marsh_instrument::TraceScope::enter);
            let value = operation();
            drop(guard);
            let _ = sender.send(value);
        });
        registered.handles.push(handle);
        drop(registered);
        Ok(receiver)
    }

    /// Closes admission and joins every worker of the current evaluation.
    ///
    /// Called before a boundary, so no native worker outlives the snapshot it writes into. A
    /// worker that panicked is joined like any other; its failure has already been observed by
    /// whoever held its receiver.
    pub(crate) async fn finish(&self) {
        let generation = {
            let mut registered = self.locked();
            let generation = registered.generation;
        // Exactly one finisher starts the join, on a task the runtime owns. Everyone else — and
        // the starter too — waits on the shared signal below, so cancelling any of them detaches
        // nothing and a later caller never sees an emptied list it did not wait for.
            if !registered.joining {
                registered.joining = true;
                registered.open = false;
                let handles = std::mem::take(&mut registered.handles);
                drop(registered);
                let joined = self.joined.clone();
                self.runtime.spawn(async move {
                    for handle in handles {
                        let _ = handle.await;
                    }
                    // `send_replace`, not `send`. With no receiver yet subscribed — and the
                    // finisher that spawned this only subscribes afterwards — `send` fails AND
                    // leaves the value untouched, so every finisher would then wait forever on a
                    // flag that was never set.
                    let _ = joined.send_replace(Some(generation));
                });
            } else {
                drop(registered);
            }
            generation
        };

        let mut done = self.joined.subscribe();
        loop {
            if done.borrow_and_update().is_some_and(|last| last >= generation) {
                return;
            }
            if done.changed().await.is_err() {
                // The sender is gone, which can only mean the join task was itself lost. There is
                // nothing better to wait for, and hanging here would stall a boundary forever.
                return;
            }
        }
    }

    /// Opens a new generation for a replay of the same logical command.
    ///
    /// Call only after [`Self::finish`] has returned, which is what makes the previous
    /// generation's workers provably gone. A forced stop is never undone by this: admission stays
    /// open, the cancellation flag stays set, and the replay's own workers see it immediately.
    pub(crate) fn reopen(&self) {
        let mut registered = self.locked();
        registered.generation += 1;
        registered.open = true;
        registered.joining = false;
        registered.handles.clear();
        drop(registered);
    }

    /// Takes the registration table, recovering a poisoned lock.
    ///
    /// The guarded code is a push and a swap; a poisoned lock means an unrelated thread died while
    /// registering, and refusing every later worker because of that would strand a boundary.
    fn locked(&self) -> std::sync::MutexGuard<'_, Registered> {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A detached executor stages nothing and records nothing, and the two observations a
    /// signalling caller makes have to answer for it without a snapshot to read: no attempts, and
    /// no pids from any mark, including one past the end of a log that does not exist.
    #[test]
    fn a_detached_executor_reports_no_spawns_from_any_mark() {
        let executor = MarshExecutor::default();

        assert_eq!(executor.spawn_record_count(), 0);
        assert!(executor.spawn_records().is_empty());
        assert!(executor.spawned_pids_since(0).is_empty());
        assert!(executor.spawned_pids_since(7).is_empty());
    }
}
