//! Persistent in-process shells with automatic native observation and capability-gated publication.
//!
//! Construct [`Shell`], run ordinary commands, and close it. Source discovery, shared authority,
//! command snapshots and recovery belong to the shell, not its callers. A [`SandboxPolicy`]
//! decides per command whether it runs in a private snapshot whose effects are published only
//! after capability checks, or directly against its source; by default a shell sandboxes exactly
//! while another live shell in this process shares its source. Stale commands are never replayed
//! automatically. This is publication control, not OS confinement: effects outside the private
//! work view are not rolled back.
//!
//! ```no_run
//! # async fn example() -> Result<(), marsh::ShellError> {
//! let shell = marsh::Shell::new(std::path::Path::new("/srv/source")).await?;
//! let result = shell.run("printf hi > greeting").await?;
//! assert_eq!(u8::from(result.exit_code), 0);
//! shell.close(false).await?;
//! # Ok(())
//! # }
//! ```

pub use marsh_core::{
    Action, Bump, CommandContext, Denial, EmptyPolicy, Event, ExecutionParameters, ExecutionResult,
    LockPolicy, MarshTool, OpenFile, Policy, PolicyDecision, PolicyValidator, Principal,
    ProfileLoadBehavior, RcLoadBehavior, SandboxPolicy, Shell, ShellBuilder, ShellCommand,
    ShellEnvironment, ShellError, ShellErrorKind, ShellFd, ShellVariable, Signal, SourceInfo,
    UIOptions, builtins, fresh_principal, shellmux,
};

pub mod rmux;
