//! Durable publication of one directory tree into another, through a write-ahead log.
//!
//! The model is a tree that a command never writes into — the *seed* — and a writable copy of it
//! the command does write into. When the command finishes, [`diff::diff_trees`] recovers what it
//! changed as a list of [`diff::CommitOp`], and [`commit::apply`] publishes that list into the seed
//! through an append-only log, so a crash at any point leaves the seed either untouched or
//! completable by a replay of [`commit::recover`].
//!
//! Nothing here knows what a command is, who ran it, or why it was allowed to. A transaction
//! carries whatever per-transaction metadata `M` its caller wants recorded beside the operations —
//! [`commit::apply`] serializes it into the log's `BEGIN` record and [`commit::recover`] hands it
//! back — and this crate never inspects it.
//!
//! Nothing here knows about btrfs either: the copy a transaction reads from is any directory. How
//! that copy is made — a btrfs snapshot in practice — belongs to the caller.
//!
//! Everything is Linux-only: the code names symlinks, modes and inode identities through
//! `std::os::unix`.

pub mod commit;
pub mod diff;
mod error;
pub mod log;

pub use commit::{LOG_FILE, Transaction, WalRecord, apply, recover};
pub use diff::{CommitOp, diff_trees};
pub use error::Error;
pub use log::{JsonLog, TEMPORARY_SUFFIX, apply_directory, apply_remove, apply_write};
