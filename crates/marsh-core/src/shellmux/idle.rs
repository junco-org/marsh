//! The idle-terminal lease: how an interactive prompt reads one job's keyboard.
//!
//! `brush-interactive` reads the *process's* standard input. A daemon owning several independent
//! panes cannot use it: there is one stdin and there are many prompts, and whichever prompt read
//! it would be stealing the others' keystrokes. What each pane actually needs is the slave side of
//! its own pseudoterminal — the end its programs read — for exactly as long as no program is using
//! it.
//!
//! That is what an [`IdleTerminal`] is: a lease on one terminal job's slave, granted only while
//! the job is idle, and revoked the moment a command is admitted into it. It is deliberately not
//! cloneable and deliberately not reissued while one is outstanding, because two readers on one
//! terminal is the same bug as two prompts on one stdin.
//!
//! The descriptor is an *independent* open file description obtained with `TIOCGPTPEER`, never a
//! `dup` of the shell's own. That distinction is load-bearing: `O_NONBLOCK` set on a `dup` would
//! be set on the shell's standard input too, and a launched program would start getting `EAGAIN`
//! from its own keyboard.

use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::io::unix::AsyncFd;

use crate::shellmux::error::MuxError;
use crate::shellmux::types::TerminalGeometry;

/// Lease is live; reads and writes proceed.
const ACTIVE: u8 = 0;
/// A command is starting: reads stop, writes are still allowed.
///
/// The prompt owns the terminal's mode while it edits, and it has to be able to put that mode back
/// before the command starts. So a run-revocation stops the *reader* and leaves the writer alone
/// for exactly one thing: the short mode reset the prompt owes.
const REVOKED_RUN: u8 = 1;
/// The job is going away: reads and writes both stop.
const REVOKED_CLOSE: u8 = 2;

/// Why a lease was revoked, as the mux asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revocation {
    /// A command is about to run in this job.
    Run,
    /// The job is stopping or the mux is shutting down.
    Close,
}

impl Revocation {
    /// The level this revocation raises the lease to.
    const fn level(self) -> u8 {
        match self {
            Self::Run => REVOKED_RUN,
            Self::Close => REVOKED_CLOSE,
        }
    }
}

/// Why a terminal could not be leased.
///
/// Every one of these is a state the *lease itself* put the terminal in, which is why they are
/// reported from here rather than inferred by the mux from a reservation it cannot see.
#[derive(Debug)]
pub(crate) enum LeaseRefusal {
    /// A lease is outstanding: some prompt already owns this terminal's mode.
    Held,
    /// The last lease released without putting the terminal back.
    ///
    /// Shared rather than consumed: the terminal is in a mode nobody configured, so the next
    /// lease request and the command admission that finally closes the job must both see it.
    Unrestored(Arc<MuxError>),
    /// The job is closing; its terminal is not coming back.
    Closing,
}

/// What the current or last lease on one terminal left behind.
#[derive(Clone, Debug)]
enum Holding {
    /// No lease has been granted, or the last one released and put the terminal back.
    Free,
    /// A lease is outstanding.
    Held,
    /// A released lease could not put the terminal back. Sticky.
    Unrestored(Arc<MuxError>),
}

/// The lease state one terminal job and its mux share, for as long as the job lives.
///
/// Both halves are written by the side that actually knows: the mux raises the revocation
/// [`level`](Self::level) when it needs the terminal back, and the *lease's own release path*
/// records that it is gone. Nothing else reaps a lease — a mux that cleared this when it happened
/// to want the terminal would leak the reservation for every caller that leases one and admits no
/// command, which is what an interactive prompt does on every line it handles itself.
#[derive(Debug)]
pub(crate) struct LeaseState {
    /// [`ACTIVE`], [`REVOKED_RUN`] or [`REVOKED_CLOSE`].
    ///
    /// A run-revocation is transient: it asks the current holder to give the terminal back, and
    /// [`Self::reserve`] clears it when the next lease is granted. A close-revocation is
    /// permanent, because the job is not coming back. Nothing else lowers it.
    level: AtomicU8,
    /// Wakes a lease blocked in a read or a write.
    signal: tokio::sync::Notify,
    /// Whether a lease is outstanding, and what the last one left behind.
    ///
    /// A watch rather than a one-shot: a terminal is leased once per prompt iteration, not once
    /// per job, so the release has to be observable again and again — and a waiting admission has
    /// to be able to wait for it without racing the grant that follows.
    holding: tokio::sync::watch::Sender<Holding>,
}

impl Default for LeaseState {
    /// A live lease state: nothing revoked, nothing leased.
    fn default() -> Self {
        Self {
            level: AtomicU8::new(ACTIVE),
            signal: tokio::sync::Notify::new(),
            holding: tokio::sync::watch::Sender::new(Holding::Free),
        }
    }
}

impl LeaseState {
    /// Raises the revocation level and wakes the holder.
    pub(crate) fn revoke(&self, revocation: Revocation) {
        self.level.fetch_max(revocation.level(), Ordering::AcqRel);
        self.signal.notify_waiters();
    }

    /// Reserves this terminal for a lease about to be granted.
    ///
    /// Clears a run-revocation on the way: the grant *is* the terminal coming back to a prompt,
    /// and leaving the level raised would hand out a lease whose every read answers "revoked".
    ///
    /// # Errors
    ///
    /// Refuses while a lease is outstanding, once a release has failed to restore the terminal,
    /// and after the job has been closed.
    pub(crate) fn reserve(&self) -> Result<(), LeaseRefusal> {
        if self.level() >= REVOKED_CLOSE {
            return Err(LeaseRefusal::Closing);
        }
        let mut refusal = None;
        self.holding.send_if_modified(|holding| match holding {
            Holding::Free => {
                *holding = Holding::Held;
                true
            }
            Holding::Held => {
                refusal = Some(LeaseRefusal::Held);
                false
            }
            Holding::Unrestored(error) => {
                refusal = Some(LeaseRefusal::Unrestored(Arc::clone(error)));
                false
            }
        });
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        // Only after the reservation is taken: a lease that cleared the revocation first and then
        // lost the reservation would have un-revoked the holder's terminal behind its back.
        self.level
            .compare_exchange(REVOKED_RUN, ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .ok();
        Ok(())
    }

    /// Gives a reservation back when the grant it was taken for failed.
    ///
    /// The reservation must not outlive a failed grant, or the terminal stays permanently held
    /// for a lease nobody has.
    pub(crate) fn abandon(&self) {
        self.holding.send_if_modified(|holding| {
            if matches!(holding, Holding::Held) {
                *holding = Holding::Free;
                return true;
            }
            false
        });
    }

    /// Records that the outstanding lease is gone, and whether the terminal came back.
    ///
    /// Called from the lease's own drop, after the restoration and after the leased descriptor is
    /// closed, so a mux waiting here never observes a released lease whose terminal is still in
    /// the prompt's raw mode.
    fn released(&self, restored: Option<std::io::Error>) {
        let holding = restored.map_or(Holding::Free, |error| {
            Holding::Unrestored(Arc::new(MuxError::Io(error)))
        });
        self.holding.send_replace(holding);
    }

    /// Waits until no lease is outstanding on this terminal.
    ///
    /// Returns immediately when there is none. Pair it with [`Self::revoke`]: this waits for the
    /// holder to let go, it does not ask it to.
    ///
    /// # Errors
    ///
    /// Fails when the lease that released could not put the terminal back. The caller must not
    /// launch anything into it: the terminal is in the prompt's raw mode, with echo off and every
    /// key unprocessed, and a program started there would run in a mode it has no way to discover.
    pub(crate) async fn settled(&self) -> Result<(), Arc<MuxError>> {
        let mut holding = self.holding.subscribe();
        // A holder that vanished without ever dropping its lease cannot be waited out, but that
        // cannot happen: the sender lives in this state, which outlives every lease on it.
        let settled = holding
            .wait_for(|holding| !matches!(holding, Holding::Held))
            .await;
        match settled.as_deref() {
            Ok(Holding::Unrestored(error)) => Err(Arc::clone(error)),
            _ => Ok(()),
        }
    }

    /// The current level.
    fn level(&self) -> u8 {
        self.level.load(Ordering::Acquire)
    }
}

/// A lease on one idle terminal job's slave side.
///
/// Not cloneable: the lease *is* the exclusivity. Dropping it restores the terminal attributes it
/// saved, closes the leased descriptor and acknowledges the release, in that order, so a mux
/// waiting to launch a command never starts one into a terminal still in the prompt's raw mode.
#[derive(Debug)]
pub struct IdleTerminal {
    /// The leased descriptor, registered with the reactor. Closed before the release is reported.
    fd: Option<AsyncFd<OwnedFd>>,
    /// The revocation state this lease shares with its mux.
    state: Arc<LeaseState>,
    /// The attributes to put back on release.
    ///
    /// The raw `libc` record rather than `nix`'s wrapper: that wrapper keeps its state in a
    /// `RefCell`, which makes it `!Sync`, which would make every future holding a lease `!Send` —
    /// and a daemon's pane driver is a spawned task. The plain C struct is `Copy`, `Send` and
    /// `Sync`, and it is exactly what `tcsetattr` wants back.
    saved: libc::termios,
}

impl IdleTerminal {
    /// Grants a lease on the terminal behind `master`.
    ///
    /// Opens an independent slave descriptor, saves its attributes, and applies raw mode with the
    /// suspend character left disabled — the prompt is an editor, so it needs every keystroke
    /// unprocessed, and Ctrl-Z must stay an ordinary byte exactly as it is for a program.
    ///
    /// # Errors
    ///
    /// Fails when the peer descriptor cannot be opened, when its attributes cannot be read or
    /// applied, or when it cannot be registered with the reactor.
    pub(crate) fn grant(master: BorrowedFd<'_>, state: Arc<LeaseState>) -> std::io::Result<Self> {
        let fd = crate::shellmux::pty::open_peer(master)?;
        let saved = get_attributes(fd.as_fd())?;

        let mut raw = saved;
        // SAFETY: `cfmakeraw` rewrites the fields of the record it is handed and reads nothing
        // else.
        unsafe { libc::cfmakeraw(&raw mut raw) };
        raw.c_cc[libc::VSUSP] = 0;
        set_attributes(fd.as_fd(), &raw)?;

        Ok(Self {
            fd: Some(AsyncFd::new(fd)?),
            state,
            saved,
        })
    }

    /// Reads whatever the terminal has, waiting until something arrives.
    ///
    /// `Ok(None)` is revocation: a command is starting in this job, or the job is closing. The
    /// bytes read so far are still the caller's; nothing is lost, and the caller must stop reading
    /// and release the lease.
    ///
    /// `Ok(Some(0))` is end of file. Anything else is a chunk, preserved byte for byte, which may
    /// split a UTF-8 or escape sequence.
    ///
    /// # Errors
    ///
    /// Fails with whatever the read reported, other than `EINTR`, which is retried.
    pub async fn read(&self, buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
        loop {
            if self.state.level() >= REVOKED_RUN {
                return Ok(None);
            }
            // Registered before the readiness wait, so a revocation landing between them still
            // wakes this rather than leaving the prompt blocked on a terminal nobody will write.
            let revoked = self.state.signal.notified();
            let Some(fd) = self.fd.as_ref() else {
                return Ok(Some(0));
            };
            let ready = tokio::select! {
                ready = fd.readable() => ready?,
                () = revoked => continue,
            };
            let mut ready = ready;
            let attempt = ready.try_io(|inner| match nix::unistd::read(inner.get_ref(), buffer) {
                // A slave whose master is gone reports `EIO`; that is this stream's end of file.
                Err(nix::errno::Errno::EIO) => Ok(0),
                other => other.map_err(std::io::Error::from),
            });
            match attempt {
                Ok(Ok(count)) => return Ok(Some(count)),
                // Retried by the outer loop rather than here, so an interrupted read returns to
                // the revocation check instead of waiting on a terminal nobody will write to.
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => {}
            }
        }
    }

    /// Writes every byte of `bytes` into the terminal, as the slave side.
    ///
    /// Slave-side output is what puts a prompt, an echo or a job report *after* the command output
    /// already queued in the same terminal, rather than racing the master pump and appearing
    /// before it.
    ///
    /// A run-revocation does not stop this: the prompt still owes the terminal its mode reset. A
    /// close-revocation does, because there is no longer a surface for the bytes to reach.
    ///
    /// # Errors
    ///
    /// Fails with [`std::io::ErrorKind::BrokenPipe`] once the job is closing, and with whatever
    /// the write reported otherwise.
    pub async fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            if self.state.level() >= REVOKED_CLOSE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "idle terminal lease was revoked",
                ));
            }
            let revoked = self.state.signal.notified();
            let Some(fd) = self.fd.as_ref() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "idle terminal lease has been released",
                ));
            };
            let ready = tokio::select! {
                ready = fd.writable() => ready?,
                () = revoked => continue,
            };
            let mut ready = ready;
            let attempt = ready.try_io(|inner| {
                nix::unistd::write(inner.get_ref(), &bytes[written..]).map_err(std::io::Error::from)
            });
            match attempt {
                Ok(Ok(count)) => written += count,
                // Retried by the outer loop, so an interrupted write returns to the revocation
                // check rather than spinning inside the readiness callback.
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => {}
            }
        }
        Ok(())
    }

    /// This terminal's current size.
    ///
    /// Read from the kernel rather than remembered, so a resize applied while the lease was held
    /// is reflected without the prompt having to be told about it.
    ///
    /// # Errors
    ///
    /// Fails when the kernel rejects `TIOCGWINSZ`.
    pub fn geometry(&self) -> std::io::Result<TerminalGeometry> {
        let fd = self.fd.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "idle terminal lease has been released",
            )
        })?;
        let (rows, cols) = crate::shellmux::pty::terminal_size(fd.get_ref().as_fd())?;
        Ok(TerminalGeometry { rows, cols })
    }

    /// Whether this lease has been revoked, for a caller between reads.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.state.level() >= REVOKED_RUN
    }
}

impl Drop for IdleTerminal {
    /// Restores the terminal, closes the lease and reports the outcome, in that order.
    ///
    /// The order is the contract. A mux waiting to launch a command must not start one until the
    /// terminal is out of the prompt's raw mode and the lease descriptor is gone — and when the
    /// restoration *failed*, it must not start one at all. So the result is reported last, and it
    /// is a result rather than an acknowledgement: a silent failure here would launch a program
    /// into a terminal configured for a line editor, with echo off and every key unprocessed.
    fn drop(&mut self) {
        let restored = match self.fd.as_ref() {
            Some(fd) => set_attributes(fd.get_ref().as_fd(), &self.saved).err(),
            None => None,
        };
        // Closed before the report, so the mux never observes a released lease whose descriptor
        // is still open.
        drop(self.fd.take());
        self.state.released(restored);
    }
}

/// Reads a terminal's current attributes.
///
/// # Errors
///
/// Fails when the descriptor is not a terminal, or the kernel refuses the request.
fn get_attributes(fd: BorrowedFd<'_>) -> std::io::Result<libc::termios> {
    // SAFETY: the record is fully initialised by `tcgetattr` before it is read, and is only read
    // when the call reported success.
    let mut attributes = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: `tcgetattr` receives an open descriptor and a writable record of the right type.
    if unsafe { libc::tcgetattr(std::os::fd::AsRawFd::as_raw_fd(&fd), &raw mut attributes) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(attributes)
}

/// Applies `attributes` to a terminal immediately.
///
/// `TCSANOW` rather than `TCSADRAIN`: a lease is released at the moment a command is about to
/// start, and draining first would let the outgoing prompt's own bytes decide when the incoming
/// command gets its terminal.
///
/// # Errors
///
/// Fails when the descriptor is not a terminal, or the kernel refuses the attributes.
fn set_attributes(fd: BorrowedFd<'_>, attributes: &libc::termios) -> std::io::Result<()> {
    // SAFETY: `tcsetattr` receives an open descriptor, a flag it defines, and a readable record of
    // the right type.
    if unsafe {
        libc::tcsetattr(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            libc::TCSANOW,
            std::ptr::from_ref(attributes),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::Poll;
    use std::time::Duration;

    use nix::sys::termios::FlowArg;

    use super::*;

    /// How long a test may wait on the kernel before it is a failure rather than a hang.
    const LIMIT: Duration = Duration::from_secs(5);

    /// One private terminal and a lease on it.
    ///
    /// Both ends of the pair are kept: a master or slave dropped here would hang the terminal up
    /// and end the very read the revocation is supposed to end.
    struct Fixture {
        /// The master, held open for the lease's lifetime.
        _master: OwnedFd,
        /// The shell's own slave end, held for the same reason.
        _slave: OwnedFd,
        /// The revocation state the mux would drive.
        state: Arc<LeaseState>,
        /// The lease under test.
        lease: IdleTerminal,
    }

    fn fixture() -> Fixture {
        let (master, slave) =
            crate::shellmux::pty::open_pty(24, 80).expect("a private pseudoterminal");
        let state = Arc::new(LeaseState::default());
        state.reserve().expect("a fresh terminal is free");
        let lease =
            IdleTerminal::grant(master.as_fd(), Arc::clone(&state)).expect("a granted lease");
        Fixture {
            _master: master,
            _slave: slave,
            state,
            lease,
        }
    }

    /// Polls `future` exactly once and leaves it alive, so a caller can establish that it is
    /// waiting without consuming it.
    async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
    }

    /// The lease's own descriptor, for the test's direct syscalls.
    fn lease_fd(lease: &IdleTerminal) -> BorrowedFd<'_> {
        lease
            .fd
            .as_ref()
            .expect("a granted lease owns its descriptor")
            .get_ref()
            .as_fd()
    }

    /// A prompt blocked on its pane's keyboard is woken by the revocation itself: the command
    /// being admitted is not going to type anything, and a lease that only noticed revocations
    /// between reads would hold the terminal until someone did.
    #[tokio::test]
    async fn idle_read_is_woken_by_run_revocation() {
        let fixture = fixture();
        let mut buffer = [0_u8; 32];
        let mut read = Box::pin(fixture.lease.read(&mut buffer));
        assert!(
            poll_once(read.as_mut()).await.is_pending(),
            "nothing has been typed, so the read is waiting inside the terminal"
        );

        fixture.state.revoke(Revocation::Run);
        let answer = tokio::time::timeout(LIMIT, read.as_mut())
            .await
            .expect("the revocation wakes the blocked read")
            .expect("a revocation is not a read failure");
        assert!(
            answer.is_none(),
            "a revoked read reports the revocation rather than bytes"
        );
    }

    /// The other half: a write waiting for a terminal that cannot take it is woken by a close,
    /// and reports the closure rather than resuming.
    #[tokio::test]
    async fn idle_write_is_woken_by_close_revocation() {
        let fixture = fixture();
        let fd = lease_fd(&fixture.lease);
        // Suspending output is what makes the backpressure stable: the queue stops draining
        // towards the master, so a full one stays full for as long as the test needs it.
        nix::sys::termios::tcflow(fd, FlowArg::TCOOFF).expect("output suspended");
        let mut refused = false;
        for _ in 0..4096 {
            match nix::unistd::write(fd, &[b'.'; 1024]) {
                Ok(_) => {}
                Err(nix::errno::Errno::EAGAIN) => {
                    refused = true;
                    break;
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(other) => panic!("filling the suspended terminal failed: {other}"),
            }
        }
        assert!(refused, "a suspended terminal stops accepting output");

        let mut write = Box::pin(fixture.lease.write_all(b"x"));
        assert!(
            poll_once(write.as_mut()).await.is_pending(),
            "the write is waiting for room the terminal does not have"
        );

        fixture.state.revoke(Revocation::Close);
        let error = tokio::time::timeout(LIMIT, write.as_mut())
            .await
            .expect("the revocation wakes the blocked write")
            .expect_err("a closing lease has no surface left to write to");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "idle terminal lease was revoked");

        // Put the terminal back before the fixture's restoration runs on it.
        nix::sys::termios::tcflow(fd, FlowArg::TCOON).expect("output resumed");
    }

    /// A run revocation stops the reader and leaves the writer alone for exactly one thing: the
    /// mode reset the outgoing prompt owes the terminal before the command starts.
    #[tokio::test]
    async fn a_run_revocation_still_admits_the_prompt_s_mode_reset() {
        let fixture = fixture();
        fixture.state.revoke(Revocation::Run);
        tokio::time::timeout(LIMIT, fixture.lease.write_all(b"\x1b[0m"))
            .await
            .expect("the write completes")
            .expect("a run revocation does not close the terminal");
        assert!(fixture.lease.is_revoked());
    }
}
