//! `rmux -c <shell-command>`: the tmux-compatible one-shot command mode.
//!
//! tmux runs this by `exec`ing `$SHELL -c <command>` in the client process. Here it is a
//! managed pane on the daemon instead, because a local `exec` would run the command outside
//! the policy gate every other rmux workload passes through. See
//! [`super::managed_io`] for the PTY semantics that follow from that choice: stdout and
//! stderr are merged and stdin EOF becomes terminal EOF, not a pipe half-close.

use std::io::IsTerminal as _;
use std::path::Path;

use rmux_proto::ProcessCommand;

use super::managed_io::{
    run_managed_pane_command, ManagedPaneCommand, ManagedPaneDisplay, ManagedPaneKind,
};
use super::{ExitFailure, StartupOptions};

/// Runs `shell_command` as a managed one-shot pane and returns its gated exit status.
///
/// `login_shell` (tmux's `-l`) is accepted and has no separate effect: a managed pane runs the
/// command through the session's own shell, and there is no host shell process here whose
/// `argv[0]` could carry the login marker. Rejecting the flag would break tmux compatibility
/// for callers that pass it habitually, so it is honoured as "run the command", which is what
/// `-l -c` has always amounted to.
///
/// # Errors
///
/// Fails when the daemon cannot be reached or started, when the current directory is unusable,
/// and with the command's own nonzero gated exit status.
pub(super) fn run_shell_startup(
    socket_path: &Path,
    startup: StartupOptions,
    shell_command: &str,
    login_shell: bool,
) -> Result<i32, ExitFailure> {
    let _ = login_shell;
    let directory = std::env::current_dir().map_err(|error| {
        ExitFailure::new(
            1,
            format!("rmux: failed to resolve the current directory: {error}"),
        )
    })?;

    run_managed_pane_command(
        ManagedPaneCommand {
            process: ProcessCommand::Shell(shell_command.to_owned()),
            directory,
            environment: Vec::new(),
            // A real terminal gets the real rmux client; redirected input gets the byte relay.
            // Attaching a full client to a pipe would render an emulator nobody can see.
            display: if std::io::stdin().is_terminal() {
                ManagedPaneDisplay::Attach
            } else {
                ManagedPaneDisplay::Relay
            },
            kind: ManagedPaneKind::Command,
        },
        socket_path,
        startup,
    )
}
