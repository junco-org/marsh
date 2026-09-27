//! Persistent in-process shells with automatic snapshot, native observation, capability checks
//! and durable publication. Callers construct, execute and close shells; storage is private.
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
    Denial, ExecutionParameters, ExecutionResult, OpenFile, Principal, ProfileLoadBehavior,
    RcLoadBehavior, Shell, ShellBuilder, ShellEnvironment, ShellError, ShellErrorKind, ShellFd,
    ShellVariable, Signal, SourceInfo, UIOptions, builtins,
};
