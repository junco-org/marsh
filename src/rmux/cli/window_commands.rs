use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::Path;

use rmux_client::Connection;
use rmux_core::formats::{DEFAULT_LIST_WINDOWS_ALL_FORMAT, DEFAULT_LIST_WINDOWS_FORMAT};
use rmux_proto::{
    CAPABILITY_CLI_LIST_WINDOWS_ALL_QUEUE, CommandOutput, ErrorResponse, ListWindowsResponse,
    MoveWindowTarget, OptionScopeSelector, PaneTarget, ResizeWindowAdjustment, ResolveTargetType,
    Response, SessionName, Target, WindowListEntry, WindowTarget,
};

use super::command_runner::{
    capture_list_windows_all_server_command_with_connection,
    run_list_windows_all_server_command_with_connection,
    run_queued_server_command_at_target_with_connection,
};
use super::format_print::print_target_format;
use super::json_output::{
    list_windows_json_format, write_length_prefixed_list_windows_json, write_list_windows_json,
};
use super::target_resolution::{
    LISTING_FIELD_SEPARATOR, connect_cli, filtered_listing_line, parse_spec,
    resolve_active_window_index, response_failure, wrong_target_kind,
};
use super::{
    CommandTarget, ExitFailure, expect_command_output, list_session_names,
    resolve_current_pane_target, resolve_current_session_target,
    resolve_existing_window_target_or_current, resolve_session_target_spec, resolve_target_spec,
    resolve_window_index_target_or_current_session, resolve_window_target_spec,
    run_command_resolved, unexpected_response, write_lines_output,
};
use crate::cli_args::{
    AlertSessionTargetArgs, KillWindowArgs, LinkWindowArgs, ListWindowsArgs, MoveWindowArgs,
    NewWindowArgs, RenameWindowArgs, ResizeWindowArgs, RespawnWindowArgs, RotateWindowArgs,
    SelectWindowArgs, SessionTargetArgs, SwapWindowArgs, TargetSpec, UnlinkWindowArgs,
};
use crate::cli_response::tmux_cli_error_message;

/// Default `-P` print format for `new-window`, naming its session, window and pane.
const DEFAULT_NEW_WINDOW_PRINT_FORMAT: &str = "#{session_name}:#{window_index}.#{pane_index}";
/// Format fragment requesting window activity and creation timestamps for client-side sorting.
const LIST_WINDOWS_SORT_METADATA_FORMAT: &str = "#{window_activity}\x1f#{window_created}";

/// Runs `link-window`, resolving the source and destination windows before sending the request.
pub(super) fn run_link_window(
    args: LinkWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_command_resolved(socket_path, "link-window", move |connection| {
        let source = WindowTarget::resolve(connection, args.source.as_ref(), "link-window")?;
        let target = resolve_link_window_target(
            connection,
            args.target.as_ref(),
            args.after,
            args.before,
            "link-window",
        )?;
        connection
            .link_window(
                source,
                target,
                args.after,
                args.before,
                args.kill_target,
                args.detached,
            )
            .map_err(ExitFailure::from)
    })
}

/// Picks the `link-window` destination, honouring `-a`/`-b` anchors and default placement.
fn resolve_link_window_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    after: bool,
    before: bool,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    let Some(target) = target else {
        if after || before {
            return resolve_window_placement_anchor_target(connection, None, command_name);
        }
        let session_name = SessionName::resolve_fallback(connection, command_name)?;
        let index = first_available_window_index(connection, &session_name)?;
        return Ok(WindowTarget::with_window(session_name, index));
    };

    if after || before {
        return resolve_window_placement_anchor_target(connection, Some(target), command_name);
    }
    resolve_window_destination_target(connection, target, command_name)
}

/// Reports whether a `link-window` target names only a session, so a free index must be chosen.
fn link_target_is_explicit_session_only(target: &TargetSpec) -> bool {
    let raw = target.raw();
    if raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    if matches!(parse_bare_relative_window_offset(raw), Ok(Some(_))) {
        return false;
    }
    if is_special_window_token(raw) {
        return false;
    }
    if raw
        .strip_suffix(':')
        .is_some_and(|session| !session.is_empty())
    {
        return true;
    }
    raw.starts_with('=') && matches!(target.exact(), Some(Target::Session(_)))
}

/// Reports whether the raw target is a special window token such as `^` or `{last}`.
fn is_special_window_token(raw: &str) -> bool {
    matches!(
        raw,
        "^" | "!" | "{start}" | "{last}" | "{end}" | "{next}" | "{previous}"
    )
}

/// Resolves an arbitrary raw target into the concrete window a link or move should land on.
fn resolve_window_destination_target(
    connection: &mut Connection,
    target: &TargetSpec,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    if let Some(target) =
        resolve_bare_relative_window_target(connection, target.raw(), command_name)?
    {
        return Ok(target);
    }
    if let Some(index) = parse_bare_window_index(target.raw())? {
        let session_name = SessionName::resolve_fallback(connection, command_name)?;
        return Ok(WindowTarget::with_window(session_name, index));
    }
    if let Some(Target::Window(target)) = target.exact() {
        return Ok(target.clone());
    }
    if link_target_is_explicit_session_only(target) {
        let session_name = resolve_session_only_destination(connection, target)?;
        let index = first_available_window_index(connection, &session_name)?;
        return Ok(WindowTarget::with_window(session_name, index));
    }
    if let Some(target) =
        resolve_exact_current_window_name_destination(connection, target.raw(), command_name)?
    {
        return Ok(target);
    }
    if let Some(target) = resolve_bare_session_window_destination(connection, target)? {
        return Ok(target);
    }

    resolve_window_index_target_or_current_session(connection, Some(target), command_name)
}

/// Looks up a bare name as an existing window of the current session, if one matches.
fn resolve_exact_current_window_name_destination(
    connection: &mut Connection,
    raw_target: &str,
    command_name: &str,
) -> Result<Option<WindowTarget>, ExitFailure> {
    if !link_target_is_bare_session_candidate(raw_target) {
        return Ok(None);
    }
    let session_name = SessionName::resolve_fallback(connection, command_name)?;
    find_window_by_name(connection, &session_name, raw_target)
}

/// Treats a bare name as a session and yields its first free window index, if that session exists.
fn resolve_bare_session_window_destination(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<Option<WindowTarget>, ExitFailure> {
    if !link_target_is_bare_session_candidate(target.raw()) {
        return Ok(None);
    }
    let Ok(session_name) = resolve_session_target_spec(connection, target, false) else {
        return Ok(None);
    };
    let index = first_available_window_index(connection, &session_name)?;
    Ok(Some(WindowTarget::with_window(session_name, index)))
}

/// Reports whether a raw target is a plain name that could denote a session or window name.
fn link_target_is_bare_session_candidate(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.contains([':', '.'])
        && !raw.starts_with(['@', '%', '+', '-', '='])
        && raw.parse::<u32>().is_err()
        && !matches!(
            raw,
            "!" | "^" | "$" | "{start}" | "{last}" | "{end}" | "{next}" | "{previous}"
        )
}

/// Resolves a `+`/`-` relative target against the current window's index.
fn resolve_bare_relative_window_target(
    connection: &mut Connection,
    raw_target: &str,
    command_name: &str,
) -> Result<Option<WindowTarget>, ExitFailure> {
    let Some(offset) = parse_bare_relative_window_offset(raw_target)? else {
        return Ok(None);
    };
    let current = WindowTarget::resolve_fallback(connection, command_name)?;
    let index = apply_window_index_offset(current.window_index(), offset)?;
    Ok(Some(WindowTarget::with_window(
        current.session_name().clone(),
        index,
    )))
}

/// Resolves the existing window that `-a`/`-b` insertion is positioned relative to.
fn resolve_window_placement_anchor_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    let Some(target) = target else {
        return WindowTarget::resolve_fallback(connection, command_name);
    };

    match signed_window_target_session_part(target.raw()) {
        Some(SignedWindowSession::Named(session_target)) => {
            let session_name = resolve_raw_session(connection, session_target)?;
            let window_index =
                resolve_active_window_index(connection, &session_name, command_name)?;
            Ok(WindowTarget::with_window(session_name, window_index))
        }
        Some(SignedWindowSession::Current) => {
            WindowTarget::resolve_fallback(connection, command_name)
        }
        None => resolve_window_target_spec(connection, target, false),
    }
}

/// Resolves the raw session part of a `session:window` spec to its session name.
fn resolve_raw_session(
    connection: &mut Connection,
    raw_session: &str,
) -> Result<SessionName, ExitFailure> {
    resolve_session_target_spec(connection, &parse_spec(raw_session)?, false)
}

/// Session part of a signed `+`/`-` window target.
enum SignedWindowSession<'a> {
    /// The signed index applies to the client's current session.
    Current,
    /// The signed index applies to this explicitly named session.
    Named(&'a str),
}

/// Splits a signed target into its session part, or `None` when it is not signed.
fn signed_window_target_session_part(raw_target: &str) -> Option<SignedWindowSession<'_>> {
    if signed_window_index_target(raw_target) {
        return Some(SignedWindowSession::Current);
    }
    let (session, window) = raw_target.split_once(':')?;
    if session.is_empty() || !signed_window_index_target(window) {
        return None;
    }
    Some(SignedWindowSession::Named(session))
}

/// Parses `+`/`-` with an optional magnitude into a signed window-index offset.
fn parse_bare_relative_window_offset(value: &str) -> Result<Option<i64>, ExitFailure> {
    let Some(sign) = value.as_bytes().first().copied() else {
        return Ok(None);
    };
    if !matches!(sign, b'+' | b'-') {
        return Ok(None);
    }
    let Some(rest) = value.get(1..) else {
        return Ok(None);
    };
    let magnitude = if rest.is_empty() {
        1
    } else if rest.bytes().all(|byte| byte.is_ascii_digit()) {
        rest.parse::<i64>()
            .map_err(|error| ExitFailure::new(1, format!("invalid window index: {error}")))?
    } else {
        return Ok(None);
    };
    Ok(Some(if sign == b'-' { -magnitude } else { magnitude }))
}

/// Adds a signed offset to a window index, failing when the result leaves `u32` range.
fn apply_window_index_offset(index: u32, offset: i64) -> Result<u32, ExitFailure> {
    let next = i64::from(index) + offset;
    u32::try_from(next).map_err(|_| ExitFailure::new(1, format!("can't find window: {next}")))
}

/// Parses an all-digit target into an absolute window index.
fn parse_bare_window_index(value: &str) -> Result<Option<u32>, ExitFailure> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|error| ExitFailure::new(1, format!("invalid window index: {error}")))
}

/// Resolves a `session:` or plain session target to its session name.
fn resolve_session_only_destination(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<SessionName, ExitFailure> {
    match target.raw().strip_suffix(':') {
        Some(session) => resolve_raw_session(connection, session),
        None => resolve_session_target_spec(connection, target, false),
    }
}

/// Finds the lowest unused window index at or above the session's `base-index`.
fn first_available_window_index(
    connection: &mut Connection,
    session_name: &SessionName,
) -> Result<u32, ExitFailure> {
    let response = connection
        .list_windows(session_name.clone(), Some("#{window_index}".to_owned()))
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "list-windows")?;
    let used = String::from_utf8_lossy(output.stdout())
        .lines()
        .map(|line| {
            line.parse::<u32>().map_err(|error| {
                ExitFailure::new(1, format!("invalid list-windows index: {error}"))
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let base_index = session_base_index(connection, session_name)?;
    (base_index..=u32::MAX)
        .find(|index| !used.contains(index))
        .ok_or_else(|| ExitFailure::new(1, "window index space exhausted"))
}

/// Reads the session's effective `base-index` option from the server.
fn session_base_index(
    connection: &mut Connection,
    session_name: &SessionName,
) -> Result<u32, ExitFailure> {
    let response = connection
        .show_options(
            OptionScopeSelector::Session(session_name.clone()),
            Some("base-index".to_owned()),
            true,
            true,
            false,
        )
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "show-options")?;
    let value = String::from_utf8_lossy(output.stdout());
    value
        .trim()
        .parse::<u32>()
        .map_err(|error| ExitFailure::new(1, format!("invalid base-index value: {error}")))
}

/// Runs `move-window`, dispatching to relative insertion when `-a` or `-b` is given.
pub(super) fn run_move_window(
    args: MoveWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if args.after || args.before {
        return run_move_window_relative(&args, socket_path);
    }

    run_command_resolved(socket_path, "move-window", move |connection| {
        // `-r` renumbers a session (or one window) in place instead of moving a source window.
        let (source, target) = if args.reindex {
            let target = match args.target.as_ref() {
                Some(target) => resolve_move_window_reindex_target(connection, target)?,
                None => MoveWindowTarget::Session(resolve_current_session_target(connection)?),
            };
            (None, target)
        } else {
            let source = WindowTarget::resolve(connection, args.source.as_ref(), "move-window")?;
            let target = resolve_move_window_destination(connection, args.target.as_ref())?;
            (Some(source), MoveWindowTarget::Window(target))
        };
        connection
            .move_window(
                source,
                target,
                args.reindex,
                args.kill_target,
                args.detached,
            )
            .map_err(ExitFailure::from)
    })
}

/// Performs `move-window` insertion before or after an anchor window.
fn run_move_window_relative(args: &MoveWindowArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    let source = WindowTarget::resolve(&mut connection, args.source.as_ref(), "move-window")?;
    let target = resolve_window_placement_anchor_target(
        &mut connection,
        args.target.as_ref(),
        "move-window",
    )?;
    let response = connection
        .move_window_with_position(
            Some(source),
            MoveWindowTarget::Window(target),
            false,
            args.kill_target,
            args.detached,
            args.after,
            args.before,
        )
        .map_err(ExitFailure::from)?;
    match response {
        Response::MoveWindow(_) => Ok(0),
        other => Err(response_failure("move-window", &other)),
    }
}

/// Runs `swap-window`, exchanging the source and target windows.
pub(super) fn run_swap_window(
    args: SwapWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_command_resolved(socket_path, "swap-window", move |connection| {
        let source = resolve_window_source_or_marked_or_current(connection, args.source.as_ref())?;
        let target = resolve_existing_window_target_or_current(
            connection,
            args.target.as_ref(),
            "swap-window",
        )?;
        connection
            .swap_window(source, target, args.detached)
            .map_err(ExitFailure::from)
    })
}

/// Resolves the swap source: explicit target, else the marked pane's window, else the current one.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{marked}` is a tmux target token, not a format argument"
)]
fn resolve_window_source_or_marked_or_current(
    connection: &mut Connection,
    source: Option<&TargetSpec>,
) -> Result<WindowTarget, ExitFailure> {
    if let Some(source) = source {
        return resolve_window_target_spec(connection, source, false);
    }

    match resolve_window_target_spec(connection, &parse_spec("{marked}")?, false) {
        Ok(target) => Ok(target),
        Err(error) if error.message().contains("{marked}") => {
            WindowTarget::resolve_fallback(connection, "swap-window")
        }
        Err(error) => Err(error),
    }
}

/// Runs `rotate-window`, rotating panes in the target window and optionally restoring zoom.
pub(super) fn run_rotate_window(
    args: &RotateWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    WindowTarget::run(
        socket_path,
        "rotate-window",
        args.target.as_ref(),
        |connection, target| {
            connection.rotate_window_with_zoom(target, args.direction(), args.restore_zoom)
        },
    )
}

/// Runs `resize-window`, translating the direction and size flags into one adjustment.
pub(super) fn run_resize_window(
    args: &ResizeWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let adjust = args.adjustment.unwrap_or(1);
    let adjustment = if args.up {
        Some(ResizeWindowAdjustment::Up(adjust))
    } else if args.down {
        Some(ResizeWindowAdjustment::Down(adjust))
    } else if args.left {
        Some(ResizeWindowAdjustment::Left(adjust))
    } else if args.right {
        Some(ResizeWindowAdjustment::Right(adjust))
    } else if args.expand {
        Some(ResizeWindowAdjustment::LargestLinkedSession)
    } else if args.shrink {
        Some(ResizeWindowAdjustment::SmallestLinkedSession)
    } else {
        None
    };
    WindowTarget::run(
        socket_path,
        "resize-window",
        args.target.as_ref(),
        |connection, target| connection.resize_window(target, args.width, args.height, adjustment),
    )
}

/// Runs `respawn-window`, restarting the target window's panes with the given command.
pub(super) fn run_respawn_window(
    args: RespawnWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    WindowTarget::run(
        socket_path,
        "respawn-window",
        args.target.as_ref(),
        |connection, target| {
            connection.respawn_window_with_environment(
                target,
                args.kill,
                (!args.environment.is_empty()).then_some(args.environment),
                args.start_directory,
                (!args.command.is_empty()).then_some(args.command),
            )
        },
    )
}

/// Runs `unlink-window`, detaching the target window from its session.
pub(super) fn run_unlink_window(
    args: UnlinkWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_command_resolved(socket_path, "unlink-window", move |connection| {
        let target = resolve_existing_window_target_or_current(
            connection,
            args.target.as_ref(),
            "unlink-window",
        )?;
        connection
            .unlink_window(target, args.kill_if_last)
            .map_err(ExitFailure::from)
    })
}

/// Resolves the `-r` renumber target, which may name a session or a single window.
fn resolve_move_window_reindex_target(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<MoveWindowTarget, ExitFailure> {
    match target.raw().split_once(':') {
        Some((_, window_part)) if !window_part.is_empty() => Ok(MoveWindowTarget::Window(
            resolve_window_destination_target(connection, target, "move-window")?,
        )),
        Some((session_part, _)) => {
            resolve_raw_session(connection, session_part).map(MoveWindowTarget::Session)
        }
        None => {
            resolve_session_target_spec(connection, target, false).map(MoveWindowTarget::Session)
        }
    }
}

/// Resolves a move destination, defaulting to the current session's first free window index.
fn resolve_move_window_destination(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
) -> Result<WindowTarget, ExitFailure> {
    let Some(target) = target else {
        let session_name = resolve_current_session_target(connection)?;
        let index = first_available_window_index(connection, &session_name)?;
        return Ok(WindowTarget::with_window(session_name, index));
    };
    resolve_window_destination_target(connection, target, "move-window")
}

/// Runs `new-window`, placing, optionally reusing, creating and printing the new window.
pub(super) fn run_new_window(args: NewWindowArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let mut connection = connect_cli(socket_path)?;
    if args.kill_existing {
        let target = resolve_current_pane_target(&mut connection, "new-window")?;
        return run_queued_server_command_at_target_with_connection(
            &mut connection,
            "new-window",
            args.queue_command,
            target,
        );
    }

    let insert_at_target = args.after || args.before;
    let (target, target_window_index) = if insert_at_target {
        resolve_new_window_placement_target(
            &mut connection,
            args.target.as_ref(),
            args.after,
            "new-window",
        )?
    } else {
        resolve_new_window_target_spec(&mut connection, args.target.as_ref())?
    };
    if args.select_existing && target_window_index.is_none() {
        if let Some(existing) = args
            .name
            .as_deref()
            .and_then(|name| find_window_by_name(&mut connection, &target, name).transpose())
            .transpose()?
        {
            if !args.detached {
                connection
                    .select_window(existing)
                    .map_err(ExitFailure::from)?;
            }
            return Ok(0);
        }
    }
    let response = connection
        .new_window_at_with_environment(
            target,
            target_window_index,
            args.name,
            args.detached,
            (!args.environment.is_empty()).then_some(args.environment),
            args.start_directory
                .or_else(|| std::env::current_dir().ok()),
            (!args.command.is_empty()).then_some(args.command),
            insert_at_target,
        )
        .map_err(ExitFailure::from)?;
    let window = match response {
        Response::NewWindow(response) => response.target,
        Response::Error(ErrorResponse { error }) => {
            return Err(ExitFailure::new(
                1,
                tmux_cli_error_message("new-window", &error),
            ));
        }
        other => return Err(unexpected_response("new-window", &other)),
    };

    if args.print_target {
        let pane = PaneTarget::with_window(window.session_name().clone(), window.window_index(), 0);
        let format = args
            .format
            .as_deref()
            .unwrap_or(DEFAULT_NEW_WINDOW_PRINT_FORMAT);
        print_target_format(&mut connection, "new-window", Target::Pane(pane), format)?;
    }
    Ok(0)
}

/// Asks the server for `session_name`'s windows rendered as `index`, separator, `name` lines.
fn list_window_names(
    connection: &mut Connection,
    session_name: &SessionName,
) -> Result<Response, ExitFailure> {
    connection
        .list_windows(
            session_name.clone(),
            Some(format!(
                "#{{window_index}}{LISTING_FIELD_SEPARATOR}#{{window_name}}"
            )),
        )
        .map_err(ExitFailure::from)
}

/// Finds the uniquely named window in a session, failing when the name is ambiguous.
fn find_window_by_name(
    connection: &mut Connection,
    session_name: &SessionName,
    name: &str,
) -> Result<Option<WindowTarget>, ExitFailure> {
    let response = list_window_names(connection, session_name)?;
    let output = expect_command_output(&response, "list-windows")?;
    let mut matched = None;
    for (index, _) in String::from_utf8_lossy(output.stdout())
        .lines()
        .filter_map(|line| line.split_once(LISTING_FIELD_SEPARATOR))
        .filter(|(_, window_name)| *window_name == name)
    {
        let window_index = index
            .parse::<u32>()
            .map_err(|_| ExitFailure::new(1, format!("invalid window index: {index}")))?;
        let target = WindowTarget::with_window(session_name.clone(), window_index);
        if matched.replace(target).is_some() {
            return Err(ExitFailure::new(
                1,
                format!("multiple windows named {name}"),
            ));
        }
    }
    Ok(matched)
}

/// Resolves the session and insertion index for `new-window -a`/`-b`.
fn resolve_new_window_placement_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    after: bool,
    command_name: &str,
) -> Result<(SessionName, Option<u32>), ExitFailure> {
    let window = resolve_window_placement_anchor_target(connection, target, command_name)?;
    let window_index = if after {
        window.window_index().checked_add(1).ok_or_else(|| {
            ExitFailure::new(
                1,
                format!(
                    "window index space exhausted for session {}",
                    window.session_name()
                ),
            )
        })?
    } else {
        window.window_index()
    };
    Ok((window.session_name().clone(), Some(window_index)))
}

/// Resolves a `new-window` target into its session and an optional explicit window index.
fn resolve_new_window_target_spec(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
) -> Result<(SessionName, Option<u32>), ExitFailure> {
    let Some(target) = target else {
        return resolve_current_session_target(connection).map(|session| (session, None));
    };

    if let Some(window) =
        resolve_bare_relative_window_target(connection, target.raw(), "new-window")?
    {
        return Ok(new_window_slot(&window));
    }

    if new_window_target_requests_window_index(target.raw()) {
        let resolved =
            resolve_target_spec(connection, target, ResolveTargetType::Window, true, false)?;
        return match resolved {
            Target::Window(window) => Ok(new_window_slot(&window)),
            other => Err(wrong_target_kind(&other, "new-window")),
        };
    }

    if let Some(window) = resolve_new_window_bare_window_target(connection, target)? {
        return Ok(new_window_slot(&window));
    }

    if let Some(session_name) = resolve_new_window_session_only_target(connection, target)? {
        return Ok((session_name, None));
    }

    match resolve_target_spec(connection, target, ResolveTargetType::Session, false, false)? {
        Target::Session(session_name) => Ok((session_name, None)),
        other => Err(wrong_target_kind(&other, "new-window")),
    }
}

/// The session and explicit window index `new-window` creates its window at.
fn new_window_slot(window: &WindowTarget) -> (SessionName, Option<u32>) {
    (window.session_name().clone(), Some(window.window_index()))
}

/// Reports whether a `new-window` target names a window index rather than a session.
fn new_window_target_requests_window_index(raw_target: &str) -> bool {
    if is_special_window_token(raw_target) {
        return true;
    }
    if !raw_target.is_empty() && raw_target.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    if signed_window_index_target(raw_target) {
        return true;
    }
    raw_target.split_once(':').is_some_and(|(_, window_part)| {
        (!window_part.is_empty() && !window_part.contains('.'))
            || signed_window_index_target(window_part)
    })
}

/// Reports whether the value is `+` or `-` followed by optional digits.
fn signed_window_index_target(value: &str) -> bool {
    let Some(rest) = value.strip_prefix(['+', '-']) else {
        return false;
    };
    rest.is_empty() || rest.chars().all(|character| character.is_ascii_digit())
}

/// Resolves a bare name to an existing window only when that window's name really matches.
fn resolve_new_window_bare_window_target(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<Option<WindowTarget>, ExitFailure> {
    let raw = target.raw();
    if !new_window_target_is_bare_lookup(raw)
        || !matches!(target.exact(), None | Some(Target::Session(_)))
    {
        return Ok(None);
    }

    match resolve_target_spec(connection, target, ResolveTargetType::Window, false, false) {
        Ok(Target::Window(window)) => {
            Ok(window_name_matches_target(connection, &window, raw)?.then_some(window))
        }
        Ok(_) => Ok(None),
        Err(window_error) => match resolve_session_target_spec(connection, target, false) {
            Ok(_) => Ok(None),
            Err(session_error) if session_error.is_ambiguous_target() => Err(session_error),
            Err(_) => Err(window_error),
        },
    }
}

/// Reports whether a `new-window` target is an unprefixed name without `:` or `.` separators.
fn new_window_target_is_bare_lookup(raw_target: &str) -> bool {
    !raw_target.is_empty()
        && !raw_target.starts_with(['@', '$', '%', '+', '-', '='])
        && !raw_target.contains([':', '.'])
}

/// Checks the server-reported name of `window` against an exact, prefix or `fnmatch` pattern.
fn window_name_matches_target(
    connection: &mut Connection,
    window: &WindowTarget,
    target: &str,
) -> Result<bool, ExitFailure> {
    let response = list_window_names(connection, window.session_name())?;
    let output = expect_command_output(&response, "list-windows")?;
    let window_index = window.window_index().to_string();
    Ok(String::from_utf8_lossy(output.stdout())
        .lines()
        .filter_map(|line| line.split_once(LISTING_FIELD_SEPARATOR))
        .find(|(index, _)| *index == window_index)
        .is_some_and(|(_, window_name)| {
            window_name == target
                || window_name.starts_with(target)
                || rmux_core::fnmatch(target, window_name)
        }))
}

/// Resolves a `session:` target with an empty window part to that session.
fn resolve_new_window_session_only_target(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<Option<SessionName>, ExitFailure> {
    match target.raw().split_once(':') {
        Some((session_name, "")) => resolve_raw_session(connection, session_name).map(Some),
        _ => Ok(None),
    }
}

/// Runs `kill-window`, destroying the target window or every other window with `-a`.
pub(super) fn run_kill_window(
    args: &KillWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    WindowTarget::run(
        socket_path,
        "kill-window",
        args.target.as_ref(),
        |connection, target| connection.kill_window(target, args.kill_others),
    )
}

/// Runs `select-window`, handling the next, previous and last shortcuts plus `-T` toggling.
pub(super) fn run_select_window(
    args: SelectWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if args.next || args.previous || args.last {
        let target = args.target.as_ref();
        return SessionName::run(
            socket_path,
            "select-window",
            target,
            |connection, session| {
                if args.next {
                    connection.next_window(session, false)
                } else if args.previous {
                    connection.previous_window(session, false)
                } else {
                    connection.last_window(session)
                }
            },
        );
    }

    run_command_resolved(socket_path, "select-window", move |connection| {
        let target = WindowTarget::resolve(connection, args.target.as_ref(), "select-window")?;
        if args.toggle_last && window_target_is_current(connection, &target)? {
            return connection
                .last_window(target.session_name().clone())
                .map_err(ExitFailure::from);
        }

        connection.select_window(target).map_err(ExitFailure::from)
    })
}

/// Reports whether the target window is the client's currently active window.
fn window_target_is_current(
    connection: &mut Connection,
    target: &WindowTarget,
) -> Result<bool, ExitFailure> {
    let current = resolve_current_pane_target(connection, "select-window")?;
    Ok(target.session_name() == current.session_name()
        && target.window_index() == current.window_index())
}

/// Runs `rename-window` after escaping backslashes in the new name.
pub(super) fn run_rename_window(
    args: &RenameWindowArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    // Backslashes are doubled so the new name survives format interpretation.
    WindowTarget::run(
        socket_path,
        "rename-window",
        args.target.as_ref(),
        |connection, target| connection.rename_window(target, args.new_name.replace('\\', r"\\")),
    )
}

/// Runs `next-window`, optionally restricting movement to windows with alerts.
pub(super) fn run_next_window(
    args: &AlertSessionTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    SessionName::run(
        socket_path,
        "next-window",
        args.target.as_ref(),
        |connection, target| connection.next_window(target, args.alerts_only),
    )
}

/// Runs `previous-window`, optionally restricting movement to windows with alerts.
pub(super) fn run_previous_window(
    args: &AlertSessionTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    SessionName::run(
        socket_path,
        "previous-window",
        args.target.as_ref(),
        |connection, target| connection.previous_window(target, args.alerts_only),
    )
}

/// Runs `last-window`, returning the session to its previously selected window.
pub(super) fn run_last_window(
    args: &SessionTargetArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    SessionName::run(
        socket_path,
        "last-window",
        args.target.as_ref(),
        |connection, target| connection.last_window(target),
    )
}

/// Runs `list-windows`, using the server-side `-a` queue when available, then sorts or emits JSON.
#[allow(
    clippy::too_many_lines,
    reason = "one linear list-windows pipeline; splitting it would obscure the ordering"
)]
pub(super) fn run_list_windows(
    args: &ListWindowsArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let json = args.json;
    let mut connection = connect_cli(socket_path)?;
    let queued_all_sessions = args.all_sessions
        && connection
            .supports_capability(CAPABILITY_CLI_LIST_WINDOWS_ALL_QUEUE)
            .map_err(ExitFailure::from)?;
    if queued_all_sessions {
        let json_format = json.then(list_windows_json_format);
        let arguments = list_windows_all_queue_arguments(args, json_format.as_deref());
        if json {
            let (exit_status, output) = capture_list_windows_all_server_command_with_connection(
                &mut connection,
                &arguments,
            )?;
            if exit_status != 0 {
                return Ok(exit_status);
            }
            return write_length_prefixed_list_windows_json(&output);
        }
        return run_list_windows_all_server_command_with_connection(&mut connection, &arguments);
    }
    let targets = if args.all_sessions {
        list_session_names(&mut connection)?
    } else {
        vec![SessionName::resolve(
            &mut connection,
            args.target.as_ref(),
            "list-windows",
        )?]
    };
    let sort_order = CliWindowListSortOrder::parse(args.sort_order.as_deref())?;
    let include_sort_metadata = args.all_sessions
        && matches!(
            sort_order,
            CliWindowListSortOrder::Activity | CliWindowListSortOrder::Creation
        );
    let format = list_windows_server_format(
        args.format.as_deref(),
        args.filter.as_deref(),
        args.all_sessions,
        include_sort_metadata,
    );
    let mut lines = Vec::new();
    let mut windows = Vec::new();
    for target in targets {
        let response = connection
            .list_windows_with_options(
                target,
                format.clone(),
                args.filter.clone(),
                (!args.all_sessions)
                    .then(|| args.sort_order.clone())
                    .flatten(),
                !args.all_sessions && args.reversed,
            )
            .map_err(ExitFailure::from)?;
        let response = match response {
            Response::ListWindows(response) => response,
            other => return Err(response_failure("list-windows", &other)),
        };
        let parsed_windows = response
            .windows
            .into_iter()
            .filter_map(|window| {
                list_window_entry(window, args.filter.as_deref(), include_sort_metadata).transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        if json || args.all_sessions {
            windows.extend(parsed_windows);
        } else {
            lines.extend(
                parsed_windows
                    .into_iter()
                    .map(|window| window.window.rendered),
            );
        }
    }
    if args.all_sessions {
        sort_all_session_window_entries(&mut windows, sort_order, args.reversed);
        if !json {
            lines.extend(windows.iter().map(|window| window.window.rendered.clone()));
        }
    }
    if json {
        return write_list_windows_json(&ListWindowsResponse {
            windows: windows.into_iter().map(|window| window.window).collect(),
            output: CommandOutput::from_stdout(Vec::new()),
        });
    }
    write_lines_output(&lines)
}

/// Rebuilds the `list-windows -a` argument vector for server-side execution.
fn list_windows_all_queue_arguments(
    args: &ListWindowsArgs,
    format_override: Option<&str>,
) -> Vec<String> {
    let mut command = vec!["list-windows".to_owned(), "-a".to_owned()];
    let flags = [
        ("-t", args.target.as_ref().map(TargetSpec::raw)),
        ("-F", format_override.or(args.format.as_deref())),
        ("-f", args.filter.as_deref()),
        ("-O", args.sort_order.as_deref()),
    ];
    for (flag, value) in flags {
        if let Some(value) = value {
            command.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    if args.reversed {
        command.push("-r".to_owned());
    }
    command
}

/// Sort order requested by `list-windows -O` and applied client-side across sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliWindowListSortOrder {
    Index,
    Name,
    Size,
    Activity,
    Creation,
    ExplicitIndex,
}

impl CliWindowListSortOrder {
    /// Parses the `-O` value, rejecting unknown orders with the invalid-sort-order message.
    fn parse(value: Option<&str>) -> Result<Self, ExitFailure> {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") => Ok(Self::Index),
            Some("index" | "order") => Ok(Self::ExplicitIndex),
            Some("name" | "title") => Ok(Self::Name),
            Some("size") => Ok(Self::Size),
            Some("activity") => Ok(Self::Activity),
            Some("creation") => Ok(Self::Creation),
            Some(_) => Err(ExitFailure::new(
                1,
                rmux_core::INVALID_SORT_ORDER.to_owned(),
            )),
        }
    }

    /// Reports whether the user asked for a specific order rather than the server's default.
    const fn is_explicit(self) -> bool {
        !matches!(self, Self::Index)
    }
}

/// One listed window plus the activity and creation timestamps used for sorting.
struct CliWindowListEntry {
    window: WindowListEntry,
    activity_at: Option<i64>,
    created_at: Option<i64>,
}

/// Strips the filter and sort-metadata prefixes from one listed window, dropping filtered rows.
fn list_window_entry(
    mut window: WindowListEntry,
    filter: Option<&str>,
    include_sort_metadata: bool,
) -> Result<Option<CliWindowListEntry>, ExitFailure> {
    let Some(rendered) = filtered_listing_line(&window.rendered, filter, "list-windows")? else {
        return Ok(None);
    };
    let (activity_at, created_at, rendered) =
        list_windows_sort_metadata(rendered, include_sort_metadata)?;
    window.rendered = rendered.to_owned();
    Ok(Some(CliWindowListEntry {
        window,
        activity_at,
        created_at,
    }))
}

/// Sorts all-session entries by the requested key, with stable session, index and name tiebreaks.
fn sort_all_session_window_entries(
    windows: &mut [CliWindowListEntry],
    sort_order: CliWindowListSortOrder,
    reversed: bool,
) {
    if !sort_order.is_explicit() {
        return;
    }
    windows.sort_by(|left, right| {
        let primary = match sort_order {
            CliWindowListSortOrder::Index | CliWindowListSortOrder::ExplicitIndex => left
                .window
                .target
                .window_index()
                .cmp(&right.window.target.window_index()),
            CliWindowListSortOrder::Name => stable_window_entry_name_cmp(left, right),
            CliWindowListSortOrder::Size => {
                cli_terminal_area(left.window.size).cmp(&cli_terminal_area(right.window.size))
            }
            CliWindowListSortOrder::Activity => right.activity_at.cmp(&left.activity_at),
            CliWindowListSortOrder::Creation => left.created_at.cmp(&right.created_at),
        };
        let primary = if reversed { primary.reverse() } else { primary };
        primary
            .then_with(|| {
                left.window
                    .target
                    .session_name()
                    .as_str()
                    .cmp(right.window.target.session_name().as_str())
            })
            .then_with(|| {
                left.window
                    .target
                    .window_index()
                    .cmp(&right.window.target.window_index())
            })
            .then_with(|| stable_window_entry_name_cmp(left, right))
    });
}

/// Window area in cells, the sort key for `-O size`.
fn cli_terminal_area(size: rmux_proto::TerminalSize) -> u64 {
    u64::from(size.cols) * u64::from(size.rows)
}

/// Compares two listed windows by name, the tiebreaker that keeps sorts deterministic.
fn stable_window_entry_name_cmp(left: &CliWindowListEntry, right: &CliWindowListEntry) -> Ordering {
    left.window.name.cmp(&right.window.name)
}

/// Builds the format string sent to the server, prefixing filter and sort-metadata fields.
fn list_windows_server_format(
    format: Option<&str>,
    filter: Option<&str>,
    all_sessions: bool,
    include_sort_metadata: bool,
) -> Option<String> {
    let default_format = if all_sessions {
        DEFAULT_LIST_WINDOWS_ALL_FORMAT
    } else {
        DEFAULT_LIST_WINDOWS_FORMAT
    };
    let mut line_format = format
        .map(ToOwned::to_owned)
        .or_else(|| all_sessions.then(|| default_format.to_owned()));
    if include_sort_metadata {
        let rendered = line_format.as_deref().unwrap_or(default_format);
        line_format = Some(format!(
            "{LIST_WINDOWS_SORT_METADATA_FORMAT}{LISTING_FIELD_SEPARATOR}{rendered}"
        ));
    }
    filter
        .map(|filter| {
            let line_format = line_format.as_deref().unwrap_or(default_format);
            format!("{filter}{LISTING_FIELD_SEPARATOR}{line_format}")
        })
        .or(line_format)
}

/// Strips the prefixed activity and creation timestamps from a rendered line.
fn list_windows_sort_metadata(
    line: &str,
    include_sort_metadata: bool,
) -> Result<(Option<i64>, Option<i64>, &str), ExitFailure> {
    if !include_sort_metadata {
        return Ok((None, None, line));
    }

    let Some((activity, rest)) = line.split_once(LISTING_FIELD_SEPARATOR) else {
        return Err(ExitFailure::new(
            1,
            "list-windows sort metadata missing activity separator",
        ));
    };
    let Some((created, rendered)) = rest.split_once(LISTING_FIELD_SEPARATOR) else {
        return Err(ExitFailure::new(
            1,
            "list-windows sort metadata missing creation separator",
        ));
    };
    let activity_at = activity.parse::<i64>().map_err(|_| {
        ExitFailure::new(1, "list-windows sort metadata has invalid activity value")
    })?;
    let created_at = created.parse::<i64>().map_err(|_| {
        ExitFailure::new(1, "list-windows sort metadata has invalid creation value")
    })?;
    Ok((Some(activity_at), Some(created_at), rendered))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_window_targets_request_window_index_only_for_window_tokens() {
        for (raw, window_index) in [
            ("+3", true),
            ("-1", true),
            ("3", true),
            ("alpha:+3", true),
            ("alpha:-1", true),
            ("^", true),
            ("!", true),
            ("alpha", false),
            ("alpha:", false),
        ] {
            assert_eq!(
                new_window_target_requests_window_index(raw),
                window_index,
                "{raw}"
            );
        }
    }

    #[test]
    fn only_colon_and_exact_session_link_targets_are_explicit_session_only() {
        for (raw, session_only) in [
            ("3", false),
            ("beta", false),
            ("beta:", true),
            ("=beta", true),
            ("{end}", false),
            ("+1", false),
        ] {
            let target = crate::cli_args::parse_target_spec(raw).expect("target parses");
            assert_eq!(
                link_target_is_explicit_session_only(&target),
                session_only,
                "{raw}"
            );
        }
    }
}
