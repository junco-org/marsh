//! Infrastructure failures.
//!
//! Every variant names a condition the storage layer could not proceed through: a seed that is not
//! on btrfs, a mount that will not let the user reclaim snapshots, a subvolume operation the kernel
//! refused. A command that merely failed is not represented here — this crate never runs one.

use std::path::PathBuf;

/// Failure raised while locating or snapshotting a seed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No btrfs subvolume contains the starting directory.
    #[error("no btrfs subvolume contains {0}")]
    NoSubvolume(PathBuf),
    /// The seed is its mount's root, so there is nowhere beside it to keep state.
    #[error("{0} is the root of its mount, so there is nowhere beside it for state")]
    SeedIsMountRoot(PathBuf),
    /// A directory named as a seed cannot be used.
    #[error("{path} cannot be used: {reason}")]
    SeedDir {
        /// The path as it was given.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },
    /// Another process currently owns this seed's session state.
    #[error("{0} already has an active session")]
    SessionBusy(PathBuf),
    /// The state directory is not on a btrfs filesystem, so snapshots are impossible.
    #[error("{0} is not on a btrfs filesystem")]
    NotBtrfs(PathBuf),
    /// The state directory's mount lacks `user_subvol_rm_allowed`, so snapshots cannot be
    /// reclaimed unprivileged.
    #[error("{0} is on a btrfs mount without `user_subvol_rm_allowed`")]
    NotUserSubvolRmAllowed(PathBuf),
    /// Something that is not a directory occupies a path the state layout needs.
    #[error("{0} exists and is not a directory")]
    StateNotDirectory(PathBuf),
    /// A run identifier that is not exactly one plain path component.
    #[error("invalid execution run id: {0:?}")]
    InvalidRunId(String),
    /// A subvolume create/snapshot/delete operation failed.
    #[error("btrfs snapshot operation failed: {0}")]
    Snapshot(String),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
