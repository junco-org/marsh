//! Durable publication of one directory tree into another, through a write-ahead log.
//!
//! The model is a tree that a command never writes into — the *seed* — and a writable copy of it
//! the command does write into. When the command finishes, [`diff::diff_trees`] recovers what it
//! changed as a list of [`diff::CommitOp`] — or [`diff::diff_paths`], reading only the paths the
//! caller knows were written — and [`commit::prepare`] checks that list against the seed and
//! serializes it without writing anything. [`commit::PreparedTransaction::apply`] then publishes
//! it through an append-only log, so a crash at any point leaves the seed either untouched or
//! completable by a replay of [`commit::recover`].
//!
//! Nothing here knows what a command is, who ran it, or why it was allowed to. A transaction
//! carries whatever per-transaction metadata `M` its caller wants recorded beside the operations —
//! [`commit::prepare`] serializes it into the log's `BEGIN` record and [`commit::recover`] hands
//! it back — and this crate never inspects it.
//!
//! Nothing here knows about btrfs either, beyond refusing to publish across a subvolume or mount
//! boundary: the copy a transaction reads from is any directory. How that copy is made — a
//! read-only btrfs snapshot in practice — belongs to the caller.
//!
//! Everything is Linux-only: the code walks trees through descriptor-relative system calls and
//! names modes, mounts and inode identities the way Linux reports them.

pub mod commit;
pub mod diff;
mod error;
pub mod log;
#[cfg(test)]
#[allow(clippy::expect_used)]
mod testing;
mod tree;
mod types;

pub use commit::{LOG_FILE, PreparedTransaction, Transaction, WalRecord, prepare, recover};
pub use diff::{CommitOp, diff_paths, diff_trees};
pub use error::Error;
pub use log::JsonLog;
pub use types::{ContentHash, EntryKind, Mode, Seq, SourceUid, Staging};
