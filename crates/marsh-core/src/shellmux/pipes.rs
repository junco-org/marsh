//! Real byte pipes, for the jobs whose output is not a terminal, and the non-blocking read and
//! write loops every job stream — pipe or pseudoterminal master — goes through.
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

use std::os::fd::OwnedFd;

use nix::fcntl::{FcntlArg, OFlag};
use tokio::io::Interest;
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
    nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(std::io::Error::from)
}

/// Puts `fd` into non-blocking mode.
///
/// Applied only to the three ends the mux itself polls. The shell's own descriptors stay blocking
/// on purpose: a program that got `EAGAIN` writing to its own stdout would either lose the write
/// or spin, and neither is something a workload can be asked to handle.
fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // Retained rather than truncated: the descriptor's other status flags are the kernel's
    // answer, and putting back only the bits this crate happens to name would clear them.
    let flags = OFlag::from_bits_retain(nix::fcntl::fcntl(fd, FcntlArg::F_GETFL)?);
    nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
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

    /// Writes every byte of `bytes` under the input's lock, or reports why it could not.
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
        let written = write_fd(fd, bytes).await;
        drop(guard);
        written
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

/// Runs `call` on `fd` once it is ready for `interest`, retrying `EINTR`.
///
/// Tokio owns the readiness retry: a call the kernel refuses with `EAGAIN` clears the
/// descriptor's readiness and waits again, inside `async_io`.
async fn retrying<T>(
    fd: &AsyncFd<OwnedFd>,
    interest: Interest,
    mut call: impl FnMut(&OwnedFd) -> nix::Result<T>,
) -> std::io::Result<T> {
    loop {
        match fd
            .async_io(interest, |inner| call(inner).map_err(std::io::Error::from))
            .await
        {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Writes every byte of `bytes` to `fd`, retrying short writes.
///
/// A failure partway through has already delivered a prefix: there is no way to take bytes back.
///
/// # Errors
///
/// Fails with whatever the write reported, other than `EINTR`, which is retried.
pub(crate) async fn write_fd(fd: &AsyncFd<OwnedFd>, bytes: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        written += retrying(fd, Interest::WRITABLE, move |inner| {
            nix::unistd::write(inner, &bytes[written..])
        })
        .await?;
    }
    Ok(())
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
    retrying(fd, Interest::READABLE, |inner| {
        nix::unistd::read(inner, &mut *buffer)
    })
    .await
}

/// Reads whatever a shell's terminal has produced into `buffer`, returning how many bytes.
///
/// Bytes are preserved exactly: escape sequences, non-UTF-8 output and a final line with no
/// newline all arrive as they were written. `0` is end of file, which on Linux is how a
/// pseudoterminal reports that its last writer is gone.
///
/// # Errors
///
/// Fails with whatever the read reported, other than `EINTR`, which is retried.
pub(crate) async fn read_terminal(
    terminal: &AsyncFd<OwnedFd>,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    retrying(
        terminal,
        Interest::READABLE,
        |inner| match nix::unistd::read(inner, &mut *buffer) {
            // A pseudoterminal master whose slave has been closed answers `EIO`. That is a hangup,
            // not a failure: it is this stream's end of file. Only the syscall's own `EIO` is one — a
            // readiness failure still propagates.
            Err(nix::errno::Errno::EIO) => Ok(0),
            other => other,
        },
    )
    .await
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use nix::fcntl::FdFlag;

    use super::*;
    use crate::shellmux::testing::{LIMIT, is_nonblocking, poll_once};

    /// The pipe's real capacity, which is a kernel property and not a constant this may assume.
    fn capacity_of(fd: &OwnedFd) -> usize {
        let size = nix::fcntl::fcntl(fd, FcntlArg::F_GETPIPE_SZ).expect("the pipe's capacity");
        usize::try_from(size).expect("a pipe capacity is not negative")
    }

    /// Fills the pipe behind `fd` until the kernel refuses, answering with what it accepted.
    fn prefill(fd: &OwnedFd) -> Vec<u8> {
        let chunk = vec![b'P'; 4096.min(capacity_of(fd))];
        let mut accepted = Vec::new();
        loop {
            match nix::unistd::write(fd, &chunk) {
                Ok(0) => break,
                Ok(count) => accepted.extend_from_slice(&chunk[..count]),
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(nix::errno::Errno::EINTR) => {}
                Err(other) => panic!("the prefill write failed: {other}"),
            }
        }
        accepted
    }

    /// `len` distinguishable bytes cycling through `modulus` values from `offset`.
    fn pattern(len: usize, modulus: usize, offset: usize) -> Vec<u8> {
        (0..len)
            .map(|index| u8::try_from(index % modulus + offset).unwrap_or(0))
            .collect()
    }

    /// Drains `reader` to end of file.
    async fn drain(reader: &AsyncFd<OwnedFd>) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = vec![0_u8; 4096];
        loop {
            let count = read_pipe(reader, &mut buffer).await.expect("a read");
            if count == 0 {
                break collected;
            }
            collected.extend_from_slice(&buffer[..count]);
        }
    }

    /// The six ends divide into two roles, and the kernel is asked which one each descriptor
    /// actually got: a program that received `EAGAIN` from its own stdout would lose the write,
    /// and an end leaked through an unrelated `exec` would outlive the job.
    #[test]
    fn pipe_endpoints_preserve_cloexec_and_blocking_roles() {
        let pipes = open_pipes().expect("three pipes");
        let polled = [&pipes.input, &pipes.stdout, &pipes.stderr];
        let child = [&pipes.child_stdin, &pipes.child_stdout, &pipes.child_stderr];

        for fd in polled.into_iter().chain(child) {
            let flags = FdFlag::from_bits_retain(
                nix::fcntl::fcntl(fd, FcntlArg::F_GETFD).expect("descriptor flags"),
            );
            assert!(
                flags.contains(FdFlag::FD_CLOEXEC),
                "every end is close-on-exec"
            );
        }
        for fd in polled {
            assert!(is_nonblocking(fd), "the mux polls the ends it owns");
        }
        for fd in child {
            assert!(!is_nonblocking(fd), "the shell's own ends stay blocking");
        }
    }

    /// Two writes and a close, all admitted while the pipe is full: the mutex decides their order
    /// before a single byte moves, and the reader then sees each write whole, in that order, and
    /// a real end of file behind them.
    #[tokio::test]
    async fn pipe_input_backpressure_preserves_whole_writes_and_eof() {
        let pipes = open_pipes().expect("three pipes");
        let capacity = capacity_of(&pipes.input);
        let prefilled = prefill(&pipes.input);
        assert!(!prefilled.is_empty(), "an empty pipe accepts something");

        // Distinguishable, and each longer than the pipe holds, so neither can be delivered
        // without the reader draining in the middle of it.
        let first = pattern(capacity * 2, 251, 0);
        let second = pattern(capacity * 2, 241, 1);
        let expected: Vec<u8> = prefilled
            .iter()
            .chain(&first)
            .chain(&second)
            .copied()
            .collect();

        let input = PipeInput::new(pipes.input).expect("a registered write end");
        let mut write_first = Box::pin(input.write_all(&first));
        assert!(
            poll_once(write_first.as_mut()).await.is_pending(),
            "a full pipe accepts no more"
        );
        let mut write_second = Box::pin(input.write_all(&second));
        assert!(
            poll_once(write_second.as_mut()).await.is_pending(),
            "the first write holds the input"
        );
        let mut closing = Box::pin(input.close());
        assert!(
            poll_once(closing.as_mut()).await.is_pending(),
            "the close queues behind both writes"
        );

        // Only the test's reader is polled: the shell's own end of this pipe is blocking.
        set_nonblocking(&pipes.child_stdin).expect("a pollable read end");
        let reader = AsyncFd::new(pipes.child_stdin).expect("a registered read end");
        let drained = tokio::time::timeout(LIMIT, async {
            let writers = async {
                write_first.as_mut().await.expect("the first write");
                write_second.as_mut().await.expect("the second write");
                closing.as_mut().await;
            };
            let ((), collected) = tokio::join!(writers, drain(&reader));
            collected
        })
        .await
        .expect("the writes and the drain finish");

        assert_eq!(drained.len(), expected.len(), "every byte arrives once");
        assert_eq!(drained, expected, "whole writes, in admission order");

        input.close().await;
        for (bytes, why) in [
            (&b"x"[..], "a closed input takes nothing"),
            (&b""[..], "not even an empty write"),
        ] {
            assert_eq!(
                input.write_all(bytes).await.expect_err(why).kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
    }

    /// Keystrokes and one-shot stdin both reach a shell through `write_fd`, and a full descriptor
    /// must delay them rather than truncate them: the reader sees the prefix that was already
    /// there and then every byte of the payload, in order.
    #[tokio::test]
    async fn terminal_write_helper_survives_backpressure() {
        let pipes = open_pipes().expect("three pipes");
        let prefilled = prefill(&pipes.input);
        assert!(!prefilled.is_empty(), "an empty pipe accepts something");
        let payload = pattern(prefilled.len() * 2, 251, 0);

        // Owned here, so a failure or a timeout below still drops it and lets the reader finish.
        let writer = AsyncFd::new(pipes.input).expect("a registered write end");
        let child_stdin = pipes.child_stdin;
        let drain = tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let mut collected = Vec::new();
            std::fs::File::from(child_stdin)
                .read_to_end(&mut collected)
                .expect("the reader drains to end of file");
            collected
        });

        tokio::time::timeout(LIMIT, write_fd(&writer, &payload))
            .await
            .expect("the write finishes once the reader drains")
            .expect("a full descriptor is backpressure, not a failure");
        drop(writer);

        let collected = tokio::time::timeout(LIMIT, drain)
            .await
            .expect("the reader sees end of file")
            .expect("the reader task");
        let expected: Vec<u8> = prefilled.iter().chain(&payload).copied().collect();
        assert_eq!(collected.len(), expected.len());
        assert_eq!(collected, expected, "every byte, in order");
    }

    /// A pseudoterminal reports the loss of its last writer as `EIO`. That is this stream's end
    /// of file, and reporting it as an error instead would make every closing shell look broken.
    #[tokio::test]
    async fn a_terminal_read_ends_when_its_slave_is_gone() {
        let (master, slave) =
            crate::shellmux::pty::open_pty(24, 80).expect("a private pseudoterminal");
        let terminal = AsyncFd::new(master).expect("a registered master");
        drop(slave);

        let mut buffer = [0_u8; 32];
        let count = tokio::time::timeout(LIMIT, read_terminal(&terminal, &mut buffer))
            .await
            .expect("the read resolves")
            .expect("a hangup is not a read failure");
        assert_eq!(count, 0, "the hangup is this stream's end of file");
    }
}
