//! `exec` for an attached shell: the program runs through the shell's external-command path
//! (so the spawner sees it) and the shell exits with its status afterwards, instead of
//! `execve` replacing the process — which would discard everything the session had not
//! published yet, and everything the exec'd program goes on to write.

use std::collections::HashMap;
use std::io::Write;

use brush_core::builtins::{self, Registration};
use brush_core::commands;
use brush_core::results::ExecutionControlFlow;
use brush_core::{
    CommandArg, ExecutionContext, ExecutionExitCode, ExecutionResult, ShellExtensions,
};

/// The `exec` registration, keyed `"exec"`, to be inserted over the stock one.
#[must_use]
pub fn exec_builtins<SE: ShellExtensions>() -> HashMap<String, Registration<SE>> {
    HashMap::from([("exec".to_string(), builtins::builtin::<ExecBuiltin, SE>())])
}

/// Runs a program in place of the shell — here: through the shell, then out of it.
#[derive(clap::Parser)]
struct ExecBuiltin {
    /// Pass given name as zeroth argument to command.
    #[arg(short = 'a', value_name = "NAME")]
    name_for_argv0: Option<String>,
    /// Exec command with an empty environment (not supported here).
    #[arg(short = 'c')]
    empty_environment: bool,
    /// Exec command as a login shell.
    #[arg(short = 'l')]
    exec_as_login: bool,
    /// Command and args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for ExecBuiltin {
    type Error = brush_core::Error;

    async fn execute<SE: ShellExtensions>(
        &self,
        mut context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // No program: the builtin's redirections become the calling shell's (stock behaviour,
        // copied from brush-builtins/src/exec.rs).
        if self.args.is_empty() {
            #[allow(
                clippy::needless_collect,
                reason = "iter_fds borrows the context that replace_open_files mutates"
            )]
            let fds: Vec<_> = context.iter_fds().collect();
            context.shell.replace_open_files(fds.into_iter());
            return Ok(ExecutionResult::success());
        }
        if self.empty_environment {
            writeln!(
                context.stderr(),
                "exec: -c is not supported in a marsh shell"
            )?;
            return Ok(ExecutionResult::new(2));
        }
        // bash runs the program, never a builtin or function of that name: resolve it to a path
        // first; a name with a separator is used as given.
        let program = &self.args[0];
        let path = if brush_core::sys::fs::contains_path_separator(program) {
            program.clone()
        } else if let Some(found) = context.shell.find_first_executable_in_path(program) {
            found.to_string_lossy().into_owned()
        } else {
            writeln!(context.stderr(), "exec: {program}: not found")?;
            return Ok(ExecutionExitCode::NotFound.into());
        };
        let mut argv0 = self
            .name_for_argv0
            .clone()
            .unwrap_or_else(|| program.clone());
        if self.exec_as_login {
            argv0.insert(0, '-');
        }
        context.command_name = path.clone();
        let argv = std::iter::once(&path)
            .chain(&self.args[1..])
            .map(CommandArg::from)
            .collect();
        let mut command = commands::SimpleCommand::new(
            commands::ShellForCommand::ParentShell(context.shell),
            context.params,
            context.command_name,
            argv,
        );
        command.use_functions = false;
        command.argv0 = Some(argv0);
        let mut result: ExecutionResult = command.execute().await?.wait().await?.into();
        result.next_control_flow = ExecutionControlFlow::ExitShell;
        Ok(result)
    }
}
