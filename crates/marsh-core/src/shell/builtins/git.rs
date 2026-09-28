//! The git command. A managed command delegates to gitexec/gitcmd, which observe and classify its
//! effects; a direct command runs the system git exactly as the shell would run any program.

use std::io::Write;

use brush_core::builtins::Command;
use brush_core::commands::{CommandArg, ShellForCommand, SimpleCommand};
use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions};

use crate::shell::execution::brush_error;

/// Exit code of a command the shell cannot find.
const NOT_FOUND: u8 = 127;

#[derive(clap::Parser)]
pub(super) struct GitBuiltin {
    #[clap(allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}
impl Command for GitBuiltin {
    type Error = brush_core::Error;
    fn new<I: IntoIterator<Item = String>>(args: I) -> Result<Self, clap::Error> {
        Ok(Self {
            args: args.into_iter().collect(),
        })
    }
    async fn execute<SE: ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let owner = super::current_context().ok_or_else(|| {
            brush_core::Error::from(brush_core::ErrorKind::InternalError(
                "git requires an active command".into(),
            ))
        })?;
        match owner.snapshot().map_err(brush_error)? {
            Some(snapshot) => super::gitexec::run(context, self.args.clone(), snapshot).await,
            None => direct(context, &self.args).await,
        }
    }
}

/// Runs the resolved system git through the shell's own external-command path — never a function
/// or builtin of that name — with the builtin's descriptors, directory and environment. `argv`
/// starts with the builtin's own name, which the resolved path replaces.
async fn direct<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    argv: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let Some(git) = context.shell.find_first_executable_in_path("git") else {
        writeln!(context.stderr(), "git: command not found")?;
        return Ok(ExecutionResult::new(NOT_FOUND));
    };
    let git = git.to_string_lossy().into_owned();
    let argv = std::iter::once(&git)
        .chain(argv.iter().skip(1))
        .map(CommandArg::from)
        .collect();
    let mut command = SimpleCommand::new(
        ShellForCommand::ParentShell(context.shell),
        context.params,
        git.clone(),
        argv,
    );
    command.use_functions = false;
    command.argv0 = Some("git".to_string());
    command.process_group_id = context.process_group_id;
    Ok(command.execute().await?.wait().await?.into())
}
