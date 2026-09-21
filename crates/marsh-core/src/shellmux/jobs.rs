//! The job table: the shells a front-end has open, the commands running in them, the streams they
//! own, and the names they answer to.
//!
//! A job is a *sandbox*, not a command: it outlives the commands that run in it, and its
//! [`ShellId`] is the principal those commands request capabilities as. That is why the table
//! lives here rather than in a front-end: a job's name and a principal's name are one identity,
//! and two registries of it would drift.
//!
//! Every job owns its standard descriptors from the moment it is created — a pseudoterminal, or
//! three real pipes — so a front-end reads bytes rather than sharing the process's own terminal,
//! and a full-screen program behaves as it would under any other shell.
//!
//! One command at a time per job. The table is never held across a launch, a line, a callback or a
//! reclamation, so [`ShellMux::snapshot`] answers while a command is starting and while another is
//! running.
//!
//! # Identity, and why handles are checked twice
//!
//! Every mutation validates two things under the same lock that admits it: that the handle came
//! from *this* mux, and that the `(ShellId, SnapshotUid)` pair it names is still the pair in the
//! table. A name can be reused; a snapshot id cannot. So a handle retained across a job's close
//! and its name's reuse fails with [`MuxError::StaleJob`] rather than reaching the replacement —
//! and it fails *atomically with the action*, never as a separate check the action could outrace.

use std::collections::HashMap;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use brush_core::SourceInfo;
use tokio::io::unix::AsyncFd;

use crate::shellmux::command::{
    CommandCompletion, CommandHandle, CommandId, CommandState, WaitError,
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

/// Why a job is to close once its command finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobCloseMode {
    /// An unnamed background command; a reader may keep this job.
    Automatic,
    /// Close after normal command completion, because a reader or the command's own options asked.
    Graceful,
    /// Abort and retire the job immediately.
    Force,
}

impl JobCloseMode {
    /// Whether this closure is a decision rather than the `1`, `2`, … series' default.
    const fn explicit(self) -> bool {
        !matches!(self, Self::Automatic)
    }
}

/// How one job ended.
///
/// Delivered once per job, after every byte of every one of its streams and after its snapshot has
/// been reclaimed. A job whose construction failed reports that failure here without ever having
/// been [`Opened`](FrontendEvent::Opened).
#[derive(Debug)]
pub struct JobEnd {
    /// The job that ended.
    pub shell: Sandbox,
    /// Why it closed, or `None` when its streams simply ended.
    pub close_mode: Option<JobCloseMode>,
    /// The verdict of the last command that ran in it, when one did.
    pub completion: Option<Arc<CommandCompletion>>,
    /// The infrastructure failure that ended it, when one did.
    ///
    /// A construction that never produced a usable job reports here. This is not an exit status
    /// and must never be rendered as one.
    pub error: Option<Arc<MuxError>>,
    /// Whether the session is now waiting for a write-ahead log replay.
    pub recovery_required: bool,
}

/// The state one job's closure watch carries.
#[derive(Clone, Debug)]
enum ClosedState {
    /// Still open.
    Pending,
    /// Closed, with the end every waiter shares.
    Done(Arc<JobEnd>),
    /// The mux was torn down before an ordinary closure could be delivered.
    Shutdown,
}

/// The endpoints a job's handle keeps, so input still reaches a job whose public row is gone.
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

/// The shared half of a job handle.
#[derive(Debug)]
struct SpawnedState {
    /// The job's identity.
    id: ShellId,
    /// Its sandbox: identity, directory label and snapshot id.
    sandbox: Sandbox,
    /// Whether it is a terminal or a pipe job. Fixed at admission.
    terminal: bool,
    /// The mux that admitted it, so a handle passed to another mux is refused rather than acted
    /// on. Weak, because a handle must not keep a torn-down mux alive.
    origin: Weak<ShellMux>,
    /// Filled once the job's streams and shell are open; absent while it is still constructing.
    endpoints: OnceLock<Endpoints>,
    /// The command the job was opened for, reserved before the launch was even scheduled.
    initial: Mutex<Option<CommandHandle>>,
    /// The job's closure.
    closed: tokio::sync::watch::Receiver<ClosedState>,
    /// Set once a forced stop retires the job, so its output readers close.
    ///
    /// A discarded job has no consumer left for its bytes, and draining it anyway is what keeps
    /// an unstoppable producer alive: a line that writes faster than it can be cancelled — a
    /// builtin, which runs inline on the runtime and cannot be signalled — is only ended by its
    /// own descriptors going away. Closing the read ends turns its next write into `EPIPE`, which
    /// is a real error the interpreter propagates, so the command finishes and the worker thread
    /// it occupied comes back.
    discarded: Arc<tokio::sync::watch::Sender<bool>>,
}

/// A handle on one open job.
///
/// Allocated when the job is *admitted*, before its terminal or pipes finish opening, so a caller
/// never has to wait for construction to have a handle. Cloning shares the job; it does not
/// duplicate its output, because a job's bytes are the mux's to pump and they reach exactly one
/// frontend. Dropping a handle stops nothing: the job is the mux's, and [`ShellMux::stop`] is how
/// one ends.
///
/// The handle is bound to one generation of one name. A mux that has since reused the name cannot
/// be reached through it.
#[derive(Clone, Debug)]
pub struct Spawned {
    /// The shared state.
    inner: Arc<SpawnedState>,
}

impl Spawned {
    /// The job's identity, which is also its principal.
    #[must_use]
    pub fn id(&self) -> &ShellId {
        &self.inner.id
    }

    /// The sandbox its commands run in.
    #[must_use]
    pub fn sandbox(&self) -> &Sandbox {
        &self.inner.sandbox
    }

    /// Which output streams this job can produce.
    ///
    /// A terminal job has exactly one, merged. A pipe job has two, independent. This is fixed at
    /// admission and never changes.
    #[must_use]
    pub fn output_channels(&self) -> &'static [OutputChannel] {
        if self.inner.terminal {
            &[OutputChannel::Terminal]
        } else {
            &[OutputChannel::Stdout, OutputChannel::Stderr]
        }
    }

    /// The command this job was opened for, if it was opened for one.
    ///
    /// Available the instant [`ShellMux::spawn`] returns: the reservation is allocated under the
    /// same lock that admits the job, so this never answers `None` merely because a scheduled
    /// launch has not run yet.
    #[must_use]
    pub fn initial_command(&self) -> Option<CommandHandle> {
        self.inner
            .initial
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether this job has already closed, without waiting.
    ///
    /// Authoritative and cheap. A reader that is about to register an observer on one of this
    /// job's streams needs it: a stream whose retained storage was already reclaimed would
    /// otherwise be recreated as an empty, never-ending one, and the observer would wait forever
    /// for bytes that can never arrive. The job's own closure watch is the only source of that
    /// answer which cannot age out.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !matches!(*self.inner.closed.borrow(), ClosedState::Pending)
    }

    /// Waits for this job to close.
    ///
    /// Resolves after every byte of every one of the job's streams has been delivered and its
    /// snapshot reclaimed — which is a different boundary from a command finishing, and later.
    ///
    /// # Errors
    ///
    /// Fails with [`WaitError::Shutdown`] when the mux was torn down first, and with
    /// [`WaitError::Aborted`] when the producer was lost unexpectedly.
    pub async fn wait_closed(&self) -> Result<Arc<JobEnd>, WaitError> {
        let mut closed = self.inner.closed.clone();
        loop {
            {
                let current = closed.borrow_and_update();
                match &*current {
                    ClosedState::Done(end) => {
                        let end = Arc::clone(end);
                        drop(current);
                        return Ok(end);
                    }
                    ClosedState::Shutdown => return Err(WaitError::Shutdown),
                    ClosedState::Pending => {}
                }
            }
            closed.changed().await.map_err(|_| WaitError::Aborted)?;
        }
    }

    /// A borrowed descriptor on this job's terminal master.
    ///
    /// For a server-private metadata probe — the terminal's name, its foreground process group —
    /// and nothing else. It is not an I/O handle: reading it would steal bytes from the mux's own
    /// pump, and writing it bypasses [`ShellMux::write_input`]'s ordering and closing checks.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`] for a pipe job and [`MuxError::JobNotReady`] before
    /// the job's streams are open.
    pub fn terminal_fd(&self) -> Result<BorrowedFd<'_>, MuxError> {
        match self.inner.endpoints.get() {
            Some(Endpoints::Terminal { master, .. }) => Ok(master.get_ref().as_fd()),
            Some(Endpoints::Pipes { .. }) => Err(MuxError::NotTerminal(self.inner.id.clone())),
            None if self.inner.terminal => Err(MuxError::JobNotReady(self.inner.id.clone())),
            None => Err(MuxError::NotTerminal(self.inner.id.clone())),
        }
    }

    /// The path of this job's terminal, when it has one and the kernel named it.
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

}

/// What a caller that must block is told when the command it asked about ends: the exit status, in
/// the shell's convention, and `-1` for a command that produced none.
///
/// One slot per job, and a legacy convenience: [`CommandHandle::wait`] is the cloneable form, it
/// carries the whole verdict rather than a status, and using it does not consume this slot.
///
/// Called once, from the task that ran the line, with no lock held. Dropped uncalled instead when
/// the session shuts down before the command ends.
pub type OnFinish = Box<dyn FnOnce(i32) + Send>;

/// The reservation one admitted command holds in its job's row.
struct ActiveCommand {
    /// This command's identity.
    id: CommandId,
    /// The line as submitted.
    text: Arc<str>,
    /// The verdict channel every [`CommandHandle`] on this command reads.
    verdict: Arc<tokio::sync::watch::Sender<CommandState>>,
    /// The single legacy callback, when one was registered.
    on_finish: Option<OnFinish>,
    /// Whether the job closes when this command ends.
    close_on_finish: bool,
    /// How many spawn records this job's executor held when the command was admitted: everything
    /// after it is this command's to signal.
    spawn_mark: usize,
    /// The managed context, once the command is actually running.
    context: Option<CommandContext>,
    /// Whether the line has been handed to the shell yet.
    running: bool,
}

/// The streams and shell one job owns for as long as it exists.
///
/// The field order is the drop order and is load-bearing: the producer descriptors close before
/// `shell` drops (whose own drop discards whatever the job left and reclaims its snapshot).
struct JobResources {
    /// Producer ends the mux holds only to close: the pseudoterminal slave, or the three pipe
    /// ends the shell was given. Closing them is what turns a retained reader into end of file.
    _producers: Vec<OwnedFd>,
    /// The job's own gated shell. Shared with the task running the current line.
    shell: Arc<crate::Shell>,
}

/// One job: a named sandbox, its snapshot, its streams, and whatever command is running in it.
struct Job {
    /// The handle a job table prints and `fg NAME` resolves.
    id: ShellId,
    /// The sandbox every command of this job runs in.
    sandbox: Sandbox,
    /// This job's attached executor: its snapshot, and the spawn records its lines leave.
    executor: crate::MarshExecutor,
    /// Terminal or pipes, with a terminal job's geometry resolved from the moment it is open.
    io: JobIo,
    /// The shared handle, allocated at admission.
    handle: Spawned,
    /// Where this job's shell currently stands, refreshed at every boundary.
    working_directory: PathBuf,
    /// The streams and shell, once they are built.
    resources: Option<JobResources>,
    /// The command admitted into it, if any.
    command: Option<ActiveCommand>,
    /// A command is being admitted or launched into it.
    starting: bool,
    /// Why this job is to close, or `None` while it is to stay.
    close: Option<JobCloseMode>,
    /// The release channel the job's lifecycle task waits on, taken at reclamation.
    release: Option<tokio::sync::oneshot::Sender<Arc<JobEnd>>>,
    /// The closure watch, so a failed construction can resolve it without a lifecycle task.
    closed: Arc<tokio::sync::watch::Sender<ClosedState>>,
}

impl Job {
    /// Whether this row has left public view: a forced job is gone the moment force is accepted,
    /// while its line, its verdict and its name remain this row's until teardown.
    const fn retired(&self) -> bool {
        matches!(self.close, Some(JobCloseMode::Force))
    }

    /// Whether this row is the generation `sandbox` names.
    fn is(&self, sandbox: &Sandbox) -> bool {
        self.sandbox.uid == sandbox.uid
    }
}

/// What a launch decided under the job table's lock, so the awaits that follow happen outside it.
enum Launched {
    /// The line is to run in this shell, with this context.
    Run(Arc<crate::Shell>, CommandContext),
    /// A forced stop arrived while the job was still starting: the line never runs.
    Retired,
}

/// The job table, the name series it draws from, the selected job, the unfinished command
/// registry, and the default geometry new terminal jobs open at.
pub(crate) struct JobTable {
    /// The open jobs, in creation order.
    open: Vec<Job>,
    /// Next automatic job name.
    counter: u64,
    /// The job a front-end is looking at, cleared when that job closes.
    current: Option<ShellId>,
    /// The size a terminal job opens at when it asks for no size of its own.
    default_geometry: TerminalGeometry,
    /// Every admitted command that has not resolved yet, including the ones whose job has already
    /// left public view. A late observer can therefore find a command it has only heard about.
    commands: HashMap<CommandId, CommandHandle>,
    /// Set once shutdown begins; no further work is admitted.
    closing: bool,
}

impl JobTable {
    /// An empty table whose first automatic name is `1` and whose jobs default to `rows` × `cols`.
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            open: Vec::new(),
            counter: 1,
            current: None,
            default_geometry: TerminalGeometry { rows, cols },
            commands: HashMap::new(),
            closing: false,
        }
    }

    /// The next automatic job name, skipping any a job already occupies.
    ///
    /// Monotonic within a session — a name is never reused while the mux lives — because a job
    /// name is a principal, and reusing one would make two sandboxes indistinguishable in the
    /// history.
    fn next_id(&mut self) -> ShellId {
        loop {
            let id = ShellId::from(self.counter.to_string());
            self.counter += 1;
            if !self.open.iter().any(|job| job.id == id) {
                return id;
            }
        }
    }

    /// The row for `id`, retired ones included.
    fn find(&self, id: &ShellId) -> Option<&Job> {
        self.open.iter().find(|job| &job.id == id)
    }


    /// The row one handle names, or why it is not reachable.
    ///
    /// The whole identity check, in the one place every mutation goes through: the handle's mux,
    /// then its name, then its generation. Never split across two acquisitions of this lock.
    fn resolve(&self, job: &Spawned, mux: &Arc<ShellMux>) -> Result<&Job, MuxError> {
        if !job
            .inner
            .origin
            .upgrade()
            .is_some_and(|origin| Arc::ptr_eq(&origin, mux))
        {
            return Err(MuxError::ForeignJob(job.inner.id.clone()));
        }
        let row = self
            .find(&job.inner.id)
            .ok_or_else(|| MuxError::StaleJob(job.inner.id.clone()))?;
        if !row.is(&job.inner.sandbox) {
            return Err(MuxError::StaleJob(job.inner.id.clone()));
        }
        Ok(row)
    }

    /// The mutable row one handle names, with the same checks as [`Self::resolve`].
    fn resolve_mut(&mut self, job: &Spawned, mux: &Arc<ShellMux>) -> Result<&mut Job, MuxError> {
        if !job
            .inner
            .origin
            .upgrade()
            .is_some_and(|origin| Arc::ptr_eq(&origin, mux))
        {
            return Err(MuxError::ForeignJob(job.inner.id.clone()));
        }
        let id = job.inner.id.clone();
        let uid = job.inner.sandbox.uid.clone();
        let row = self
            .open
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or_else(|| MuxError::StaleJob(id.clone()))?;
        if row.sandbox.uid != uid {
            return Err(MuxError::StaleJob(id));
        }
        Ok(row)
    }

    /// Removes a job that is to close and has nothing left in flight, returning the whole row.
    ///
    /// A job with a command admitted is kept whatever its mode: the snapshot is what that command
    /// is running in, and its line is not concluded yet.
    ///
    /// The whole row, not only its resources: the row also holds the job's attached executor, and
    /// dropping that may be what releases the last handle on the job's snapshot — blocking work
    /// that must not happen under this lock.
    fn take_closable(&mut self, id: &ShellId) -> Option<Job> {
        let index = self.open.iter().position(|job| {
            &job.id == id && job.close.is_some() && job.command.is_none() && !job.starting
        })?;
        let job = self.open.remove(index);
        if self.current.as_ref() == Some(id) {
            self.current = None;
        }
        Some(job)
    }
}

/// The command in flight in a job.
#[derive(Clone, Debug)]
pub struct RunningView {
    /// The command line as submitted.
    pub cmd: String,
    /// Its identity, so a view can be correlated with a receipt.
    pub id: CommandId,
}

/// One job as a caller sees it.
///
/// A view rather than the row itself, because the shell a job runs its lines in, and the streams
/// it owns, are the mux's and must not leave the table.
#[derive(Clone, Debug)]
pub struct JobView {
    /// The job's identity, which is also its principal.
    pub id: ShellId,
    /// The sandbox its commands run in.
    pub sandbox: Sandbox,
    /// Terminal or pipes. A terminal view always carries a resolved geometry.
    pub io: JobIo,
    /// Where its shell currently stands, as of the last boundary.
    pub working_directory: PathBuf,
    /// The root of its own snapshot, when it has one.
    pub snapshot_root: Option<PathBuf>,
    /// The command in flight, or `None` when the job is idle.
    pub running: Option<RunningView>,
    /// A command is being admitted or launched into it, so it is neither idle nor yet running.
    pub starting: bool,
    /// A stop has been accepted, so it will close and takes no new command.
    pub closing: bool,
}

/// One consistent look at everything a mux holds.
///
/// Taken under a single acquisition of the job table's lock, so the jobs, the selection, the
/// default geometry and the unfinished commands are all the same instant. A frontend that read
/// them one at a time could otherwise see a command whose job it has not heard of, or a selection
/// naming a job that is no longer in its list.
#[derive(Clone, Debug)]
pub struct MuxSnapshot {
    /// Every visible job, in creation order.
    pub jobs: Vec<JobView>,
    /// Every admitted command that has not resolved, including those whose job has retired.
    pub commands: Vec<CommandHandle>,
    /// The selected job, if one is selected and still visible.
    pub current: Option<ShellId>,
    /// The size a terminal job opens at when it asks for none.
    pub default_geometry: TerminalGeometry,
}

/// The background work one mux owns: every launch, one lifecycle task per job, one per line.
pub(crate) struct Background {
    /// Handles joined by [`ShellMux::shutdown`].
    handles: Vec<tokio::task::JoinHandle<()>>,
    /// Tasks that end by themselves: a job's lifecycle, a line's run.
    detached: tokio::task::JoinSet<()>,
}

impl Background {
    /// A set with nothing started yet.
    pub(crate) fn new() -> Self {
        Self {
            handles: Vec::new(),
            detached: tokio::task::JoinSet::new(),
        }
    }

    /// Registers a task that ends by itself, so [`ShellMux::shutdown`] cancels whatever is left.
    ///
    /// `runtime` is the mux's own, never the caller's: the task outlives the call that created it,
    /// and a job's lifecycle or a running line placed on a caller's throwaway executor would be
    /// cancelled the moment that executor went away.
    ///
    /// The tasks that have already finished are taken first: a set holds a finished task's record
    /// until someone joins it, so a long session that opened and closed many jobs would otherwise
    /// accumulate one record per job for its whole life.
    fn spawn_detached(
        &mut self,
        runtime: &tokio::runtime::Handle,
        task: impl Future<Output = ()> + Send + 'static,
    ) {
        while self.detached.try_join_next().is_some() {}
        self.detached.spawn_on(task, runtime);
    }
}

impl ShellMux {
    /// The job table, recovering a poisoned lock like the rest of this module.
    pub(crate) fn job_table(&self) -> MutexGuard<'_, JobTable> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The background task set, with the same poisoning recovery.
    fn background(&self) -> MutexGuard<'_, Background> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a task this mux owns, so [`Self::shutdown`] joins it.
    fn own_task(&self, handle: tokio::task::JoinHandle<()>) {
        self.background().handles.push(handle);
    }

    /// The next command identity.
    fn next_command_id(&self) -> CommandId {
        CommandId(self.command_counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Opens a job over `dir`, gives it a snapshot and its streams, and reserves it for `cmd` when
    /// one is given.
    ///
    /// `id` is `None` for the next number in the `1`, `2`, … series. A name a live job already
    /// holds is refused, because a job name is a capability principal and two sandboxes sharing
    /// one would be indistinguishable in the history. `dir` is seed-relative.
    ///
    /// `cmd` is the command the job is being opened *for*. Its receipt is allocated here, under
    /// the same lock that admits the job, so [`Spawned::initial_command`] answers the instant this
    /// returns. The line itself is launched by a task this mux owns, so a front-end's own prompt
    /// is never held for it.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::ShuttingDown`] once teardown has begun, with [`MuxError::JobExists`]
    /// when `id` is taken, with [`MuxError::SandboxDir`] when `dir` escapes the seed or names
    /// nothing in it, with [`MuxError::InvalidTerminalSize`] for a zero dimension, and with
    /// whatever the snapshot, the streams or the shell reported. A failed construction closes
    /// every descriptor it opened and releases the name.
    pub async fn spawn(
        self: &Arc<Self>,
        dir: &str,
        id: Option<ShellId>,
        cmd: Option<&str>,
        options: SpawnOptions,
    ) -> Result<Spawned, MuxError> {
        let anonymous = id.is_none();
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

        let (handle, sandbox, io, reservation) = {
            let mut table = self.job_table();
            if table.closing {
                return Err(MuxError::ShuttingDown);
            }
            // Refused at *admission*, not only at the boundary. A session whose approved
            // publication failed may have a seed that is neither its old state nor its new one,
            // and a command admitted against that would run — spawning processes, writing files,
            // reaching the network — before its own gate eventually refused it. Every job over
            // this session refuses, not only the one that failed.
            if self.executor_recovery_required() {
                return Err(MuxError::RecoveryRequired);
            }
            let id = match id {
                Some(id) => {
                    if table.find(&id).is_some() {
                        return Err(MuxError::JobExists(id));
                    }
                    id
                }
                None => table.next_id(),
            };
            let (sandbox, executor) = self.new_sandbox(&id, dir)?;
            let io = match options.io {
                JobIo::Terminal { geometry } => JobIo::Terminal {
                    geometry: Some(geometry.unwrap_or(table.default_geometry)),
                },
                JobIo::Pipes => JobIo::Pipes,
            };

            let (closed_tx, closed_rx) = tokio::sync::watch::channel(ClosedState::Pending);
            let handle = Spawned {
                inner: Arc::new(SpawnedState {
                    id: id.clone(),
                    sandbox: sandbox.clone(),
                    terminal,
                    origin: Arc::downgrade(self),
                    endpoints: OnceLock::new(),
                    initial: Mutex::new(None),
                    closed: closed_rx,
                    discarded: Arc::new(tokio::sync::watch::channel(false).0),
                }),
            };

            // The reservation is allocated here, not by the launcher: a caller that inspects the
            // handle the instant this returns must already find it, and the background task that
            // eventually runs the line must consume this one rather than allocate a second.
            let reservation = cmd.map(|cmd| {
                let (command, active) = self.reserve(&sandbox, cmd, CommandOptions::default(), 0);
                *handle
                    .inner
                    .initial
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(command.clone());
                table.commands.insert(command.id, command.clone());
                (command, active)
            });

            let working_directory = executor
                .snapshot_root()
                .map_or_else(PathBuf::new, |root| root.join(sandbox.dir.as_str()));
            table.open.push(Job {
                id: id.clone(),
                sandbox: sandbox.clone(),
                executor,
                io,
                handle: handle.clone(),
                working_directory,
                resources: None,
                command: reservation.as_ref().map(|(_, active)| active.clone_slot()),
                starting: cmd.is_some(),
                close: match (anonymous, cmd.is_some(), terminal) {
                    // A pipe job is one-shot: it closes after the command it runs. But only
                    // *after* one — a job opened idle so a caller can arm its output receivers
                    // before submitting is the whole shape `execute` is built from, and marking
                    // it closing here would refuse the very command it was opened to run.
                    (_, true, false) => Some(JobCloseMode::Graceful),
                    (_, false, false) => None,
                    // An unnamed terminal job with a background command reclaims itself, unless a
                    // reader takes an interest in it.
                    (true, true, true) => Some(JobCloseMode::Automatic),
                    (_, _, true) => None,
                },
                release: None,
                closed: Arc::new(closed_tx),
            });
            drop(table);
            (
                handle,
                sandbox,
                io,
                reservation.map(|(command, _)| command),
            )
        };
        self.announce(FrontendEvent::Changed);
        if let Some(command) = &reservation {
            self.announce(FrontendEvent::CommandAccepted { command });
        }

        if let Err(error) = self.build_resources(&handle, &sandbox, io, options).await {
            // Shared rather than reported three times: the caller, the reserved command's waiters
            // and the job's own end all need this failure, and none of the errors it wraps clone.
            let shared = Arc::new(error);
            self.fail_construction(&handle, Arc::clone(&shared)).await;
            return Err(MuxError::Shared(shared));
        }

        if let Some(cmd) = cmd {
            let mux = Arc::clone(self);
            let launch = handle.clone();
            let cmd = cmd.to_string();
            // On this mux's runtime rather than the caller's: the launch outlives this call, and
            // the caller may be a status thread or a foreign runtime that is about to go away.
            let task = self.runtime.spawn(async move {
                if let Err(error) = mux.launch_into(&launch, &cmd).await {
                    mux.report_launch_failure(&launch, error).await;
                }
            });
            self.own_task(task);
        }
        Ok(handle)
    }

    /// Allocates one command's identity, text and verdict channel.
    ///
    /// Returns the public receipt and the row's private half. Both halves exist before anything
    /// can run, which is what makes a completion impossible to steal: the identity the verdict
    /// resolves is captured here, not read back off the row afterwards.
    fn reserve(
        &self,
        sandbox: &Sandbox,
        cmd: &str,
        options: CommandOptions,
        spawn_mark: usize,
    ) -> (CommandHandle, ActiveCommand) {
        let id = self.next_command_id();
        let text: Arc<str> = Arc::from(cmd);
        let (verdict, watch) = tokio::sync::watch::channel(CommandState::Pending);
        let verdict = Arc::new(verdict);
        (
            CommandHandle {
                id,
                shell: sandbox.clone(),
                text: Arc::clone(&text),
                state: watch,
            },
            ActiveCommand {
                id,
                text,
                verdict,
                on_finish: options.on_finish,
                close_on_finish: options.close_on_finish,
                spawn_mark,
                context: None,
                running: false,
            },
        )
    }

    /// Builds one job's streams and shell, publishes them, and starts its lifecycle task.
    async fn build_resources(
        self: &Arc<Self>,
        handle: &Spawned,
        sandbox: &Sandbox,
        io: JobIo,
        options: SpawnOptions,
    ) -> Result<(), MuxError> {
        let executor = {
            let table = self.job_table();
            let row = table.resolve(handle, self)?;
            let executor = row.executor.clone();
            drop(table);
            executor
        };

        let (fds, producers, endpoints, readers) = match io {
            JobIo::Terminal { geometry } => {
                let geometry = geometry.unwrap_or(TerminalGeometry { rows: 24, cols: 80 });
                let (master, slave) = crate::shellmux::pty::open_pty(geometry.rows, geometry.cols)?;
                let shell_slave = slave.try_clone()?;
                let file: brush_core::openfiles::OpenFile =
                    std::fs::File::from(shell_slave).into();
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
                        (OutputChannel::Stdout, Reader::Pipe(AsyncFd::new(pipes.stdout)?)),
                        (OutputChannel::Stderr, Reader::Pipe(AsyncFd::new(pipes.stderr)?)),
                    ],
                )
            }
        };

        let shell = self
            .build_shell(&executor, sandbox, fds, options.environment)
            .await?;
        // Mux-owned: every command it runs concludes explicitly, so its final boundary must
        // discard rather than publish whatever an abort left.
        shell.discard_on_drop();

        let (release, released) = tokio::sync::oneshot::channel::<Arc<JobEnd>>();

        let (latest, closed) = {
            let mut table = self.job_table();
            let default = table.default_geometry;
            let row = table.resolve_mut(handle, self)?;
            row.resources = Some(JobResources {
                _producers: producers,
                shell,
            });
            row.release = Some(release);
            let latest = match row.io {
                JobIo::Terminal { geometry } => geometry.or(Some(default)),
                JobIo::Pipes => None,
            };
            let closed = Arc::clone(&row.closed);
            drop(table);
            (latest, closed)
        };
        // Endpoints are published before the resize and before `Opened`, so nothing that reads the
        // handle afterwards can find it half-built.
        let _ = handle.inner.endpoints.set(endpoints);

        // Outside the lock: the ioctl is a syscall on a descriptor nothing else may take away
        // while the row holds it, and a resize that landed during construction must not be lost.
        if let (Some(geometry), Some(master)) = (latest, handle.master()) {
            crate::shellmux::pty::resize_pty(master.get_ref().as_fd(), geometry.rows, geometry.cols)?;
        }
        self.launched.notify_waiters();

        self.announce(FrontendEvent::Opened(handle));
        // Publication is a state change, not only a new handle: the reservation announced
        // `Changed` while this row still had no resources.
        self.announce(FrontendEvent::Changed);

        let lifecycle = job_lifecycle(
            self.frontend(),
            sandbox.clone(),
            readers,
            released,
            closed,
            handle.inner.discarded.subscribe(),
            self.runtime.clone(),
        );
        self.background()
            .spawn_detached(&self.runtime, lifecycle);
        Ok(())
    }

    /// Tears down a job whose construction failed, resolving its handle's closure.
    ///
    /// No `Opened` was delivered and no byte of output exists, so the failure is reported as the
    /// job's end rather than as a command result or fabricated output.
    async fn fail_construction(self: &Arc<Self>, handle: &Spawned, error: Arc<MuxError>) {
        let removed = {
            let mut table = self.job_table();
            let index = table
                .open
                .iter()
                .position(|row| row.id == handle.inner.id && row.is(&handle.inner.sandbox));
            let removed = index.map(|index| table.open.remove(index));
            if table.current.as_ref() == Some(&handle.inner.id) {
                table.current = None;
            }
            if let Some(command) = removed.as_ref().and_then(|row| row.command.as_ref()) {
                table.commands.remove(&command.id);
            }
            drop(table);
            removed
        };

        let end = Arc::new(JobEnd {
            shell: handle.inner.sandbox.clone(),
            close_mode: Some(JobCloseMode::Force),
            completion: None,
            error: Some(Arc::clone(&error)),
            recovery_required: self.executor_recovery_required(),
        });

        if let Some(row) = removed {
            // A reserved initial command that will now never run still owes its waiters an answer.
            if let Some(command) = &row.command {
                let completion = Arc::new(CommandCompletion {
                    id: command.id,
                    shell: row.sandbox.clone(),
                    command: Arc::clone(&command.text),
                    exit_code: None,
                    outcome: Arc::new(Err(MuxError::Shared(Arc::clone(&error)))),
                });
                command
                    .verdict
                    .send_replace(CommandState::Done(Arc::clone(&completion)));
                self.announce(FrontendEvent::Finished {
                    completion: &completion,
                });
            }
            let closed = Arc::clone(&row.closed);
            // The row off the lock and onto a blocking worker: it holds the job's snapshot, and
            // nothing else does once the failed construction is gone. This mux's blocking pool,
            // not the caller's: reclaiming a snapshot is filesystem work that must finish, and a
            // caller's executor may be gone before it has.
            let _ = self.runtime.spawn_blocking(move || drop(row)).await;
            closed.send_replace(ClosedState::Done(Arc::clone(&end)));
        }
        self.announce(FrontendEvent::Closed { end: &end });
        self.launched.notify_waiters();
        self.announce(FrontendEvent::Changed);
    }

    /// Starts `cmd` in `job`, returning its receipt.
    ///
    /// Returns once the line has been admitted and handed to the job's shell. Its output reaches
    /// the frontend on its own; its verdict reaches every holder of the returned handle, and every
    /// frontend as [`FrontendEvent::Finished`].
    ///
    /// An outstanding idle-terminal lease is revoked first and waited out **outside** every lock,
    /// so a prompt that is itself calling this cannot deadlock on its own acknowledgement.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::ShuttingDown`], [`MuxError::StaleJob`], [`MuxError::ForeignJob`],
    /// [`MuxError::JobClosing`], [`MuxError::JobBusy`] or [`MuxError::JobNotReady`].
    pub async fn start_in(
        self: &Arc<Self>,
        job: &Spawned,
        cmd: &str,
        options: CommandOptions,
    ) -> Result<CommandHandle, MuxError> {
        {
            let mut table = self.job_table();
            if table.closing {
                return Err(MuxError::ShuttingDown);
            }
            // As in `spawn`: a line admitted against a half-applied seed would have real effects
            // before its gate could refuse it.
            if self.executor_recovery_required() {
                return Err(MuxError::RecoveryRequired);
            }
            let row = table.resolve_mut(job, self)?;
            if row.close.is_some_and(JobCloseMode::explicit) {
                return Err(MuxError::JobClosing(row.id.clone()));
            }
            if row.command.is_some() || row.starting {
                return Err(MuxError::JobBusy(row.id.clone()));
            }
            if row.resources.is_none() {
                return Err(MuxError::JobNotReady(row.id.clone()));
            }
            row.starting = true;
            drop(table);
        }
        self.announce(FrontendEvent::Changed);

        // Outside every lock. The prompt that owns the lease may be the caller of this very
        // method, and it releases only after its own bookkeeping. `starting` is already set, so
        // no further lease can be granted while this waits.
        if let Some(state) = job.lease() {
            state.revoke(Revocation::Run);
            // The restoration *result*, not an acknowledgement. A lease that could not put the
            // terminal back left it in the prompt's raw mode: echo off, every key unprocessed,
            // every special character disabled. Launching a command into that would start a
            // program in a mode nobody configured and which it has no way to discover. So the
            // job is closed instead — the one outcome that leaves neither a permanently busy
            // terminal nor a program running blind.
            if let Err(error) = state.settled().await {
                let closed = {
                    let mut table = self.job_table();
                    if let Ok(row) = table.resolve_mut(job, self) {
                        row.starting = false;
                        row.close = Some(JobCloseMode::Force);
                    }
                    let closed = table.take_closable(&job.inner.id);
                    drop(table);
                    closed
                };
                if let Some(closed) = closed {
                    self.reclaim(closed, None).await;
                }
                self.announce(FrontendEvent::Changed);
                return Err(MuxError::Shared(error));
            }
        }

        let command = {
            let mut table = self.job_table();
            let (mark, piped) = {
                let row = table.resolve(job, self)?;
                (row.executor.spawn_records().len(), !row.io.is_terminal())
            };
            // A pipe job is one-shot whatever the caller asked for: it has no prompt to return to
            // and no terminal to keep, so a second line would run in a job with nothing left to
            // carry its output. Recorded here, with the reservation, so the job is already marked
            // closing the instant the command's verdict releases its running slot.
            let options = CommandOptions {
                close_on_finish: options.close_on_finish || piped,
                ..options
            };
            let (command, active) = self.reserve(&job.inner.sandbox, cmd, options, mark);
            table.commands.insert(command.id, command.clone());
            let row = table.resolve_mut(job, self)?;
            row.command = Some(active);
            drop(table);
            command
        };
        self.announce(FrontendEvent::CommandAccepted { command: &command });

        self.launch_into(job, cmd).await?;
        Ok(command)
    }

    /// Hands the reserved command to the job's shell.
    async fn launch_into(self: &Arc<Self>, job: &Spawned, cmd: &str) -> Result<(), MuxError> {
        let launched = self.launch_command(job, cmd).await;
        self.launched.notify_waiters();
        launched
    }

    /// The body of a launch, without the completion notification its callers owe.
    ///
    /// A forced stop accepted while the job was still `starting` is carried out here, under the
    /// same table lock that would have published the command: the line simply never runs, which is
    /// the in-process form of killing a just-launched group.
    async fn launch_command(self: &Arc<Self>, job: &Spawned, cmd: &str) -> Result<(), MuxError> {
        let context_seed = {
            let table = self.job_table();
            let row = table.resolve(job, self)?;
            let seed = (
                row.executor.seed().map(std::path::Path::to_path_buf),
                row.executor.snapshot_root().map(std::path::Path::to_path_buf),
            );
            drop(table);
            seed
        };

        let launched = {
            let mut table = self.job_table();
            let row = table.resolve_mut(job, self)?;
            let Some(resources) = row.resources.as_ref() else {
                return Err(MuxError::JobNotReady(row.id.clone()));
            };
            let shell = Arc::clone(&resources.shell);
            row.starting = false;

            let Some(active) = row.command.as_mut() else {
                // Nothing reserved: the caller's own reservation is gone, which only a teardown
                // between the two sections can do.
                drop(table);
                return Err(MuxError::StaleJob(job.inner.id.clone()));
            };
            if row.close == Some(JobCloseMode::Force) {
                drop(shell);
                drop(table);
                Launched::Retired
            } else {
                let context = CommandContext::new(
                    active.id,
                    job.inner.sandbox.clone(),
                    context_seed.0,
                    context_seed.1,
                    self.runtime.clone(),
                );
                active.context = Some(context.clone());
                active.running = true;
                active.spawn_mark = row.executor.spawn_records().len();
                drop(table);
                Launched::Run(shell, context)
            }
        };

        match launched {
            Launched::Retired => {
                // The stop that retired this row could not reclaim it: a row with a reserved
                // command and a pending launch is deliberately not closable, because its snapshot
                // is what that command would have run in. Reclaiming it is therefore owed here,
                // by the launch that decided the line never runs — exactly as `run_command` owes
                // it after a line that did.
                let completion = self
                    .conclude_command(job, None, Ok(Outcome::Discarded))
                    .await;
                let closed = self.job_table().take_closable(&job.inner.id);
                if let Some(closed) = closed {
                    self.reclaim(closed, completion).await;
                }
                self.announce(FrontendEvent::Changed);
            }
            Launched::Run(shell, context) => {
                self.announce(FrontendEvent::Changed);
                let task = run_command(
                    Arc::clone(self),
                    job.clone(),
                    shell,
                    cmd.to_string(),
                    context,
                );
                self.background().spawn_detached(&self.runtime, task);
            }
        }
        Ok(())
    }

    /// Reports a failed launch, and closes the job it opened.
    async fn report_launch_failure(self: &Arc<Self>, job: &Spawned, error: MuxError) {
        self.conclude_command(job, None, Err(error)).await;
        let closed = {
            let mut table = self.job_table();
            if let Ok(row) = table.resolve_mut(job, self) {
                row.close = Some(JobCloseMode::Graceful);
            }
            let closed = table.take_closable(&job.inner.id);
            drop(table);
            closed
        };
        if let Some(closed) = closed {
            self.reclaim(closed, None).await;
        }
    }

    /// Resolves the admitted command's receipt, delivers it, and records the job's closure.
    ///
    /// The whole handoff happens in one acquisition of the table lock: the command's identity, its
    /// text, its callback, its sandbox and its closure decision are captured *before* the running
    /// slot is released, so a second command admitted the instant afterwards cannot inherit any of
    /// them. Nothing is read back off the row later.
    async fn conclude_command(
        self: &Arc<Self>,
        job: &Spawned,
        exit_code: Option<i32>,
        outcome: Result<Outcome, MuxError>,
    ) -> Option<Arc<CommandCompletion>> {
        let captured = {
            let mut table = self.job_table();
            let Ok(row) = table.resolve_mut(job, self) else {
                return None;
            };
            let sandbox = row.sandbox.clone();
            let active = row.command.take()?;
            if active.close_on_finish && row.close.is_none() {
                row.close = Some(JobCloseMode::Graceful);
            }
            table.commands.remove(&active.id);
            drop(table);
            (sandbox, active)
        };
        let (sandbox, active) = captured;

        let completion = Arc::new(CommandCompletion {
            id: active.id,
            shell: sandbox,
            command: Arc::clone(&active.text),
            exit_code,
            outcome: Arc::new(outcome),
        });
        active
            .verdict
            .send_replace(CommandState::Done(Arc::clone(&completion)));

        // Outside the lock, like every other callback delivery.
        self.announce(FrontendEvent::Finished {
            completion: &completion,
        });
        if let Some(done) = active.on_finish {
            done(completion.legacy_status());
        }
        Some(completion)
    }

    /// One consistent look at every job, every unfinished command and the selection.
    #[must_use]
    pub fn snapshot(&self) -> MuxSnapshot {
        let table = self.job_table();
        let jobs = table
            .open
            .iter()
            .filter(|job| !job.retired())
            .map(job_view)
            .collect();
        let mut commands: Vec<CommandHandle> = table.commands.values().cloned().collect();
        commands.sort_by_key(CommandHandle::id);
        let snapshot = MuxSnapshot {
            jobs,
            commands,
            current: table.current.clone(),
            default_geometry: table.default_geometry,
        };
        drop(table);
        snapshot
    }

    /// Every open job, in creation order. A forced job is not one: it left public view when its
    /// stop was accepted.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobView> {
        self.job_table()
            .open
            .iter()
            .filter(|job| !job.retired())
            .map(job_view)
            .collect()
    }

    /// One job, by identity, excluding a forced one for the reason [`Self::jobs`] does.
    #[must_use]
    pub fn job(&self, id: &ShellId) -> Option<JobView> {
        self.job_table()
            .open
            .iter()
            .find(|job| &job.id == id && !job.retired())
            .map(job_view)
    }

    /// The job a front-end has selected, if it is still open.
    #[must_use]
    pub fn current_job(&self) -> Option<JobView> {
        let table = self.job_table();
        let current = table.current.clone()?;
        let view = table
            .open
            .iter()
            .find(|job| job.id == current && !job.retired())
            .map(job_view);
        drop(table);
        view
    }

    /// A handle on the job named `id`, which may still be constructing.
    #[must_use]
    pub fn handle(&self, id: &ShellId) -> Option<Spawned> {
        self.job_table()
            .find(id)
            .filter(|job| !job.retired())
            .map(|job| job.handle.clone())
    }

    /// Keeps a job a reader has taken an interest in: it will not close itself any more.
    ///
    /// Only the automatic closure of an unnamed job with a background command is cancelled. An
    /// explicit stop, and a one-shot command's own `close_on_finish`, are decisions this may not
    /// quietly revoke; a pipe job cannot be retained at all, because it has no prompt to return
    /// to.
    ///
    /// # Errors
    ///
    /// Fails for a stale or foreign handle.
    pub fn keep(&self, job: &Spawned) -> Result<bool, MuxError> {
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        let mut table = self.job_table();
        let row = table.resolve_mut(job, &mux)?;
        let kept = if row.io.is_terminal() && row.close == Some(JobCloseMode::Automatic) {
            row.close = None;
            true
        } else {
            false
        };
        drop(table);
        if kept {
            self.announce(FrontendEvent::Changed);
        }
        Ok(kept)
    }

    /// Asks to be told when the command now running in `job` ends.
    ///
    /// `false` when nothing is running, or when a caller already occupies the slot. The cloneable
    /// alternative is [`CommandHandle::wait`], which has no single-slot limit.
    ///
    /// # Errors
    ///
    /// Fails for a stale or foreign handle.
    pub fn on_finish(&self, job: &Spawned, done: OnFinish) -> Result<bool, MuxError> {
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        let mut table = self.job_table();
        let row = table.resolve_mut(job, &mux)?;
        let registered = match row.command.as_mut() {
            Some(active) if active.running && active.on_finish.is_none() => {
                active.on_finish = Some(done);
                true
            }
            _ => false,
        };
        drop(table);
        Ok(registered)
    }

    /// Selects `job` as the one a front-end is looking at, waiting for a launch already in flight.
    ///
    /// Terminal jobs only: a pipe job has nothing to display.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`] for a pipe job, [`MuxError::JobClosing`] when an
    /// accepted stop has already closed it, and for a stale or foreign handle.
    pub async fn switch(self: &Arc<Self>, job: &Spawned) -> Result<JobView, MuxError> {
        {
            let table = self.job_table();
            let row = table.resolve(job, self)?;
            if !row.io.is_terminal() {
                return Err(MuxError::NotTerminal(row.id.clone()));
            }
            if row.retired() || row.close.is_some_and(JobCloseMode::explicit) {
                return Err(MuxError::JobClosing(row.id.clone()));
            }
            drop(table);
        }
        // A reader who brought this job up means to look at it, so it is no longer one the series
        // can reclaim on its own.
        let _ = self.keep(job)?;
        self.await_launch(&job.inner.id).await;
        let mut table = self.job_table();
        let row = table.resolve(job, self)?;
        if row.retired() {
            return Err(MuxError::StaleJob(job.inner.id.clone()));
        }
        let view = job_view(row);
        table.current = Some(job.inner.id.clone());
        drop(table);
        self.announce(FrontendEvent::Changed);
        Ok(view)
    }

    /// Waits until nothing is being launched into job `id`.
    async fn await_launch(&self, id: &ShellId) {
        loop {
            // Registered before the check: a publication that lands between them is not a lost
            // wakeup.
            let notified = self.launched.notified();
            {
                let table = self.job_table();
                let pending = table
                    .find(id)
                    .is_some_and(|job| job.starting || job.resources.is_none());
                drop(table);
                if !pending {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Accepts a stop for `job`, gracefully or by force.
    ///
    /// Graceful sends no signal: it records that the job closes once its command is over, and a
    /// repeated request is harmless. Force kills every process the running command spawned, asks
    /// its native workers to stop, retires the row at once, and discards the line rather than
    /// gating it.
    ///
    /// # Cancellation versus conclusion
    ///
    /// The decision is linearized under the job lock. A force accepted before finalization begins
    /// causes the line to be discarded. A force that arrives after an approved publication has
    /// begun cannot undo it: the eventual completion is authoritative, and nothing here promises
    /// to roll back effects that have already been published.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobTermination`] when a forced job's processes could not be
    /// signalled — in which case the row is left exactly as it was — and for a stale or foreign
    /// handle.
    pub async fn stop(self: &Arc<Self>, job: &Spawned, force: bool) -> Result<(), MuxError> {
        let (closed, context) = {
            let mut table = self.job_table();
            let row = table.resolve_mut(job, self)?;
            if row.retired() {
                return Err(MuxError::StaleJob(row.id.clone()));
            }
            let mut context = None;
            if force {
                // Before the row changes: a failed kill leaves the job as it was.
                if let Some(active) = row.command.as_ref().filter(|active| active.running) {
                    kill_since(&row.executor, active.spawn_mark, &row.id)?;
                    context = active.context.clone();
                }
                row.close = Some(JobCloseMode::Force);
            } else {
                row.close = Some(JobCloseMode::Graceful);
            }
            let closed = table.take_closable(&job.inner.id);
            drop(table);
            (closed, context)
        };
        if force {
            // Every other half of a forced stop only *asks*. `kill_since` reaches processes this
            // line started and a managed context is cooperative, so a line whose writer is a
            // builtin — running inline on this runtime, with no process and no poll point — is
            // reached by neither. Closing its output read ends is what actually ends it: the next
            // write fails instead of waiting for a drain the discard has already made pointless.
            job.inner.discarded.send_replace(true);
        }
        if let Some(context) = context {
            context.request_cancellation();
        }
        // The revocation is unconditional — the job is going away whether or not a prompt is
        // reading it — and the wait is what keeps the terminal's mode from being restored into a
        // job that has already been reclaimed. Both are no-ops for a pipe job or an idle one.
        if let Some(state) = job.lease() {
            state.revoke(Revocation::Close);
            let _ = state.settled().await;
        }
        self.announce(FrontendEvent::Changed);
        if let Some(closed) = closed {
            self.reclaim(closed, None).await;
        }
        Ok(())
    }

    /// Sends `signal` to the processes the command now running in `job` started.
    ///
    /// Only that command's own process groups, tracked from the spawn mark taken when it was
    /// admitted: this is not an arbitrary-pid interface. Best-effort, for the reason the spawn log
    /// is append-only — a recorded pid may already be gone, which is not a failure. A
    /// builtin-only line has no process at all, and signalling one succeeds having sent nothing.
    ///
    /// Unlike a forced [`Self::stop`], this does not retire the job or discard its work: a command
    /// that catches the signal and finishes normally is still gated normally.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobTermination`] carrying the last errno that was not `ESRCH`, and
    /// for a stale or foreign handle.
    pub fn signal(&self, job: &Spawned, signal: crate::Signal) -> Result<(), MuxError> {
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        let table = self.job_table();
        let row = table.resolve(job, &mux)?;
        let Some(active) = row.command.as_ref().filter(|active| active.running) else {
            drop(table);
            return Ok(());
        };
        let outcome = signal_since(&row.executor, active.spawn_mark, &row.id, signal);
        drop(table);
        outcome
    }

    /// Applies `size` to one terminal job.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::InvalidTerminalSize`] for a zero dimension — changing nothing —
    /// [`MuxError::NotTerminal`] for a pipe job, and for a stale or foreign handle.
    pub async fn resize(
        self: &Arc<Self>,
        job: &Spawned,
        size: TerminalGeometry,
    ) -> Result<(), MuxError> {
        if !size.is_valid() {
            return Err(MuxError::InvalidTerminalSize {
                rows: size.rows,
                cols: size.cols,
            });
        }
        let sandbox = {
            let mut table = self.job_table();
            let row = table.resolve_mut(job, self)?;
            if !row.io.is_terminal() {
                return Err(MuxError::NotTerminal(row.id.clone()));
            }
            row.io = JobIo::Terminal {
                geometry: Some(size),
            };
            let sandbox = row.sandbox.clone();
            drop(table);
            sandbox
        };
        // The physical application is serialized through this job's own resize lock, and the
        // desired size is re-read inside it: two concurrent resizes must end with the terminal at
        // whichever one the table settled on, never at the older of the two.
        let guard = self.resize_lock.lock().await;
        let desired = {
            let table = self.job_table();
            let desired = table.resolve(job, self).ok().and_then(|row| match row.io {
                JobIo::Terminal { geometry } => geometry,
                JobIo::Pipes => None,
            });
            drop(table);
            desired
        };
        if let (Some(desired), Some(master)) = (desired, job.master()) {
            crate::shellmux::pty::resize_pty(
                master.get_ref().as_fd(),
                desired.rows,
                desired.cols,
            )?;
        }
        drop(guard);
        self.announce(FrontendEvent::Resized {
            shell: &sandbox,
            geometry: size,
        });
        Ok(())
    }

    /// Applies `size` to every live terminal job, and to every terminal job opened afterwards.
    ///
    /// Pipe jobs are skipped: they have no terminal. A repeated resize is reapplied, because a
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
            let mut table = self.job_table();
            table.default_geometry = size;
            let targets = table
                .open
                .iter_mut()
                .filter(|row| row.io.is_terminal())
                .filter_map(|row| {
                    row.io = JobIo::Terminal {
                        geometry: Some(size),
                    };
                    row.handle
                        .master()
                        .map(|master| (row.sandbox.clone(), Arc::clone(master)))
                })
                .collect();
            drop(table);
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

    /// Writes `bytes` to the job's standard input.
    ///
    /// For a terminal job these are keystrokes: the line discipline sees them, a program in raw
    /// mode gets them unchanged, and there is no end-of-file to be sent this way. For a pipe job
    /// they are bytes on a pipe, and [`Self::close_input`] is what ends it.
    ///
    /// Partial writes are retried, so the whole slice reaches the job or an error is reported. A
    /// failure partway through has already delivered a prefix.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobClosing`] when the job has been closed, [`MuxError::InputClosed`]
    /// when a pipe job's input has already been ended, [`MuxError::JobNotReady`] before its
    /// streams exist, and for a stale or foreign handle.
    pub async fn write_input(&self, job: &Spawned, bytes: &[u8]) -> Result<(), MuxError> {
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        {
            let table = self.job_table();
            let row = table.resolve(job, &mux)?;
            if row.close.is_some_and(JobCloseMode::explicit) {
                return Err(MuxError::JobClosing(row.id.clone()));
            }
            drop(table);
        }
        match job.inner.endpoints.get() {
            Some(Endpoints::Terminal { master, .. }) => write_fd(master, bytes).await,
            Some(Endpoints::Pipes { input }) => input.write_all(bytes).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    MuxError::InputClosed(job.inner.id.clone())
                } else {
                    MuxError::Io(error)
                }
            }),
            None => Err(MuxError::JobNotReady(job.inner.id.clone())),
        }
    }

    /// Ends a pipe job's standard input: a real end-of-file, after every write already accepted.
    ///
    /// Idempotent. It closes *this* job's input; a job whose name was later reused has a
    /// descriptor of its own and cannot be reached through this handle.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`]'s counterpart [`MuxError::NotPiped`] for a terminal
    /// job — a pseudoterminal has no half-close, and sending an end-of-transmission character
    /// instead would be a keystroke, not an end of file — and for a stale or foreign handle.
    pub async fn close_input(&self, job: &Spawned) -> Result<(), MuxError> {
        // The same two checks every other mutation makes, and for the same reason: this reaches a
        // retained endpoint that outlives the public row, so without them a handle from another
        // mux — or one whose job closed and whose name has since been reused — would quietly end
        // the input of a job its holder no longer names.
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        {
            let table = self.job_table();
            table.resolve(job, &mux)?;
            drop(table);
        }
        match job.inner.endpoints.get() {
            Some(Endpoints::Pipes { input }) => {
                input.close().await;
                Ok(())
            }
            Some(Endpoints::Terminal { .. }) => Err(MuxError::NotPiped(job.inner.id.clone())),
            None if job.inner.terminal => Err(MuxError::NotPiped(job.inner.id.clone())),
            None => Err(MuxError::JobNotReady(job.inner.id.clone())),
        }
    }

    /// Lends this terminal job's slave side while no command is running in it.
    ///
    /// The lease is how an interactive prompt reads *this pane's* keyboard without touching the
    /// process's own standard input. It is revoked the moment a command is admitted, and the
    /// admission waits for the acknowledgement outside every lock.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::NotTerminal`] for a pipe job, [`MuxError::TerminalBusy`] when a
    /// command is running or another lease is outstanding, [`MuxError::JobClosing`] once the job
    /// is closing, [`MuxError::JobNotReady`] before the terminal exists, [`MuxError::Shared`]
    /// when an earlier lease left the terminal in a mode it could not undo, and for a stale or
    /// foreign handle.
    pub async fn idle_terminal(
        self: &Arc<Self>,
        job: &Spawned,
    ) -> Result<IdleTerminal, MuxError> {
        let state = {
            let mut table = self.job_table();
            if table.closing {
                return Err(MuxError::ShuttingDown);
            }
            let row = table.resolve_mut(job, self)?;
            if !row.io.is_terminal() {
                return Err(MuxError::NotTerminal(row.id.clone()));
            }
            if row.command.is_some() || row.starting {
                return Err(MuxError::TerminalBusy(row.id.clone()));
            }
            if row.resources.is_none() {
                return Err(MuxError::JobNotReady(row.id.clone()));
            }
            let Some(state) = row.handle.lease().map(Arc::clone) else {
                return Err(MuxError::JobNotReady(row.id.clone()));
            };
            // Under the table lock, beside `command` and `starting`: those three are what make a
            // terminal unavailable, and checking them in two places would let an admission and a
            // grant each conclude the terminal was theirs.
            state.reserve().map_err(|refusal| match refusal {
                LeaseRefusal::Held => MuxError::TerminalBusy(row.id.clone()),
                LeaseRefusal::Closing => MuxError::JobClosing(row.id.clone()),
                LeaseRefusal::Unrestored(error) => MuxError::Shared(error),
            })?;
            drop(table);
            state
        };

        let Some(master) = job.master() else {
            return Err(MuxError::JobNotReady(job.inner.id.clone()));
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
    /// Parsed by the job's own shell, so its options — POSIX mode, extended globbing — are the
    /// ones that decide. Only an incomplete tokenization or an end-of-input parse failure answers
    /// `false`: any other syntax error is a real error, and the line should run so the shell
    /// itself produces the diagnostic the user expects.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::JobBusy`] while a command is running, [`MuxError::NotTerminal`] for
    /// a pipe job, [`MuxError::JobNotReady`] before the shell exists, and for a stale or foreign
    /// handle.
    pub async fn input_is_complete(&self, job: &Spawned, line: &str) -> Result<bool, MuxError> {
        let mux = job
            .inner
            .origin
            .upgrade()
            .ok_or_else(|| MuxError::ForeignJob(job.inner.id.clone()))?;
        let shell = {
            let table = self.job_table();
            let row = table.resolve(job, &mux)?;
            if !row.io.is_terminal() {
                return Err(MuxError::NotTerminal(row.id.clone()));
            }
            if row.command.is_some() || row.starting {
                return Err(MuxError::JobBusy(row.id.clone()));
            }
            let Some(resources) = row.resources.as_ref() else {
                return Err(MuxError::JobNotReady(row.id.clone()));
            };
            let shell = Arc::clone(&resources.shell);
            drop(table);
            shell
        };
        let guard = shell.shell_ref().lock().await;
        let parsed = guard.parse_string(line.to_string());
        drop(guard);
        Ok(match parsed {
            Ok(_) => true,
            Err(brush_core::parser::ParseError::ParsingAtEndOfInput)
            | Err(brush_core::parser::ParseError::Tokenizing { .. }) => false,
            Err(_) => true,
        })
    }

    /// Reclaims a closed job: its shell, its streams and its snapshot, then resolves its closure.
    ///
    /// Dropping the row *is* the reclamation, and all of it is blocking filesystem work, so it
    /// happens on a blocking worker of this mux's own runtime — a caller's executor may be gone
    /// before a snapshot is finished with. The [`JobEnd`] is sent only afterwards, which is what
    /// makes [`Spawned::wait_closed`] resolve after the snapshot is actually gone.
    async fn reclaim(self: &Arc<Self>, mut closed: Job, completion: Option<Arc<CommandCompletion>>) {
        let release = closed.release.take();
        let end = Arc::new(JobEnd {
            shell: closed.sandbox.clone(),
            close_mode: closed.close,
            completion,
            error: None,
            recovery_required: self.executor_recovery_required(),
        });
        let watch = Arc::clone(&closed.closed);
        let _ = self.runtime.spawn_blocking(move || drop(closed)).await;
        watch.send_replace(ClosedState::Done(Arc::clone(&end)));
        if let Some(release) = release {
            // The lifecycle task publishes `Closed` once every stream has ended, so a late chunk
            // is never delivered after the job's end.
            let _ = release.send(end);
        }
        self.announce(FrontendEvent::Changed);
    }

    /// Ends the session.
    ///
    /// Admission closes first, so nothing new is accepted while teardown runs. Then: every
    /// outstanding command's native workers are asked to stop and its processes killed, every
    /// waiting receipt and closure watch is resolved with [`WaitError::Shutdown`], owned tasks are
    /// joined, remaining lifecycle and line tasks are cancelled, every job is reclaimed, and the
    /// frontend is detached.
    ///
    /// Idempotent: a second call finds admission already closed and nothing left to reclaim.
    ///
    /// Startup owns persistent recovery, so nothing here sweeps snapshots or the write-ahead log.
    /// A snapshot whose *approved* publication failed is left on disk deliberately: it is the
    /// content the next [`MarshExecutor::open`](crate::MarshExecutor::open) replays from.
    ///
    /// # Errors
    ///
    /// Fails with the first termination failure observed while killing outstanding lines. Every
    /// job is still reclaimed.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), MuxError> {
        let mut failure = Ok(());
        let (abandoned, contexts) = {
            let mut table = self.job_table();
            table.closing = true;
            let mut abandoned = Vec::new();
            let mut contexts = Vec::new();
            for job in &mut table.open {
                if let Some(active) = job.command.as_mut() {
                    if active.running
                        && let Err(error) = kill_since(&job.executor, active.spawn_mark, &job.id)
                        && failure.is_ok()
                    {
                        failure = Err(error);
                    }
                    contexts.extend(active.context.clone());
                    abandoned.extend(active.on_finish.take());
                    active.verdict.send_replace(CommandState::Shutdown);
                }
                job.closed.send_replace(ClosedState::Shutdown);
            }
            table.commands.clear();
            drop(table);
            (abandoned, contexts)
        };
        // Outside the lock, like every other callback delivery.
        drop(abandoned);
        for context in &contexts {
            context.request_cancellation();
        }
        for context in &contexts {
            context.finish().await;
        }

        let handles = std::mem::take(&mut self.background().handles);
        for handle in handles {
            let _ = handle.await;
        }

        // Before the rows go: every running line's task holds an `Arc<crate::Shell>` clone, and
        // dropping the last one is blocking work that must not land on an async worker.
        let mut detached = std::mem::take(&mut self.background().detached);
        detached.shutdown().await;

        // Whole rows, not only their resources: a row holds the job's attached executor too, and
        // that is the other handle keeping its snapshot alive.
        let rows: Vec<Job> = {
            let mut table = self.job_table();
            let rows = std::mem::take(&mut table.open);
            table.current = None;
            drop(table);
            rows
        };
        let _ = self.runtime.spawn_blocking(move || {
            for row in &rows {
                if let (Some(active), Some(resources)) = (&row.command, &row.resources)
                    && active.running
                    && let Ok(mut guard) = resources.shell.shell_ref().try_lock()
                {
                    // The line's task was aborted above, so nothing holds the shell; a failure
                    // here has nowhere to go, and the shell's own drop then discards whatever the
                    // discard could not clear.
                    let _ = resources.shell.discard(&mut guard, &active.text);
                }
            }
            drop(rows);
        })
        .await;

        self.detach();
        failure
    }
}

impl ActiveCommand {
    /// The row's half of a reservation, for the constructor that builds both at once.
    fn clone_slot(&self) -> Self {
        Self {
            id: self.id,
            text: Arc::clone(&self.text),
            verdict: Arc::clone(&self.verdict),
            on_finish: None,
            close_on_finish: self.close_on_finish,
            spawn_mark: self.spawn_mark,
            context: None,
            running: false,
        }
    }
}

/// Runs one line in the job's own shell, then publishes or discards what the gate made of it.
///
/// The snapshot is refreshed, the line run inside its managed context, that context's native
/// workers joined, and — once every process of it has exited — the boundary decided: a line whose
/// job was retired by a forced stop while it ran is discarded unchecked; any other line is
/// concluded exactly as [`crate::Shell::run`] would have.
async fn run_command(
    mux: Arc<ShellMux>,
    job: Spawned,
    shell: Arc<crate::Shell>,
    cmd: String,
    context: CommandContext,
) {
    let ran = {
        let mut guard = shell.shell_ref().lock().await;
        let outcome = match shell.refresh(&mut guard) {
            Ok(()) => {
                let params = guard.default_exec_params();
                // Boxed for the same reason the shell build is: an interpreter run's state
                // machine is as deep as the script it is running, and storing it inline would
                // push that depth into every frame above it.
                Box::pin(with_context(context.clone(), async {
                    guard
                        .run_string(&cmd, &SourceInfo::default(), &params)
                        .await
                        .map_err(MarshError::from)
                }))
                .await
            }
            Err(error) => Err(error),
        };
        // Before the boundary, and with the shell still held: a native worker that outlived this
        // would be writing into a snapshot the discard below is about to reset.
        context.finish().await;

        outcome.and_then(|result| {
            let forced = {
                let table = mux.job_table();
                let forced = table
                    .find(job.id())
                    .is_some_and(|row| row.is(job.sandbox()) && row.retired());
                drop(table);
                forced
            };
            let boundary = if forced {
                shell.discard(&mut guard, &cmd)
            } else {
                shell.conclude(&mut guard, &cmd)
            };
            boundary.map(|boundary| (result, boundary))
        })
    };
    // Before the table: the reclamation below may drop the row's resources, and a last
    // `Shell::drop` does blocking I/O, so this clone must not be the one that dies on an async
    // worker.
    drop(shell);

    let (exit_code, outcome) = match ran {
        Ok((result, boundary)) => (
            Some(i32::from(u8::from(&result.exit_code))),
            Ok(boundary),
        ),
        Err(error) => (None, Err(MuxError::from(error))),
    };

    // Also refresh the recorded working directory, so a view taken after this line reports where
    // the shell actually stands rather than where it started.
    let completion = mux.conclude_command(&job, exit_code, outcome).await;
    let closed = mux.job_table().take_closable(job.id());
    if let Some(closed) = closed {
        mux.reclaim(closed, completion).await;
    }
    mux.announce(FrontendEvent::Changed);
}

/// One row as a caller sees it.
fn job_view(job: &Job) -> JobView {
    JobView {
        id: job.id.clone(),
        sandbox: job.sandbox.clone(),
        io: job.io,
        working_directory: job.working_directory.clone(),
        snapshot_root: job.executor.snapshot_root().map(std::path::Path::to_path_buf),
        running: job
            .command
            .as_ref()
            .filter(|active| active.running)
            .map(|active| RunningView {
                cmd: active.text.to_string(),
                id: active.id,
            }),
        starting: job.starting || job.resources.is_none(),
        closing: job.close.is_some_and(JobCloseMode::explicit),
    }
}

/// Refuses a terminal geometry with a zero dimension.
pub(crate) const fn validate_size(rows: u16, cols: u16) -> Result<(), MuxError> {
    if rows == 0 || cols == 0 {
        return Err(MuxError::InvalidTerminalSize { rows, cols });
    }
    Ok(())
}

/// Sends `signal` to every process group one command started since `mark`.
fn signal_since(
    executor: &crate::MarshExecutor,
    mark: usize,
    job: &ShellId,
    signal: crate::Signal,
) -> Result<(), MuxError> {
    let number = match signal {
        crate::Signal::Interrupt => libc::SIGINT,
        crate::Signal::Terminate => libc::SIGTERM,
        crate::Signal::Kill => libc::SIGKILL,
        crate::Signal::Hangup => libc::SIGHUP,
        crate::Signal::Continue => libc::SIGCONT,
    };
    let records = executor.spawn_records();
    let mut failure: Option<std::io::Error> = None;
    for record in records.get(mark..).unwrap_or_default() {
        let marsh_instrument::SpawnRecord::Spawned {
            pid: Some(pid), ..
        } = record
        else {
            continue;
        };
        let Ok(pid) = libc::pid_t::try_from(*pid) else {
            continue;
        };
        // SAFETY: `kill` signals a process group by the negation of its id and has no
        // memory-safety requirements.
        if unsafe { libc::kill(-pid, number) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                failure = Some(error);
            }
        }
    }
    match failure {
        None => Ok(()),
        Some(source) => Err(MuxError::JobTermination {
            job: job.clone(),
            source,
        }),
    }
}

/// One job's output stream, whichever kind it is.
enum Reader {
    /// A pseudoterminal master, shared with the handle that writes input to it.
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
        let mut guard = fd.writable().await.map_err(MuxError::Io)?;
        let attempt = guard.try_io(|inner| {
            let slice = &bytes[written..];
            // SAFETY: `write` receives an open descriptor, a valid pointer and the length of the
            // slice behind it.
            let count = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    slice.as_ptr().cast(),
                    slice.len(),
                )
            };
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(usize::try_from(count).unwrap_or(0))
        });
        match attempt {
            Ok(Ok(count)) => written += count,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(MuxError::Io(error)),
            // Not ready after all; the guard is cleared and the next await waits again.
            Err(_would_block) => {}
        }
    }
    Ok(())
}

/// Reads whatever a job's terminal has produced into `buffer`, returning how many bytes.
///
/// Bytes are preserved exactly: escape sequences, non-UTF-8 output and a final line with no
/// newline all arrive as they were written. `0` is end of file, which on Linux is how a
/// pseudoterminal reports that its last writer is gone.
async fn read_terminal(terminal: &AsyncFd<OwnedFd>, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = terminal.readable().await?;
        let attempt = guard.try_io(|inner| {
            // SAFETY: `read` receives an open descriptor, a valid writable pointer and the
            // length of the slice behind it.
            let count = unsafe {
                libc::read(
                    inner.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(usize::try_from(count).unwrap_or(0))
        });
        match attempt {
            Ok(Ok(count)) => return Ok(count),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // A pseudoterminal master whose slave has been closed answers `EIO`. That is a
            // hangup, not a failure: it is this stream's end of file.
            Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
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

/// Carries one job's output streams to the frontend, then announces the job's end.
///
/// Every stream of a *live* job is drained unconditionally, the jobs nobody is looking at
/// included: a pseudoterminal whose master nobody reads fills its buffer and stops the command
/// writing into it, and a pipe does the same.
///
/// A job a forced stop has retired is the one exception, and it has to be. Its work is discarded,
/// so its bytes have no consumer by definition — and draining them anyway is what keeps an
/// unstoppable producer running: a line whose writer is a *builtin* runs inline on this runtime,
/// so it has no process to kill and no cancellation point to reach, and a reader that keeps
/// politely emptying its pipe lets it write forever, occupying one worker thread for the rest of
/// the daemon's life. So `discarded` closes the read ends instead. The producer's next write
/// fails with `EPIPE`, which the interpreter propagates like any other I/O error, and the command
/// ends.
///
/// Nothing here holds a reference to the mux. The end of the job is the end of its resources: when
/// every reader is done, this waits for the release the row owned before announcing
/// [`FrontendEvent::Closed`], so a failed read alone never claims a live job closed.
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
    closed: Arc<tokio::sync::watch::Sender<ClosedState>>,
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
            // and the sender outlives this task inside the handle, so an `Err` here means the job
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

    // The row's own token, so this ends only once the job's resources are released — which the mux
    // does after the snapshot that job named is gone.
    let end = released.await.unwrap_or_else(|_| {
        Arc::new(JobEnd {
            shell: shell.clone(),
            close_mode: None,
            completion: None,
            error: None,
            recovery_required: false,
        })
    });
    closed.send_replace(ClosedState::Done(Arc::clone(&end)));
    let _ = notify(&frontend, FrontendEvent::Closed { end: &end });
}

/// Carries one stream to the frontend, honouring the receipts it returns.
///
/// A withheld receipt slows *this* stream and nothing else: the awaits happen outside every mux
/// lock and outside the frontend's own mutex, so another job's output, another channel and every
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

    /// A table row with no resources: none of these tests reaches a terminal or a shell.
    ///
    /// The handle is real — a retained `Spawned` is what every mutation is validated against — so
    /// the identity these rows carry is the identity the table would actually check.
    fn row(id: &str, close: Option<JobCloseMode>, starting: bool) -> Job {
        let sandbox = Sandbox {
            id: ShellId::from(id),
            dir: JobDir::default(),
            uid: SnapshotUid::from(format!("uid-{id}")),
        };
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(ClosedState::Pending);
        let handle = Spawned {
            inner: Arc::new(SpawnedState {
                id: sandbox.id.clone(),
                sandbox: sandbox.clone(),
                terminal: true,
                origin: Weak::new(),
                endpoints: OnceLock::new(),
                initial: Mutex::new(None),
                closed: closed_rx,
                discarded: Arc::new(tokio::sync::watch::channel(false).0),
            }),
        };
        Job {
            id: sandbox.id.clone(),
            sandbox,
            executor: crate::MarshExecutor::default(),
            io: JobIo::Terminal {
                geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
            },
            handle,
            working_directory: PathBuf::new(),
            resources: None,
            command: None,
            starting,
            close,
            release: None,
            closed: Arc::new(closed_tx),
        }
    }

    /// A reservation for a row that is running a command.
    fn reserve(id: u64, text: &str) -> ActiveCommand {
        let (verdict, _watch) = tokio::sync::watch::channel(CommandState::Pending);
        ActiveCommand {
            id: CommandId(id),
            text: Arc::from(text),
            verdict: Arc::new(verdict),
            on_finish: None,
            close_on_finish: false,
            spawn_mark: 0,
            context: None,
            running: true,
        }
    }

    /// A job name is a capability principal. Reusing one while its holder is alive would make two
    /// sandboxes indistinguishable in the policy history, so the series steps over a name a job
    /// already holds rather than colliding with it.
    #[test]
    fn the_automatic_series_never_collides_with_a_live_name() {
        let mut table = JobTable::new(24, 80);
        table.open.push(row("1", None, false));
        table.open.push(row("2", None, false));

        assert_eq!(table.next_id(), ShellId::from("3"));
        assert_eq!(
            table.next_id(),
            ShellId::from("4"),
            "the series is monotonic within a session, so a closed name is never handed out twice"
        );
    }

    /// A job's snapshot is what its command is running in, and its line has not been concluded
    /// yet. Reclaiming the row underneath it would delete the tree mid-command.
    #[test]
    fn a_job_with_work_in_flight_is_never_reclaimed() {
        let mut table = JobTable::new(24, 80);

        let mut running = row("busy", Some(JobCloseMode::Graceful), false);
        running.command = Some(reserve(1, "sleep 30"));
        table.open.push(running);
        table
            .open
            .push(row("starting", Some(JobCloseMode::Force), true));
        table.open.push(row("idle", Some(JobCloseMode::Graceful), false));

        assert!(
            table.take_closable(&ShellId::from("busy")).is_none(),
            "a command is still staged in that snapshot"
        );
        assert!(
            table.take_closable(&ShellId::from("starting")).is_none(),
            "a reserved launch still owes its waiter a verdict, force or not"
        );
        assert!(
            table.take_closable(&ShellId::from("idle")).is_some(),
            "an idle job that is to close has nothing left to conclude"
        );
    }

    /// A job that is *not* closing is not reclaimable at all: closure is a decision, and the table
    /// never makes it on a caller's behalf.
    #[test]
    fn a_job_nobody_closed_stays_open() {
        let mut table = JobTable::new(24, 80);
        table.open.push(row("kept", None, false));

        assert!(table.take_closable(&ShellId::from("kept")).is_none());
        assert_eq!(table.open.len(), 1);
    }

    /// Reclaiming the selected job clears the selection: a front-end that kept drawing a job the
    /// table no longer holds would be rendering a snapshot that has been deleted.
    #[test]
    fn reclaiming_the_selected_job_clears_the_selection() {
        let mut table = JobTable::new(24, 80);
        table.open.push(row("api", Some(JobCloseMode::Graceful), false));
        table.current = Some(ShellId::from("api"));

        assert!(table.take_closable(&ShellId::from("api")).is_some());
        assert_eq!(table.current, None);
    }

    /// The `1`, `2`, … series implies closure; a reader's `stop` and a one-shot command's own
    /// option *decide* it. Only the first may be cancelled by `keep`.
    #[test]
    fn only_the_automatic_closure_is_a_default() {
        assert!(!JobCloseMode::Automatic.explicit());
        assert!(JobCloseMode::Graceful.explicit());
        assert!(JobCloseMode::Force.explicit());
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

    /// A view reports what a caller can act on. A row whose resources are still being built is
    /// `starting` even when no command was submitted, because nothing can be sent to it yet.
    #[test]
    fn a_view_reports_readiness_rather_than_existence() {
        let mut opening = row("opening", None, false);
        opening.resources = None;
        assert!(
            job_view(&opening).starting,
            "a row with no streams cannot take input, whatever its flags say"
        );

        let mut running = row("running", None, false);
        running.command = Some(reserve(7, "make"));
        let view = job_view(&running);
        assert_eq!(view.running.as_ref().map(|running| running.cmd.as_str()), Some("make"));
        assert_eq!(view.running.map(|running| running.id), Some(CommandId(7)));
        assert!(!view.closing);
    }

    /// A reserved-but-not-yet-launched command is not *running*: a front-end that showed it as
    /// running would be reporting a process that does not exist.
    #[test]
    fn a_reserved_command_is_not_reported_as_running() {
        let mut reserved = row("reserved", None, true);
        let mut slot = reserve(3, "echo hi");
        slot.running = false;
        reserved.command = Some(slot);

        let view = job_view(&reserved);
        assert!(view.running.is_none());
        assert!(view.starting);
    }

    /// The whole point of the output receipt: a frontend that withholds one stops that stream
    /// being read, rather than being handed bytes faster than it can take them.
    ///
    /// The pump must not read the second chunk until the first chunk's receipt is completed, and
    /// it must resume immediately once it is.
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
            let mut held = receipts.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(receipt) = held.pop() {
                break receipt;
            }
            drop(held);
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
            let mut held = receipts.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(receipt) = held.pop() {
                break receipt;
            }
            drop(held);
            tokio::task::yield_now().await;
        };
        let _ = second.send(());
        pump.await.expect("the pump ends when its stream does");
    }
}
