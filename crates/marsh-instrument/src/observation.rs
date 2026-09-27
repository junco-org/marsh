//! Seized-only observation around lurk's native types, argument table and filter.
//! No CLI, renderer, tracee launcher, or dependency source patch participates here.

use std::collections::{HashMap, HashSet};
use std::io;
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
use crate::capture::{Captured, native_usize, proc_field};

#[cfg(target_arch = "x86_64")]
const NATIVE_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const NATIVE_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "riscv64")]
const NATIVE_ARCH: u32 = 0xC000_00F3;

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

pub(crate) struct Observer {
    selected: SysnoSet,
    tasks: HashMap<Pid, Task>,
    initial: Vec<(Pid, i32, Option<u64>)>,
    sequence: u64,
}

impl Observer {
    pub(crate) fn attach(root: Pid, args: &Args) -> io::Result<Self> {
        let mut selected = args
            .create_filter()
            .map_err(io::Error::other)?
            .all_enabled();
        // Upstream 0.3.14's category lists predate these filesystem/process interfaces.
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
        let mut observer = Self {
            selected,
            tasks: HashMap::new(),
            initial: Vec::new(),
            sequence: 0,
        };
        observer.seize_all(root)?;
        Ok(observer)
    }

    fn next(&mut self) -> io::Result<u64> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("native delivery sequence exhausted"))?;
        Ok(self.sequence)
    }

    /// Stop the entire host before admission, including threads auto-attached during enumeration.
    /// No initial task resumes until all retained real stops have been delivered.
    fn seize_all(&mut self, root: Pid) -> io::Result<()> {
        let options = Options::PTRACE_O_TRACESYSGOOD
            | Options::PTRACE_O_TRACEFORK
            | Options::PTRACE_O_TRACEVFORK
            | Options::PTRACE_O_TRACECLONE
            | Options::PTRACE_O_TRACEEXEC
            | Options::PTRACE_O_TRACEEXIT;
        let mut waiting = HashSet::new();
        let mut stopped = HashSet::new();
        loop {
            for tid in list_tasks(root)? {
                let tid = tid?;
                if !self.tasks.contains_key(&tid) && self.seize(tid, options)? {
                    waiting.insert(tid);
                }
            }
            if waiting.is_empty() {
                return Ok(());
            }
            while !waiting.is_empty() {
                let (tid, status) = wait_any()?
                    .ok_or_else(|| io::Error::other("host attachment lost its tracees"))?;
                let wait = WaitStatus::from_raw(tid, status)?;
                let event = match wait {
                    WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
                        self.tasks.remove(&tid);
                        None
                    }
                    WaitStatus::PtraceEvent(_, _, code) => {
                        let (message, other) = self.lifecycle(tid, code)?;
                        match other {
                            Some(child) if creation(code) => {
                                if !stopped.contains(&child) {
                                    waiting.insert(child);
                                }
                            }
                            Some(former) => {
                                waiting.remove(&former);
                            }
                            None => {}
                        }
                        message
                    }
                    WaitStatus::Continued(_) | WaitStatus::StillAlive => continue,
                    _ => {
                        self.tasks.entry(tid).or_default();
                        None
                    }
                };
                waiting.remove(&tid);
                stopped.insert(tid);
                self.initial.push((tid, status, event));
            }
        }
    }

    /// Seizes and interrupts one listed host task, reporting whether it now owes an initial stop.
    fn seize(&mut self, tid: Pid, options: Options) -> io::Result<bool> {
        match ptrace::seize(tid, options) {
            Ok(()) => match ptrace::interrupt(tid) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "interrupting host task {tid}: {error}"
                    )));
                }
            },
            // A concurrently cloned thread is already ours, and has an initial stop.
            Err(Errno::EPERM) if traced_by_self(tid)? => {}
            // A task can exit between its listing and its seizure: it is no longer part of the
            // host, and any thread it created before exiting appears in the next listing.
            Err(Errno::ESRCH) => return Ok(false),
            Err(error) => {
                return Err(io::Error::other(format!(
                    "seizing host task {tid}: {error}"
                )));
            }
        }
        self.tasks.insert(tid, Task::default());
        Ok(true)
    }

    pub(crate) fn run(
        &mut self,
        mut observe: impl FnMut(u64, Pid, i32, Option<u64>, Option<Syscall>) -> io::Result<()>,
    ) -> io::Result<()> {
        let initial = std::mem::take(&mut self.initial);
        for &(tid, status, event) in &initial {
            observe(self.next()?, tid, status, event, None)?;
        }
        for (tid, status, _) in initial {
            resume_stop(tid, WaitStatus::from_raw(tid, status)?)?;
        }
        while let Some((tid, status)) = wait_any()? {
            self.stop(tid, status, &mut observe)?;
        }
        if self.tasks.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(
                "native wait ended before every task's final exit",
            ))
        }
    }

    /// Applies a lifecycle stop to the task table, returning its message and any created or
    /// exec-displaced task.
    fn lifecycle(&mut self, tid: Pid, code: i32) -> io::Result<(Option<u64>, Option<Pid>)> {
        let message = event_message(tid, code)?;
        let exec = code == Event::PTRACE_EVENT_EXEC as i32;
        let other = if creation(code) || exec {
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

    fn stop(&mut self, tid: Pid, status: i32, observe: &mut Observe<'_>) -> io::Result<()> {
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
        observe(self.next()?, tid, status, event, None)?;
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
            let capture = Captured::at_entry(tid, syscall, registers)?;
            let order = self.next()?;
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
            let call = entry.capture.complete(
                tid,
                entry.syscall,
                result,
                entry.order,
                entry.started.elapsed(),
            )?;
            observe(self.next()?, tid, status, None, Some(call))?;
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
        ptrace::getevent(tid)
            .map(|message| Some(message.cast_unsigned()))
            .map_err(Into::into)
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
        // SAFETY: status points to one writable integer. The helper is the sole wait owner.
        let tid = unsafe { libc::waitpid(-1, &raw mut status, libc::__WALL) };
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

fn list_tasks(root: Pid) -> io::Result<impl Iterator<Item = io::Result<Pid>>> {
    Ok(
        std::fs::read_dir(format!("/proc/{root}/task"))?.map(|entry| {
            let name = entry?.file_name();
            name.to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .filter(|tid| *tid > 0)
                .map(Pid::from_raw)
                .ok_or_else(|| io::Error::other("invalid procfs task identity"))
        }),
    )
}

fn traced_by_self(tid: Pid) -> io::Result<bool> {
    Ok(proc_field::<u32>(format!("/proc/{tid}/status"), "TracerPid:")? == Some(std::process::id()))
}

#[cfg(test)]
pub(crate) mod tests;
