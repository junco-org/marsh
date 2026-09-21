//! Real byte pipes, for the jobs whose output is not a terminal.
//!
//! A pseudoterminal is the wrong carrier for a helper process. It merges stdout and stderr, it
//! rewrites `\n` into `\r\n`, it has no end-of-file a writer can send, and a line discipline
//! sitting between the program and its reader is free to lose a NUL or reorder a control
//! character. A workload whose output is *data* — a captured buffer, a log, a tunnel's readiness
//! banner — needs none of that, and cannot tolerate any of it.
//!
//! So a [`JobIo::Pipes`](crate::shellmux::JobIo::Pipes) job gets three ordinary pipes. The shell
//! gets blocking fds 0, 1 and 2, so a program writing into a full pipe waits rather than losing
//! bytes to `EAGAIN`. The mux keeps the other three ends non-blocking, so one async reactor can
//! poll them. Every descriptor is created `O_CLOEXEC` atomically with `pipe2`, so a concurrent
//! `fork`/`exec` elsewhere in the process cannot leak one into an unrelated child.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

use tokio::io::unix::AsyncFd;

/// One job's six pipe ends, split by owner.
pub(crate) struct PipeSet {
    /// Read end of standard input: the shell's fd 0, blocking.
    pub(crate) child_stdin: OwnedFd,
    /// Write end of standard output: the shell's fd 1, blocking.
    pub(crate) child_stdout: OwnedFd,
    /// Write end of standard error: the shell's fd 2, blocking.
    pub(crate) child_stderr: OwnedFd,
    /// Write end of standard input: the mux's, non-blocking.
    pub(crate) input: OwnedFd,
    /// Read end of standard output: the mux's, non-blocking.
    pub(crate) stdout: OwnedFd,
    /// Read end of standard error: the mux's, non-blocking.
    pub(crate) stderr: OwnedFd,
}

/// Creates one `O_CLOEXEC` pipe as `(read, write)`.
fn pipe2_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe2` receives a pointer to a two-element array it is allowed to write, and a
    // flag value it defines.
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if created < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by `pipe2` and are owned by nothing else.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above, for the write end.
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read, write))
}

/// Puts `fd` into non-blocking mode.
///
/// Applied only to the three ends the mux itself polls. The shell's own descriptors stay blocking
/// on purpose: a program that got `EAGAIN` writing to its own stdout would either lose the write
/// or spin, and neither is something a workload can be asked to handle.
fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: `fcntl` receives an open descriptor and a command that takes no further argument.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fcntl` receives the same open descriptor and the flag word it just reported, plus
    // one bit this call is setting.
    let applied = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if applied < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Opens one job's three pipes.
///
/// # Errors
///
/// Fails when a pipe cannot be created or the mux's own ends cannot be made non-blocking. Every
/// descriptor opened along the way is closed before returning an error, because `OwnedFd` owns it
/// from the moment it exists.
pub(crate) fn open_pipes() -> std::io::Result<PipeSet> {
    let (stdin_read, stdin_write) = pipe2_cloexec()?;
    let (stdout_read, stdout_write) = pipe2_cloexec()?;
    let (stderr_read, stderr_write) = pipe2_cloexec()?;

    set_nonblocking(&stdin_write)?;
    set_nonblocking(&stdout_read)?;
    set_nonblocking(&stderr_read)?;

    Ok(PipeSet {
        child_stdin: stdin_read,
        child_stdout: stdout_write,
        child_stderr: stderr_write,
        input: stdin_write,
        stdout: stdout_read,
        stderr: stderr_read,
    })
}

/// The write end of one job's standard input, shared by every handle on that job.
///
/// Behind an async mutex, which is also what serializes writes: two callers writing concurrently
/// produce two whole writes in some order rather than one interleaved one. Closing takes the same
/// lock, so a close can never land between a write's two halves, and is idempotent — the second
/// close finds the descriptor already gone and says so by succeeding.
#[derive(Debug)]
pub(crate) struct PipeInput {
    /// `None` once the write end has been closed.
    fd: tokio::sync::Mutex<Option<AsyncFd<OwnedFd>>>,
}

impl PipeInput {
    /// Takes ownership of a job's non-blocking stdin write end.
    ///
    /// # Errors
    ///
    /// Fails when the descriptor cannot be registered with the reactor.
    pub(crate) fn new(fd: OwnedFd) -> std::io::Result<Self> {
        Ok(Self {
            fd: tokio::sync::Mutex::new(Some(AsyncFd::new(fd)?)),
        })
    }

    /// Writes every byte of `bytes`, or reports why it could not.
    ///
    /// A short write is retried until the slice is gone. A failure partway through has already
    /// delivered a prefix: this is a pipe, and there is no way to take bytes back.
    ///
    /// # Errors
    ///
    /// Fails with [`std::io::ErrorKind::BrokenPipe`] when the input has been closed, and with the
    /// underlying error when the write itself fails.
    pub(crate) async fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        let guard = self.fd.lock().await;
        let Some(fd) = guard.as_ref() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "job input is closed",
            ));
        };
        let mut written = 0;
        while written < bytes.len() {
            let mut ready = fd.writable().await?;
            let attempt = ready.try_io(|inner| {
                let slice = &bytes[written..];
                // SAFETY: `write` receives an open descriptor, a valid pointer and the length of
                // the slice behind it.
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
                Ok(Err(error)) => return Err(error),
                // Not ready after all; the guard is cleared and the next await waits again.
                Err(_would_block) => {}
            }
        }
        drop(guard);
        Ok(())
    }

    /// Closes the write end, which is the end-of-file the program reads.
    ///
    /// Idempotent, and ordered after every write this handle already accepted. It closes *this*
    /// job's input: a job whose name was later reused has its own descriptor, and this can never
    /// reach it.
    pub(crate) async fn close(&self) {
        let mut guard = self.fd.lock().await;
        drop(guard.take());
        drop(guard);
    }
}

/// Reads whatever is available from a pipe's read end into `buffer`.
///
/// `0` is end of file: every write end is gone. Bytes are preserved exactly — a NUL, a lone `\r`
/// and a final line with no newline all arrive as they were written, because nothing is between
/// the writer and this read.
///
/// # Errors
///
/// Fails with whatever the read reported, other than `EINTR`, which is retried.
pub(crate) async fn read_pipe(fd: &AsyncFd<OwnedFd>, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut ready = fd.readable().await?;
        let attempt = ready.try_io(|inner| {
            // SAFETY: `read` receives an open descriptor, a valid writable pointer and the length
            // of the slice behind it.
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
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
    }
}
