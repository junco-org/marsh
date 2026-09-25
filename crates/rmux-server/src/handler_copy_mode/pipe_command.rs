//! `copy-pipe`: the copy-mode selection into a command's standard input.
//!
//! The command is a managed pipe job now, not a child of the daemon. Three things that were
//! accidental before become explicit:
//!
//! * its **output goes somewhere**. Upstream let it inherit the daemon's own standard output and
//!   standard error, which on a detached daemon means nowhere in particular and on a foreground
//!   one means interleaved into the operator's terminal. It is drained and discarded here.
//! * its **input really ends**. The selection is written and then standard input is closed, so a
//!   filter that reads to end of file — `xsel`, `pbcopy`, `wl-copy` — actually terminates.
//! * its **effects are staged**. A `copy-pipe 'cat > selection.txt'` reaches the seed only if the
//!   gate approves it, exactly like the same redirection typed into a pane.
//!
//! # Why startup is still acknowledged separately
//!
//! The caller needs to know the command *started*, not that it finished: `copy-pipe` returns as
//! soon as the selection has been handed over, and the filter may outlive the request. So
//! admission is awaited here and the rest is owned by a background task, which is also what holds
//! the cancellation the daemon's shutdown reaches.

use std::path::PathBuf;

use marsh_core::shellmux::MuxError;
use rmux_proto::{ProcessCommand, RmuxError};

use super::super::shell_processes::ShellProcessRegistrationError;
use super::super::RequestHandler;
use crate::io::IoError;
use crate::managed_workload;

pub(super) async fn run_pipe_command(
    handler: &RequestHandler,
    shell: &str,
    command: &str,
    working_directory: Option<&PathBuf>,
    data: &[u8],
) -> Result<(), RmuxError> {
    if command.is_empty() {
        return Ok(());
    }
    // The configured `default-shell` no longer selects an interpreter: the text runs in the
    // managed shell, which is what puts marsh's builtins and instrumentation in front of it. The
    // option still matters to panes, so it is accepted and ignored rather than rejected.
    let _ = shell;

    let io = managed_workload::handler_facade(handler)?;
    let working_directory = working_directory
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let spec = managed_workload::spec(
        &io,
        &working_directory,
        std::iter::empty(),
        None,
        ProcessCommand::Shell(command.to_owned()),
    )?;

    let execution = managed_workload::start(&io, spec).await?;
    let mut guard = match handler.shell_processes.register(&io, execution.shell()) {
        Ok(guard) => guard,
        Err(ShellProcessRegistrationError::Closing) => {
            let _ = execution.cancel().await;
            return Err(rejected(command, "server shutdown started"));
        }
        Err(ShellProcessRegistrationError::LimitReached { limit }) => {
            let _ = execution.cancel().await;
            return Err(rejected(
                command,
                &format!("active shell process limit of {limit} was reached"),
            ));
        }
    };
    if guard.shutdown_started() {
        let _ = execution.cancel().await;
        return Err(RmuxError::Server(format!(
            "pipe command '{command}' was interrupted by server shutdown"
        )));
    }

    // The selection goes in before the request returns, so a failure to deliver it is reported to
    // the caller rather than logged into a background task nobody is reading.
    let input = execution.input();
    deliver_selection(input.write_all(data).await, command)?;
    // Real end of file: a filter blocked on `read` would otherwise never finish, and the daemon
    // would hold its job open until shutdown.
    deliver_selection(input.close().await, command)?;

    let shell = execution.shell().clone();
    let command = command.to_owned();
    io.runtime().clone().spawn(async move {
        let collect = execution.collect(managed_workload::DISCARD);
        tokio::pin!(collect);
        tokio::select! {
            result = &mut collect => match result {
                Ok(captured) if !captured.completion.is_published() => {
                    tracing::info!(
                        "copy-mode pipe command '{command}' completed without publication"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("copy-mode pipe command '{command}' failed: {error}");
                }
            },
            () = guard.cancelled() => {
                let _ = io.stop(&shell, true).await;
            }
        }
    });
    Ok(())
}

fn rejected(command: &str, reason: &str) -> RmuxError {
    RmuxError::Server(format!(
        "pipe command '{command}' was cancelled before startup completed: {reason}"
    ))
}

/// Accepts "the filter is already gone" as a delivered selection rather than a failed command.
///
/// A `copy-pipe 'exit 7'` — or any filter that returns before reading — closes its job while the
/// selection is still on its way in. That is this daemon's spelling of `EPIPE`, which is what
/// upstream's buffered write gets and silently discards, and `copy-pipe` is defined as *starting*
/// the filter: the copy itself already succeeded, and there is simply nobody left to hand the
/// bytes to.
///
/// Narrow on purpose. Only the three job-lifetime refusals are absorbed; a closed engine, a
/// foreign handle or transport failure is still a real error, because in those cases the filter
/// may well be alive and waiting for input it will never get.
///
/// # Errors
///
/// Fails for every other reason a write or a close can fail.
fn deliver_selection(result: Result<(), IoError>, command: &str) -> Result<(), RmuxError> {
    match result {
        Ok(()) => Ok(()),
        Err(IoError::Mux(error))
            if matches!(
                *error,
                MuxError::StaleJob(_) | MuxError::JobClosing(_) | MuxError::InputClosed(_)
            ) =>
        {
            tracing::debug!(
                "copy-mode pipe command '{command}' ended before the selection was delivered: \
                 {error}"
            );
            Ok(())
        }
        Err(error) => Err(managed_workload::io_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::run_pipe_command;
    use crate::handler::RequestHandler;
    use std::time::{Duration, Instant};

    /// A filter that reads its input to end of file still terminates.
    ///
    /// The regression this defends is a real deadlock: without an explicit close, `cat` never
    /// sees end of file, the job never finishes, and `copy-pipe` leaves work behind on every
    /// invocation until the daemon shuts down.
    #[tokio::test]
    async fn selection_reaches_a_filter_that_reads_to_end_of_file() {
        let handler = RequestHandler::new();
        let Ok(io) = crate::managed_workload::handler_facade(&handler) else {
            return;
        };
        let seed = io.default_dir().to_path_buf();
        let destination = seed.join("selection.txt");

        run_pipe_command(
            &handler,
            "/bin/sh",
            "cat > selection.txt",
            Some(&seed),
            b"picked\n",
        )
        .await
        .expect("the pipe command should start and accept the selection");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if std::fs::read(&destination).is_ok_and(|bytes| bytes == b"picked\n") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the filter never published its input"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// An empty command is a no-op, not an admitted job.
    #[tokio::test]
    async fn an_empty_command_does_nothing() {
        let handler = RequestHandler::new();
        run_pipe_command(&handler, "/bin/sh", "", None, b"ignored")
            .await
            .expect("an empty pipe command should succeed without running anything");
    }
}
