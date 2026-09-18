//! Infrastructure failures of an attached executor.

/// Failure raised while attaching to a seed, publishing into it, or recording what was dispatched.
#[derive(Debug, thiserror::Error)]
pub enum MarshError {
    /// Locating, snapshotting, or leasing the seed failed.
    #[error(transparent)]
    Btrfs(#[from] brush_btrfs::Error),
    /// Logging or publishing a transaction failed.
    #[error(transparent)]
    Wal(#[from] brush_wal::Error),
    /// A record stream could not be serialized.
    #[error("instrumentation dump failed: {0}")]
    Dump(#[from] serde_json::Error),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The shell failed to run a line.
    #[error(transparent)]
    Shell(#[from] brush_core::Error),
    /// The executor was built with `Default` and is not attached to a seed.
    #[error("executor is not attached to a seed")]
    Detached,
}
