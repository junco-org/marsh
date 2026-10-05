//! Seized-only observation of spawned descendants around lurk's native types, argument table and
//! filter. It runs on the in-process tracer thread that seized them: the host itself is never
//! traced, which Linux refuses within one thread group anyway. No CLI, renderer, tracee launcher,
//! or dependency source patch participates here.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lurk_cli::args::Args;
use lurk_cli::syscall_info::RetCode;
use nix::errno::Errno;
use nix::sys::ptrace::{self, Event, Options};
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use syscalls::{Sysno, SysnoSet};

use crate::Syscall;
use crate::capture::{Captured, native_usize};
use crate::ring::Rings;
use crate::tracing::ExecCommand;

#[cfg(target_arch = "x86_64")]
const NATIVE_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const NATIVE_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "riscv64")]
const NATIVE_ARCH: u32 = 0xC000_00F3;

const OPTIONS: Options = Options::PTRACE_O_TRACESYSGOOD
    .union(Options::PTRACE_O_TRACEFORK)
    .union(Options::PTRACE_O_TRACEVFORK)
    .union(Options::PTRACE_O_TRACECLONE)
    .union(Options::PTRACE_O_TRACEEXEC)
    .union(Options::PTRACE_O_TRACEEXIT);

/// What a delivered stop asks of the observer, after the consumer has seen it.
#[derive(Debug, Default)]
pub(crate) struct Resume {
    /// Keep the delivered task stopped until a later delivery releases it.
    pub hold_current: bool,
    /// Held tasks to resume now, each from the stop it was held at.
    pub release: Vec<Pid>,
}

/// A stop as the consumer sees it: sequence, task, raw status, event message, completed call,
/// and — at a selected exec's `PTRACE_EVENT_EXEC` — what the new image runs.
pub(crate) type Observe<'a> = dyn FnMut(
        u64,
        Pid,
        i32,
        Option<u64>,
        Option<Syscall>,
        Option<io::Result<ExecCommand>>,
    ) -> io::Result<Resume>
    + 'a;

/// Decides at an exec's entry, from the executable it names, whether its arguments are read.
pub(crate) type Select = Box<dyn FnMut(Pid, &Path) -> bool>;

/// Why a tree whose remaining tasks are all held can make no progress.
pub(crate) const UNCLASSIFIED: &str = "native creator exited with unclassified child effects";

/// A lifecycle stop's message and a selected exec's command.
type Lifecycle = (Option<u64>, Option<io::Result<ExecCommand>>);

struct Entry {
    syscall: Sysno,
    capture: Captured,
    order: u64,
    started: Instant,
}

#[derive(Default)]
struct Task {
    /// Whether an entry, or the one return allowed before any, has kept this task in step.
    synchronized: bool,
    in_syscall: bool,
    entry: Option<Entry>,
}

/// The service-wide delivery order shared by every tracer thread and host record.
#[derive(Default)]
pub(crate) struct Sequence(AtomicU64);

impl Sequence {
    pub(crate) fn next(&self) -> io::Result<u64> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|_| io::Error::other("native delivery sequence exhausted"))
    }
}

pub(crate) struct Observer {
    selected: SysnoSet,
    tasks: HashMap<Pid, Task>,
    sequence: Arc<Sequence>,
    /// The spawned command this thread seized.
    root: Pid,
    /// Whether `root` has replaced its launcher image.
    root_executed: bool,
    /// Whether `root` was released untraced after its exec failed.
    detached: bool,
    /// The `io_uring` rings this tree set up.
    rings: Rings,
    /// Stops left unresumed at the consumer's request, by task: their raw status.
    held: HashMap<Pid, i32>,
    /// Which execs have their arguments read; none without one.
    select: Option<Select>,
}

impl Observer {
    /// The observed syscalls: lurk's categories plus the interfaces its 0.3.14 lists predate.
    pub(crate) fn selection() -> io::Result<SysnoSet> {
        let args = Args {
            follow_forks: true,
            expr: vec!["trace=%file,%desc,%process,%memory,%fstat,%fstatfs".into()],
            ..Args::default()
        };
        let mut selected = args
            .create_filter()
            .map_err(io::Error::other)?
            .all_enabled();
        for syscall in [
            Sysno::openat2,
            Sysno::clone3,
            Sysno::close_range,
            Sysno::faccessat2,
            Sysno::fchmodat2,
            Sysno::io_uring_setup,
            Sysno::io_uring_enter,
            Sysno::io_uring_register,
            Sysno::io_submit,
            Sysno::io_getevents,
        ] {
            selected.insert(syscall);
        }
        Ok(selected)
    }

    pub(crate) fn new(selected: SysnoSet, sequence: Arc<Sequence>, root: Pid) -> Self {
        Self {
            selected,
            tasks: HashMap::new(),
            sequence,
            root,
            root_executed: false,
            detached: false,
            rings: Rings::default(),
            held: HashMap::new(),
            select: None,
        }
    }

    /// Reads the arguments, environment and cwd of every exec `select` accepts at its entry.
    pub(crate) fn select_exec(&mut self, select: Select) {
        self.select = Some(select);
    }

    /// Whether the command was handed back untraced because its exec failed: the launcher then
    /// reports that failure and reaps the process itself.
    pub(crate) const fn detached(&self) -> bool {
        self.detached
    }

    /// Seizes a spawned child parked in its launch handshake and stops it. Returns the raw status
    /// of its interrupt stop, still unresumed, or `None` once the child vanished first.
    pub(crate) fn seize(&mut self, pid: Pid) -> io::Result<Option<i32>> {
        ptrace::seize(pid, OPTIONS)
            .map_err(|error| io::Error::other(format!("seizing child {pid}: {error}")))?;
        self.tasks.insert(pid, Task::default());
        // A child that died meanwhile is still reported, and reaped, by the wait below.
        alive(ptrace::interrupt(pid))?;
        loop {
            let mut status = 0;
            // SAFETY: status points to one writable integer; the pid is this thread's tracee.
            let waited = unsafe {
                libc::waitpid(pid.as_raw(), &raw mut status, libc::__WALL | libc::__WNOTHREAD)
            };
            if waited < 0 {
                match Errno::last() {
                    Errno::EINTR => continue,
                    error => return Err(error.into()),
                }
            }
            match WaitStatus::from_raw(pid, status)? {
                WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_STOP as i32 => {
                    return Ok(Some(status));
                }
                WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
                    self.tasks.remove(&pid);
                    return Ok(None);
                }
                // A signal delivered before the interrupt: pass it on; the interrupt stays pending.
                WaitStatus::Stopped(_, signal) => {
                    alive(ptrace::cont(pid, signal))?;
                }
                _ => {
                    alive(ptrace::cont(pid, None))?;
                }
            }
        }
    }

    /// Observes every stop until this thread has no tracee left. A tree whose every remaining
    /// task is held can never be released by any of them, and fails.
    pub(crate) fn run(&mut self, observe: &mut Observe<'_>) -> io::Result<()> {
        loop {
            if !self.held.is_empty() && self.tasks.keys().all(|tid| self.held.contains_key(tid)) {
                return Err(io::Error::other(UNCLASSIFIED));
            }
            let Some((tid, status)) = wait_any()? else {
                break;
            };
            self.stop(tid, status, observe)?;
        }
        if self.tasks.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(
                "native wait ended before every task's final exit",
            ))
        }
    }

    /// After a failure: kills every tracee and resumes whatever still stops, observing nothing,
    /// until none is left. `exited` sees each final status; no tracee is left stopped or unreaped.
    pub(crate) fn abandon(&mut self, mut exited: impl FnMut(Pid, i32)) {
        for tid in self.tasks.keys() {
            let _ = nix::sys::signal::kill(*tid, Signal::SIGKILL);
        }
        // A held task stays stopped until resumed; the kill above ends it there.
        for tid in std::mem::take(&mut self.held).into_keys() {
            let _ = ptrace::cont(tid, None);
        }
        while let Ok(Some((tid, status))) = wait_any() {
            if let Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) =
                WaitStatus::from_raw(tid, status)
            {
                self.tasks.remove(&tid);
                exited(tid, status);
            } else {
                // A task created after the kill above is killed on its first stop.
                if self.tasks.insert(tid, Task::default()).is_none() {
                    let _ = nix::sys::signal::kill(tid, Signal::SIGKILL);
                }
                let _ = ptrace::cont(tid, None);
            }
        }
    }

    /// Applies a lifecycle stop to the task table, returning its message and at an exec what a
    /// selected command runs.
    fn lifecycle(&mut self, tid: Pid, code: i32) -> io::Result<Lifecycle> {
        let message = event_message(tid, code)?;
        let exec = code == Event::PTRACE_EVENT_EXEC as i32;
        if exec && tid == self.root {
            self.root_executed = true;
        }
        // A task killed at this stop has no message left to read; its lifecycle then ends here.
        let other = if (creation(code) || exec) && message.is_some() {
            Some(event_pid(message)?)
        } else {
            None
        };
        match other {
            Some(former) if exec => rekey(&mut self.tasks, tid, former),
            Some(child) => {
                self.tasks.entry(child).or_default();
            }
            None => {}
        }
        let task = self.tasks.entry(tid).or_default();
        if code == Event::PTRACE_EVENT_EXIT as i32 {
            // exit/exit_group never return. A killed or exec-displaced call is also
            // retired here, without manufacturing a successful kernel result.
            task.entry = None;
            task.in_syscall = false;
        }
        // The exec succeeded: the new image has not run an instruction yet.
        let command = if exec {
            task.entry.as_mut().and_then(|entry| {
                let order = entry.order;
                entry.capture.take_exec().map(|image| {
                    image.map(|image| ExecCommand {
                        pid: tid.as_raw(),
                        entry_order: order,
                        program: image.program,
                        argv: image.argv,
                        environment: image.environment,
                        cwd: image.cwd,
                    })
                })
            })
        } else {
            None
        };
        Ok((message, command))
    }

    /// Delivers one stop, then resumes its task unless the consumer holds it.
    pub(crate) fn stop(
        &mut self,
        tid: Pid,
        status: i32,
        observe: &mut Observe<'_>,
    ) -> io::Result<()> {
        let wait = WaitStatus::from_raw(tid, status)?;
        let (event, command) = match wait {
            WaitStatus::PtraceSyscall(_) => return self.syscall_stop(tid, status, observe),
            WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
                self.tasks.remove(&tid);
                self.held.remove(&tid);
                (None, None)
            }
            WaitStatus::PtraceEvent(_, _, code) => self.lifecycle(tid, code)?,
            WaitStatus::Stopped(..) => {
                self.tasks.entry(tid).or_default();
                (None, None)
            }
            WaitStatus::Continued(_) | WaitStatus::StillAlive => return Ok(()),
        };
        let resume = observe(self.sequence.next()?, tid, status, event, None, command)?;
        self.apply(tid, status, resume)
    }

    /// Resumes the delivered task — or holds it — and every held task the consumer released.
    /// A released task resumes from its own saved stop; nothing about that stop is delivered
    /// again.
    fn apply(&mut self, tid: Pid, status: i32, resume: Resume) -> io::Result<()> {
        let wait = WaitStatus::from_raw(tid, status)?;
        if resume.hold_current && !matches!(wait, WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        {
            self.held.insert(tid, status);
        } else {
            resume_stop(tid, wait)?;
        }
        for released in resume.release {
            if let Some(status) = self.held.remove(&released) {
                resume_stop(released, WaitStatus::from_raw(released, status)?)?;
            }
        }
        Ok(())
    }

    fn syscall_stop(&mut self, tid: Pid, status: i32, observe: &mut Observe<'_>) -> io::Result<()> {
        let Some(info) = syscall_info(tid)? else {
            return Ok(());
        };
        if info.arch != NATIVE_ARCH {
            return Err(io::Error::other(format!(
                "task {tid} uses unsupported syscall ABI {:#x}",
                info.arch
            )));
        }
        match info.op {
            libc::PTRACE_SYSCALL_INFO_ENTRY => {
                // SAFETY: the kernel tagged this as the entry union member.
                let number = unsafe { info.u.entry.nr };
                #[cfg(target_arch = "x86_64")]
                if number & 0x4000_0000 != 0 {
                    return Err(io::Error::other("x32 syscalls cannot be observed natively"));
                }
                self.entry(tid, status, number, observe)
            }
            libc::PTRACE_SYSCALL_INFO_EXIT => {
                // SAFETY: the kernel tagged this as the exit union member.
                let exit = unsafe { info.u.exit };
                let result = if exit.is_error != 0 {
                    RetCode::Err(i32::try_from(exit.sval).map_err(io::Error::other)?)
                } else if let Ok(value) = i32::try_from(exit.sval)
                    && (0..=0x8000).contains(&value)
                {
                    RetCode::Ok(value)
                } else {
                    // Upstream Address is also a success: preserve all large values, not
                    // only addresses, and never infer errno from a signed-looking bit pattern.
                    RetCode::Address(native_usize(exit.sval.cast_unsigned()))
                };
                self.exit(tid, status, result, observe)
            }
            // The kernel no longer holds this task at a syscall stop (it was killed or woken
            // between the wait and this query): nothing is left to record, only to release.
            libc::PTRACE_SYSCALL_INFO_NONE => resume(tid, None),
            operation => Err(io::Error::other(format!(
                "unexpected syscall stop kind {operation} for {tid}"
            ))),
        }
    }

    fn entry(
        &mut self,
        tid: Pid,
        status: i32,
        number: u64,
        observe: &mut Observe<'_>,
    ) -> io::Result<()> {
        let task = self.tasks.entry(tid).or_default();
        if task.in_syscall {
            return Err(io::Error::other(format!(
                "task {tid} replaced an incomplete syscall"
            )));
        }
        task.in_syscall = true;
        task.synchronized = true;
        let selected = usize::try_from(number)
            .ok()
            .and_then(Sysno::new)
            .filter(|syscall| self.selected.contains(*syscall));
        if let Some(syscall) = selected {
            let Some(registers) = alive(ptrace::getregs(tid))? else {
                return Ok(());
            };
            let mut capture = Captured::at_entry(tid, syscall, registers)?;
            if syscall == Sysno::io_uring_enter {
                capture.submit(self.rings.pending(tid, capture.descriptor(0)));
            }
            if let Some(select) = &mut self.select
                && let Some(program) = capture.exec_path(syscall)
                && select(tid, &program)
            {
                capture.capture_exec(tid, syscall, registers, program);
            }
            let order = self.sequence.next()?;
            self.tasks
                .get_mut(&tid)
                .ok_or_else(|| io::Error::other("missing entry task"))?
                .entry = Some(Entry {
                syscall,
                capture,
                order,
                started: Instant::now(),
            });
            let resume = observe(order, tid, status, None, None, None)?;
            return self.apply(tid, status, resume);
        }
        resume(tid, None)
    }

    fn exit(
        &mut self,
        tid: Pid,
        status: i32,
        result: RetCode,
        observe: &mut Observe<'_>,
    ) -> io::Result<()> {
        let task = self.tasks.entry(tid).or_default();
        if !task.in_syscall {
            if task.synchronized {
                return Err(io::Error::other(format!(
                    "task {tid} returned without an observed entry"
                )));
            }
            // One syscall may predate attachment (or be the child's inherited clone return).
            task.synchronized = true;
            return resume(tid, None);
        }
        task.in_syscall = false;
        if let Some(entry) = task.entry.take() {
            let mut call = entry.capture.complete(
                tid,
                entry.syscall,
                result,
                entry.order,
                entry.started.elapsed(),
            )?;
            if call.info.syscall == Sysno::io_uring_setup
                && !matches!(call.info.result, RetCode::Err(_))
            {
                self.rings.created(tid, &mut call);
            }
            let failed_launch = tid == self.root
                && !self.root_executed
                && matches!(call.info.syscall, Sysno::execve | Sysno::execveat)
                && matches!(call.info.result, RetCode::Err(_));
            let resume = observe(self.sequence.next()?, tid, status, None, Some(call), None)?;
            if failed_launch {
                // The launcher now reports the errno and waits for this process itself. Only
                // the launcher's own code runs from here, which ran untraced before the
                // handshake too; its wait must not race this thread for the remaining stops.
                alive(ptrace::detach(tid, None))?;
                self.tasks.remove(&tid);
                self.detached = true;
                return Ok(());
            }
            return self.apply(tid, status, resume);
        }
        resume(tid, None)
    }
}

fn event_pid(event: Option<u64>) -> io::Result<Pid> {
    let raw = event
        .and_then(|value| i32::try_from(value).ok())
        .filter(|raw| *raw > 0)
        .ok_or_else(|| io::Error::other("missing or invalid ptrace lifecycle identity"))?;
    Ok(Pid::from_raw(raw))
}

pub(crate) const fn creation(event: i32) -> bool {
    event == Event::PTRACE_EVENT_CLONE as i32
        || event == Event::PTRACE_EVENT_FORK as i32
        || event == Event::PTRACE_EVENT_VFORK as i32
}

fn event_message(tid: Pid, event: i32) -> io::Result<Option<u64>> {
    if creation(event)
        || event == Event::PTRACE_EVENT_EXEC as i32
        || event == Event::PTRACE_EVENT_EXIT as i32
    {
        // A killed task leaves its stop at once; ESRCH is its vanishing, not a failure.
        alive(ptrace::getevent(tid)).map(|message| message.map(i64::cast_unsigned))
    } else {
        Ok(None)
    }
}

fn resume(tid: Pid, signal: Option<Signal>) -> io::Result<()> {
    alive(ptrace::syscall(tid, signal)).map(|_| ())
}

/// Treats a vanished task (`ESRCH`) as absent rather than as an observation failure.
pub(crate) fn alive<T>(result: nix::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Errno::ESRCH) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Moves an exec-displaced thread's bookkeeping to the thread that now owns its identity.
pub(crate) fn rekey<V>(map: &mut HashMap<Pid, V>, tid: Pid, former: Pid) {
    if former != tid
        && let Some(value) = map.remove(&former)
    {
        map.insert(tid, value);
    }
}

fn resume_stop(tid: Pid, wait: WaitStatus) -> io::Result<()> {
    match wait {
        WaitStatus::PtraceEvent(_, signal, code)
            if code == Event::PTRACE_EVENT_STOP as i32
                && matches!(
                    signal,
                    Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU
                ) =>
        {
            // SAFETY: PTRACE_LISTEN takes no address or data. Only seized group-stops reach it.
            alive(Errno::result(unsafe {
                libc::ptrace(
                    libc::PTRACE_LISTEN,
                    tid.as_raw(),
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            }))
            .map(|_| ())
        }
        WaitStatus::Stopped(_, signal) => resume(tid, Some(signal)),
        WaitStatus::PtraceEvent(..) | WaitStatus::PtraceSyscall(_) => resume(tid, None),
        _ => Ok(()),
    }
}

/// Reads the current syscall stop, or `None` once its task has vanished.
fn syscall_info(tid: Pid) -> io::Result<Option<libc::ptrace_syscall_info>> {
    let mut info = std::mem::MaybeUninit::<libc::ptrace_syscall_info>::zeroed();
    // SAFETY: unlike nix 0.30.1's syscall_info wrapper, this supplies the actual writable
    // buffer size. Kernel versions without this operation are errors, never a fallback.
    let Some(copied) = alive(Errno::result(unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            tid.as_raw(),
            std::mem::size_of::<libc::ptrace_syscall_info>(),
            info.as_mut_ptr(),
        )
    }))?
    else {
        return Ok(None);
    };
    // SAFETY: the zero-initialized structure and union contain only integer fields.
    let info = unsafe { info.assume_init() };
    let required = match info.op {
        libc::PTRACE_SYSCALL_INFO_ENTRY => 80,
        libc::PTRACE_SYSCALL_INFO_EXIT => 33,
        _ => 24,
    };
    if copied < required {
        return Err(io::Error::other(
            "truncated PTRACE_GET_SYSCALL_INFO response",
        ));
    }
    Ok(Some(info))
}

fn wait_any() -> io::Result<Option<(Pid, i32)>> {
    loop {
        let mut status = 0;
        // SAFETY: status points to one writable integer. `__WNOTHREAD` confines the wait to this
        // tracer thread's own tracees; the host's other children are never touched.
        let tid =
            unsafe { libc::waitpid(-1, &raw mut status, libc::__WALL | libc::__WNOTHREAD) };
        if tid > 0 {
            return Ok(Some((Pid::from_raw(tid), status)));
        }
        match Errno::last() {
            Errno::EINTR => {}
            Errno::ECHILD => return Ok(None),
            error => return Err(error.into()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
