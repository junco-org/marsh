//! Shell attribution over native observation callbacks. No syscall decoder lives here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use crate::Syscall;
use nix::errno::Errno;
use nix::sys::ptrace::Event;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use syscalls::Sysno;

use crate::capture::proc_field;
use crate::helper::{FRAME_LIMIT, PRELUDE};
use crate::observation::{alive, creation, rekey};

const DEADLINE: Duration = Duration::from_secs(10);
static SHARED: Mutex<Weak<Tracing>> = Mutex::new(Weak::new());

/// Opaque registration identity. Never reused within a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootId(u64);

/// Opaque builtin invocation identity, distinct from a root, run or scope marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvocationId(u64);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    run: TraceRun,
    builtin: Option<InvocationId>,
}

struct Root {
    path: PathBuf,
    observe: Arc<dyn Fn(TraceRun, Option<InvocationId>, Syscall) -> io::Result<()> + Send + Sync>,
}

/// A process identity pinned in the helper before its stopped task resumed.
struct ProcessLease {
    descriptor: OwnedFd,
    group: Option<Pid>,
}
impl ProcessLease {
    fn receive(descriptor: OwnedFd) -> io::Result<Self> {
        let group = proc_field::<i32>(
            format!("/proc/self/fdinfo/{}", descriptor.as_raw_fd()),
            "Pid:",
        )?
        .ok_or_else(|| io::Error::other("helper supplied a non-pidfd identity"))?;
        if group == 0 || group < -1 {
            return Err(io::Error::other("invalid pidfd identity"));
        }
        Ok(Self {
            descriptor,
            group: (group > 0).then(|| Pid::from_raw(group)),
        })
    }
}

/// A stop buffered until its thread's creator is known: sequence, status, event and record.
type Early = (u64, i32, Option<u64>, Option<Syscall>);

#[derive(Default)]
struct Thread {
    stack: Vec<(ScopeId, Option<Target>)>,
    inherited: Option<Target>,
    host: bool,
    known_parent: bool,
    process: Option<ProcessLease>,
    pending: Option<(u64, Option<Target>)>,
    classify_after: Option<u64>,
    early: Vec<Early>,
}

impl Thread {
    fn target(&self) -> Option<Target> {
        self.stack
            .last()
            .map_or(self.inherited, |(_, target)| *target)
    }

    /// Kills a non-host producer through its pinned process identity.
    fn kill(&self) -> io::Result<()> {
        if !self.host
            && let Some(process) = &self.process
        {
            signal_process(&process.descriptor, libc::SIGKILL)?;
        }
        Ok(())
    }
}

struct Scope {
    target: Option<Target>,
    first: Option<u64>,
    last: Option<u64>,
    retired: bool,
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
    deferred: HashMap<u64, Vec<(Target, Syscall)>>,
    barriers: HashMap<u64, bool>,
    failure: Option<String>,
}

impl State {
    fn next(&mut self) -> io::Result<u64> {
        self.ids = self
            .ids
            .checked_add(1)
            .ok_or_else(|| io::Error::other("trace ID exhaustion"))?;
        Ok(self.ids)
    }

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

    /// Whether an attribution target's run has been cancelled.
    fn cancelled(&self, target: Option<Target>) -> bool {
        target.is_some_and(|target| self.runs.get(&target.run).is_some_and(|run| run.cancelled))
    }

    /// Forgets a retired internal scope once no thread's stack still holds it.
    fn release(&mut self, id: ScopeId) {
        if !self
            .threads
            .values()
            .any(|thread| thread.stack.iter().any(|(scope, _)| *scope == id))
        {
            self.scopes.remove(&id);
        }
    }
}

/// Locks trace bookkeeping, recovering the guard from a panicked holder.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The monitor thread's startup report: the helper's pinned identity and its release pipe.
type Startup = io::Result<(Arc<OwnedFd>, std::process::ChildStdin)>;

struct Service {
    process: Arc<OwnedFd>,
    socket: UnixStream,
    expected: Arc<AtomicBool>,
    receiver: Option<std::thread::JoinHandle<()>>,
    monitor: Option<std::thread::JoinHandle<()>>,
}

/// The host's shared native observation service; Shell owns every registration and run.
pub struct Tracing {
    lifecycle: Mutex<()>,
    state: Mutex<State>,
    progress: Condvar,
    service: Mutex<Option<Service>>,
    active: AtomicBool,
    prefix: String,
    #[cfg(feature = "testing")]
    internal_records: Mutex<Option<Vec<Syscall>>>,
}

impl Tracing {
    /// Shares the live service, or creates a fresh idle generation after its last owner closes.
    pub fn shared() -> Arc<Self> {
        let mut shared = lock(&SHARED);
        if let Some(tracing) = shared.upgrade() {
            return tracing;
        }
        let tracing = Arc::new(Self {
            lifecycle: Mutex::new(()),
            state: Mutex::new(State::default()),
            progress: Condvar::new(),
            service: Mutex::new(None),
            active: AtomicBool::new(false),
            prefix: format!("/proc/self/marsh-trace/{}-", std::process::id()),
            #[cfg(feature = "testing")]
            internal_records: Mutex::new(None),
        });
        *shared = Arc::downgrade(&tracing);
        tracing
    }

    /// Starts test-only recording of explicitly Internal-scoped native calls.
    ///
    /// # Errors
    /// Refuses overlapping captures on the shared observation service.
    #[cfg(feature = "testing")]
    pub fn begin_internal_capture(&self) -> io::Result<()> {
        let mut records = lock(&self.internal_records);
        if records.is_some() {
            return Err(io::Error::other("internal capture already active"));
        }
        *records = Some(Vec::new());
        drop(records);
        Ok(())
    }

    /// Drains prior native callbacks, disables capture and moves out its records.
    ///
    /// # Errors
    /// Returns trace failures or a missing capture. Production builds expose no capture API.
    #[cfg(feature = "testing")]
    pub fn end_internal_capture(&self) -> io::Result<Vec<Syscall>> {
        self.barrier(DEADLINE)?;
        lock(&self.internal_records)
            .take()
            .ok_or_else(|| io::Error::other("internal capture is not active"))
    }

    /// Registers a physical work root and its native observation consumer.
    pub fn register_root(
        self: &Arc<Self>,
        root: &Path,
        observe: Arc<
            dyn Fn(TraceRun, Option<InvocationId>, Syscall) -> io::Result<()> + Send + Sync,
        >,
    ) -> io::Result<RootId> {
        let _lifecycle = lock(&self.lifecycle);
        let mut state = lock(&self.state);
        state.check()?;
        if state.roots.values().any(|entry| entry.path == root) {
            return Err(io::Error::other("root already registered"));
        }
        let first = state.roots.is_empty();
        let id = RootId(state.next()?);
        state.roots.insert(
            id,
            Root {
                path: root.to_path_buf(),
                observe,
            },
        );
        drop(state);
        if first && let Err(error) = self.start() {
            lock(&self.state).roots.remove(&id);
            let _ = self.stop();
            return Err(error);
        }
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

    /// Allocates a workload scope. Its markers are valid only on host threads.
    pub fn scope(
        self: &Arc<Self>,
        run: TraceRun,
        builtin: Option<InvocationId>,
    ) -> io::Result<TraceScope> {
        lock(&self.state).run(run)?;
        self.new_scope(Some(Target { run, builtin }))
    }

    /// Marks implementation work so it cannot become a command's evidence or producer.
    pub fn internal_scope(self: &Arc<Self>) -> io::Result<TraceScope> {
        self.new_scope(None)
    }

    fn new_scope(self: &Arc<Self>, target: Option<Target>) -> io::Result<TraceScope> {
        let mut state = lock(&self.state);
        state.check()?;
        let id = ScopeId(state.next()?);
        state.scopes.insert(
            id,
            Scope {
                target,
                first: None,
                last: None,
                retired: false,
            },
        );
        drop(state);
        Ok(TraceScope {
            tracing: Arc::clone(self),
            id,
        })
    }

    /// Allocates an invocation identifier from the service's nonwrapping series.
    pub fn invocation(&self, run: TraceRun) -> io::Result<InvocationId> {
        let mut state = lock(&self.state);
        state.run(run)?;
        state.next().map(InvocationId)
    }

    /// Returns native marker orders for one invocation, after its final delivery barrier.
    pub fn invocation_orders(
        &self,
        run: TraceRun,
        builtin: InvocationId,
    ) -> io::Result<(u64, u64)> {
        let state = lock(&self.state);
        state.run(run)?;
        let target = Some(Target {
            run,
            builtin: Some(builtin),
        });
        let scopes = || {
            state
                .scopes
                .values()
                .filter(move |scope| scope.target == target)
        };
        let orders = scopes()
            .filter_map(|scope| scope.first)
            .min()
            .zip(scopes().filter_map(|scope| scope.last).max());
        drop(state);
        orders.ok_or_else(|| io::Error::other("invocation has no completed native scope"))
    }

    /// Proves prior callbacks finished. Producer quiescence is checked separately, never inferred
    /// from a quiet interval or a drain marker.
    pub fn drain(&self, run: TraceRun) -> io::Result<()> {
        lock(&self.state).run(run)?;
        self.barrier(DEADLINE)
    }

    /// Waits for all inherited producers and their pending calls, then proves final delivery.
    pub fn quiesce(&self, run: TraceRun) -> io::Result<()> {
        self.drain(run)?;
        let mut state = lock(&self.state);
        loop {
            state.run(run)?;
            let busy = state.threads.values().any(|thread| {
                run.owns(thread.inherited)
                    || thread.pending.is_some_and(|(_, target)| run.owns(target))
                    || thread.stack.iter().any(|(_, target)| run.owns(*target))
            }) || state
                .deferred
                .values()
                .flatten()
                .any(|(target, _)| target.run == run);
            if !busy {
                break;
            }
            state = self
                .progress
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(state);
        self.drain(run)
    }

    /// Signals verified live descendant processes, never historical PIDs or the host itself.
    pub fn signal(&self, run: TraceRun, signal: i32) -> io::Result<usize> {
        let state = lock(&self.state);
        if !state.runs.contains_key(&run) {
            return Err(io::Error::other("closed trace run"));
        }
        let mut signalled = HashSet::new();
        for thread in state
            .threads
            .values()
            .filter(|thread| !thread.host && run.owns(thread.inherited))
        {
            if let Some(process) = &thread.process
                && let Some(group) = process.group
                && !signalled.contains(&group)
                && signal_process(&process.descriptor, signal)?
            {
                signalled.insert(group);
            }
        }
        drop(state);
        Ok(signalled.len())
    }

    /// Reports loss of the native service even while a consumer callback is blocked.
    pub fn health(&self, run: TraceRun) -> io::Result<()> {
        lock(&self.state).run(run)
    }

    /// Cancels existing descendants and descendants whose creation callbacks are still queued.
    pub fn cancel(&self, run: TraceRun) -> io::Result<usize> {
        if let Some(state) = lock(&self.state).runs.get_mut(&run) {
            state.cancelled = true;
        }
        self.signal(run, libc::SIGKILL)
    }

    /// Registers the owning evaluation for immediate failure notification from the pidfd monitor.
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
        state.scopes.retain(|_, scope| !run.owns(scope.target));
        drop(state);
        Ok(())
    }

    /// Releases a root only after its runs have finished; the last root stops the helper.
    pub fn unregister_root(&self, root: RootId) -> io::Result<()> {
        let _lifecycle = lock(&self.lifecycle);
        let mut state = lock(&self.state);
        if state.runs.keys().any(|run| run.root == root) && state.failure.is_none() {
            return Err(io::Error::other("trace root still owns a run"));
        }
        // A failed service cannot prove reclamation. Its owner retains the private tree, but
        // must still be able to close all roots and start a fresh attachment generation.
        state.runs.retain(|run, _| run.root != root);
        state
            .roots
            .remove(&root)
            .ok_or_else(|| io::Error::other("missing trace root"))?;
        let last = state.roots.is_empty();
        drop(state);
        if last {
            self.stop()?;
        }
        Ok(())
    }

    fn barrier(&self, patience: Duration) -> io::Result<()> {
        let id = {
            let mut state = lock(&self.state);
            state.check()?;
            let id = state.next()?;
            state.barriers.insert(id, false);
            id
        };
        self.mark(id, "barrier");
        let deadline = Instant::now() + patience;
        let mut state = lock(&self.state);
        loop {
            state.check()?;
            if state.barriers.get(&id) == Some(&true) {
                state.barriers.remove(&id);
                return Ok(());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                state.barriers.remove(&id);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native trace barrier timed out",
                ));
            };
            state = self
                .progress
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn mark(&self, id: u64, kind: &str) {
        // Stack-only emission: no allocation or filesystem effect before the observed marker.
        let mut path = [0_u8; 128];
        let _ = write!(&mut path[..127], "{}{id}/{kind}", self.prefix);
        let mut sink = [0_u8];
        // SAFETY: both buffers are live; `path` remains NUL-terminated within its fixed capacity.
        unsafe {
            libc::readlink(path.as_ptr().cast(), sink.as_mut_ptr().cast(), sink.len());
        }
    }

    fn marker(&self, info: &Syscall) -> Option<(u64, &str)> {
        let bytes = match info.info.syscall {
            Sysno::readlink => info.path(0),
            Sysno::readlinkat => info.path(1),
            _ => None,
        }?;
        let suffix = bytes.strip_prefix(self.prefix.as_bytes())?;
        let slash = suffix.iter().position(|byte| *byte == b'/')?;
        let id = std::str::from_utf8(&suffix[..slash]).ok()?.parse().ok()?;
        let kind = ["enter", "leave", "barrier", "retire"]
            .into_iter()
            .find(|kind| kind.as_bytes() == &suffix[slash + 1..])?;
        Some((id, kind))
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
        process: Option<ProcessLease>,
    ) -> io::Result<()> {
        let wait = WaitStatus::from_raw(tid, status).map_err(io::Error::other)?;
        let mut state = lock(&self.state);
        state.check()?;
        if state.runs.is_empty()
            && matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        {
            state.threads.remove(&tid);
            return Ok(());
        }
        if matches!(wait, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXEC as i32)
        {
            let former = Self::event_thread(event, "missing exec identity")?;
            rekey(&mut state.threads, tid, former);
        }
        Self::lease(&mut state, tid, process)?;
        if let WaitStatus::PtraceEvent(_, _, code) = wait
            && creation(code)
        {
            let child = Self::event_thread(event, "missing child identity")?;
            let early = Self::adopt(&mut state, tid, child)?;
            drop(state);
            for (sequence, status, event, info) in early {
                self.deliver(sequence, child, status, event, info, None)?;
            }
            self.progress.notify_all();
            return Ok(());
        }
        if let Some(thread) = state.threads.get_mut(&tid)
            && !thread.known_parent
        {
            thread.early.push((sequence, status, event, info));
            return Ok(());
        }
        if matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) {
            state.threads.remove(&tid);
            self.progress.notify_all();
            return Ok(());
        }
        let Some(info) = info else {
            return Self::note_stop(&mut state, tid, wait, sequence);
        };
        let (entry, target) = state
            .threads
            .get_mut(&tid)
            .and_then(|thread| thread.pending.take())
            .ok_or_else(|| io::Error::other("native completion lacks attribution entry"))?;
        if entry != info.entry_order {
            return Err(io::Error::other("native completion entry mismatch"));
        }
        let host = state.threads.get(&tid).is_some_and(|thread| thread.host);
        if host && let Some((id, kind)) = self.marker(&info) {
            Self::scope_marker(&mut state, tid, id, kind, info.entry_order)?;
            self.progress.notify_all();
            return Ok(());
        }
        #[cfg(feature = "testing")]
        let internal = state.threads.get(&tid).is_some_and(|thread| {
            thread
                .stack
                .last()
                .is_some_and(|(_, target)| target.is_none())
        });
        drop(state);
        if let Some(target) = target {
            self.classify(target, info)?;
        } else {
            #[cfg(feature = "testing")]
            if internal && let Some(records) = lock(&self.internal_records).as_mut() {
                records.push(info);
            }
        }
        self.progress.notify_all();
        Ok(())
    }

    /// Pins a stop's process identity on its thread, killing it when its run was cancelled.
    fn lease(state: &mut State, tid: Pid, process: Option<ProcessLease>) -> io::Result<()> {
        let thread = state.threads.entry(tid).or_default();
        if let Some(process) = process {
            thread.host = process
                .group
                .is_some_and(|group| group.as_raw().cast_unsigned() == std::process::id());
            thread.known_parent |= thread.host;
            thread.process = Some(process);
        }
        let inherited = thread.inherited;
        if state.cancelled(inherited)
            && let Some(thread) = state.threads.get(&tid)
        {
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
            .and_then(|(_, target)| target)
            .or_else(|| parent.and_then(Thread::target));
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
            let target = thread.target();
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

    /// Applies one host scope marker to the barrier, scope and stack bookkeeping.
    fn scope_marker(
        state: &mut State,
        tid: Pid,
        id: u64,
        kind: &str,
        order: u64,
    ) -> io::Result<()> {
        let scope = ScopeId(id);
        match kind {
            "barrier" => {
                if let Some(reached) = state.barriers.get_mut(&id) {
                    *reached = true;
                }
            }
            "enter" => {
                let issued = state
                    .scopes
                    .get_mut(&scope)
                    .ok_or_else(|| io::Error::other("unissued scope marker"))?;
                issued.first.get_or_insert(order);
                let target = issued.target;
                if let Some(thread) = state.threads.get_mut(&tid) {
                    thread.stack.push((scope, target));
                }
            }
            "leave" => {
                let popped = state
                    .threads
                    .get_mut(&tid)
                    .and_then(|thread| thread.stack.pop());
                if popped.map(|(top, _)| top) != Some(scope) {
                    return Err(io::Error::other("non-LIFO native scope"));
                }
                if let Some(left) = state.scopes.get_mut(&scope) {
                    left.last = Some(order);
                }
                if state
                    .scopes
                    .get(&scope)
                    .is_some_and(|left| left.target.is_none() && left.retired)
                {
                    state.release(scope);
                }
            }
            "retire" => {
                if let Some(retired) = state.scopes.get_mut(&scope) {
                    retired.retired = true;
                }
                state.release(scope);
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn classify(&self, target: Target, info: Syscall) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some(entry) = state
            .threads
            .get(&info.info.pid)
            .and_then(|thread| thread.classify_after)
        {
            state
                .deferred
                .entry(entry)
                .or_default()
                .push((target, info));
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
            for (target, info) in deferred {
                self.classify(target, info)?;
            }
        }
        Ok(())
    }

    fn start(self: &Arc<Self>) -> io::Result<()> {
        let helper = helper_path()?;
        let (socket, output) = UnixStream::pair()?;
        let receiver_socket = socket.try_clone()?;
        let expected = Arc::new(AtomicBool::new(false));
        let weak = Arc::downgrade(self);
        let monitor_expected = Arc::clone(&expected);
        let (started, startup) = std::sync::mpsc::sync_channel(1);
        // Linux binds PDEATHSIG to the creating thread, not the whole parent process. The
        // monitor therefore creates the child and stays alive until it has reaped that child;
        // a caller's short-lived Tokio runtime cannot kill another runtime's shared helper.
        let monitor = std::thread::Builder::new()
            .name("marsh-native-monitor".into())
            .spawn(move || {
                Self::launch_helper(&helper, output, &started, &weak, &monitor_expected);
            })?;
        let (process, mut release) = match startup
            .recv()
            .map_err(io::Error::other)
            .and_then(std::convert::identity)
        {
            Ok(resources) => resources,
            Err(error) => {
                let _ = monitor.join();
                return Err(error);
            }
        };
        let weak = Arc::downgrade(self);
        let receiver_expected = Arc::clone(&expected);
        let receiver = std::thread::Builder::new()
            .name("marsh-native-receiver".into())
            .spawn(move || {
                if let Err(error) = receive(&weak, receiver_socket)
                    && !receiver_expected.load(Ordering::Acquire)
                {
                    if let Some(tracing) = weak.upgrade() {
                        tracing.fail(format!("native transport: {error}"));
                    }
                }
            });
        let (receiver, failure) =
            receiver.map_or_else(|error| (None, Some(error)), |thread| (Some(thread), None));
        // Install all owned resources even on receiver failure: register_root's error path
        // stops/reaps the helper, joins the monitor and revokes its ptrace permission.
        *lock(&self.service) = Some(Service {
            process,
            socket,
            expected,
            receiver,
            monitor: Some(monitor),
        });
        if let Some(error) = failure {
            return Err(error);
        }
        release.write_all(&[1])?;
        drop(release);
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.barrier(Duration::from_millis(50)) {
                Ok(()) => {
                    self.active.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut && Instant::now() < deadline => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Spawns and bootstraps the native helper on the monitor thread, then supervises it.
    fn launch_helper(
        helper: &Path,
        output: UnixStream,
        started: &std::sync::mpsc::SyncSender<Startup>,
        weak: &Weak<Self>,
        expected: &AtomicBool,
    ) {
        let mut command = std::process::Command::new(helper);
        command
            .args(["--host-pid", &std::process::id().to_string()])
            .stdin(std::process::Stdio::piped())
            .stdout(OwnedFd::from(output))
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let spawned = command.spawn();
        drop(command);
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let _ = started.send(Err(error));
                return;
            }
        };
        let bootstrap = (|| -> io::Result<_> {
            let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
            let process = Arc::new(open_process(pid)?);
            let release = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing helper release pipe"))?;
            let diagnostics = child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("missing helper diagnostics"))?;
            // SAFETY: PR_SET_PTRACER accepts the owned child's PID and accesses no pointer.
            Errno::result(unsafe { libc::prctl(libc::PR_SET_PTRACER, pid) })?;
            Ok((process, release, diagnostics))
        })();
        let failure = match bootstrap {
            Ok((process, release, diagnostics)) => {
                match started.send(Ok((Arc::clone(&process), release))) {
                    Ok(()) => {
                        return monitor(weak, child, diagnostics, &process, expected);
                    }
                    Err(_) => None,
                }
            }
            Err(error) => Some(error),
        };
        let _ = child.kill();
        let _ = child.wait();
        if let Some(error) = failure {
            let _ = started.send(Err(error));
        }
    }

    fn stop(&self) -> io::Result<()> {
        let service = lock(&self.service).take();
        let Some(mut service) = service else {
            return Ok(());
        };
        self.active.store(false, Ordering::Release);
        service.expected.store(true, Ordering::Release);
        signal_process(&service.process, libc::SIGKILL)?;
        let _ = service.socket.shutdown(std::net::Shutdown::Both);
        let here = std::thread::current().id();
        for thread in [service.receiver.take(), service.monitor.take()]
            .into_iter()
            .flatten()
        {
            if thread.thread().id() != here {
                thread
                    .join()
                    .map_err(|_| io::Error::other("native service thread panicked"))?;
            }
        }
        // SAFETY: zero revokes this process's previous precise-child ptrace permission.
        Errno::result(unsafe { libc::prctl(libc::PR_SET_PTRACER, 0) })?;
        let mut state = lock(&self.state);
        state.threads.clear();
        state.deferred.clear();
        state.scopes.clear();
        state.barriers.clear();
        state.failure = None;
        drop(state);
        Ok(())
    }
}

impl Drop for Tracing {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// An issued attribution bracket; its identifier is valid only in its owning service.
pub struct TraceScope {
    tracing: Arc<Tracing>,
    id: ScopeId,
}

impl TraceScope {
    /// Enters on this thread. The guard cannot move to another thread.
    pub fn enter(&self) -> TraceScopeGuard {
        let enabled = self.tracing.active.load(Ordering::Acquire);
        if enabled {
            self.tracing.mark(self.id.0, "enter");
        }
        TraceScopeGuard {
            tracing: Arc::clone(&self.tracing),
            id: self.id,
            thread: PhantomData,
            enabled,
        }
    }
}
impl Drop for TraceScope {
    fn drop(&mut self) {
        let internal = lock(&self.tracing.state)
            .scopes
            .get(&self.id)
            .is_some_and(|scope| scope.target.is_none());
        if !internal {
            return;
        }
        if self.tracing.active.load(Ordering::Acquire) {
            self.tracing.mark(self.id.0, "retire");
        } else {
            lock(&self.tracing.state).scopes.remove(&self.id);
        }
    }
}

/// A thread-bound scope exit guard. Never hold across an await.
pub struct TraceScopeGuard {
    tracing: Arc<Tracing>,
    id: ScopeId,
    thread: PhantomData<Rc<()>>,
    enabled: bool,
}

impl Drop for TraceScopeGuard {
    fn drop(&mut self) {
        if self.enabled && lock(&self.tracing.state).scopes.contains_key(&self.id) {
            self.tracing.mark(self.id.0, "leave");
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

fn helper_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .ok_or_else(|| io::Error::other("executable has no parent"))?;
    let direct = parent.join("marsh-trace");
    if direct.try_exists()? {
        return Ok(direct);
    }
    if parent
        .file_name()
        .is_some_and(|name| name == "deps" || name == "examples")
    {
        let sibling = parent
            .parent()
            .ok_or_else(|| io::Error::other("missing cargo output parent"))?
            .join("marsh-trace");
        if sibling.try_exists()? {
            return Ok(sibling);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "build and bundle marsh-trace beside the application",
    ))
}

/// Delivers the helper's frames until the service is gone or the helper closes the stream.
///
/// A stream that closes before the prelude or between frames is no transport failure: the helper
/// closes it only as it ends, and the pidfd monitor reports that end with the exit status and
/// diagnostics this stream cannot carry. A stream that closes inside a frame was truncated.
fn receive(tracing: &Weak<Tracing>, socket: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(NativeSocket {
        socket,
        descriptors: VecDeque::new(),
    });
    if reader.fill_buf()?.is_empty() {
        return Ok(());
    }
    let mut prelude = [0; PRELUDE.len()];
    reader.read_exact(&mut prelude)?;
    if prelude != PRELUDE {
        return Err(io::Error::other("incompatible native helper"));
    }
    let mut frame = Vec::new();
    let mut expected = 1_u64;
    loop {
        frame.clear();
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                if frame.is_empty() {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "native stream ended inside a frame",
                ));
            }
            let end = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1);
            let count = end.unwrap_or(buffer.len());
            if frame.len() + count > FRAME_LIMIT {
                return Err(io::Error::other("oversized native frame"));
            }
            frame.extend_from_slice(&buffer[..count]);
            reader.consume(count);
            if end.is_some() {
                break;
            }
        }
        let (sequence, tid, status, event, info): (u64, i32, i32, Option<u64>, Option<Syscall>) =
            serde_json::from_slice(&frame)?;
        if sequence != expected || tid <= 0 {
            return Err(io::Error::other("native sequence gap or invalid tid"));
        }
        let tid = Pid::from_raw(tid);
        if info.as_ref().is_some_and(|info| info.info.pid != tid) {
            return Err(io::Error::other("native record tid mismatch"));
        }
        let wait = WaitStatus::from_raw(tid, status).map_err(io::Error::other)?;
        let process = crate::helper::carries_process(&wait)
            .then(|| {
                reader
                    .get_mut()
                    .descriptors
                    .pop_front()
                    .ok_or_else(|| {
                        io::Error::other("native lifecycle frame lacks its pinned process handle")
                    })
                    .and_then(ProcessLease::receive)
            })
            .transpose()?;
        expected = expected
            .checked_add(1)
            .ok_or_else(|| io::Error::other("native sequence exhausted"))?;
        let Some(tracing) = tracing.upgrade() else {
            return Ok(());
        };
        tracing.deliver(sequence, tid, status, event, info, process)?;
    }
}

pub(crate) fn open_process(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open accepts two integer arguments and accesses no pointer.
    let fd = i32::try_from(Errno::result(unsafe {
        libc::syscall(libc::SYS_pidfd_open, pid, 0)
    })?)
    .map_err(io::Error::other)?;
    // SAFETY: the syscall returned a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal_process(process: &OwnedFd, signal: i32) -> io::Result<bool> {
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

/// Reads bytes and their `SCM_RIGHTS` together. Buffering plain recv calls would silently discard
/// the pinned identities. Native JSON framing stays in `receive`, not in a second codec.
struct NativeSocket {
    socket: UnixStream,
    descriptors: VecDeque<OwnedFd>,
}
impl Read for NativeSocket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use rustix::net::{
            RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, recvmsg,
        };
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(253))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let received = rustix::io::retry_on_intr(|| {
            recvmsg(
                &self.socket,
                &mut [io::IoSliceMut::new(buffer)],
                &mut ancillary,
                RecvFlags::CMSG_CLOEXEC,
            )
        })?;
        if received.flags.contains(ReturnFlags::CTRUNC) {
            return Err(io::Error::other("native pidfd transport truncated"));
        }
        for message in ancillary.drain() {
            match message {
                RecvAncillaryMessage::ScmRights(descriptors) => {
                    self.descriptors.extend(descriptors);
                }
                _ => return Err(io::Error::other("unexpected native ancillary message")),
            }
        }
        Ok(received.bytes)
    }
}

fn monitor(
    tracing: &Weak<Tracing>,
    mut child: std::process::Child,
    mut stderr: std::process::ChildStderr,
    process: &OwnedFd,
    expected: &AtomicBool,
) {
    let mut diagnostics = Vec::new();
    let mut eof = false;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if eof { -1 } else { stderr.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: the array contains exactly two initialized pollfd structures.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if let Some(tracing) = tracing.upgrade() {
                tracing.fail("native pidfd monitor failed");
            }
            let _ = signal_process(process, libc::SIGKILL);
            break;
        }
        if fds[1].revents != 0 {
            let mut buffer = [0; 4096];
            match stderr.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    let keep = count.min(4096 - diagnostics.len());
                    diagnostics.extend_from_slice(&buffer[..keep]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => eof = true,
            }
        }
        if fds[0].revents != 0 {
            break;
        }
    }
    let status = child.wait();
    if !expected.load(Ordering::Acquire)
        && let Some(tracing) = tracing.upgrade()
    {
        tracing.fail(format!(
            "native helper exited ({status:?}): {}",
            String::from_utf8_lossy(&diagnostics)
        ));
    }
}

#[cfg(test)]
mod tests;
