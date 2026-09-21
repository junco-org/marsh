//! `run-shell` and the `if-shell` predicate, as managed work.
//!
//! Both used to be a `/bin/sh` child with piped or discarded output. Both are now a managed pipe
//! job: real separate stdout and stderr, a real principal, and a publication gate between whatever
//! the command wrote and the seed.
//!
//! # What that changes for a caller
//!
//! A `run-shell` that redirects into a file no longer writes into the seed as a side effect of
//! having been configured. It stages the write, the gate decides, and the caller learns both
//! facts: the process's exit status *and* the verdict. They are independent, and the interesting
//! failure is the one where they disagree — the command printed, exited zero, and changed nothing.
//!
//! For `if-shell` that distinction is not optional. A predicate whose publication was refused has
//! not answered the question, so neither branch may run: it is an error, never `false`. Treating a
//! refusal as `false` would quietly take the else-branch on a policy decision, which is the one
//! outcome a user reading their configuration would never suspect.
//!
//! # Limits that are preserved exactly
//!
//! * the 300-second timeout, after which the job is forced and discarded;
//! * the frame-derived output cap and its truncation marker;
//! * the shutdown interruption, which ends a helper at the first opportunity rather than at its
//!   timeout.
//!
//! The cap is now a *combined* budget across stdout and stderr rather than one per stream. It
//! exists to keep a response inside the wire's maximum frame, and two independently capped streams
//! could together exceed it — which is the bug the cap was there to prevent.

use std::sync::Arc;
use std::time::Duration;

use marsh_core::shellmux::CommandCompletion;
use rmux_proto::{ProcessCommand, RmuxError, DEFAULT_MAX_FRAME_LENGTH};

use super::super::shell_processes::{
    ShellProcessGuard, ShellProcessRegistrationError, ShellProcessRegistry,
};
use crate::io::ShellIo;
use crate::managed_workload;
use crate::terminal::TerminalProfile;

/// How long a helper may run before it is forced and discarded.
const RUN_SHELL_TIMEOUT: Duration = Duration::from_secs(300);
/// Appended when the cap discarded output, so a reader can tell a short answer from a clipped one.
const RUN_SHELL_TRUNCATION_MARKER: &[u8] = b"\nrmux: run-shell output truncated\n";
/// The cap, derived from the wire's maximum frame with room for the rest of the response.
const RUN_SHELL_OUTPUT_LIMIT: usize = DEFAULT_MAX_FRAME_LENGTH - 64 * 1024;

/// What one `run-shell` produced.
///
/// The completion is carried whole rather than flattened into a status, because the caller has to
/// be able to tell "the command failed" from "the command succeeded and the gate refused it".
pub(super) struct ShellRunOutput {
    /// Standard output, with standard error appended when `-t` asked for it, capped and marked.
    pub(super) stdout: Vec<u8>,
    /// The process's exit status, or `1` when no execution result was obtained.
    pub(super) exit_status: i32,
    /// The gate's verdict, the exit code it saw, and the command text it belongs to.
    pub(super) completion: Arc<CommandCompletion>,
}

/// Runs one `run-shell` command to completion.
///
/// # Errors
///
/// Fails when the engine refused the job, when the command outran its timeout, and when shutdown
/// interrupted it.
pub(super) async fn run_shell_foreground(
    io: &ShellIo,
    command: String,
    profile: &TerminalProfile,
    show_stderr: bool,
    shell_processes: Option<Arc<ShellProcessRegistry>>,
) -> Result<ShellRunOutput, RmuxError> {
    run_shell_foreground_with_timeout(
        io,
        command,
        profile,
        show_stderr,
        RUN_SHELL_TIMEOUT,
        shell_processes,
    )
    .await
}

async fn run_shell_foreground_with_timeout(
    io: &ShellIo,
    command: String,
    profile: &TerminalProfile,
    show_stderr: bool,
    timeout: Duration,
    shell_processes: Option<Arc<ShellProcessRegistry>>,
) -> Result<ShellRunOutput, RmuxError> {
    let captured = run_managed_shell(
        io,
        command,
        profile,
        timeout,
        shell_processes,
        "run-shell",
        managed_workload::truncating(RUN_SHELL_OUTPUT_LIMIT),
    )
    .await?;

    let mut stdout = captured.stdout;
    if show_stderr && !captured.stderr.is_empty() {
        stdout.extend_from_slice(&captured.stderr);
    }
    if captured.truncated {
        stdout.extend_from_slice(RUN_SHELL_TRUNCATION_MARKER);
    }
    // No execution result at all is `1`, the same status a shell reports for a command it could
    // not run. Inventing a signal from it is deliberately not done: nothing observed one.
    let exit_status = captured.completion.exit_code.unwrap_or(1);
    Ok(ShellRunOutput {
        stdout,
        exit_status,
        completion: captured.completion,
    })
}

/// Answers an `if-shell` predicate.
///
/// # Errors
///
/// Fails for the reasons [`run_shell_foreground`] does, and — importantly — when the condition's
/// publication was not approved. An unapproved predicate is not an answer, so neither branch runs.
pub(super) async fn shell_condition_is_true(
    io: &ShellIo,
    command: String,
    profile: &TerminalProfile,
    shell_processes: Option<Arc<ShellProcessRegistry>>,
) -> Result<bool, RmuxError> {
    shell_condition_is_true_with_timeout(io, command, profile, RUN_SHELL_TIMEOUT, shell_processes)
        .await
}

async fn shell_condition_is_true_with_timeout(
    io: &ShellIo,
    command: String,
    profile: &TerminalProfile,
    timeout: Duration,
    shell_processes: Option<Arc<ShellProcessRegistry>>,
) -> Result<bool, RmuxError> {
    #[cfg(windows)]
    match command.trim() {
        "true" => return Ok(true),
        "false" => return Ok(false),
        _ => {}
    }

    // The predicate's output is discarded, as it was upstream — but it is still drained to end of
    // file, because a condition that filled a pipe and blocked would never answer at all.
    let captured = run_managed_shell(
        io,
        command,
        profile,
        timeout,
        shell_processes,
        "if-shell",
        managed_workload::DISCARD,
    )
    .await?;
    managed_workload::require_published(&captured)?;
    Ok(captured.completion.exit_code == Some(0))
}

/// Admits one helper, waits for it, and ends it on a timeout, a shutdown, or a dropped caller.
///
/// Cancellation goes through the job rather than the collection: dropping the collection would
/// stop *watching* a command that is still running, which is the difference between a timeout that
/// ends the work and one that merely stops reporting on it.
///
/// That has to hold for the cancellation nobody here can await, too. The listener's bounded
/// shutdown drain does not signal this helper — it drops the whole request future — so the
/// `select!` below never runs, and without [`StopOnDrop`] the line's external processes would
/// outlive the daemon that started them. A `sleep 30` left behind by `rmux kill-server` is the
/// visible form of that.
async fn run_managed_shell(
    io: &ShellIo,
    command: String,
    profile: &TerminalProfile,
    timeout: Duration,
    shell_processes: Option<Arc<ShellProcessRegistry>>,
    command_name: &str,
    options: crate::io::CollectOptions,
) -> Result<crate::io::CapturedOutput, RmuxError> {
    let spec = managed_workload::spec(
        io,
        profile.cwd(),
        profile.raw_environment(),
        None,
        ProcessCommand::Shell(command),
    )?;
    let execution = managed_workload::start(io, spec).await?;
    let mut guard = register_shell_process(io, execution.shell(), shell_processes.as_ref())?;
    if guard
        .as_ref()
        .is_some_and(ShellProcessGuard::shutdown_started)
    {
        let _ = execution.cancel().await;
        return Err(RmuxError::Server(format!(
            "{command_name} interrupted by server shutdown"
        )));
    }

    let shell = execution.shell().clone();
    // Armed for the whole wait: from here on, every way out of this function ends the job —
    // the two explicit stops below, and the drop this cannot intercept any other way.
    let mut stop_on_drop = StopOnDrop {
        io: io.clone(),
        shell: shell.clone(),
        armed: true,
    };
    let collect = execution.collect(options);
    tokio::pin!(collect);

    let interrupted = async {
        match guard.as_mut() {
            Some(guard) => guard.cancelled().await,
            None => std::future::pending().await,
        }
    };

    let outcome = tokio::select! {
        result = &mut collect => {
            // The job answered, so it is over: stopping it now would be stopping nothing.
            stop_on_drop.armed = false;
            Some(result.map_err(managed_workload::io_error))
        }
        () = tokio::time::sleep(timeout) => None,
        () = interrupted => {
            stop_on_drop.armed = false;
            let _ = io.stop(&shell, true).await;
            return Err(RmuxError::Server(format!(
                "{command_name} interrupted by server shutdown"
            )));
        }
    };

    match outcome {
        Some(result) => result,
        None => {
            // Forced, so every process the line spawned goes with it; discarded, so nothing it
            // staged reaches the seed on the way out.
            stop_on_drop.armed = false;
            let _ = io.stop(&shell, true).await;
            Err(RmuxError::Server(format!(
                "{command_name} timed out after {}s",
                timeout.as_secs()
            )))
        }
    }
}

/// Ends a helper's job when the future waiting on it is dropped.
///
/// [`Drop`] cannot await, so the stop is handed to the engine's own runtime — the same runtime
/// the job's pumps already run on, which is still alive precisely because the job is. Disarmed on
/// every path that has already dealt with the job, so a completed helper is never stopped twice.
struct StopOnDrop {
    /// The engine to ask, and the runtime to ask it on.
    io: ShellIo,
    /// The generation-bound job, so a stop can never reach whatever later took its name.
    shell: crate::io::ShellHandle,
    /// Whether dropping still has to end the job.
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let io = self.io.clone();
        let shell = self.shell.clone();
        io.runtime().clone().spawn(async move {
            // Forced: the caller is gone, so there is nobody left to read what the line would
            // still produce, and its external children are the only thing that would survive.
            let _ = io.stop(&shell, true).await;
        });
    }
}

/// Puts one helper on the daemon's ledger.
///
/// # Errors
///
/// Fails when the daemon is shutting down and when the admission limit is reached, in both cases
/// with the message upstream used.
fn register_shell_process(
    io: &ShellIo,
    job: &crate::io::ShellHandle,
    shell_processes: Option<&Arc<ShellProcessRegistry>>,
) -> Result<Option<ShellProcessGuard>, RmuxError> {
    let Some(shell_processes) = shell_processes else {
        return Ok(None);
    };
    match shell_processes.register(io, job) {
        Ok(guard) => Ok(Some(guard)),
        Err(ShellProcessRegistrationError::Closing) => Err(RmuxError::Server(
            "shell command interrupted by server shutdown".to_owned(),
        )),
        Err(ShellProcessRegistrationError::LimitReached { limit }) => Err(RmuxError::Server(
            format!("too many active shell process groups; limit is {limit}"),
        )),
    }
}

pub(super) fn run_shell_delay_duration(seconds: f64) -> Result<Duration, RmuxError> {
    Duration::try_from_secs_f64(seconds).map_err(|_| {
        RmuxError::Server("run-shell -d expects a non-negative finite delay".to_owned())
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::run_shell_foreground_with_timeout;
    use crate::handler::RequestHandler;
    use crate::managed_workload;
    use crate::terminal::TerminalProfile;
    use rmux_core::{EnvironmentStore, OptionStore};
    use rmux_proto::{OptionName, ScopeSelector, SetOptionMode};
    use std::path::Path;
    use std::time::Duration;

    /// A helper that outruns its deadline is ended, not merely stopped being watched.
    ///
    /// The proof is the descendant: `sleep 30 &` is a second process in the same job, and a
    /// timeout that only abandoned the collection would leave it running. Forcing the job kills
    /// everything the line spawned, which is what the previous process-group teardown guaranteed
    /// and what the managed stop has to keep guaranteeing.
    #[tokio::test]
    async fn timeout_ends_the_whole_managed_job() {
        let handler = RequestHandler::new();
        let Ok(io) = managed_workload::handler_facade(&handler) else {
            return;
        };
        let seed = io.executor_info().seed.expect("test engine has a seed");
        let profile = test_profile(&seed);

        let result = run_shell_foreground_with_timeout(
            &io,
            "sleep 30 & sleep 30".to_owned(),
            &profile,
            false,
            Duration::from_millis(200),
            None,
        )
        .await;

        assert!(
            result
                .err()
                .expect("command should time out")
                .to_string()
                .contains("timed out"),
            "expected a timeout error"
        );
        assert!(
            io.jobs().iter().all(|job| !job.id.as_str().is_empty()),
            "the engine must still answer after a forced stop"
        );
    }

    /// The cap clips the answer and says so.
    #[tokio::test]
    async fn output_beyond_the_cap_is_marked_truncated() {
        let handler = RequestHandler::new();
        let Ok(io) = managed_workload::handler_facade(&handler) else {
            return;
        };
        let seed = io.executor_info().seed.expect("test engine has a seed");
        let profile = test_profile(&seed);

        let output = super::run_managed_shell(
            &io,
            "printf 'abcdefghij'".to_owned(),
            &profile,
            Duration::from_secs(30),
            None,
            "run-shell",
            managed_workload::truncating(4),
        )
        .await
        .expect("a short command should complete");

        assert!(output.truncated, "the cap must be reported");
        assert_eq!(output.stdout.len(), 4, "only the budget may be retained");
    }

    fn test_profile(cwd: &Path) -> TerminalProfile {
        let mut options = OptionStore::default();
        options
            .set(
                ScopeSelector::Global,
                OptionName::DefaultShell,
                "/bin/sh".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("default-shell test option is valid");
        TerminalProfile::for_run_shell(
            &EnvironmentStore::default(),
            &options,
            None,
            None,
            Path::new("/tmp/rmux-run-shell-runtime-test.sock"),
            None,
            false,
            None,
            Some(cwd),
        )
        .expect("profile")
    }
}
