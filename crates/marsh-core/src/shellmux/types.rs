//! The records the multiplexer's operations take and answer with: geometry, I/O shape, the
//! per-operation options, and the one profile every shell of a mux is built from.
//!
//! Nothing here holds a lock, a descriptor or a handle. These are plain values, so a caller can
//! build them before it owns anything and a frontend can copy them out of a callback.

use std::collections::HashMap;
use std::path::PathBuf;

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
    /// Whether a named shell is a stable agent identity whose capabilities survive closing and
    /// reopening the seed, as [`ShellId::durable`](crate::shellmux::ShellId::durable) describes.
    ///
    /// An explicit authority choice by the embedding caller, which must control the name and keep
    /// it stable for the same agent. Ignored for an unnamed shell: the `1`, `2`, … series reuses
    /// its numbers by design, so such a shell is always session-local.
    pub durable: bool,
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
    /// Called once when this command ends, when a caller asked to be told.
    ///
    /// One slot per shell, kept for the callers that only have to stop waiting.
    /// [`CommandHandle::wait`](crate::shellmux::CommandHandle::wait) is the cloneable form and
    /// does not occupy it.
    pub on_finish: Option<crate::shellmux::OnFinish>,
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
            .field("on_finish", &self.on_finish.is_some())
            .field("close_on_finish", &self.close_on_finish)
            .field("on_accept", &self.on_accept.is_some())
            .finish()
    }
}

/// The one shell profile a mux builds every one of its shells from.
///
/// Frozen at [`ShellMux::new`](crate::shellmux::ShellMux::new) and applied identically to panes,
/// popups and hidden helper jobs. That uniformity is not a convenience:
/// [`MarshExecutor::attach`](crate::MarshExecutor) installs *one* process-wide instrumentation
/// table, so every attached shell in the process must hold the same builtin names — a shell
/// carrying a builtin the latest installation does not know fails that builtin outright. Register
/// every extra builtin here; never after a shell is attached.
#[derive(Default)]
pub struct MuxProfile {
    /// Variables seeded into every shell, on top of what the process inherited.
    pub environment: brush_core::env::ShellEnvironment,
    /// Builtins registered on every shell in addition to stock brush's and marsh's own.
    ///
    /// Registered *before* `Shell::attach`, so the instrumentation covers them exactly as it
    /// covers `git` and `exec`. A builtin doing native work reaches its command through
    /// [`current_command_context`](crate::shellmux::current_command_context).
    pub builtins: HashMap<String, brush_core::builtins::Registration<crate::MarshShellExtensions>>,
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

/// What a mux will say about one seed it has opened, without handing its executor out.
///
/// The raw [`MarshExecutor`](crate::MarshExecutor) is deliberately not reachable through a mux: it
/// is both an ungated spawner and a publication capability, and a caller that only wants to know
/// where a seed is must not have to hold one to find out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedInfo {
    /// The canonical seed, which is also the key every query about it is asked with.
    pub seed: PathBuf,
    /// The directory whose immediate children are this seed's per-job snapshots.
    ///
    /// Deliberately not called `snapshot_root`: that is
    /// [`JobView::snapshot_root`](crate::shellmux::JobView::snapshot_root), one *job's own* tree,
    /// and the two were once spelled alike. This is their shared parent.
    ///
    /// What it is for: recognizing a path a client handed over from inside one of this daemon's
    /// panes. A pane's shell starts at its job's snapshot, so it spells its own directory
    /// `<snapshot_parent>/<uid>/<rest>` — the same place in the seed as `<seed>/<rest>`.
    pub snapshot_parent: PathBuf,
    /// Whether an approved publication failed and this seed's durable log still has to be
    /// replayed.
    ///
    /// While this is true the session admits no new work and every gate over *this* seed refuses:
    /// continuing against a partially applied seed would publish on top of a state nobody has
    /// verified. Other seeds of the same mux are unaffected. The snapshot that failed is kept on
    /// disk as the recovery source, and an explicit reopen is what replays it.
    pub recovery_required: bool,
}
