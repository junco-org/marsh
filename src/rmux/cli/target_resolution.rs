use std::fmt::Display;
use std::path::Path;

use rmux_client::{Connection, connect};
use rmux_core::formats::is_truthy;
use rmux_proto::request::ListSessionsRequest;
use rmux_proto::{
    ErrorResponse, PaneTarget, ResolveTargetType, Response, RmuxError, SessionName, Target,
    WindowTarget,
};

use crate::cli_args::{TargetSpec, parse_target_spec};
use crate::cli_response::expect_command_output;

use super::{ExitFailure, run_command_resolved, unexpected_response};

/// Connects to the server socket, mapping client errors into a CLI `ExitFailure`.
pub(super) fn connect_cli(socket_path: &Path) -> Result<Connection, ExitFailure> {
    connect(socket_path).map_err(|error| ExitFailure::from_client_connect(socket_path, error))
}

/// Parses raw target text into a spec, reporting malformed text as an exit-code-`1` failure.
pub(super) fn parse_spec(raw: &str) -> Result<TargetSpec, ExitFailure> {
    parse_target_spec(raw).map_err(|error| ExitFailure::new(1, error))
}

/// Signature of the `-t`-or-fallback resolvers behind [`CommandTarget`].
type TargetResolver<T> = fn(&mut Connection, Option<&TargetSpec>, &str) -> Result<T, ExitFailure>;

/// A target a command resolves from its optional `-t` spec before it builds its request.
pub(super) trait CommandTarget: Sized {
    /// Resolves the spec, or the command's fallback target when none was given.
    const RESOLVE: TargetResolver<Self>;
}

impl CommandTarget for SessionName {
    const RESOLVE: TargetResolver<Self> = resolve_session_target_or_current;
}

impl CommandTarget for WindowTarget {
    const RESOLVE: TargetResolver<Self> = resolve_window_target_or_current;
}

impl CommandTarget for PaneTarget {
    const RESOLVE: TargetResolver<Self> = resolve_pane_target_or_current;
}

impl CommandTarget for Option<PaneTarget> {
    const RESOLVE: TargetResolver<Self> =
        |connection, target, _| resolve_optional_pane_target(connection, target);
}

/// Resolves a command's `-t` target as `T`, sends the request `send` builds for it, and reports
/// the response the way every one-shot command does.
pub(super) fn run_targeted<T: CommandTarget, E>(
    socket_path: &Path,
    command_name: &'static str,
    target: Option<&TargetSpec>,
    send: impl FnOnce(&mut Connection, T) -> Result<Response, E>,
) -> Result<i32, ExitFailure>
where
    ExitFailure: From<E>,
{
    run_command_resolved(socket_path, command_name, |connection| {
        let target = T::RESOLVE(connection, target, command_name)?;
        send(connection, target).map_err(ExitFailure::from)
    })
}

/// The failure for a response a command did not expect: a server error by its own message,
/// anything else as a protocol error naming `command_name`.
pub(super) fn response_failure(command_name: &str, response: &Response) -> ExitFailure {
    match response {
        Response::Error(ErrorResponse { error }) => ExitFailure::new(1, error.to_string()),
        other => unexpected_response(command_name, other),
    }
}

/// Asks the server which session the current client is attached to.
pub(super) fn resolve_current_session_target(
    connection: &mut Connection,
) -> Result<SessionName, ExitFailure> {
    resolve_current_as(connection, ToString::to_string)
}

/// Lists every live session name by formatting `list-sessions` output one name per line.
pub(super) fn list_session_names(
    connection: &mut Connection,
) -> Result<Vec<SessionName>, ExitFailure> {
    let response = connection
        .list_sessions(ListSessionsRequest {
            format: Some("#{session_name}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "list-sessions")?;
    String::from_utf8_lossy(output.stdout())
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            SessionName::new(line).map_err(|error| {
                ExitFailure::new(
                    1,
                    format!("invalid session name from server '{line}': {error}"),
                )
            })
        })
        .collect()
}

/// Resolves a `-t` spec to a session name, optionally preferring an unattached session.
pub(super) fn resolve_session_target_spec(
    connection: &mut Connection,
    target: &TargetSpec,
    prefer_unattached: bool,
) -> Result<SessionName, ExitFailure> {
    resolve_spec_as(connection, target, false, prefer_unattached)
}

/// Resolves a `-t` spec to a window, short-circuiting an exact `@id` spec without a round trip.
pub(super) fn resolve_window_target_spec(
    connection: &mut Connection,
    target: &TargetSpec,
    window_index: bool,
) -> Result<WindowTarget, ExitFailure> {
    if target.raw().starts_with('@') {
        if let Some(Target::Window(target)) = target.exact() {
            return Ok(target.clone());
        }
    }
    resolve_spec_as(connection, target, window_index, false)
}

/// Resolves an optional window spec, falling back to the window holding the current pane.
pub(super) fn resolve_window_target_or_current(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    match target {
        Some(target) => resolve_window_target_spec(connection, target, false),
        None => {
            resolve_current_pane_target(connection, command_name).map(|pane| pane_window(&pane))
        }
    }
}

/// Resolves an optional window spec as an index, defaulting to the current session's window.
pub(super) fn resolve_window_index_target_or_current_session(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    if let Some(target) = target {
        return resolve_window_target_spec(connection, target, true);
    }

    let session_name = resolve_session_target_or_current(connection, None, command_name)?;
    let implicit = parse_spec(&format!("{session_name}:"))?;
    resolve_window_target_spec(connection, &implicit, true)
}

/// Resolves a `-t` spec to a pane, failing when the server names a different target kind.
pub(super) fn resolve_pane_target_spec(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<PaneTarget, ExitFailure> {
    resolve_spec_as(connection, target, false, false)
}

/// Resolves an optional pane spec, leaving the pane unset when none was given.
pub(super) fn resolve_optional_pane_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
) -> Result<Option<PaneTarget>, ExitFailure> {
    target
        .map(|target| resolve_pane_target_spec(connection, target))
        .transpose()
}

/// Resolves a `-t` spec to a pane, mapping a server error to `None` instead of failing.
pub(super) fn resolve_canfail_pane_target_spec(
    connection: &mut Connection,
    target: &TargetSpec,
) -> Result<Option<PaneTarget>, ExitFailure> {
    let response = connection
        .resolve_target(
            Some(target.raw().to_owned()),
            ResolveTargetType::Pane,
            false,
            false,
        )
        .map_err(ExitFailure::from)?;
    match response {
        Response::ResolveTarget(response) => narrow_target(response.target).map(Some),
        Response::Error(_) => Ok(None),
        other => Err(unexpected_response("resolve-target", &other)),
    }
}

/// Unit separator between the prefixed filter or metadata fields and the rendered listing line.
pub(super) const LISTING_FIELD_SEPARATOR: char = '\x1f';

/// Splits a line rendered as `filter`, separator, `format`, keeping the rendered text only when
/// the filter field is truthy; lines pass through untouched when no filter was requested.
pub(super) fn filtered_listing_line<'a>(
    line: &'a str,
    filter: Option<&str>,
    command_name: &str,
) -> Result<Option<&'a str>, ExitFailure> {
    if filter.is_none() {
        return Ok(Some(line));
    }
    let Some((filter_value, rendered_line)) = line.split_once(LISTING_FIELD_SEPARATOR) else {
        return Err(ExitFailure::new(
            1,
            format!("{command_name} filter output missing separator"),
        ));
    };
    Ok(is_truthy(filter_value).then_some(rendered_line))
}

/// Reports whether a listed visible pane index equals the target index shifted by the base index.
pub(super) fn listed_pane_index_matches_target(
    target: &PaneTarget,
    visible_pane_index: &str,
    pane_base_index: &str,
) -> bool {
    let (Ok(visible_pane_index), Ok(pane_base_index)) = (
        visible_pane_index.parse::<u32>(),
        pane_base_index.parse::<u32>(),
    ) else {
        return false;
    };
    target.pane_index().saturating_add(pane_base_index) == visible_pane_index
}

/// Resolves an optional existing-window spec, falling back to the current pane's window.
pub(super) fn resolve_existing_window_target_or_current(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<WindowTarget, ExitFailure> {
    match target {
        Some(target) => resolve_spec_as(connection, target, false, false),
        None => {
            resolve_current_pane_target(connection, command_name).map(|pane| pane_window(&pane))
        }
    }
}

/// Resolves an optional pane spec, falling back to the client's current pane.
pub(super) fn resolve_pane_target_or_current(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<PaneTarget, ExitFailure> {
    match target {
        Some(target) => resolve_pane_target_spec(connection, target),
        None => resolve_current_pane_target(connection, command_name),
    }
}

/// The single funnel every `-t` spec passes through, rewriting Claude session namespaces first.
pub(super) fn resolve_target_spec(
    connection: &mut Connection,
    target: &TargetSpec,
    target_type: ResolveTargetType,
    window_index: bool,
    prefer_unattached: bool,
) -> Result<Target, ExitFailure> {
    // Structural resolution is the single funnel every `-t` target passes through, so it is
    // where a Claude shim invocation's logical session names become this invocation's owned
    // ones. Only the whole session component is rewritten; the `:window.pane` suffix and the
    // `=` exact marker are preserved exactly as written.
    let raw = super::claude_namespace::rewrite_target(target.raw());
    let response = connection
        .resolve_target(Some(raw), target_type, window_index, prefer_unattached)
        .map_err(ExitFailure::from)?;
    match response {
        Response::ResolveTarget(response) => Ok(response.target),
        Response::Error(ErrorResponse { error }) => {
            Err(target_resolution_failure(&error, target_type, target.raw()))
        }
        other => Err(unexpected_response("resolve-target", &other)),
    }
}

/// Turns a server resolution error into an exit failure, flagging ambiguity with its own code.
fn target_resolution_failure(
    error: &RmuxError,
    target_type: ResolveTargetType,
    raw_target: &str,
) -> ExitFailure {
    let message = target_resolution_error_message(error, target_type, raw_target);
    if matches!(
        error,
        RmuxError::InvalidTarget { reason, .. } if reason.starts_with("ambiguous ")
    ) {
        ExitFailure::ambiguous_target(message)
    } else {
        ExitFailure::new(1, message)
    }
}

/// Rewrites a server resolution error into the `can't find <kind>: <token>` wording tmux prints.
fn target_resolution_error_message(
    error: &RmuxError,
    target_type: ResolveTargetType,
    raw_target: &str,
) -> String {
    match error {
        RmuxError::InvalidTarget { reason, .. }
            if target_type == ResolveTargetType::Window
                && raw_target.rsplit_once('.').is_some()
                && reason.starts_with("can't find window") =>
        {
            format!("can't find pane: {}", pane_target_lookup_token(raw_target))
        }
        RmuxError::InvalidTarget { reason, .. } if reason.starts_with("can't find ") => {
            reason.clone()
        }
        RmuxError::InvalidTarget { reason, .. }
            if target_type == ResolveTargetType::Pane
                && reason == "pane index does not exist in session" =>
        {
            format!("can't find pane: {}", pane_target_lookup_token(raw_target))
        }
        RmuxError::Server(message)
            if target_type == ResolveTargetType::Pane && message == "no current target" =>
        {
            format!("can't find pane: {}", pane_target_lookup_token(raw_target))
        }
        RmuxError::InvalidTarget { reason, .. }
            if target_type == ResolveTargetType::Window
                && reason == "window index does not exist in session" =>
        {
            format!(
                "can't find window: {}",
                window_target_lookup_token(raw_target)
            )
        }
        RmuxError::Server(message)
            if target_type == ResolveTargetType::Window && message == "no current target" =>
        {
            format!(
                "can't find window: {}",
                window_target_lookup_token(raw_target)
            )
        }
        RmuxError::SessionNotFound(_) if target_type == ResolveTargetType::Window => {
            format!(
                "can't find window: {}",
                window_target_lookup_token(raw_target)
            )
        }
        _ => error.to_string(),
    }
}

/// Extracts the pane component a `can't find pane` message should name.
fn pane_target_lookup_token(raw_target: &str) -> &str {
    if raw_target.starts_with('%') {
        return raw_target;
    }
    raw_target
        .rsplit_once('.')
        .map_or(raw_target, |(_, pane)| pane)
}

/// Extracts the window component a `can't find window` message should name.
fn window_target_lookup_token(raw_target: &str) -> &str {
    if raw_target.starts_with('@') {
        return raw_target;
    }
    raw_target
        .rsplit_once(':')
        .map_or(raw_target, |(_, window)| window)
}

/// Names the target kind a `resolve-target` response carried, for mismatch diagnostics.
const fn response_name_for_target(target: &Target) -> &'static str {
    match target {
        Target::Session(_) => "session target",
        Target::Window(_) => "window target",
        Target::Pane(_) => "pane target",
    }
}

/// The failure for a `resolve-target` answer of another kind than the `required` one.
pub(super) fn wrong_target_kind(target: &Target, required: &str) -> ExitFailure {
    ExitFailure::new(
        1,
        format!(
            "resolve-target produced {} where a {required} target was required",
            response_name_for_target(target)
        ),
    )
}

/// Resolves an optional session spec, defaulting to the current session.
pub(super) fn resolve_session_target_or_current(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &str,
) -> Result<SessionName, ExitFailure> {
    if let Some(target) = target {
        return resolve_session_target_spec(connection, target, false);
    }

    let _ = command_name;
    resolve_current_session_target(connection)
}

/// Asks the server for the client's current pane, reporting misses as `can't find pane`.
pub(super) fn resolve_current_pane_target(
    connection: &mut Connection,
    command_name: &str,
) -> Result<PaneTarget, ExitFailure> {
    resolve_current_as(connection, |error| {
        target_resolution_error_message(error, ResolveTargetType::Pane, command_name)
    })
}

/// Asks the server for the index of the active window in `session_name`.
pub(super) fn resolve_active_window_index(
    connection: &mut Connection,
    session_name: &SessionName,
    command_name: &str,
) -> Result<u32, ExitFailure> {
    let response = connection
        .list_windows(
            session_name.clone(),
            Some("#{window_index}:#{window_active}".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "list-windows")?;
    active_row_index(output.stdout(), command_name, "window", session_name)
}

/// Asks the server for the index of the active pane in the given session and window.
pub(super) fn resolve_active_pane_index(
    connection: &mut Connection,
    session_name: &SessionName,
    window_index: u32,
    command_name: &str,
) -> Result<u32, ExitFailure> {
    let response = connection
        .list_panes_in_window(
            session_name.clone(),
            Some(window_index),
            Some("#{pane_index}:#{pane_active}".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "list-panes")?;
    let scope = format!("{session_name}:{window_index}");
    active_row_index(output.stdout(), command_name, "pane", &scope)
}

/// Returns the index of the first `index:active` listing row flagged active.
fn active_row_index(
    listing: &[u8],
    command_name: &str,
    kind: &str,
    scope: &dyn Display,
) -> Result<u32, ExitFailure> {
    let listing = String::from_utf8_lossy(listing);
    let Some(index) = listing
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(index, active)| (active == "1").then_some(index))
    else {
        return Err(ExitFailure::new(
            1,
            format!("{command_name}: no active {kind} in session {scope}"),
        ));
    };
    index.parse::<u32>().map_err(|error| {
        ExitFailure::new(1, format!("{command_name}: invalid active {kind}: {error}"))
    })
}

/// The window holding `pane`.
pub(super) fn pane_window(pane: &PaneTarget) -> WindowTarget {
    WindowTarget::with_window(pane.session_name().clone(), pane.window_index())
}

/// The window a resolved target names, a bare session standing for its current window.
pub(super) fn target_window(target: Target) -> WindowTarget {
    match target {
        Target::Session(session_name) => WindowTarget::new(session_name),
        Target::Window(window) => window,
        Target::Pane(pane) => pane_window(&pane),
    }
}

/// The session a resolved target belongs to.
pub(super) fn target_session(target: Target) -> SessionName {
    match target {
        Target::Session(session_name) => session_name,
        Target::Window(window) => window.session_name().clone(),
        Target::Pane(pane) => pane.session_name().clone(),
    }
}

/// A concrete kind a `resolve-target` answer is narrowed to.
trait TargetKind: Sized {
    /// The target type `resolve-target` is asked for.
    const TYPE: ResolveTargetType;
    /// The kind word naming this kind in mismatch diagnostics.
    const NAME: &'static str;
    /// Takes this kind out of a resolved target, handing any other kind back.
    fn narrow(target: Target) -> Result<Self, Target>;
}

impl TargetKind for SessionName {
    const TYPE: ResolveTargetType = ResolveTargetType::Session;
    const NAME: &'static str = "session";
    fn narrow(target: Target) -> Result<Self, Target> {
        match target {
            Target::Session(session_name) => Ok(session_name),
            other => Err(other),
        }
    }
}

impl TargetKind for WindowTarget {
    const TYPE: ResolveTargetType = ResolveTargetType::Window;
    const NAME: &'static str = "window";
    fn narrow(target: Target) -> Result<Self, Target> {
        match target {
            Target::Window(window) => Ok(window),
            other => Err(other),
        }
    }
}

impl TargetKind for PaneTarget {
    const TYPE: ResolveTargetType = ResolveTargetType::Pane;
    const NAME: &'static str = "pane";
    fn narrow(target: Target) -> Result<Self, Target> {
        match target {
            Target::Pane(pane) => Ok(pane),
            other => Err(other),
        }
    }
}

/// Narrows a resolved target to kind `T`, failing when the server produced another kind.
fn narrow_target<T: TargetKind>(target: Target) -> Result<T, ExitFailure> {
    T::narrow(target).map_err(|other| wrong_target_kind(&other, T::NAME))
}

/// Resolves a `-t` spec through [`resolve_target_spec`] as target kind `T`.
fn resolve_spec_as<T: TargetKind>(
    connection: &mut Connection,
    target: &TargetSpec,
    window_index: bool,
    prefer_unattached: bool,
) -> Result<T, ExitFailure> {
    narrow_target(resolve_target_spec(
        connection,
        target,
        T::TYPE,
        window_index,
        prefer_unattached,
    )?)
}

/// Asks the server for the client's current target of kind `T`, wording a server error with
/// `error_message`.
fn resolve_current_as<T: TargetKind>(
    connection: &mut Connection,
    error_message: impl FnOnce(&RmuxError) -> String,
) -> Result<T, ExitFailure> {
    match connection
        .resolve_target(None, T::TYPE, false, false)
        .map_err(ExitFailure::from)?
    {
        Response::ResolveTarget(response) => narrow_target(response.target),
        Response::Error(ErrorResponse { error }) => Err(ExitFailure::new(1, error_message(&error))),
        other => Err(unexpected_response("resolve-target", &other)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn listed_pane_index_matches_visible_base_adjustment() {
        let target = rmux_proto::PaneTarget::with_window(
            rmux_proto::SessionName::new("alpha").expect("valid session name"),
            0,
            1,
        );

        assert!(listed_pane_index_matches_target(&target, "2", "1"));
        assert!(!listed_pane_index_matches_target(&target, "1", "1"));
        assert!(!listed_pane_index_matches_target(&target, "x", "1"));
        assert!(!listed_pane_index_matches_target(&target, "1", "x"));
    }

    #[test]
    fn listed_pane_index_matches_server_saturation() {
        let target = rmux_proto::PaneTarget::with_window(
            rmux_proto::SessionName::new("alpha").expect("valid session name"),
            0,
            u32::MAX,
        );

        assert!(listed_pane_index_matches_target(
            &target,
            &u32::MAX.to_string(),
            "1"
        ));
    }

    #[test]
    fn window_resolution_no_current_target_reports_missing_window() {
        let message = target_resolution_error_message(
            &RmuxError::Server("no current target".to_owned()),
            ResolveTargetType::Window,
            "missing",
        );
        assert_eq!(message, "can't find window: missing");
    }

    #[test]
    fn window_resolution_uses_window_part_for_session_window_targets() {
        let message = target_resolution_error_message(
            &RmuxError::Server("no current target".to_owned()),
            ResolveTargetType::Window,
            "alpha:missing",
        );
        assert_eq!(message, "can't find window: missing");
    }

    #[test]
    fn window_resolution_uses_pane_part_for_window_dot_pane_targets() {
        let message = target_resolution_error_message(
            &RmuxError::InvalidTarget {
                value: "0.5".to_owned(),
                reason: "can't find window: 0.5".to_owned(),
            },
            ResolveTargetType::Window,
            "0.5",
        );
        assert_eq!(message, "can't find pane: 5");
    }

    #[test]
    fn window_resolution_session_miss_reports_requested_window_index() {
        let message = target_resolution_error_message(
            &RmuxError::SessionNotFound("a".to_owned()),
            ResolveTargetType::Window,
            "a:99",
        );
        assert_eq!(message, "can't find window: 99");
    }
}
