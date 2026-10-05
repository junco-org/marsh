//! Shell attribution over in-process native observation. No syscall decoder lives here.
//!
//! Linux lets a process trace its own descendants but never its own threads. Every command a
//! managed run spawns is therefore seized, before it can run anything of its own, by a tracer
//! thread of this process that owns exactly that command's process tree; the interpreter's own
//! filesystem accesses arrive instead as host records it reports right after making them. Both
//! are attributed through the scopes the host enters on its threads.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::ThreadId;
use std::time::Duration;

use crate::Syscall;
use crate::capture::proc_field;
use crate::host::{HostCall, records};
use crate::observation::{Observer, Resume, Sequence, UNCLASSIFIED, alive, creation};
use lurk_cli::syscall_info::{RetCode, SyscallArgs, SyscallInfo};
use marsh_lib::{CheckedAdvance, RecoverPoison as _};
use nix::errno::Errno;
use nix::sys::ptrace::Event;
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use syscalls::{Sysno, SysnoSet};

/// The published generation; a weak that no longer upgrades admits a new one.
static SHARED: Mutex<Option<Weak<Tracing>>> = Mutex::new(None);

thread_local! {
    /// Scopes entered on this host thread, innermost last: service, scope and attribution.
    static ENTERED: RefCell<Vec<(usize, ScopeId, Option<Target>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Opaque registration identity. Never reused within a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootId(u64);

/// Opaque builtin invocation identity, distinct from a root, run or scope marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvocationId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ScopeId(u64);

/// One evaluation's attribution identity, separate from native syscall evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraceRun {
    /// Registered root owning the evaluation.
    root: RootId,
    /// Monotonic evaluation identifier.
    sequence: u64,
}

impl TraceRun {
    /// Whether an attribution target belongs to this run.
    fn owns(self, target: Option<Target>) -> bool {
        target.is_some_and(|target| target.run == self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    run: TraceRun,
    builtin: Option<InvocationId>,
}

struct Root {
    path: PathBuf,
    observe: Arc<dyn Fn(TraceRun, Option<InvocationId>, Syscall) -> io::Result<()> + Send + Sync>,
    exec: Option<ExecHooks>,
}

/// A selected command a traced task has just executed, read before its new image ran anything.
///
/// Ephemeral: it is handed to [`ExecHooks::begin`] and never recorded as evidence.
pub struct ExecCommand {
    /// The task, now the executed image's.
    pub pid: i32,
    /// The delivery order of the exec call's entry.
    pub entry_order: u64,
    /// The executable, resolved against the entry-time cwd or directory descriptor.
    pub program: PathBuf,
    /// The argument vector, byte for byte.
    pub argv: Vec<OsString>,
    /// The environment entries, byte for byte.
    pub environment: Vec<OsString>,
    /// The entry-time working directory.
    pub cwd: PathBuf,
}

/// What a root decides for a selected command before it runs.
pub enum ExecDecision {
    /// Leave its attribution as it is.
    Continue,
    /// Attribute it and every descendant to this invocation, until all of them ended.
    Track(InvocationId),
    /// End it before it runs anything.
    Refuse,
}

/// A root's interest in executed commands.
///
/// Every hook is called without trace locks held, from the tracer thread that owns the task;
/// `begin` and `end` may spawn and wait for other traced commands, never for the task's own tree.
#[derive(Clone)]
pub struct ExecHooks {
    /// Whether an exec by a task of `run` owned by the invocation, of this executable, is
    /// selected: its arguments and environment are read at its entry.
    #[expect(
        clippy::type_complexity,
        reason = "a hook's whole signature is its contract; an alias would only hide it"
    )]
    pub select: Arc<dyn Fn(TraceRun, Option<InvocationId>, &Path) -> bool + Send + Sync>,
    /// A selected exec succeeded; the new image is stopped until this returns.
    pub begin: Arc<
        dyn Fn(TraceRun, Option<InvocationId>, ExecCommand) -> io::Result<ExecDecision>
            + Send
            + Sync,
    >,
    /// A tracked invocation's last task ended, with the executed task's status; `None` when its
    /// observation is incomplete. Called exactly once per tracked invocation.
    pub end: Arc<dyn Fn(TraceRun, InvocationId, Option<ExitStatus>) -> io::Result<()> + Send + Sync>,
}

/// Where a tracked invocation stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Some task of it is still live.
    Running,
    /// Every task ended, but creator-ordered records of it are still deferred; the held task
    /// stays stopped until they are classified.
    Barrier(Option<Pid>),
    /// Its end hook runs.
    Ending,
    /// Its end hook returned.
    Ended,
}

/// A command a root tracked from its exec until its last task ended.
struct Native {
    run: TraceRun,
    /// The tracer thread owning its tasks.
    tracer: ThreadId,
    /// The executed task.
    root: Pid,
    /// The exec's entry order.
    start: u64,
    /// The order after its end hook returned.
    finish: Option<u64>,
    /// Tasks of it that have not begun exiting.
    live: usize,
    /// The executed task's raw exit status.
    status: Option<i32>,
    phase: Phase,
}

/// A traced process's identity, pinned while one of its tasks was stopped.
struct ProcessLease {
    descriptor: OwnedFd,
    group: Pid,
}

/// A stop buffered until its thread's creator is known: sequence, status, event and record.
type Early = (u64, i32, Option<u64>, Option<Syscall>);

#[derive(Default)]
struct Thread {
    inherited: Option<Target>,
    /// The tracked invocation whose tree this task belongs to; descendants inherit it.
    cohort: Option<InvocationId>,
    known_parent: bool,
    process: Option<ProcessLease>,
    pending: Option<(u64, Option<Target>)>,
    classify_after: Option<u64>,
    early: Vec<Early>,
    /// Whether the task's last stop was a job-control (group) stop.
    job_stopped: bool,
    /// Whether the task began exiting.
    exiting: bool,
}

impl Thread {
    /// Kills the task's process through its pinned identity.
    fn kill(&self) -> io::Result<()> {
        if let Some(process) = &self.process {
            signal_process(&process.descriptor, libc::SIGKILL)?;
        }
        Ok(())
    }
}

/// A workload scope's native order window.
struct Scope {
    target: Target,
    first: Option<u64>,
    last: Option<u64>,
}

#[derive(Default)]
struct RunState {
    cancelled: bool,
    waker: Option<std::task::Waker>,
}

#[derive(Default)]
struct State {
    ids: u64,
    roots: HashMap<RootId, Root>,
    runs: HashMap<TraceRun, RunState>,
    scopes: HashMap<ScopeId, Scope>,
    threads: HashMap<Pid, Thread>,
    /// Records waiting for their creator's call, by its entry: attribution, cohort, record.
    deferred: HashMap<u64, Vec<(Target, Option<InvocationId>, Syscall)>>,
    natives: HashMap<InvocationId, Native>,
    failure: Option<String>,
}

impl CheckedAdvance for &mut State {
    type Output = u64;
    type Error = io::Error;
    fn value(&self) -> u64 {
        self.ids
    }
    fn advance(self, value: u64) -> u64 {
        self.ids = value;
        value
    }
    fn exhausted() -> io::Error {
        io::Error::other("trace ID exhaustion")
    }
}

impl State {
    fn check(&self) -> io::Result<()> {
        self.failure
            .as_ref()
            .map_or(Ok(()), |cause| Err(io::Error::other(cause.clone())))
    }

    fn run(&self, run: TraceRun) -> io::Result<()> {
        self.check()?;
        if self.runs.contains_key(&run) {
            Ok(())
        } else {
            Err(io::Error::other("closed trace run"))
        }
    }

    /// Whether records of the tracked invocation `id` still wait for a creator's call.
    fn deferred_for(&self, id: InvocationId) -> bool {
        self.deferred
            .values()
            .flatten()
            .any(|(_, cohort, _)| *cohort == Some(id))
    }

    /// Moves an exec-displaced thread's bookkeeping to the thread that now owns its identity.
    /// A displaced live member of a tracked tree no longer counts toward it.
    fn rekey(&mut self, tid: Pid, former: Pid) {
        if former == tid {
            return;
        }
        let Some(moved) = self.threads.remove(&former) else {
            return;
        };
        if let Some(displaced) = self.threads.insert(tid, moved)
            && !displaced.exiting
            && let Some(native) = displaced.cohort.and_then(|id| self.natives.get_mut(&id))
        {
            native.live = native.live.saturating_sub(1);
        }
    }

    /// Notes that `tid` began exiting — with `status` when known — and returns the tracked
    /// invocation it was the last live task of.
    ///
    /// # Errors
    /// A creator that exits inside a creating call whose child is already observed leaves that
    /// child's records unclassifiable; outside a cancelled run that fails observation.
    fn retire(&mut self, tid: Pid, status: Option<i32>) -> io::Result<Option<InvocationId>> {
        let cancelled = self.cancelled(self.threads.get(&tid).and_then(|thread| thread.inherited));
        let Some(thread) = self.threads.get_mut(&tid) else {
            return Ok(None);
        };
        let first = !std::mem::replace(&mut thread.exiting, true);
        let cohort = thread.cohort;
        if first && let Some((entry, _)) = thread.pending {
            let orphaned = self.deferred.contains_key(&entry)
                || self
                    .threads
                    .values()
                    .any(|thread| thread.classify_after == Some(entry));
            if orphaned {
                if !cancelled {
                    return Err(io::Error::other(UNCLASSIFIED));
                }
                // Cancelled work publishes nothing: drop what can never be ordered.
                self.deferred.remove(&entry);
                for thread in self.threads.values_mut() {
                    if thread.classify_after == Some(entry) {
                        thread.classify_after = None;
                    }
                }
            }
        }
        let Some(id) = cohort else {
            return Ok(None);
        };
        let Some(native) = self.natives.get_mut(&id) else {
            return Ok(None);
        };
        if native.root == tid && native.status.is_none() {
            native.status = status;
        }
        if !first {
            return Ok(None);
        }
        native.live = native.live.saturating_sub(1);
        Ok((native.live == 0 && native.phase == Phase::Running).then_some(id))
    }

    /// Puts an invocation whose last task ended into its barrier: still waiting on deferred
    /// records, holding `held`, or ready to end. Returns whether it is ready.
    fn barrier(&mut self, id: InvocationId, held: Option<Pid>) -> bool {
        let waiting = self.deferred_for(id);
        if let Some(native) = self.natives.get_mut(&id) {
            native.phase = if waiting {
                Phase::Barrier(held)
            } else {
                Phase::Ending
            };
        }
        !waiting
    }

    /// Whether an attribution target's run has been cancelled.
    fn cancelled(&self, target: Option<Target>) -> bool {
        target.is_some_and(|target| self.runs.get(&target.run).is_some_and(|run| run.cancelled))
    }
}

/// Locks trace bookkeeping, recovering the guard from a panicked holder.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().recover()
}

/// What a traced command's own process does, as its spawner needs to know it.
#[derive(Clone, Copy, Debug)]
pub enum ChildEvent {
    /// The process entered a job-control stop.
    Stopped,
    /// The process ended; its status was reaped by the tracer.
    Exited(std::process::ExitStatus),
}

/// A command spawned under observation. Its tracer reaps it: never wait on its pid elsewhere.
pub struct TracedChild {
    /// The process id.
    pub pid: u32,
    /// A pidfd pinning the process, for signalling it.
    pub process: OwnedFd,
    /// The tracer thread observing the command's tree.
    observer: std::thread::JoinHandle<io::Result<()>>,
}

impl TracedChild {
    /// Waits until the command's whole tree has ended and every record of it was delivered.
    ///
    /// # Errors
    /// Returns the observation failure that ended its tracer.
    pub fn wait_observed(self) -> io::Result<()> {
        self.observer
            .join()
            .map_err(|_| io::Error::other("native tracer panicked"))?
    }
}

/// The host's shared native observation service; Shell owns every registration and run.
pub struct Tracing {
    state: Mutex<State>,
    progress: Condvar,
    sequence: Arc<Sequence>,
    selected: SysnoSet,
}

impl Tracing {
    /// Shares the live service, or starts a new generation once the previous one is gone.
    ///
    /// # Errors
    /// Returns a live generation's recorded failure.
    pub fn shared() -> io::Result<Arc<Self>> {
        let mut shared = lock(&SHARED);
        if let Some(tracing) = shared.as_ref().and_then(Weak::upgrade) {
            // Never drop a possibly-final owner while holding the cache.
            drop(shared);
            let health = lock(&tracing.state).check();
            return health.map(|()| tracing);
        }
        let tracing = Arc::new(Self {
            state: Mutex::new(State::default()),
            progress: Condvar::new(),
            sequence: Arc::new(Sequence::default()),
            selected: Observer::selection()?,
        });
        *shared = Some(Arc::downgrade(&tracing));
        drop(shared);
        Ok(tracing)
    }

    /// Registers a physical work root, its native observation consumer, and the executed
    /// commands it tracks, if any.
    pub fn register_root(
        &self,
        root: &Path,
        observe: Arc<
            dyn Fn(TraceRun, Option<InvocationId>, Syscall) -> io::Result<()> + Send + Sync,
        >,
        exec: Option<ExecHooks>,
    ) -> io::Result<RootId> {
        let mut state = lock(&self.state);
        state.check()?;
        if state.roots.values().any(|entry| entry.path == root) {
            return Err(io::Error::other("root already registered"));
        }
        let id = RootId(state.next()?);
        state.roots.insert(
            id,
            Root {
                path: root.to_path_buf(),
                observe,
                exec,
            },
        );
        drop(state);
        Ok(id)
    }

    /// Admits one evaluation on a registered root.
    pub fn begin_run(&self, root: RootId) -> io::Result<TraceRun> {
        let mut state = lock(&self.state);
        state.check()?;
        if !state.roots.contains_key(&root) || state.runs.keys().any(|run| run.root == root) {
            return Err(io::Error::other("missing or busy trace root"));
        }
        let run = TraceRun {
            root,
            sequence: state.next()?,
        };
        state.runs.insert(run, RunState::default());
        drop(state);
        Ok(run)
    }

    /// Allocates a workload scope: what the host does inside it, and what it spawns, is the run's.
    pub fn scope(
        self: &Arc<Self>,
        run: TraceRun,
        builtin: Option<InvocationId>,
    ) -> io::Result<TraceScope> {
        let mut state = lock(&self.state);
        state.run(run)?;
        let id = ScopeId(state.next()?);
        let target = Target { run, builtin };
        state.scopes.insert(
            id,
            Scope {
                target,
                first: None,
                last: None,
            },
        );
        drop(state);
        Ok(TraceScope {
            tracing: Arc::clone(self),
            id,
            target: Some(target),
        })
    }

    /// Marks implementation work so it cannot become a command's evidence or producer.
    pub fn internal_scope(self: &Arc<Self>) -> io::Result<TraceScope> {
        let mut state = lock(&self.state);
        state.check()?;
        let id = ScopeId(state.next()?);
        drop(state);
        Ok(TraceScope {
            tracing: Arc::clone(self),
            id,
            target: None,
        })
    }

    /// Allocates an invocation identifier from the service's nonwrapping series.
    pub fn invocation(&self, run: TraceRun) -> io::Result<InvocationId> {
        let mut state = lock(&self.state);
        state.run(run)?;
        state.next().map(InvocationId)
    }

    /// Returns the native order window of one invocation: its entered scopes and, for a tracked
    /// command, everything from its exec's entry until its end hook returned.
    pub fn invocation_orders(
        &self,
        run: TraceRun,
        builtin: InvocationId,
    ) -> io::Result<(u64, u64)> {
        let state = lock(&self.state);
        state.run(run)?;
        let target = Target {
            run,
            builtin: Some(builtin),
        };
        let scopes = || {
            state
                .scopes
                .values()
                .filter(move |scope| scope.target == target)
        };
        let mut orders = scopes()
            .filter_map(|scope| scope.first)
            .min()
            .zip(scopes().filter_map(|scope| scope.last).max());
        if let Some(native) = state.natives.get(&builtin).filter(|native| native.run == run) {
            let finish = native
                .finish
                .ok_or_else(|| io::Error::other("tracked invocation has not ended"))?;
            orders = Some(orders.map_or((native.start, finish), |(first, last)| {
                (first.min(native.start), last.max(finish))
            }));
        }
        drop(state);
        orders.ok_or_else(|| io::Error::other("invocation has no completed native scope"))
    }

    /// Checks the run is still observable. Every record is classified before its producer
    /// proceeds (a tracee stays stopped, a host thread is inside the report), so there is no
    /// queue of undelivered callbacks to wait for.
    pub fn drain(&self, run: TraceRun) -> io::Result<()> {
        lock(&self.state).run(run)
    }

    /// Waits for all inherited producers and their pending calls to end.
    pub fn quiesce(&self, run: TraceRun) -> io::Result<()> {
        let mut state = lock(&self.state);
        loop {
            state.run(run)?;
            let busy = state.threads.values().any(|thread| {
                run.owns(thread.inherited)
                    || thread.pending.is_some_and(|(_, target)| run.owns(target))
            }) || state
                .deferred
                .values()
                .flatten()
                .any(|(target, ..)| target.run == run)
                || state
                    .natives
                    .values()
                    .any(|native| native.run == run && native.phase != Phase::Ended);
            if !busy {
                break;
            }
            state = self.progress.wait(state).recover();
        }
        drop(state);
        Ok(())
    }

    /// Signals the run's live traced processes, never historical PIDs or the host itself.
    ///
    /// `SIGCONT` goes only to processes in a job-control stop. Every tracer stop notifies this
    /// process with `SIGCHLD`, and an embedder that answers `SIGCHLD` by continuing its jobs
    /// would otherwise keep interrupting running commands with continue notifications.
    pub fn signal(&self, run: TraceRun, signal: i32) -> io::Result<usize> {
        let state = lock(&self.state);
        if !state.runs.contains_key(&run) {
            return Err(io::Error::other("closed trace run"));
        }
        let mut signalled = HashSet::new();
        for thread in state.threads.values().filter(|thread| {
            run.owns(thread.inherited) && (signal != libc::SIGCONT || thread.job_stopped)
        }) {
            if let Some(process) = &thread.process
                && !signalled.contains(&process.group)
                && signal_process(&process.descriptor, signal)?
            {
                signalled.insert(process.group);
            }
        }
        drop(state);
        Ok(signalled.len())
    }

    /// Reports loss of the native service even while a consumer callback is blocked.
    pub fn health(&self, run: TraceRun) -> io::Result<()> {
        lock(&self.state).run(run)
    }

    /// Cancels existing descendants and descendants whose creation is still being delivered.
    pub fn cancel(&self, run: TraceRun) -> io::Result<usize> {
        if let Some(state) = lock(&self.state).runs.get_mut(&run) {
            state.cancelled = true;
        }
        self.signal(run, libc::SIGKILL)
    }

    /// Registers the owning evaluation for immediate failure notification.
    pub fn poll_failure(
        &self,
        run: TraceRun,
        context: &std::task::Context<'_>,
    ) -> std::task::Poll<io::Error> {
        let mut state = lock(&self.state);
        if let Err(error) = state.run(run) {
            return std::task::Poll::Ready(error);
        }
        if let Some(run) = state.runs.get_mut(&run)
            && run
                .waker
                .as_ref()
                .is_none_or(|waker| !waker.will_wake(context.waker()))
        {
            run.waker = Some(context.waker().clone());
        }
        drop(state);
        std::task::Poll::Pending
    }

    /// Ends a fully quiescent evaluation and retires its attribution bookkeeping.
    pub fn end_run(&self, run: TraceRun) -> io::Result<()> {
        self.quiesce(run)?;
        let mut state = lock(&self.state);
        state.runs.remove(&run);
        state.scopes.retain(|_, scope| scope.target.run != run);
        state.natives.retain(|_, native| native.run != run);
        drop(state);
        Ok(())
    }

    /// Releases a root only after its runs have finished.
    pub fn unregister_root(&self, root: RootId) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.runs.keys().any(|run| run.root == root) && state.failure.is_none() {
            return Err(io::Error::other("trace root still owns a run"));
        }
        // A failed service cannot prove reclamation. Its owner retains the private tree, but
        // must still be able to close all roots and release the failed generation.
        state.runs.retain(|run, _| run.root != root);
        state
            .roots
            .remove(&root)
            .ok_or_else(|| io::Error::other("missing trace root"))?;
        drop(state);
        Ok(())
    }

    /// Reports one filesystem access the host interpreter performed on this thread. Outside a
    /// workload scope it is implementation work and records nothing.
    ///
    /// # Errors
    /// Fails when the access cannot be restated as evidence; the caller must refuse the run.
    pub fn host(&self, call: HostCall<'_>) -> io::Result<()> {
        let Some(target) = self.current() else {
            return Ok(());
        };
        lock(&self.state).run(target.run)?;
        for record in records(call, nix::unistd::gettid(), &self.sequence)? {
            self.classify(target, record)?;
        }
        Ok(())
    }

    /// Spawns `command` for the workload scope entered on this thread, traced from before it
    /// runs anything of its own until its whole process tree has ended.
    ///
    /// The child parks in a handshake between its launch setup and `exec`; a dedicated tracer
    /// thread seizes it there, and every task of its tree stays that thread's. The tracer reaps
    /// the command, reporting it through `events`, so nothing else may wait on its pid: in one
    /// thread group any such wait would also consume the tracer's stops.
    ///
    /// # Errors
    /// Refuses outside a workload scope, a program named without a path, and launch failures.
    pub fn spawn(
        self: &Arc<Self>,
        mut command: std::process::Command,
        events: Box<dyn FnMut(ChildEvent) + Send>,
    ) -> io::Result<TracedChild> {
        let target = self
            .current()
            .ok_or_else(|| io::Error::other("a managed command spawned outside its run's scope"))?;
        lock(&self.state).run(target.run)?;
        // A PATH search would exec several candidates, and only the first failure is detached.
        if !command.get_program().as_encoded_bytes().contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a managed command needs its resolved program path",
            ));
        }
        let host = nix::unistd::gettid();
        let (request_read, request_write) = pipe()?;
        let (release_read, release_write) = pipe()?;
        let (pinned, identity) = std::sync::mpsc::sync_channel(1);
        let tracing = Arc::clone(self);
        let tracer = std::thread::Builder::new()
            .name("marsh-tracer".into())
            .spawn(move || tracing.trace(request_read, release_write, &pinned, target, host, events))?;
        let (request, release) = (request_write.as_raw_fd(), release_read.as_raw_fd());
        // SAFETY: the hook runs in the forked child and only calls async-signal-safe getpid, write
        // and read on two descriptors the child inherited; it allocates nothing.
        unsafe {
            command.pre_exec(move || handshake(request, release));
        }
        let spawned = command.spawn();
        drop(command);
        drop((request_write, release_read));
        match spawned {
            Ok(child) => {
                let pid = child.id();
                // Dropping the std handle closes nothing the command uses and never waits.
                drop(child);
                let process = identity
                    .recv()
                    .map_err(|_| io::Error::other("tracer ended before pinning its command"))?;
                Ok(TracedChild {
                    pid,
                    process,
                    observer: tracer,
                })
            }
            Err(error) => {
                // No command runs; the tracer ends once it has nothing left to trace.
                let _ = tracer.join();
                Err(error)
            }
        }
    }

    /// The attribution of the innermost scope this thread entered on this service.
    fn current(&self) -> Option<Target> {
        let service = std::ptr::from_ref(self) as usize;
        ENTERED.with_borrow(|entered| {
            entered
                .iter()
                .rev()
                .find(|(owner, ..)| *owner == service)
                .and_then(|(_, _, target)| *target)
        })
    }

    /// One command tree's tracer thread: seize the command in its handshake, release it, then
    /// deliver every stop of its tree until none is left.
    fn trace(
        self: &Arc<Self>,
        request: OwnedFd,
        release: OwnedFd,
        pinned: &std::sync::mpsc::SyncSender<OwnedFd>,
        target: Target,
        host: Pid,
        mut events: Box<dyn FnMut(ChildEvent) + Send>,
    ) -> io::Result<()> {
        let mut pid = [0; 4];
        // End of file: the launch failed before its handshake.
        if std::fs::File::from(request).read_exact(&mut pid).is_err() {
            return Ok(());
        }
        let root = Pid::from_raw(i32::from_ne_bytes(pid));
        let tracer = std::thread::current().id();
        let mut observer = Observer::new(self.selected.clone(), Arc::clone(&self.sequence), root);
        let selector = Arc::clone(self);
        observer.select_exec(Box::new(move |tid, path| selector.selects(tid, path)));
        let result = (|| -> io::Result<()> {
            let mut observe = |sequence, tid: Pid, status, event, info, exec| {
                let resume = self.deliver(sequence, tid, status, event, info, exec)?;
                if tid == root {
                    report(&mut *events, status)?;
                }
                Ok(resume)
            };
            let Some(status) = observer.seize(root)? else {
                return Ok(());
            };
            let process = open_process(root.as_raw())?;
            pinned
                .send(process.try_clone()?)
                .map_err(|_| io::Error::other("spawner abandoned its command"))?;
            self.adopt_root(root, target, host, process)?;
            observer.stop(root, status, &mut observe)?;
            std::fs::File::from(release).write_all(&[1])?;
            observer.run(&mut observe)?;
            // A barrier still waiting once its whole tree is gone can never be lifted.
            if lock(&self.state)
                .natives
                .values()
                .any(|native| native.tracer == tracer && native.phase != Phase::Ended)
            {
                return Err(io::Error::other(UNCLASSIFIED));
            }
            Ok(())
        })();
        if observer.detached() {
            lock(&self.state).threads.remove(&root);
            self.progress.notify_all();
        }
        if let Err(error) = &result {
            let mut state = lock(&self.state);
            if state.cancelled(Some(target)) && error.to_string() == UNCLASSIFIED {
                // A cancelled run publishes nothing: what it left unordered is only dropped.
                for records in state.deferred.values_mut() {
                    records.retain(|(owner, ..)| owner.run != target.run);
                }
                state.deferred.retain(|_, records| !records.is_empty());
                drop(state);
            } else {
                drop(state);
                self.fail(format!("native observation: {error}"));
            }
            observer.abandon(|tid, status| {
                lock(&self.state).threads.remove(&tid);
                if tid == root {
                    events(ChildEvent::Exited(std::process::ExitStatus::from_raw(status)));
                }
            });
            self.abandon_natives(tracer);
            self.progress.notify_all();
        }
        result
    }

    /// Whether `tid`'s owner selects the executable it is about to exec.
    fn selects(&self, tid: Pid, path: &Path) -> bool {
        let state = lock(&self.state);
        let Some(target) = state.threads.get(&tid).and_then(|thread| thread.inherited) else {
            return false;
        };
        if state.cancelled(Some(target)) {
            return false;
        }
        let select = state
            .roots
            .get(&target.run.root)
            .and_then(|root| root.exec.as_ref())
            .map(|exec| Arc::clone(&exec.select));
        drop(state);
        select.is_some_and(|select| select(target.run, target.builtin, path))
    }

    /// Ends every tracked invocation of this tracer's tree as incomplete, after its tasks are
    /// gone.
    fn abandon_natives(&self, tracer: ThreadId) {
        let open: Vec<InvocationId> = lock(&self.state)
            .natives
            .iter()
            .filter(|(_, native)| native.tracer == tracer && native.phase != Phase::Ended)
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            let _ = self.end_native(id, false);
        }
    }

    /// Calls a tracked invocation's end hook once, outside every lock, then closes its order
    /// window and returns the task its barrier held, for the caller to release. Its status is
    /// passed only when `complete` and its run was not cancelled.
    fn end_native(&self, id: InvocationId, complete: bool) -> io::Result<Option<Pid>> {
        let mut state = lock(&self.state);
        let Some(run) = state.natives.get(&id).map(|native| native.run) else {
            return Ok(None);
        };
        let complete = complete && !state.cancelled(Some(Target { run, builtin: None }));
        let end = state
            .roots
            .get(&run.root)
            .and_then(|root| root.exec.as_ref())
            .map(|exec| Arc::clone(&exec.end));
        let Some(native) = state.natives.get_mut(&id) else {
            return Ok(None);
        };
        let held = match native.phase {
            Phase::Barrier(held) => held,
            _ => None,
        };
        native.phase = Phase::Ending;
        let status = native
            .status
            .filter(|_| complete)
            .map(ExitStatus::from_raw);
        drop(state);
        let ended = end.map_or(Ok(()), |end| end(run, id, status));
        let order = self.sequence.next();
        if let Some(native) = lock(&self.state).natives.get_mut(&id) {
            native.finish = order.as_ref().ok().copied();
            native.phase = Phase::Ended;
        }
        self.progress.notify_all();
        ended?;
        order.map(|_| held)
    }

    /// Ends the invocations that became `ready`, and every barrier of this tracer whose
    /// deferred records have all been classified.
    fn settle(&self, mut resume: Resume, mut ready: Vec<InvocationId>) -> io::Result<Resume> {
        let tracer = std::thread::current().id();
        let state = lock(&self.state);
        ready.extend(
            state
                .natives
                .iter()
                .filter(|(id, native)| {
                    native.tracer == tracer
                        && matches!(native.phase, Phase::Barrier(_))
                        && !state.deferred_for(**id)
                })
                .map(|(id, _)| *id),
        );
        drop(state);
        for id in ready {
            resume.release.extend(self.end_native(id, true)?);
        }
        Ok(resume)
    }

    /// Registers a seized command under the scope that spawned it, and hands its classifier
    /// the fork that created it, so its inherited descriptors are the host's.
    fn adopt_root(&self, root: Pid, target: Target, host: Pid, process: OwnedFd) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.run(target.run)?;
        let cancelled = state.cancelled(Some(target));
        let thread = state.threads.entry(root).or_default();
        thread.inherited = Some(target);
        thread.known_parent = true;
        thread.process = Some(ProcessLease {
            descriptor: process,
            group: root,
        });
        if cancelled {
            thread.kill()?;
        }
        drop(state);
        let fork = Syscall {
            info: SyscallInfo {
                typ: "SYSCALL",
                pid: host,
                syscall: Sysno::fork,
                args: SyscallArgs(Vec::new()),
                result: RetCode::Ok(root.as_raw()),
                duration: Duration::ZERO,
            },
            entry_order: self.sequence.next()?,
            cwd: None,
            return_fd: None,
            paths: Vec::new(),
            descriptors: Vec::new(),
            flags: None,
            submissions: None,
        };
        self.classify(target, fork)
    }

    fn fail(&self, cause: impl Into<String>) {
        let mut state = lock(&self.state);
        state.failure.get_or_insert_with(|| cause.into());
        let waiters: Vec<_> = state
            .runs
            .values_mut()
            .filter_map(|run| run.waker.take())
            .collect();
        drop(state);
        self.progress.notify_all();
        for waker in waiters {
            waker.wake();
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one stop's attribution, exec admission, adoption and exit barrier are applied \
                  in a single order against one locked state"
    )]
    fn deliver(
        &self,
        sequence: u64,
        tid: Pid,
        status: i32,
        event: Option<u64>,
        info: Option<Syscall>,
        exec: Option<io::Result<ExecCommand>>,
    ) -> io::Result<Resume> {
        let wait = WaitStatus::from_raw(tid, status).map_err(io::Error::other)?;
        let mut resume = Resume::default();
        let mut state = lock(&self.state);
        state.check()?;
        if state.runs.is_empty()
            && matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        {
            state.threads.remove(&tid);
            return Ok(resume);
        }
        // A missing message means the task was killed at this stop; nothing is left to rekey or
        // adopt through it.
        if matches!(wait, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXEC as i32)
            && event.is_some()
        {
            let former = Self::event_thread(event, "missing exec identity")?;
            state.rekey(tid, former);
        }
        if let Some(thread) = state.threads.get_mut(&tid) {
            thread.job_stopped = matches!(
                wait,
                WaitStatus::PtraceEvent(_, signal, code)
                    if code == Event::PTRACE_EVENT_STOP as i32
                        && matches!(
                            signal,
                            Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU
                        )
            );
        }
        if matches!(wait, WaitStatus::PtraceEvent(..) | WaitStatus::Stopped(..)) {
            Self::lease(&mut state, tid)?;
        }
        if let Some(command) = exec {
            let command = command.map_err(|error| {
                io::Error::other(format!("capturing a selected exec of {tid}: {error}"))
            })?;
            state = self.admit(state, tid, command)?;
        }
        if let WaitStatus::PtraceEvent(_, _, code) = wait
            && creation(code)
            && event.is_some()
        {
            let child = Self::event_thread(event, "missing child identity")?;
            let early = Self::adopt(&mut state, tid, child)?;
            drop(state);
            let mut held = false;
            for (sequence, status, event, info) in early {
                let replay = self.deliver(sequence, child, status, event, info, None)?;
                // The child was held at its first stop; it goes on unless its replay holds it.
                held = !ended(status) && !replay.hold_current;
                resume.release.extend(replay.release);
            }
            if held {
                resume.release.push(child);
            }
            self.progress.notify_all();
            return self.settle(resume, Vec::new());
        }
        if let Some(thread) = state.threads.get_mut(&tid)
            && !thread.known_parent
        {
            // Nothing it does may run before its owner is known: it could exec a command its
            // owner would have selected.
            resume.hold_current = !ended(status);
            thread.early.push((sequence, status, event, info));
            return Ok(resume);
        }
        let mut ready = Vec::new();
        if matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) {
            if let Some(id) = state.retire(tid, Some(status))?
                && state.barrier(id, None)
            {
                ready.push(id);
            }
            state.threads.remove(&tid);
            drop(state);
            self.progress.notify_all();
            return self.settle(resume, ready);
        }
        if matches!(wait, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXIT as i32)
            && let Some(id) = state.retire(tid, event.and_then(|code| i32::try_from(code).ok()))?
        {
            // The last task of a tracked tree: its parent cannot see it end before the
            // invocation's end hook has returned.
            if state.barrier(id, Some(tid)) {
                ready.push(id);
            } else {
                resume.hold_current = true;
            }
        }
        let Some(info) = info else {
            Self::note_stop(&mut state, tid, wait, sequence)?;
            drop(state);
            return self.settle(resume, ready);
        };
        let (entry, target) = state
            .threads
            .get_mut(&tid)
            .and_then(|thread| thread.pending.take())
            .ok_or_else(|| io::Error::other("native completion lacks attribution entry"))?;
        if entry != info.entry_order {
            return Err(io::Error::other("native completion entry mismatch"));
        }
        drop(state);
        if let Some(target) = target {
            self.classify(target, info)?;
        }
        self.progress.notify_all();
        self.settle(resume, ready)
    }

    /// Hands a selected command its owner's root before its new image runs, and applies the
    /// decision. No lock is held while the hook runs.
    fn admit<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        tid: Pid,
        command: ExecCommand,
    ) -> io::Result<MutexGuard<'a, State>> {
        let Some(target) = state.threads.get(&tid).and_then(|thread| thread.inherited) else {
            return Ok(state);
        };
        let begin = state
            .roots
            .get(&target.run.root)
            .and_then(|root| root.exec.as_ref())
            .map(|exec| Arc::clone(&exec.begin));
        let Some(begin) = begin.filter(|_| !state.cancelled(Some(target))) else {
            return Ok(state);
        };
        drop(state);
        let start = command.entry_order;
        let decision = begin(target.run, target.builtin, command)?;
        state = lock(&self.state);
        state.check()?;
        match decision {
            ExecDecision::Continue => {}
            ExecDecision::Track(id) => {
                let tracked = Target {
                    run: target.run,
                    builtin: Some(id),
                };
                let thread = state
                    .threads
                    .get_mut(&tid)
                    .ok_or_else(|| io::Error::other("a tracked exec lost its task"))?;
                thread.inherited = Some(tracked);
                thread.cohort = Some(id);
                if let Some((_, pending)) = &mut thread.pending {
                    *pending = Some(tracked);
                }
                state.natives.insert(
                    id,
                    Native {
                        run: target.run,
                        tracer: std::thread::current().id(),
                        root: tid,
                        start,
                        finish: None,
                        live: 1,
                        status: None,
                        phase: Phase::Running,
                    },
                );
            }
            ExecDecision::Refuse => {
                if let Some(thread) = state.threads.get(&tid) {
                    thread.kill()?;
                }
            }
        }
        Ok(state)
    }

    /// Pins a stopped task's process identity once, killing it when its run was cancelled.
    fn lease(state: &mut State, tid: Pid) -> io::Result<()> {
        let cancelled = state.cancelled(state.threads.get(&tid).and_then(|thread| thread.inherited));
        let thread = state.threads.entry(tid).or_default();
        if thread.process.is_none()
            && let Some(group) = proc_field::<i32>(format!("/proc/{tid}/status"), "Tgid:")
                .ok()
                .flatten()
                .filter(|group| *group > 0)
            && let Some(descriptor) = alive_process(group)?
        {
            thread.process = Some(ProcessLease {
                descriptor,
                group: Pid::from_raw(group),
            });
        }
        if cancelled {
            thread.kill()?;
        }
        Ok(())
    }

    /// Decodes a ptrace event message that carries a thread identity.
    fn event_thread(event: Option<u64>, missing: &'static str) -> io::Result<Pid> {
        Ok(Pid::from_raw(
            i32::try_from(event.ok_or_else(|| io::Error::other(missing))?)
                .map_err(io::Error::other)?,
        ))
    }

    /// Registers a created thread under its creator's attribution and tracked tree, returning
    /// its early stops.
    fn adopt(state: &mut State, tid: Pid, child: Pid) -> io::Result<Vec<Early>> {
        let parent = state.threads.get(&tid);
        let parent_entry = parent.and_then(|thread| thread.pending);
        let cohort = parent.and_then(|thread| thread.cohort);
        let inherited = parent_entry
            .and_then(|(_, target)| target)
            .or_else(|| parent.and_then(|thread| thread.inherited));
        let cancelled = state.cancelled(inherited);
        let thread = state.threads.entry(child).or_default();
        thread.known_parent = true;
        thread.inherited = inherited;
        thread.cohort = cohort;
        if cancelled {
            thread.kill()?;
        }
        if inherited.is_some() {
            thread.classify_after = parent_entry.map(|(entry, _)| entry);
        }
        let early = std::mem::take(&mut thread.early);
        if let Some(native) = cohort.and_then(|id| state.natives.get_mut(&id)) {
            native.live += 1;
        }
        Ok(early)
    }

    /// Records a record-less stop: a syscall entry's attribution, or an exit's cleared entry.
    fn note_stop(state: &mut State, tid: Pid, wait: WaitStatus, sequence: u64) -> io::Result<()> {
        let thread = state.threads.entry(tid).or_default();
        if matches!(wait, WaitStatus::PtraceSyscall(_)) {
            let target = thread.inherited;
            if thread.pending.replace((sequence, target)).is_some() {
                return Err(io::Error::other(
                    "native entry replaced pending attribution",
                ));
            }
        } else if matches!(wait, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXIT as i32)
        {
            thread.pending = None;
        }
        Ok(())
    }

    fn classify(&self, target: Target, info: Syscall) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some((entry, cohort)) = state
            .threads
            .get(&info.info.pid)
            .and_then(|thread| thread.classify_after.map(|entry| (entry, thread.cohort)))
        {
            state
                .deferred
                .entry(entry)
                .or_default()
                .push((target, cohort, info));
            return Ok(());
        }
        let created = matches!(
            info.info.syscall,
            Sysno::clone | Sysno::clone3 | Sysno::fork | Sysno::vfork
        )
        .then_some(info.entry_order);
        let observe = state
            .roots
            .get(&target.run.root)
            .map(|root| Arc::clone(&root.observe));
        drop(state);
        if let Some(observe) = observe {
            observe(target.run, target.builtin, info)?;
        }
        if let Some(entry) = created {
            let mut state = lock(&self.state);
            for thread in state.threads.values_mut() {
                if thread.classify_after == Some(entry) {
                    thread.classify_after = None;
                }
            }
            let deferred = state.deferred.remove(&entry).unwrap_or_default();
            drop(state);
            for (target, _, info) in deferred {
                self.classify(target, info)?;
            }
        }
        Ok(())
    }

    /// Records a scope boundary's order for a workload scope.
    fn mark(&self, id: ScopeId, entering: bool) {
        let order = match self.sequence.next() {
            Ok(order) => order,
            Err(error) => return self.fail(error.to_string()),
        };
        if let Some(scope) = lock(&self.state).scopes.get_mut(&id) {
            if entering {
                scope.first.get_or_insert(order);
            } else {
                scope.last = Some(order);
            }
        }
    }
}

impl Drop for Tracing {
    fn drop(&mut self) {
        // Only the published generation clears its entry.
        let mut shared = lock(&SHARED);
        if shared
            .as_ref()
            .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self))
        {
            *shared = None;
        }
        drop(shared);
    }
}

/// The child's side of the launch handshake: announce its pid, then wait to be released.
fn handshake(request: RawFd, release: RawFd) -> io::Result<()> {
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() }.to_ne_bytes();
    let mut written = 0;
    while written < pid.len() {
        // SAFETY: the buffer is live for the stated length; the descriptor was inherited open.
        let count = unsafe {
            libc::write(
                request,
                pid[written..].as_ptr().cast(),
                pid.len() - written,
            )
        };
        match usize::try_from(count) {
            Ok(count) => written += count,
            Err(_) if Errno::last() == Errno::EINTR => {}
            Err(_) => return Err(io::Error::last_os_error()),
        }
    }
    let mut byte = 0_u8;
    loop {
        // SAFETY: one writable byte; the descriptor was inherited open.
        let count = unsafe { libc::read(release, (&raw mut byte).cast(), 1) };
        if count == 1 && byte == 1 {
            return Ok(());
        }
        if count < 0 && Errno::last() == Errno::EINTR {
            continue;
        }
        // The tracer refused or vanished: never run unobserved.
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
}

/// Whether a raw wait status is a task's final exit rather than a stop.
const fn ended(status: i32) -> bool {
    libc::WIFEXITED(status) || libc::WIFSIGNALED(status)
}

/// Tells the spawner what a delivered stop means for its command's own process.
fn report(events: &mut dyn FnMut(ChildEvent), status: i32) -> io::Result<()> {
    match WaitStatus::from_raw(Pid::from_raw(0), status)? {
        WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
            events(ChildEvent::Exited(std::process::ExitStatus::from_raw(status)));
        }
        WaitStatus::PtraceEvent(_, signal, code)
            if code == Event::PTRACE_EVENT_STOP as i32
                && matches!(
                    signal,
                    Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU
                ) =>
        {
            events(ChildEvent::Stopped);
        }
        _ => {}
    }
    Ok(())
}

/// A close-on-exec pipe: read end, write end.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = io::pipe()?;
    Ok((read.into(), write.into()))
}

/// An issued attribution bracket; its identifier is valid only in its owning service.
pub struct TraceScope {
    tracing: Arc<Tracing>,
    id: ScopeId,
    target: Option<Target>,
}

impl TraceScope {
    /// Enters on this thread. The guard cannot move to another thread.
    pub fn enter(&self) -> TraceScopeGuard {
        if self.target.is_some() {
            self.tracing.mark(self.id, true);
        }
        let service = Arc::as_ptr(&self.tracing) as usize;
        ENTERED.with_borrow_mut(|entered| entered.push((service, self.id, self.target)));
        TraceScopeGuard {
            tracing: Arc::clone(&self.tracing),
            id: self.id,
            workload: self.target.is_some(),
            thread: PhantomData,
        }
    }
}

/// A thread-bound scope exit guard. Never hold across an await.
pub struct TraceScopeGuard {
    tracing: Arc<Tracing>,
    id: ScopeId,
    workload: bool,
    thread: PhantomData<Rc<()>>,
}

impl Drop for TraceScopeGuard {
    fn drop(&mut self) {
        let popped = ENTERED.with_borrow_mut(Vec::pop);
        if popped.map(|(_, id, _)| id) != Some(self.id) {
            self.tracing.fail("non-LIFO trace scope");
        }
        if self.workload {
            self.tracing.mark(self.id, false);
        }
    }
}

/// A per-poll context whose guard is entered for one poll and dropped before it returns.
pub trait PollScope {
    /// The thread-bound exit guard; it is never held across a suspension.
    type Guard;
    /// Enters this scope on the polling thread.
    fn enter(&self) -> Self::Guard;
}

impl PollScope for TraceScope {
    type Guard = TraceScopeGuard;
    fn enter(&self) -> TraceScopeGuard {
        Self::enter(self)
    }
}

/// A future whose individual polls, not its suspended intervals, belong to a scope.
pub struct Scoped<F, S> {
    inner: F,
    scope: S,
}

impl<F: Future, S: PollScope> Scoped<F, S> {
    /// Wraps one future in an already-issued scope.
    pub const fn new(inner: F, scope: S) -> Self {
        Self { inner, scope }
    }
}

impl<F: Future, S: PollScope> Future for Scoped<F, S> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: `inner` is structurally pinned and never moved by this implementation;
        // `scope` is only borrowed.
        let this = unsafe { self.get_unchecked_mut() };
        let _scope = this.scope.enter();
        // SAFETY: projection preserves the pin on `inner` for the lifetime of `Self`.
        unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(context)
    }
}

/// Opens a pidfd pinning the identity of live process `pid`, so a later signal cannot reach a
/// recycled PID.
///
/// # Errors
/// Fails when the process no longer exists or the kernel refuses a pidfd.
pub fn open_process(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open accepts two integer arguments and accesses no pointer.
    let fd = i32::try_from(Errno::result(unsafe {
        libc::syscall(libc::SYS_pidfd_open, pid, 0)
    })?)
    .map_err(io::Error::other)?;
    // SAFETY: the syscall returned a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A pidfd for `pid`, or `None` once it has vanished.
fn alive_process(pid: i32) -> io::Result<Option<OwnedFd>> {
    match open_process(pid) {
        Ok(descriptor) => Ok(Some(descriptor)),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Sends `signal` through a pidfd; answers whether the process was still alive to receive it.
///
/// # Errors
/// Fails for any refusal other than the process having already exited.
pub fn signal_process(process: &OwnedFd, signal: i32) -> io::Result<bool> {
    // SAFETY: the descriptor pins process identity; a null siginfo requests the standard signal.
    Ok(alive(Errno::result(unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            process.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    }))?
    .is_some())
}

#[cfg(test)]
mod tests;
