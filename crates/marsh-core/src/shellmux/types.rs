//! The records the multiplexer's operations take and answer with: geometry, I/O shape, the
//! per-operation options, a shell's closure record, and the one profile every shell of a mux is
//! built from.
//!
//! Nothing here holds a lock, a descriptor or a handle. These are plain values, so a caller can
//! build them before it owns anything and a frontend can copy them out of a callback.

use std::collections::HashMap;
use std::sync::Arc;

use crate::shellmux::{CommandCompletion, MuxError, Sandbox};

/// Why a shell is to close once its command finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobCloseMode {
    /// A shell opened with [`SpawnOptions::automatic_close`]; a reader may keep it.
    Automatic,
    /// Close after normal command completion, because a reader or the command's own options asked.
    Graceful,
    /// Abort and retire the shell immediately.
    Force,
}

impl JobCloseMode {
    /// Whether this closure is a decision rather than the `1`, `2`, … series' default.
    pub(crate) const fn explicit(self) -> bool {
        !matches!(self, Self::Automatic)
    }
}

/// How one shell ended.
///
/// Delivered once per shell, after every byte of every one of its streams and after its snapshot
/// has been reclaimed. A shell whose construction failed reports that failure here without ever
/// having been [`Opened`](crate::shellmux::FrontendEvent::Opened).
#[derive(Debug)]
pub struct JobEnd {
    /// The shell that ended.
    pub shell: Sandbox,
    /// Why it closed, or `None` when its streams simply ended.
    pub close_mode: Option<JobCloseMode>,
    /// The verdict of the last command that ran in it, when one did.
    pub completion: Option<Arc<CommandCompletion>>,
    /// The infrastructure failure that ended it, when one did.
    ///
    /// A construction that never produced a usable shell reports here. This is not an exit status
    /// and must never be rendered as one.
    pub error: Option<Arc<MuxError>>,
}

/// A terminal's size, in character cells.
///
/// Rows before columns, in that order, everywhere in this crate. There are no pixel dimensions:
/// the kernel's `winsize` carries two, but nothing in this engine reads them, every job sets them
/// to zero, and a second geometry that could disagree with this one would only drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalGeometry {
    /// Height, in character cells. Never zero in an accepted geometry.
    pub rows: u16,
    /// Width, in character cells. Never zero in an accepted geometry.
    pub cols: u16,
}

impl TerminalGeometry {
    /// Whether both dimensions are usable.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.rows != 0 && self.cols != 0
    }
}

/// What a shell's standard descriptors are.
///
/// The choice is made once, at [`open_shell`](crate::shellmux::ShellMux::open_shell), and never
/// changes: a terminal shell and a pipe shell differ in what their bytes *mean*, not only in how
/// they are carried, so converting one into the other afterwards would silently change the
/// semantics a running program already depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobIo {
    /// One pseudoterminal on fds 0, 1 and 2.
    ///
    /// Output is a single merged stream — the program's stdout, its stderr and the terminal's own
    /// echo, interleaved by the kernel — because that is what a terminal is. A program can ask it
    /// for its size, put it in raw mode, and reply to a terminal query through it. There is no
    /// half-close: a terminal has no end-of-file a writer can send.
    Terminal {
        /// The size to open it at, or `None` for the mux's current default.
        ///
        /// A [`JobView`](crate::shellmux::JobView) never reports `None`: by the time a job is
        /// visible its terminal has a resolved size, and reporting the request rather than the
        /// answer would make a caller guess.
        geometry: Option<TerminalGeometry>,
    },
    /// Three ordinary byte pipes on fds 0, 1 and 2.
    ///
    /// Stdout and stderr are independent streams with independent ordering and independent
    /// backpressure; there is no relative order between them and none is invented. Closing stdin
    /// is a real end-of-file the program observes. Nothing about this is a terminal: `isatty` is
    /// false, there is no size to query, and a full-screen program will not work.
    Pipes,
}

impl JobIo {
    /// Whether this is a pseudoterminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal { .. })
    }

    /// The requested size, or `None` for pipes or for a terminal asking for the default.
    pub(crate) const fn geometry(self) -> Option<TerminalGeometry> {
        match self {
            Self::Terminal { geometry } => geometry,
            Self::Pipes => None,
        }
    }

    /// This shape with a terminal's unspecified size resolved to `default`.
    pub(crate) const fn resolved(self, default: TerminalGeometry) -> Self {
        match self {
            Self::Terminal { geometry: None } => Self::Terminal {
                geometry: Some(default),
            },
            other => other,
        }
    }
}

/// Which of a job's output streams a chunk of bytes came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutputChannel {
    /// A terminal job's single merged stream. Never produced by a pipe job.
    Terminal,
    /// A pipe job's standard output. Never produced by a terminal job.
    Stdout,
    /// A pipe job's standard error. Never produced by a terminal job.
    Stderr,
}

/// How a shell is to be opened.
#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// The shell's standard descriptors.
    pub io: JobIo,
    /// Variables replacing the mux profile's for this shell alone, or `None` to inherit them.
    ///
    /// `Some` *replaces* the ordinary inherited and profile variables rather than merging with
    /// them; it does not resurrect a variable the caller unset. Marsh's own principal, git and
    /// snapshot variables are reapplied afterwards, because they are the shell's identity rather
    /// than the caller's configuration.
    pub environment: Option<brush_core::env::ShellEnvironment>,
    /// Whether this shell reclaims itself once a command run in it ends.
    ///
    /// Terminal shells only, and cancellable by
    /// [`Shell::keep`](crate::shellmux::Shell::keep): it is the `1`, `2`, … series' default
    /// rather than a decision, which is the one closure a reader taking an interest may revoke.
    ///
    /// `false` — the default — is an ordinary persistent shell: opening one runs nothing, and it
    /// outlives every command submitted into it until something stops it.
    pub automatic_close: bool,
}

impl Default for JobIo {
    /// A pseudoterminal at the mux's current default geometry.
    fn default() -> Self {
        Self::Terminal { geometry: None }
    }
}

/// How one command submitted into an open shell is to be treated.
#[derive(Default)]
pub struct CommandOptions {
    /// Whether the shell closes once this command ends.
    ///
    /// Recorded on the *command* when it is admitted, and applied to the shell in the same
    /// critical section that releases the running slot: neither a `keep` nor a second submission
    /// can slip into the window between the command's last byte and its verdict, while standard
    /// input stays writable for the whole of the command that is still running.
    ///
    /// Terminal shells only. A pipe shell is one-shot by construction and closes after its first
    /// admitted command whatever this says.
    pub close_on_finish: bool,
    /// Sent the command's receipt once, the moment it is admitted.
    ///
    /// [`Shell::run_command`](crate::shellmux::Shell::run_command) resolves with the *completed*
    /// verdict, which is the wrong moment for a caller that has to act while the command runs:
    /// feed its standard input, close that input so a program reading to end of file can finish,
    /// signal it, or attach an observer. This is that moment — after admission and after the
    /// idle-terminal lease is back, and before interpretation can emit a byte or finish.
    ///
    /// A failed admission drops the sender rather than inventing a receipt, and a dropped
    /// receiver cancels nothing: the command was accepted and remains the collection's.
    pub on_accept: Option<tokio::sync::oneshot::Sender<crate::shellmux::CommandHandle>>,
}

impl std::fmt::Debug for CommandOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandOptions")
            .field("close_on_finish", &self.close_on_finish)
            .field("on_accept", &self.on_accept.is_some())
            .finish()
    }
}

/// Ordinary shell configuration applied independently to every shell built by this mux.
#[derive(Default)]
pub struct MuxProfile {
    /// Variables seeded into every shell, on top of what the process inherited.
    pub environment: brush_core::env::ShellEnvironment,
    /// Builtins registered on every shell in addition to stock brush's and marsh's own.
    ///
    /// Native work uses [`crate::builtins::current_context`] for logical I/O and cancellation.
    pub builtins: HashMap<String, crate::builtins::Registration>,
}

impl std::fmt::Debug for MuxProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.builtins.keys().map(String::as_str).collect();
        names.sort_unstable();
        formatter
            .debug_struct("MuxProfile")
            .field("variables", &self.environment.iter().count())
            .field("builtins", &names)
            .finish()
    }
}
