//! `ShellMux`: a capability-gated collection of shells over the btrfs seeds those shells name.
//!
//! One [`ShellMux`] holds one [`Shell`] per principal, and one
//! [`marsh::Shell`](crate::Shell) inside each of those, over a snapshot taken for that
//! principal out of the seed its starting directory lies in. A line is that inner shell's
//! [`run`](crate::Shell::run): the snapshot is refreshed from the seed when another principal has
//! published, the line runs, and the gate translates, checks and publishes or discards it.
//! Everything atomic about a command is the shell's; the collection never snapshots, diffs,
//! checks or publishes.
//!
//! A seed — its exclusive lease, its recovered write-ahead log and the live capability history
//! judged against it — is opened by the first shell that asks for one and kept until the
//! collection is dropped. Seeds are isolated from each other: one's failed publication blocks
//! only its own shells, and one's grants are never visible to another's gate.
//!
//! # Obtain a shell, then run through it
//!
//! [`ShellMux::open_shell`] creates one and [`ShellMux::get_shell`] retrieves one by
//! [`Principal`](crate::policy::Principal); neither runs anything.
//! [`Shell::run_command`] is the only way a line runs, and it answers with the *completed*
//! verdict: `Ok` means [`Outcome::Published`](crate::Outcome::Published) and nothing else, so a
//! line the policy refused is [`RunError::Policy`] even when its process exited zero.
//!
//! Selection is not here. Which shell a display is looking at is the front-end's own state; the
//! collection indexes by principal and has no opinion about what anyone is watching.
//!
//! # The five things a caller must keep apart
//!
//! | Boundary | What it proves |
//! |---|---|
//! | [`Shell::run_command`] resolving | the line ran and the gate decided |
//! | `exit_code` | the *process* status, nothing about the seed |
//! | [`Outcome::Published`](crate::Outcome::Published) | and only this: the effects are in the seed |
//! | end of a pipe shell's output | the program's writers are gone |
//! | [`Shell::wait_closed`] resolving | every stream ended and the snapshot was reclaimed |
//!
//! A command can exit zero and be denied. A command can be discarded having already spawned
//! processes. Output observed live is provisional until the verdict says otherwise.
//!
//! # Terminal shells and pipe shells
//!
//! A [`JobIo::Terminal`] shell runs on a pseudoterminal: one merged output stream, a size a
//! program can query, raw mode, terminal replies, and no end-of-file a writer can send. A
//! [`JobIo::Pipes`] shell runs on three real pipes: byte-exact independent stdout and stderr with
//! no promised order between them, and a real end-of-file through [`Shell::close_input`]. The
//! choice is made once and never changes, because it decides what the bytes *mean*.
//!
//! # The frontend
//!
//! The frontend is a contract this crate delivers to and does not implement: [`ShellFrontend`] and
//! [`FrontendEvent`] are the whole of it, and a console, a full-screen UI or a terminal
//! multiplexer is the embedding application's.

mod command;
mod context;
mod error;
mod frontend;
mod idle;
mod ids;
pub mod jobctl;
mod jobs;
mod mux;
mod pipes;
pub mod pty;
pub mod repl;
mod types;

pub use command::{CommandCompletion, CommandHandle, CommandId, PolicyError, RunError, WaitError};
pub use context::{CommandContext, current_command_context};
pub use error::MuxError;
pub use frontend::{FrontendEvent, ShellFrontend};
pub use idle::IdleTerminal;
pub use ids::{JobDir, ShellId, SnapshotUid};
pub use jobs::{JobCloseMode, JobEnd, JobView, MuxSnapshot, OnFinish, RunningView, Shell};
pub use mux::{Sandbox, ShellMux};
pub use types::{
    CommandOptions, JobIo, MuxProfile, OutputChannel, SeedInfo, SpawnOptions, TerminalGeometry,
};
