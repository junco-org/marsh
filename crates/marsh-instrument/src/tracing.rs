//! Shell attribution over in-process native observation. No syscall decoder lives here.
//!
//! Linux lets a process trace its own descendants but never its own threads. Every command a
//! managed run spawns is therefore seized, before it can run anything of its own, by a tracer
//! thread of this process that owns exactly that command's process tree; the interpreter's own
//! filesystem accesses arrive instead as host records it reports right after making them. Both
//! are attributed through the scopes the host enters on its threads.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

use crate::Syscall;
use crate::capture::proc_field;
use crate::host::{HostCall, records};
use crate::observation::{Observer, Resume, Sequence, UNCLASSIFIED, alive, creation, rekey};
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
    static ENTERED: RefCell<Vec<(usize, ScopeId, Option<TraceRun>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Opaque registration identity. Never reused within a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootId(u64);

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

struct Root {
    path: PathBuf,
    observe: Arc<dyn Fn(TraceRun, Syscall) -> io::Result<()> + Send + Sync>,
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
    inherited: Option<TraceRun>,
    known_parent: bool,
    process: Option<ProcessLease>,
    pending: Option<(u64, Option<TraceRun>)>,
    classify_after: Option<u64>,
    early: Vec<Early>,
    /// Whether the task's last stop was a job-control (group) stop.
    job_stopped: bool,
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
    threads: HashMap<Pid, Thread>,
    /// Records waiting for their creator's call, by its entry: attribution and record.
    deferred: HashMap<u64, Vec<(TraceRun, Syscall)>>,
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

    /// Notes that `tid` began exiting.
    ///
    /// # Errors
    /// A creator that exits inside a creating call whose child is already observed leaves that
    /// child's records unclassifiable; outside a cancelled run that fails observation.
    fn retire(&mut self, tid: Pid) -> io::Result<()> {
        let Some(thread) = self.threads.get(&tid) else {
            return Ok(());
        };
        let cancelled = self.cancelled(thread.inherited);
        let Some((entry, _)) = thread.pending else {
            return Ok(());
        };
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
        Ok(())
    }

    /// Whether an attributed run has been cancelled.
    fn cancelled(&self, run: Option<TraceRun>) -> bool {
        run.is_some_and(|run| self.runs.get(&run).is_some_and(|run| run.cancelled))
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

    /// Registers a physical work root and its native observation consumer.
    pub fn register_root(
        &self,
        root: &Path,
        observe: Arc<dyn Fn(TraceRun, Syscall) -> io::Result<()> + Send + Sync>,
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
    pub fn scope(self: &Arc<Self>, run: TraceRun) -> io::Result<TraceScope> {
        let mut state = lock(&self.state);
        state.run(run)?;
        let id = ScopeId(state.next()?);
        drop(state);
        Ok(TraceScope {
            tracing: Arc::clone(self),
            id,
            target: Some(run),
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

    /// Allocates the run's next entry order from the sequence its syscall records use.
    pub fn next_order(&self, run: TraceRun) -> io::Result<u64> {
        lock(&self.state).run(run)?;
        self.sequence.next()
    }

    /// Waits for all inherited producers and their pending calls to end.
    pub fn quiesce(&self, run: TraceRun) -> io::Result<()> {
        let mut state = lock(&self.state);
        loop {
            state.run(run)?;
            let busy = state.threads.values().any(|thread| {
                thread.inherited == Some(run)
                    || thread
                        .pending
                        .is_some_and(|(_, target)| target == Some(run))
            }) || state
                .deferred
                .values()
                .flatten()
                .any(|(owner, _)| *owner == run);
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
            thread.inherited == Some(run) && (signal != libc::SIGCONT || thread.job_stopped)
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
        lock(&self.state).runs.remove(&run);
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
        let Some(run) = self.current() else {
            return Ok(());
        };
        lock(&self.state).run(run)?;
        for record in records(call, nix::unistd::gettid(), &self.sequence)? {
            self.classify(run, record)?;
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
        let run = self
            .current()
            .ok_or_else(|| io::Error::other("a managed command spawned outside its run's scope"))?;
        lock(&self.state).run(run)?;
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
            .spawn(move || {
                tracing.trace(request_read, release_write, &pinned, run, host, events)
            })?;
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
    fn current(&self) -> Option<TraceRun> {
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
        run: TraceRun,
        host: Pid,
        mut events: Box<dyn FnMut(ChildEvent) + Send>,
    ) -> io::Result<()> {
        let mut pid = [0; 4];
        // End of file: the launch failed before its handshake.
        if std::fs::File::from(request).read_exact(&mut pid).is_err() {
            return Ok(());
        }
        let root = Pid::from_raw(i32::from_ne_bytes(pid));
        let mut observer = Observer::new(self.selected.clone(), Arc::clone(&self.sequence), root);
        let result = (|| -> io::Result<()> {
            let mut observe = |sequence, tid: Pid, status, event, info| {
                let resume = self.deliver(sequence, tid, status, event, info)?;
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
            self.adopt_root(root, run, host, process)?;
            observer.stop(root, status, &mut observe)?;
            std::fs::File::from(release).write_all(&[1])?;
            observer.run(&mut observe)
        })();
        if observer.detached() {
            lock(&self.state).threads.remove(&root);
            self.progress.notify_all();
        }
        if let Err(error) = &result {
            let mut state = lock(&self.state);
            if state.cancelled(Some(run)) && error.to_string() == UNCLASSIFIED {
                // A cancelled run publishes nothing: what it left unordered is only dropped.
                for records in state.deferred.values_mut() {
                    records.retain(|(owner, _)| *owner != run);
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
            self.progress.notify_all();
        }
        result
    }

    /// Registers a seized command under the scope that spawned it, and hands its classifier
    /// the fork that created it, so its inherited descriptors are the host's.
    fn adopt_root(&self, root: Pid, run: TraceRun, host: Pid, process: OwnedFd) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.run(run)?;
        let cancelled = state.cancelled(Some(run));
        let thread = state.threads.entry(root).or_default();
        thread.inherited = Some(run);
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
        self.classify(run, fork)
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

    fn deliver(
        &self,
        sequence: u64,
        tid: Pid,
        status: i32,
        event: Option<u64>,
        info: Option<Syscall>,
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
            rekey(&mut state.threads, tid, former);
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
        if let WaitStatus::PtraceEvent(_, _, code) = wait
            && creation(code)
            && event.is_some()
        {
            let child = Self::event_thread(event, "missing child identity")?;
            let early = Self::adopt(&mut state, tid, child)?;
            drop(state);
            let mut held = false;
            for (sequence, status, event, info) in early {
                let replay = self.deliver(sequence, child, status, event, info)?;
                // The child was held at its first stop; it goes on unless its replay holds it.
                held = !ended(status) && !replay.hold_current;
                resume.release.extend(replay.release);
            }
            if held {
                resume.release.push(child);
            }
            self.progress.notify_all();
            return Ok(resume);
        }
        if let Some(thread) = state.threads.get_mut(&tid)
            && !thread.known_parent
        {
            // Nothing it does may run before its owner is known: its records could not be
            // attributed.
            resume.hold_current = !ended(status);
            thread.early.push((sequence, status, event, info));
            return Ok(resume);
        }
        if matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) {
            state.retire(tid)?;
            state.threads.remove(&tid);
            drop(state);
            self.progress.notify_all();
            return Ok(resume);
        }
        if matches!(wait, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXIT as i32)
        {
            state.retire(tid)?;
        }
        let Some(info) = info else {
            Self::note_stop(&mut state, tid, wait, sequence)?;
            return Ok(resume);
        };
        let (entry, run) = state
            .threads
            .get_mut(&tid)
            .and_then(|thread| thread.pending.take())
            .ok_or_else(|| io::Error::other("native completion lacks attribution entry"))?;
        if entry != info.entry_order {
            return Err(io::Error::other("native completion entry mismatch"));
        }
        drop(state);
        if let Some(run) = run {
            self.classify(run, info)?;
        }
        self.progress.notify_all();
        Ok(resume)
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

    /// Registers a created thread under its creator's attribution, returning its early stops.
    fn adopt(state: &mut State, tid: Pid, child: Pid) -> io::Result<Vec<Early>> {
        let parent = state.threads.get(&tid);
        let parent_entry = parent.and_then(|thread| thread.pending);
        let inherited = parent_entry
            .and_then(|(_, run)| run)
            .or_else(|| parent.and_then(|thread| thread.inherited));
        let cancelled = state.cancelled(inherited);
        let thread = state.threads.entry(child).or_default();
        thread.known_parent = true;
        thread.inherited = inherited;
        if cancelled {
            thread.kill()?;
        }
        if inherited.is_some() {
            thread.classify_after = parent_entry.map(|(entry, _)| entry);
        }
        Ok(std::mem::take(&mut thread.early))
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

    fn classify(&self, run: TraceRun, info: Syscall) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some(entry) = state
            .threads
            .get(&info.info.pid)
            .and_then(|thread| thread.classify_after)
        {
            state.deferred.entry(entry).or_default().push((run, info));
            return Ok(());
        }
        let created = matches!(
            info.info.syscall,
            Sysno::clone | Sysno::clone3 | Sysno::fork | Sysno::vfork
        )
        .then_some(info.entry_order);
        let observe = state
            .roots
            .get(&run.root)
            .map(|root| Arc::clone(&root.observe));
        drop(state);
        if let Some(observe) = observe {
            observe(run, info)?;
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
            for (run, info) in deferred {
                self.classify(run, info)?;
            }
        }
        Ok(())
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
    target: Option<TraceRun>,
}

impl TraceScope {
    /// Enters on this thread. The guard cannot move to another thread.
    pub fn enter(&self) -> TraceScopeGuard {
        let service = Arc::as_ptr(&self.tracing) as usize;
        ENTERED.with_borrow_mut(|entered| entered.push((service, self.id, self.target)));
        TraceScopeGuard {
            tracing: Arc::clone(&self.tracing),
            id: self.id,
            thread: PhantomData,
        }
    }
}

/// A thread-bound scope exit guard. Never hold across an await.
pub struct TraceScopeGuard {
    tracing: Arc<Tracing>,
    id: ScopeId,
    thread: PhantomData<Rc<()>>,
}

impl Drop for TraceScopeGuard {
    fn drop(&mut self) {
        let popped = ENTERED.with_borrow_mut(Vec::pop);
        if popped.map(|(_, id, _)| id) != Some(self.id) {
            self.tracing.fail("non-LIFO trace scope");
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
