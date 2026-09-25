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
//! Everything here is btrfs, so everything here is Linux: the subvolume ioctls exist nowhere else.

mod error;
#[cfg(feature = "fake")]
pub mod fake;
pub mod persistence;
pub mod snapshot;

pub use error::Error;
pub use persistence::{PersistenceLayer, STATE_DIR, short_id};
pub use snapshot::{LibBtrfs, Subvolumes, delete_subvolume, snapshot};
