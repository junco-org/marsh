//! `ShellMux`: a capability-gated shell multiplexer over one btrfs seed.
//!
//! One [`ShellMux`] owns a seed-level [`MarshExecutor`](crate::MarshExecutor) — the seed's
//! exclusive lease and its recovered write-ahead log — and one [`marsh::Shell`](crate::Shell) per
//! job, each over a snapshot of its own taken for that job's principal. A job's line is that
//! shell's [`run`](crate::Shell::run): the snapshot is refreshed from the seed when another
//! principal has published, the line runs, and the gate translates, checks and publishes or
//! discards it. Everything atomic about a command is the shell's; the mux never snapshots, diffs,
//! checks or publishes.
//!
//! What the mux owns is the multiplexing: the streams every job runs attached to, the job table
//! and the names it draws from, the pumps that keep every job draining whether or not anyone is
//! looking at it, and the delivery of all of that to one frontend.
//!
//! # The five things a caller must keep apart
//!
//! | Boundary | What it proves |
//! |---|---|
//! | [`CommandHandle::wait`] resolving | the line ran and the gate decided |
//! | `exit_code` | the *process* status, nothing about the seed |
//! | [`Outcome::Published`](crate::Outcome::Published) | and only this: the effects are in the seed |
//! | end of a pipe job's output | the program's writers are gone |
//! | [`Spawned::wait_closed`] resolving | every stream ended and the snapshot was reclaimed |
//!
//! A command can exit zero and be denied. A command can be discarded having already spawned
//! processes. Output observed live is provisional until the verdict says otherwise.
//!
//! # Terminal jobs and pipe jobs
//!
//! A [`JobIo::Terminal`] job runs on a pseudoterminal: one merged output stream, a size a program
//! can query, raw mode, terminal replies, and no end-of-file a writer can send. A [`JobIo::Pipes`]
//! job runs on three real pipes: byte-exact independent stdout and stderr with no promised order
//! between them, and a real end-of-file through [`ShellMux::close_input`]. The choice is made once
//! and never changes, because it decides what the bytes *mean*.
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

pub use command::{CommandCompletion, CommandHandle, CommandId, WaitError};
pub use context::{CommandContext, current_command_context};
pub use error::MuxError;
pub use frontend::{FrontendEvent, ShellFrontend};
pub use idle::IdleTerminal;
pub use ids::{JobDir, ShellId, SnapshotUid};
pub use jobs::{JobCloseMode, JobEnd, JobView, MuxSnapshot, OnFinish, RunningView, Spawned};
pub use mux::{Sandbox, ShellMux};
pub use types::{
    CommandOptions, ExecutorInfo, JobIo, MuxProfile, OutputChannel, SpawnOptions, TerminalGeometry,
};
