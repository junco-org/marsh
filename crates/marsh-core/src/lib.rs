//! Persistent in-process shells whose [`SandboxPolicy`] routes every command either through an
//! automatic snapshot with native observation, capability checks and durable publication, or
//! directly to its source. Callers construct, execute and close shells; storage is private.
//!
//! ```no_run
//! # async fn example() -> Result<(), marsh_core::ShellError> {
//! let shell = marsh_core::Shell::new(std::path::Path::new("/srv/source")).await?;
//! let result = shell.run("printf hi > greeting").await?;
//! assert_eq!(u8::from(result.exit_code), 0);
//! shell.close(false).await?;
//! # Ok(())
//! # }
//! ```

mod shell;
pub mod shellmux;

#[cfg(feature = "testing")]
pub mod test_support;

pub use shell::{
    Action, CommandContext, Denial, Event, ExecutionParameters, ExecutionResult, MarshTool,
    OpenFile, PolicyDecision, PolicyObserver, PolicyValidator, Principal, ProfileLoadBehavior,
    RcLoadBehavior, SandboxPolicy, Shell, ShellBuilder, ShellCommand, ShellEnvironment, ShellError,
    ShellErrorKind, ShellFd, ShellVariable, Signal, SourceInfo, UIOptions, builtins,
};
