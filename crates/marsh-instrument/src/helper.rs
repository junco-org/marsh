//! Disposable observation companion using unmodified lurk's native facilities.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use crate::capture::proc_field;
use crate::observation::Observer;
use lurk_cli::args::Args;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;

pub(crate) const PRELUDE: &[u8] = b"marsh-lurk/2\n";
pub(crate) const FRAME_LIMIT: usize = 256 * 1024;
const QUEUE_RECORDS: usize = 65_536;
const QUEUE_BYTES: usize = 64 * 1024 * 1024;

/// Runs the package-owned companion. Only its actual parent can release attachment.
///
/// # Errors
/// Returns bootstrap, native observation, encoding or backpressure failures. The caller must
/// exit: kernel detach on this disposable process's exit is the final cleanup mechanism.
#[doc(hidden)]
pub fn run_tracer_helper() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let host = match (arguments.next(), arguments.next(), arguments.next()) {
        (Some(flag), Some(pid), None) if flag == "--host-pid" => pid
            .to_str()
            .and_then(|pid| pid.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| io::Error::other("invalid parent pid"))?,
        _ => return Err(io::Error::other("usage: marsh-trace --host-pid PID")),
    };
    if nix::unistd::getppid().as_raw() != host {
        return Err(io::Error::other("helper may observe only its parent"));
    }
    // SAFETY: this prctl accepts an integer signal; it accesses no userspace memory.
    nix::errno::Errno::result(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
    if nix::unistd::getppid().as_raw() != host {
        return Err(io::Error::other("parent exited during bootstrap"));
    }
    let mut release = [0];
    io::stdin().read_exact(&mut release)?;
    if release != [1] {
        return Err(io::Error::other("invalid attachment release"));
    }
    // SAFETY: the parent installs the connected Unix socket as stdout, uniquely owned here.
    let mut socket = unsafe { UnixStream::from_raw_fd(libc::STDOUT_FILENO) };
    socket.write_all(PRELUDE)?;
    let (sender, receiver) = mpsc::sync_channel::<(Vec<u8>, Option<OwnedFd>)>(QUEUE_RECORDS);
    let reserved = Arc::new(AtomicUsize::new(0));
    let writer_bytes = Arc::clone(&reserved);
    let writer = std::thread::Builder::new()
        .name("marsh-trace-writer".into())
        .spawn(move || {
            for (frame, process) in receiver {
                if let Err(error) = send_frame(&mut socket, &frame, process.as_ref()) {
                    eprintln!("native trace writer: {error}");
                    // No tracee may remain stopped behind a broken host reader.
                    std::process::exit(1);
                }
                writer_bytes.fetch_sub(frame.len(), Ordering::Release);
            }
        })?;
    let args = Args {
        follow_forks: true,
        expr: vec!["trace=%file,%desc,%process,%memory,%fstat,%fstatfs".into()],
        ..Args::default()
    };
    let mut tracer = Observer::attach(Pid::from_raw(host), &args)
        .map_err(|error| io::Error::other(format!("native attachment: {error:#}")))?;
    let result = tracer.run(|sequence, tid, status, event, info| {
        let stopped = WaitStatus::from_raw(tid, status)?;
        let process = if carries_process(&stopped) {
            Some(pin_stopped_group(tid)?)
        } else {
            None
        };
        let mut frame = serde_json::to_vec(&(sequence, tid.as_raw(), status, event, info))?;
        frame.push(b'\n');
        if frame.len() > FRAME_LIMIT {
            return Err(io::Error::other("native frame exceeds limit"));
        }
        let bytes = frame.len();
        reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= QUEUE_BYTES)
            })
            .map_err(|_| io::Error::other("native trace byte queue exhausted"))?;
        if let Err(error) = sender.try_send((frame, process)) {
            reserved.fetch_sub(bytes, Ordering::Release);
            return Err(io::Error::other(format!("native trace queue: {error}")));
        }
        Ok(())
    });
    // Never join a blocked writer after failure: the entry point exits and releases tracees.
    result.map_err(|error| io::Error::other(format!("native observation: {error:#}")))?;
    drop(sender);
    writer
        .join()
        .map_err(|_| io::Error::other("native writer panicked"))
}

/// Every stopped lifecycle callback carries its pinned process identity as `SCM_RIGHTS`. This is
/// producer ownership, not another syscall representation; the newline-delimited tuple is unchanged.
pub(crate) const fn carries_process(status: &WaitStatus) -> bool {
    matches!(
        status,
        WaitStatus::PtraceEvent(..) | WaitStatus::Stopped(..)
    )
}

fn pin_stopped_group(tid: Pid) -> io::Result<OwnedFd> {
    let group = proc_field::<i32>(format!("/proc/{tid}/status"), "Tgid:")?
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("stopped task has no process identity"))?;
    crate::tracing::open_process(group)
}

fn send_frame(socket: &mut UnixStream, frame: &[u8], process: Option<&OwnedFd>) -> io::Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    let Some(process) = process else {
        return socket.write_all(frame);
    };
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let descriptors = [process.as_fd()];
    if !ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)) {
        return Err(io::Error::other("pidfd ancillary buffer is too small"));
    }
    let written = rustix::io::retry_on_intr(|| {
        sendmsg(
            &*socket,
            &[io::IoSlice::new(frame)],
            &mut ancillary,
            SendFlags::NOSIGNAL,
        )
    })?;
    if written == 0 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "native socket closed",
        ));
    }
    socket.write_all(&frame[written..])
}
