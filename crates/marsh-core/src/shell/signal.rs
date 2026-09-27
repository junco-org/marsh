//! Ordinary shell signals; process identity is pinned by native producer ownership.

/// A signal delivered to verified live shell work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Catchable terminal interrupt.
    Interrupt,
    /// Catchable termination request.
    Terminate,
    /// Uncatchable termination.
    Kill,
    /// Terminal hangup.
    Hangup,
    /// Continue a stopped process.
    Continue,
}
impl Signal {
    /// Standard signal name without the `SIG` prefix.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "INT",
            Self::Terminate => "TERM",
            Self::Kill => "KILL",
            Self::Hangup => "HUP",
            Self::Continue => "CONT",
        }
    }
    pub(super) const fn number(self) -> i32 {
        match self {
            Self::Interrupt => libc::SIGINT,
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Hangup => libc::SIGHUP,
            Self::Continue => libc::SIGCONT,
        }
    }
}
