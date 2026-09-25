use rmux_proto::WebTerminalPalette;

/// Unix palette capture that queries the controlling terminal with `OSC` color requests.
mod imp {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use super::WebTerminalPalette;

    const QUERY_TIMEOUT: Duration = Duration::from_millis(800);
    const READ_POLL_SLICE: Duration = Duration::from_millis(25);
    const QUIET_DRAIN_TIMEOUT: Duration = Duration::from_millis(80);
    const READ_BUF_SIZE: usize = 4096;

    /// Opens `/dev/tty` and asks it for its palette, yielding `None` when there is no terminal.
    pub(super) fn capture() -> Option<WebTerminalPalette> {
        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        capture_from_tty(&mut tty)
    }

    /// Puts `tty` in raw mode, writes the palette query, and parses the replies until timeout.
    fn capture_from_tty(tty: &mut std::fs::File) -> Option<WebTerminalPalette> {
        let fd = tty.as_raw_fd();
        let original = TermiosGuard::new(fd)?;

        tty.write_all(query_bytes().as_bytes()).ok()?;
        tty.flush().ok()?;

        let mut bytes = Vec::new();
        let mut theme = None;
        let deadline = Instant::now() + QUERY_TIMEOUT;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !poll_readable(fd, remaining.min(READ_POLL_SLICE)) {
                continue;
            }
            if !read_available(tty, &mut bytes) {
                break;
            }
            theme = parse_theme(&String::from_utf8_lossy(&bytes));
            if theme.is_some() {
                break;
            }
        }

        drain_quiet_period(fd, tty, &mut bytes, QUIET_DRAIN_TIMEOUT);
        let _ = flush_input(fd);

        drop(original);
        theme.or_else(|| parse_theme(&String::from_utf8_lossy(&bytes)))
    }

    /// Builds the `OSC` query string for foreground, background, cursor, and the 16 `ANSI` slots.
    fn query_bytes() -> String {
        use std::fmt::Write as _;

        let mut query = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]12;?\x1b\\");
        for index in 0..16 {
            let _ = write!(query, "\x1b]4;{index};?\x1b\\");
        }
        query
    }

    /// Extracts a complete palette from accumulated terminal replies, or `None` if any is missing.
    fn parse_theme(input: &str) -> Option<WebTerminalPalette> {
        let foreground = parse_osc_color(input, "10")?;
        let background = parse_osc_color(input, "11")?;
        let cursor = parse_osc_color(input, "12").unwrap_or_else(|| foreground.clone());
        let ansi: [Option<String>; 16] =
            std::array::from_fn(|index| parse_osc_color(input, &format!("4;{index}")));
        let ansi = ansi.into_iter().collect::<Option<Vec<_>>>()?;
        Some(WebTerminalPalette {
            foreground,
            background,
            cursor,
            ansi: ansi.try_into().ok()?,
        })
    }

    /// Finds the reply for `OSC` `code` in `input` and returns its color as a hex string.
    fn parse_osc_color(input: &str, code: &str) -> Option<String> {
        for terminator in ["\x1b\\", "\x07"] {
            let prefix = format!("\x1b]{code};");
            for segment in input.split(terminator) {
                let Some(value) = segment.strip_prefix(&prefix) else {
                    continue;
                };
                if let Some(hex) = parse_rgb(value) {
                    return Some(hex);
                }
            }
        }
        None
    }

    /// Converts an `X11` `rgb:r/g/b` color specification into a `#rrggbb` hex string.
    fn parse_rgb(value: &str) -> Option<String> {
        let rgb = value.strip_prefix("rgb:")?;
        let mut parts = rgb.split('/');
        let red = scale_channel(parts.next()?)?;
        let green = scale_channel(parts.next()?)?;
        let blue = scale_channel(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(format!("#{red:02x}{green:02x}{blue:02x}"))
    }

    /// Scales one hex color channel of 1 to 4 digits down to 8 bits, rounding to nearest.
    fn scale_channel(value: &str) -> Option<u8> {
        let digits = value.len();
        if digits == 0 || digits > 4 {
            return None;
        }
        let raw = u16::from_str_radix(value, 16).ok()?;
        let max = (1u32 << (digits * 4)) - 1;
        u8::try_from((u32::from(raw) * 255 + (max / 2)) / max).ok()
    }

    /// Waits up to `timeout` for `fd` to become readable.
    fn poll_readable(fd: libc::c_int, timeout: Duration) -> bool {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: `pollfd` points to a valid single-entry array for the duration of the call,
        // and `fd` is an open terminal descriptor owned by the caller.
        unsafe {
            libc::poll(std::ptr::from_mut(&mut pollfd), 1, timeout_ms) > 0
                && pollfd.revents & libc::POLLIN != 0
        }
    }

    /// Appends everything currently readable from `tty` to `bytes`, returning `false` on error.
    fn read_available(tty: &mut std::fs::File, bytes: &mut Vec<u8>) -> bool {
        loop {
            let mut buf = [0; READ_BUF_SIZE];
            match tty.read(&mut buf) {
                Ok(0) => return true,
                Ok(n) => {
                    bytes.extend_from_slice(&buf[..n]);
                    if n < READ_BUF_SIZE {
                        return true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
    }

    /// Keeps collecting late replies until `timeout` elapses without useful input.
    fn drain_quiet_period(
        fd: libc::c_int,
        tty: &mut std::fs::File,
        bytes: &mut Vec<u8>,
        timeout: Duration,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !poll_readable(fd, remaining.min(READ_POLL_SLICE)) {
                continue;
            }
            if !read_available(tty, bytes) {
                break;
            }
        }
    }

    /// Discards unread terminal input so stray late replies are not echoed by the user's shell.
    fn flush_input(fd: libc::c_int) -> std::io::Result<()> {
        // Best-effort cleanup for terminal emulators that answer OSC palette queries late.
        // Without this, unread replies can be consumed and echoed by the user's shell.
        // SAFETY: `fd` is borrowed for a `tcflush` call that only affects the terminal input
        // queue. Invalid descriptors are reported through the libc return value.
        if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Restores the terminal's original `termios` settings when dropped.
    struct TermiosGuard {
        fd: libc::c_int,
        original: libc::termios,
    }

    impl TermiosGuard {
        /// Saves the current attributes of `fd` and switches it to noncanonical, unechoed reads.
        fn new(fd: libc::c_int) -> Option<Self> {
            // SAFETY: `libc::termios` is a plain C struct whose all-zero value is only used as
            // an output buffer before being read after a successful `tcgetattr` call.
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            // SAFETY: `original` is a valid writable termios buffer and `fd` is expected to be
            // an open terminal descriptor.
            if unsafe { libc::tcgetattr(fd, std::ptr::from_mut(&mut original)) } != 0 {
                return None;
            }
            let mut raw = original;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            // SAFETY: `raw` was derived from a termios value returned by `tcgetattr` for this
            // descriptor, with only documented local-mode/control-byte fields adjusted.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, std::ptr::from_ref(&raw)) } != 0 {
                return None;
            }
            Some(Self { fd, original })
        }
    }

    impl Drop for TermiosGuard {
        /// Puts the saved attributes back, ignoring failures during unwinding.
        fn drop(&mut self) {
            // SAFETY: `original` was captured from this descriptor by `tcgetattr`; restoring it
            // is best-effort and the return value is intentionally ignored during drop.
            let _ = unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, std::ptr::from_ref(&self.original))
            };
        }
    }

    #[cfg(test)]
    #[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
    mod tests {
        use super::*;
        use std::fs::File;
        use std::io::Write;
        use std::os::fd::FromRawFd;
        use std::thread;

        fn palette_reply() -> String {
            use std::fmt::Write as _;

            let mut input = "\x1b]10;rgb:eeee/eeee/eeee\x1b\\\
                         \x1b]11;rgb:3333/4444/5555\x1b\\\
                         \x1b]12;rgb:ffff/0000/0000\x1b\\"
                .to_owned();
            for index in 0..16 {
                let _ = write!(input, "\x1b]4;{index};rgb:{index:04x}/0000/ffff\x1b\\");
            }
            input
        }

        #[test]
        fn parses_vte_palette_replies() {
            let input = palette_reply();

            let theme = parse_theme(&input).expect("valid theme");

            assert_eq!(theme.foreground, "#eeeeee");
            assert_eq!(theme.background, "#334455");
            assert_eq!(theme.cursor, "#ff0000");
            assert_eq!(theme.ansi[0], "#0000ff");
            assert_eq!(theme.ansi[15], "#0000ff");
        }

        #[test]
        fn delayed_palette_replies_are_captured() {
            let (master, mut slave) = open_pty_pair();
            let mut responder_master = master.try_clone().expect("clone pty master");
            let reply = palette_reply();
            let responder = thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                responder_master
                    .write_all(reply.as_bytes())
                    .expect("write reply");
            });

            let theme = capture_from_tty(&mut slave).expect("delayed theme reply");
            responder.join().expect("responder thread");

            assert_eq!(theme.foreground, "#eeeeee");
            assert_eq!(theme.background, "#334455");
            assert_eq!(theme.cursor, "#ff0000");
            assert_eq!(theme.ansi[15], "#0000ff");
        }

        fn open_pty_pair() -> (File, File) {
            let mut master = -1;
            let mut slave = -1;
            // SAFETY: `master` and `slave` are valid writable out-pointers. Null optional
            // arguments request libc defaults, and success initializes both descriptors.
            let result = unsafe {
                libc::openpty(
                    std::ptr::from_mut(&mut master),
                    std::ptr::from_mut(&mut slave),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                result,
                0,
                "openpty failed: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: `openpty` returned success, so the master descriptor is initialized and
            // its ownership is transferred exactly once into `File`.
            let master = unsafe { File::from_raw_fd(master) };
            // SAFETY: `openpty` returned success, so the slave descriptor is initialized and
            // its ownership is transferred exactly once into `File`.
            let slave = unsafe { File::from_raw_fd(slave) };
            (master, slave)
        }
    }
}

/// Best-effort capture of the local terminal palette for `web-share`.
pub(crate) fn capture_terminal_palette() -> Option<WebTerminalPalette> {
    imp::capture()
}
