//! Infrastructure failures of the multiplexer.
//!
//! `MuxError` is reserved for the mux *breaking*: a job directory that names nothing, a name two
//! jobs would answer to, a terminal that refused an ioctl, a seed that could not be leased. Domain
//! outcomes — a line whose capabilities were denied, a line that lost a race — are **not** errors;
//! they are [`Outcome`](crate::Outcome) variants, because they are answers the gate computed
//! successfully.

use std::path::PathBuf;
use std::sync::Arc;

use crate::MarshError;
use crate::shellmux::ids::ShellId;

/// Infrastructure failure raised by the mux.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// The shell layer failed: seed discovery, the lease, a snapshot, a publication, the shell
    /// itself.
    #[error(transparent)]
    Marsh(#[from] MarshError),
    /// A job directory escapes the seed or names nothing in it.
    #[error("{path} cannot be used as a job directory: {reason}")]
    SandboxDir {
        /// The directory as the user typed it.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },
    /// A job name a live job already holds.
    #[error("{} already exists", .0.reference())]
    JobExists(ShellId),
    /// A job name nothing in the table answers to.
    #[error("no such job: {}", .0.reference())]
    NoSuchJob(ShellId),
    /// A command was submitted to a job that is already running one.
    #[error("{} is already running a command", .0.reference())]
    JobBusy(ShellId),
    /// A command was submitted to a job an accepted stop has already closed.
    #[error("{} is closing", .0.reference())]
    JobClosing(ShellId),
    /// A terminal geometry with a zero dimension, which no job could be given.
    #[error("invalid terminal size: {rows}x{cols}")]
    InvalidTerminalSize {
        /// Requested height in character cells.
        rows: u16,
        /// Requested width in character cells.
        cols: u16,
    },
    /// A forcibly stopped job's processes could not be signalled.
    #[error("could not terminate job {}: {source}", .job.reference())]
    JobTermination {
        /// The job whose processes outlived the kill.
        job: ShellId,
        /// Why the kill failed.
        #[source]
        source: std::io::Error,
    },
    /// A handle from a different mux was passed to this one.
    #[error("{} belongs to a different shell multiplexer", .0.reference())]
    ForeignJob(ShellId),
    /// The job this handle names has closed, and the name may since have been reused.
    ///
    /// Distinct from [`Self::NoSuchJob`], which is a name nothing answers to: this one says the
    /// *instance* is gone. A handle that outlived its job can never reach the replacement that
    /// took its name, because the two differ in [`Sandbox::uid`](crate::shellmux::Sandbox).
    #[error("{} has closed", .0.reference())]
    StaleJob(ShellId),
    /// The job exists but its terminal, pipes or shell are not open yet.
    #[error("{} is still opening", .0.reference())]
    JobNotReady(ShellId),
    /// A terminal-only operation was asked of a pipe job.
    ///
    /// Resizing, selecting and leasing an idle terminal all need a terminal. A pipe job has none,
    /// and inventing one would be a lie a caller could act on.
    #[error("{} is not a terminal job", .0.reference())]
    NotTerminal(ShellId),
    /// A pipe-only operation was asked of a terminal job.
    ///
    /// Closing standard input is the one that matters: a pseudoterminal has no half-close, and
    /// faking one with an end-of-transmission character would be a keystroke, not an end of file.
    #[error("{} is not a piped job", .0.reference())]
    NotPiped(ShellId),
    /// Standard input was written after it had been closed.
    #[error("{}'s input is closed", .0.reference())]
    InputClosed(ShellId),
    /// A second idle-terminal lease was asked for while one was outstanding, or the job is busy.
    #[error("{}'s terminal is already leased or running a command", .0.reference())]
    TerminalBusy(ShellId),
    /// The mux is shutting down and admits no new work.
    #[error("the shell multiplexer is shutting down")]
    ShuttingDown,
    /// An approved publication failed; the seed needs its log replayed before anything else runs.
    #[error(
        "an approved publication failed and has not been recovered; reopen the seed to replay \
         its write-ahead log"
    )]
    RecoveryRequired,
    /// A command context was asked to start native work after its command stopped admitting it.
    #[error("command {0} is finalizing and admits no further work")]
    CommandFinalizing(crate::shellmux::CommandId),
    /// A failure that had to be observed in more than one place.
    ///
    /// A job whose construction fails reports that failure to its caller, to the receipt of the
    /// command it was opened for, and in its own [`JobEnd`](crate::shellmux::JobEnd). Neither
    /// [`MuxError`] nor the errors it wraps are cloneable, and flattening them to a string at that
    /// point would throw away the source chain every one of those readers may want.
    #[error(transparent)]
    Shared(Arc<MuxError>),
    /// Filesystem or terminal I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A task the mux owns panicked or was cancelled.
    #[error("background task failed: {0}")]
    Task(String),
}

impl From<brush_core::Error> for MuxError {
    /// A shell failure travels as the shell layer's own error, so a caller matching on
    /// [`MuxError::Marsh`] sees every shell failure in one place.
    fn from(error: brush_core::Error) -> Self {
        Self::Marsh(MarshError::Shell(error))
    }
}
