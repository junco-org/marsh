//! Infrastructure failures.
//!
//! Every variant names a condition the publication layer could not proceed through: a log that
//! cannot be written or replayed, or an I/O failure underneath one. A command that merely failed is
//! not represented here — this crate never runs one.

/// Failure raised while logging or publishing a transaction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The write-ahead log is unusable or its recovery failed.
    #[error("write-ahead log failure: {0}")]
    Wal(String),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
