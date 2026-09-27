use std::path::{Path, PathBuf};

use rmux_client::{ClientContext, ClientContextParent, detect_context, detect_parent};
use rmux_proto::request::{AttachSessionExt2Request, SwitchClientExt3Request};
use rmux_proto::request::{KillSessionRequest, ListSessionsRequest, NewSessionExtRequest};
use rmux_proto::{ClientTerminalContext, Response};

use super::json_output::{list_sessions_json_format, write_list_sessions_json};
use super::target_resolution::{connect_cli, response_failure, run_targeted};
use super::{
    ExitFailure, StartupOptions, build_terminal_size, connect_with_startserver,
    expect_command_output, expect_command_success, optional_client_flags,
    resolve_current_session_target, resolve_session_target_or_current, resolve_session_target_spec,
    run_payload_command, write_command_output,
};
use super::{
    attach_with_connection, current_terminal_size, require_attach_terminal,
    run_switch_client_on_connection,
};
use crate::cli_args::{
    KillSessionArgs, ListSessionsArgs, NewSessionArgs, RenameSessionArgs, SessionTargetArgs,
};

/// Runs `new-session`, creating the session and then attaching unless it was detached.
pub(super) fn run_new_session(
    args: NewSessionArgs,
    socket_path: &Path,
    startup: StartupOptions,
    client_terminal: ClientTerminalContext,
) -> Result<i32, ExitFailure> {
    validate_new_session_size(args.cols, args.rows)?;

    if !args.detached && detect_parent() == ClientContextParent::Tmux {
        return Err(ExitFailure::new(
            1,
            "sessions should be nested with care, unset $TMUX to force",
        ));
    }

    let client_context = detect_context();
    let mut connection = connect_with_startserver(socket_path, startup)?;
    if !args.detached && client_context == ClientContext::Outside {
        reject_existing_session_before_attach_preflight(&args, &mut connection)?;
        require_attach_terminal()?;
    }

    let client_flags = optional_client_flags(args.flags.clone());
    let working_directory = args
        .working_directory
        .or_else(current_working_directory_string);
    // Claude Code's teammate mode creates its swarm session by exact name rather than by
    // resolving a target, so session creation is the second place a shim invocation's logical
    // names must land on this invocation's owned pair.
    let session_name = args
        .session_name
        .clone()
        .map(super::claude_namespace::rewrite_session_name);
    let response = connection
        .new_session_extended(NewSessionExtRequest {
            session_name,
            detached: args.detached,
            size: build_terminal_size(args.cols, args.rows),
            environment: (!args.environment.is_empty()).then_some(args.environment),
            group_target: args.group_target,
            working_directory,
            attach_if_exists: args.attach_if_exists,
            detach_other_clients: args.detach_other_clients || args.kill_other_clients,
            kill_other_clients: args.kill_other_clients,
            flags: client_flags.clone(),
            window_name: args.window_name,
            print_session_info: args.print_session_info,
            print_format: args.print_format,
            command: (!args.command.is_empty()).then_some(args.command),
            process_command: None,
            client_environment: invoking_client_environment(),
            skip_environment_update: args.skip_environment_update,
        })
        .map_err(ExitFailure::from)
        .map_err(|error| error.with_startup_context("create initial session", Some(socket_path)))?;
    let output = response.command_output().cloned();
    let (target, detached) = match response {
        Response::NewSession(response) => (response.session_name, response.detached),
        other => {
            expect_command_success(other, "new-session")?;
            unreachable!("new-session success must return a new-session response")
        }
    };

    if let Some(output) = output {
        write_command_output(&output)?;
    }

    if detached {
        return Ok(0);
    }

    match client_context {
        ClientContext::Nested => run_switch_client_on_connection(
            &mut connection,
            SwitchClientExt3Request {
                target_client: None,
                target: Some(target.to_string()),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                sort_order: None,
                skip_environment_update: false,
                zoom: false,
            },
        ),
        ClientContext::Outside => attach_with_connection(
            connection,
            AttachSessionExt2Request {
                target: Some(target.clone()),
                target_spec: Some(target.to_string()),
                detach_other_clients: false,
                kill_other_clients: false,
                read_only: false,
                skip_environment_update: false,
                flags: client_flags,
                working_directory: None,
                client_terminal,
                client_size: current_terminal_size(),
            },
            socket_path,
        ),
    }
}

/// Fails before attach preflight when the requested session name is already taken.
fn reject_existing_session_before_attach_preflight(
    args: &NewSessionArgs,
    connection: &mut rmux_client::Connection,
) -> Result<(), ExitFailure> {
    let Some(session_name) = args
        .session_name
        .as_ref()
        .filter(|_| !args.attach_if_exists)
    else {
        return Ok(());
    };
    let response = connection
        .has_session(session_name.clone())
        .map_err(ExitFailure::from)?;
    match response {
        Response::HasSession(response) if response.exists => Err(ExitFailure::new(
            1,
            format!("duplicate session: {session_name}"),
        )),
        Response::HasSession(_) => Ok(()),
        other => Err(response_failure("has-session", &other)),
    }
}

/// Rejects a zero `cols` or `rows` request with tmux's width/height error.
fn validate_new_session_size(cols: Option<u16>, rows: Option<u16>) -> Result<(), ExitFailure> {
    if cols == Some(0) {
        return Err(ExitFailure::new(1, "width too small"));
    }
    if rows == Some(0) {
        return Err(ExitFailure::new(1, "height too small"));
    }
    Ok(())
}

/// Returns the process working directory as a lossy `String` for the new session.
fn current_working_directory_string() -> Option<String> {
    current_working_directory().map(|path| path.to_string_lossy().into_owned())
}

/// The client forwards no environment to the daemon.
const fn invoking_client_environment() -> Option<Vec<String>> {
    None
}

/// Returns the process working directory, discarding it unless it still names a directory.
fn current_working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok().filter(|path| path.is_dir())
}

/// Runs `has-session`, exiting nonzero with tmux's message when the target is absent.
pub(super) fn run_has_session(
    args: &SessionTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let missing_message = args.target.as_ref().map_or_else(
        || "can't find session".to_owned(),
        |target| format!("can't find session: {target}"),
    );
    let target = match args.target.as_ref() {
        Some(target) => resolve_session_target_spec(&mut connection, target, false)
            .map_err(|error| map_has_session_lookup_error(error, target.raw()))?,
        None => resolve_current_session_target(&mut connection)?,
    };
    match connection.has_session(target).map_err(ExitFailure::from)? {
        Response::HasSession(response) if response.exists => Ok(0),
        Response::HasSession(_) => Err(ExitFailure::new(1, missing_message)),
        other => Err(response_failure("has-session", &other)),
    }
}

/// Turns a `has-session` target lookup failure into tmux's missing-session message.
fn map_has_session_lookup_error(error: ExitFailure, raw_target: &str) -> ExitFailure {
    if error.message().contains("ambiguous session match") {
        return ExitFailure::new(1, format!("can't find session: {raw_target}"));
    }
    normalize_session_lookup_error(error)
}

/// Runs `kill-session` against the resolved target, or the current session.
pub(super) fn run_kill_session(
    args: &KillSessionArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let target =
        resolve_session_target_or_current(&mut connection, args.target.as_ref(), "kill-session")
            .map_err(normalize_session_lookup_error)?;
    let response = connection
        .kill_session(KillSessionRequest {
            target,
            kill_all_except_target: args.kill_all_except_target,
            clear_alerts: args.clear_alerts,
            kill_group: args.kill_group,
        })
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "kill-session")?;
    Ok(0)
}

/// Trims a missing-session failure down to tmux's `can't find session: <name>` message.
fn normalize_session_lookup_error(error: ExitFailure) -> ExitFailure {
    const PREFIX: &str = "can't find session: ";

    if let Some((_, session_name)) = error.message().split_once(PREFIX) {
        return ExitFailure::new(1, format!("{PREFIX}{session_name}"));
    }
    error
}

/// Runs `rename-session`, resolving the target before sending the new name.
pub(super) fn run_rename_session(
    args: RenameSessionArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_targeted(
        socket_path,
        "rename-session",
        args.target.as_ref(),
        |connection, target| connection.rename_session(target, args.new_name),
    )
}

/// Runs `list-sessions`, emitting the JSON encoding when `--json` was requested.
pub(super) fn run_list_sessions(
    args: ListSessionsArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if args.json {
        let mut connection = connect_cli(socket_path)?;
        let response = connection
            .list_sessions(ListSessionsRequest {
                format: Some(list_sessions_json_format()),
                filter: args.filter,
                sort_order: args.sort_order,
                reversed: args.reversed,
            })
            .map_err(ExitFailure::from)?;
        let output = expect_command_output(&response, "list-sessions")?;
        return write_list_sessions_json(output);
    }

    run_payload_command(socket_path, "list-sessions", move |connection| {
        connection.list_sessions(ListSessionsRequest {
            format: args.format,
            filter: args.filter,
            sort_order: args.sort_order,
            reversed: args.reversed,
        })
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {}
