//! Pseudoterminals, and the suspend character a job's terminal does not have.
//!
//! A job runs attached to its own pseudoterminal: the shell owns the master, the traced child gets
//! the slave as its standard descriptors and controlling terminal. Two things about that terminal
//! are not what a default `openpty` would give, and both are load-bearing:
//!
//! * The descriptors are opened with `O_CLOEXEC` atomically, so a concurrent `fork`/`exec`
//!   elsewhere in the process cannot leak a job's terminal into an unrelated child.
//! * The suspend character is disabled, so Ctrl-Z reaches the job as an ordinary byte instead of
//!   becoming a `SIGTSTP` that no child disposition can take back.
//!
//! [`SuspendKeyGuard`] does the same to the *console's* terminal for the length of a session, and
//! puts back exactly the one character it took — never a whole snapshot, which would silently
//! revert everything else the session changed meanwhile.

use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

use brush_core::openfiles::OpenFile;
use nix::errno::Errno;

/// Guard that disables the terminal's suspend character and restores it on drop.
///
/// Disabling `VSUSP` is the terminal-level way to take Ctrl-Z away from a whole session: the
/// character never becomes a `SIGTSTP` at all, so it survives a child that installs its own signal
/// disposition and an `exec` that resets one. `brush_core::terminal::AutoModeGuard` is not a
/// substitute — it restores a whole snapshot, overwriting every unrelated mode change made while
/// it was alive.
pub struct SuspendKeyGuard {
    /// The terminal whose suspend character was replaced.
    file: OpenFile,
    /// The character that was there before, restored on drop.
    previous: libc::cc_t,
}

impl SuspendKeyGuard {
    /// Disables the suspend character on `file`, remembering what it was.
    ///
    /// # Arguments
    ///
    /// * `file` - The terminal to control. Held for the guard's lifetime.
    ///
    /// # Errors
    ///
    /// Fails when `file` is not a terminal or its attributes cannot be read or written.
    pub fn new(file: OpenFile) -> Result<Self, brush_core::Error> {
        let previous = {
            let fd = file.try_borrow_as_fd()?;
            replace_suspend_char::<brush_core::Error>(fd, nix::sys::termios::_POSIX_VDISABLE)?
        };
        Ok(Self { file, previous })
    }

    /// Disables the suspend character again, without taking a second guard.
    ///
    /// For reasserting the setting after something else has reconfigured the terminal: a completed
    /// `stty sane` restores the suspend character, and the original guard still holds the value to
    /// put back at the end of the session.
    ///
    /// # Arguments
    ///
    /// * `file` - The terminal to control.
    ///
    /// # Errors
    ///
    /// Fails when `file` is not a terminal or its attributes cannot be read or written.
    pub fn disable(file: &OpenFile) -> Result<(), brush_core::Error> {
        let fd = file.try_borrow_as_fd()?;
        replace_suspend_char::<brush_core::Error>(fd, nix::sys::termios::_POSIX_VDISABLE)?;
        Ok(())
    }
}

impl Drop for SuspendKeyGuard {
    fn drop(&mut self) {
        // Against freshly read attributes, so restoring one character does not undo whatever else
        // the session changed. Best-effort throughout: a terminal that has gone away is nothing a
        // destructor can act on, and neither is a file that can no longer lend a descriptor.
        if let Ok(fd) = self.file.try_borrow_as_fd() {
            let _ = replace_suspend_char::<brush_core::Error>(fd, self.previous);
        }
    }
}

/// Writes `value` as the terminal's suspend character, returning the one it replaced.
///
/// The attributes are read and written directly rather than through `brush_core::sys::terminal::
/// Config`, whose `termios` field is private to brush-core: only one control character changes
/// here, and a round trip through a settings struct would carry every other mode with it.
///
/// Generic in the error because the same three syscalls serve two callers with two error domains:
/// a session guard reporting [`brush_core::Error`], and a terminal still being assembled that has
/// only [`std::io::Error`]. Both already convert from nix's `Errno`, so the conversion the `?`
/// operators perform is the caller's own.
fn replace_suspend_char<E: From<nix::errno::Errno>>(
    fd: BorrowedFd<'_>,
    value: libc::cc_t,
) -> Result<libc::cc_t, E> {
    let mut termios = nix::sys::termios::tcgetattr(fd)?;
    let index = nix::sys::termios::SpecialCharacterIndices::VSUSP as usize;
    let previous = termios.control_chars[index];
    termios.control_chars[index] = value;
    nix::sys::termios::tcsetattr(fd, nix::sys::termios::SetArg::TCSANOW, &termios)?;
    Ok(previous)
}

/// Opens a private pseudoterminal pair sized `rows` by `cols`, returning `(master, slave)`.
///
/// The slave is the end handed to a child as its standard descriptors and controlling terminal;
/// the master is the end the shell reads and writes. It is obtained with the `TIOCGPTPEER` ioctl
/// rather than `openpty` or `ptsname` followed by `open`: those hand back a descriptor without
/// `O_CLOEXEC`, and a concurrent `fork`/`exec` between the open and a later `FD_CLOEXEC` change
/// leaks the terminal into an unrelated child. `TIOCGPTPEER` applies the flag atomically.
///
/// The master is opened `O_NONBLOCK` so an async reactor can poll it for readiness; the slave is
/// left blocking, because a child writing to a full terminal buffer must wait rather than lose
/// output to `EAGAIN`. Neither end becomes this process's controlling terminal (`O_NOCTTY`), and
/// packet mode is left off.
///
/// The requested size is applied to the slave, which both ends observe: the pair shares one
/// `winsize`. The suspend character is disabled on the new terminal, so Ctrl-Z reaches the job as
/// an ordinary byte instead of becoming a `SIGTSTP` no child disposition can take back.
///
/// # Arguments
///
/// * `rows` - The terminal height, in character cells.
/// * `cols` - The terminal width, in character cells.
///
/// # Errors
///
/// Fails when no pseudoterminal can be allocated, when the master cannot be granted or unlocked,
/// when the kernel refuses `TIOCGPTPEER`, or when the initial size or attributes cannot be
/// applied. Every descriptor opened along the way is closed before returning an error.
pub fn open_pty(rows: u16, cols: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let master = nix::pty::posix_openpt(
        nix::fcntl::OFlag::O_RDWR
            | nix::fcntl::OFlag::O_CLOEXEC
            | nix::fcntl::OFlag::O_NOCTTY
            | nix::fcntl::OFlag::O_NONBLOCK,
    )?;
    nix::pty::grantpt(&master)?;
    nix::pty::unlockpt(&master)?;

    // `PtyMaster` already owns its descriptor, and so does the `OwnedFd` it converts into, so
    // every early return below still closes the master.
    let master = OwnedFd::from(master);
    let slave = open_peer(master.as_fd(), 0)?;
    resize_pty(slave.as_fd(), rows, cols)?;
    replace_suspend_char::<std::io::Error>(slave.as_fd(), nix::sys::termios::_POSIX_VDISABLE)?;

    Ok((master, slave))
}

/// Opens an independent file description on the slave side of `master`, with `extra` status flags.
///
/// The descriptor a job's shell holds is not shareable for this: it is the shell's standard
/// input, its flags are the shell's, and making it non-blocking would make the shell's own reads
/// non-blocking too. `TIOCGPTPEER` hands back a *separate* open file description on the same
/// terminal, so the flags set here are this descriptor's alone — which is why an idle prompt may
/// poll it without changing anything the shell or a launched program sees.
///
/// `O_NONBLOCK` is therefore safe in `extra` and would not be on a `dup`. `O_CLOEXEC` is applied
/// atomically for the reason [`open_pty`] documents, and `O_NOCTTY` keeps this process from
/// acquiring the terminal.
///
/// # Errors
///
/// Fails when the kernel refuses `TIOCGPTPEER`, which is what a master whose terminal is already
/// gone reports.
pub(crate) fn open_peer(master: BorrowedFd<'_>, extra: libc::c_int) -> std::io::Result<OwnedFd> {
    let flags = libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOCTTY | extra;
    // SAFETY:
    // This is calling a libc function on a live, unlocked pseudoterminal master kept alive by the
    // borrow. `TIOCGPTPEER` takes its argument by value, so no pointer is handed to the kernel.
    let peer = Errno::result(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags) })?;
    // SAFETY:
    // `peer` is a fresh descriptor the kernel just returned and nothing else owns it yet.
    Ok(unsafe { OwnedFd::from_raw_fd(peer) })
}

/// Resizes the terminal behind `fd` to `rows` by `cols`.
///
/// Either half of a pseudoterminal pair may be passed: they share one `winsize`, so a resize
/// through the master is what the child sees through the slave.
///
/// # Arguments
///
/// * `fd` - A descriptor open on the terminal to resize.
/// * `rows` - The new terminal height, in character cells.
/// * `cols` - The new terminal width, in character cells.
///
/// # Errors
///
/// Fails when `fd` is not a terminal or the kernel rejects the `TIOCSWINSZ` request.
pub fn resize_pty(fd: BorrowedFd<'_>, rows: u16, cols: u16) -> std::io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY:
    // This is calling a libc function with a descriptor kept alive by the borrow and a pointer to
    // a live `winsize`, which is exactly what `TIOCSWINSZ` reads.
    Errno::result(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) })?;
    Ok(())
}

/// Reads the size of the terminal behind `fd` as `(rows, cols)`.
///
/// Rows come first: the pair is returned in the order [`resize_pty`] and [`open_pty`] take it, so
/// a caller can pass it straight back without transposing it.
///
/// # Arguments
///
/// * `fd` - A descriptor open on the terminal to measure.
///
/// # Errors
///
/// Fails when `fd` is not a terminal or the kernel rejects the `TIOCGWINSZ` request.
pub fn terminal_size(fd: BorrowedFd<'_>) -> std::io::Result<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY:
    // This is calling a libc function with a descriptor kept alive by the borrow and a pointer to
    // a live `winsize`, which is exactly what `TIOCGWINSZ` fills in.
    Errno::result(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &raw mut size) })?;
    Ok((size.ws_row, size.ws_col))
}

#[allow(
    clippy::expect_used,
    reason = "a terminal test has nothing to recover to"
)]
#[cfg(test)]
mod tests {
    use nix::sys::termios::{_POSIX_VDISABLE, LocalFlags, SetArg, tcgetattr, tcsetattr};

    use super::*;
    use crate::shellmux::testing::is_nonblocking;

    /// Where the suspend character sits among a terminal's control characters.
    const VSUSP: usize = nix::sys::termios::SpecialCharacterIndices::VSUSP as usize;

    /// The suspend character is the only thing the guard owns. Restoring a whole snapshot instead
    /// would silently revert whatever the session changed while the guard was alive — which is
    /// exactly what a line editor does to `ECHO` on every prompt.
    #[test]
    fn suspend_key_guard_restores_only_vsusp() {
        let pty = nix::pty::openpty(None, None).expect("open a private pty");
        // The master is retained for the whole test: closing it would hang up the slave.
        let _master = pty.master;
        let terminal: OpenFile = std::fs::File::from(pty.slave).into();
        let attributes = |what: &str| {
            tcgetattr(terminal.try_borrow_as_fd().expect("borrow the terminal")).expect(what)
        };

        let original = attributes("read the original attributes");
        let echo_before = original.local_flags.contains(LocalFlags::ECHO);

        let guard = SuspendKeyGuard::new(terminal.clone()).expect("disable the suspend character");
        let mut toggled = attributes("read the guarded attributes");
        assert_eq!(
            toggled.control_chars[VSUSP], _POSIX_VDISABLE,
            "the guard disables the suspend character while it lives"
        );

        // An unrelated mode change, made while the guard is alive, that the guard must not undo.
        toggled.local_flags.set(LocalFlags::ECHO, !echo_before);
        tcsetattr(
            terminal.try_borrow_as_fd().expect("borrow the terminal"),
            SetArg::TCSANOW,
            &toggled,
        )
        .expect("toggle ECHO");
        drop(guard);

        let restored = attributes("read the restored attributes");
        assert_eq!(
            restored.control_chars[VSUSP], original.control_chars[VSUSP],
            "the suspend character comes back"
        );
        assert_eq!(
            restored.local_flags.contains(LocalFlags::ECHO),
            !echo_before,
            "and the unrelated change made meanwhile survives"
        );
    }

    /// A job's terminal is created at the mux's size and resized in place afterwards, so both
    /// halves must agree: a resize applied to the master that the slave did not observe would
    /// leave every child reading a stale `TIOCGWINSZ`. The blocking-ness of each end and the
    /// disabled suspend character are the other properties the job runtime depends on.
    #[test]
    fn a_new_pty_reports_the_size_it_was_opened_with() {
        let (master, slave) = open_pty(24, 80).expect("open a private pty");

        let sizes = || {
            (
                terminal_size(master.as_fd()).expect("measure the master"),
                terminal_size(slave.as_fd()).expect("measure the slave"),
            )
        };
        assert_eq!(
            sizes(),
            ((24, 80), (24, 80)),
            "both halves report the size the pair was opened with"
        );
        resize_pty(master.as_fd(), 30, 100).expect("resize through the master");
        assert_eq!(
            sizes(),
            ((30, 100), (30, 100)),
            "a resize is retained, and the two descriptors share one window size"
        );

        let attributes = tcgetattr(slave.as_fd()).expect("read the slave attributes");
        assert_eq!(
            attributes.control_chars[VSUSP], _POSIX_VDISABLE,
            "Ctrl-Z never becomes a SIGTSTP on a freshly opened job terminal"
        );

        assert!(
            is_nonblocking(&master),
            "the master is non-blocking, so an async reactor can poll it"
        );
        assert!(
            !is_nonblocking(&slave),
            "the slave stays blocking, so a child's output is not lost to EAGAIN"
        );
    }
}
