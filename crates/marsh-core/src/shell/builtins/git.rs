//! The `git` builtin: the system git, started through the shell's own recorded external-command
//! path, so every git a line runs is the git the line's boundary saw.
//!
//! The name is registered as a builtin so it is found before any `PATH` search and the
//! invocation is recorded as a builtin, while the work itself is the system git's: every command
//! line is forwarded unchanged, and git's own grammar, errors and exit status are what the caller
//! sees. [`super::gitexec`] adds only what makes that git safe to run here — no host
//! configuration, repository discovery bounded by the tree, nothing written outside it — and, in
//! a snapshot-attached shell, the observation that turns what git did into the actions the line
//! requests.
#![allow(
    clippy::unused_async_trait_impl,
    reason = "builtins implement a trait whose `execute` is async by contract"
)]

use std::collections::HashMap;

use brush_core::builtins::{self, BoxFuture, Registration};
use brush_core::{CommandArg, ExecutionContext, ExecutionResult, ShellExtensions};

use super::gitexec;
use crate::shell::MarshExecutor;

/// Environment variable naming the tree a command belongs to.
///
/// Its value is the boundary a git builtin may not use a repository past. A shell built without
/// it discovers repositories as git itself would.
pub const SNAPSHOT_ROOT_VAR: &str = "MARSH_SNAPSHOT_ROOT";

/// The git registration: one `git` builtin, which forwards its whole command line to the system
/// git.
///
/// Extend a builtin map with it — `map.extend(git_builtins())` — before handing the
/// map to `Shell::builder().builtins`.
#[must_use]
pub fn git_builtins<SE: ShellExtensions>() -> HashMap<String, Registration<SE>> {
    HashMap::from([("git".to_string(), builtins::builtin::<GitBuiltin, SE>())])
}

/// The same builtin for a shell whose spawner carries a snapshot: its invocations are observed
/// in that snapshot, and what they did is recorded for the line's boundary.
pub(crate) fn managed_registration<SE>() -> Registration<SE>
where
    SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>,
{
    Registration {
        execute_func: managed_execute::<SE>,
        ..builtins::builtin::<GitBuiltin, SE>()
    }
}

/// The managed registration's `execute_func`: the stock one, told which snapshot it runs in.
fn managed_execute<SE>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, brush_core::Error>>
where
    SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>,
{
    Box::pin(async move {
        let snapshot = context.shell.external_command_spawner().attached().cloned();
        let argv = args.iter().map(ToString::to_string).collect();
        gitexec::run(context, argv, snapshot).await
    })
}

/// The `git` builtin. Argv is kept verbatim and forwarded; clap is bypassed.
#[derive(clap::Parser)]
struct GitBuiltin {
    /// The command's own argument vector, `argv[0]` included.
    #[clap(allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}

impl builtins::Command for GitBuiltin {
    type Error = brush_core::Error;

    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        Ok(Self {
            args: args.into_iter().collect(),
        })
    }

    async fn execute<SE: ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        gitexec::run(context, self.args.clone(), None).await
    }
}
