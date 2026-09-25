use std::path::Path;

use rmux_client::ClientError;
use rmux_client::connect;
use rmux_client::{ConnectResult, connect_or_absent};
use rmux_proto::RmuxError;
use rmux_proto::{ListSessionsRequest, RMUX_WIRE_VERSION};

use super::{
    ExitFailure, StartupOptions, connect_with_startserver, expect_command_output,
    resolve_session_target_or_current, run_command, run_command_resolved, run_payload_command,
};
use super::{expect_command_success, write_command_output};
use crate::cli_args::{ClientTargetArgs, ServerAccessArgs, SessionTargetArgs};

/// Runs `start-server`, auto-starting the daemon and confirming it answers `list-sessions`.
pub(super) fn run_start_server(
    socket_path: &Path,
    startup: StartupOptions,
) -> Result<i32, ExitFailure> {
    let mut connection = connect_with_startserver(socket_path, startup)?;
    let response = connection
        .list_sessions(ListSessionsRequest {
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .map_err(ExitFailure::from)?;
    let _ = expect_command_output(&response, "list-sessions")?;
    Ok(0)
}

/// Runs `kill-server`, tolerating the server closing the connection before replying.
pub(super) fn run_kill_server(socket_path: &Path) -> Result<i32, ExitFailure> {
    let mut connection = connect(socket_path)
        .map_err(|error| ExitFailure::from_client_connect(socket_path, error))?;
    match connection.kill_server() {
        Ok(response) => {
            let output = response.command_output().cloned();
            expect_command_success(response, "kill-server")?;
            if let Some(output) = output {
                write_command_output(&output)?;
            }
            drop(connection);
            wait_for_killed_server_socket_cleanup(socket_path)?;
            Ok(0)
        }
        Err(error) if kill_server_connection_closed(&error) => {
            drop(connection);
            wait_for_killed_server_socket_cleanup(socket_path)?;
            Ok(0)
        }
        Err(error) => {
            if let Some(wire_version) = legacy_shutdown_fallback_wire_version(&error) {
                run_legacy_wire_kill_server(socket_path, wire_version)
            } else {
                Err(ExitFailure::from(error))
            }
        }
    }
}

/// Repeats `kill-server` over an older wire version when the server refused the current one.
fn run_legacy_wire_kill_server(socket_path: &Path, wire_version: u32) -> Result<i32, ExitFailure> {
    let mut connection = match connect_or_absent(socket_path)
        .map_err(|error| ExitFailure::from_client_connect(socket_path, error))?
    {
        ConnectResult::Connected(connection) => connection,
        ConnectResult::Absent => {
            wait_for_killed_server_socket_cleanup(socket_path)?;
            return Ok(0);
        }
    };

    let shutdown = connection.kill_server_legacy_wire(wire_version);
    drop(connection);
    match shutdown {
        Ok(()) => {
            wait_for_killed_server_socket_cleanup(socket_path)?;
            Ok(0)
        }
        Err(error) if kill_server_connection_closed(&error) => {
            wait_for_killed_server_socket_cleanup(socket_path)?;
            Ok(0)
        }
        Err(error) => Err(ExitFailure::from(error)),
    }
}

/// Blocks until the killed server's socket has disappeared, so a reopen cannot race it.
fn wait_for_killed_server_socket_cleanup(socket_path: &Path) -> Result<(), ExitFailure> {
    rmux_client::wait_for_server_endpoint_cleanup(socket_path).map_err(ExitFailure::from)
}

/// Runs `server-access`, changing or listing per-user access to the server.
pub(super) fn run_server_access(
    args: ServerAccessArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_payload_command(socket_path, "server-access", move |connection| {
        connection.server_access(rmux_proto::ServerAccessRequest {
            add: args.add,
            deny: args.deny,
            list: args.list,
            read_only: args.read_only,
            write: args.write,
            target: None,
            user: args.user,
        })
    })
}

/// Runs `lock-server`, locking every attached client.
pub(super) fn run_lock_server(socket_path: &Path) -> Result<i32, ExitFailure> {
    run_command(socket_path, "lock-server", |connection| {
        connection.lock_server()
    })
}

/// Runs `lock-session`, locking the clients attached to one session.
pub(super) fn run_lock_session(
    args: SessionTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_command_resolved(socket_path, "lock-session", move |connection| {
        let target =
            resolve_session_target_or_current(connection, args.target.as_ref(), "lock-session")?;
        connection.lock_session(target).map_err(ExitFailure::from)
    })
}

/// Runs `lock-client`, locking a single client and defaulting to the caller's own.
pub(super) fn run_lock_client(
    args: ClientTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_command(socket_path, "lock-client", move |connection| {
        connection.lock_client(args.target.unwrap_or_else(|| "=".to_owned()))
    })
}

/// Reports whether the error is just the server closing the connection as it shuts down.
fn kill_server_connection_closed(error: &ClientError) -> bool {
    matches!(error, ClientError::UnexpectedEof)
        || matches!(
            error,
            ClientError::Io(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::UnexpectedEof
                )
        )
}

/// Extracts the older wire version to retry with from a version-mismatch protocol error.
fn legacy_shutdown_fallback_wire_version(error: &ClientError) -> Option<u32> {
    match error {
        ClientError::Protocol(RmuxError::UnsupportedWireVersion { got, .. })
            if (1..RMUX_WIRE_VERSION).contains(got) =>
        {
            Some(*got)
        }
        _ => None,
    }
}
