//! The complete system-I/O interface this daemon exposes in process.
//!
//! # The layers, and who owns what
//!
//! ```text
//!   one leased btrfs seed
//!        └── one ShellMux            the shells, their snapshots, the publication gate
//!             └── one ShellIo        this facade: admission, ownership, observation
//!                  ├── ShellHandle   one generation of one named job
//!                  ├── Execution     one managed command with real pipes
//!                  ├── IoEventStream one observer's bounded view
//!                  └── SDK handles   rmux sessions, windows, panes over this host's socket
//! ```
//!
//! One leased seed, one managed multiplexer, one server and its I/O facade, and then as many
//! shell, execution and observer handles as an application cares to hold. There is exactly one of
//! each of the first three and no way to make a second: the seed's lease is exclusive, so a second
//! mux over it cannot open at all, and a second facade over one mux would be a second admission
//! path with its own idea of what is live. Handles are the plural layer — cloneable,
//! generation-bound, and owning nothing the service does not already own.
//!
//! # Every capability, and the route to it
//!
//! | Multiplexer capability | Public application route |
//! |---|---|
//! | Construction, seed/lease ownership | [`RmuxFrontend::open`](crate::RmuxFrontend::open) plus [`ShellIo::executor_info`]; no raw spawner escape |
//! | Default directory, policy history | [`default_dir`](ShellIo::default_dir), [`history`](ShellIo::history) |
//! | Jobs, one job, current selection | [`snapshot`](ShellIo::snapshot), [`jobs`](ShellIo::jobs), [`job`](ShellIo::job), [`current_job`](ShellIo::current_job), [`shell`](ShellIo::shell) |
//! | Spawn and initial-command lifetime | [`spawn`](ShellIo::spawn), [`ShellHandle::initial_command`], [`keep`](ShellIo::keep) |
//! | Submit a line and finish callback | [`start_in`](ShellIo::start_in), [`CommandHandle::wait`](marsh_core::shellmux::CommandHandle::wait), [`on_finish`](ShellIo::on_finish) |
//! | Selection, graceful/forced stop | [`switch`](ShellIo::switch), [`stop`](ShellIo::stop), [`ShellHandle::wait_closed`] |
//! | Raw input and global resize | [`write_input`](ShellIo::write_input), [`resize`](ShellIo::resize), [`resize_all`](ShellIo::resize_all) |
//! | Frontend output, lifecycle and errors | [`observe`](ShellIo::observe), [`output`](ShellIo::output), typed completion and closure watches |
//! | Shutdown | [`ShellIo::shutdown`], and the owning frontend's `wait`/`shutdown` |
//! | Additional managed process I/O | [`execute`](ShellIo::execute), separate pipe streams, input end-of-file, [`signal`](ShellIo::signal), bounded collection |
//! | Rich rmux operations | SDK [`Session`](rmux_sdk::Session), [`Window`](rmux_sdk::Window) and [`Pane`](rmux_sdk::Pane) handles, and [`open_protocol`](ShellIo::open_protocol) |
//!
//! That table is the whole surface. There is no `mux()`, no executor accessor and no raw job
//! descriptor beside it, so a capability missing from the right-hand column is a capability this
//! facade does not grant rather than one reachable another way.
//!
//! # What a result proves
//!
//! | Observation | What it proves | What it does **not** prove |
//! |---|---|---|
//! | `exit_code == Some(0)` | the process exited zero | nothing reached the seed |
//! | [`Outcome::Published`](marsh_core::Outcome::Published) | the line's staged changes are in the seed | nothing about the exit code |
//! | [`Outcome::Denied`](marsh_core::Outcome::Denied) | the policy refused a capability | nothing about the exit code |
//! | [`Outcome::Stale`](marsh_core::Outcome::Stale) | another principal won a path first | that a retry will fail |
//! | [`Outcome::Discarded`](marsh_core::Outcome::Discarded) | the work was thrown away unchecked | that it never ran or spawned |
//! | pipe EOF | the program's write ends are gone | the command finished |
//! | `Finished` | the gate decided | the job closed |
//! | `Closed` | every stream ended and the snapshot was reclaimed | that anything was published |
//! | `WaitError::Shutdown` | teardown pre-empted an ordinary result | that nothing happened |
//! | `WaitError::Aborted` | the producer was lost | that nothing happened — do not auto-retry |
//!
//! Bytes observed live are **provisional**. A command's output is real output; whether the
//! filesystem changes behind it survive is the gate's answer, and only [`Outcome::Published`](marsh_core::Outcome::Published) is
//! that answer.
//!
//! A denial outlives the process that earned it. The durable log records each transaction's
//! granted capabilities together with the snapshot id that earned them, and reopening the seed
//! reinstalls that history before any shell can run a line — so [`history`](ShellIo::history) is
//! a property of the seed rather than of this daemon, and a restart does not hand the next
//! caller a clean slate. An owner is named by its **snapshot id**, never by a job name: a name
//! comes back when a pane index is reused or a daemon renumbers from one, and naming owners that
//! way would let the next holder inherit the last holder's stake. A live denial therefore reads
//! `owned is unstaged by 1` while that job is alive and `owned is unstaged by 4fe65b15` after a
//! restart, naming the same stake both times.
//!
//! # The trust boundary
//!
//! "Approved" means marsh's publication gate, not OS confinement. Staged regular-file changes
//! inside a job's snapshot are published only when every capability the line requested was
//! granted. What is *not* claimed:
//!
//! * The daemon's own socket, pseudoterminal allocation, IPC framing, configuration bootstrap and
//!   write-ahead-log handling are trusted infrastructure. They are not recursively executed as
//!   workloads, and they are not gated.
//! * A workload can still reach the network, and can still write outside the seed if it names an
//!   absolute path there. The executor observes; it does not confine.
//! * There is no ungated escape hatch in this facade: no raw executor, no raw job descriptor for
//!   I/O, no mutable validator, no CLI subprocess runner, and no file-save helper that bypasses
//!   the gate.
//!
//! # Terminal jobs and pipe jobs
//!
//! A terminal job is a pseudoterminal: one merged output stream, a queryable size, raw mode,
//! terminal replies, and no end-of-file a writer can send. A pipe job is three real pipes:
//! byte-exact independent stdout and stderr with no promised relative order, and a real
//! end-of-file through [`InputWriter::close`](crate::io::execution::InputWriter::close). [`ShellIo::execute`] always uses pipes, because a
//! workload's output is data.

pub mod builtins;
pub mod events;
pub mod execution;
/// The single consumer that drains the engine's observations into this daemon.
///
/// Crate-internal: it is this server's own wiring between the core's frontend queue and its
/// handlers, not something an application drives. Applications observe through
/// [`ShellIo::observe`].
#[cfg(any(unix, windows))]
pub(crate) mod observation;
pub mod protocol;
/// Which runtime a mutation's work ends up on when the caller is not this daemon.
#[cfg(test)]
mod runtime_tests;
mod streams;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use marsh_core::shellmux::{
    CommandHandle, CommandOptions, ExecutorInfo, JobEnd, JobView, MuxError, OutputChannel,
    ShellId, Spawned, SpawnOptions, TerminalGeometry, WaitError,
};

pub use events::{IoEnvelope, IoEvent, IoEventStream, IoPhase, IoSnapshot, Observation};
pub use execution::{
    CapturedOutput, CollectOptions, Execution, ExecutionParts, ExecutionSpec, InputWriter,
    OutputLimit, OutputStream, OverflowPolicy,
};

/// Everything this facade can answer with.
pub type IoResult<T> = Result<T, IoError>;

/// What went wrong.
///
/// Every variant keeps its source rather than a formatted string, so a caller can match on the
/// underlying failure. A *successfully computed* denial is deliberately **not** in here: a denied,
/// stale or discarded command is a completion value, and only an adapter that requires an approved
/// result turns one into [`IoError::Unapproved`].
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// The shell multiplexer refused or failed.
    #[error(transparent)]
    Mux(Arc<MuxError>),
    /// An rmux SDK operation failed.
    #[error(transparent)]
    Sdk(Arc<rmux_sdk::RmuxError>),
    /// An rmux client operation failed.
    #[error(transparent)]
    Client(Arc<rmux_client::ClientError>),
    /// A protocol request or response was rejected.
    #[error(transparent)]
    Protocol(Arc<rmux_proto::RmuxError>),
    /// Transport I/O failed.
    #[error(transparent)]
    Transport(Arc<std::io::Error>),
    /// A wait ended without a verdict.
    #[error(transparent)]
    Wait(#[from] WaitError),
    /// The service has closed and admits no work.
    #[error("the rmux I/O service is closed")]
    Closed,
    /// A handle from a different host was passed to this one.
    #[error("that handle belongs to a different rmux host")]
    WrongHost,
    /// The job has no rmux surface: a hidden pipe helper, or a popup-only job.
    ///
    /// Deliberately an error rather than a fabricated pane: a caller that acted on an invented
    /// pane id would be addressing something that does not exist.
    #[error("that job has no rmux pane")]
    NoPresentation,
    /// An observer fell behind and lost events.
    #[error("observer lagged: expected sequence {expected}, resuming at {resume}")]
    Lagged {
        /// The sequence the observer wanted next.
        expected: u64,
        /// The oldest sequence still available.
        resume: u64,
    },
    /// A bounded collection exceeded its limit under [`OverflowPolicy::Error`].
    #[error("collected output exceeded {limit} bytes")]
    OutputLimit {
        /// The limit that was exceeded.
        limit: usize,
    },
    /// A caller that required an approved result did not get one.
    ///
    /// The completion is carried whole: the exit code, the outcome and the command's text are all
    /// still inspectable, because "the program exited zero but the policy refused it" is a
    /// different fact from "the program failed".
    #[error("the command completed without an approved publication")]
    Unapproved {
        /// The completion that was not approved.
        completion: Arc<marsh_core::shellmux::CommandCompletion>,
    },
}

impl From<MuxError> for IoError {
    fn from(error: MuxError) -> Self {
        Self::Mux(Arc::new(error))
    }
}

impl From<rmux_sdk::RmuxError> for IoError {
    fn from(error: rmux_sdk::RmuxError) -> Self {
        Self::Sdk(Arc::new(error))
    }
}

impl From<rmux_client::ClientError> for IoError {
    fn from(error: rmux_client::ClientError) -> Self {
        Self::Client(Arc::new(error))
    }
}

impl From<rmux_proto::RmuxError> for IoError {
    fn from(error: rmux_proto::RmuxError) -> Self {
        Self::Protocol(Arc::new(error))
    }
}

impl From<std::io::Error> for IoError {
    fn from(error: std::io::Error) -> Self {
        Self::Transport(Arc::new(error))
    }
}

/// A handle on one generation of one named job.
///
/// Opaque and generation-bound. A name is resolved exactly once, in [`ShellIo::shell`]; every
/// action afterwards validates the retained instance inside the core's own admission, atomically
/// with the action it authorises. A handle whose job has closed can never reach a job that later
/// took its name.
#[derive(Clone, Debug)]
pub struct ShellHandle {
    /// The host this handle came from, so one passed to another host is refused.
    origin: Arc<IoService>,
    /// The core's generation-bound handle.
    job: Spawned,
}

impl ShellHandle {
    /// The job's identity, which is also its capability principal.
    #[must_use]
    pub fn id(&self) -> &ShellId {
        self.job.id()
    }

    /// The sandbox its commands run in: identity, directory label and snapshot id.
    #[must_use]
    pub fn sandbox(&self) -> &marsh_core::shellmux::Sandbox {
        self.job.sandbox()
    }

    /// Which output streams this job can produce. Fixed when it was admitted.
    ///
    /// A terminal job answers `[Terminal]` and nothing else, and that is a statement about the
    /// *descriptors*, not about this facade's plumbing: the program's standard output and
    /// standard error are the same pseudoterminal, so they were never two streams and nothing
    /// downstream can separate them again. The terminal's own replies are mixed in with them.
    /// A pipe job answers `[Stdout, Stderr]`, which are genuinely independent — byte-exact,
    /// separately ordered, and with no relative order promised between the two.
    #[must_use]
    pub fn output_channels(&self) -> &'static [OutputChannel] {
        self.job.output_channels()
    }

    /// The command this job was opened for, if it was opened for one.
    ///
    /// Available the instant the spawn returns; it never answers `None` merely because a scheduled
    /// launch has not run yet.
    #[must_use]
    pub fn initial_command(&self) -> Option<CommandHandle> {
        self.job.initial_command()
    }

    /// Waits for this job to close: every stream ended, snapshot reclaimed.
    ///
    /// A later boundary than any command finishing.
    ///
    /// # Errors
    ///
    /// Fails with [`WaitError::Shutdown`] when the host was torn down first, and
    /// [`WaitError::Aborted`] when the producer was lost.
    pub async fn wait_closed(&self) -> Result<Arc<JobEnd>, WaitError> {
        self.job.wait_closed().await
    }

    /// The core handle, for this crate's own handlers.
    ///
    /// Read-only server probes borrow this: the terminal's name, its foreground process group.
    /// It is reached from `pane_terminals`, which still allocates its own pseudoterminal and so
    /// has nothing to probe yet.
    #[allow(dead_code, reason = "consumed when PaneTerminal is backed by a ShellHandle")]
    pub(crate) const fn spawned(&self) -> &Spawned {
        &self.job
    }
}

/// The shared service state behind every [`ShellIo`] clone.
pub(crate) struct IoService {
    /// The one mux, until teardown releases it.
    ///
    /// Cleared at shutdown so the last strong reference to the core — and through it the seed's
    /// lease — is dropped even while facade clones are still held. What a caller can still read
    /// afterwards is the frozen metadata below.
    mux: std::sync::Mutex<Option<Arc<marsh_core::shellmux::ShellMux>>>,
    /// Executor metadata and policy history, captured before the mux is released.
    frozen: std::sync::Mutex<Frozen>,
    /// The observation bus.
    bus: events::EventBus,
    /// Where the service is in its life.
    phase: std::sync::Mutex<IoPhase>,
    /// The daemon's own runtime, captured once.
    ///
    /// Every managed operation is admitted and every task created on this handle, so a call
    /// arriving from a status thread, a foreign runtime or a detached queue still lands on the
    /// daemon's own runtime rather than wherever the caller happened to be.
    runtime: tokio::runtime::Handle,
    /// Live activity: native client leases, accepted operations and admitted jobs.
    activity: Activity,
    /// This host's socket, so every SDK connection is pinned to it rather than discovered.
    socket: PathBuf,
    /// Retained bytes and registered readers, per job stream.
    streams: streams::Streams,
    /// Which rmux surface presents each job generation.
    ///
    /// Keyed by snapshot id, never by name or index: a pane can be moved, linked or renamed, and
    /// a name can be reused, so either of those would eventually address the wrong thing.
    routes: std::sync::Mutex<std::collections::HashMap<marsh_core::shellmux::SnapshotUid, Route>>,
    /// The one selection the rmux surface and the engine have agreed on.
    ///
    /// Written by whichever side is about to propagate a change and read by the other side when
    /// it observes that change arriving, which is what keeps the two directions from handing one
    /// selection back and forth forever. Stored as a stable instance — one snapshot id presented
    /// by one stable pane id — because an index, a name or an output generation on its own can
    /// each name a different thing after a move, a rename or a respawn.
    selection: std::sync::Mutex<Option<(marsh_core::shellmux::SnapshotUid, rmux_core::PaneId)>>,
    /// The request handler this facade belongs to, once the daemon has installed it.
    ///
    /// Weak, and behind a lock because it is set after construction: the facade exists before the
    /// handler does. It is how a task holding only a `ShellIo` — a pane's prompt, most of all —
    /// reaches the *normal* transcript publisher instead of writing to a terminal it would have
    /// to reopen.
    handler: std::sync::Mutex<Option<crate::handler::WeakRequestHandler>>,
    /// Serializes this daemon's pane-creation transactions.
    ///
    /// Per daemon, deliberately. Releasing the handler state lock mid-transaction is what makes
    /// serialization necessary at all: two creations that interleave would each hold a rollback
    /// snapshot of a session the other has since mutated. But that hazard exists only *within one
    /// session model* — two independent daemons share no sessions and have nothing to roll back
    /// over each other. A process-wide lock would serialize every daemon in the process against
    /// every other, which in a test binary means hundreds of unrelated handlers queueing behind
    /// one, and one slow creation starving all of them.
    pane_creation: Arc<tokio::sync::Mutex<()>>,
    /// Serializes the short creation-or-adoption transaction.
    ///
    /// A server-initiated spawn takes this, installs its route (or a failed-spawn tombstone), and
    /// only then releases it. The observation consumer takes it before treating an unmapped
    /// `Opened` as an externally created job. Without it, an `Opened` that arrives before its own
    /// `spawn` call has returned would be adopted as a second, duplicate pane.
    ///
    /// Safe to hold briefly: callbacks only queue, so nothing under this lock can wait on one.
    admission: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for IoService {
    /// Names what the service is and where it is, never the environment values it seeds shells
    /// with: those are the caller's, and a debug print of a daemon is not the place for them.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IoService")
            .field("phase", &self.phase())
            .field("socket", &self.socket)
            .field("open", &self.mux_opt().is_some())
            .finish()
    }
}

/// Read-only state that outlives the mux.
#[derive(Debug)]
struct Frozen {
    /// The executor metadata as of the last time it was readable.
    executor: ExecutorInfo,
    /// The policy history as of teardown.
    history: Vec<marsh_core::policy::Event>,
}

/// What keeps this daemon from deciding it is idle.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    /// Public handles held by the owning library host.
    leases: std::sync::atomic::AtomicUsize,
    /// Operations accepted and not yet finished.
    operations: std::sync::atomic::AtomicUsize,
    /// Admitted jobs, until their closure or failed open.
    instances: std::sync::Mutex<std::collections::HashSet<marsh_core::shellmux::SnapshotUid>>,
    /// Wakes anything waiting for a retirement.
    retirement: tokio::sync::Notify,
    /// Verdicts a prompt has already rendered, so a job's close does not render them twice.
    ///
    /// A persistent pane renders its own verdict through the idle-terminal lease and carries on.
    /// When that pane is later stopped, `JobEnd::completion` still names that same last command —
    /// so a close that rendered unconditionally would print the verdict a second time, long after
    /// the user saw it.
    ///
    /// One entry per live job, holding only its LATEST rendered command, and removed when that
    /// job's close consumes it. A capped queue would be wrong rather than merely lossy: a pane
    /// that rendered once and then stayed open through enough other reports would have its marker
    /// evicted and duplicate the verdict at close. Keyed by generation, so memory is bounded by
    /// the number of live jobs and nothing has to be evicted for correctness.
    rendered: std::sync::Mutex<
        std::collections::HashMap<
            marsh_core::shellmux::SnapshotUid,
            marsh_core::shellmux::CommandId,
        >,
    >,
}

/// The cloneable application interface to this daemon's shells, processes and terminals.
///
/// Every operation an [`RmuxFrontend`](crate::RmuxFrontend) offers is one of these: the owner
/// derefs to the handle it holds, and [`RmuxFrontend::io`](crate::RmuxFrontend::io) hands out an
/// independently shareable one. Cloning is cheap and shares one service; it does not create a
/// second mux, a second admission path or a second seed lease.
///
/// A handle the owning frontend holds or handed out is a *native client lease*: while one exists,
/// this daemon does not decide it is idle merely because no UI session is attached, which is what
/// keeps `exit-empty` from racing a headless API consumer that has never opened a session.
/// Internal handlers use unleased clones, which this daemon makes for itself and no caller can
/// ask for. An explicit shutdown always overrides every lease.
///
/// **Cloning a leased handle yields another lease.** That is the only correct reading of "this
/// handle keeps the daemon up": a clone is as capable as its original, and the lease has to be
/// balanced one increment per live handle. A derived `Clone` would copy the flag without
/// incrementing, and the second drop would then take the counter below zero — after which the
/// daemon would consider itself permanently busy and never honour `exit-empty` again.
#[derive(Debug)]
pub struct ShellIo {
    /// The shared service.
    service: Arc<IoService>,
    /// Whether this handle holds a native-client lease.
    leased: bool,
}

impl Clone for ShellIo {
    /// Clones the handle, and its lease with it.
    fn clone(&self) -> Self {
        if self.leased {
            self.service
                .activity
                .leases
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        Self {
            service: Arc::clone(&self.service),
            leased: self.leased,
        }
    }
}

impl Drop for ShellIo {
    /// Releases this handle's lease, if it held one.
    fn drop(&mut self) {
        if self.leased {
            self.service
                .activity
                .leases
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            // Every decrement of the activity counter wakes the waiters, not only a retirement.
            // `wait_quiet` asks whether ANY activity remains — leases, operations and instances —
            // so a wakeup fired for one of those three and not the others leaves it asleep
            // forever the moment the last lease is the thing that went away.
            self.service.activity.retirement.notify_waiters();
        }
    }
}

impl IoService {
    /// The mux, or [`IoError::Closed`] once teardown has released it.
    fn mux(&self) -> IoResult<Arc<marsh_core::shellmux::ShellMux>> {
        self.mux
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(IoError::Closed)
    }

    /// The mux if it is still there, for read-only queries that answer emptily after teardown.
    fn mux_opt(&self) -> Option<Arc<marsh_core::shellmux::ShellMux>> {
        self.mux
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Refuses work once closing has begun.
    fn admit(&self) -> IoResult<Arc<marsh_core::shellmux::ShellMux>> {
        if *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            != IoPhase::Running
        {
            return Err(IoError::Closed);
        }
        self.mux()
    }

    /// The current phase.
    fn phase(&self) -> IoPhase {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ShellIo {
    /// Opens `seed`'s engine and builds the one service every handle of this daemon shares.
    ///
    /// This is the *only* place a mux is constructed in this crate, and it is why nothing has to
    /// check afterwards that a frontend belongs to a mux, that a queue was not already taken, or
    /// that the support builtins were registered: each of those is true by construction here.
    ///
    /// In order, because no step may be reordered:
    ///
    /// 1. open the seed's exclusive lease and recover its write-ahead log;
    /// 2. build the frontend queue, because the mux reads its geometry during construction;
    /// 3. freeze one profile, so every shell in the process holds the identical builtin set;
    /// 4. build exactly one mux over that seed and that profile.
    ///
    /// Returns the **unleased** facade and the queue's unique consumer end together, so the
    /// caller cannot end up with one and not the other.
    ///
    /// `runtime` is the runtime every managed operation and every task this facade creates lands
    /// on, including ones requested from a status thread, a foreign runtime or a detached command
    /// queue. It must be the runtime this call is entered into: [`ShellMux::new`] captures the
    /// ambient one for the job pumps.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Mux`] when the seed cannot be discovered, leased or recovered, and
    /// when a dimension of `geometry` is zero.
    pub(crate) fn new(
        seed: &std::path::Path,
        validator: Arc<std::sync::Mutex<marsh_core::PolicyValidator>>,
        environment: brush_core::env::ShellEnvironment,
        geometry: TerminalGeometry,
        filesystem: Arc<dyn marsh_btrfs::Subvolumes>,
        runtime: tokio::runtime::Handle,
        socket: PathBuf,
    ) -> IoResult<(
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::shell_frontend::FrontendMessage>,
    )> {
        use marsh_core::shellmux::ShellFrontend as _;

        let executor = marsh_core::MarshExecutor::open_with(seed, filesystem)
            .map_err(|error| IoError::from(MuxError::from(error)))?;

        let mut frontend = crate::shell_frontend::FrontendQueue::new(geometry.rows, geometry.cols);
        let events = frontend
            .receiver
            .take()
            .expect("fresh frontend owns its observation queue");

        // One profile, frozen here, applied to panes, popups and hidden helper jobs alike.
        // Attaching a shell installs *one* process-wide instrumented builtin table, so a shell
        // carrying a builtin the latest installation does not know fails that builtin outright:
        // registering `__rmux_io` for helpers only would break `git` and `exec` for every pane.
        let mut builtins = std::collections::HashMap::new();
        builtins.insert(
            builtins::RMUX_IO_BUILTIN.to_string(),
            builtins::registration(),
        );
        let profile = marsh_core::shellmux::MuxProfile {
            environment,
            builtins,
        };

        let mux = marsh_core::shellmux::ShellMux::new(
            executor,
            validator,
            profile,
            Arc::new(std::sync::Mutex::new(frontend)),
        )?;

        let frozen = Frozen {
            executor: mux.executor_info(),
            history: Vec::new(),
        };
        let io = Self {
            service: Arc::new(IoService {
                mux: std::sync::Mutex::new(Some(mux)),
                frozen: std::sync::Mutex::new(frozen),
                bus: events::EventBus::new(),
                phase: std::sync::Mutex::new(IoPhase::Running),
                runtime,
                activity: Activity::default(),
                socket,
                streams: streams::Streams::default(),
                routes: std::sync::Mutex::new(std::collections::HashMap::new()),
                selection: std::sync::Mutex::new(None),
                handler: std::sync::Mutex::new(None),
                admission: tokio::sync::Mutex::new(()),
                pane_creation: Arc::new(tokio::sync::Mutex::new(())),
            }),
            leased: false,
        };
        Ok((io, events))
    }

    /// A clone that holds a native-client lease.
    ///
    /// Taken by the owning [`RmuxFrontend`](crate::RmuxFrontend) and by the handles it hands out.
    /// While one is alive this daemon does not treat itself as empty, which is what keeps
    /// `exit-empty` from racing a headless API consumer that has never opened a session.
    #[must_use]
    pub(crate) fn leased(&self) -> Self {
        self.service
            .activity
            .leases
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self {
            service: Arc::clone(&self.service),
            leased: true,
        }
    }

    /// An unleased clone, for this daemon's own handlers.
    #[must_use]
    pub(crate) fn unleased(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            leased: false,
        }
    }

    /// Whether anything is going on that an idle check must not ignore.
    ///
    /// Deliberately wider than "the mux has visible jobs": a headless consumer holding a lease has
    /// no session and no pane, and a helper job admitted a microsecond ago is not in anyone's
    /// window list yet. Retained bytes from a stream that already ended are *not* activity — a
    /// reader keeping a transcript alive must not pin the daemon open forever.
    #[must_use]
    pub(crate) fn has_activity(&self) -> bool {
        let activity = &self.service.activity;
        activity.leases.load(std::sync::atomic::Ordering::Acquire) > 0
            || activity.operations.load(std::sync::atomic::Ordering::Acquire) > 0
            || !activity
                .instances
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }

    /// The observation bus, for the frontend adapter that publishes onto it.
    pub(crate) fn bus(&self) -> &events::EventBus {
        &self.service.bus
    }

    /// The daemon runtime every managed operation is scheduled on.
    ///
    /// Read by the synchronous callers that have no ambient runtime of their own — the status
    /// renderer's job cache is the standing example — so they schedule onto this daemon rather
    /// than building a private current-thread runtime that dies with the call. Asynchronous
    /// callers do not need it: every mutation below already routes itself here.
    pub(crate) fn runtime(&self) -> tokio::runtime::Handle {
        self.service.runtime.clone()
    }

    /// Whether the calling task is already running on the runtime this host is bound to.
    ///
    /// Compared by runtime identity rather than by thread: a multi-threaded runtime answers from
    /// any of its workers, and two runtimes in one process can share neither an identity nor a
    /// task's fate.
    fn on_bound_runtime(&self) -> bool {
        tokio::runtime::Handle::try_current()
            .is_ok_and(|current| current.id() == self.service.runtime.id())
    }

    /// Runs one mutation on the runtime this host is bound to.
    ///
    /// Every task the core creates for a piece of work — a job's launch, its byte pumps, the task
    /// a line runs on, a builtin's native workers — is created on whichever runtime the admitting
    /// call was made from, and every descriptor it opens is registered with that runtime's
    /// reactor. A caller is not always the daemon: a status thread, a library consumer's own
    /// runtime, a detached queue and a completion callback can all reach this facade. Left alone,
    /// each of those would leave a live job whose pumps are cancelled and whose reactor is gone
    /// the moment the caller's executor is dropped — output that silently stops on a job that is
    /// still, by every observation this daemon publishes, running.
    ///
    /// So the work is moved here instead. A caller already on this runtime awaits it inline,
    /// which is the overwhelmingly common case and costs nothing; anyone else hands it over and
    /// waits for the result. Waiting needs no runtime of the caller's own — a join handle is an
    /// ordinary future, woken by the runtime that owns the task — so a plain `std::thread`
    /// blocking on this future works as well as a foreign runtime awaiting it.
    ///
    /// `work` is therefore `Send + 'static`: it owns clones of what it needs rather than
    /// borrowing from the caller's frame, because the caller's frame may be gone before it
    /// finishes. Dropping this future after the work was handed over does not cancel it — the
    /// operation may already have been admitted, and an admitted job is the core's, not the
    /// caller's.
    ///
    /// Read-only snapshots deliberately do not come through here. They take a short lock and
    /// answer; a runtime hop would make them slower and no more correct.
    async fn dispatch<T>(
        &self,
        work: impl Future<Output = IoResult<T>> + Send + 'static,
    ) -> IoResult<T>
    where
        T: Send + 'static,
    {
        if self.on_bound_runtime() {
            return work.await;
        }
        match self.service.runtime.spawn(work).await {
            Ok(result) => result,
            // A panic in the core is the core's bug, and swallowing it here would report it as an
            // orderly closure. Cancellation, by contrast, only happens when the daemon's own
            // runtime is being torn down, which is exactly what `Closed` means.
            Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
            Err(_) => Err(IoError::Closed),
        }
    }

    /// Wraps a core handle as a host-bound one.
    pub(crate) fn wrap(&self, job: Spawned) -> ShellHandle {
        ShellHandle {
            origin: Arc::clone(&self.service),
            job,
        }
    }

    /// Refuses once the core has been released, for the read paths that must not answer emptily.
    ///
    /// Distinct from [`IoService::admit`]: this permits a service that is still *closing*, for
    /// operations that only observe. What it refuses is a service with no core at all, where the
    /// honest answer is [`IoError::Closed`] rather than a handle onto nothing.
    pub(crate) fn ensure_open(&self) -> IoResult<()> {
        self.service.mux().map(drop)
    }

    /// Checks a handle came from this host.
    pub(crate) fn owned<'a>(&self, job: &'a ShellHandle) -> IoResult<&'a Spawned> {
        if Arc::ptr_eq(&job.origin, &self.service) {
            Ok(&job.job)
        } else {
            Err(IoError::WrongHost)
        }
    }

    /// Metadata about the executor this host was built over.
    ///
    /// Answers after teardown too, from the frozen copy: where the seed was is still a legitimate
    /// question once the lease is gone.
    #[must_use]
    pub fn executor_info(&self) -> ExecutorInfo {
        if let Some(mux) = self.service.mux_opt() {
            let info = mux.executor_info();
            self.service
                .frozen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .executor = info.clone();
            return info;
        }
        self.service
            .frozen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .executor
            .clone()
    }

    /// Whether every shell this host builds carries the builtin `name`.
    ///
    /// A property of the frozen profile, so it can be checked before a single pane exists.
    #[must_use]
    pub fn has_builtin(&self, name: &str) -> bool {
        self.service
            .mux_opt()
            .is_some_and(|mux| mux.has_builtin(name))
    }

    /// The seed-relative directory a job starts in when none is named.
    #[must_use]
    pub fn default_dir(&self) -> String {
        self.service.mux_opt().map_or_else(String::new, |mux| {
            let cwd = std::env::current_dir().unwrap_or_default();
            mux.default_dir(&cwd).as_str().to_string()
        })
    }

    /// The committed capability history, in grant order.
    ///
    /// After teardown this is the copy captured before the mux was released.
    #[must_use]
    pub fn history(&self) -> Vec<marsh_core::policy::Event> {
        if let Some(mux) = self.service.mux_opt() {
            return mux.history();
        }
        self.service
            .frozen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history
            .clone()
    }

    /// One consistent look at the service and the core behind it.
    #[must_use]
    pub fn snapshot(&self) -> IoSnapshot {
        let registration = self.service.bus.registration();
        let state = self.service.mux_opt().map_or_else(
            || marsh_core::shellmux::MuxSnapshot {
                jobs: Vec::new(),
                commands: Vec::new(),
                current: None,
                default_geometry: TerminalGeometry { rows: 24, cols: 80 },
            },
            |mux| mux.snapshot(),
        );
        let next_event_sequence = *registration;
        drop(registration);
        IoSnapshot {
            phase: self.service.phase(),
            state,
            next_event_sequence,
        }
    }

    /// Every visible job, in creation order. Empty once the host has closed.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        self.service
            .mux_opt()
            .map(|mux| mux.jobs())
            .unwrap_or_default()
    }

    /// One job by name.
    #[must_use]
    pub fn job(&self, id: &ShellId) -> Option<JobView> {
        self.service.mux_opt().and_then(|mux| mux.job(id))
    }

    /// The selected job, if one is selected and still visible.
    #[must_use]
    pub fn current_job(&self) -> Option<JobView> {
        self.service.mux_opt().and_then(|mux| mux.current_job())
    }

    /// Resolves a name to a handle, once.
    ///
    /// The *only* place a name becomes a handle. Every later action validates the handle rather
    /// than re-resolving the name, which is what stops an action from reaching a different job
    /// that took the name in between.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Closed`] after teardown and [`MuxError::NoSuchJob`] when nothing
    /// visible answers to `id`.
    pub fn shell(&self, id: &ShellId) -> IoResult<ShellHandle> {
        let mux = self.service.mux()?;
        mux.handle(id)
            .map(|job| self.wrap(job))
            .ok_or_else(|| IoError::from(MuxError::NoSuchJob(id.clone())))
    }

    /// Opens a job.
    ///
    /// `dir` is seed-relative, and empty means the seed root. `id` is the job's name *and* its
    /// capability principal; `None` draws the next automatic one. `cmd` opens the job *for* one
    /// line, whose receipt is allocated before this call returns and is reachable through
    /// [`ShellHandle::initial_command`]; such a job closes when that line ends unless
    /// [`keep`](Self::keep) cancels the closure. `SpawnOptions::default()` is a terminal job at
    /// this host's default geometry with no environment replacement.
    ///
    /// Returning is *acceptance*: the job has a row, a principal and an identity, and its
    /// resources may still be opening. Readiness is [`IoEvent::Opened`]; the end of its life is
    /// [`ShellHandle::wait_closed`]. Dropping this future after admission does not un-admit the
    /// job — an admitted job belongs to the core, and it stays observable whether or not the
    /// caller waited for it.
    ///
    /// Admission, and every task the new job owns, happen on this host's own runtime whoever
    /// calls — a job admitted from a status thread or a foreign runtime does not die with it.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Closed`] once teardown has begun, and with whatever the core reported
    /// about the name, the directory, the geometry or the shell.
    pub async fn spawn(
        &self,
        dir: &str,
        id: Option<ShellId>,
        cmd: Option<&str>,
        options: SpawnOptions,
    ) -> IoResult<ShellHandle> {
        let mux = self.service.admit()?;
        let work = self.begin_operation();
        let dir = dir.to_string();
        let cmd = cmd.map(ToString::to_string);
        // The instance is recorded inside the dispatched work, not after it: a caller that drops
        // this future has still admitted a job, and the idle check has to see it either way.
        let service = Arc::clone(&self.service);
        let job = self
            .dispatch(async move {
                let _work = work;
                let job = mux.spawn(&dir, id, cmd.as_deref(), options).await?;
                service
                    .activity
                    .instances
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(job.sandbox().uid.clone());
                Ok(job)
            })
            .await?;
        Ok(self.wrap(job))
    }

    /// Submits one line into an open job.
    ///
    /// `cmd` is one shell command line for the job's embedded brush interpreter — the unit the
    /// publication gate works on. It is **not** an rmux control-command block: those are the
    /// multiplexer's own scripted commands, arrive over the protocol, address sessions, windows
    /// and panes, and never stage a filesystem change or reach the gate. The two vocabularies
    /// overlap nowhere, and a caller wanting the second reaches it through the SDK handles or
    /// [`open_protocol`](Self::open_protocol).
    ///
    /// Returning is *acceptance*: the line has an identity, a receipt and a reservation in the
    /// job's row, and nothing has run yet. Completion is [`CommandHandle::wait`], which is a
    /// separate moment and the only one carrying a verdict. Dropping this future after the line
    /// was admitted does not un-admit it.
    ///
    /// One command at a time per job: a second line submitted while one is running is refused
    /// rather than queued. The task the line runs on belongs to this host's runtime, not the
    /// caller's.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::WrongHost`] for a foreign handle, [`IoError::Closed`] after teardown,
    /// and with whatever the core reported about the job's state.
    ///
    /// # Examples
    ///
    /// One persistent shell running several lines, each gated on its own. Needs a live host over
    /// a leased btrfs seed and real programs to run, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{CommandOptions, JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoResult, ShellIo};
    ///
    /// # async fn build(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .spawn(
    ///         "",
    ///         Some(ShellId::from("builder")),
    ///         None,
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             environment: None,
    ///         },
    ///     )
    ///     .await?;
    ///
    /// // One job, one principal, three lines. The job outlives all of them, which is how an
    /// // application accumulates approved state instead of inventing a principal per line.
    /// for line in ["./configure --prefix=/usr", "make -j8", "make install"] {
    ///     let command = io.start_in(&job, line, CommandOptions::default()).await?;
    ///     // `start_in` returning is acceptance; this is completion. They are different
    ///     // moments, and only the second one carries a verdict.
    ///     let completion = command.wait().await?;
    ///     // Not the exit code: a line can exit zero and publish nothing, so continuing on
    ///     // status alone would build on state that never reached the seed.
    ///     if !completion.is_published() {
    ///         break;
    ///     }
    /// }
    ///
    /// io.stop(&job, false).await
    /// # }
    /// ```
    pub async fn start_in(
        &self,
        job: &ShellHandle,
        cmd: &str,
        options: CommandOptions,
    ) -> IoResult<CommandHandle> {
        let mux = self.service.admit()?;
        let spawned = self.owned(job)?.clone();
        let work = self.begin_operation();
        let cmd = cmd.to_string();
        self.dispatch(async move {
            let _work = work;
            Ok(mux.start_in(&spawned, &cmd, options).await?)
        })
        .await
    }

    /// Cancels a job's automatic closure, so it outlives the command it was opened for.
    ///
    /// `true` means the closure was still cancellable and has been cancelled. `false` means there
    /// was none to cancel — the job was opened idle — or that its command already reached the
    /// finish/close gate and the decision is made. Neither answer is an error, and `false` is
    /// never a reason to retry.
    ///
    /// Synchronous and immediate: there is no admitted work to wait for. Bound to one generation
    /// of one job, so a handle whose job has closed cannot retain whatever later took its name.
    /// Retaining is for terminal jobs, whose lifetime is a *shell*; a pipe job is one-shot by
    /// construction and closes after its single command whatever this says.
    ///
    /// # Errors
    ///
    /// Fails for a foreign or stale handle, and after teardown.
    ///
    /// # Examples
    ///
    /// A job opened *for* a command, its receipt taken before anything could run, and its
    /// automatic closure cancelled so it stays open afterwards. Needs a live host over a leased
    /// seed and a real program to run, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoResult, ShellIo};
    ///
    /// # async fn watcher(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .spawn(
    ///         "",
    ///         Some(ShellId::from("watcher")),
    ///         Some("cargo watch -x test"),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             environment: None,
    ///         },
    ///     )
    ///     .await?;
    ///
    /// // The initial command's receipt exists the instant the spawn returns — before its launch
    /// // task has run — so there is no window in which a caller holds a job it cannot wait on.
    /// let initial = job.initial_command();
    ///
    /// // Without this, the job closes when that command ends. With it, the job is an ordinary
    /// // persistent shell and closing it becomes the caller's job.
    /// let retained = io.keep(&job)?;
    ///
    /// if let Some(initial) = initial {
    ///     let completion = initial.wait().await?;
    ///     // A forced stop resolves here too, with `Outcome::Discarded` and no exit code. That
    ///     // is the verdict, not the absence of one.
    ///     let _ = (completion.exit_code, completion.is_published());
    /// }
    ///
    /// if retained {
    ///     // Graceful: the shell finishes and its staged work is gated normally. `true` would
    ///     // retire it and discard that work instead.
    ///     io.stop(&job, false).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn keep(&self, job: &ShellHandle) -> IoResult<bool> {
        let mux = self.service.mux()?;
        Ok(mux.keep(self.owned(job)?)?)
    }

    /// Writes the job table to `out`, exactly as the console builtin renders it.
    ///
    /// A forwarder rather than a second renderer. `jobctl::print_jobs` takes the multiplexer,
    /// which this facade deliberately never hands out, so the one place that has it lends it for
    /// the length of the call — and the interactive prompt's `jobs` output stays byte for byte
    /// what `jobs` prints anywhere else instead of drifting into a private column layout.
    ///
    /// Writes nothing once the core has been released: there is no table left to print.
    pub(crate) fn print_jobs(&self, out: &mut dyn std::io::Write) {
        if let Some(mux) = self.service.mux_opt() {
            marsh_core::shellmux::jobctl::print_jobs(&mux, out);
        }
    }

    /// Registers the single legacy completion callback on a job.
    ///
    /// **One slot, and it belongs to the job rather than to a command.** Registering a second
    /// callback replaces the first, which is what the `bool` reports: `true` means one was
    /// already installed and has been displaced. The callback receives only an `i32` — the
    /// completion's
    /// [`legacy_status`](marsh_core::shellmux::CommandCompletion::legacy_status), which maps "no
    /// execution result was obtained" onto `-1` — so through it a denied publication and a clean
    /// exit are indistinguishable. It runs once, on this host's runtime, outside every mux lock.
    ///
    /// [`CommandHandle::wait`] is what everything else should use. It occupies no slot, any
    /// number of holders may wait on one command before or after it ends, and each observes the
    /// identical [`CommandCompletion`](marsh_core::shellmux::CommandCompletion) — with the
    /// process status and the publication verdict kept apart.
    ///
    /// # Errors
    ///
    /// Fails for a foreign or stale handle, and after teardown.
    ///
    /// # Examples
    ///
    /// The one slot taken, and two independent waiters on the same command beside it. Needs a
    /// live host over a leased seed and a real program to run, so it is compiled rather than
    /// executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::CommandOptions;
    /// use rmux_server::io::{IoResult, ShellHandle, ShellIo};
    ///
    /// # async fn observe(io: &ShellIo, job: &ShellHandle) -> IoResult<()> {
    /// let command = io.start_in(job, "make test", CommandOptions::default()).await?;
    ///
    /// // The slot. `displaced` being true means this just took someone else's callback away,
    /// // which is precisely why a library should prefer the receipt below.
    /// let displaced = io.on_finish(
    ///     job,
    ///     Box::new(|status| eprintln!("job finished with {status}")),
    /// )?;
    /// let _ = displaced;
    ///
    /// // Two waiters, neither occupying anything. Both resolve to the same completion, and a
    /// // third attaching after the command had already ended would resolve to it immediately.
    /// let reporter = command.clone();
    /// let auditor = command.clone();
    /// let (reported, audited) = tokio::join!(reporter.wait(), auditor.wait());
    /// let (reported, audited) = (reported?, audited?);
    ///
    /// assert_eq!(reported.id, audited.id);
    /// // The exit status is the process's; publication is the gate's. The callback above could
    /// // only ever have carried the first of those two.
    /// let _ = (reported.exit_code, audited.is_published());
    /// # Ok(())
    /// # }
    /// ```
    pub fn on_finish(
        &self,
        job: &ShellHandle,
        done: marsh_core::shellmux::OnFinish,
    ) -> IoResult<bool> {
        let mux = self.service.mux()?;
        Ok(mux.on_finish(self.owned(job)?, done)?)
    }

    /// Selects a terminal job.
    ///
    /// # Errors
    ///
    /// Fails for a pipe job, a closing job, a foreign or stale handle, and after teardown.
    pub async fn switch(&self, job: &ShellHandle) -> IoResult<JobView> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?.clone();
        self.dispatch(async move { Ok(mux.switch(&spawned).await?) })
            .await
    }

    /// Stops a job, gracefully or by force.
    ///
    /// `force = false` closes the job's shell and lets whatever is running reach its ordinary
    /// boundary, so its staged work is gated as usual. `force = true` retires it: processes are
    /// killed, native workers are cancelled and joined, and the line's staged changes are
    /// **discarded** — which resolves its receipt `Ok` with
    /// [`Outcome::Discarded`](marsh_core::Outcome::Discarded) and no exit code. That is a
    /// verdict, not a lost one, and it is not evidence that nothing ran.
    ///
    /// The cancel-versus-conclude decision is linearized under the job's own lock. A force
    /// accepted before finalization discards; a force arriving once an approved publication has
    /// begun cannot undo it, and the completion that eventually lands is authoritative. No
    /// rollback of published effects is promised, because none is possible.
    ///
    /// Returning is acceptance of the stop, not the job's closure: every stream still has to end
    /// and the snapshot still has to be reclaimed. [`ShellHandle::wait_closed`] is that later
    /// boundary.
    ///
    /// # Errors
    ///
    /// Fails when a forced job's processes could not be signalled, for a foreign or stale handle,
    /// and after teardown.
    pub async fn stop(&self, job: &ShellHandle, force: bool) -> IoResult<()> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?.clone();
        self.dispatch(async move { Ok(mux.stop(&spawned, force).await?) })
            .await
    }

    /// Signals the processes the running command started.
    ///
    /// Only that command's own process groups; this is not an arbitrary-pid interface. Best
    /// effort, and it does not retire the job: a command that catches the signal and finishes
    /// normally is gated normally.
    ///
    /// # Errors
    ///
    /// Fails for a foreign or stale handle, after teardown, and when the signal could not be
    /// delivered for a reason other than the process already being gone.
    pub fn signal(&self, job: &ShellHandle, signal: marsh_core::Signal) -> IoResult<()> {
        let mux = self.service.mux()?;
        Ok(mux.signal(self.owned(job)?, signal)?)
    }

    /// Resizes one terminal job.
    ///
    /// # Errors
    ///
    /// Fails for a zero dimension, a pipe job, a foreign or stale handle, and after teardown.
    pub async fn resize(&self, job: &ShellHandle, size: TerminalGeometry) -> IoResult<()> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?.clone();
        self.dispatch(async move { Ok(mux.resize(&spawned, size).await?) })
            .await
    }

    /// Resizes every live terminal job and sets the default for future ones.
    ///
    /// # Errors
    ///
    /// Fails for a zero dimension — changing nothing — and reports the first terminal that refused
    /// the change after attempting all of them.
    pub async fn resize_all(&self, size: TerminalGeometry) -> IoResult<()> {
        let mux = self.service.mux()?;
        self.dispatch(async move { Ok(mux.resize_all(size).await?) })
            .await
    }

    /// Writes bytes to a job's standard input.
    ///
    /// Keystrokes for a terminal job, pipe bytes for a pipe job. A failure partway through has
    /// already delivered a prefix; there is no atomic delivery to promise, and consumption by the
    /// application is a separate question this cannot answer.
    ///
    /// # Errors
    ///
    /// Fails for a closing job, a pipe job whose input is closed, a foreign or stale handle, and
    /// after teardown.
    ///
    /// # Examples
    ///
    /// Keystrokes into a terminal job, then a resize the program actually observes. Needs a live
    /// host over a leased seed and a real pseudoterminal, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{JobIo, ShellId, SpawnOptions, TerminalGeometry};
    /// use rmux_server::io::{IoResult, ShellIo};
    ///
    /// # async fn drive(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .spawn(
    ///         "",
    ///         Some(ShellId::from("pane")),
    ///         None,
    ///         SpawnOptions {
    ///             io: JobIo::Terminal {
    ///                 geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
    ///             },
    ///             environment: None,
    ///         },
    ///     )
    ///     .await?;
    ///
    /// // Raw bytes on the master side, not a line-oriented API: the carriage return is what
    /// // submits the line, because the line discipline belongs to the terminal and not to this.
    /// io.write_input(&job, b"printf 'hi'\r").await?;
    /// // Control characters are bytes too. This one asks the terminal driver to raise SIGINT,
    /// // which is a different mechanism from `ShellIo::signal` addressing the running command's
    /// // own process groups.
    /// io.write_input(&job, b"\x03").await?;
    /// // There is no end-of-file to send here at all: `close_input` refuses a terminal job, and
    /// // Ctrl-D would be a keystroke rather than a writer going away.
    ///
    /// // A real window-size change plus the SIGWINCH it implies, so a full-screen program
    /// // repaints. Zero in either dimension is refused rather than clamped.
    /// io.resize(&job, TerminalGeometry { rows: 50, cols: 132 }).await?;
    /// // The same for every live terminal, and the default future ones open at. Pipe jobs are
    /// // skipped rather than failed: they have no geometry to change.
    /// io.resize_all(TerminalGeometry { rows: 50, cols: 132 }).await
    /// # }
    /// ```
    pub async fn write_input(&self, job: &ShellHandle, bytes: &[u8]) -> IoResult<()> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?;
        // The one mutation with a borrowed payload, and the one this daemon performs per
        // keystroke: a caller already on this runtime writes the caller's own slice, and only a
        // foreign one pays for the copy a handed-over future needs to own.
        if self.on_bound_runtime() {
            return Ok(mux.write_input(spawned, bytes).await?);
        }
        let spawned = spawned.clone();
        let bytes = bytes.to_vec();
        self.dispatch(async move { Ok(mux.write_input(&spawned, &bytes).await?) })
            .await
    }

    /// Ends a pipe job's standard input.
    ///
    /// A real end-of-file, ordered after every accepted write, and idempotent. Not available for a
    /// terminal job: a pseudoterminal has no half-close, and sending Ctrl-D instead would be a
    /// keystroke, not an end of file.
    ///
    /// # Errors
    ///
    /// Fails for a terminal job, a foreign or stale handle, and after teardown.
    pub async fn close_input(&self, job: &ShellHandle) -> IoResult<()> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?.clone();
        self.dispatch(async move { Ok(mux.close_input(&spawned).await?) })
            .await
    }

    /// Leases a terminal job's slave side while no command is running in it.
    ///
    /// How an interactive prompt reads *this pane's* keyboard. Revoked the instant a command is
    /// admitted.
    ///
    /// The lease's descriptor is registered with this host's reactor, not the caller's, so a
    /// prompt driven from one runtime and leased from another still reads.
    ///
    /// # Errors
    ///
    /// Fails for a pipe job, a busy job, an outstanding lease, a foreign or stale handle, and
    /// after teardown.
    pub async fn idle_terminal(
        &self,
        job: &ShellHandle,
    ) -> IoResult<marsh_core::shellmux::IdleTerminal> {
        let mux = self.service.admit()?;
        let spawned = self.owned(job)?.clone();
        self.dispatch(async move { Ok(mux.idle_terminal(&spawned).await?) })
            .await
    }

    /// Whether `line` is a complete shell command for this job's shell.
    ///
    /// `false` only for an incomplete tokenization or an end-of-input parse failure. Any other
    /// syntax error answers `true`, so the line runs and the shell produces its own diagnostic.
    ///
    /// # Errors
    ///
    /// Fails for a busy job, a pipe job, a foreign or stale handle, and after teardown.
    pub async fn input_is_complete(&self, job: &ShellHandle, line: &str) -> IoResult<bool> {
        let mux = self.service.mux()?;
        let spawned = self.owned(job)?.clone();
        let line = line.to_string();
        self.dispatch(async move { Ok(mux.input_is_complete(&spawned, &line).await?) })
            .await
    }

    /// Subscribes to observations, atomically with a snapshot.
    ///
    /// A mutation concurrent with this call is either already in the snapshot or arrives on the
    /// stream. Never neither.
    ///
    /// The stream is bounded and independent of every other observer: falling behind produces an
    /// explicit [`IoError::Lagged`], never silence, and never backpressure on the job producing
    /// the bytes. Dropping the stream unsubscribes and cancels nothing.
    ///
    /// # Examples
    ///
    /// Reconciling the snapshot with the events that continue from it. Needs a live host over a
    /// leased seed, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::ShellId;
    /// use rmux_server::io::{IoEvent, IoResult, ShellIo};
    ///
    /// # async fn follow(io: &ShellIo) -> IoResult<Vec<ShellId>> {
    /// let observation = io.observe();
    ///
    /// // Everything before `next_event_sequence` is already in the snapshot; everything from it
    /// // onwards arrives on the stream. A mutation racing this call lands in exactly one of the
    /// // two, which is what makes them safe to merge.
    /// let mut live: Vec<ShellId> = observation
    ///     .snapshot
    ///     .state
    ///     .jobs
    ///     .iter()
    ///     .map(|job| job.id.clone())
    ///     .collect();
    /// let _resume_at = observation.snapshot.next_event_sequence;
    ///
    /// let mut events = observation.events;
    /// while let Some(envelope) = events.recv().await? {
    ///     match &envelope.event {
    ///         IoEvent::Opened { job } => live.push(job.id().clone()),
    ///         // Every stream of that job ended and its snapshot was reclaimed. A later
    ///         // boundary than any of its commands finishing, and it proves nothing about
    ///         // publication either way.
    ///         IoEvent::Closed { end } => live.retain(|id| *id != end.shell.id),
    ///         // Stateless and coalesced, so re-read `ShellIo::snapshot` rather than infer.
    ///         IoEvent::Changed => {}
    ///         _ => {}
    ///     }
    /// }
    /// Ok(live)
    /// # }
    /// ```
    #[must_use]
    pub fn observe(&self) -> Observation {
        let registration = self.service.bus.registration();
        let (events, next_event_sequence) = self.service.bus.subscribe_locked(&registration);
        let state = self.service.mux_opt().map_or_else(
            || marsh_core::shellmux::MuxSnapshot {
                jobs: Vec::new(),
                commands: Vec::new(),
                current: None,
                default_geometry: TerminalGeometry { rows: 24, cols: 80 },
            },
            |mux| mux.snapshot(),
        );
        drop(registration);
        Observation {
            snapshot: IoSnapshot {
                phase: self.service.phase(),
                state,
                next_event_sequence,
            },
            events,
        }
    }

    /// Marks the start of an accepted operation, so an idle check sees it.
    fn begin_operation(&self) -> OperationGuard {
        self.service
            .activity
            .operations
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        OperationGuard {
            service: Arc::clone(&self.service),
        }
    }

    /// Records that a job's instance has terminated, for the idle check.
    pub(crate) fn retire_instance(&self, uid: &marsh_core::shellmux::SnapshotUid) {
        self.service
            .activity
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(uid);
        self.service.activity.retirement.notify_waiters();
    }

    /// Waits until nothing this daemon admitted is still live.
    ///
    /// The same honest counter [`has_activity`](ShellIo::has_activity) reads — native client
    /// leases, accepted operations and admitted job instances — so this cannot be used to declare
    /// anything dead early. It only waits for a boundary that already exists.
    ///
    /// For the one caller that needs it: an idle decision taken the instant a kill returns is
    /// taken while the teardown that kill started is still in flight. `wait_closed()` is not
    /// enough — it resolves when the snapshot is reclaimed, before the delivery workers have
    /// drained and before the instance is retired — and by then the caller no longer has the uids
    /// to wait on individually, because the sessions and their handles are already gone.
    ///
    /// Always bound this at the call site. A teardown that wedges must not take kill-session down
    /// with it.
    pub(crate) async fn wait_quiet(&self) {
        loop {
            // Registered before the check, so a retirement landing between them still wakes this.
            let notified = self.service.activity.retirement.notified();
            if !self.has_activity() {
                return;
            }
            notified.await;
        }
    }

    /// Records that a prompt has rendered this command's verdict itself.
    ///
    /// Called only after the bytes actually reached the slave. A verdict marked before the write
    /// succeeded would be suppressed at close having never been shown at all, which is the one
    /// outcome worse than showing it twice.
    pub(crate) fn mark_report_rendered(
        &self,
        uid: &marsh_core::shellmux::SnapshotUid,
        command: marsh_core::shellmux::CommandId,
    ) {
        self.service
            .activity
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(uid.clone(), command);
    }

    /// Whether this command's verdict has already been rendered by a prompt.
    ///
    /// Consuming: the entry is removed as it is read, because the only caller is the job's own
    /// close and there is nothing after it to ask again. That is what keeps this map bounded by
    /// live jobs without any eviction policy.
    #[must_use]
    pub(crate) fn report_was_rendered(
        &self,
        uid: &marsh_core::shellmux::SnapshotUid,
        command: marsh_core::shellmux::CommandId,
    ) -> bool {
        self.service
            .activity
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(uid)
            .is_some_and(|rendered| rendered == command)
    }


    /// This host's socket, for the SDK connections pinned to it.
    pub(crate) fn socket(&self) -> &std::path::Path {
        &self.service.socket
    }

    /// Binds this facade to the request handler that serves it.
    ///
    /// Called once, from `listener::serve`, beside `install_shell_io`.
    pub(crate) fn install_handler(&self, handler: crate::handler::WeakRequestHandler) {
        *self
            .service
            .handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    /// The request handler, if this facade is bound to one that is still alive.
    ///
    /// `None` after the daemon has gone, which is the honest answer: there is no surface left to
    /// render onto, and a caller must drop what it was going to publish rather than reopen one.
    #[must_use]
    pub(crate) fn handler(&self) -> Option<crate::handler::RequestHandler> {
        self.service
            .handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(crate::handler::WeakRequestHandler::upgrade)
    }



    /// Retained bytes and registered readers, per job stream.
    pub(crate) fn streams(&self) -> &streams::Streams {
        &self.service.streams
    }

    /// Holds this daemon's pane-creation transaction for as long as the guard lives.
    ///
    /// The guard is *owned* rather than borrowed, because every caller reaches the facade through
    /// `RequestHandler::shell_io()`, which hands back a clone by value. A borrowed guard could not
    /// outlive that temporary, and the transaction has to span the plan, the open and the commit.
    pub(crate) async fn pane_creation_transaction(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.service.pane_creation).lock_owned().await
    }

    /// The creation-or-adoption transaction lock.
    pub(crate) fn admission_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.service.admission
    }

    /// Records which rmux surface presents one job generation.
    pub(crate) fn install_route(&self, uid: marsh_core::shellmux::SnapshotUid, route: Route) {
        self.service
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(uid, route);
    }

    /// Whether one job generation already has a surface, whatever kind.
    pub(crate) fn has_route(&self, uid: &marsh_core::shellmux::SnapshotUid) -> bool {
        self.service
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(uid)
    }

    /// Whether one job generation is presented by a popup overlay rather than a pane.
    ///
    /// Separate from [`Self::route_for`], which answers only for panes, because a popup has no
    /// session, no stable pane id and no output generation: it belongs to one attached client's
    /// overlay. The observation consumer needs a single cheap lookup to decide which of the two
    /// surfaces a terminal chunk belongs to, and this is it.
    pub(crate) fn is_popup_route(&self, uid: &marsh_core::shellmux::SnapshotUid) -> bool {
        matches!(
            self.service
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(uid),
            Some(Route::Popup)
        )
    }

    /// Forgets one job generation's surface.
    ///
    /// Deliberately not its retained bytes. A stream's storage belongs to whoever is still reading
    /// it: an observer that fell behind must still receive its retained chunks, and its explicit
    /// gaps, before it is told the stream is over. Dropping the storage here would turn "you fell
    /// behind" into "there was never anything here" — byte loss reported as a clean end of file.
    /// Reclamation is instead tied to the readers themselves, in
    /// [`Streams::end`](streams::Streams::end) and [`Streams::release`](streams::Streams::release).
    pub(crate) fn forget_route(&self, uid: &marsh_core::shellmux::SnapshotUid) {
        self.service
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(uid);
    }

    /// The session, stable pane id and output generation presenting one job generation.
    ///
    /// Stable id rather than an index: a pane can be moved into another window or linked into
    /// another session, and an index captured earlier would then address a different pane.
    ///
    /// The output generation is the one the pane's creator *reserved* before the job was admitted,
    /// not one read back from the pane afterwards. That is what makes it usable as a rejection
    /// test: a respawn advances the pane's generation and opens a new job, so bytes still arriving
    /// from the job that is being replaced carry the older number and are refused by the surface
    /// they no longer belong to.
    pub(crate) fn route_for(
        &self,
        sandbox: &marsh_core::shellmux::Sandbox,
    ) -> Option<(rmux_proto::SessionName, rmux_core::PaneId, u64)> {
        match self
            .service
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&sandbox.uid)
        {
            Some(Route::Pane {
                session,
                pane,
                generation,
            }) => Some((session.clone(), *pane, *generation)),
            // A popup surface, a hidden helper or a spawn that failed all genuinely have no pane.
            _ => None,
        }
    }

    /// Takes the right to propagate a selection, unless it is one both sides already agree on.
    ///
    /// Selection is synchronised in both directions: an rmux `select-pane`/`select-window`/
    /// `switch-client` commit makes that pane's job the engine's current terminal, and a native
    /// [`ShellIo::switch`] selects the mapped rmux pane. Each of those changes is *observed* by
    /// the other side, so without a stop condition the first change would travel around the loop
    /// forever.
    ///
    /// This is that stop condition, and it compares stable instances rather than names. A pane
    /// index is a mutable addressing slot, a session name can be reused, and a respawn hands the
    /// same [`rmux_core::PaneId`] to a different job — so only the pair "this snapshot instance,
    /// presented by this stable pane" identifies a selection precisely enough that an echo of it
    /// can be recognised and a genuinely new selection cannot be mistaken for one.
    ///
    /// `None` means `identity` is already the agreed selection: the caller is looking at the
    /// echo of a change the other direction already applied, and must do nothing. A returned
    /// claim means the caller now owns the propagation; [`Self::restore_selection`] gives it back
    /// if the propagation did not happen after all.
    pub(crate) fn claim_selection(
        &self,
        uid: &marsh_core::shellmux::SnapshotUid,
        pane: rmux_core::PaneId,
    ) -> Option<SelectionClaim> {
        let identity = (uid.clone(), pane);
        let mut agreed = self
            .service
            .selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if agreed.as_ref() == Some(&identity) {
            return None;
        }
        let previous = agreed.replace(identity.clone());
        drop(agreed);
        Some(SelectionClaim { identity, previous })
    }

    /// Gives back a claim whose propagation never happened.
    ///
    /// Only when the claim is still the agreed selection: a later selection has every right to
    /// overwrite an abandoned one, and putting a stale identity back over it would suppress the
    /// newer change's echo instead of this one's.
    pub(crate) fn restore_selection(&self, claim: SelectionClaim) {
        let mut agreed = self
            .service
            .selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if agreed.as_ref() == Some(&claim.identity) {
            *agreed = claim.previous;
        }
    }

    /// Moves the service to `phase` and publishes the change.
    pub(crate) fn set_phase(&self, phase: IoPhase) {
        let mut current = self
            .service
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == phase {
            return;
        }
        *current = phase;
        drop(current);
        self.service.bus.publish(IoEvent::PhaseChanged { phase });
    }

    /// Releases the core, after caching everything a closed facade can still answer.
    ///
    /// Called once by the facade's own teardown, so the last strong reference to the mux — and
    /// through it the seed's exclusive lease — is dropped even while application clones are still
    /// held. The host slice that owns `ShellIo::shutdown` is what reaches it.
    #[allow(dead_code, reason = "called by the facade teardown path")]
    pub(crate) fn release_core(&self) {
        let mux = self
            .service
            .mux
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mux) = mux {
            let mut frozen = self
                .service
                .frozen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            frozen.executor = mux.executor_info();
            frozen.history = mux.history();
            drop(frozen);
        }
    }

    /// Tears the service down: no new work, outstanding work finished or discarded, core released.
    ///
    /// The sequence is fixed, and every step of it is load-bearing:
    ///
    /// 1. the phase moves to [`IoPhase::Closing`], so admission refuses immediately — before
    ///    anything is torn down, rather than after, which is what keeps a request accepted during
    ///    teardown from finding half a service;
    /// 2. the multiplexer shuts down: admission closes there too, every outstanding command's
    ///    native workers are asked to stop and its processes killed, every waiting receipt and
    ///    closure watch is resolved with [`WaitError::Shutdown`], and every job's snapshot is
    ///    reclaimed;
    /// 3. the core reference is released, so the last strong handle on the multiplexer — and
    ///    through it the seed's exclusive lease — is dropped even while application clones are
    ///    still held;
    /// 4. the phase moves to [`IoPhase::Closed`].
    ///
    /// Idempotent: a second call finds the core already released and does nothing further.
    ///
    /// Remaining [`ShellIo`], [`ShellHandle`], [`CommandHandle`] and stream clones stay usable.
    /// They answer frozen read-only state, report empty live state, and refuse new work; none of
    /// them keeps the seed leased. A caller that separately retained the `MarshExecutor` it built
    /// this host from is still an explicit lease owner, because it explicitly asked to be.
    ///
    /// A snapshot whose *approved* publication failed is deliberately left on disk. It is the
    /// content the next `MarshExecutor::open` replays from, and deleting it to make the shutdown
    /// look tidy would strand the durable log with nothing to recover from.
    ///
    /// # Errors
    ///
    /// Fails with the first termination failure observed while stopping outstanding commands.
    /// Every job is still reclaimed and the core is still released.
    ///
    /// # Examples
    ///
    /// Tearing the service down while handles are still outstanding, and what each of them
    /// answers afterwards. Needs a live host over a leased seed, so it is compiled rather than
    /// executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoError, IoPhase, IoResult, ShellIo};
    ///
    /// # async fn teardown(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .spawn(
    ///         "",
    ///         Some(ShellId::from("worker")),
    ///         Some("sleep 600"),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             environment: None,
    ///         },
    ///     )
    ///     .await?;
    /// let command = job.initial_command();
    /// let observation = io.observe();
    ///
    /// io.shutdown().await?;
    ///
    /// // Nothing was invalidated. What changed is what the outstanding handles answer.
    /// assert_eq!(io.snapshot().phase, IoPhase::Closed);
    /// assert!(io.jobs().is_empty());
    /// // Frozen read-only state still answers: where the seed was is a fair question once its
    /// // lease has gone.
    /// let _ = io.executor_info().seed;
    /// // New work is refused rather than silently dropped.
    /// assert!(matches!(io.keep(&job), Err(IoError::Closed)));
    ///
    /// if let Some(command) = command {
    ///     // `WaitError::Shutdown`, not `Aborted`, and not a verdict: teardown pre-empted an
    ///     // ordinary result. It does not prove the line had no effects, so it is not
    ///     // permission to retry one.
    ///     let _ = command.wait().await;
    /// }
    ///
    /// // The observer sees `PhaseChanged { phase: Closed }` and then end of stream — never a
    /// // stream that merely stops.
    /// let mut events = observation.events;
    /// while let Some(_envelope) = events.recv().await? {}
    /// # Ok(())
    /// # }
    /// ```
    pub async fn shutdown(&self) -> IoResult<()> {
        self.set_phase(IoPhase::Closing);
        // The core's own teardown joins the tasks it owns and drops every job's row on a blocking
        // worker. Both belong to this host's runtime, so the teardown is performed there even
        // when a library consumer calls `shutdown` from its own: a runtime cannot join tasks that
        // are not its own, and reclaiming a snapshot is filesystem work that has to finish.
        let outcome = match self.service.mux_opt() {
            Some(mux) => {
                self.dispatch(async move { mux.shutdown().await.map_err(IoError::from) })
                    .await
            }
            None => Ok(()),
        };
        self.release_core();
        self.set_phase(IoPhase::Closed);
        // Last, and only after the final phase has been published: an observer must see
        // `Closed` and then end of stream, never a stream that simply stops. Until the bus is
        // closed, a retained facade clone keeps its sender alive and every outstanding
        // `IoEventStream::recv` would block for the life of the process.
        self.service.bus.close();
        outcome
    }
}

/// Which rmux surface presents one job generation.
///
/// A job is not required to have one. A hidden pipe helper is real work with a real principal and
/// no pane; a popup lives on an overlay owned by one attached client; a spawn that failed leaves a
/// tombstone so the observation consumer does not adopt its `Opened` as an external job.
#[derive(Clone, Debug)]
#[allow(
    dead_code,
    reason = "Popup and Failed are installed by popup creation and the failed-spawn tombstone; \
              the observation consumer already reads every variant"
)]
pub(crate) enum Route {
    /// An ordinary pane, addressed by its session and its stable id.
    Pane {
        /// The session the pane's window currently lives in.
        session: rmux_proto::SessionName,
        /// The pane's stable id, which survives moves, links and renames.
        pane: rmux_core::PaneId,
        /// The pane output generation this job's bytes belong to.
        ///
        /// Reserved by the creating transaction before the job is admitted, so a respawn that
        /// replaces a pane's job can be told apart from the job it replaced even while both are
        /// briefly alive. Reading it back from the pane after installation would name whichever
        /// generation won the race instead.
        generation: u64,
    },
    /// A popup overlay owned by one attached client.
    Popup,
    /// Claimed by an adoption that has not finished yet.
    ///
    /// A reservation, not a surface. It exists so the claim can be taken under the admission lock
    /// and the lock then *released* before the handler's own state is touched: ordinary pane
    /// creation takes the handler state first and the admission lock second, so an adoption that
    /// held admission while waiting for handler state would invert the two and deadlock the
    /// daemon. The adoption replaces this with [`Route::Pane`] when it succeeds, or with
    /// [`Route::Hidden`] when it does not.
    Adopting,
    /// A helper job with native streams and no rmux surface at all.
    Hidden,
    /// A creation that failed after being admitted.
    Failed,
}

/// The right to propagate one selection change across the rmux/engine boundary.
///
/// Handed out by [`ShellIo::claim_selection`] and consumed either by the propagation succeeding —
/// in which case the claim is simply dropped and the identity stays agreed — or by
/// [`ShellIo::restore_selection`], which puts the previous agreement back so a propagation that
/// never happened does not permanently suppress the change it was meant to carry.
pub(crate) struct SelectionClaim {
    /// The stable instance this claim installed as the agreed selection.
    identity: (marsh_core::shellmux::SnapshotUid, rmux_core::PaneId),
    /// What was agreed before, to be restored if the propagation is abandoned.
    previous: Option<(marsh_core::shellmux::SnapshotUid, rmux_core::PaneId)>,
}

/// Keeps an accepted operation visible to the idle check until it finishes.
struct OperationGuard {
    /// The service whose counter to decrement.
    service: Arc<IoService>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.service
            .activity
            .operations
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        // As in `ShellIo::drop`: `wait_quiet` is woken by every activity decrement, because it
        // asks about all three counters and cannot tell which one it is waiting on.
        self.service.activity.retirement.notify_waiters();
    }
}
