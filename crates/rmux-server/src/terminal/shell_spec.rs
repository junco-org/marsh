//! The argument plan an explicitly configured `default-shell` is launched with.
//!
//! This is a description of a workload, not a way to start one. Every plan here ends up as an
//! `exec` line submitted to a managed job, so the shell a user configured still gets its own
//! dialect — `-c` for a command, a login `argv0` for an interactive one — while the process
//! itself is created, gated and observed by the engine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rmux_proto::RmuxError;

use crate::io::protocol::exec_plan;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct ShellSpec {
    program: PathBuf,
}

impl ShellSpec {
    pub(super) fn new(shell: &Path) -> Self {
        Self {
            program: shell.to_path_buf(),
        }
    }

    /// The managed `exec` line that runs `command` through this configured shell.
    ///
    /// # Errors
    ///
    /// Fails when a word of the plan is not UTF-8: `exec` takes an argument vector, and a word
    /// that is not text has no spelling in a managed command line.
    pub(super) fn command_line(&self, command: &str) -> Result<String, RmuxError> {
        self.command_plan(command).into_exec_line()
    }

    /// The managed `exec` line that runs this configured shell interactively.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::command_line`] fails.
    pub(super) fn interactive_line(&self) -> Result<String, RmuxError> {
        self.interactive_plan().into_exec_line()
    }

    fn command_plan(&self, command: &str) -> ShellCommandPlan {
        ShellCommandPlan::new(&self.program).arg("-c").arg(command)
    }

    fn interactive_plan(&self) -> ShellCommandPlan {
        ShellCommandPlan::new(&self.program).arg0(login_shell_argv0(&self.program))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ShellCommandPlan {
    program: PathBuf,
    arg0: Option<OsString>,
    args: Vec<OsString>,
}

impl ShellCommandPlan {
    fn new(program: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            arg0: None,
            args: Vec::new(),
        }
    }

    fn arg0(mut self, arg0: impl Into<OsString>) -> Self {
        self.arg0 = Some(arg0.into());
        self
    }

    fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Composes this plan into the managed `exec` line that runs it.
    ///
    /// Every word is force-quoted by [`exec_plan`], so an argument containing spaces, quotes,
    /// newlines or glob characters reaches the program unchanged rather than being re-split by
    /// the interpreter that submits it.
    ///
    /// # Errors
    ///
    /// Fails when the program or one of its arguments is not UTF-8 — a managed line is text, and
    /// a lossy conversion would launch a *different* program from the configured one.
    fn into_exec_line(self) -> Result<String, RmuxError> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(os_word(self.program.as_os_str())?);
        for argument in &self.args {
            argv.push(os_word(argument)?);
        }
        let argv0: Option<String> = self.arg0.as_deref().map(os_word).transpose()?;
        Ok(exec_plan(&argv, argv0.as_deref()))
    }
}

/// One plan word as the text a managed line carries.
///
/// # Errors
///
/// Fails when the word is not UTF-8.
fn os_word(value: &std::ffi::OsStr) -> Result<String, RmuxError> {
    value.to_str().map(str::to_owned).ok_or_else(|| {
        RmuxError::spawn_failed(format!(
            "{} shell: {} is not valid UTF-8 and cannot be written into a managed command line",
            rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX,
            value.to_string_lossy()
        ))
    })
}

fn login_shell_argv0(shell: &Path) -> OsString {
    let name = shell
        .file_name()
        .unwrap_or(shell.as_os_str())
        .to_os_string();
    let mut login_name = OsString::from("-");
    login_name.push(name);
    login_name
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interactive plan for the configured shell really does launch it with a login `argv0`.
    ///
    /// Executed rather than inspected. The fields this used to assert are a plan's *intent*, and
    /// the bug the plan exists for lived one layer further down, in the composed line: `-a=-bash`
    /// parsed as the program, so the configured shell never ran at all. So the line is submitted
    /// to a real managed engine and the shell is asked to print its own `$0`. `--noprofile` and
    /// `--norc` keep that answer from depending on whichever startup files this machine has.
    #[tokio::test]
    async fn interactive_shell_uses_login_argv0() {
        let handler = crate::handler::RequestHandler::new();
        let io = crate::managed_workload::handler_facade(&handler)
            .expect("the per-handler test engine builds a facade");

        let line = ShellSpec::new(Path::new("/bin/bash"))
            .interactive_plan()
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("printf '%s' \"$0\"")
            .into_exec_line()
            .expect("the interactive plan composes a managed line");

        let execution = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            io.execute(crate::io::ExecutionSpec {
                initial_dir: std::path::PathBuf::new(),
                id: None,
                process: rmux_proto::ProcessCommand::Shell(line),
                environment: None,
            }),
        )
        .await
        .expect("the workload is admitted within the bound")
        .expect("the workload is admitted");

        let captured = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            execution.collect(crate::io::CollectOptions::default()),
        )
        .await
        .expect("the collection settles within the bound")
        .expect("the collection succeeds");

        assert_eq!(
            captured.stdout.as_slice(),
            b"-bash",
            "`exec -a` must reach the shell as two words, so it runs under a login argv0"
        );
        assert_eq!(
            captured.completion.exit_code,
            Some(0),
            "the shell that printed it exited cleanly"
        );
        assert!(
            captured.completion.is_published(),
            "the line was gated like any other, and approved"
        );

        let _ = io.shutdown().await;
    }
}
