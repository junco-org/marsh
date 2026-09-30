//! Multiplexing of ordinary managed shells: reusable names, terminal/pipe ownership and receipts.
//!
//! A command's `result` is the Shell's sole verdict. Native status remains independently readable
//! through `CommandCompletion::exit_code`, including after denial or finalization failure.

mod command;
mod error;
mod frontend;
mod idle;
mod ids;
mod jobs;
mod mux;
mod pipes;
pub mod pty;
#[cfg(test)]
#[allow(clippy::expect_used)]
mod testing;
mod types;

pub use command::{CommandCompletion, CommandHandle, CommandId, RunError, WaitError};
pub use error::MuxError;
pub use frontend::{FrontendEvent, ShellFrontend};
pub use idle::IdleTerminal;
pub use ids::{JobDir, Principal, ShellId};
pub use jobs::{JobView, MuxSnapshot, RunningView, Shell};
pub use mux::{Sandbox, ShellMux};
pub use types::{
    CommandOptions, JobCloseMode, JobEnd, JobIo, MuxProfile, OutputChannel, SpawnOptions,
    TerminalGeometry,
};
