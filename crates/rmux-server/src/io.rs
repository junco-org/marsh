//! The complete system-I/O interface this daemon exposes in process.
//!
//! # The layers, and who owns what
//!
//! ```text
//!   one ShellMux                      the shells, their snapshots, the publication gate
//!        ├── one leased btrfs seed    per distinct seed any of those shells discovered
//!        └── one ShellIo              this facade: admission, ownership, observation
//!             ├── ShellHandle         one generation of one named job
//!             ├── Execution           one managed command with real pipes
//!             ├── IoEventStream       one observer's bounded view
//!             └── SDK handles         rmux sessions, windows, panes over this host's socket
//! ```
//!
//! One managed multiplexer, one server and its I/O facade, and then as many shell, execution and
//! observer handles as an application cares to hold. There is exactly one of each of the first
//! two and no way to make a second: a second facade over one mux would be a second admission
//! path with its own idea of what is live. Handles are the plural layer — cloneable,
//! generation-bound, and owning nothing the service does not already own.
//!
//! Seeds are the other plural layer, and they are **lazy**. The host itself leases nothing; it
//! holds only a default directory for requests that name none. A shell's own `initial_dir`
//! discovers its seed, so one mux can host shells over several seeds at once, each with its own
//! exclusive lease, durable log and policy history. A seed already leased elsewhere therefore
//! fails the *spawn* that wants it, not the construction of this host, and [`ShellIo::seeds`]
//! answers empty until the first shell opens one.
//!
//! # Every capability, and the route to it
//!
//! | Multiplexer capability | Public application route |
//! |---|---|
//! | Construction, seed/lease ownership | [`RmuxFrontend::open`](crate::RmuxFrontend::open) plus [`ShellIo::seeds`]; no raw spawner escape |
//! | Default directory, policy history | [`default_dir`](ShellIo::default_dir), [`history`](ShellIo::history) |
//! | Shells, one shell, current selection | [`snapshot`](ShellIo::snapshot), [`jobs`](ShellIo::jobs), [`job`](ShellIo::job), [`current_job`](ShellIo::current_job), [`shell`](ShellIo::shell) |
//! | Create a shell | [`open_shell`](ShellIo::open_shell), [`keep`](ShellIo::keep) |
//! | Run a line and get its verdict | [`ShellHandle::run_command`], [`CommandHandle::wait`](marsh_core::shellmux::CommandHandle::wait) |
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
//! granted capabilities together with the Junco principal that earned them. Reopening the seed
//! reinstalls that same ownership before any shell can run a line; a restart never hands the
//! next caller a clean slate. A shell's principal is independent of its display name, which may
//! be reused when a pane closes or the daemon restarts. Denials identify the same principal
//! before and after recovery, so a new shell cannot inherit an earlier shell's stake.
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
    CommandHandle, CommandOptions, JobEnd, JobView, MuxError, OutputChannel, Principal, RunError,
    Shell, ShellId, SpawnOptions, TerminalGeometry, WaitError,
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
    /// A line ran through this host and did not end in an approved publication.
    ///
    /// The whole typed answer, not a message: a refused admission, a policy denial carrying the
    /// completion it refused, an unpublished conclusion carrying its original discriminant, and a
    /// lost answer are four different facts and a caller acts differently on each.
    #[error(transparent)]
    Run(#[from] RunError),
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

/// A handle on one generation of one named shell.
///
/// Opaque and generation-bound. A name is resolved exactly once, in [`ShellIo::shell`]; every
/// action afterwards acts through the retained core object, which owns its own live state and
/// resolves no name at all. A handle whose shell has closed can never reach the shell that later
/// took its name.
#[derive(Clone, Debug)]
pub struct ShellHandle {
    /// The host this handle came from, so one passed to another host is refused.
    origin: Arc<IoService>,
    /// The core's generation-bound shell.
    job: Shell,
}

impl ShellHandle {
    /// The shell's reusable display/lookup name, not its capability principal.
    #[must_use]
    pub fn id(&self) -> &ShellId {
        self.job.id()
    }

    /// Its logical source, directory label and stable principal.
    #[must_use]
    pub fn sandbox(&self) -> &marsh_core::shellmux::Sandbox {
        self.job.sandbox()
    }

    /// Which output streams this shell can produce. Fixed when it was admitted.
    ///
    /// A terminal shell answers `[Terminal]` and nothing else, and that is a statement about the
    /// *descriptors*, not about this facade's plumbing: the program's standard output and
    /// standard error are the same pseudoterminal, so they were never two streams and nothing
    /// downstream can separate them again. The terminal's own replies are mixed in with them.
    /// A pipe shell answers `[Stdout, Stderr]`, which are genuinely independent — byte-exact,
    /// separately ordered, and with no relative order promised between the two.
    #[must_use]
    pub fn output_channels(&self) -> &'static [OutputChannel] {
        self.job.output_channels()
    }

    /// Runs one line in this shell and answers with what the gate made of it.
    ///
    /// Resolving is *completion*, never acceptance. `Ok` means and only means
    /// [`Outcome::Published`](marsh_core::Outcome::Published): a line whose process exited
    /// nonzero but whose effects were published is `Ok`, and a line that exited zero and was
    /// refused is [`IoError::Run`] carrying [`RunError::Policy`].
    ///
    /// The command runs on this host's own runtime whoever calls, and belongs to it. Dropping
    /// this future abandons the *answer*, never the work.
    ///
    /// A caller that has to act while the line runs — feed standard input, end that input,
    /// signal it — supplies
    /// [`CommandOptions::on_accept`](marsh_core::shellmux::CommandOptions::on_accept) and gets the
    /// receipt at admission instead of waiting here.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::WrongHost`] for a foreign handle, [`IoError::Closed`] after
    /// teardown, and [`IoError::Run`] with whatever the core made of the line.
    ///
    /// # Examples
    ///
    /// One persistent shell running several lines, each gated on its own. Needs a live host, a
    /// directory inside a btrfs seed and real programs, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{CommandOptions, JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoError, IoResult, ShellIo};
    ///
    /// # async fn build(io: &ShellIo) -> IoResult<()> {
    /// let shell = io
    ///     .open_shell(
    ///         std::path::Path::new(""),
    ///         Some(ShellId::from("builder")),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             ..SpawnOptions::default()
    ///         },
    ///     )
    ///     .await?;
    ///
    /// // One shell, one principal, three lines. The shell outlives all of them, which is how an
    /// // application accumulates approved state instead of inventing a principal per line.
    /// for line in ["./configure --prefix=/usr", "make -j8", "make install"] {
    ///     match shell.run_command(line, CommandOptions::default()).await {
    ///         // The only success: the line's staged changes are in the seed. A zero exit code
    ///         // never gets here on its own.
    ///         Ok(_published) => {}
    ///         // Ran, and changed nothing. Continuing would build on state that does not exist.
    ///         Err(IoError::Run(_)) => break,
    ///         Err(other) => return Err(other),
    ///     }
    /// }
    ///
    /// io.stop(&shell, false).await
    /// # }
    /// ```
    pub async fn run_command(
        &self,
        cmd: &str,
        options: CommandOptions,
    ) -> IoResult<Arc<marsh_core::shellmux::CommandCompletion>> {
        let io = ShellIo {
            service: Arc::clone(&self.origin),
            leased: false,
        };
        let _mux = io.service.admit()?;
        let work = io.begin_operation();
        let shell = self.job.clone();
        let cmd = cmd.to_owned();
        io.dispatch(async move {
            let _work = work;
            Ok(shell.run_command(&cmd, options).await?)
        })
        .await
    }

    /// Waits for this shell to close: every stream ended, snapshot reclaimed.
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

    /// The core shell, for this crate's own handlers.
    ///
    /// Read-only server probes borrow this: the terminal's name, its foreground process group,
    /// and whether the generation has already closed. It is never an I/O route: input, resizing
    /// and stopping all go back through the facade so a single host owns the ordering.
    pub(crate) const fn shell(&self) -> &Shell {
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
    /// The absolute directory a request that names none starts its shell in.
    ///
    /// Captured at construction and never resolved against a seed: it is a plain host path, and
    /// which seed it lies in — if any — is decided by the spawn that uses it. Answered after
    /// shutdown too, because where the daemon was started is still a legitimate question.
    default_dir: PathBuf,
    /// Retained bytes and registered readers, per job stream.
    streams: streams::Streams,
    /// Which rmux surface presents each job generation.
    ///
    /// Keyed by principal, never by name or index: a pane can be moved, linked or renamed, and
    /// a name can be reused, so either of those would eventually address the wrong thing.
    routes: std::sync::Mutex<std::collections::HashMap<marsh_core::shellmux::Principal, Route>>,
    /// The one selection the rmux surface and the engine have agreed on.
    ///
    /// Written by whichever side is about to propagate a change and read by the other side when
    /// it observes that change arriving, which is what keeps the two directions from handing one
    /// selection back and forth forever. Stored as a stable instance — one principal presented
    /// by one stable pane id — because an index, a name or an output generation on its own can
    /// each name a different thing after a move, a rename or a respawn.
    selection: std::sync::Mutex<Option<(marsh_core::shellmux::Principal, rmux_core::PaneId)>>,
    /// The shell this front-end is looking at, with the generation that was selected.
    ///
    /// Selection is presentation, so it lives here and not in the engine: the collection indexes
    /// shells by name and has no opinion about what anyone is watching. The principal is
    /// stored beside the name because a name comes back after a shell closes, and a stale
    /// selection must not silently follow the replacement.
    ///
    /// Deliberately not [`Self::selection`], which is the bidirectional echo-suppression claim
    /// rather than an authoritative choice, and deliberately not a [`ShellHandle`], which would
    /// hold an `Arc` back on this service and make the pair a cycle.
    current_shell: std::sync::Mutex<Option<(ShellId, Principal)>>,
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
    /// `open_shell` call has returned would be adopted as a second, duplicate pane.
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

/// What keeps this daemon from deciding it is idle.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    /// Public handles held by the owning library host.
    leases: std::sync::atomic::AtomicUsize,
    /// Operations accepted and not yet finished.
    operations: std::sync::atomic::AtomicUsize,
    /// Admitted jobs, until their closure or failed open.
    instances: std::sync::Mutex<std::collections::HashSet<marsh_core::shellmux::Principal>>,
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
        std::collections::HashMap<marsh_core::shellmux::Principal, marsh_core::shellmux::CommandId>,
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
    /// Builds the one service every handle of this daemon shares.
    ///
    /// This is the *only* place a mux is constructed in this crate, and it is why nothing has to
    /// check afterwards that a frontend belongs to a mux, that a queue was not already taken, or
    /// that the support builtins were registered: each of those is true by construction here.
    ///
    /// In order, because no step may be reordered:
    ///
    /// 1. capture the default starting directory, absolute and unresolved;
    /// 2. build the frontend queue, because the mux reads its geometry during construction;
    /// 3. freeze one profile, so every shell in the process holds the identical builtin set;
    /// 4. build exactly one mux over that profile and that backend.
    ///
    /// No seed is discovered, leased or recovered: each shell's own starting directory selects
    /// the seed it publishes into, so `initial_dir` here is only the default for requests that
    /// name none. It is made absolute rather than canonicalized, so a listener may be started in
    /// a directory that is not a subvolume at all and still serve a shell inside one.
    ///
    /// Returns the **unleased** facade and the queue's unique consumer end together, so the
    /// caller cannot end up with one and not the other.
    ///
    /// `runtime` is the runtime every managed operation and every task this facade creates lands
    /// on, including ones requested from a status thread, a foreign runtime or a detached command
    /// queue. It must be the runtime this call is entered into: [`ShellMux::new_with`] captures
    /// the ambient one for the job pumps.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Transport`] when `initial_dir` cannot be made absolute, and with
    /// [`IoError::Mux`] when a dimension of `geometry` is zero.
    pub(crate) fn new<F>(
        initial_dir: &std::path::Path,
        environment: brush_core::env::ShellEnvironment,
        geometry: TerminalGeometry,
        runtime: tokio::runtime::Handle,
        socket: PathBuf,
        create_mux: F,
    ) -> IoResult<(
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::shell_frontend::FrontendMessage>,
    )>
    where
        F: FnOnce(
            marsh_core::shellmux::MuxProfile,
            Arc<std::sync::Mutex<crate::shell_frontend::FrontendQueue>>,
        ) -> Result<Arc<marsh_core::shellmux::ShellMux>, MuxError>,
    {
        use marsh_core::shellmux::ShellFrontend as _;

        let default_dir = std::path::absolute(if initial_dir.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            initial_dir
        })?;

        let mut frontend = crate::shell_frontend::FrontendQueue::new(geometry.rows, geometry.cols);
        let events = frontend
            .receiver
            .take()
            .expect("fresh frontend owns its observation queue");

        // Each shell receives its own opaque builtin registration; no process-global installer.
        let mut builtins = std::collections::HashMap::new();
        builtins.insert(
            builtins::RMUX_IO_BUILTIN.to_string(),
            builtins::registration(),
        );
        let profile = marsh_core::shellmux::MuxProfile {
            environment,
            builtins,
            sandbox_policy: marsh_core::SandboxPolicy::default(),
        };

        let mux = create_mux(profile, Arc::new(std::sync::Mutex::new(frontend)))?;
        let io = Self {
            service: Arc::new(IoService {
                mux: std::sync::Mutex::new(Some(mux)),
                bus: events::EventBus::new(),
                phase: std::sync::Mutex::new(IoPhase::Running),
                runtime,
                activity: Activity::default(),
                socket,
                default_dir,
                streams: streams::Streams::default(),
                routes: std::sync::Mutex::new(std::collections::HashMap::new()),
                selection: std::sync::Mutex::new(None),
                current_shell: std::sync::Mutex::new(None),
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
            || activity
                .operations
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
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
    pub(crate) fn wrap(&self, job: Shell) -> ShellHandle {
        ShellHandle {
            origin: Arc::clone(&self.service),
            job,
        }
    }

    /// Refuses once the core has been released, for the read paths that must not answer emptily.
    ///
    /// Distinct from [`IoService::admit`]: this permits a service that is still *closing*, for
    /// operations that only observe. What it refuses is a service with no core at all, where the
    /// honest answer is [`IoError::Closed`] rather than a handle onto noShell
    pub(crate) fn ensure_open(&self) -> IoResult<()> {
        self.service.mux().map(drop)
    }

    /// Checks a handle came from this host.
    pub(crate) fn owned<'a>(&self, job: &'a ShellHandle) -> IoResult<&'a Shell> {
        if Arc::ptr_eq(&job.origin, &self.service) {
            Ok(&job.job)
        } else {
            Err(IoError::WrongHost)
        }
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

    /// The absolute directory a request that names none starts its shell in.
    ///
    /// A plain host path, not a seed-relative label and not a seed: which seed it lies in — if
    /// any — is decided by the spawn that uses it. Answered after shutdown too.
    #[must_use]
    pub fn default_dir(&self) -> &std::path::Path {
        &self.service.default_dir
    }

    /// One consistent look at the service, the core behind it, and this front-end's selection.
    #[must_use]
    pub fn snapshot(&self) -> IoSnapshot {
        let registration = self.service.bus.registration();
        let state = self.service.mux_opt().map_or_else(
            || marsh_core::shellmux::MuxSnapshot {
                jobs: Vec::new(),
                commands: Vec::new(),
                default_geometry: TerminalGeometry { rows: 24, cols: 80 },
            },
            |mux| mux.snapshot(),
        );
        let next_event_sequence = *registration;
        // Resolved against the very shells this snapshot returns, under the same registration
        // boundary: a selection validated against a different read could name a shell this
        // snapshot does not contain.
        let current = self.selected_in(&state.jobs);
        drop(registration);
        IoSnapshot {
            phase: self.service.phase(),
            state,
            current,
            next_event_sequence,
        }
    }

    /// Every visible shell, in creation order. Empty once the host has closed.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        self.service
            .mux_opt()
            .map(|mux| mux.jobs())
            .unwrap_or_default()
    }

    /// One shell's view by name.
    #[must_use]
    pub fn job(&self, id: &ShellId) -> Option<JobView> {
        self.service.mux_opt().and_then(|mux| mux.job(id))
    }

    /// The selected shell, if one is selected and still visible as the generation that was
    /// selected.
    ///
    /// Both halves are checked. A selected shell that was stopped and whose name was then
    /// reopened answers `None` rather than silently handing the replacement the selection its
    /// predecessor earned.
    #[must_use]
    pub fn current_job(&self) -> Option<JobView> {
        let (id, uid) = self.selected()?;
        let view = self.job(&id)?;
        (view.sandbox.uid == uid).then_some(view)
    }

    /// This front-end's stored selection, name and generation together.
    fn selected(&self) -> Option<(ShellId, Principal)> {
        self.service
            .current_shell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The selected shell's name, as matched against exactly the `views` a caller is reporting.
    fn selected_in(&self, views: &[JobView]) -> Option<ShellId> {
        let (id, uid) = self.selected()?;
        views
            .iter()
            .find(|view| view.id == id && view.sandbox.uid == uid)
            .map(|view| view.id.clone())
    }

    /// Forgets the selection when it names `uid`.
    ///
    /// Generation-keyed on purpose: a late close of an older instance must not clear a selection
    /// that has since moved to a newer one.
    fn clear_selection_of(&self, uid: &Principal) {
        let mut current = self
            .service
            .current_shell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.as_ref().is_some_and(|(_, held)| held == uid) {
            *current = None;
        }
        drop(current);
    }

    /// Resolves a name to a handle, once.
    ///
    /// The *only* place a name becomes a handle. Every later action acts through that object
    /// rather than re-resolving the name, which is what stops an action from reaching a different
    /// shell that took the name in between.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Closed`] after teardown and [`MuxError::NoSuchJob`] when nothing
    /// visible answers to `id`.
    pub fn shell(&self, id: &ShellId) -> IoResult<ShellHandle> {
        let mux = self.service.mux()?;
        mux.get_shell(id)
            .map(|job| self.wrap(job))
            .ok_or_else(|| IoError::from(MuxError::NoSuchJob(id.clone())))
    }

    /// Opens a shell.
    ///
    /// `initial_dir` is a **host filesystem path** the shell starts in, and the seed it publishes
    /// into is discovered from it: an empty path selects [`Self::default_dir`], a relative one is
    /// joined onto that default, and an absolute one keeps its host meaning. One host therefore
    /// serves shells over as many seeds as its callers name. `id` is only the shell's display and
    /// lookup name; `None` draws the next automatic one. `SpawnOptions::default()` is a
    /// terminal shell at this host's default geometry, with no environment replacement, that
    /// persists across every command run in it.
    ///
    /// This creates and nothing else. [`ShellHandle::run_command`] is how a line runs, and it is
    /// a separate call because a caller frequently has to arm output receivers or install a route
    /// between the two.
    ///
    /// Returning is *acceptance*: the shell has a principal and an identity, and its resources
    /// may still be opening. Readiness is [`IoEvent::Opened`]; the end of its life is
    /// [`ShellHandle::wait_closed`]. Dropping this future after admission does not un-admit the
    /// shell — an admitted shell belongs to the core, and it stays observable whether or not the
    /// caller waited for it.
    ///
    /// Admission, and every task the new shell owns, happen on this host's own runtime whoever
    /// calls — a shell admitted from a status thread or a foreign runtime does not die with it.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Closed`] once teardown has begun, and with whatever the core reported
    /// about the name, the directory, the seed, the geometry or the shell.
    pub async fn open_shell(
        &self,
        initial_dir: &std::path::Path,
        id: Option<ShellId>,
        options: SpawnOptions,
    ) -> IoResult<ShellHandle> {
        let mux = self.service.admit()?;
        let work = self.begin_operation();
        // Resolved against this host's default before dispatch, because the default is the
        // *host's* and the core knows only the process's own directory.
        let initial_dir = if initial_dir.as_os_str().is_empty() {
            self.service.default_dir.clone()
        } else {
            self.service.default_dir.join(initial_dir)
        };
        // The instance is recorded inside the dispatched work, not after it: a caller that drops
        // this future has still admitted a shell, and the idle check has to see it either way.
        let service = Arc::clone(&self.service);
        let job = self
            .dispatch(async move {
                let _work = work;
                let job = mux.open_shell(&initial_dir, id, options).await?;
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

    /// Schedules one line into an open shell and waits only for its *admission*.
    ///
    /// Server-private, and deliberately the only thing in this crate that still returns a receipt
    /// instead of a verdict. A pane, a popup and an execution all have to go on doing something
    /// while their line runs — draw a prompt, feed standard input, end that input so a program
    /// reading to end of file can finish — and none of them can block on the completion to get
    /// there. After this returns, the receipt and the existing observation events are the
    /// authoritative completion; nothing here re-decides policy or runs an interpreter.
    ///
    /// The run itself is owned by this host's runtime, not by the caller's future: dropping the
    /// returned receipt cancels nothing.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::WrongHost`] for a foreign handle, [`IoError::Closed`] after teardown,
    /// and with whatever the core made of the line when it never reached admission at all.
    pub(crate) async fn start_command(
        &self,
        job: &ShellHandle,
        cmd: &str,
        options: CommandOptions,
    ) -> IoResult<CommandHandle> {
        self.service.admit()?;
        let shell = self.owned(job)?.clone();
        let work = self.begin_operation();
        let cmd = cmd.to_owned();
        let (accepted, admission) = tokio::sync::oneshot::channel();
        let options = CommandOptions {
            on_accept: Some(accepted),
            ..options
        };
        let runtime = self.service.runtime.clone();
        let run = runtime.spawn(async move {
            let _work = work;
            shell.run_command(&cmd, options).await
        });
        match admission.await {
            Ok(command) => Ok(command),
            // The sender went away without admitting: the real reason is whatever the scheduled
            // call is about to report, so it is awaited rather than guessed at. A completed run
            // with no receipt is impossible by construction and is reported as the internal
            // failure it would be rather than papered over with a fabricated handle.
            Err(_) => match run.await {
                Ok(Err(error)) => Err(IoError::Run(error)),
                Ok(Ok(_)) => Err(IoError::from(MuxError::Task(
                    "command finished without an admission receipt".to_owned(),
                ))),
                Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
                Err(_) => Err(IoError::Closed),
            },
        }
    }

    /// Cancels a shell's automatic closure, so it outlives the command that would have ended it.
    ///
    /// `true` means the closure was still cancellable and has been cancelled. `false` means there
    /// was none to cancel — the shell was opened without
    /// [`SpawnOptions::automatic_close`](marsh_core::shellmux::SpawnOptions::automatic_close) —
    /// or that its command already reached the finish/close gate and the decision is made.
    /// Neither answer is an error, and `false` is never a reason to retry.
    ///
    /// Synchronous and immediate: there is no admitted work to wait for. Bound to one generation,
    /// so a handle whose shell has closed cannot retain whatever later took its name. Retaining
    /// is for terminal shells; a pipe shell is one-shot by construction and closes after its
    /// single command whatever this says.
    ///
    /// # Errors
    ///
    /// Fails for a foreign or stale handle, and after teardown.
    ///
    /// # Examples
    ///
    /// A shell opened with an automatic closure, its line scheduled, and that closure cancelled
    /// so the shell stays open afterwards. Needs a live host, a directory inside a btrfs seed and
    /// a real program to run, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{CommandOptions, JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoResult, ShellIo};
    ///
    /// # async fn watcher(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .open_shell(
    ///         std::path::Path::new(""),
    ///         Some(ShellId::from("watcher")),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             automatic_close: true,
    ///             ..SpawnOptions::default()
    ///         },
    ///     )
    ///     .await?;
    ///
    /// // Without this, the shell closes when the line below ends. With it, the shell is an
    /// // ordinary persistent one and closing it becomes the caller's job.
    /// let retained = io.keep(&job)?;
    ///
    /// // Completion, with the verdict: a forced stop resolves here too, as an error carrying
    /// // `Outcome::Discarded` and no exit code. That is the verdict, not the absence of one.
    /// let _ = job.run_command("cargo watch -x test", CommandOptions::default()).await;
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
        self.ensure_open()?;
        Ok(self.owned(job)?.keep()?)
    }

    /// Writes the shell table to `out`, exactly as the console builtin renders it.
    ///
    /// A forwarder rather than a second renderer. `jobctl::print_jobs` takes the collection,
    /// which this facade deliberately never hands out, so the one place that has it lends it for
    /// the length of the call — and the interactive prompt's `jobs` output stays byte for byte
    /// what `jobs` prints anywhere else instead of drifting into a private column layout.
    ///
    /// The selection marker is this facade's, because selection is this facade's: the collection
    /// has none to lend.
    ///
    /// Writes nothing once the core has been released: there is no table left to print.
    pub(crate) fn print_jobs(&self, out: &mut dyn std::io::Write) {
        if let Some(mux) = self.service.mux_opt() {
            let current = self.current_job().map(|view| view.id);
            marsh_core::shellmux::jobctl::print_jobs(&mux, current.as_ref(), out);
        }
    }

    /// Selects a terminal shell as the one this front-end is looking at.
    ///
    /// Selection is presentation and lives here: the engine indexes shells by name and has
    /// no opinion about what anyone is watching. Nothing is started, and the shell's own state is
    /// unchanged apart from [`keep`](Self::keep) cancelling an automatic closure — a reader who
    /// brought a shell up means to look at it, so it is no longer one the series may reclaim.
    ///
    /// The stored selection is the `(name, generation)` pair, checked again on every read: a
    /// shell that was stopped and whose name was reopened cannot inherit the selection its
    /// predecessor earned.
    ///
    /// # Errors
    ///
    /// Fails for a pipe shell, a closing shell, a foreign or stale handle, and after teardown.
    pub async fn switch(&self, job: &ShellHandle) -> IoResult<JobView> {
        self.ensure_open()?;
        let shell = self.owned(job)?.clone();
        let view = shell.view()?;
        if !view.io.is_terminal() {
            return Err(IoError::from(MuxError::NotTerminal(view.id)));
        }
        if view.closing {
            return Err(IoError::from(MuxError::JobClosing(view.id)));
        }
        let _ = shell.keep()?;
        // Outside every state lock, and before the recheck: a shell whose launch is still in
        // flight is not yet something a display can present.
        let view = self
            .dispatch(async move { Ok(shell.wait_ready().await?) })
            .await?;
        if view.closing {
            return Err(IoError::from(MuxError::StaleJob(view.id)));
        }
        {
            let mut current = self
                .service
                .current_shell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = Some((view.id.clone(), view.sandbox.uid.clone()));
            drop(current);
        }
        // Published after the state moved, on the same bus a subscription registers against, so
        // an observer cannot miss both the new state and the event announcing it.
        self.service.bus.publish(IoEvent::Changed);
        // Outside the lock: the reverse handler reads this facade back, and holding selection
        // state across it would deadlock the first handler that asks what is selected.
        if let Some(handler) = self.handler() {
            handler.note_shell_state_changed();
        }
        Ok(view)
    }

    /// Stops a shell, gracefully or by force.
    ///
    /// `force = false` closes the shell and lets whatever is running reach its ordinary
    /// boundary, so its staged work is gated as usual. `force = true` retires it: processes are
    /// killed, native workers are cancelled and joined, and the line's staged changes are
    /// **discarded** — which resolves its run as an error carrying
    /// [`Outcome::Discarded`](marsh_core::Outcome::Discarded) and no exit code. That is a
    /// verdict, not a lost one, and it is not evidence that nothing ran.
    ///
    /// The cancel-versus-conclude decision is linearized under the shell's own lock. A force
    /// accepted before finalization discards; a force arriving once an approved publication has
    /// begun cannot undo it, and the completion that eventually lands is authoritative. No
    /// rollback of published effects is promised, because none is possible.
    ///
    /// Returning is acceptance of the stop, not the shell's closure: every stream still has to
    /// end and the snapshot still has to be reclaimed. [`ShellHandle::wait_closed`] is that later
    /// boundary.
    ///
    /// # Errors
    ///
    /// Fails when a forced shell's processes could not be signalled, for a foreign or stale
    /// handle, and after teardown.
    pub async fn stop(&self, job: &ShellHandle, force: bool) -> IoResult<()> {
        self.ensure_open()?;
        let shell = self.owned(job)?.clone();
        self.dispatch(async move { Ok(shell.stop(force).await?) })
            .await
    }

    /// Signals the processes the running command started.
    ///
    /// Only that command's own process groups; this is not an arbitrary-pid interface. Best
    /// effort, and it does not retire the shell: a command that catches the signal and finishes
    /// normally is gated normally.
    ///
    /// # Errors
    ///
    /// Fails for a foreign or stale handle, after teardown, and when the signal could not be
    /// delivered for a reason other than the process already being gone.
    pub fn signal(&self, job: &ShellHandle, signal: marsh_core::Signal) -> IoResult<()> {
        self.ensure_open()?;
        Ok(self.owned(job)?.signal(signal)?)
    }

    /// Resizes one terminal shell.
    ///
    /// # Errors
    ///
    /// Fails for a zero dimension, a pipe shell, a foreign or stale handle, and after teardown.
    pub async fn resize(&self, job: &ShellHandle, size: TerminalGeometry) -> IoResult<()> {
        self.ensure_open()?;
        let shell = self.owned(job)?.clone();
        self.dispatch(async move { Ok(shell.resize(size).await?) })
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
    /// Keystrokes into a terminal shell, then a resize the program actually observes. Needs a
    /// live host, a seed directory and a real pseudoterminal, so it is compiled rather than
    /// executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{JobIo, ShellId, SpawnOptions, TerminalGeometry};
    /// use rmux_server::io::{IoResult, ShellIo};
    ///
    /// # async fn drive(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .open_shell(
    ///         std::path::Path::new(""),
    ///         Some(ShellId::from("pane")),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal {
    ///                 geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
    ///             },
    ///             ..SpawnOptions::default()
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
    /// // There is no end-of-file to send here at all: `close_input` refuses a terminal shell,
    /// // and Ctrl-D would be a keystroke rather than a writer going away.
    ///
    /// // A real window-size change plus the SIGWINCH it implies, so a full-screen program
    /// // repaints. Zero in either dimension is refused rather than clamped.
    /// io.resize(&job, TerminalGeometry { rows: 50, cols: 132 }).await?;
    /// // The same for every live terminal, and the default future ones open at. Pipe shells are
    /// // skipped rather than failed: they have no geometry to change.
    /// io.resize_all(TerminalGeometry { rows: 50, cols: 132 }).await
    /// # }
    /// ```
    pub async fn write_input(&self, job: &ShellHandle, bytes: &[u8]) -> IoResult<()> {
        self.ensure_open()?;
        let shell = self.owned(job)?;
        // The one mutation with a borrowed payload, and the one this daemon performs per
        // keystroke: a caller already on this runtime writes the caller's own slice, and only a
        // foreign one pays for the copy a handed-over future needs to own.
        if self.on_bound_runtime() {
            return Ok(shell.write_input(bytes).await?);
        }
        let shell = shell.clone();
        let bytes = bytes.to_vec();
        self.dispatch(async move { Ok(shell.write_input(&bytes).await?) })
            .await
    }

    /// Ends a pipe shell's standard input.
    ///
    /// A real end-of-file, ordered after every accepted write, and idempotent. Not available for
    /// a terminal shell: a pseudoterminal has no half-close, and sending Ctrl-D instead would be
    /// a keystroke, not an end of file.
    ///
    /// # Errors
    ///
    /// Fails for a terminal shell, a foreign or stale handle, and after teardown.
    pub async fn close_input(&self, job: &ShellHandle) -> IoResult<()> {
        self.ensure_open()?;
        let shell = self.owned(job)?.clone();
        self.dispatch(async move { Ok(shell.close_input().await?) })
            .await
    }

    /// Leases a terminal shell's slave side while no command is running in it.
    ///
    /// How an interactive prompt reads *this pane's* keyboard. Revoked the instant a command is
    /// admitted.
    ///
    /// The lease's descriptor is registered with this host's reactor, not the caller's, so a
    /// prompt driven from one runtime and leased from another still reads.
    ///
    /// # Errors
    ///
    /// Fails for a pipe shell, a busy shell, an outstanding lease, a foreign or stale handle, and
    /// after teardown.
    pub async fn idle_terminal(
        &self,
        job: &ShellHandle,
    ) -> IoResult<marsh_core::shellmux::IdleTerminal> {
        self.service.admit()?;
        let shell = self.owned(job)?.clone();
        self.dispatch(async move { Ok(shell.idle_terminal()?) })
            .await
    }

    /// Whether `line` is a complete shell command for this shell's interpreter.
    ///
    /// `false` only for an incomplete tokenization or an end-of-input parse failure. Any other
    /// syntax error answers `true`, so the line runs and the shell produces its own diagnostic.
    ///
    /// # Errors
    ///
    /// Fails for a busy shell, a pipe shell, a foreign or stale handle, and after teardown.
    pub async fn input_is_complete(&self, job: &ShellHandle, line: &str) -> IoResult<bool> {
        self.ensure_open()?;
        let shell = self.owned(job)?.clone();
        let line = line.to_string();
        self.dispatch(async move { Ok(shell.input_is_complete(&line).await?) })
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
    /// Reconciling the snapshot with the events that continue from it. Needs a live host, so it
    /// is compiled rather than executed.
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
                default_geometry: TerminalGeometry { rows: 24, cols: 80 },
            },
            |mux| mux.snapshot(),
        );
        let current = self.selected_in(&state.jobs);
        drop(registration);
        Observation {
            snapshot: IoSnapshot {
                phase: self.service.phase(),
                state,
                current,
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

    /// Records that a shell's instance has terminated, for the idle check.
    ///
    /// The selection goes with it when it named *this* generation. A late close of an older
    /// instance leaves a newer selection alone, which is why the check is keyed by uid.
    pub(crate) fn retire_instance(&self, uid: &marsh_core::shellmux::Principal) {
        self.service
            .activity
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(uid);
        self.clear_selection_of(uid);
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
        uid: &marsh_core::shellmux::Principal,
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
        uid: &marsh_core::shellmux::Principal,
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
    pub(crate) fn install_route(&self, uid: marsh_core::shellmux::Principal, route: Route) {
        self.service
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(uid, route);
    }

    /// Whether one job generation already has a surface, whatever kind.
    pub(crate) fn has_route(&self, uid: &marsh_core::shellmux::Principal) -> bool {
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
    pub(crate) fn is_popup_route(&self, uid: &marsh_core::shellmux::Principal) -> bool {
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
    pub(crate) fn forget_route(&self, uid: &marsh_core::shellmux::Principal) {
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
        uid: &marsh_core::shellmux::Principal,
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
    ///
    /// The selection goes too: with no collection left there is no shell to be looking at, and a
    /// retained name would otherwise outlive every generation it could have meant.
    #[allow(dead_code, reason = "called by the facade teardown path")]
    pub(crate) fn release_core(&self) {
        *self
            .service
            .current_shell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let mux = self
            .service
            .mux
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(mux);
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
    /// answers afterwards. Needs a live host and a job on a seed, so it is compiled rather than
    /// executed.
    ///
    /// ```no_run
    /// use marsh_core::shellmux::{CommandOptions, JobIo, ShellId, SpawnOptions};
    /// use rmux_server::io::{IoError, IoPhase, IoResult, ShellIo};
    ///
    /// # async fn teardown(io: &ShellIo) -> IoResult<()> {
    /// let job = io
    ///     .open_shell(
    ///         std::path::Path::new(""),
    ///         Some(ShellId::from("worker")),
    ///         SpawnOptions {
    ///             io: JobIo::Terminal { geometry: None },
    ///             ..SpawnOptions::default()
    ///         },
    ///     )
    ///     .await?;
    /// let observation = io.observe();
    ///
    /// // Owned by this host from its first poll, so the shutdown below pre-empts it rather than
    /// // waiting for it. Its answer is abandoned here; the verdict still reaches every observer.
    /// let running = job.clone();
    /// let line = tokio::spawn(async move {
    ///     running.run_command("sleep 600", CommandOptions::default()).await
    /// });
    ///
    /// io.shutdown().await?;
    ///
    /// // Nothing was invalidated. What changed is what the outstanding handles answer.
    /// assert_eq!(io.snapshot().phase, IoPhase::Closed);
    /// assert!(io.jobs().is_empty());
    /// // The directory a request naming none would have started in answers after teardown: it
    /// // was never the mux's to begin with.
    /// let _ = io.default_dir();
    /// // New work is refused rather than silently dropped.
    /// assert!(matches!(io.keep(&job), Err(IoError::Closed)));
    ///
    /// // `WaitError::Shutdown`, not `Aborted`, and not a verdict: teardown pre-empted an
    /// // ordinary result. It does not prove the line had no effects, so it is not permission to
    /// // retry one.
    /// let _ = line.await;
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
    identity: (marsh_core::shellmux::Principal, rmux_core::PaneId),
    /// What was agreed before, to be restored if the propagation is abandoned.
    previous: Option<(marsh_core::shellmux::Principal, rmux_core::PaneId)>,
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
