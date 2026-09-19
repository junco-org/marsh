//! `marsh::Shell`: brush inside a btrfs snapshot, every command line instrumented, checked against a
//! capability policy, and published through a write-ahead log — or discarded.
//!
//! A shell built here never writes into the tree it is pointed at. It runs inside one long-lived
//! writable btrfs snapshot of that tree — the *seed* — and a command line's effects reach the seed
//! only through a write-ahead log, and only once the policy has granted every capability they amount
//! to: a crash leaves the seed untouched or completable by a replay, a refused line leaves it
//! untouched. Alongside, every external command the shell spawns and every builtin it runs is
//! recorded, which is the only way effects inside the shell process can be attributed to the command
//! that caused them.
//!
//! Nothing here modifies brush. [`MarshExecutor`] is a `brush_core::extensions::ExternalCommandSpawner`,
//! selected statically through [`MarshShellExtensions`]; [`Shell::attach`] makes a built shell the
//! executor's — the `git` and `exec` builtins added, every builtin it holds instrumented — and wraps
//! it. Each line the [`Shell`] runs is staged in the snapshot, its requests are checked by
//! [`PolicyValidator`] — junco-policy's git legality over a history shared by every shell in the
//! process — and it is published only when every request was granted. A script running in the
//! shell cannot see the instrumentation, write to it, or turn it off.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let validator = brush_shell::marsh::PolicyValidator::global();
//! let shell = brush_shell::marsh::Shell::new(std::path::Path::new("/srv/seed"), validator).await?;
//! let (result, outcome) = shell.run("printf hi > greeting").await?;
//! # let _ = (result, outcome);
//! # Ok(())
//! # }
//! ```
//!
//! The pieces are separate crates: `marsh-btrfs` (seed discovery, lease, snapshots), `marsh-wal`
//! (diff and durable publication) and `marsh-instrument` (builtin hook and record vocabulary).
//! This module joins them to the shell; `entry` wires it to the command line, `builtins` holds the
//! `git` and `exec` builtins, `policy` the validator and the translation of a line's effects into
//! requests.

pub mod builtins;
mod error;
mod executor;
pub(crate) mod input;
pub mod policy;
mod session;
mod shell;

pub use error::MarshError;
pub use executor::{MarshExecutor, MarshShellExtensions};
pub use policy::{Denial, PolicyValidator};
pub use session::{Publication, PublishMeta};
pub use shell::{Outcome, Shell, ShellRef};
