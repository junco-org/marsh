//! `MarshExecutor`: btrfs-snapshotted, WAL-published, instrumented command execution for a *stock*
//! brush shell.
//!
//! A shell built with this executor never writes into the tree it is pointed at. It runs inside one
//! long-lived writable btrfs snapshot of that tree — the *seed* — and each command's effects are
//! published back into the seed through a write-ahead log, so a crash leaves the seed either
//! untouched or completable by a replay. Alongside, every external command the shell spawns and
//! every builtin it runs is recorded, which is the only way effects inside the shell process can be
//! attributed to the command that caused them.
//!
//! Nothing here modifies brush. [`MarshExecutor`] is a
//! `brush_core::extensions::ExternalCommandSpawner`, selected statically through
//! [`MarshShellExtensions`]; [`MarshExecutor::builtins`] is a stock builtin map with `git` added
//! and each registration's public `execute_func` wrapped. A script running in the shell cannot see
//! the instrumentation, write to it, or turn it off.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let executor = brush_extensions::MarshExecutor::open(std::path::Path::new("/srv/seed"))?;
//! let mut shell = brush_extensions::build_shell(&executor).await?;
//! let (result, publication) = executor.run(&mut shell, "printf hi > greeting").await?;
//! # let _ = (result, publication);
//! # Ok(())
//! # }
//! ```
//!
//! The pieces are separate crates: `brush-btrfs` (seed discovery, lease, snapshots), `brush-wal`
//! (diff and durable publication), `brush-instrument` (builtin hook and record vocabulary) and
//! `brush-builtin` (the `git` builtin). This crate is the one that joins them to a shell.

mod error;
mod executor;
mod session;
mod shell;

pub use brush_builtin::SNAPSHOT_ROOT_VAR;
pub use error::MarshError;
pub use executor::{MarshExecutor, MarshShellExtensions};
pub use session::{Publication, PublishMeta};
pub use shell::build_shell;
