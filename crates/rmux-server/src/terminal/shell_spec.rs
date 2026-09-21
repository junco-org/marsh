//! The argument plan an explicitly configured `default-shell` is launched with.
//!
//! This is a description of a workload, not a way to start one. Every plan here ends up as an
//! `exec` line submitted to a managed job, so the shell a user configured still gets its own
//! dialect — `-c` for a POSIX shell, `-NoProfile -Command` for PowerShell, a login `argv0` for an
//! interactive one — while the process itself is created, gated and observed by the engine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rmux_proto::RmuxError;

use crate::io::protocol::exec_plan;

#[cfg(windows)]
use super::executable_name;
#[cfg(windows)]
use rmux_os::command::cmd_c_verbatim_tail;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct ShellSpec {
    program: PathBuf,
    kind: ShellKind,
}

impl ShellSpec {
    pub(super) fn new(shell: &Path) -> Self {
        Self {
            program: shell.to_path_buf(),
            kind: detect_shell_kind(shell),
        }
    }

    /// The managed `exec` line that runs `command` through this configured shell.
    ///
    /// # Errors
    ///
    /// Fails when a word of the plan is not UTF-8, and on Windows for a plan whose arguments are
    /// one verbatim command tail: `exec` takes an argument vector, and a `cmd.exe` tail is by
    /// construction a string the shell itself splits.
    pub(super) fn command_line(&self, cwd: &Path, command: &str) -> Result<String, RmuxError> {
        self.command_plan(cwd, command).into_exec_line()
    }

    /// The managed `exec` line that runs this configured shell interactively.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::command_line`] fails.
    pub(super) fn interactive_line(&self, cwd: &Path) -> Result<String, RmuxError> {
        self.interactive_plan(cwd).into_exec_line()
    }

    fn command_plan(&self, cwd: &Path, command: &str) -> ShellCommandPlan {
        #[cfg(unix)]
        let _ = cwd;

        match self.kind {
            #[cfg(unix)]
            ShellKind::Unix => ShellCommandPlan::new(&self.program).arg("-c").arg(command),
            #[cfg(windows)]
            ShellKind::PowerShell | ShellKind::WindowsPowerShell => {
                ShellCommandPlan::new(&self.program)
                    .arg("-NoProfile")
                    .arg("-Command")
                    .arg(format!(
                        "Set-Location -LiteralPath {}; {command}",
                        powershell_single_quoted(cwd)
                    ))
            }
            #[cfg(windows)]
            ShellKind::Cmd => ShellCommandPlan::new(&self.program)
                .arg("/D")
                .arg("/S")
                .arg("/C")
                .windows_verbatim_args(cmd_c_verbatim_tail(command)),
            #[cfg(windows)]
            ShellKind::Posix => ShellCommandPlan::new(&self.program).arg("-lc").arg(command),
            #[cfg(windows)]
            ShellKind::Nu => ShellCommandPlan::new(&self.program).arg("-c").arg(command),
            #[cfg(windows)]
            ShellKind::Batch => ShellCommandPlan::new(&cmd_wrapper_program())
                .arg("/D")
                .arg("/S")
                .arg("/C")
                .windows_verbatim_args(batch_cmd_tail(&self.program, command)),
            #[cfg(windows)]
            ShellKind::Other => ShellCommandPlan::new(&self.program).arg(command),
        }
    }

    fn interactive_plan(&self, cwd: &Path) -> ShellCommandPlan {
        #[cfg(unix)]
        let _ = cwd;
        #[cfg(windows)]
        let _ = cwd;

        match self.kind {
            #[cfg(unix)]
            ShellKind::Unix => {
                ShellCommandPlan::new(&self.program).arg0(login_shell_argv0(&self.program))
            }
            #[cfg(windows)]
            ShellKind::PowerShell => ShellCommandPlan::new(&self.program)
                .arg("-NoLogo")
                .arg("-NoExit"),
            #[cfg(windows)]
            ShellKind::WindowsPowerShell => ShellCommandPlan::new(&self.program)
                .arg("-NoLogo")
                .arg("-NoExit"),
            #[cfg(windows)]
            ShellKind::Cmd => ShellCommandPlan::new(&self.program).arg("/D").arg("/K"),
            #[cfg(windows)]
            ShellKind::Batch => ShellCommandPlan::new(&cmd_wrapper_program())
                .arg("/D")
                .arg("/K")
                .arg(self.program.as_os_str()),
            #[cfg(windows)]
            ShellKind::Posix | ShellKind::Nu | ShellKind::Other => {
                ShellCommandPlan::new(&self.program)
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ShellCommandPlan {
    program: PathBuf,
    arg0: Option<OsString>,
    args: Vec<OsString>,
    #[cfg(windows)]
    windows_verbatim_args: Option<OsString>,
}

impl ShellCommandPlan {
    fn new(program: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            arg0: None,
            args: Vec::new(),
            #[cfg(windows)]
            windows_verbatim_args: None,
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

    #[cfg(windows)]
    fn windows_verbatim_args(mut self, args: impl Into<OsString>) -> Self {
        self.windows_verbatim_args = Some(args.into());
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
    /// a lossy conversion would launch a *different* program from the configured one. On Windows
    /// it also fails for a verbatim `cmd.exe` tail, which has no argument-vector spelling.
    fn into_exec_line(self) -> Result<String, RmuxError> {
        #[cfg(windows)]
        if self.windows_verbatim_args.is_some() {
            return Err(RmuxError::spawn_failed(format!(
                "{} shell: {} is launched through a verbatim command tail, which cannot be \
                 expressed as a managed argument vector",
                rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX,
                self.program.display()
            )));
        }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellKind {
    #[cfg(unix)]
    Unix,
    #[cfg(windows)]
    Cmd,
    #[cfg(windows)]
    PowerShell,
    #[cfg(windows)]
    WindowsPowerShell,
    #[cfg(windows)]
    Posix,
    #[cfg(windows)]
    Nu,
    #[cfg(windows)]
    Batch,
    #[cfg(windows)]
    Other,
}

#[cfg(unix)]
fn detect_shell_kind(_shell: &Path) -> ShellKind {
    ShellKind::Unix
}

#[cfg(windows)]
fn detect_shell_kind(shell: &Path) -> ShellKind {
    if is_windows_batch_script(shell) {
        return ShellKind::Batch;
    }

    match executable_name(shell)
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("cmd.exe" | "cmd") => ShellKind::Cmd,
        Some("powershell.exe" | "powershell") => ShellKind::WindowsPowerShell,
        Some("pwsh.exe" | "pwsh") => ShellKind::PowerShell,
        Some("bash.exe" | "bash" | "sh.exe" | "sh" | "zsh.exe" | "zsh") => ShellKind::Posix,
        Some("nu.exe" | "nu") => ShellKind::Nu,
        _ => ShellKind::Other,
    }
}

#[cfg(windows)]
fn is_windows_batch_script(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| matches!(extension.to_ascii_lowercase().as_str(), "bat" | "cmd"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn powershell_single_quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[cfg(windows)]
fn cmd_wrapper_program() -> PathBuf {
    std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

#[cfg(windows)]
fn batch_cmd_tail(program: &Path, command: &str) -> OsString {
    let tail = format!(
        "{} {}",
        cmd_double_quoted(&program.to_string_lossy()),
        cmd_double_quoted(command)
    );
    format!("\"{tail}\"").into()
}

#[cfg(windows)]
fn cmd_double_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(unix)]
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

    #[cfg(windows)]
    #[test]
    fn detects_windows_shell_families_by_executable_name() {
        assert_eq!(
            detect_shell_kind(Path::new(r"C:\Windows\System32\cmd.exe")),
            ShellKind::Cmd
        );
        assert_eq!(
            detect_shell_kind(Path::new("powershell")),
            ShellKind::WindowsPowerShell
        );
        assert_eq!(
            detect_shell_kind(Path::new("pwsh.exe")),
            ShellKind::PowerShell
        );
        assert_eq!(detect_shell_kind(Path::new("bash.exe")), ShellKind::Posix);
        assert_eq!(detect_shell_kind(Path::new("nu.exe")), ShellKind::Nu);
        assert_eq!(
            detect_shell_kind(Path::new(r"C:\Users\RMUX User\shells\custom.cmd")),
            ShellKind::Batch
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_interactive_launches_directly_and_loads_profiles() {
        let spec = ShellSpec::new(Path::new(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ));
        let plan = spec.interactive_plan(Path::new(r"C:\tmp"));

        assert_eq!(
            plan.program,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        );
        assert_eq!(plan.args, os_args(["-NoLogo", "-NoExit"]));
        assert!(
            !plan.args.iter().any(|arg| arg == "-NoProfile"),
            "interactive Windows PowerShell must load the user's profile"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pwsh_interactive_loads_profiles() {
        let spec = ShellSpec::new(Path::new("pwsh.exe"));
        let plan = spec.interactive_plan(Path::new(r"C:\tmp"));

        assert_eq!(plan.program, PathBuf::from("pwsh.exe"));
        assert_eq!(plan.args, os_args(["-NoLogo", "-NoExit"]));
        assert!(
            !plan.args.iter().any(|arg| arg == "-NoProfile"),
            "interactive PowerShell must not suppress profiles"
        );
    }

    #[cfg(windows)]
    #[test]
    fn cmd_interactive_uses_current_dir_instead_of_cd_wrapper() {
        let spec = ShellSpec::new(Path::new("cmd.exe"));
        let plan = spec.interactive_plan(Path::new(r"C:\Users\RMUXUser\Documents\rmux"));

        assert_eq!(plan.program, PathBuf::from("cmd.exe"));
        assert_eq!(plan.arg0, None);
        assert_eq!(plan.args, os_args(["/D", "/K"]));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_command_preserves_command_text_without_wrapping_cwd() {
        let spec = ShellSpec::new(Path::new("cmd.exe"));
        let command = r#"echo "RMUX OK" & echo left^&right"#;
        let plan = spec.command_plan(Path::new(r"C:\tmp"), command);

        assert_eq!(plan.args, os_args(["/D", "/S", "/C"]));
        assert_eq!(
            plan.windows_verbatim_args.as_deref(),
            Some(cmd_c_verbatim_tail(command).as_os_str())
        );
    }

    #[cfg(windows)]
    #[test]
    fn powershell_plans_quote_cwd_with_literal_path() {
        let spec = ShellSpec::new(Path::new("pwsh.exe"));
        let cwd = Path::new(r"C:\Users\RMUXUser's Workspace\rmux");

        let interactive = spec.interactive_plan(cwd);
        assert_eq!(interactive.args, os_args(["-NoLogo", "-NoExit"]));

        let one_shot = spec.command_plan(cwd, "Write-Output RMUX_OK");
        assert_eq!(
            one_shot.args,
            os_args([
                "-NoProfile",
                "-Command",
                "Set-Location -LiteralPath 'C:\\Users\\RMUXUser''s Workspace\\rmux'; Write-Output RMUX_OK",
            ])
        );
    }

    #[cfg(windows)]
    #[test]
    fn posix_shell_command_uses_lc_not_cmd_c() {
        let spec = ShellSpec::new(Path::new("bash.exe"));
        let plan = spec.command_plan(Path::new(r"C:\tmp"), "echo RMUX_OK");

        assert_eq!(plan.args, os_args(["-lc", "echo RMUX_OK"]));
    }

    #[cfg(windows)]
    #[test]
    fn nushell_command_uses_c_not_cmd_c() {
        let spec = ShellSpec::new(Path::new("nu.exe"));
        let plan = spec.command_plan(Path::new(r"C:\tmp"), "echo RMUX_OK");

        assert_eq!(plan.args, os_args(["-c", "echo RMUX_OK"]));
    }

    #[cfg(windows)]
    #[test]
    fn unknown_windows_shell_does_not_receive_cmd_c_flag() {
        let spec = ShellSpec::new(Path::new("custom-shell.exe"));
        let plan = spec.command_plan(Path::new(r"C:\tmp"), "echo RMUX_OK");

        assert_eq!(plan.args, os_args(["echo RMUX_OK"]));
    }

    #[cfg(windows)]
    #[test]
    fn batch_default_shell_interactive_uses_cmd_keepalive_wrapper() {
        let spec = ShellSpec::new(Path::new(r"C:\Users\RMUX User\shells\custom shell.cmd"));
        let plan = spec.interactive_plan(Path::new(r"C:\tmp"));

        assert_eq!(
            plan.program
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .as_deref(),
            Some("cmd.exe")
        );
        assert_eq!(
            plan.args,
            os_args(["/D", "/K", r"C:\Users\RMUX User\shells\custom shell.cmd"])
        );
    }

    #[cfg(windows)]
    #[test]
    fn batch_default_shell_command_uses_cmd_c_wrapper() {
        let spec = ShellSpec::new(Path::new(r"C:\Users\RMUX User\shells\custom shell.bat"));
        let plan = spec.command_plan(Path::new(r"C:\tmp"), "echo RMUX_OK");

        assert_eq!(
            plan.program
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .as_deref(),
            Some("cmd.exe")
        );
        assert_eq!(plan.args, os_args(["/D", "/S", "/C"]));
        assert_eq!(
            plan.windows_verbatim_args.as_deref(),
            Some(
                OsString::from(
                    "\"\"C:\\Users\\RMUX User\\shells\\custom shell.bat\" \"echo RMUX_OK\"\""
                )
                .as_os_str()
            )
        );
    }

    /// The interactive plan for a Unix shell really does launch it with a login `argv0`.
    ///
    /// Executed rather than inspected. The fields this used to assert are a plan's *intent*, and
    /// the bug the plan exists for lived one layer further down, in the composed line: `-a=-bash`
    /// parsed as the program, so the configured shell never ran at all. So the line is submitted
    /// to a real managed engine and the shell is asked to print its own `$0`. `--noprofile` and
    /// `--norc` keep that answer from depending on whichever startup files this machine has.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_interactive_shell_uses_login_argv0() {
        let handler = crate::handler::RequestHandler::new();
        let io = crate::managed_workload::handler_facade(&handler)
            .expect("the per-handler test engine builds a facade");
        let seed = io.executor_info().seed.expect("the test engine has a seed");

        let line = ShellSpec::new(Path::new("/bin/bash"))
            .interactive_plan(&seed)
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("printf '%s' \"$0\"")
            .into_exec_line()
            .expect("the interactive plan composes a managed line");

        let execution = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            io.execute(crate::io::ExecutionSpec {
                directory: String::new(),
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

    #[cfg(windows)]
    fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
        args.into_iter().map(OsString::from).collect()
    }
}
