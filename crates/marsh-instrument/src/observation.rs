//! Seized-only observation of spawned descendants around lurk's native types, argument table and
//! filter. It runs on the in-process tracer thread that seized them: the host itself is never
//! traced, which Linux refuses within one thread group anyway. No CLI, renderer, tracee launcher,
//! or dependency source patch participates here.

use std::collections::HashMap;
use std::io;
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

type Observe<'a> = dyn FnMut(u64, Pid, i32, Option<u64>, Option<Syscall>) -> io::Result<()> + 'a;

struct Entry {
    syscall: Sysno,
    capture: Captured,
    order: u64,
    started: Instant,
}

#[derive(Default)]
struct Task {
    seen_entry: bool,
    discarded_initial_exit: bool,
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
        }
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

    /// Observes every stop until this thread has no tracee left.
    pub(crate) fn run(&mut self, observe: &mut Observe<'_>) -> io::Result<()> {
        while let Some((tid, status)) = wait_any()? {
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

    /// Applies a lifecycle stop to the task table, returning its message and any created or
    /// exec-displaced task.
    fn lifecycle(&mut self, tid: Pid, code: i32) -> io::Result<(Option<u64>, Option<Pid>)> {
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
        Ok((message, other))
    }

    /// Delivers one stop, then resumes its task.
    pub(crate) fn stop(
        &mut self,
        tid: Pid,
        status: i32,
        observe: &mut Observe<'_>,
    ) -> io::Result<()> {
        let wait = WaitStatus::from_raw(tid, status)?;
        let event = match wait {
            WaitStatus::PtraceSyscall(_) => return self.syscall_stop(tid, status, observe),
            WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
                self.tasks.remove(&tid);
                None
            }
            WaitStatus::PtraceEvent(_, _, code) => self.lifecycle(tid, code)?.0,
            WaitStatus::Stopped(..) => {
                self.tasks.entry(tid).or_default();
                None
            }
            WaitStatus::Continued(_) | WaitStatus::StillAlive => return Ok(()),
        };
        observe(self.sequence.next()?, tid, status, event, None)?;
        resume_stop(tid, wait)
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
        task.seen_entry = true;
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
            observe(order, tid, status, None, None)?;
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
            if task.seen_entry || task.discarded_initial_exit {
                return Err(io::Error::other(format!(
                    "task {tid} returned without an observed entry"
                )));
            }
            // One syscall may predate attachment (or be the child's inherited clone return).
            task.discarded_initial_exit = true;
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
            observe(self.sequence.next()?, tid, status, None, Some(call))?;
            if failed_launch {
                // The launcher now reports the errno and waits for this process itself. Only
                // the launcher's own code runs from here, which ran untraced before the
                // handshake too; its wait must not race this thread for the remaining stops.
                alive(ptrace::detach(tid, None))?;
                self.tasks.remove(&tid);
                self.detached = true;
                return Ok(());
            }
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
