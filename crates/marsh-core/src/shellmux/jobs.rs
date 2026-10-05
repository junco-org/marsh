//! The shell collection: the shells a front-end has open, the commands running in them, the
//! streams they own, and the principals they answer to.
//!
//! A job keeps a reusable presentation name and the opaque identity returned by its Shell.
//! Names never select policy principals; source sharing and authority live below the mux.
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
//! Registry → shell live state whenever both are needed. No transaction lock lives here.

use std::collections::HashMap;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::io::unix::AsyncFd;

use marsh_lib::{RecoverPoison as _, WaitState, wait_for_completion};

use crate::shellmux::command::{CommandCompletion, CommandHandle, CommandId, RunError, WaitError};
use crate::shellmux::error::MuxError;
use crate::shellmux::frontend::{FrontendEvent, ShellFrontend, notify};
use crate::shellmux::idle::{IdleTerminal, LeaseRefusal, LeaseState, Revocation};
use crate::shellmux::ids::ShellId;
use crate::shellmux::mux::{Sandbox, ShellMux};
use crate::shellmux::pipes::{PipeInput, read_pipe, read_terminal, write_fd};
use crate::shellmux::pty::resize_pty;
use crate::shellmux::types::{
    CommandOptions, JobCloseMode, JobEnd, JobIo, OutputChannel, SpawnOptions, TerminalGeometry,
};
use crate::{ExecutionResult, ShellError, ShellErrorKind};

/// How many bytes one output read takes at most.
const CHUNK: usize = 8192;

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
    /// Its reusable display/lookup name, logical source, directory label and stable principal.
    sandbox: Sandbox,
    /// Whether it is a terminal or a pipe shell. Fixed at admission.
    terminal: bool,
    /// The collection that admitted it. Weak, because a shell must not keep a torn-down mux
    /// alive; an action needing the collection answers [`MuxError::ShuttingDown`] once it is gone.
    origin: Weak<ShellMux>,
    /// The streams its input and terminal operations reach; `None` only for a shell built without.
    endpoints: Option<Endpoints>,
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

/// The reservation one admitted command holds in its shell's live state.
struct ActiveCommand {
    /// The public receipt reserved for it — identity and text included — so a snapshot can report
    /// it without a second registry.
    handle: CommandHandle,
    /// The verdict channel every [`CommandHandle`] on this command reads.
    verdict: Arc<tokio::sync::watch::Sender<WaitState<CommandCompletion, WaitError>>>,
    /// Whether the shell closes when this command ends.
    ///
    /// Recorded here rather than in the shell's own `close`, so standard input stays writable for
    /// the whole of a one-shot command: marking the shell closing at admission would reject the
    /// stdin a pipe reader is waiting for and deadlock it.
    close_on_finish: bool,
    /// Whether the line has been handed to the interpreter yet.
    running: bool,
}

impl ActiveCommand {
    /// Allocates one command's receipt and verdict channel, not yet running.
    ///
    /// Both halves exist before anything can run, which is what makes a completion impossible to
    /// steal: the identity the verdict resolves is captured here, not read back off the shell
    /// afterwards.
    fn new(id: CommandId, shell: &Sandbox, cmd: &str, close_on_finish: bool) -> Self {
        let (verdict, state) = tokio::sync::watch::channel(WaitState::Pending);
        let handle = CommandHandle {
            id,
            shell: shell.clone(),
            text: Arc::from(cmd),
            state,
            execution: crate::shell::ExecutionProgress::new(),
        };
        Self {
            handle,
            verdict: Arc::new(verdict),
            close_on_finish,
            running: false,
        }
    }
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

    /// Whether a command is admitted into it or being launched into it.
    const fn busy(&self) -> bool {
        self.command.is_some() || self.starting
    }

    /// Whether a stop has been accepted, so it will close and takes no new command.
    fn closing(&self) -> bool {
        self.close.is_some_and(JobCloseMode::explicit)
    }

    /// The interpreter, once the shell's resources are built.
    fn interpreter(&self) -> Option<Arc<crate::Shell>> {
        self.resources
            .as_ref()
            .map(|resources| Arc::clone(&resources.shell))
    }

    /// Closes the shell once its command is over, unless a forced stop has already retired it.
    const fn finish_gracefully(&mut self) {
        if !self.retired() {
            self.close = Some(JobCloseMode::Graceful);
        }
    }

    /// Fails the admitted command's receipt: a teardown means no verdict is coming.
    fn abandon(&self) {
        if let Some(active) = &self.command {
            active
                .verdict
                .send_replace(WaitState::Failed(WaitError::Shutdown));
        }
    }
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
    /// The shell's reusable display/lookup name, not its principal.
    pub id: ShellId,
    /// The sandbox its commands run in.
    pub sandbox: Sandbox,
    /// Terminal or pipes. A terminal view always carries a resolved geometry.
    pub io: JobIo,
    /// Where its interpreter currently stands, as of the last boundary.
    pub working_directory: PathBuf,
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

/// Admission state: name reservations, creation order, the automatic series, geometry and
/// shutdown linearize under one lock. A collection alone cannot express this heterogeneous
/// admission invariant.
pub(crate) struct ShellRegistry {
    /// Every name in creation order. `None` reserves a name while its ordinary Shell constructor
    /// runs outside the lock; names are unique.
    shells: Vec<(ShellId, Option<Shell>)>,
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
    pub(crate) const fn new(rows: u16, cols: u16) -> Self {
        Self {
            shells: Vec::new(),
            counter: 1,
            default_geometry: TerminalGeometry { rows, cols },
            closing: false,
        }
    }

    /// Whether `name` is reserved or held by a ready shell.
    fn holds(&self, name: &ShellId) -> bool {
        self.shells.iter().any(|(held, _)| held == name)
    }

    /// The next automatic shell name, skipping any a shell already occupies.
    ///
    /// Monotonic within a session, keeping automatic names predictable without colliding with
    /// explicit names. Policy ownership uses each shell's separate stable principal.
    fn next_id(&mut self) -> ShellId {
        loop {
            let id = ShellId::from(self.counter.to_string());
            self.counter += 1;
            if !self.holds(&id) {
                return id;
            }
        }
    }

    /// Records one name, reserved (`None`) or ready, keeping its place in creation order.
    fn insert(&mut self, name: ShellId, shell: Option<Shell>) {
        match self.shells.iter_mut().find(|(held, _)| *held == name) {
            Some((_, slot)) => *slot = shell,
            None => self.shells.push((name, shell)),
        }
    }

    /// Forgets one name, answering the ready shell it held.
    fn remove(&mut self, name: &ShellId) -> Option<Shell> {
        let index = self.shells.iter().position(|(held, _)| held == name)?;
        self.shells.remove(index).1
    }

    /// The ready shell a name holds; a reservation still under construction is none.
    fn get(&self, name: &ShellId) -> Option<&Shell> {
        self.shells
            .iter()
            .find(|(held, _)| held == name)?
            .1
            .as_ref()
    }

    /// Every ready shell, in creation order.
    fn ready(&self) -> impl Iterator<Item = &Shell> {
        self.shells.iter().filter_map(|(_, shell)| shell.as_ref())
    }
}

/// A reservation lives across async construction and releases only its own still-empty slot.
struct Opening {
    mux: Weak<ShellMux>,
    id: ShellId,
}
impl Drop for Opening {
    fn drop(&mut self) {
        if let Some(mux) = self.mux.upgrade() {
            mux.shell_registry()
                .shells
                .retain(|(held, shell)| shell.is_some() || *held != self.id);
        }
    }
}

/// Disjoint stream roles move together until the constructed shell is installed.
struct StreamSetup {
    fds: HashMap<crate::ShellFd, crate::OpenFile>,
    producers: Vec<OwnedFd>,
    endpoints: Endpoints,
    readers: Vec<(OutputChannel, Reader)>,
}

impl ShellMux {
    /// The shell registry, recovering a poisoned lock like the rest of this module.
    pub(crate) fn shell_registry(&self) -> MutexGuard<'_, ShellRegistry> {
        self.shells.lock().recover()
    }

    /// The background task set, with the same poisoning recovery.
    fn background(&self) -> MutexGuard<'_, tokio::task::JoinSet<()>> {
        self.tasks.lock().recover()
    }

    /// Reaps completed lifecycle tasks before admitting another onto this mux's own runtime.
    fn spawn_detached(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.background();
        while tasks.try_join_next().is_some() {}
        tasks.spawn_on(task, &self.runtime);
    }

    /// Whether teardown has begun and no further work is admitted.
    pub(crate) fn is_closing(&self) -> bool {
        self.shell_registry().closing
    }

    /// An answer that never arrived. Teardown is the one explanation that is not a bug, and it is
    /// deliberately reported as itself: neither says the line had no effects, so neither is
    /// permission to retry.
    fn lost_answer(&self) -> RunError {
        RunError::Wait(if self.is_closing() {
            WaitError::Shutdown
        } else {
            WaitError::Aborted
        })
    }

    /// Opens an ordinary Shell at a logical directory and attaches this mux's streams.
    /// A name is reserved while construction runs; only a fully constructed Shell supplies the
    /// sandbox's source and opaque instance identity. Failure releases the reservation.
    pub async fn open_shell(
        self: &Arc<Self>,
        initial_dir: &Path,
        name: Option<ShellId>,
        options: SpawnOptions,
    ) -> Result<Shell, MuxError> {
        let terminal = options.io.is_terminal();
        if let Some(geometry) = options.io.geometry() {
            validate_size(geometry.rows, geometry.cols)?;
        }
        let requested = std::path::absolute(if initial_dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            initial_dir
        })?;
        let (id, io) = self.reserve_name(name, options.io)?;
        let _reservation = Opening {
            mux: Arc::downgrade(self),
            id: id.clone(),
        };
        let StreamSetup {
            fds,
            producers,
            endpoints,
            readers,
        } = Self::open_streams(io)?;
        let interpreter = self
            .build_shell(
                Some(id.clone()),
                &requested,
                fds,
                options.environment,
                options.force_sandbox,
            )
            .await?;
        let working_directory = interpreter.working_dir().await;
        let sandbox = interpreter.sandbox().clone();
        let (release, released) = tokio::sync::oneshot::channel::<Arc<JobEnd>>();
        let shell = Shell::new(
            sandbox,
            terminal,
            Arc::downgrade(self),
            Some(endpoints),
            LiveShell {
                io,
                working_directory,
                resources: Some(JobResources {
                    _producers: producers,
                    shell: interpreter,
                }),
                command: None,
                starting: false,
                close: (options.automatic_close && terminal).then_some(JobCloseMode::Automatic),
                release: Some(release),
            },
        );
        let latest = self.install(id, &shell, options.io)?;
        if let (Some(geometry), Some(master)) = (latest, shell.master())
            && let Err(error) = resize_pty(master.get_ref().as_fd(), geometry.rows, geometry.cols)
        {
            self.shell_registry().remove(shell.id());
            if let Some(live) = shell.take_live()
                && let Some(resources) = live.resources
            {
                let _ = resources.shell.close(true).await;
            }
            return Err(error.into());
        }
        self.launched.notify_waiters();
        self.announce(FrontendEvent::Opened(&shell));
        self.announce(FrontendEvent::Changed);
        let inner = &shell.inner;
        self.spawn_detached(job_lifecycle(
            self.frontend(),
            inner.sandbox.clone(),
            readers,
            released,
            Arc::clone(&inner.closed_tx),
            inner.discarded.subscribe(),
            self.runtime.clone(),
        ));
        Ok(shell)
    }

    /// Reserves `name`, or the next automatic one, with `io` resolved to the default size.
    fn reserve_name(&self, name: Option<ShellId>, io: JobIo) -> Result<(ShellId, JobIo), MuxError> {
        let mut registry = self.shell_registry();
        if registry.closing {
            return Err(MuxError::ShuttingDown);
        }
        let id = name.unwrap_or_else(|| registry.next_id());
        if registry.holds(&id) {
            return Err(MuxError::JobExists(id));
        }
        let io = io.resolved(registry.default_geometry);
        registry.insert(id.clone(), None);
        drop(registry);
        Ok((id, io))
    }

    /// Publishes `shell` under its reserved `id` with `io` resolved to the current default size,
    /// answering the size its terminal must take.
    fn install(
        &self,
        id: ShellId,
        shell: &Shell,
        io: JobIo,
    ) -> Result<Option<TerminalGeometry>, MuxError> {
        let mut registry = self.shell_registry();
        if registry.closing {
            return Err(MuxError::ShuttingDown);
        }
        let io = io.resolved(registry.default_geometry);
        if io.is_terminal()
            && let Some(live) = shell.live_lock().as_mut()
        {
            live.io = io;
        }
        registry.insert(id, Some(shell.clone()));
        drop(registry);
        Ok(io.geometry())
    }

    /// The shell a display name selects, or `None` when nothing visible answers to it.
    ///
    /// Clones the current generation's object. It neither creates a shell, opens a seed, changes
    /// selection nor runs a command; a name no shell holds, and one a forced stop has
    /// retired, both answer `None`. A shell still constructing *is* returned — execution through
    /// it reports [`MuxError::JobNotReady`] until its streams exist.
    #[must_use]
    pub fn get_shell(&self, name: &ShellId) -> Option<Shell> {
        let shell = self.shell_registry().get(name)?.clone();
        shell.visible(|_| ())?;
        Some(shell)
    }

    /// The stream endpoints are created together so every producer copy has one cleanup owner.
    fn open_streams(io: JobIo) -> Result<StreamSetup, MuxError> {
        use brush_core::openfiles::OpenFiles;
        /// A second descriptor on `fd`, as a file the interpreter owns.
        fn file(fd: &OwnedFd) -> std::io::Result<crate::OpenFile> {
            Ok(std::fs::File::from(fd.try_clone()?).into())
        }
        let (files, producers, endpoints, readers) = match io {
            JobIo::Terminal { geometry } => {
                let geometry = geometry.unwrap_or(TerminalGeometry { rows: 24, cols: 80 });
                let (master, slave) = crate::shellmux::pty::open_pty(geometry.rows, geometry.cols)?;
                let tty = file(&slave)?;
                let tty_path = terminal_name(slave.as_fd());
                let master = Arc::new(AsyncFd::new(master)?);
                (
                    [tty.clone(), tty.clone(), tty],
                    vec![slave],
                    Endpoints::Terminal {
                        master: Arc::clone(&master),
                        lease: Arc::default(),
                        tty_path,
                    },
                    vec![(OutputChannel::Terminal, Reader::Terminal(master))],
                )
            }
            JobIo::Pipes => {
                let pipes = crate::shellmux::pipes::open_pipes()?;
                let files = [
                    file(&pipes.child_stdin)?,
                    file(&pipes.child_stdout)?,
                    file(&pipes.child_stderr)?,
                ];
                (
                    files,
                    vec![pipes.child_stdin, pipes.child_stdout, pipes.child_stderr],
                    Endpoints::Pipes {
                        input: Arc::new(PipeInput::new(pipes.input)?),
                    },
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
        let fds = [
            OpenFiles::STDIN_FD,
            OpenFiles::STDOUT_FD,
            OpenFiles::STDERR_FD,
        ]
        .into_iter()
        .zip(files)
        .collect();
        Ok(StreamSetup {
            fds,
            producers,
            endpoints,
            readers,
        })
    }

    /// Resolves the admitted command's receipt, delivers it, and records the shell's closure.
    ///
    /// The whole handoff happens in one acquisition of the live-state lock: the command's
    /// identity, its text and its closure decision are captured *before* the running
    /// slot is released, so neither a `keep` nor a second command admitted the instant afterwards
    /// can slip between completion and a one-shot closure.
    fn conclude_command(
        &self,
        shell: &Shell,
        result: Result<ExecutionResult, ShellError>,
    ) -> Option<Arc<CommandCompletion>> {
        let active = {
            let mut guard = shell.live_lock();
            let live = guard.as_mut()?;
            let active = live.command.take()?;
            if active.close_on_finish
                || live
                    .resources
                    .as_ref()
                    .is_some_and(|resources| resources.shell.is_closed())
            {
                live.finish_gracefully();
            }
            drop(guard);
            active
        };
        let completion = Arc::new(CommandCompletion {
            id: active.handle.id,
            shell: active.handle.shell,
            command: active.handle.text,
            result: Arc::new(result),
        });
        active
            .verdict
            .send_replace(WaitState::Done(Arc::clone(&completion)));
        // Outside the lock, like every other callback delivery.
        self.announce(FrontendEvent::Finished {
            completion: &completion,
        });
        Some(completion)
    }

    /// One consistent look at every shell and every unfinished command.
    #[must_use]
    pub fn snapshot(&self) -> MuxSnapshot {
        let registry = self.shell_registry();
        let default_geometry = registry.default_geometry;
        let guards: Vec<_> = registry
            .ready()
            .map(|shell| (shell, shell.live_lock()))
            .collect();
        let mut jobs = Vec::with_capacity(guards.len());
        let mut commands = Vec::new();
        for (shell, guard) in &guards {
            let Some(live) = guard.as_ref() else { continue };
            commands.extend(live.command.as_ref().map(|active| active.handle.clone()));
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
        let views = registry
            .ready()
            .filter_map(|shell| shell.visible(|live| job_view(shell, live)))
            .collect();
        drop(registry);
        views
    }

    /// One shell's view, by identity, excluding a forced one for the reason [`Self::jobs`] does.
    #[must_use]
    pub fn job(&self, id: &ShellId) -> Option<JobView> {
        let shell = self.shell_registry().get(id)?.clone();
        shell.visible(|live| job_view(&shell, live))
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
        validate_size(size.rows, size.cols)?;
        let targets: Vec<(Sandbox, Arc<AsyncFd<OwnedFd>>)> = {
            let mut registry = self.shell_registry();
            registry.default_geometry = size;
            let targets = registry
                .ready()
                .filter_map(|shell| {
                    let mut guard = shell.live_lock();
                    let live = guard.as_mut().filter(|live| live.io.is_terminal())?;
                    live.io = JobIo::Terminal {
                        geometry: Some(size),
                    };
                    drop(guard);
                    shell
                        .master()
                        .map(|master| (shell.inner.sandbox.clone(), Arc::clone(master)))
                })
                .collect();
            drop(registry);
            targets
        };

        let guard = self.resize_lock.lock().await;
        let mut failure = Ok(());
        let mut resized = Vec::with_capacity(targets.len());
        for (sandbox, master) in targets {
            match resize_pty(master.get_ref().as_fd(), size.rows, size.cols) {
                Ok(()) => resized.push(sandbox),
                Err(error) if failure.is_ok() => failure = Err(MuxError::Io(error)),
                Err(_) => {}
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

    /// Takes one shell's live half out of the collection, if it is to close and has nothing left
    /// in flight.
    ///
    /// A shell with a command admitted is kept whatever its mode: the snapshot is what that
    /// command is running in, and its line is not concluded yet.
    fn take_closable(&self, shell: &Shell) -> Option<LiveShell> {
        let mut registry = self.shell_registry();
        let held = registry.get(&shell.inner.sandbox.id)?;
        if !Arc::ptr_eq(&held.inner, &shell.inner) {
            return None;
        }
        let taken = shell
            .live_lock()
            .take_if(|live| live.close.is_some() && !live.busy())?;
        registry.remove(&shell.inner.sandbox.id);
        drop(registry);
        Some(taken)
    }

    /// Reclaims a closable shell: closes the ordinary Shell, then publishes its final lifecycle
    /// observation.
    ///
    /// The whole live half is taken, not only its resources: it also holds the shell's attached
    /// executor, and dropping that may be what releases the last handle on the shell's snapshot —
    /// blocking work that must not happen under a lock.
    async fn reclaim(&self, shell: &Shell, completion: Option<&Arc<CommandCompletion>>) {
        let Some(mut live) = self.take_closable(shell) else {
            return;
        };
        let release = live.release.take();
        let error = match live.resources.take() {
            Some(resources) => resources
                .shell
                .close(live.retired())
                .await
                .err()
                .map(|error| Arc::new(MuxError::from(error))),
            None => None,
        };
        let end = Arc::new(JobEnd {
            shell: shell.inner.sandbox.clone(),
            close_mode: live.close,
            completion: completion.cloned(),
            error,
        });
        drop(live);
        shell
            .inner
            .closed_tx
            .send_replace(WaitState::Done(Arc::clone(&end)));
        // The lifecycle task publishes `Closed` once every stream has ended, so a late chunk is
        // never delivered after the shell's end.
        if let Some(release) = release {
            let _ = release.send(end);
        }
        self.announce(FrontendEvent::Changed);
    }

    /// Stops admission, asks every Shell to cancel and join its producers, then releases mux I/O.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), MuxError> {
        let shells: Vec<Shell> = {
            let mut registry = self.shell_registry();
            registry.closing = true;
            let shells = registry.ready().cloned().collect();
            drop(registry);
            shells
        };
        let mut closes = Vec::new();
        for shell in &shells {
            let core = shell.live_lock().as_ref().and_then(|live| {
                live.abandon();
                live.interpreter()
            });
            shell.inner.discarded.send_replace(true);
            shell
                .inner
                .closed_tx
                .send_replace(WaitState::Failed(WaitError::Shutdown));
            if let Some(core) = core {
                closes.push(self.runtime.spawn(async move { core.close(true).await }));
            }
        }
        let mut failure = Ok(());
        for close in closes {
            match close.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if failure.is_ok() => failure = Err(MuxError::from(error)),
                Err(error) if failure.is_ok() => {
                    failure = Err(MuxError::Io(std::io::Error::other(error)));
                }
                _ => {}
            }
        }
        let mut detached = std::mem::take(&mut *self.background());
        detached.shutdown().await;
        for shell in self.drain_registry() {
            drop(shell.take_live());
        }
        self.detach();
        failure
    }

    /// Removes every registration, yielding ready shells in creation order.
    ///
    /// The iterator owns the removed slots and borrows no mux state. Shutdown and Drop
    /// release each shell's live resources after this method releases the registry lock;
    /// draining again finds nothing.
    fn drain_registry(&self) -> impl Iterator<Item = Shell> + use<> {
        let mut registry = self.shell_registry();
        registry.closing = true;
        let shells = std::mem::take(&mut registry.shells);
        drop(registry);
        shells.into_iter().filter_map(|(_, shell)| shell)
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
            if let Some(live) = shell.take_live() {
                live.abandon();
            }
            shell
                .inner
                .closed_tx
                .send_replace(WaitState::Failed(WaitError::Shutdown));
        }
    }
}

impl Shell {
    /// Allocates one generation around its endpoints and its live half.
    fn new(
        sandbox: Sandbox,
        terminal: bool,
        origin: Weak<ShellMux>,
        endpoints: Option<Endpoints>,
        live: LiveShell,
    ) -> Self {
        let (closed_tx, closed) = tokio::sync::watch::channel(WaitState::Pending);
        Self {
            inner: Arc::new(ShellState {
                sandbox,
                terminal,
                origin,
                endpoints,
                closed_tx: Arc::new(closed_tx),
                closed,
                discarded: Arc::new(tokio::sync::watch::channel(false).0),
                live: Mutex::new(Some(live)),
            }),
        }
    }

    /// The shell's reusable display/lookup name, not its principal.
    #[must_use]
    pub fn id(&self) -> &ShellId {
        &self.inner.sandbox.id
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
        // The sender going away with the shell still open is the producer being lost: nothing
        // will ever resolve this, and a waiter must not hang on it.
        wait_for_completion(&mut self.inner.closed.clone(), || WaitError::Aborted).await
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
        match self.inner.endpoints.as_ref() {
            Some(Endpoints::Terminal { master, .. }) => Ok(master.get_ref().as_fd()),
            None if self.inner.terminal => Err(self.error(MuxError::JobNotReady)),
            Some(Endpoints::Pipes { .. }) | None => Err(self.error(MuxError::NotTerminal)),
        }
    }

    /// The path of this shell's terminal, when it has one and the kernel named it.
    #[must_use]
    pub fn tty_path(&self) -> Option<&std::path::Path> {
        if let Some(Endpoints::Terminal { tty_path, .. }) = self.inner.endpoints.as_ref() {
            tty_path.as_deref()
        } else {
            None
        }
    }

    /// The terminal master, for the mux's own operations.
    fn master(&self) -> Option<&Arc<AsyncFd<OwnedFd>>> {
        if let Some(Endpoints::Terminal { master, .. }) = self.inner.endpoints.as_ref() {
            Some(master)
        } else {
            None
        }
    }

    /// The lease state, for the mux's own operations.
    fn lease(&self) -> Option<&Arc<LeaseState>> {
        if let Some(Endpoints::Terminal { lease, .. }) = self.inner.endpoints.as_ref() {
            Some(lease)
        } else {
            None
        }
    }

    /// `kind` of mux failure, naming this shell.
    fn error(&self, kind: fn(ShellId) -> MuxError) -> MuxError {
        kind(self.inner.sandbox.id.clone())
    }

    /// This generation's live half, recovering a poisoned lock like the rest of this module.
    fn live_lock(&self) -> MutexGuard<'_, Option<LiveShell>> {
        self.inner.live.lock().recover()
    }

    /// Takes this generation's live half out, releasing the lock before it is dropped.
    fn take_live(&self) -> Option<LiveShell> {
        self.live_lock().take()
    }

    /// Runs `body` on this generation's live half, under its lock.
    ///
    /// The one synchronous shape every operation on a live shell has: take the lock, answer
    /// [`MuxError::StaleJob`] if the generation has been reclaimed, do the work, release the
    /// lock. `body` is synchronous and its result is owned, so nothing borrowed from the
    /// generation escapes the guard and no `await` can happen while it is held.
    fn with_live<T>(
        &self,
        body: impl FnOnce(&mut LiveShell) -> Result<T, MuxError>,
    ) -> Result<T, MuxError> {
        let mut guard = self.live_lock();
        let outcome = guard
            .as_mut()
            .map_or_else(|| Err(self.error(MuxError::StaleJob)), body);
        drop(guard);
        outcome
    }

    /// Runs `body` on this generation's live half while it is in public view: `None` once it has
    /// been reclaimed or a forced stop has retired it.
    fn visible<T>(&self, body: impl FnOnce(&LiveShell) -> T) -> Option<T> {
        let guard = self.live_lock();
        let answer = guard.as_ref().filter(|live| !live.retired()).map(body);
        drop(guard);
        answer
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
        let kept = self.with_live(|live| {
            let kept = live.io.is_terminal() && live.close == Some(JobCloseMode::Automatic);
            if kept {
                live.close = None;
            }
            Ok(kept)
        })?;
        if kept && let Ok(mux) = self.mux() {
            mux.announce(FrontendEvent::Changed);
        }
        Ok(kept)
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
        let core = self.with_live(|live| Ok(live.interpreter()))?;
        if let Some(core) = core {
            core.signal_running(signal)?;
        }
        Ok(())
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
        let core = self.with_live(|live| {
            if live.retired() {
                return Err(self.error(MuxError::StaleJob));
            }
            live.close = Some(if force {
                JobCloseMode::Force
            } else {
                JobCloseMode::Graceful
            });
            Ok(live.interpreter())
        })?;
        if force {
            self.inner.discarded.send_replace(true);
            if let Some(core) = core {
                core.close(true).await?;
            }
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
        mux.reclaim(self, None).await;
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
        validate_size(size.rows, size.cols)?;
        self.with_live(|live| {
            if !live.io.is_terminal() {
                return Err(self.error(MuxError::NotTerminal));
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
        let desired = self
            .live_lock()
            .as_ref()
            .and_then(|live| live.io.geometry());
        if let (Some(desired), Some(master)) = (desired, self.master()) {
            resize_pty(master.get_ref().as_fd(), desired.rows, desired.cols)?;
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
            if live.retired() || (live.closing() && live.command.is_none()) {
                return Err(self.error(MuxError::JobClosing));
            }
            Ok(())
        })?;
        match self.inner.endpoints.as_ref() {
            Some(Endpoints::Terminal { master, .. }) => {
                write_fd(master, bytes).await.map_err(MuxError::Io)
            }
            Some(Endpoints::Pipes { input }) => input.write_all(bytes).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    self.error(MuxError::InputClosed)
                } else {
                    MuxError::Io(error)
                }
            }),
            None => Err(self.error(MuxError::JobNotReady)),
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
        match self.inner.endpoints.as_ref() {
            Some(Endpoints::Pipes { input }) => {
                input.close().await;
                Ok(())
            }
            None if !self.inner.terminal => Err(self.error(MuxError::JobNotReady)),
            Some(Endpoints::Terminal { .. }) | None => Err(self.error(MuxError::NotPiped)),
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
                return Err(self.error(MuxError::NotTerminal));
            }
            if live.busy() {
                return Err(self.error(MuxError::TerminalBusy));
            }
            let (Some(_), Some(state)) = (&live.resources, self.lease()) else {
                return Err(self.error(MuxError::JobNotReady));
            };
            // Under the live-state lock, beside `command` and `starting`: those three are what
            // make a terminal unavailable, and checking them in two places would let an admission
            // and a grant each conclude the terminal was theirs.
            state.reserve().map_err(|refusal| match refusal {
                LeaseRefusal::Held => self.error(MuxError::TerminalBusy),
                LeaseRefusal::Closing => self.error(MuxError::JobClosing),
                LeaseRefusal::Unrestored(error) => MuxError::Shared(error),
            })?;
            Ok(Arc::clone(state))
        })?;
        let Some(master) = self.master() else {
            return Err(self.error(MuxError::JobNotReady));
        };
        IdleTerminal::grant(master.get_ref().as_fd(), Arc::clone(&state)).map_err(|error| {
            // The reservation must not outlive a failed grant, or the terminal stays permanently
            // held for a lease nobody has.
            state.abandon();
            MuxError::Io(error)
        })
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
                return Err(self.error(MuxError::NotTerminal));
            }
            if live.busy() {
                return Err(self.error(MuxError::JobBusy));
            }
            live.interpreter()
                .ok_or_else(|| self.error(MuxError::JobNotReady))
        })?;
        interpreter
            .input_is_complete(line)
            .await
            .map_err(MuxError::from)
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
        let (shell, cmd, owner) = (self.clone(), cmd.to_owned(), Arc::clone(&mux));
        // On this collection's runtime, registered in its own task set: the work outlives this
        // call, so a caller that is a status thread, a foreign runtime or a detached queue must
        // not be what its pumps and its interpreter are tied to.
        mux.spawn_detached(async move {
            let _ = answer.send(execute_command(owner, shell, cmd, options).await);
        });
        // The producer is gone without an answer.
        wait.await.unwrap_or_else(|_| Err(mux.lost_answer()))
    }
}

/// Admits one line into `shell`, runs it, and resolves its boundary exactly once.
///
/// The whole of a command's life, on the collection's own runtime: admission, the idle-terminal
/// lease revocation, the reservation, the managed run, the gate, the receipt and the reclamation a
/// one-shot closure owes.
async fn execute_command(
    mux: Arc<ShellMux>,
    shell: Shell,
    cmd: String,
    options: CommandOptions,
) -> Result<Arc<CommandCompletion>, RunError> {
    let CommandOptions {
        close_on_finish,
        on_accept,
    } = options;
    if mux.is_closing() {
        return Err(RunError::Admission(MuxError::ShuttingDown));
    }
    shell
        .with_live(|live| {
            // As at creation: a line admitted against a half-applied seed would have real effects
            // before its gate could refuse it. This shell's own seed, never any other's.
            if live.closing() {
                return Err(shell.error(MuxError::JobClosing));
            }
            if live.busy() {
                return Err(shell.error(MuxError::JobBusy));
            }
            if live.resources.is_none() {
                return Err(shell.error(MuxError::JobNotReady));
            }
            live.starting = true;
            Ok(())
        })
        .map_err(RunError::Admission)?;
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
            // A generation already reclaimed has nothing left to retire.
            let _ = shell.with_live(|live| {
                live.starting = false;
                live.close = Some(JobCloseMode::Force);
                Ok(())
            });
            mux.reclaim(&shell, None).await;
            mux.announce(FrontendEvent::Changed);
            return Err(RunError::Admission(MuxError::Shared(error)));
        }
    }

    let (receipt, interpreter) = shell
        .with_live(|live| {
            let Some(interpreter) = live.interpreter() else {
                live.starting = false;
                return Err(shell.error(MuxError::JobNotReady));
            };
            // A pipe shell is one-shot whatever the caller asked for: it has no prompt to return to
            // and no terminal to keep, so a second line would run in a shell with nothing left to
            // carry its output. Recorded on the *command* rather than on the shell, so standard input
            // stays writable until that command's own verdict releases the running slot.
            let id = CommandId(mux.command_counter.fetch_add(1, Ordering::Relaxed));
            let mut active = ActiveCommand::new(
                id,
                &shell.inner.sandbox,
                &cmd,
                close_on_finish || !live.io.is_terminal(),
            );
            // A forced stop that arrived while the shell was still starting means the line never runs.
            let launch = !live.retired();
            active.running = launch;
            let receipt = active.handle.clone();
            // Together with `starting`, under the one lock that admits the line: a view taken between
            // the two would report a shell that is neither starting nor running a command.
            live.starting = false;
            live.command = Some(active);
            Ok((receipt, launch.then_some(interpreter)))
        })
        .map_err(RunError::Admission)?;
    mux.announce(FrontendEvent::CommandAccepted { command: &receipt });
    let progress = receipt.execution.clone();
    // After admission and after the lease is back, before anything can be written or finished:
    // this is the observation a caller feeding standard input or signalling the line needs, and
    // it must never arrive late enough for the command to have ended first.
    if let Some(sender) = on_accept {
        let _ = sender.send(receipt);
    }
    mux.launched.notify_waiters();

    let result = match interpreter {
        None => Err(ShellError::new(ShellErrorKind::Interrupted)),
        Some(interpreter) => {
            mux.announce(FrontendEvent::Changed);
            run_line(&shell, interpreter, &cmd, progress).await
        }
    };
    let completion = mux.conclude_command(&shell, result);
    mux.reclaim(&shell, completion.as_ref()).await;
    mux.announce(FrontendEvent::Changed);

    // The reservation was resolved by someone else, which only a teardown does.
    let completion = completion.ok_or_else(|| mux.lost_answer())?;
    // The gate's answer, mapped exactly once. A nonzero process status under an approved
    // publication is success; a zero status under a refusal is not.
    if completion.result.is_ok() {
        Ok(completion)
    } else {
        Err(RunError::Execution { completion })
    }
}

/// Runs the ordinary Shell once and mirrors only its logical cwd and closed state.
async fn run_line(
    shell: &Shell,
    interpreter: Arc<crate::Shell>,
    cmd: &str,
    progress: crate::shell::ExecutionProgress,
) -> Result<ExecutionResult, ShellError> {
    let result = interpreter.run_with_progress(cmd, progress).await;
    let working_directory = interpreter.working_dir().await;
    let mut guard = shell.live_lock();
    if let Some(live) = guard.as_mut() {
        live.working_directory = working_directory;
        if interpreter.is_closed() {
            live.finish_gracefully();
        }
    }
    drop(guard);
    result
}

/// One shell as a caller sees it.
fn job_view(shell: &Shell, live: &LiveShell) -> JobView {
    JobView {
        id: shell.id().clone(),
        sandbox: shell.inner.sandbox.clone(),
        io: live.io,
        working_directory: live.working_directory.clone(),
        running: live
            .command
            .as_ref()
            .filter(|active| active.running)
            .map(|active| RunningView {
                cmd: active.handle.text.to_string(),
                id: active.handle.id,
            }),
        starting: live.starting || live.resources.is_none(),
        closing: live.closing(),
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
        pumps.spawn_on(
            pump_stream(Arc::clone(&frontend), shell.clone(), channel, reader),
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
        })
    });
    closed.send_replace(WaitState::Done(Arc::clone(&end)));
    let _ = notify(&frontend, FrontendEvent::Closed { end: &end });
}

/// Carries one stream to the frontend, honouring the receipts it returns.
///
/// A withheld receipt slows *this* stream and nothing else: the awaits happen outside every mux
/// lock and outside the frontend's own mutex, so another shell's output, another channel and every
/// control operation keep running while this one waits. A stream that fails, or whose receipt is
/// dropped uncompleted, ends with one [`FrontendEvent::IoError`].
async fn pump_stream(
    frontend: Arc<Mutex<dyn ShellFrontend>>,
    shell: Sandbox,
    channel: OutputChannel,
    reader: Reader,
) {
    let mut buffer = [0u8; CHUNK];
    let error = loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(count) => count,
            Err(error) => break error,
        };
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
            break std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "frontend output consumer stopped",
            );
        }
    };
    let _ = notify(
        &frontend,
        FrontendEvent::IoError {
            shell: &shell,
            channel,
            error: &error,
        },
    );
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellmux::ids::{JobDir, Principal};

    /// A sandbox under a fixed seed whose principal is derived from its name.
    fn sandbox(id: &str) -> Sandbox {
        Sandbox {
            id: ShellId::from(id),
            seed: PathBuf::from("/seed"),
            dir: JobDir::default(),
            uid: Principal::from(format!("uid-{id}")),
        }
    }

    /// A shell with no resources: none of these tests reaches a terminal or an interpreter.
    ///
    /// Real objects, because a retained [`Shell`] is what every mutation now acts through, so the
    /// identity these carry is the identity the registry would actually check.
    fn shell(id: &str, close: Option<JobCloseMode>, starting: bool) -> Shell {
        let io = JobIo::Terminal {
            geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
        };
        Shell::new(
            sandbox(id),
            true,
            Weak::new(),
            None,
            LiveShell {
                io,
                working_directory: PathBuf::new(),
                resources: None,
                command: None,
                starting,
                close,
                release: None,
            },
        )
    }

    /// A reservation for a shell that is running a command.
    fn reserve(id: u64, text: &str, sandbox: &Sandbox) -> ActiveCommand {
        ActiveCommand {
            running: true,
            ..ActiveCommand::new(CommandId(id), sandbox, text, false)
        }
    }

    /// Installs `command` in `shell`'s live half.
    fn admit(shell: &Shell, command: ActiveCommand) {
        shell
            .live_lock()
            .as_mut()
            .expect("the fixture shell is live")
            .command = Some(command);
    }

    /// Runs `body` against `shell`'s live half.
    fn with_live<T>(shell: &Shell, body: impl FnOnce(&LiveShell) -> T) -> T {
        body(
            shell
                .live_lock()
                .as_ref()
                .expect("the fixture shell is live"),
        )
    }

    /// Automatic names must not hide shells already registered under an explicit name.
    #[test]
    fn the_automatic_series_never_collides_with_a_live_name() {
        let mut registry = ShellRegistry::new(24, 80);
        for name in ["1", "2"] {
            let held = shell(name, None, false);
            registry.insert(held.id().clone(), Some(held));
        }

        assert_eq!(registry.next_id(), ShellId::from("3"));
        assert_eq!(
            registry.next_id(),
            ShellId::from("4"),
            "the series is monotonic within a session, so a closed name is never handed out twice"
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

    /// A one-shot closure lives on the *command*, not on the shell, so standard input stays
    /// writable for the whole of that command. Marking the shell closing at admission would reject
    /// the stdin a pipe reader is waiting for.
    #[tokio::test]
    async fn a_one_shot_command_leaves_its_shell_open_to_input() {
        let held = shell("piped", None, false);
        let mut command = reserve(1, "cat", held.sandbox());
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
        admit(&running, reserve(7, "make", running.sandbox()));
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
        let mut slot = reserve(3, "echo hi", reserved.sandbox());
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
    #[tokio::test]
    async fn a_withheld_receipt_stops_the_stream_it_belongs_to() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// The receipts a [`Gate`] handed out, in delivery order.
        type Receipts = Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>;

        /// Counts deliveries and hands every one of them a receipt the test controls.
        struct Gate {
            /// How many output chunks have been delivered.
            delivered: Arc<AtomicUsize>,
            /// The receipts, in delivery order.
            receipts: Receipts,
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
                        self.receipts.lock().recover().push(sender);
                        Some(receiver)
                    }
                    _ => None,
                }
            }
        }

        /// The latest receipt the gate handed out, once it has handed one out.
        async fn next_receipt(receipts: &Receipts) -> tokio::sync::oneshot::Sender<()> {
            loop {
                let popped = receipts.lock().recover().pop();
                if let Some(receipt) = popped {
                    return receipt;
                }
                tokio::task::yield_now().await;
            }
        }

        let delivered = Arc::new(AtomicUsize::new(0));
        let receipts = Receipts::default();
        let frontend: Arc<Mutex<dyn ShellFrontend>> = Arc::new(Mutex::new(Gate {
            delivered: Arc::clone(&delivered),
            receipts: Arc::clone(&receipts),
        }));

        let pipes = crate::shellmux::pipes::open_pipes().expect("a pipe pair");
        let reader = Reader::Pipe(AsyncFd::new(pipes.stdout).expect("a registered read end"));
        let writer = pipes.child_stdout;
        drop((
            pipes.child_stdin,
            pipes.child_stderr,
            pipes.input,
            pipes.stderr,
        ));
        let pump = tokio::spawn(pump_stream(
            Arc::clone(&frontend),
            sandbox("gated"),
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
        let first = next_receipt(&receipts).await;
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
        while delivered.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }

        // Closing every writer ends the stream, which ends the pump.
        drop(writer);
        let _ = next_receipt(&receipts).await.send(());
        pump.await.expect("the pump ends when its stream does");
    }
}
