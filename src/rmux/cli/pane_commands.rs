use std::path::Path;

use rmux_client::{ClientError, Connection};
use rmux_core::formats::{
    DEFAULT_LIST_PANES_ALL_FORMAT, DEFAULT_LIST_PANES_SESSION_FORMAT,
    DEFAULT_LIST_PANES_WINDOW_FORMAT,
};
use rmux_proto::{
    CommandOutput, PaneTarget, ResizePaneAdjustment, ResizePaneRelativeDirection,
    ResizePaneTargetActionRequest, ResolveTargetType, RespawnPaneRequest, Response, SessionName,
    Target, WindowTarget,
};

/// Client-side implementation of `split-window`.
#[path = "pane_commands/split.rs"]
mod split;
/// Client-side implementations of the pane-moving commands (`break`, `join`, `move`, `swap`).
#[path = "pane_commands/transfer.rs"]
mod transfer;

use super::json_output::{
    filter_delimited_json_output, list_panes_json_format, write_list_panes_json,
};
use super::target_resolution::{
    CommandTarget, LISTING_FIELD_SEPARATOR, connect_cli, filtered_listing_line,
    resolve_active_window_index, target_session,
};
use super::{
    ExitFailure, cli_target_actions_enabled, expect_command_output, expect_command_success,
    list_session_names, listed_pane_index_matches_target, resolve_current_session_target,
    resolve_target_spec, shell_command_text, target_action_needs_legacy_retry, write_lines_output,
};
use crate::cli_args::{
    LastPaneArgs, ListPanesArgs, PipePaneArgs, ResizePaneArgs, ResizePaneSize, RespawnPaneArgs,
    SelectPaneArgs, TargetSpec,
};

pub(super) use split::run_split_window;
pub(super) use transfer::{run_break_pane, run_join_pane, run_move_pane, run_swap_pane};

/// Runs `last-pane`, switching a window back to its previously active pane.
pub(super) fn run_last_pane(args: &LastPaneArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let input_disabled = if args.enable_input {
        Some(false)
    } else {
        args.disable_input.then_some(true)
    };
    WindowTarget::run(
        socket_path,
        "last-pane",
        args.target.as_ref(),
        |connection, target| {
            connection.last_pane_with_options(target, args.keep_zoom, input_disabled)
        },
    )
}

/// Runs `pipe-pane`, wiring pane output (and optionally input) into a shell command.
pub(super) fn run_pipe_pane(args: PipePaneArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let command = (!args.command.is_empty()).then(|| shell_command_text(args.command));
    let stdout = args.stdout || !args.stdin;
    PaneTarget::run(
        socket_path,
        "pipe-pane",
        args.target.as_ref(),
        |connection, target| connection.pipe_pane(target, args.stdin, stdout, args.once, command),
    )
}

/// Runs `respawn-pane`, restarting a pane's process in place.
pub(super) fn run_respawn_pane(
    args: RespawnPaneArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    PaneTarget::run(
        socket_path,
        "respawn-pane",
        args.target.as_ref(),
        |connection, target| {
            connection.respawn_pane(RespawnPaneRequest {
                target,
                kill: args.kill,
                start_directory: args.start_directory,
                environment: (!args.environment.is_empty()).then_some(args.environment),
                command: (!args.command.is_empty()).then_some(args.command),
                process_command: None,
            })
        },
    )
}

/// Runs `list-panes`, rendering panes of one window, one session, or every session.
pub(super) fn run_list_panes(args: &ListPanesArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let json = args.json;
    let json_format = json.then(list_panes_json_format);
    let format = Some(list_panes_server_format(
        json_format.as_deref().or(args.format.as_deref()),
        args.filter.as_deref(),
        list_panes_default_format(args.all_sessions, args.session_scope),
    ));
    let pane_targets = if args.all_sessions {
        list_session_names(&mut connection)?
            .into_iter()
            .map(|session_name| (session_name, None))
            .collect::<Vec<_>>()
    } else {
        let (session_name, window_index) =
            resolve_list_panes_target(&mut connection, args.target.as_ref(), "list-panes")?;
        vec![(session_name, window_index.filter(|_| !args.session_scope))]
    };
    let mut lines = Vec::new();
    let mut json_stdout = Vec::new();
    for (session_name, target_window_index) in pane_targets {
        let response = connection
            .list_panes_in_window_with_options(
                session_name,
                target_window_index,
                format.clone(),
                args.filter.clone(),
                args.sort_order.clone(),
                args.reversed,
            )
            .map_err(ExitFailure::from)?;
        let output = expect_command_output(&response, "list-panes")?;
        if json {
            if args.filter.is_some() {
                let filtered = filter_delimited_json_output(output, "list-panes")?;
                json_stdout.extend_from_slice(filtered.stdout());
            } else {
                json_stdout.extend_from_slice(output.stdout());
            }
            continue;
        }
        for line in String::from_utf8_lossy(output.stdout()).lines() {
            if let Some(line) = filtered_listing_line(line, args.filter.as_deref(), "list-panes")? {
                lines.push(line.to_owned());
            }
        }
    }
    if json {
        return write_list_panes_json(&CommandOutput::from_stdout(json_stdout));
    }
    write_lines_output(&lines)
}

/// Picks the default `list-panes` line format for the requested listing scope.
const fn list_panes_default_format(all_sessions: bool, session_scope: bool) -> &'static str {
    if all_sessions {
        DEFAULT_LIST_PANES_ALL_FORMAT
    } else if session_scope {
        DEFAULT_LIST_PANES_SESSION_FORMAT
    } else {
        DEFAULT_LIST_PANES_WINDOW_FORMAT
    }
}

/// Builds the format string sent to the server, prefixing the filter expression when one is given.
fn list_panes_server_format(
    format: Option<&str>,
    filter: Option<&str>,
    default_format: &'static str,
) -> String {
    let line_format = format.unwrap_or(default_format);
    filter.map_or_else(
        || line_format.to_owned(),
        |filter| format!("{filter}{LISTING_FIELD_SEPARATOR}{line_format}"),
    )
}

/// Resolves a `list-panes` target spec into a session name and optional window index.
fn resolve_list_panes_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<(SessionName, Option<u32>), ExitFailure> {
    let target = match target {
        Some(target) => {
            resolve_target_spec(connection, target, ResolveTargetType::Pane, false, false)?
        }
        None => Target::Session(resolve_current_session_target(connection)?),
    };
    let window_index = match &target {
        Target::Session(session_name) => {
            resolve_active_window_index(connection, session_name, command_name)?
        }
        Target::Window(window) => window.window_index(),
        Target::Pane(pane) => pane.window_index(),
    };
    Ok((target_session(target), Some(window_index)))
}

/// Runs `select-pane`, covering input toggles, last-pane, directional moves, styles, and marks.
pub(super) fn run_select_pane(
    args: SelectPaneArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let keep_zoom = args.keep_zoom;
    let target = args.target.as_ref();
    if args.disable_input || args.enable_input {
        let input_disabled = Some(args.disable_input);
        return select_pane_uncached(socket_path, "select-pane", target, |connection, pane| {
            connection.select_pane_with_options(pane, None, args.style, input_disabled, keep_zoom)
        });
    }
    if args.last {
        return select_pane_uncached(socket_path, "last-pane", target, |connection, window| {
            connection.last_pane_with_zoom(window, keep_zoom)
        });
    }
    if let Some(direction) = args.direction() {
        return PaneTarget::run(socket_path, "select-pane", target, |connection, pane| {
            connection.select_pane_adjacent_with_zoom(pane, direction, keep_zoom)
        });
    }
    if !args.mark && !args.clear_marked {
        return PaneTarget::run(socket_path, "select-pane", target, |connection, pane| {
            connection.select_pane_with_options(pane, args.title, args.style, None, keep_zoom)
        });
    }
    select_pane_uncached(socket_path, "select-pane", target, |connection, pane| {
        connection.select_pane_mark_with_title(pane, args.clear_marked, args.title)
    })
}

/// Resolves a `select-pane` target over a fresh connection, then checks the response to the
/// request `send` builds as `check_name` without printing any output it carries.
fn select_pane_uncached<T: CommandTarget>(
    socket_path: &Path,
    check_name: &'static str,
    target: Option<&TargetSpec>,
    send: impl FnOnce(&mut Connection, T) -> Result<Response, ClientError>,
) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let target = T::resolve(&mut connection, target, "select-pane")?;
    expect_command_success(send(&mut connection, target)?, check_name)?;
    Ok(0)
}

/// Runs `resize-pane`, preferring the server-resolved target action over the legacy request.
pub(super) fn run_resize_pane(
    args: &ResizePaneArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if !cli_target_actions_enabled() || resize_pane_uses_percent(args) {
        return run_resize_pane_legacy(args, socket_path);
    }

    let target = args.target.as_ref().map(|target| target.raw().to_owned());
    let adjustment = resize_pane_adjustment(args, None);
    let mut connection = connect_cli(socket_path)?;
    let response =
        connection.resize_pane_target_action(ResizePaneTargetActionRequest { target, adjustment });
    if target_action_needs_legacy_retry(&response) {
        return run_resize_pane_legacy(args, socket_path);
    }
    expect_command_success(response?, "resize-pane")?;
    Ok(0)
}

/// Resizes a pane after resolving its target locally, as older servers require.
fn run_resize_pane_legacy(args: &ResizePaneArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let target = PaneTarget::resolve(&mut connection, args.target.as_ref(), "resize-pane")?;
    let window_size = resize_pane_uses_percent(args)
        .then(|| resize_pane_window_size(&mut connection, &target))
        .transpose()?;
    let adjustment = resize_pane_adjustment(args, window_size);
    expect_command_success(connection.resize_pane(target, adjustment)?, "resize-pane")?;
    Ok(0)
}

/// Reports whether either requested dimension is a percentage, which needs the window size.
fn resize_pane_uses_percent(args: &ResizePaneArgs) -> bool {
    [args.columns, args.rows]
        .into_iter()
        .flatten()
        .any(|size| matches!(size, ResizePaneSize::Percent(_)))
}

/// Turns the parsed `resize-pane` flags into the single adjustment the server understands.
fn resize_pane_adjustment(
    args: &ResizePaneArgs,
    window_size: Option<(u16, u16)>,
) -> ResizePaneAdjustment {
    if args.trim_below {
        return ResizePaneAdjustment::TrimBelow;
    }
    if args.zoom {
        return ResizePaneAdjustment::Zoom;
    }
    let columns = args
        .columns
        .map(|size| size.resolve(window_size.map_or(0, |(width, _)| width)));
    let rows = args
        .rows
        .map(|size| size.resolve(window_size.map_or(0, |(_, height)| height)));
    let relative = if let Some(cells) = args.left {
        Some((ResizePaneRelativeDirection::Left, cells))
    } else if let Some(cells) = args.right {
        Some((ResizePaneRelativeDirection::Right, cells))
    } else if let Some(cells) = args.up {
        Some((ResizePaneRelativeDirection::Up, cells))
    } else {
        args.down
            .map(|cells| (ResizePaneRelativeDirection::Down, cells))
    };

    match (columns, rows, relative) {
        (columns @ Some(_), rows, Some((relative, cells)))
        | (columns @ None, rows @ Some(_), Some((relative, cells))) => {
            ResizePaneAdjustment::Composite {
                columns,
                rows,
                relative: Some(relative),
                cells,
            }
        }
        (Some(columns), Some(rows), None) => ResizePaneAdjustment::AbsoluteSize { columns, rows },
        (Some(columns), None, None) => ResizePaneAdjustment::AbsoluteWidth { columns },
        (None, Some(rows), None) => ResizePaneAdjustment::AbsoluteHeight { rows },
        (None, None, Some((relative, cells))) => relative.to_adjustment(cells),
        (None, None, None) => ResizePaneAdjustment::NoOp,
    }
}

/// Looks up the cell width and height of the window holding `target` via `list-panes`.
fn resize_pane_window_size(
    connection: &mut Connection,
    target: &rmux_proto::PaneTarget,
) -> Result<(u16, u16), ExitFailure> {
    let response = connection
        .list_panes_in_window(
            target.session_name().clone(),
            Some(target.window_index()),
            Some("#{pane_index}\t#{pane-base-index}\t#{window_width}\t#{window_height}".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "list-panes")?;
    let stdout = String::from_utf8_lossy(output.stdout());
    let (width, height) = stdout
        .lines()
        .find_map(|line| {
            let mut fields = line.split('\t');
            listed_pane_index_matches_target(target, fields.next()?, fields.next()?)
                .then(|| fields.next().zip(fields.next()))
                .flatten()
        })
        .ok_or_else(|| {
            ExitFailure::new(
                1,
                format!("resize-pane could not resolve dimensions for pane {target}"),
            )
        })?;
    let width = width.parse::<u16>().map_err(|error| {
        ExitFailure::new(1, format!("invalid resize-pane window width: {error}"))
    })?;
    let height = height.parse::<u16>().map_err(|error| {
        ExitFailure::new(1, format!("invalid resize-pane window height: {error}"))
    })?;
    Ok((width, height))
}
