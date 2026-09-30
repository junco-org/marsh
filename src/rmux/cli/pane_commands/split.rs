use std::path::Path;

use rmux_client::Connection;
use rmux_proto::{
    ErrorResponse, PaneTarget, ProcessCommand, Request, Response, SplitWindowExtRequest,
    SplitWindowRequest, SplitWindowTarget, SplitWindowTargetActionRequest, Target,
};

use super::super::format_print::print_target_format;
use super::super::target_resolution::{connect_cli, response_failure};
use super::super::{
    CommandTarget, ExitFailure, cli_target_actions_enabled, target_action_needs_legacy_retry,
    unexpected_response,
};
use crate::cli_args::SplitWindowArgs;
use crate::cli_response::tmux_cli_error_message;

/// Target format printed by `split-window -P` when no `-F` format is supplied.
const DEFAULT_SPLIT_WINDOW_PRINT_FORMAT: &str = "#{session_name}:#{window_index}.#{pane_index}";

/// Runs `split-window`, preferring the server-resolved target action over the legacy request.
pub(in crate::cli) fn run_split_window(
    mut args: SplitWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if args.start_directory.is_none() {
        args.start_directory = std::env::current_dir().ok();
    }
    if !cli_target_actions_enabled() {
        return run_split_window_legacy(args, socket_path, None);
    }

    let legacy_args = args.clone();
    let direction = args.direction();
    let mut connection = connect_cli(socket_path)?;
    let target = args.target.as_ref().map(|target| target.raw().to_owned());
    let size = args.size_spec();
    let environment = (!args.environment.is_empty()).then_some(args.environment);
    let command = (!args.command.is_empty()).then_some(args.command);
    let stdin_to_empty_pane = args.stdin && command.is_none();
    let legacy_stdin_payload = stdin_to_empty_pane.then(read_stdin_payload).transpose()?;
    let response = connection.split_window_target_action(SplitWindowTargetActionRequest {
        target,
        direction,
        before: args.before,
        environment,
        command,
        process_command: stdin_to_empty_pane.then_some(ProcessCommand::Shell(String::new())),
        start_directory: args.start_directory,
        keep_alive_on_exit: (args.keep_alive_on_exit || stdin_to_empty_pane).then_some(true),
        detached: args.detached,
        size,
        preserve_zoom: args.preserve_zoom,
        full_size: args.full_size,
        stdin_payload: legacy_stdin_payload.clone(),
    });
    if target_action_needs_legacy_retry(&response) {
        return run_split_window_legacy(legacy_args, socket_path, legacy_stdin_payload);
    }
    let pane = match response.map_err(ExitFailure::from)? {
        Response::SplitWindow(response) => response.pane,
        Response::Error(ErrorResponse { error }) => {
            return Err(ExitFailure::new(
                1,
                tmux_cli_error_message("split-window", &error),
            ));
        }
        other => return Err(unexpected_response("split-window", &other)),
    };
    print_new_pane(
        &mut connection,
        args.print_target,
        args.format.as_deref(),
        pane,
    )
}

/// Splits a window after resolving the target locally, as older servers require, reusing stdin
/// already read for a target-action attempt.
fn run_split_window_legacy(
    args: SplitWindowArgs,
    socket_path: &Path,
    preloaded_stdin_payload: Option<Vec<u8>>,
) -> Result<i32, ExitFailure> {
    let direction = args.direction();
    let mut connection = connect_cli(socket_path)?;
    let target = SplitWindowTarget::Pane(PaneTarget::resolve(
        &mut connection,
        args.target.as_ref(),
        "split-window",
    )?);
    let size = args.size_spec();
    let environment = (!args.environment.is_empty()).then_some(args.environment);
    let command = (!args.command.is_empty()).then_some(args.command);
    let stdin_to_empty_pane = args.stdin && command.is_none();
    let stdin_payload = stdin_to_empty_pane
        .then(|| preloaded_stdin_payload.map_or_else(read_stdin_payload, Ok))
        .transpose()?;
    let process_command = stdin_to_empty_pane.then_some(ProcessCommand::Shell(String::new()));
    let request = if command.is_some()
        || process_command.is_some()
        || args.start_directory.is_some()
        || args.detached
        || size.is_some()
        || args.full_size
        || args.preserve_zoom
        || args.keep_alive_on_exit
        || stdin_payload.is_some()
    {
        Request::SplitWindowExt(Box::new(SplitWindowExtRequest {
            target,
            direction,
            before: args.before,
            environment,
            command,
            process_command,
            start_directory: args.start_directory,
            keep_alive_on_exit: (args.keep_alive_on_exit || stdin_to_empty_pane).then_some(true),
            detached: args.detached,
            size,
            preserve_zoom: args.preserve_zoom,
            full_size: args.full_size,
            stdin_payload,
        }))
    } else {
        Request::SplitWindow(SplitWindowRequest {
            target,
            direction,
            before: args.before,
            environment,
        })
    };
    let pane = match connection.roundtrip(&request).map_err(ExitFailure::from)? {
        Response::SplitWindow(response) => response.pane,
        other => return Err(response_failure("split-window", &other)),
    };
    print_new_pane(
        &mut connection,
        args.print_target,
        args.format.as_deref(),
        pane,
    )
}

/// Prints the new pane through `-F`, or the default target format, when `-P` was given.
fn print_new_pane(
    connection: &mut Connection,
    print_target: bool,
    format: Option<&str>,
    pane: PaneTarget,
) -> Result<i32, ExitFailure> {
    if print_target {
        let format = format.unwrap_or(DEFAULT_SPLIT_WINDOW_PRINT_FORMAT);
        print_target_format(connection, "split-window", Target::Pane(pane), format)?;
    }
    Ok(0)
}

/// Reads the whole of standard input, used to seed an empty pane with `-I`.
fn read_stdin_payload() -> Result<Vec<u8>, ExitFailure> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
        .map_err(|error| ExitFailure::new(1, format!("failed to read stdin: {error}")))?;
    Ok(bytes)
}
