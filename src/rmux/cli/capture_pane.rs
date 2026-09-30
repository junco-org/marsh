use std::path::Path;

use rmux_client::Connection;
use rmux_proto::{CapturePaneRequest, CapturePaneTargetActionRequest, PaneTarget, Response};

use crate::cli_args::{CapturePaneArgs, TargetSpec};

use super::target_resolution::connect_cli;
use super::{
    CommandTarget, ExitFailure, capture_target_action_needs_legacy_retry,
    cli_target_actions_enabled,
};

/// A validated `capture-pane` invocation held until a target and transport are chosen.
pub(super) struct PendingCapturePaneRequest {
    /// The parsed `-t` spec, resolved locally only for the legacy request shape.
    target: Option<TargetSpec>,
    /// The request as the server-side target action sends it, carrying the raw `-t` text.
    request: CapturePaneTargetActionRequest,
}

/// Validates `capture-pane` arguments, parsing the `-S`/`-E` bounds into a pending request.
pub(super) fn capture_pane_request(
    args: CapturePaneArgs,
) -> Result<PendingCapturePaneRequest, ExitFailure> {
    let (start, start_is_absolute) = parse_capture_bound(args.start.as_deref(), "-S")?;
    let (end, end_is_absolute) = parse_capture_bound(args.end.as_deref(), "-E")?;

    Ok(PendingCapturePaneRequest {
        request: CapturePaneTargetActionRequest {
            target: args.target.as_ref().map(|target| target.raw().to_owned()),
            start,
            end,
            print: args.print,
            buffer_name: args.buffer_name,
            alternate: args.alternate,
            escape_ansi: args.escape_ansi,
            escape_sequences: args.escape_sequences,
            include_format: args.include_format,
            hyperlinks: args.hyperlinks,
            line_numbers: args.line_numbers,
            join_wrapped: args.join_wrapped,
            use_mode_screen: args.use_mode_screen,
            preserve_trailing_spaces: args.preserve_trailing_spaces,
            do_not_trim_spaces: args.do_not_trim_spaces,
            pending_input: args.pending_input,
            quiet: args.quiet,
            start_is_absolute,
            end_is_absolute,
        },
        target: args.target,
    })
}

/// Resolves the pending request's target to a concrete pane for the legacy request shape.
fn build_capture_pane_request(
    connection: &mut Connection,
    pending: PendingCapturePaneRequest,
) -> Result<CapturePaneRequest, ExitFailure> {
    let request = pending.request;
    Ok(CapturePaneRequest {
        target: PaneTarget::resolve(connection, pending.target.as_ref(), "capture-pane")?,
        start: request.start,
        end: request.end,
        print: request.print,
        buffer_name: request.buffer_name,
        alternate: request.alternate,
        escape_ansi: request.escape_ansi,
        escape_sequences: request.escape_sequences,
        include_format: request.include_format,
        hyperlinks: request.hyperlinks,
        line_numbers: request.line_numbers,
        join_wrapped: request.join_wrapped,
        use_mode_screen: request.use_mode_screen,
        preserve_trailing_spaces: request.preserve_trailing_spaces,
        do_not_trim_spaces: request.do_not_trim_spaces,
        pending_input: request.pending_input,
        quiet: request.quiet,
        start_is_absolute: request.start_is_absolute,
        end_is_absolute: request.end_is_absolute,
    })
}

/// Sends the capture, preferring server-side target actions and retrying on a legacy server.
pub(super) fn send_capture_pane_request(
    connection: &mut Connection,
    socket_path: &Path,
    pending: PendingCapturePaneRequest,
) -> Result<Response, ExitFailure> {
    if !cli_target_actions_enabled() {
        let request = build_capture_pane_request(connection, pending)?;
        return connection.capture_pane(request).map_err(ExitFailure::from);
    }

    let response = connection.capture_pane_target_action(pending.request.clone());
    if !capture_target_action_needs_legacy_retry(&response) {
        return response.map_err(ExitFailure::from);
    }

    let mut legacy_connection = connect_cli(socket_path)?;
    let request = build_capture_pane_request(&mut legacy_connection, pending)?;
    legacy_connection
        .capture_pane(request)
        .map_err(ExitFailure::from)
}

/// Parses an `-S`/`-E` bound, where `-` means the absolute history edge rather than a number.
fn parse_capture_bound(
    value: Option<&str>,
    flag: &str,
) -> Result<(Option<i64>, bool), ExitFailure> {
    match value {
        None => Ok((None, false)),
        Some("-") => Ok((None, true)),
        Some(value) => value
            .parse::<i64>()
            .map(|value| (Some(value), false))
            .map_err(|_| {
                ExitFailure::new(1, format!("command capture-pane: {flag} expects a number"))
            }),
    }
}
