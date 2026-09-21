//! Infrastructure failures of an attached session.

/// Failure raised while attaching to a seed, publishing into it or retaking its snapshot, or
/// recording what was dispatched.
#[derive(Debug, thiserror::Error)]
pub enum MarshError {
    /// Locating, snapshotting, or leasing the seed failed.
    #[error(transparent)]
    Btrfs(#[from] marsh_btrfs::Error),
    /// Logging or publishing a transaction failed.
    #[error(transparent)]
    Wal(#[from] marsh_wal::Error),
    /// A record stream could not be serialized.
    #[error("instrumentation dump failed: {0}")]
    Dump(#[from] serde_json::Error),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The shell failed to run a line.
    #[error(transparent)]
    Shell(#[from] brush_core::Error),
    /// The executor names a seed but carries no snapshot, so a shell over it would stage nothing.
    #[error(
        "the executor names a seed but no snapshot; take one with MarshExecutor::snapshot before \
         building the shell"
    )]
    NoSnapshot,
    /// An approved publication failed partway through, and its durable log still has to be
    /// replayed before anything else may be published into this seed.
    ///
    /// The snapshot whose publication failed is kept on disk as the recovery source. Continuing
    /// against a seed that is partially applied would publish on top of a state nobody has
    /// verified, so every gate refuses until an explicit reopen has replayed the log.
    #[error(
        "an approved publication failed and has not been recovered; reopen the seed to replay \
         its write-ahead log"
    )]
    RecoveryRequired,
    /// No job matched the given job specification.
    #[error("no such job: {0}")]
    NoSuchJob(String),
}
