//! btrfs snapshot isolation for shells that run commands off to one side.
//!
//! The model is one directory tree — the *seed* — that is a btrfs subvolume, and a command that
//! never writes into it. A command runs inside a writable copy-on-write [snapshot](mod@snapshot)
//! of the seed; publishing what it changed back into the seed is `marsh-wal`'s job, not this
//! crate's.
//!
//! [`PersistenceLayer`] locates the seed and the state directory beside it, and owns the exclusive
//! lease that keeps two processes from publishing into one seed at the same time.
//!
//! Every btrfs operation goes through the [`Subvolumes`] trait. [`LibBtrfs`] is the real one; the
//! optional `fake` feature adds `fake::CopyTree`, which copies directories instead, so a caller's
//! own tests need no btrfs.
//!
//! Everything is Linux-only: btrfs subvolume ioctls exist nowhere else. On other platforms the
//! crate compiles to an empty library rather than failing to build, so a portable caller can depend
//! on it unconditionally and gate its own use.

#[cfg(target_os = "linux")]
mod error;
#[cfg(all(target_os = "linux", feature = "fake"))]
pub mod fake;
#[cfg(target_os = "linux")]
pub mod persistence;
#[cfg(target_os = "linux")]
pub mod snapshot;

#[cfg(target_os = "linux")]
pub use error::Error;
#[cfg(target_os = "linux")]
pub use persistence::{PersistenceLayer, STATE_DIR, short_id};
#[cfg(target_os = "linux")]
pub use snapshot::{LibBtrfs, Subvolumes, delete_subvolume, snapshot};
