//! The shell collection: the shells a front-end has open, the commands running in them, the
//! streams they own, and the principals they answer to.
//!
//! A shell is a *sandbox*, not a command: it outlives the commands run in it, and its [`ShellId`]
//! is the principal those commands request capabilities as. That is why the collection lives here
//! rather than in a front-end: a shell's name and a principal's name are one identity, and two
//! registries of it would drift. The registry is therefore keyed by [`Principal`] directly, with
//! one insertion-order list beside it so creation order survives.
//!
//! Every shell owns its standard descriptors from the moment it is created — a pseudoterminal, or
//! three real pipes — so a front-end reads bytes rather than sharing the process's own terminal,
//! and a full-screen program behaves as it would under any other shell.
//!
//! One command at a time per shell. No registry or live-state guard is held across a launch, a
//! line, a callback or a reclamation, so [`ShellMux::snapshot`] answers while a command is
//! starting and while another is running.
//!
//! # Identity, and why a retained handle is safe
//!
//! A [`Shell`] is the *object*, not a name: it owns the live state of one generation of one
//! principal outright, so an action performed through it can never reach the shell that later
//! took its name. A generation whose resources have been reclaimed holds `None` in its live slot
//! and answers [`MuxError::StaleJob`]; there is no second lookup by name anywhere in this module
//! that could resolve to a replacement.
//!
//! # Lock order
//!
//! Registry → shell live state → seed registry, whenever more than one is needed. A single
//! shell's own operations lock only their live state, and release it before touching the registry.

use std::collections::HashMap;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tokio::io::unix::AsyncFd;

use crate::policy::Principal;
use marsh_lib::{WaitState, wait_for_completion};

use crate::shellmux::command::{
    CommandCompletion, CommandHandle, CommandId, PolicyError, RunError, WaitError,
};
use crate::shellmux::context::{CommandContext, with_context};
use crate::shellmux::error::MuxError;
use crate::shellmux::frontend::{FrontendEvent, ShellFrontend, notify};
use crate::shellmux::idle::{IdleTerminal, LeaseRefusal, LeaseState, Revocation};
use crate::shellmux::ids::ShellId;
use crate::shellmux::mux::{Sandbox, ShellMux, kill_since};
use crate::shellmux::pipes::{PipeInput, read_pipe};
use crate::shellmux::types::{
    CommandOptions, JobIo, OutputChannel, SpawnOptions, TerminalGeometry,
};
use crate::{MarshError, Outcome};

/// How many bytes one output read takes at most.
const CHUNK: usize = 8192;

/// Why a shell is to close once its command finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobCloseMode {
    /// A shell opened with [`SpawnOptions::automatic_close`]; a reader may keep it.
    Automatic,
    /// Close after normal command completion, because a reader or the command's own options asked.
    Graceful,
    /// Abort and retire the shell immediately.
    Force,
}

impl JobCloseMode {
    /// Whether this closure is a decision rather than the `1`, `2`, … series' default.
    const fn explicit(self) -> bool {
        !matches!(self, Self::Automatic)
    }
}

/// How one shell ended.
///
/// Delivered once per shell, after every byte of every one of its streams and after its snapshot
/// has been reclaimed. A shell whose construction failed reports that failure here without ever
/// having been [`Opened`](FrontendEvent::Opened).
#[derive(Debug)]
pub struct JobEnd {
    /// The shell that ended.
    pub shell: Sandbox,
    /// Why it closed, or `None` when its streams simply ended.
    pub close_mode: Option<JobCloseMode>,
    /// The verdict of the last command that ran in it, when one did.
    pub completion: Option<Arc<CommandCompletion>>,
    /// The infrastructure failure that ended it, when one did.
    ///
    /// A construction that never produced a usable shell reports here. This is not an exit status
    /// and must never be rendered as one.
    pub error: Option<Arc<MuxError>>,
    /// Whether the session is now waiting for a write-ahead log replay.
    pub recovery_required: bool,
}

/// The endpoints a shell keeps, so input still reaches a shell whose public row is gone.
#[derive(Debug)]
enum Endpoints {
    /// A pseudoterminal: one master, and the lease state an idle prompt shares with the mux.
    Terminal {
        /// Master side, shared with the pump that drains it.
        master: Arc<AsyncFd<OwnedFd>>,
        /// Revocation state of the idle-terminal lease, if one is ever taken.
        lease: Arc<LeaseState>,
        /// The slave's path, cached at open rather than re-derived from a second descriptor.
        tty_path: Option<PathBuf>,
    },
    /// Three pipes: only the input end outlives the row, because the output ends belong to pumps.
    Pipes {
        /// Write end of standard input, shared and idempotently closeable.
        input: Arc<PipeInput>,
    },
}

/// The stable half of one shell: its identity, its endpoints, and the live state it owns.
///
/// Everything here outlives the shell's resources. [`Self::live`] is the generation's mutable
/// half, and taking it out *is* the reclamation: the executor, the validator, the producer
/// descriptors and the private gated evaluator all go with it, even while callers still hold
/// [`Shell`] clones.
struct ShellState {
    /// The shell's identity.
    id: ShellId,
    /// Its sandbox: identity, directory label and snapshot id.
    sandbox: Sandbox,
    /// Whether it is a terminal or a pipe shell. Fixed at admission.
    terminal: bool,
    /// The collection that admitted it. Weak, because a shell must not keep a torn-down mux
    /// alive; an action needing the collection answers [`MuxError::ShuttingDown`] once it is gone.
    origin: Weak<ShellMux>,
    /// Filled once the shell's streams and interpreter are open; absent while still constructing.
    endpoints: OnceLock<Endpoints>,
    /// The closure watch's producer, so a failed construction, a reclamation and a teardown can
    /// all resolve it without a lifecycle task.
    closed_tx: Arc<tokio::sync::watch::Sender<WaitState<JobEnd, WaitError>>>,
    /// The shell's closure.
    closed: tokio::sync::watch::Receiver<WaitState<JobEnd, WaitError>>,
    /// Set once a forced stop retires the shell, so its output readers close.
    ///
    /// A discarded shell has no consumer left for its bytes, and draining them anyway is what
    /// keeps an unstoppable producer alive: a line that writes faster than it can be cancelled —
    /// a builtin, which runs inline on the runtime and cannot be signalled — is only ended by its
    /// own descriptors going away. Closing the read ends turns its next write into `EPIPE`, which
    /// is a real error the interpreter propagates, so the command finishes and the worker thread
    /// it occupied comes back.
    discarded: Arc<tokio::sync::watch::Sender<bool>>,
    /// This generation's live resources and lifecycle, or `None` once it has been reclaimed.
    live: Mutex<Option<LiveShell>>,
}

impl std::fmt::Debug for ShellState {
    /// Names the sandbox and nothing else.
    ///
    /// Never the live resources: the interpreter carries the caller's environment — frequently
    /// credentials — and the mux's own debug output deliberately excludes profile variables for
    /// exactly that reason. The public handle never exposed them and must not start now.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Shell")
            .field("id", &self.sandbox.id)
            .field("uid", &self.sandbox.uid)
            .finish_non_exhaustive()
    }
}

/// One open shell: the object a collection hands out, and the thing commands run through.
///
/// Allocated when the shell is *admitted*, before its terminal or pipes finish opening, so a
/// caller never has to wait for construction to have one. Cloning shares the shell; it does not
/// duplicate its output, because a shell's bytes are the mux's to pump and they reach exactly one
/// frontend. Dropping a clone stops nothing: [`Shell::stop`] is how one ends.
///
/// Bound to one generation of one name. A collection that has since reused the name cannot be
/// reached through it.
#[derive(Clone)]
pub struct Shell {
    /// The shared state.
    inner: Arc<ShellState>,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

/// What a caller that must block is told when the command it asked about ends: the exit status, in
/// the shell's convention, and `-1` for a command that produced none.
///
/// One slot per shell, and a legacy convenience: [`CommandHandle::wait`] is the cloneable form, it
/// carries the whole verdict rather than a status, and using it does not consume this slot.
///
/// Called once, from the task that ran the line, with no lock held. Dropped uncalled instead when
/// the session shuts down before the command ends.
pub type OnFinish = Box<dyn FnOnce(i32) + Send>;

/// The reservation one admitted command holds in its shell's live state.
struct ActiveCommand {
    /// This command's identity.
    id: CommandId,
    /// The public receipt reserved for it, so a snapshot can report it without a second registry.
    handle: CommandHandle,
    /// The line as submitted.
    text: Arc<str>,
    /// The verdict channel every [`CommandHandle`] on this command reads.
    verdict: Arc<tokio::sync::watch::Sender<WaitState<CommandCompletion, WaitError>>>,
    /// The single legacy callback, when one was registered.
    on_finish: Option<OnFinish>,
    /// Whether the shell closes when this command ends.
    ///
    /// Recorded here rather than in the shell's own `close`, so standard input stays writable for
    /// the whole of a one-shot command: marking the shell closing at admission would reject the
    /// stdin a pipe reader is waiting for and deadlock it.
    close_on_finish: bool,
    /// How many spawn records this shell's executor held when the command was admitted:
    /// everything after it is this command's to signal.
    spawn_mark: usize,
    /// The managed context, once the command is actually running.
    context: Option<CommandContext>,
    /// Whether the line has been handed to the interpreter yet.
    running: bool,
}

/// The streams and interpreter one shell owns for as long as it exists.
///
/// The field order is the drop order and is load-bearing: the producer descriptors close before
/// `shell` drops (whose own drop discards whatever the shell left and reclaims its snapshot).
struct JobResources {
    /// Producer ends the mux holds only to close: the pseudoterminal slave, or the three pipe
    /// ends the interpreter was given. Closing them is what turns a retained reader into end of
    /// file.
    _producers: Vec<OwnedFd>,
    /// The shell's own gated interpreter. Shared with the task running the current line.
    shell: Arc<crate::Shell>,
}

/// One generation's live half: its snapshot, its streams, and whatever command is running in it.
///
/// Owned by [`ShellState::live`]. Taking it out releases the executor, the validator, the producer
/// descriptors and the private gated evaluator in one move, whoever still holds a [`Shell`].
struct LiveShell {
    /// This shell's attached executor: its snapshot, and the spawn records its lines leave.
    executor: crate::MarshExecutor,
    /// The capability history this shell's lines are judged against: its *seed's*, shared with
    /// every other shell over that seed and with no shell over another.
    validator: Arc<Mutex<crate::PolicyValidator>>,
    /// Terminal or pipes, with a terminal shell's geometry resolved from the moment it is open.
    io: JobIo,
    /// Where this shell's interpreter currently stands, refreshed at every boundary.
    working_directory: PathBuf,
    /// The streams and interpreter, once they are built.
    resources: Option<JobResources>,
    /// The command admitted into it, if any.
    command: Option<ActiveCommand>,
    /// A command is being admitted or launched into it.
    starting: bool,
    /// Why this shell is to close, or `None` while it is to stay.
    close: Option<JobCloseMode>,
    /// The release channel the shell's lifecycle task waits on, taken at reclamation.
    release: Option<tokio::sync::oneshot::Sender<Arc<JobEnd>>>,
}

impl LiveShell {
    /// Whether this shell has left public view: a forced shell is gone the moment force is
    /// accepted, while its line, its verdict and its name remain its own until teardown.
    const fn retired(&self) -> bool {
        matches!(self.close, Some(JobCloseMode::Force))
    }
}

/// What a launch decided under the live-state lock, so the awaits that follow happen outside it.
enum Launched {
    /// The line is to run in this interpreter, with this context.
    Run(Arc<crate::Shell>, CommandContext),
    /// A forced stop arrived while the shell was still starting: the line never runs.
    Retired,
}

/// The command in flight in a shell.
#[derive(Clone, Debug)]
pub struct RunningView {
    /// The command line as submitted.
    pub cmd: String,
    /// Its identity, so a view can be correlated with a receipt.
    pub id: CommandId,
}

/// One shell as a caller sees it.
///
/// A view rather than the live state itself, because the interpreter a shell runs its lines in,
/// and the streams it owns, are the mux's and must not leave the collection.
#[derive(Clone, Debug)]
pub struct JobView {
    /// The shell's identity, which is also its principal.
    pub id: ShellId,
    /// The sandbox its commands run in.
    pub sandbox: Sandbox,
    /// Terminal or pipes. A terminal view always carries a resolved geometry.
    pub io: JobIo,
    /// Where its interpreter currently stands, as of the last boundary.
    pub working_directory: PathBuf,
    /// The root of its own snapshot, when it has one.
    pub snapshot_root: Option<PathBuf>,
    /// The command in flight, or `None` when the shell is idle.
    pub running: Option<RunningView>,
    /// A command is being admitted or launched into it, so it is neither idle nor yet running.
    pub starting: bool,
    /// A stop has been accepted, so it will close and takes no new command.
    pub closing: bool,
}

/// One consistent look at everything a collection holds.
///
/// Taken under a single acquisition of the registry's lock and of every shell's live state in
/// insertion order, so the shells, the default geometry and the unfinished commands are all the
/// same instant. A frontend that read them one at a time could otherwise see a command whose
/// shell it has not heard of.
///
/// Selection is deliberately absent: which shell a display is looking at is a property of the
/// front-end, not of the collection.
#[derive(Clone, Debug)]
pub struct MuxSnapshot {
    /// Every visible shell, in creation order.
    pub jobs: Vec<JobView>,
    /// Every admitted command that has not resolved, including those whose shell has retired.
    pub commands: Vec<CommandHandle>,
    /// The size a terminal shell opens at when it asks for none.
    pub default_geometry: TerminalGeometry,
}

/// The background work one mux owns: every shell's lifecycle, and every command run in one.
///
/// One set, not two. Nothing the collection creates is joined *to completion* at teardown: a
/// running line has already been killed and cancelled by the time shutdown reaches here, and
/// waiting on it instead would make a shutdown wait out whatever the line is still doing.
pub(crate) struct Background {
    /// Tasks that end by themselves: a shell's lifecycle, a line's run.
    detached: tokio::task::JoinSet<()>,
}

impl Background {
    /// A set with nothing started yet.
    pub(crate) fn new() -> Self {
        Self {
            detached: tokio::task::JoinSet::new(),
        }
    }

    /// Registers a task that ends by itself, so [`ShellMux::shutdown`] cancels whatever is left.
    ///
    /// `runtime` is the mux's own, never the caller's: the task outlives the call that created it,
    /// and a shell's lifecycle or a running line placed on a caller's throwaway executor would be
    /// cancelled the moment that executor went away.
    ///
    /// The tasks that have already finished are taken first: a set holds a finished task's record
    /// until someone joins it, so a long session that opened and closed many shells would
    /// otherwise accumulate one record per shell for its whole life.
    fn spawn_detached(
        &mut self,
        runtime: &tokio::runtime::Handle,
        task: impl Future<Output = ()> + Send + 'static,
    ) {
        while self.detached.try_join_next().is_some() {}
        self.detached.spawn_on(task, runtime);
    }
}

/// The shell collection: every open shell by principal, the name series it draws from, and the
/// default geometry new terminal shells open at.
pub(crate) struct ShellRegistry {
    /// The open shells, by principal. Retired entries stay until their resources are reclaimed,
    /// so a name a forced stop retired cannot be taken by a second shell mid-teardown.
    shells: HashMap<Principal, Shell>,
    /// The principals in creation order, so every listing keeps the order it always had.
    order: Vec<Principal>,
    /// Next automatic shell name.
    counter: u64,
    /// The size a terminal shell opens at when it asks for no size of its own.
    default_geometry: TerminalGeometry,
    /// Set once shutdown begins; no further work is admitted.
    closing: bool,
}

impl ShellRegistry {
    /// An empty registry whose first automatic name is `1` and whose shells default to
    /// `rows` × `cols`.
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            shells: HashMap::new(),
            order: Vec::new(),
            counter: 1,
            default_geometry: TerminalGeometry { rows, cols },
            closing: false,
        }
    }

    /// The next automatic shell name, skipping any a shell already occupies.
    ///
    /// Monotonic within a session — a name is never reused while the mux lives — because a shell
    /// name is a principal, and reusing one would make two sandboxes indistinguishable in the
    /// history.
    fn next_id(&mut self) -> ShellId {
        loop {
            let id = ShellId::from(self.counter.to_string());
            self.counter += 1;
            if !self.shells.contains_key(id.principal()) {
                return id;
            }
        }
    }

    /// Records one newly admitted shell, in both the map and the order beside it.
    fn insert(&mut self, principal: Principal, shell: Shell) {
        self.order.push(principal.clone());
        self.shells.insert(principal, shell);
    }

    /// Forgets one shell, from both the map and the order beside it.
    fn remove(&mut self, principal: &Principal) -> Option<Shell> {
        let removed = self.shells.remove(principal);
        if removed.is_some() {
            self.order.retain(|held| held != principal);
        }
        removed
    }
}

impl ShellMux {
    /// The shell registry, recovering a poisoned lock like the rest of this module.
    pub(crate) fn shell_registry(&self) -> MutexGuard<'_, ShellRegistry> {
        self.shells.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The background task set, with the same poisoning recovery.
    fn background(&self) -> MutexGuard<'_, Background> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether teardown has begun and no further work is admitted.
    pub(crate) fn is_closing(&self) -> bool {
        self.shell_registry().closing
    }

    /// The next command identity.
    fn next_command_id(&self) -> CommandId {
        CommandId(self.command_counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Opens a shell whose interpreter starts at `initial_dir`, gives it a snapshot of the seed
    /// that directory lies in, and opens its streams.
    ///
    /// `initial_dir` is a **host filesystem path**, not a seed-relative label: an absolute path
    /// keeps its host meaning, a relative one resolves against the process's current directory,
    /// and the empty path means `.`. The seed is then discovered from it, so one collection hosts
    /// shells over as many seeds as its callers name. The process's own directory is never
    /// changed.
    ///
    /// A path that already lies inside one of this collection's shell snapshots names the place in
    /// *that* shell's seed it stands for, rather than making the snapshot a seed of its own.
    ///
    /// `principal` is `None` for the next number in the `1`, `2`, … series. A name a live shell
    /// already holds is refused — anywhere in the collection, not only on one seed, and whether
    /// it matches by capability principal or by displayed name — because a shell name is a
    /// capability principal and two sandboxes sharing one would be indistinguishable in the
    /// history, while two sharing a displayed name would be one `%name` for two principals.
    /// [`SpawnOptions::durable`] opts a named shell into [`ShellId::durable`] ownership.
    ///
    /// No command is submitted here. A shell is a place to run lines in;
    /// [`Shell::run_command`] is how one runs.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::ShuttingDown`] once teardown has begun, with [`MuxError::JobExists`]
    /// when `principal` is taken, with [`MuxError::Io`] when `initial_dir` cannot be made
    /// absolute, with [`MuxError::Marsh`] when it names no seed or that seed cannot be leased,
    /// with [`MuxError::RecoveryRequired`] when that seed's log is unreplayed, with
    /// [`MuxError::InvalidTerminalSize`] for a zero dimension, and with whatever the snapshot, the
    /// streams or the interpreter reported. A failed admission opens nothing and reserves no name;
    /// a failed construction closes every descriptor it opened and releases the name.
    pub async fn open_shell(
        self: &Arc<Self>,
        initial_dir: &Path,
        principal: Option<Principal>,
        options: SpawnOptions,
    ) -> Result<Shell, MuxError> {
        let terminal = options.io.is_terminal();
        if let JobIo::Terminal {
            geometry: Some(geometry),
        } = options.io
            && !geometry.is_valid()
        {
            return Err(MuxError::InvalidTerminalSize {
                rows: geometry.rows,
                cols: geometry.cols,
            });
        }
        // After the geometry check and before the registry lock: this reads the process's current
        // directory for a relative request, which is I/O and must not happen under a mux lock.
        // `absolute` is purely lexical beyond that — it resolves no symlink and touches no
        // directory — so discovery still sees the path the caller named.
        let requested = std::path::absolute(if initial_dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            initial_dir
        })?;

        let (shell, io) = {
            let mut registry = self.shell_registry();
            if registry.closing {
                return Err(MuxError::ShuttingDown);
            }
            let id = match principal {
                Some(principal) if options.durable => ShellId::durable(principal),
                Some(principal) => ShellId::from(principal),
                None => registry.next_id(),
            };
            if registry.shells.iter().any(|(held, shell)| {
                held == id.principal() || shell.id().as_str() == id.as_str()
            }) {
                return Err(MuxError::JobExists(id));
            }
            // Under the registry lock, in registry → seed-registry order: an open, a lease and a
            // recovery happen here, and two first shells racing onto one seed must not both take
            // it. Nothing here awaits or calls back.
            let (initial_dir, seed, state) = self.seed_for(&requested)?;
            // Refused at *admission*, not only at the boundary, and only for the seed that failed.
            // A session whose approved publication failed may have a seed that is neither its old
            // state nor its new one, and a command admitted against that would run — spawning
            // processes, writing files, reaching the network — before its own gate eventually
            // refused it. Every shell over *that* seed refuses; a healthy seed of the same
            // collection is untouched.
            if state.executor.recovery_required() {
                return Err(MuxError::RecoveryRequired);
            }
            let (sandbox, executor) = Self::new_sandbox(&id, &initial_dir, seed, &state)?;
            let io = match options.io {
                JobIo::Terminal { geometry } => JobIo::Terminal {
                    geometry: Some(geometry.unwrap_or(registry.default_geometry)),
                },
                JobIo::Pipes => JobIo::Pipes,
            };

            let working_directory = executor
                .snapshot_root()
                .ok_or(crate::MarshError::NoSnapshot)?
                .join(sandbox.dir.as_str());
            let live = LiveShell {
                executor,
                validator: Arc::clone(&state.validator),
                io,
                working_directory,
                resources: None,
                command: None,
                starting: false,
                // Only a terminal shell can be reclaimed this way, and only when the caller asked
                // for it. A plain shell opened idle persists across every command run in it.
                close: match (options.automatic_close, terminal) {
                    (true, true) => Some(JobCloseMode::Automatic),
                    _ => None,
                },
                release: None,
            };
            let (closed_tx, closed_rx) = tokio::sync::watch::channel(WaitState::Pending);
            let shell = Shell {
                inner: Arc::new(ShellState {
                    id: id.clone(),
                    sandbox,
                    terminal,
                    origin: Arc::downgrade(self),
                    endpoints: OnceLock::new(),
                    closed_tx: Arc::new(closed_tx),
                    closed: closed_rx,
                    discarded: Arc::new(tokio::sync::watch::channel(false).0),
                    live: Mutex::new(Some(live)),
                }),
            };
            registry.insert(id.principal().clone(), shell.clone());
            drop(registry);
            (shell, io)
        };
        self.announce(FrontendEvent::Changed);

        if let Err(error) = self.build_resources(&shell, io, options).await {
            // Shared rather than reported twice: the caller and the shell's own end both need
            // this failure, and none of the errors it wraps clone.
            let shared = Arc::new(error);
            self.fail_construction(&shell, Arc::clone(&shared)).await;
            return Err(MuxError::Shared(shared));
        }
        Ok(shell)
    }

    /// The shell one principal names, or `None` when nothing visible answers to it.
    ///
    /// Clones the current generation's object. It neither creates a shell, opens a seed, changes
    /// selection nor runs a command; a principal no shell holds, and one a forced stop has
    /// retired, both answer `None`. A shell still constructing *is* returned — execution through
    /// it reports [`MuxError::JobNotReady`] until its streams exist.
    #[must_use]
    pub fn get_shell(&self, principal: &Principal) -> Option<Shell> {
        let registry = self.shell_registry();
        let shell = registry.shells.get(principal)?.clone();
        drop(registry);
        let guard = shell.live_lock();
        let visible = guard.as_ref().is_some_and(|live| !live.retired());
        drop(guard);
        visible.then_some(shell)
    }

    /// Allocates one command's identity, text, receipt and verdict channel.
    ///
    /// Both halves exist before anything can run, which is what makes a completion impossible to
    /// steal: the identity the verdict resolves is captured here, not read back off the shell
    /// afterwards.
    fn reserve(
        &self,
        sandbox: &Sandbox,
        cmd: &str,
        on_finish: Option<OnFinish>,
        close_on_finish: bool,
        spawn_mark: usize,
    ) -> ActiveCommand {
        let id = self.next_command_id();
        let text: Arc<str> = Arc::from(cmd);
        let (verdict, watch) = tokio::sync::watch::channel(WaitState::Pending);
        ActiveCommand {
            id,
            handle: CommandHandle {
                id,
                shell: sandbox.clone(),
                text: Arc::clone(&text),
                state: watch,
            },
            text,
            verdict: Arc::new(verdict),
            on_finish,
            close_on_finish,
            spawn_mark,
            context: None,
            running: false,
        }
    }

    /// Builds one shell's streams and interpreter, publishes them, and starts its lifecycle task.
    #[allow(
        clippy::too_many_lines,
        reason = "one shell's streams, interpreter and lifecycle are built as a single ordered \
                  sequence; splitting it would only scatter the cleanup obligations it carries"
    )]
    async fn build_resources(
        self: &Arc<Self>,
        shell: &Shell,
        io: JobIo,
        options: SpawnOptions,
    ) -> Result<(), MuxError> {
        // Both together, under one acquisition: a shell's executor and the validator its lines are
        // judged against belong to the same seed, and reading them apart would let the live state
        // change between the two.
        let (executor, validator) = {
            let guard = shell.live_lock();
            let Some(live) = guard.as_ref() else {
                return Err(MuxError::StaleJob(shell.inner.id.clone()));
            };
            let pair = (live.executor.clone(), Arc::clone(&live.validator));
            drop(guard);
            pair
        };

        let (fds, producers, endpoints, readers) = match io {
            JobIo::Terminal { geometry } => {
                let geometry = geometry.unwrap_or(TerminalGeometry { rows: 24, cols: 80 });
                let (master, slave) = crate::shellmux::pty::open_pty(geometry.rows, geometry.cols)?;
                let shell_slave = slave.try_clone()?;
                let file: brush_core::openfiles::OpenFile = std::fs::File::from(shell_slave).into();
                let mut fds = HashMap::new();
                fds.insert(brush_core::openfiles::OpenFiles::STDIN_FD, file.clone());
                fds.insert(brush_core::openfiles::OpenFiles::STDOUT_FD, file.clone());
                fds.insert(brush_core::openfiles::OpenFiles::STDERR_FD, file);
                let tty_path = terminal_name(slave.as_fd());
                let master = Arc::new(AsyncFd::new(master)?);
                (
                    fds,
                    vec![slave],
                    Endpoints::Terminal {
                        master: Arc::clone(&master),
                        lease: Arc::new(LeaseState::default()),
                        tty_path,
                    },
                    vec![(OutputChannel::Terminal, Reader::Terminal(master))],
                )
            }
            JobIo::Pipes => {
                let pipes = crate::shellmux::pipes::open_pipes()?;
                let mut fds = HashMap::new();
                fds.insert(
                    brush_core::openfiles::OpenFiles::STDIN_FD,
                    std::fs::File::from(pipes.child_stdin.try_clone()?).into(),
                );
                fds.insert(
                    brush_core::openfiles::OpenFiles::STDOUT_FD,
                    std::fs::File::from(pipes.child_stdout.try_clone()?).into(),
                );
                fds.insert(
                    brush_core::openfiles::OpenFiles::STDERR_FD,
                    std::fs::File::from(pipes.child_stderr.try_clone()?).into(),
                );
                let input = Arc::new(PipeInput::new(pipes.input)?);
                (
                    fds,
                    vec![pipes.child_stdin, pipes.child_stdout, pipes.child_stderr],
                    Endpoints::Pipes { input },
                    vec![
                        (
                            OutputChannel::Stdout,
                            Reader::Pipe(AsyncFd::new(pipes.stdout)?),
                        ),
                        (
                            OutputChannel::Stderr,
                            Reader::Pipe(AsyncFd::new(pipes.stderr)?),
                        ),
                    ],
                )
            }
        };

        let interpreter = self
            .build_shell(
                &executor,
                validator,
                &shell.inner.sandbox,
                fds,
                options.environment,
            )
            .await?;
        // Mux-owned: every command it runs concludes explicitly, so its final boundary must
        // discard rather than publish whatever an abort left.
        interpreter.discard_on_drop();

        let (release, released) = tokio::sync::oneshot::channel::<Arc<JobEnd>>();

        let latest = {
            // Registry then live state, never the other way round: the default geometry belongs to
            // the collection and the shell's own to its live half.
            let registry = self.shell_registry();
            let default = registry.default_geometry;
            let mut guard = shell.live_lock();
            let Some(live) = guard.as_mut() else {
                return Err(MuxError::StaleJob(shell.inner.id.clone()));
            };
            live.resources = Some(JobResources {
                _producers: producers,
                shell: interpreter,
            });
            live.release = Some(release);
            let latest = match live.io {
                JobIo::Terminal { geometry } => geometry.or(Some(default)),
                JobIo::Pipes => None,
            };
            drop(guard);
            drop(registry);
            latest
        };
        // Endpoints are published before the resize and before `Opened`, so nothing that reads the
        // shell afterwards can find it half-built.
        let _ = shell.inner.endpoints.set(endpoints);

        // Outside the lock: the ioctl is a syscall on a descriptor nothing else may take away
        // while the shell holds it, and a resize that landed during construction must not be lost.
        if let (Some(geometry), Some(master)) = (latest, shell.master()) {
            crate::shellmux::pty::resize_pty(
                master.get_ref().as_fd(),
                geometry.rows,
                geometry.cols,
            )?;
        }
        self.launched.notify_waiters();

        self.announce(FrontendEvent::Opened(shell));
        // Publication is a state change, not only a new handle: the admission announced `Changed`
        // while this shell still had no resources.
        self.announce(FrontendEvent::Changed);

        let lifecycle = job_lifecycle(
            self.frontend(),
            shell.inner.sandbox.clone(),
            readers,
            released,
            Arc::clone(&shell.inner.closed_tx),
            shell.inner.discarded.subscribe(),
            self.runtime.clone(),
        );
        self.background().spawn_detached(&self.runtime, lifecycle);
        Ok(())
    }

    /// Tears down a shell whose construction failed, resolving its closure.
    ///
    /// No `Opened` was delivered and no byte of output exists, so the failure is reported as the
    /// shell's end rather than as a command result or fabricated output.
    async fn fail_construction(self: &Arc<Self>, shell: &Shell, error: Arc<MuxError>) {
        let live = {
            let mut registry = self.shell_registry();
            let removed = registry
                .shells
                .get(shell.inner.sandbox.id.principal())
                .is_some_and(|held| Arc::ptr_eq(&held.inner, &shell.inner));
            if removed {
                registry.remove(shell.inner.sandbox.id.principal());
            }
            let live = shell.take_live();
            drop(registry);
            live
        };

        let end = Arc::new(JobEnd {
            shell: shell.inner.sandbox.clone(),
            close_mode: Some(JobCloseMode::Force),
            completion: None,
            error: Some(Arc::clone(&error)),
            // The row is already gone, so this asks the registry for the seed the shell names.
            // Never opens one: a teardown is no place to take a lease, and a seed this collection
            // never opened owes it nothing.
            recovery_required: self.seed_recovery_required(&shell.inner.sandbox.seed),
        });

        if let Some(live) = live {
            // Off the lock and onto a blocking worker: it holds the shell's snapshot, and nothing
            // else does once the failed construction is gone. This mux's blocking pool, not the
            // caller's: reclaiming a snapshot is filesystem work that must finish, and a caller's
            // executor may be gone before it has.
            let _ = self.runtime.spawn_blocking(move || drop(live)).await;
            shell
                .inner
                .closed_tx
                .send_replace(WaitState::Done(Arc::clone(&end)));
        }
        self.announce(FrontendEvent::Closed { end: &end });
        self.launched.notify_waiters();
        self.announce(FrontendEvent::Changed);
    }

    /// Resolves the admitted command's receipt, delivers it, and records the shell's closure.
    ///
    /// The whole handoff happens in one acquisition of the live-state lock: the command's
    /// identity, its text, its callback and its closure decision are captured *before* the running
    /// slot is released, so neither a `keep` nor a second command admitted the instant afterwards
    /// can slip between completion and a one-shot closure.
    fn conclude_command(
        self: &Arc<Self>,
        shell: &Shell,
        exit_code: Option<i32>,
        outcome: Result<Outcome, MuxError>,
    ) -> Option<Arc<CommandCompletion>> {
        let active = {
            let mut guard = shell.live_lock();
            let live = guard.as_mut()?;
            let active = live.command.take()?;
            if active.close_on_finish && live.close != Some(JobCloseMode::Force) {
                live.close = Some(JobCloseMode::Graceful);
            }
            drop(guard);
            active
        };

        let completion = Arc::new(CommandCompletion {
            id: active.id,
            shell: shell.inner.sandbox.clone(),
            command: Arc::clone(&active.text),
            exit_code,
            outcome: Arc::new(outcome),
        });
        active
            .verdict
            .send_replace(WaitState::Done(Arc::clone(&completion)));

        // Outside the lock, like every other callback delivery.
        self.announce(FrontendEvent::Finished {
            completion: &completion,
        });
        if let Some(done) = active.on_finish {
            done(completion.legacy_status());
        }
        Some(completion)
    }

    /// One consistent look at every shell and every unfinished command.
    #[must_use]
    pub fn snapshot(&self) -> MuxSnapshot {
        let registry = self.shell_registry();
        let default_geometry = registry.default_geometry;
        let mut guards = Vec::with_capacity(registry.order.len());
        for principal in &registry.order {
            if let Some(shell) = registry.shells.get(principal) {
                guards.push((shell, shell.live_lock()));
            }
        }
        let mut jobs = Vec::with_capacity(guards.len());
        let mut commands = Vec::new();
        for (shell, guard) in &guards {
            let Some(live) = guard.as_ref() else {
                continue;
            };
            if let Some(active) = live.command.as_ref() {
                commands.push(active.handle.clone());
            }
            if !live.retired() {
                jobs.push(job_view(shell, live));
            }
        }
        drop(guards);
        drop(registry);
        commands.sort_by_key(CommandHandle::id);
        MuxSnapshot {
            jobs,
            commands,
            default_geometry,
        }
    }

    /// Every open shell, in creation order. A forced shell is not one: it left public view when
    /// its stop was accepted.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        let registry = self.shell_registry();
        let mut views = Vec::with_capacity(registry.order.len());
        for principal in &registry.order {
            let Some(shell) = registry.shells.get(principal) else {
                continue;
            };
            let guard = shell.live_lock();
            if let Some(live) = guard.as_ref()
                && !live.retired()
            {
                views.push(job_view(shell, live));
            }
            drop(guard);
        }
        drop(registry);
        views
    }

    /// One shell's view, by identity, excluding a forced one for the reason [`Self::jobs`] does.
    #[must_use]
    pub fn job(&self, id: &ShellId) -> Option<JobView> {
        let registry = self.shell_registry();
        let shell = registry.shells.get(id.principal())?.clone();
        drop(registry);
        let guard = shell.live_lock();
        let live = guard.as_ref()?;
        if live.retired() {
            return None;
        }
        let view = job_view(&shell, live);
        drop(guard);
        Some(view)
    }

    /// Applies `size` to every live terminal shell, and to every terminal shell opened afterwards.
    ///
    /// Pipe shells are skipped: they have no terminal. A repeated resize is reapplied, because a
    /// program may have changed the terminal underneath. Every terminal is attempted; the first
    /// failure is reported once the pass is over, so one dead terminal does not silently skip the
    /// rest.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::InvalidTerminalSize`] for a zero dimension, changing nothing at all
    /// — including telling the frontend.
    pub async fn resize_all(self: &Arc<Self>, size: TerminalGeometry) -> Result<(), MuxError> {
        if !size.is_valid() {
            return Err(MuxError::InvalidTerminalSize {
                rows: size.rows,
                cols: size.cols,
            });
        }
        let targets: Vec<(Sandbox, Arc<AsyncFd<OwnedFd>>)> = {
            let mut registry = self.shell_registry();
            registry.default_geometry = size;
            let mut targets = Vec::new();
            for principal in &registry.order {
                let Some(shell) = registry.shells.get(principal) else {
                    continue;
                };
                let mut guard = shell.live_lock();
                if let Some(live) = guard.as_mut()
                    && live.io.is_terminal()
                {
                    live.io = JobIo::Terminal {
                        geometry: Some(size),
                    };
                    if let Some(master) = shell.master() {
                        targets.push((shell.inner.sandbox.clone(), Arc::clone(master)));
                    }
                }
                drop(guard);
            }
            drop(registry);
            targets
        };

        let guard = self.resize_lock.lock().await;
        let mut failure = Ok(());
        let mut resized = Vec::with_capacity(targets.len());
        for (sandbox, master) in targets {
            match crate::shellmux::pty::resize_pty(master.get_ref().as_fd(), size.rows, size.cols) {
                Ok(()) => resized.push(sandbox),
                Err(error) => {
                    if failure.is_ok() {
                        failure = Err(MuxError::Io(error));
                    }
                }
            }
        }
        drop(guard);

        self.announce(FrontendEvent::DefaultResized { geometry: size });
        for sandbox in &resized {
            self.announce(FrontendEvent::Resized {
                shell: sandbox,
                geometry: size,
            });
        }
        failure
    }

    /// Takes one shell's live half out, if it is to close and has nothing left in flight.
    ///
    /// A shell with a command admitted is kept whatever its mode: the snapshot is what that
    /// command is running in, and its line is not concluded yet.
    ///
    /// The whole live half, not only its resources: it also holds the shell's attached executor,
    /// and dropping that may be what releases the last handle on the shell's snapshot — blocking
    /// work that must not happen under a lock.
    fn take_closable(&self, shell: &Shell) -> Option<LiveShell> {
        let mut registry = self.shell_registry();
        let held = registry
            .shells
            .get(shell.inner.sandbox.id.principal())?
            .clone();
        if !Arc::ptr_eq(&held.inner, &shell.inner) {
            return None;
        }
        let mut guard = shell.live_lock();
        let closable = guard
            .as_ref()
            .is_some_and(|live| live.close.is_some() && live.command.is_none() && !live.starting);
        if !closable {
            drop(guard);
            return None;
        }
        let taken = guard.take();
        drop(guard);
        registry.remove(shell.inner.sandbox.id.principal());
        drop(registry);
        taken
    }

    /// Reclaims a closed shell: its interpreter, its streams and its snapshot, then resolves its
    /// closure.
    ///
    /// Dropping the live half *is* the reclamation, and all of it is blocking filesystem work, so
    /// it happens on a blocking worker of this mux's own runtime — a caller's executor may be gone
    /// before a snapshot is finished with. The [`JobEnd`] is sent only afterwards, which is what
    /// makes [`Shell::wait_closed`] resolve after the snapshot is actually gone.
    async fn reclaim(
        self: &Arc<Self>,
        shell: &Shell,
        mut live: LiveShell,
        completion: Option<Arc<CommandCompletion>>,
    ) {
        let release = live.release.take();
        let end = Arc::new(JobEnd {
            shell: shell.inner.sandbox.clone(),
            close_mode: live.close,
            completion,
            error: None,
            // The closing shell's own seed, not the collection's: a poisoned seed A must not
            // report every shell of a healthy seed B as needing recovery.
            recovery_required: live.executor.recovery_required(),
        });
        let _ = self.runtime.spawn_blocking(move || drop(live)).await;
        shell
            .inner
            .closed_tx
            .send_replace(WaitState::Done(Arc::clone(&end)));
        if let Some(release) = release {
            // The lifecycle task publishes `Closed` once every stream has ended, so a late chunk
            // is never delivered after the shell's end.
            let _ = release.send(end);
        }
        self.announce(FrontendEvent::Changed);
    }

    /// Ends the session.
    ///
    /// Admission closes first, so nothing new is accepted while teardown runs. Then: every
    /// outstanding command's native workers are asked to stop and its processes killed, every
    /// waiting receipt and closure watch is resolved with [`WaitError::Shutdown`], owned tasks are
    /// joined, remaining lifecycle and line tasks are cancelled, every shell is reclaimed, and the
    /// frontend is detached.
    ///
    /// Idempotent: a second call finds admission already closed and nothing left to reclaim, and
    /// so does the collection's own [`Drop`].
    ///
    /// Startup owns persistent recovery, so nothing here sweeps snapshots or the write-ahead log.
    /// A snapshot whose *approved* publication failed is left on disk deliberately: it is the
    /// content the next [`MarshExecutor::open`](crate::MarshExecutor::open) replays from.
    ///
    /// # Errors
    ///
    /// Fails with the first termination failure observed while killing outstanding lines. Every
    /// shell is still reclaimed.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), MuxError> {
        let mut failure = Ok(());
        let shells: Vec<Shell> = {
            let mut registry = self.shell_registry();
            registry.closing = true;
            let shells = registry
                .order
                .iter()
                .filter_map(|principal| registry.shells.get(principal).cloned())
                .collect();
            drop(registry);
            shells
        };

        let mut abandoned = Vec::new();
        let mut contexts = Vec::new();
        for shell in &shells {
            let mut guard = shell.live_lock();
            if let Some(live) = guard.as_mut()
                && let Some(active) = live.command.as_mut()
            {
                if active.running
                    && let Err(error) =
                        kill_since(&live.executor, active.spawn_mark, &shell.inner.id)
                    && failure.is_ok()
                {
                    failure = Err(error);
                }
                contexts.extend(active.context.clone());
                abandoned.extend(active.on_finish.take());
                active
                    .verdict
                    .send_replace(WaitState::Failed(WaitError::Shutdown));
            }
            drop(guard);
            shell
                .inner
                .closed_tx
                .send_replace(WaitState::Failed(WaitError::Shutdown));
        }
        // Outside the lock, like every other callback delivery.
        drop(abandoned);
        for context in &contexts {
            context.request_cancellation();
        }
        for context in &contexts {
            context.finish().await;
        }

        // Before the live halves go: every running line's task holds an `Arc<crate::Shell>` clone,
        // and dropping the last one is blocking work that must not land on an async worker.
        let mut detached = std::mem::take(&mut self.background().detached);
        detached.shutdown().await;

        let lives: Vec<LiveShell> = self
            .drain_registry()
            .into_iter()
            .filter_map(|shell| shell.take_live())
            .collect();
        let _ = self
            .runtime
            .spawn_blocking(move || {
                for live in &lives {
                    if let (Some(active), Some(resources)) = (&live.command, &live.resources)
                        && active.running
                        && let Ok(mut guard) = resources.shell.shell_ref().try_lock()
                    {
                        // The line's task was aborted above, so nothing holds the interpreter; a
                        // failure here has nowhere to go, and its own drop then discards whatever
                        // the discard could not clear.
                        let _ = resources.shell.discard(&mut guard, &active.text);
                    }
                }
                drop(lives);
            })
            .await;

        self.detach();
        failure
    }

    /// Empties the registry, returning every shell that was still in it.
    ///
    /// The one place a teardown removes shells, shared by [`Self::shutdown`] and [`Drop`] so that
    /// a drop after an explicit shutdown finds nothing and is harmless. What each caller does
    /// with the live halves afterwards is its own: shutdown releases them on a blocking worker,
    /// while a drop has no runtime left to hand them to and releases them inline.
    fn drain_registry(&self) -> Vec<Shell> {
        let mut registry = self.shell_registry();
        registry.closing = true;
        let order = std::mem::take(&mut registry.order);
        let shells = order
            .into_iter()
            .filter_map(|principal| registry.shells.remove(&principal))
            .collect();
        registry.shells.clear();
        drop(registry);
        shells
    }
}

impl Drop for ShellMux {
    /// Releases every shell this collection still holds, synchronously.
    ///
    /// A collection that is dropped without an explicit [`ShellMux::shutdown`] must still give
    /// back its seeds: a retained [`Shell`] whose collection is gone is a dead object, and it must
    /// not be what keeps a lease or a snapshot alive. So the live halves are taken out here and
    /// released off every lock, rather than waiting for a caller to drop its handles or for a task
    /// that may never be polled again.
    ///
    /// A command still running holds an `Arc` on this collection, so this cannot run underneath
    /// one; [`ShellMux::shutdown`] remains the path that interrupts and joins those.
    fn drop(&mut self) {
        for shell in self.drain_registry() {
            if let Some(mut live) = shell.take_live() {
                if let Some(active) = live.command.take() {
                    active
                        .verdict
                        .send_replace(WaitState::Failed(WaitError::Shutdown));
                }
                drop(live);
            }
            shell
                .inner
                .closed_tx
                .send_replace(WaitState::Failed(WaitError::Shutdown));
        }
    }
}

impl Shell {
    /// The shell's identity, which is also its principal.
    #[must_use]
    pub fn id(&self) -> &ShellId {
        &self.inner.id
    }

    /// The capability principal this shell's lines request capabilities as.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        self.inner.sandbox.id.principal()
    }

    /// The sandbox its commands run in.
    #[must_use]
    pub fn sandbox(&self) -> &Sandbox {
        &self.inner.sandbox
    }

    /// Which output streams this shell can produce.
    ///
    /// A terminal shell has exactly one, merged. A pipe shell has two, independent. This is fixed
    /// at admission and never changes.
    #[must_use]
    pub fn output_channels(&self) -> &'static [OutputChannel] {
        if self.inner.terminal {
            &[OutputChannel::Terminal]
        } else {
            &[OutputChannel::Stdout, OutputChannel::Stderr]
        }
    }

    /// Whether this shell has already closed, without waiting.
    ///
    /// Authoritative and cheap. A reader that is about to register an observer on one of this
    /// shell's streams needs it: a stream whose retained storage was already reclaimed would
    /// otherwise be recreated as an empty, never-ending one, and the observer would wait forever
    /// for bytes that can never arrive. The shell's own closure watch is the only source of that
    /// answer which cannot age out.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !matches!(*self.inner.closed.borrow(), WaitState::Pending)
    }

    /// Waits for this shell to close.
    ///
    /// Resolves after every byte of every one of the shell's streams has been delivered and its
    /// snapshot reclaimed — which is a different boundary from a command finishing, and later.
    ///
    /// # Errors
    ///
    /// Fails with [`WaitError::Shutdown`] when the collection was torn down first, and with
    /// [`WaitError::Aborted`] when the producer was lost unexpectedly.
    pub async fn wait_closed(&self) -> Result<Arc<JobEnd>, WaitError> {
        let mut closed = self.inner.closed.clone();
        // The sender going away with the shell still open is the producer being lost: nothing
        // will ever resolve this, and a waiter must not hang on it.
        wait_for_completion(&mut closed, || WaitError::Aborted).await
    }

    /// A borrowed descriptor on this shell's terminal master.
    ///
    /// For a server-private metadata probe — the terminal's name, its foreground process group —
    /// and nothing else. It is not an I/O handle: reading it would steal bytes from the mux's own
    /// pump, and writing it bypasses [`Shell::write_input`]'s ordering and closing checks.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`] for a pipe shell and [`MuxError::JobNotReady`] before
    /// the shell's streams are open.
    pub fn terminal_fd(&self) -> Result<BorrowedFd<'_>, MuxError> {
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { master, .. }) => Ok(master.get_ref().as_fd()),
            Some(Endpoints::Pipes { .. }) => Err(MuxError::NotTerminal(self.inner.id.clone())),
            None if self.inner.terminal => Err(MuxError::JobNotReady(self.inner.id.clone())),
            None => Err(MuxError::NotTerminal(self.inner.id.clone())),
        }
    }

    /// The path of this shell's terminal, when it has one and the kernel named it.
    #[must_use]
    pub fn tty_path(&self) -> Option<&std::path::Path> {
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { tty_path, .. }) => tty_path.as_deref(),
            _ => None,
        }
    }

    /// The terminal master, for the mux's own operations.
    fn master(&self) -> Option<&Arc<AsyncFd<OwnedFd>>> {
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { master, .. }) => Some(master),
            _ => None,
        }
    }

    /// The lease state, for the mux's own operations.
    fn lease(&self) -> Option<&Arc<LeaseState>> {
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { lease, .. }) => Some(lease),
            _ => None,
        }
    }

    /// This generation's live half, recovering a poisoned lock like the rest of this module.
    fn live_lock(&self) -> MutexGuard<'_, Option<LiveShell>> {
        self.inner
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes this generation's live half out, releasing the lock before it is dropped.
    fn take_live(&self) -> Option<LiveShell> {
        let mut guard = self.live_lock();
        let taken = guard.take();
        drop(guard);
        taken
    }

    /// Runs `body` on this generation's live half, under its lock.
    ///
    /// The one synchronous shape every operation on a live shell has: take the lock, answer
    /// [`MuxError::StaleJob`] if the generation has been reclaimed, do the work, release the
    /// lock. `body` is synchronous and its result is owned, so nothing borrowed from the
    /// generation escapes the guard and no `await` can happen while it is held.
    fn with_live<T>(
        &self,
        body: impl FnOnce(&LiveShell) -> Result<T, MuxError>,
    ) -> Result<T, MuxError> {
        let guard = self.live_lock();
        let Some(live) = guard.as_ref() else {
            return Err(MuxError::StaleJob(self.inner.id.clone()));
        };
        let outcome = body(live);
        drop(guard);
        outcome
    }

    /// [`Self::with_live`], for an operation that mutates the generation.
    fn with_live_mut<T>(
        &self,
        body: impl FnOnce(&mut LiveShell) -> Result<T, MuxError>,
    ) -> Result<T, MuxError> {
        let mut guard = self.live_lock();
        let Some(live) = guard.as_mut() else {
            return Err(MuxError::StaleJob(self.inner.id.clone()));
        };
        let outcome = body(live);
        drop(guard);
        outcome
    }

    /// The collection this shell belongs to, or [`MuxError::ShuttingDown`] once it is gone.
    fn mux(&self) -> Result<Arc<ShellMux>, MuxError> {
        self.inner.origin.upgrade().ok_or(MuxError::ShuttingDown)
    }

    /// This shell as a caller sees it.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub fn view(&self) -> Result<JobView, MuxError> {
        self.with_live(|live| Ok(job_view(self, live)))
    }

    /// Waits until this shell's resources exist and nothing is being launched into it.
    ///
    /// Readiness of the *shell*, not of a command: a line already running is ready, because it has
    /// a terminal, a snapshot and an interpreter. What this waits out is construction and the
    /// short window in which a command is being admitted.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::StaleJob`] once this generation has been reclaimed, and with
    /// [`MuxError::ShuttingDown`] once its collection is gone.
    pub async fn wait_ready(&self) -> Result<JobView, MuxError> {
        let mux = self.mux()?;
        loop {
            // Registered before the check: a publication that lands between them is not a lost
            // wakeup.
            let notified = mux.launched.notified();
            let ready = self.with_live(|live| {
                Ok((!live.starting && live.resources.is_some()).then(|| job_view(self, live)))
            })?;
            if let Some(view) = ready {
                return Ok(view);
            }
            notified.await;
        }
    }

    /// Keeps a shell a reader has taken an interest in: it will not close itself any more.
    ///
    /// Only the automatic closure of a shell opened with [`SpawnOptions::automatic_close`] is
    /// cancelled. An explicit stop, and a one-shot command's own `close_on_finish`, are decisions
    /// this may not quietly revoke; a pipe shell cannot be retained at all, because it has no
    /// prompt to return to.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub fn keep(&self) -> Result<bool, MuxError> {
        let kept = self.with_live_mut(|live| {
            Ok(
                if live.io.is_terminal() && live.close == Some(JobCloseMode::Automatic) {
                    live.close = None;
                    true
                } else {
                    false
                },
            )
        })?;
        if kept && let Ok(mux) = self.mux() {
            mux.announce(FrontendEvent::Changed);
        }
        Ok(kept)
    }

    /// Asks to be told when the command now running in this shell ends.
    ///
    /// `false` when nothing is running, or when a caller already occupies the slot. The cloneable
    /// alternative is [`CommandHandle::wait`], which has no single-slot limit.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub fn on_finish(&self, done: OnFinish) -> Result<bool, MuxError> {
        // Held in an `Option` the closure only *takes* from: a refused callback must be dropped
        // after the mutex is released, because its captures' destructors are arbitrary code and
        // one of them running under this shell's live lock would deadlock the shell it queries.
        let mut done = Some(done);
        let registered = self.with_live_mut(|live| {
            Ok(match live.command.as_mut() {
                Some(active) if active.running && active.on_finish.is_none() => {
                    active.on_finish = done.take();
                    true
                }
                _ => false,
            })
        })?;
        drop(done);
        Ok(registered)
    }

    /// Sends `signal` to the processes the command now running in this shell started.
    ///
    /// Only that command's own process groups, tracked from the spawn mark taken when it was
    /// admitted: this is not an arbitrary-pid interface. Best-effort, for the reason the spawn log
    /// is append-only — a recorded pid may already be gone, which is not a failure. A
    /// builtin-only line has no process at all, and signalling one succeeds having sent nothing.
    ///
    /// Unlike a forced [`Self::stop`], this does not retire the shell or discard its work: a
    /// command that catches the signal and finishes normally is still gated normally.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobTermination`] carrying the last errno that was not `ESRCH`, and
    /// with [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub fn signal(&self, signal: crate::Signal) -> Result<(), MuxError> {
        self.with_live(|live| {
            let Some(active) = live.command.as_ref().filter(|active| active.running) else {
                return Ok(());
            };
            live.executor
                .signal_since(active.spawn_mark, signal)
                .map_err(|source| MuxError::JobTermination {
                    job: self.inner.id.clone(),
                    source,
                })
        })
    }

    /// Accepts a stop for this shell, gracefully or by force.
    ///
    /// Graceful sends no signal: it records that the shell closes once its command is over, and a
    /// repeated request is harmless. Force kills every process the running command spawned, asks
    /// its native workers to stop, retires the shell at once, and discards the line rather than
    /// gating it.
    ///
    /// # Cancellation versus conclusion
    ///
    /// The decision is linearized under the live-state lock. A force accepted before finalization
    /// begins causes the line to be discarded. A force that arrives after an approved publication
    /// has begun cannot undo it: the eventual completion is authoritative, and nothing here
    /// promises to roll back effects that have already been published.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobTermination`] when a forced shell's processes could not be
    /// signalled — in which case nothing is changed — and with [`MuxError::StaleJob`] once this
    /// generation has been reclaimed or retired.
    pub async fn stop(&self, force: bool) -> Result<(), MuxError> {
        let context = self.with_live_mut(|live| {
            if live.retired() {
                return Err(MuxError::StaleJob(self.inner.id.clone()));
            }
            let mut context = None;
            if force {
                // Before anything changes: a failed kill leaves the shell as it was.
                if let Some(active) = live.command.as_ref().filter(|active| active.running) {
                    kill_since(&live.executor, active.spawn_mark, &self.inner.id)?;
                    context.clone_from(&active.context);
                }
                live.close = Some(JobCloseMode::Force);
            } else {
                live.close = Some(JobCloseMode::Graceful);
            }
            Ok(context)
        })?;
        if force {
            // Every other half of a forced stop only *asks*. `kill_since` reaches processes this
            // line started and a managed context is cooperative, so a line whose writer is a
            // builtin — running inline on this runtime, with no process and no poll point — is
            // reached by neither. Closing its output read ends is what actually ends it: the next
            // write fails instead of waiting for a drain the discard has already made pointless.
            self.inner.discarded.send_replace(true);
        }
        if let Some(context) = context {
            context.request_cancellation();
        }
        // The revocation is unconditional — the shell is going away whether or not a prompt is
        // reading it — and the wait is what keeps the terminal's mode from being restored into a
        // shell that has already been reclaimed. Both are no-ops for a pipe shell or an idle one.
        if let Some(state) = self.lease() {
            state.revoke(Revocation::Close);
            let _ = state.settled().await;
        }
        let mux = self.mux()?;
        mux.announce(FrontendEvent::Changed);
        if let Some(live) = mux.take_closable(self) {
            mux.reclaim(self, live, None).await;
        }
        Ok(())
    }

    /// Applies `size` to this terminal shell.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::InvalidTerminalSize`] for a zero dimension — changing nothing —
    /// [`MuxError::NotTerminal`] for a pipe shell, [`MuxError::StaleJob`] once this generation has
    /// been reclaimed, and [`MuxError::ShuttingDown`] once its collection is gone.
    pub async fn resize(&self, size: TerminalGeometry) -> Result<(), MuxError> {
        if !size.is_valid() {
            return Err(MuxError::InvalidTerminalSize {
                rows: size.rows,
                cols: size.cols,
            });
        }
        self.with_live_mut(|live| {
            if !live.io.is_terminal() {
                return Err(MuxError::NotTerminal(self.inner.id.clone()));
            }
            live.io = JobIo::Terminal {
                geometry: Some(size),
            };
            Ok(())
        })?;
        let mux = self.mux()?;
        // The physical application is serialized through the collection's own resize lock, and
        // the desired size is re-read inside it: two concurrent resizes must end with the terminal
        // at whichever one the live state settled on, never at the older of the two.
        let guard = mux.resize_lock.lock().await;
        let desired = {
            let live = self.live_lock();
            let desired = live.as_ref().and_then(|live| match live.io {
                JobIo::Terminal { geometry } => geometry,
                JobIo::Pipes => None,
            });
            drop(live);
            desired
        };
        if let (Some(desired), Some(master)) = (desired, self.master()) {
            crate::shellmux::pty::resize_pty(master.get_ref().as_fd(), desired.rows, desired.cols)?;
        }
        drop(guard);
        mux.announce(FrontendEvent::Resized {
            shell: &self.inner.sandbox,
            geometry: size,
        });
        Ok(())
    }

    /// Writes `bytes` to this shell's standard input.
    ///
    /// For a terminal shell these are keystrokes: the line discipline sees them, a program in raw
    /// mode gets them unchanged, and there is no end-of-file to be sent this way. For a pipe shell
    /// they are bytes on a pipe, and [`Self::close_input`] is what ends it.
    ///
    /// Permitted while a command is running, which is the point: a one-shot pipe command's own
    /// standard input arrives this way, and a shell that marked itself closing at admission would
    /// reject it.
    ///
    /// Partial writes are retried, so the whole slice reaches the shell or an error is reported. A
    /// failure partway through has already delivered a prefix.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobClosing`] when the shell has been closed, [`MuxError::InputClosed`]
    /// when a pipe shell's input has already been ended, [`MuxError::JobNotReady`] before its
    /// streams exist, and [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub async fn write_input(&self, bytes: &[u8]) -> Result<(), MuxError> {
        self.with_live(|live| {
            if live.close.is_some_and(JobCloseMode::explicit) {
                return Err(MuxError::JobClosing(self.inner.id.clone()));
            }
            Ok(())
        })?;
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { master, .. }) => write_fd(master, bytes).await,
            Some(Endpoints::Pipes { input }) => input.write_all(bytes).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    MuxError::InputClosed(self.inner.id.clone())
                } else {
                    MuxError::Io(error)
                }
            }),
            None => Err(MuxError::JobNotReady(self.inner.id.clone())),
        }
    }

    /// Ends a pipe shell's standard input: a real end-of-file, after every write already accepted.
    ///
    /// Idempotent. It closes *this* shell's input; a shell whose name was later reused has a
    /// descriptor of its own and cannot be reached through this object.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotPiped`] for a terminal shell — a pseudoterminal has no
    /// half-close, and sending an end-of-transmission character instead would be a keystroke, not
    /// an end of file — and with [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub async fn close_input(&self) -> Result<(), MuxError> {
        self.with_live(|_| Ok(()))?;
        match self.inner.endpoints.get() {
            Some(Endpoints::Pipes { input }) => {
                input.close().await;
                Ok(())
            }
            Some(Endpoints::Terminal { .. }) => Err(MuxError::NotPiped(self.inner.id.clone())),
            None if self.inner.terminal => Err(MuxError::NotPiped(self.inner.id.clone())),
            None => Err(MuxError::JobNotReady(self.inner.id.clone())),
        }
    }

    /// Lends this terminal shell's slave side while no command is running in it.
    ///
    /// The lease is how an interactive prompt reads *this pane's* keyboard without touching the
    /// process's own standard input. It is revoked the moment a command is admitted, and the
    /// admission waits for the acknowledgement outside every lock.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`] for a pipe shell, [`MuxError::TerminalBusy`] when a
    /// command is running or another lease is outstanding, [`MuxError::JobClosing`] once the shell
    /// is closing, [`MuxError::JobNotReady`] before the terminal exists, [`MuxError::Shared`]
    /// when an earlier lease left the terminal in a mode it could not undo, [`MuxError::StaleJob`]
    /// once this generation has been reclaimed, and [`MuxError::ShuttingDown`] during teardown.
    pub fn idle_terminal(&self) -> Result<IdleTerminal, MuxError> {
        let mux = self.mux()?;
        if mux.is_closing() {
            return Err(MuxError::ShuttingDown);
        }
        let state = self.with_live(|live| {
            if !live.io.is_terminal() {
                return Err(MuxError::NotTerminal(self.inner.id.clone()));
            }
            if live.command.is_some() || live.starting {
                return Err(MuxError::TerminalBusy(self.inner.id.clone()));
            }
            if live.resources.is_none() {
                return Err(MuxError::JobNotReady(self.inner.id.clone()));
            }
            let Some(state) = self.lease().map(Arc::clone) else {
                return Err(MuxError::JobNotReady(self.inner.id.clone()));
            };
            // Under the live-state lock, beside `command` and `starting`: those three are what
            // make a terminal unavailable, and checking them in two places would let an admission
            // and a grant each conclude the terminal was theirs.
            state.reserve().map_err(|refusal| match refusal {
                LeaseRefusal::Held => MuxError::TerminalBusy(self.inner.id.clone()),
                LeaseRefusal::Closing => MuxError::JobClosing(self.inner.id.clone()),
                LeaseRefusal::Unrestored(error) => MuxError::Shared(error),
            })?;
            Ok(state)
        })?;

        let Some(master) = self.master() else {
            return Err(MuxError::JobNotReady(self.inner.id.clone()));
        };
        match IdleTerminal::grant(master.get_ref().as_fd(), Arc::clone(&state)) {
            Ok(lease) => Ok(lease),
            Err(error) => {
                // The reservation must not outlive a failed grant, or the terminal stays
                // permanently held for a lease nobody has.
                state.abandon();
                Err(MuxError::Io(error))
            }
        }
    }

    /// Whether `line` is a complete shell command, or is still waiting for more input.
    ///
    /// Parsed by this shell's own interpreter, so its options — POSIX mode, extended globbing —
    /// are the ones that decide. Only an incomplete tokenization or an end-of-input parse failure
    /// answers `false`: any other syntax error is a real error, and the line should run so the
    /// shell itself produces the diagnostic the user expects.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobBusy`] while a command is running, [`MuxError::NotTerminal`] for
    /// a pipe shell, [`MuxError::JobNotReady`] before the interpreter exists, and
    /// [`MuxError::StaleJob`] once this generation has been reclaimed.
    pub async fn input_is_complete(&self, line: &str) -> Result<bool, MuxError> {
        let interpreter = self.with_live(|live| {
            if !live.io.is_terminal() {
                return Err(MuxError::NotTerminal(self.inner.id.clone()));
            }
            if live.command.is_some() || live.starting {
                return Err(MuxError::JobBusy(self.inner.id.clone()));
            }
            let Some(resources) = live.resources.as_ref() else {
                return Err(MuxError::JobNotReady(self.inner.id.clone()));
            };
            Ok(Arc::clone(&resources.shell))
        })?;
        let guard = interpreter.shell_ref().lock().await;
        let parsed = guard.parse_string(line.to_string());
        drop(guard);
        Ok(match parsed {
            Ok(_) => true,
            Err(
                brush_core::parser::ParseError::ParsingAtEndOfInput
                | brush_core::parser::ParseError::Tokenizing { .. },
            ) => false,
            Err(_) => true,
        })
    }

    /// Runs one line in this shell and answers with what the gate made of it.
    ///
    /// Resolving is *completion*, not acceptance: the line ran, its native workers were joined and
    /// the publication boundary was decided. `Ok` means and only means
    /// [`Outcome::Published`] — including for a line whose process exited nonzero, because a
    /// process status is not a publication verdict. A refusal by the policy is
    /// [`RunError::Policy`], carrying the same completion a receipt would have.
    ///
    /// The command runs on this collection's own runtime and is owned by it. Dropping this future
    /// abandons the *answer*, never the work: an admitted line keeps running, its effects are
    /// still gated, and its receipt still resolves for every other holder.
    ///
    /// One command at a time per shell: a second call while one is running is refused with
    /// [`MuxError::JobBusy`] rather than queued behind the interpreter.
    ///
    /// [`CommandOptions::on_accept`] is how a caller that must act *during* the command — feed
    /// standard input, signal it, watch its output — obtains the receipt without waiting here.
    ///
    /// # Errors
    ///
    /// Fails with [`RunError::Admission`] when the line was never accepted,
    /// [`RunError::Policy`] when the gate refused it, [`RunError::Unpublished`] when it was stale,
    /// discarded, detached or broken by infrastructure, and [`RunError::Wait`] when this caller
    /// lost the answer to a teardown or a lost producer.
    pub async fn run_command(
        &self,
        cmd: &str,
        options: CommandOptions,
    ) -> Result<Arc<CommandCompletion>, RunError> {
        let mux = self.mux().map_err(RunError::Admission)?;
        let (answer, wait) = tokio::sync::oneshot::channel();
        let shell = self.clone();
        let cmd = cmd.to_owned();
        let owner = Arc::clone(&mux);
        // On this collection's runtime, registered in its own task set: the work outlives this
        // call, so a caller that is a status thread, a foreign runtime or a detached queue must
        // not be what its pumps and its interpreter are tied to.
        mux.background().spawn_detached(&mux.runtime, async move {
            let outcome = execute_command(owner, shell, cmd, options).await;
            let _ = answer.send(outcome);
        });
        match wait.await {
            Ok(outcome) => outcome,
            // The producer is gone without an answer. Teardown is the one explanation that is not
            // a bug, and it is deliberately reported as itself: neither says the line had no
            // effects, so neither is permission to retry.
            Err(_) => Err(RunError::Wait(if mux.is_closing() {
                WaitError::Shutdown
            } else {
                WaitError::Aborted
            })),
        }
    }
}

/// Admits one line into `shell`, runs it, and resolves its boundary exactly once.
///
/// The whole of a command's life, on the collection's own runtime: admission, the idle-terminal
/// lease revocation, the reservation, the managed run, the gate, the receipt and the reclamation a
/// one-shot closure owes.
#[allow(
    clippy::too_many_lines,
    reason = "one command's admission, launch and conclusion are a single ordered sequence; \
              splitting it would scatter the reservation and reclamation obligations it carries"
)]
async fn execute_command(
    mux: Arc<ShellMux>,
    shell: Shell,
    cmd: String,
    options: CommandOptions,
) -> Result<Arc<CommandCompletion>, RunError> {
    let CommandOptions {
        on_finish,
        close_on_finish,
        on_accept,
    } = options;

    if mux.is_closing() {
        return Err(RunError::Admission(MuxError::ShuttingDown));
    }
    {
        let mut guard = shell.live_lock();
        let Some(live) = guard.as_mut() else {
            return Err(RunError::Admission(MuxError::StaleJob(
                shell.inner.id.clone(),
            )));
        };
        // As at creation: a line admitted against a half-applied seed would have real effects
        // before its gate could refuse it. This shell's own seed, never any other's.
        if live.executor.recovery_required() {
            return Err(RunError::Admission(MuxError::RecoveryRequired));
        }
        if live.close.is_some_and(JobCloseMode::explicit) {
            return Err(RunError::Admission(MuxError::JobClosing(
                shell.inner.id.clone(),
            )));
        }
        if live.command.is_some() || live.starting {
            return Err(RunError::Admission(MuxError::JobBusy(
                shell.inner.id.clone(),
            )));
        }
        if live.resources.is_none() {
            return Err(RunError::Admission(MuxError::JobNotReady(
                shell.inner.id.clone(),
            )));
        }
        live.starting = true;
        drop(guard);
    }
    mux.announce(FrontendEvent::Changed);

    // Outside every lock. The prompt that owns the lease may be the caller of this very command,
    // and it releases only after its own bookkeeping. `starting` is already set, so no further
    // lease can be granted while this waits.
    if let Some(state) = shell.lease() {
        state.revoke(Revocation::Run);
        // The restoration *result*, not an acknowledgement. A lease that could not put the
        // terminal back left it in the prompt's raw mode: echo off, every key unprocessed, every
        // special character disabled. Launching a command into that would start a program in a
        // mode nobody configured and which it has no way to discover. So the shell is closed
        // instead — the one outcome that leaves neither a permanently busy terminal nor a program
        // running blind.
        if let Err(error) = state.settled().await {
            {
                let mut guard = shell.live_lock();
                if let Some(live) = guard.as_mut() {
                    live.starting = false;
                    live.close = Some(JobCloseMode::Force);
                }
                drop(guard);
            }
            if let Some(live) = mux.take_closable(&shell) {
                mux.reclaim(&shell, live, None).await;
            }
            mux.announce(FrontendEvent::Changed);
            return Err(RunError::Admission(MuxError::Shared(error)));
        }
    }

    let (receipt, launched) = {
        let mut guard = shell.live_lock();
        let Some(live) = guard.as_mut() else {
            return Err(RunError::Admission(MuxError::StaleJob(
                shell.inner.id.clone(),
            )));
        };
        let Some(resources) = live.resources.as_ref() else {
            live.starting = false;
            return Err(RunError::Admission(MuxError::JobNotReady(
                shell.inner.id.clone(),
            )));
        };
        let interpreter = Arc::clone(&resources.shell);
        // A pipe shell is one-shot whatever the caller asked for: it has no prompt to return to
        // and no terminal to keep, so a second line would run in a shell with nothing left to
        // carry its output. Recorded on the *command* rather than on the shell, so standard input
        // stays writable until that command's own verdict releases the running slot.
        let one_shot = close_on_finish || !live.io.is_terminal();
        let mark = live.executor.spawn_record_count();
        let mut active = mux.reserve(&shell.inner.sandbox, &cmd, on_finish, one_shot, mark);
        let receipt = active.handle.clone();
        let launched = if live.close == Some(JobCloseMode::Force) {
            drop(interpreter);
            Launched::Retired
        } else {
            let context = CommandContext::new(
                active.id,
                shell.inner.sandbox.clone(),
                live.executor
                    .snapshot_root()
                    .map(std::path::Path::to_path_buf),
                mux.runtime.clone(),
            );
            active.context = Some(context.clone());
            active.running = true;
            Launched::Run(interpreter, context)
        };
        // Together with `starting`, under the one lock that admits the line: a view taken between
        // the two would report a shell that is neither starting nor running a command.
        live.starting = false;
        live.command = Some(active);
        drop(guard);
        (receipt, launched)
    };
    mux.announce(FrontendEvent::CommandAccepted { command: &receipt });
    // After admission and after the lease is back, before anything can be written or finished:
    // this is the observation a caller feeding standard input or signalling the line needs, and
    // it must never arrive late enough for the command to have ended first.
    if let Some(sender) = on_accept {
        let _ = sender.send(receipt);
    }
    mux.launched.notify_waiters();

    let (exit_code, outcome) = match launched {
        // The stop that retired this shell could not reclaim it: a shell with a reserved command
        // is deliberately not closable, because its snapshot is what that command would have run
        // in. Reclaiming it is therefore owed here, by the launch that decided the line never
        // runs — exactly as a completed line owes it below.
        Launched::Retired => (None, Ok(Outcome::Discarded)),
        Launched::Run(interpreter, context) => {
            mux.announce(FrontendEvent::Changed);
            match run_line(&shell, interpreter, &cmd, context).await {
                Ok((result, boundary)) => {
                    (Some(i32::from(u8::from(&result.exit_code))), Ok(boundary))
                }
                Err(error) => (None, Err(MuxError::from(error))),
            }
        }
    };

    let completion = mux.conclude_command(&shell, exit_code, outcome);
    if let Some(live) = mux.take_closable(&shell) {
        mux.reclaim(&shell, live, completion.clone()).await;
    }
    mux.announce(FrontendEvent::Changed);

    let Some(completion) = completion else {
        // The reservation was resolved by someone else, which only a teardown does.
        return Err(RunError::Wait(if mux.is_closing() {
            WaitError::Shutdown
        } else {
            WaitError::Aborted
        }));
    };
    // The gate's answer, mapped exactly once. A nonzero process status under an approved
    // publication is success; a zero status under a refusal is not.
    if matches!(completion.outcome.as_ref(), Ok(Outcome::Published { .. })) {
        Ok(completion)
    } else if matches!(completion.outcome.as_ref(), Ok(Outcome::Denied { .. })) {
        Err(RunError::Policy(PolicyError::new(completion)))
    } else {
        Err(RunError::Unpublished { completion })
    }
}

/// Runs one line in the shell's own interpreter and decides its publication boundary.
///
/// The snapshot is refreshed, the line run inside its managed context, that context's native
/// workers joined, and — once every process of it has exited — the boundary decided: a line whose
/// shell was retired by a forced stop while it ran is discarded unchecked; any other line is
/// concluded exactly as [`crate::Shell::run`] would have.
///
/// A line that asked the shell itself to exit — Brush's `exit` builtin, or its managed `exec`
/// failing over — is recorded on the running command's own closure decision, so the finalizer
/// closes the shell gracefully with that line's status instead of dropping the request.
async fn run_line(
    shell: &Shell,
    interpreter: Arc<crate::Shell>,
    cmd: &str,
    context: CommandContext,
) -> Result<(brush_core::ExecutionResult, Outcome), MarshError> {
    // The whole evaluation — refresh, run, join this command's native workers, decide the
    // boundary, and evaluate the line again if its reads were invalidated — belongs to the shell
    // below this layer. What is left here is the scheduling: the same logical command id, the
    // same receipt, the same admission, and a shell that asked to close.
    let (result, outcome) = with_context(
        context.clone(),
        interpreter.run_with_workers(cmd, context.workers()),
    )
    .await;

    if let Some(result) = &result {
        let mut live = shell.live_lock();
        // The shell's own `exit`, recorded exactly where a one-shot command's closure is: the
        // finalizer releases the running slot and marks the shell gracefully closing in one
        // acquisition, so nothing admitted afterwards can slip between this line's exit and that
        // closure. This is the *final* evaluation's control flow, and it is recorded whether or
        // not the boundary then succeeded; an internal unwind never reaches here, because the loop
        // below the mux consumed it.
        if matches!(
            result.next_control_flow,
            brush_core::ExecutionControlFlow::ExitShell
        ) && let Some(active) = live.as_mut().and_then(|live| live.command.as_mut())
        {
            active.close_on_finish = true;
        }
        drop(live);
    }

    // Before the caller's reclamation: a last `crate::Shell::drop` does blocking I/O, so this
    // clone must not be the one that dies on an async worker.
    drop(interpreter);
    // A boundary that broke reports no exit code at all: the line ran, and what it left was never
    // judged, so nothing about it is an approved result.
    outcome.map(|outcome| (result.unwrap_or_default(), outcome))
}

/// One shell as a caller sees it.
fn job_view(shell: &Shell, live: &LiveShell) -> JobView {
    JobView {
        id: shell.inner.id.clone(),
        sandbox: shell.inner.sandbox.clone(),
        io: live.io,
        working_directory: live.working_directory.clone(),
        snapshot_root: live
            .executor
            .snapshot_root()
            .map(std::path::Path::to_path_buf),
        running: live
            .command
            .as_ref()
            .filter(|active| active.running)
            .map(|active| RunningView {
                cmd: active.text.to_string(),
                id: active.id,
            }),
        starting: live.starting || live.resources.is_none(),
        closing: live.close.is_some_and(JobCloseMode::explicit),
    }
}

/// Refuses a terminal geometry with a zero dimension.
pub(crate) const fn validate_size(rows: u16, cols: u16) -> Result<(), MuxError> {
    if rows == 0 || cols == 0 {
        return Err(MuxError::InvalidTerminalSize { rows, cols });
    }
    Ok(())
}

/// One shell's output stream, whichever kind it is.
enum Reader {
    /// A pseudoterminal master, shared with the object that writes input to it.
    Terminal(Arc<AsyncFd<OwnedFd>>),
    /// A pipe's read end, owned outright by its pump.
    Pipe(AsyncFd<OwnedFd>),
}

impl Reader {
    /// Reads whatever is available, `0` meaning end of file.
    async fn read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Terminal(master) => read_terminal(master, buffer).await,
            Self::Pipe(pipe) => read_pipe(pipe, buffer).await,
        }
    }
}

/// Writes every byte of `bytes` to `fd`, retrying short writes.
async fn write_fd(fd: &AsyncFd<OwnedFd>, bytes: &[u8]) -> Result<(), MuxError> {
    let mut written = 0;
    while written < bytes.len() {
        // Tokio owns the readiness retry: a write the kernel refuses with `EAGAIN` clears the
        // descriptor's readiness and waits again, inside `async_io`.
        let attempt = fd
            .async_io(tokio::io::Interest::WRITABLE, |inner| {
                nix::unistd::write(inner, &bytes[written..]).map_err(std::io::Error::from)
            })
            .await;
        match attempt {
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(MuxError::Io(error)),
        }
    }
    Ok(())
}

/// Reads whatever a shell's terminal has produced into `buffer`, returning how many bytes.
///
/// Bytes are preserved exactly: escape sequences, non-UTF-8 output and a final line with no
/// newline all arrive as they were written. `0` is end of file, which on Linux is how a
/// pseudoterminal reports that its last writer is gone.
async fn read_terminal(terminal: &AsyncFd<OwnedFd>, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let attempt = terminal
            .async_io(tokio::io::Interest::READABLE, |inner| {
                match nix::unistd::read(inner, &mut *buffer) {
                    // A pseudoterminal master whose slave has been closed answers `EIO`. That is
                    // a hangup, not a failure: it is this stream's end of file. Only the syscall's
                    // own `EIO` is one — a readiness failure still propagates.
                    Err(nix::errno::Errno::EIO) => Ok(0),
                    other => other.map_err(std::io::Error::from),
                }
            })
            .await;
        match attempt {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// The path of the terminal behind `fd`, when the kernel names it.
fn terminal_name(fd: BorrowedFd<'_>) -> Option<PathBuf> {
    let mut buffer = [0 as libc::c_char; 128];
    // SAFETY: `ttyname_r` receives an open descriptor, a writable buffer and its exact length.
    let code = unsafe { libc::ttyname_r(fd.as_raw_fd(), buffer.as_mut_ptr(), buffer.len()) };
    if code != 0 {
        return None;
    }
    // SAFETY: `ttyname_r` returning zero guarantees a NUL-terminated string inside `buffer`.
    let name = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes())))
}

/// Carries one shell's output streams to the frontend, then announces the shell's end.
///
/// Every stream of a *live* shell is drained unconditionally, the shells nobody is looking at
/// included: a pseudoterminal whose master nobody reads fills its buffer and stops the command
/// writing into it, and a pipe does the same.
///
/// A shell a forced stop has retired is the one exception, and it has to be. Its work is
/// discarded, so its bytes have no consumer by definition — and draining them anyway is what keeps
/// an unstoppable producer running: a line whose writer is a *builtin* runs inline on this runtime,
/// so it has no process to kill and no cancellation point to reach, and a reader that keeps
/// politely emptying its pipe lets it write forever, occupying one worker thread for the rest of
/// the daemon's life. So `discarded` closes the read ends instead. The producer's next write
/// fails with `EPIPE`, which the interpreter propagates like any other I/O error, and the command
/// ends.
///
/// Nothing here holds a reference to the collection. The end of the shell is the end of its
/// resources: when every reader is done, this waits for the release the live half owned before
/// announcing [`FrontendEvent::Closed`], so a failed read alone never claims a live shell closed.
///
/// `runtime` is the mux's own, carried in rather than taken from `Handle::current()`, so that the
/// byte pumps are created on it too. This task is already on that runtime, but saying so
/// explicitly is what keeps the guarantee from depending on where the task set happens to be
/// polled.
async fn job_lifecycle(
    frontend: Arc<Mutex<dyn ShellFrontend>>,
    shell: Sandbox,
    readers: Vec<(OutputChannel, Reader)>,
    released: tokio::sync::oneshot::Receiver<Arc<JobEnd>>,
    closed: Arc<tokio::sync::watch::Sender<WaitState<JobEnd, WaitError>>>,
    mut discarded: tokio::sync::watch::Receiver<bool>,
    runtime: tokio::runtime::Handle,
) {
    let mut pumps = tokio::task::JoinSet::new();
    for (channel, reader) in readers {
        let frontend = Arc::clone(&frontend);
        let shell = shell.clone();
        pumps.spawn_on(
            async move { pump_stream(frontend, shell, channel, reader).await },
            &runtime,
        );
    }
    loop {
        tokio::select! {
            joined = pumps.join_next() => if joined.is_none() { break },
            // `changed` is the only wakeup that matters: the initial `false` is never reported,
            // and the sender outlives this task inside the shell, so an `Err` here means the shell
            // is gone and its readers should go with it.
            signal = discarded.changed() => {
                if signal.is_err() || *discarded.borrow_and_update() {
                    // Aborted rather than asked: a pump waiting on a frontend receipt has no
                    // cancellation point of its own, and every one of these owns a read end that
                    // has to be closed for the producer to be released. `shutdown` drops their
                    // futures, and the descriptors with them.
                    pumps.shutdown().await;
                    break;
                }
            }
        }
    }

    // The shell's own token, so this ends only once its resources are released — which the
    // collection does after the snapshot that shell named is gone.
    let end = released.await.unwrap_or_else(|_| {
        Arc::new(JobEnd {
            shell: shell.clone(),
            close_mode: None,
            completion: None,
            error: None,
            recovery_required: false,
        })
    });
    closed.send_replace(WaitState::Done(Arc::clone(&end)));
    let _ = notify(&frontend, FrontendEvent::Closed { end: &end });
}

/// Carries one stream to the frontend, honouring the receipts it returns.
///
/// A withheld receipt slows *this* stream and nothing else: the awaits happen outside every mux
/// lock and outside the frontend's own mutex, so another shell's output, another channel and every
/// control operation keep running while this one waits.
async fn pump_stream(
    frontend: Arc<Mutex<dyn ShellFrontend>>,
    shell: Sandbox,
    channel: OutputChannel,
    reader: Reader,
) {
    let mut buffer = [0u8; CHUNK];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                let receipt = notify(
                    &frontend,
                    FrontendEvent::Output {
                        shell: &shell,
                        channel,
                        bytes: &buffer[..count],
                    },
                );
                if let Some(receipt) = receipt
                    && receipt.await.is_err()
                {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "frontend output consumer stopped",
                    );
                    let _ = notify(
                        &frontend,
                        FrontendEvent::IoError {
                            shell: &shell,
                            channel,
                            error: &error,
                        },
                    );
                    break;
                }
            }
            Err(error) => {
                let _ = notify(
                    &frontend,
                    FrontendEvent::IoError {
                        shell: &shell,
                        channel,
                        error: &error,
                    },
                );
                break;
            }
        }
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellmux::ids::{JobDir, SnapshotUid};

    /// A shell with no resources: none of these tests reaches a terminal or an interpreter.
    ///
    /// Real objects, because a retained [`Shell`] is what every mutation now acts through, so the
    /// identity these carry is the identity the registry would actually check.
    fn shell(id: &str, close: Option<JobCloseMode>, starting: bool) -> Shell {
        let sandbox = Sandbox {
            id: ShellId::from(id),
            seed: PathBuf::from("/seed"),
            dir: JobDir::default(),
            uid: SnapshotUid::from(format!("uid-{id}")),
        };
        let live = LiveShell {
            executor: crate::MarshExecutor::default(),
            validator: Arc::new(Mutex::new(crate::PolicyValidator::new())),
            io: JobIo::Terminal {
                geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
            },
            working_directory: PathBuf::new(),
            resources: None,
            command: None,
            starting,
            close,
            release: None,
        };
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(WaitState::Pending);
        Shell {
            inner: Arc::new(ShellState {
                id: sandbox.id.clone(),
                sandbox,
                terminal: true,
                origin: Weak::new(),
                endpoints: OnceLock::new(),
                closed_tx: Arc::new(closed_tx),
                closed: closed_rx,
                discarded: Arc::new(tokio::sync::watch::channel(false).0),
                live: Mutex::new(Some(live)),
            }),
        }
    }

    /// How long a descriptor test may wait on the kernel before it is a failure rather than a
    /// hang.
    const IO_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// Fills the pipe behind `fd` until the kernel refuses, answering with what it accepted.
    ///
    /// The capacity is read back rather than assumed: it is a kernel property, and a test that
    /// hard-coded a size would stop filling the pipe the day it changed.
    fn prefill(fd: &OwnedFd) -> Vec<u8> {
        let capacity =
            nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETPIPE_SZ).expect("the pipe's capacity");
        let capacity = usize::try_from(capacity).expect("a pipe capacity is not negative");
        let chunk = vec![b'P'; 4096.min(capacity)];
        let mut accepted = Vec::new();
        loop {
            match nix::unistd::write(fd, &chunk) {
                Ok(0) => break,
                Ok(count) => accepted.extend_from_slice(&chunk[..count]),
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(nix::errno::Errno::EINTR) => {}
                Err(other) => panic!("the prefill write failed: {other}"),
            }
        }
        accepted
    }

    /// Keystrokes and one-shot stdin both reach a shell through `write_fd`, and a full descriptor
    /// must delay them rather than truncate them: the reader sees the prefix that was already
    /// there and then every byte of the payload, in order.
    #[tokio::test]
    async fn terminal_write_helper_survives_backpressure() {
        let pipes = crate::shellmux::pipes::open_pipes().expect("three pipes");
        let prefilled = prefill(&pipes.input);
        assert!(!prefilled.is_empty(), "an empty pipe accepts something");
        let payload: Vec<u8> = (0..prefilled.len() * 2)
            .map(|index| u8::try_from(index % 251).unwrap_or(0))
            .collect();

        // Owned here, so a failure or a timeout below still drops it and lets the reader finish.
        let writer = AsyncFd::new(pipes.input).expect("a registered write end");
        let child_stdin = pipes.child_stdin;
        let drain = tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let mut reader = std::fs::File::from(child_stdin);
            let mut collected = Vec::new();
            reader
                .read_to_end(&mut collected)
                .expect("the reader drains to end of file");
            collected
        });

        tokio::time::timeout(IO_LIMIT, write_fd(&writer, &payload))
            .await
            .expect("the write finishes once the reader drains")
            .expect("a full descriptor is backpressure, not a failure");
        drop(writer);

        let collected = tokio::time::timeout(IO_LIMIT, drain)
            .await
            .expect("the reader sees end of file")
            .expect("the reader task");
        let expected: Vec<u8> = prefilled.iter().chain(&payload).copied().collect();
        assert_eq!(collected.len(), expected.len());
        assert_eq!(collected, expected, "every byte, in order");
    }

    /// A pseudoterminal reports the loss of its last writer as `EIO`. That is this stream's end
    /// of file, and reporting it as an error instead would make every closing shell look broken.
    #[tokio::test]
    async fn a_terminal_read_ends_when_its_slave_is_gone() {
        let (master, slave) =
            crate::shellmux::pty::open_pty(24, 80).expect("a private pseudoterminal");
        let terminal = AsyncFd::new(master).expect("a registered master");
        drop(slave);

        let mut buffer = [0_u8; 32];
        let count = tokio::time::timeout(IO_LIMIT, read_terminal(&terminal, &mut buffer))
            .await
            .expect("the read resolves")
            .expect("a hangup is not a read failure");
        assert_eq!(count, 0, "the hangup is this stream's end of file");
    }

    /// A reservation for a shell that is running a command.
    fn reserve(id: u64, text: &str, sandbox: &Sandbox) -> ActiveCommand {
        let (verdict, watch) = tokio::sync::watch::channel(WaitState::Pending);
        let text: Arc<str> = Arc::from(text);
        ActiveCommand {
            id: CommandId(id),
            handle: CommandHandle {
                id: CommandId(id),
                shell: sandbox.clone(),
                text: Arc::clone(&text),
                state: watch,
            },
            text,
            verdict: Arc::new(verdict),
            on_finish: None,
            close_on_finish: false,
            spawn_mark: 0,
            context: None,
            running: true,
        }
    }

    /// Installs `command` in `shell`'s live half.
    fn admit(shell: &Shell, command: ActiveCommand) {
        let mut guard = shell.live_lock();
        guard.as_mut().expect("the fixture shell is live").command = Some(command);
        drop(guard);
    }

    /// Runs `body` against `shell`'s live half.
    fn with_live<T>(shell: &Shell, body: impl FnOnce(&LiveShell) -> T) -> T {
        let guard = shell.live_lock();
        let answer = body(guard.as_ref().expect("the fixture shell is live"));
        drop(guard);
        answer
    }

    /// A shell name is a capability principal. Reusing one while its holder is alive would make
    /// two sandboxes indistinguishable in the policy history, so the series steps over a name a
    /// shell already holds rather than colliding with it.
    #[test]
    fn the_automatic_series_never_collides_with_a_live_name() {
        let mut registry = ShellRegistry::new(24, 80);
        for name in ["1", "2"] {
            let held = shell(name, None, false);
            registry.insert(held.principal().clone(), held);
        }

        assert_eq!(registry.next_id(), ShellId::from("3"));
        assert_eq!(
            registry.next_id(),
            ShellId::from("4"),
            "the series is monotonic within a session, so a closed name is never handed out twice"
        );
    }

    /// The registry keeps creation order beside the map, because a front-end's list of shells is
    /// the order they were opened in and a hash map has no order at all.
    #[test]
    fn the_registry_keeps_creation_order_beside_its_map() {
        let mut registry = ShellRegistry::new(24, 80);
        for name in ["alpha", "beta", "gamma"] {
            let held = shell(name, None, false);
            registry.insert(held.principal().clone(), held);
        }
        registry.remove(ShellId::from("beta").principal());

        let names: Vec<String> = registry
            .order
            .iter()
            .map(|principal| principal.as_str().to_owned())
            .collect();
        assert_eq!(names, vec!["alpha".to_owned(), "gamma".to_owned()]);
        assert!(
            !registry
                .shells
                .contains_key(ShellId::from("beta").principal()),
            "removal takes the shell out of both halves of the registry"
        );
    }

    /// Taking a live half out *is* the reclamation: the executor, the descriptors and the private
    /// interpreter all go with it, however many objects still name the shell.
    #[test]
    fn taking_the_live_half_leaves_the_retained_object_dead() {
        let held = shell("api", Some(JobCloseMode::Graceful), false);
        let retained = held.clone();

        assert!(held.take_live().is_some());
        assert!(
            retained.take_live().is_none(),
            "a second taker finds the generation already reclaimed"
        );
        match retained.view() {
            Err(MuxError::StaleJob(id)) => assert_eq!(id, ShellId::from("api")),
            other => panic!("a reclaimed generation must be stale, got {other:?}"),
        }
        assert!(
            matches!(retained.keep(), Err(MuxError::StaleJob(_))),
            "a reclaimed generation answers stale rather than reaching a replacement"
        );
    }

    /// A refused callback is destroyed only after the live-state lock is released.
    ///
    /// What a callback captures is arbitrary code: the natural thing for a caller to keep in one
    /// is the shell it is registering against. Dropping the refusal inside the closure — which is
    /// what moving it in there would do — would run that destructor under this shell's own mutex,
    /// and the first thing it asks the shell would never answer.
    #[test]
    fn a_refused_on_finish_capture_is_dropped_after_the_live_unlock() {
        /// What the capture's destructor saw when it ran.
        #[derive(Debug)]
        enum Observed {
            /// The live state was still locked, so the shell could not have answered anything.
            Locked,
            /// The shell answered the destructor's query.
            Queried(Result<ShellId, MuxError>),
        }

        /// A capture that queries the shell that refused it, from its own destructor.
        struct Probe {
            /// The shell the refusal came from.
            shell: Shell,
            /// Where the destructor reports what it saw.
            observed: Arc<Mutex<Option<Observed>>>,
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                // `try_lock` first: a destructor running under the live lock must fail the test
                // promptly rather than deadlock it. The probe guard is released immediately —
                // `view` takes the same lock for itself.
                let seen = match self.shell.inner.live.try_lock() {
                    Ok(guard) => {
                        drop(guard);
                        Observed::Queried(self.shell.view().map(|view| view.id))
                    }
                    Err(_) => Observed::Locked,
                };
                *self.observed.lock().unwrap_or_else(PoisonError::into_inner) = Some(seen);
            }
        }

        /// One offered callback, and the two things the test learns from it.
        struct Refusal {
            /// The callback offered to the shell.
            callback: OnFinish,
            /// What the capture's destructor saw when it ran.
            observed: Arc<Mutex<Option<Observed>>>,
            /// Whether the callback's own body ever ran.
            called: Arc<Mutex<bool>>,
        }

        /// A callback whose capture queries `shell`, and which records that it ever ran.
        fn refused(shell: &Shell) -> Refusal {
            let observed = Arc::new(Mutex::new(None));
            let called = Arc::new(Mutex::new(false));
            let probe = Probe {
                shell: shell.clone(),
                observed: Arc::clone(&observed),
            };
            let ran = Arc::clone(&called);
            Refusal {
                callback: Box::new(move |_status| {
                    drop(probe);
                    *ran.lock().unwrap_or_else(PoisonError::into_inner) = true;
                }),
                observed,
                called,
            }
        }

        // Live, but with nothing running: the slot is refused and the shell still answers.
        let idle = shell("idle", None, false);
        let offered = refused(&idle);
        assert_eq!(idle.on_finish(offered.callback).ok(), Some(false));
        let seen = offered
            .observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        assert!(
            matches!(&seen, Some(Observed::Queried(Ok(id))) if *id == ShellId::from("idle")),
            "the destructor ran outside the lock and the shell answered it, got {seen:?}"
        );
        assert!(
            !*offered
                .called
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        );

        // Reclaimed: the registration fails, and the refusal is still dropped outside the lock.
        let stale = shell("stale", None, false);
        let offered = refused(&stale);
        assert!(stale.take_live().is_some());
        assert!(matches!(
            stale.on_finish(offered.callback),
            Err(MuxError::StaleJob(_))
        ));
        let seen = offered
            .observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        assert!(
            matches!(&seen, Some(Observed::Queried(Err(MuxError::StaleJob(_))))),
            "a reclaimed generation answers its own destructor, got {seen:?}"
        );
        assert!(
            !*offered
                .called
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        );
    }

    /// A one-shot closure lives on the *command*, not on the shell, so standard input stays
    /// writable for the whole of that command. Marking the shell closing at admission would reject
    /// the stdin a pipe reader is waiting for.
    #[tokio::test]
    async fn a_one_shot_command_leaves_its_shell_open_to_input() {
        let held = shell("piped", None, false);
        let sandbox = held.sandbox().clone();
        let mut command = reserve(1, "cat", &sandbox);
        command.close_on_finish = true;
        admit(&held, command);

        assert!(
            with_live(&held, |live| live.close.is_none()),
            "the shell itself is not closing while its one-shot command runs"
        );
        assert!(
            !matches!(held.write_input(b"x").await, Err(MuxError::JobClosing(_))),
            "input is never refused because of a closure that has not happened yet"
        );
    }

    /// A shell that is *not* closing is not reclaimable at all: closure is a decision, and nothing
    /// makes it on a caller's behalf.
    #[test]
    fn a_shell_nobody_closed_stays_open() {
        let held = shell("kept", None, false);
        assert!(with_live(&held, |live| live.close.is_none()));
    }

    /// The `1`, `2`, … series implies closure; a reader's `stop` and a one-shot command's own
    /// option *decide* it. Only the first may be cancelled by `keep`.
    #[test]
    fn only_the_automatic_closure_is_a_default() {
        assert!(!JobCloseMode::Automatic.explicit());
        assert!(JobCloseMode::Graceful.explicit());
        assert!(JobCloseMode::Force.explicit());
    }

    /// `keep` cancels the automatic closure and nothing else: an explicit stop is a decision it
    /// may not quietly revoke.
    #[test]
    fn keep_cancels_only_the_automatic_closure() {
        let automatic = shell("auto", Some(JobCloseMode::Automatic), false);
        assert_eq!(automatic.keep().ok(), Some(true));
        assert!(with_live(&automatic, |live| live.close.is_none()));

        let stopped = shell("stopped", Some(JobCloseMode::Graceful), false);
        assert_eq!(stopped.keep().ok(), Some(false));
        assert!(with_live(&stopped, |live| live.close == Some(JobCloseMode::Graceful)));
    }

    /// A geometry with a zero dimension is not a small terminal, it is one no program can draw
    /// into. It is refused before anything is opened at it.
    #[test]
    fn a_zero_dimension_is_not_a_terminal_size() {
        assert!(validate_size(24, 80).is_ok());
        assert!(matches!(
            validate_size(0, 80),
            Err(MuxError::InvalidTerminalSize { rows: 0, cols: 80 })
        ));
        assert!(matches!(
            validate_size(24, 0),
            Err(MuxError::InvalidTerminalSize { rows: 24, cols: 0 })
        ));
    }

    /// A view reports what a caller can act on. A shell whose resources are still being built is
    /// `starting` even when no command was submitted, because nothing can be sent to it yet.
    #[test]
    fn a_view_reports_readiness_rather_than_existence() {
        let opening = shell("opening", None, false);
        assert!(
            opening.view().expect("a live shell").starting,
            "a shell with no streams cannot take input, whatever its flags say"
        );

        let running = shell("running", None, false);
        let sandbox = running.sandbox().clone();
        admit(&running, reserve(7, "make", &sandbox));
        let view = running.view().expect("a live shell");
        assert_eq!(
            view.running.as_ref().map(|running| running.cmd.as_str()),
            Some("make")
        );
        assert_eq!(view.running.map(|running| running.id), Some(CommandId(7)));
        assert!(!view.closing);
    }

    /// A reserved-but-not-yet-launched command is not *running*: a front-end that showed it as
    /// running would be reporting a process that does not exist.
    #[test]
    fn a_reserved_command_is_not_reported_as_running() {
        let reserved = shell("reserved", None, true);
        let sandbox = reserved.sandbox().clone();
        let mut slot = reserve(3, "echo hi", &sandbox);
        slot.running = false;
        admit(&reserved, slot);

        let view = reserved.view().expect("a live shell");
        assert!(view.running.is_none());
        assert!(view.starting);
    }

    /// The whole point of the output receipt: a frontend that withholds one stops that stream
    /// being read, rather than being handed bytes faster than it can take them.
    ///
    /// The pump must not read the second chunk until the first chunk's receipt is completed, and
    /// it must resume immediately once it is.
    #[allow(
        clippy::too_many_lines,
        reason = "one ordered receipt-backpressure scenario; splitting it would hide the ordering it asserts"
    )]
    #[tokio::test]
    async fn a_withheld_receipt_stops_the_stream_it_belongs_to() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counts deliveries and hands every one of them a receipt the test controls.
        struct Gate {
            /// How many output chunks have been delivered.
            delivered: Arc<AtomicUsize>,
            /// The receipts, in delivery order.
            receipts: Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>,
        }

        impl ShellFrontend for Gate {
            fn new(_rows: u16, _cols: u16) -> Self {
                unreachable!("built directly by the test")
            }
            fn size(&self) -> (u16, u16) {
                (24, 80)
            }
            fn bind(&mut self, _mux: Weak<ShellMux>) {}
            fn update(
                &mut self,
                event: FrontendEvent<'_>,
            ) -> Option<tokio::sync::oneshot::Receiver<()>> {
                match event {
                    FrontendEvent::Output { .. } => {
                        self.delivered.fetch_add(1, Ordering::AcqRel);
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        self.receipts
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(sender);
                        Some(receiver)
                    }
                    _ => None,
                }
            }
        }

        let delivered = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let frontend: Arc<Mutex<dyn ShellFrontend>> = Arc::new(Mutex::new(Gate {
            delivered: Arc::clone(&delivered),
            receipts: Arc::clone(&receipts),
        }));

        let pipes = crate::shellmux::pipes::open_pipes().expect("a pipe pair");
        let reader = Reader::Pipe(AsyncFd::new(pipes.stdout).expect("a registered read end"));
        let writer = pipes.child_stdout;
        drop(pipes.child_stdin);
        drop(pipes.child_stderr);
        drop(pipes.input);
        drop(pipes.stderr);

        let sandbox = Sandbox {
            id: ShellId::from("gated"),
            seed: PathBuf::from("/seed"),
            dir: JobDir::default(),
            uid: SnapshotUid::from("uid-gated"),
        };
        let pump = tokio::spawn(pump_stream(
            Arc::clone(&frontend),
            sandbox,
            OutputChannel::Stdout,
            reader,
        ));

        // Two writes, so the pump has something to read again the instant it is allowed to.
        let write = |bytes: &'static [u8]| {
            let fd = writer.try_clone().expect("a duplicate write end");
            std::thread::spawn(move || {
                use std::io::Write as _;
                let mut file = std::fs::File::from(fd);
                file.write_all(bytes).expect("the pipe accepts the write");
            })
        };
        write(b"first").join().expect("the first write completes");

        // One delivery, and then nothing: the receipt is still outstanding.
        let first = loop {
            let popped = {
                let mut held = receipts.lock().unwrap_or_else(PoisonError::into_inner);
                held.pop()
            };
            if let Some(receipt) = popped {
                break receipt;
            }
            tokio::task::yield_now().await;
        };
        write(b"second").join().expect("the second write completes");
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            delivered.load(Ordering::Acquire),
            1,
            "the second chunk must not be delivered while the first receipt is withheld"
        );

        // Released: the stream resumes on its own, without anything else prompting it.
        let _ = first.send(());
        loop {
            if delivered.load(Ordering::Acquire) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Closing every writer ends the stream, which ends the pump.
        drop(writer);
        let second = loop {
            let popped = {
                let mut held = receipts.lock().unwrap_or_else(PoisonError::into_inner);
                held.pop()
            };
            if let Some(receipt) = popped {
                break receipt;
            }
            tokio::task::yield_now().await;
        };
        let _ = second.send(());
        pump.await.expect("the pump ends when its stream does");
    }
}
