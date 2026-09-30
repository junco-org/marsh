use std::io::{self, ErrorKind, Write};
use std::time::{Duration, Instant};

use rmux_client::Connection;
use rmux_proto::{
    CommandOutput, ErrorResponse, PaneId, PaneSnapshotCell, PaneSnapshotResponse, PaneTarget,
    PaneTargetRef, Response, SessionName, Target,
};
use serde_json::{Value, json};

use crate::cli_args::TargetSpec;
use crate::cli_response::tmux_cli_error_message;

use super::super::{CommandTarget, ExitFailure, listed_pane_index_matches_target};
use super::pane_exit::PaneExitStatus;

pub(super) const SCHEMA_VERSION: u8 = 1;
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DEFAULT_STABLE_FOR: Duration = Duration::from_millis(300);
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Fails with exit code `1` when `env_name` is set, refusing to run `command_name`.
pub(super) fn check_disabled(env_name: &str, command_name: &str) -> Result<(), ExitFailure> {
    if std::env::var_os(env_name).is_some() {
        return Err(ExitFailure::new(
            1,
            format!("{command_name} disabled by {env_name}"),
        ));
    }
    Ok(())
}

/// Resolves `target` (or the current pane) to a reference pinned by pane id, immune to later
/// renumbering.
pub(super) fn resolve_pane_ref(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &'static str,
) -> Result<PaneTargetRef, ExitFailure> {
    let slot = PaneTarget::resolve(connection, target, command_name)?;
    stable_pane_ref_for_slot(connection, &slot, command_name)
}

/// Converts a positional pane slot into an id-pinned pane reference.
pub(in crate::cli) fn stable_pane_ref_for_slot(
    connection: &mut Connection,
    slot: &PaneTarget,
    command_name: &'static str,
) -> Result<PaneTargetRef, ExitFailure> {
    let pane_id = pane_id_for_slot(connection, slot, command_name)?;
    Ok(PaneTargetRef::by_id(slot.session_name().clone(), pane_id))
}

/// Fetches the current cell grid of `target`, turning error responses into failures.
pub(super) fn pane_snapshot(
    connection: &mut Connection,
    target: PaneTargetRef,
) -> Result<PaneSnapshotResponse, ExitFailure> {
    match connection
        .pane_snapshot_ref(target)
        .map_err(ExitFailure::from)?
    {
        Response::PaneSnapshot(snapshot) => Ok(snapshot),
        other => Err(response_error(&other, "pane-snapshot", "for pane-snapshot")),
    }
}

/// The failure for a server error answering `command_name`, in tmux's wording.
pub(super) fn command_error(command_name: &str, error: &ErrorResponse) -> ExitFailure {
    ExitFailure::new(1, tmux_cli_error_message(command_name, &error.error))
}

/// The protocol failure for a response that does not fit `context`, such as `for send-keys`.
pub(super) fn protocol_mismatch(response: &Response, context: &str) -> ExitFailure {
    ExitFailure::new(
        1,
        format!(
            "protocol error: unexpected '{}' response {context}",
            response.command_name()
        ),
    )
}

/// The failure for a response `command_name` did not expect: a server error in tmux's wording,
/// anything else as a protocol mismatch described by `context`.
pub(super) fn response_error(
    response: &Response,
    command_name: &str,
    context: &str,
) -> ExitFailure {
    match response {
        Response::Error(error) => command_error(command_name, error),
        other => protocol_mismatch(other, context),
    }
}

/// The human-readable kind word for a resolved `Target`.
pub(super) const fn target_kind_name(target: &Target) -> &'static str {
    match target {
        Target::Session(_) => "session",
        Target::Window(_) => "window",
        Target::Pane(_) => "pane",
    }
}

/// Renders each visible row of `snapshot` as a trailing-space-trimmed string.
pub(super) fn visible_lines(snapshot: &PaneSnapshotResponse) -> Vec<String> {
    let cols = usize::from(snapshot.cols);
    let rows = usize::from(snapshot.rows);
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let start = row.saturating_mul(cols);
        let end = start.saturating_add(cols).min(snapshot.cells.len());
        lines.push(visible_line_from_cells(&snapshot.cells[start..end]));
    }
    lines
}

/// Renders the whole visible grid as newline-joined text.
pub(super) fn visible_text(snapshot: &PaneSnapshotResponse) -> String {
    visible_lines(snapshot).join("\n")
}

/// Joins one row's non-padding cell text into a trimmed line.
pub(super) fn visible_line_from_cells(cells: &[PaneSnapshotCell]) -> String {
    let mut line = String::new();
    for cell in cells {
        if !cell.padding {
            line.push_str(&cell.text);
        }
    }
    trim_trailing_spaces(&mut line);
    line
}

/// Drops trailing spaces from `value` in place.
fn trim_trailing_spaces(value: &mut String) {
    while value.ends_with(' ') {
        value.pop();
    }
}

/// A literal needle found in the pane grid, with its row and column span.
#[derive(Debug, Clone)]
pub(super) struct TextMatch {
    pub(super) row: usize,
    pub(super) col: usize,
    pub(super) end_col: usize,
    pub(super) text: String,
}

/// Finds every literal occurrence of `needle` in the visible grid, with cell coordinates.
pub(super) fn find_visible_text(snapshot: &PaneSnapshotResponse, needle: &str) -> Vec<TextMatch> {
    if needle.is_empty() {
        return Vec::new();
    }

    (0..usize::from(snapshot.rows))
        .flat_map(|row| {
            let rendered = rendered_row(snapshot, row);
            literal_match_ranges(&rendered.text, needle)
                .into_iter()
                .filter_map(move |(start, end)| {
                    let start_coord = rendered.coords.get(start)?;
                    let end_coord = end
                        .checked_sub(1)
                        .and_then(|index| rendered.coords.get(index))?;
                    Some(TextMatch {
                        row,
                        col: start_coord.start_col,
                        end_col: end_coord.end_col,
                        text: rendered.text.get(start..end)?.to_owned(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Encodes text matches as a JSON array of row, column and text objects.
pub(super) fn matches_json(matches: &[TextMatch]) -> Value {
    Value::Array(
        matches
            .iter()
            .map(|found| {
                json!({
                    "row": found.row,
                    "col": found.col,
                    "end_col": found.end_col,
                    "text": found.text,
                })
            })
            .collect(),
    )
}

/// One rendered row plus the pane column span backing each of its text bytes.
#[derive(Debug)]
struct RenderedRow {
    text: String,
    coords: Vec<ByteCoord>,
}

/// The half-open pane column span occupied by a rendered text byte.
#[derive(Debug, Clone, Copy)]
struct ByteCoord {
    start_col: usize,
    end_col: usize,
}

/// Renders row `row` of `snapshot`, recording the column span of every text byte.
fn rendered_row(snapshot: &PaneSnapshotResponse, row: usize) -> RenderedRow {
    let cols = usize::from(snapshot.cols);
    let start = row.saturating_mul(cols);
    let end = start.saturating_add(cols).min(snapshot.cells.len());
    let mut text = String::new();
    let mut coords = Vec::new();
    for (relative_col, cell) in snapshot.cells[start..end].iter().enumerate() {
        if cell.padding {
            continue;
        }
        text.push_str(&cell.text);
        let width = usize::from(cell.width.max(1));
        let cell_end_col = relative_col.saturating_add(width).min(cols);
        coords.extend(cell.text.bytes().map(|_| ByteCoord {
            start_col: relative_col,
            end_col: cell_end_col,
        }));
    }
    trim_trailing_spaces(&mut text);
    coords.truncate(text.len());
    RenderedRow { text, coords }
}

/// Returns the byte ranges of every literal `needle` occurrence, including overlapping ones.
fn literal_match_ranges(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut search_start = 0;
    while search_start <= haystack.len() {
        let Some(relative) = haystack
            .get(search_start..)
            .and_then(|tail| tail.find(needle))
        else {
            break;
        };
        let start = search_start + relative;
        let end = start + needle.len();
        ranges.push((start, end));
        search_start = next_char_boundary_after(haystack, start);
    }
    ranges
}

/// Returns the byte index just past the character starting at `index`.
fn next_char_boundary_after(value: &str, index: usize) -> usize {
    value
        .get(index..)
        .and_then(|tail| tail.chars().next())
        .map_or(value.len() + 1, |character| index + character.len_utf8())
}

/// Writes `value` as one JSON line on stdout and yields the success exit code.
pub(super) fn write_json_line(value: &Value) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|error| ExitFailure::new(1, format!("failed to encode JSON: {error}")))?;
    write_all_stdout(&mut stdout, b"\n")?;
    Ok(0)
}

/// Writes raw bytes to stdout and yields the success exit code.
pub(super) fn write_stdout_bytes(bytes: &[u8]) -> Result<i32, ExitFailure> {
    write_stdout_bytes_or_broken_pipe(bytes)?;
    Ok(0)
}

/// Whether a stdout write landed or the downstream reader had already hung up.
pub(super) enum StdoutWrite {
    Written,
    BrokenPipe,
}

/// Reports whether stdout has hung up, via a non-blocking `poll` status probe.
pub(super) fn stdout_closed() -> bool {
    let mut pollfd = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `pollfd` points to one initialized pollfd entry that remains
    // valid for the duration of the call; timeout 0 makes this a non-blocking
    // status probe of stdout.
    let ready = unsafe { libc::poll(&raw mut pollfd, 1, 0) };
    ready > 0 && pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

/// Writes bytes to stdout, reporting a broken pipe as an outcome rather than an error.
pub(super) fn write_stdout_bytes_or_broken_pipe(bytes: &[u8]) -> Result<StdoutWrite, ExitFailure> {
    write_all_stdout(&mut io::stdout().lock(), bytes)
}

/// Writes `line` and a newline to stdout, yielding the success exit code.
pub(super) fn write_stdout_line(line: &str) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    write_all_stdout(&mut stdout, line.as_bytes())?;
    write_all_stdout(&mut stdout, b"\n")?;
    Ok(0)
}

/// Writes `text` and a newline to stdout, or nothing at all when `text` is empty.
pub(super) fn write_stdout_text(text: &str) -> Result<i32, ExitFailure> {
    if text.is_empty() {
        return write_stdout_bytes(b"");
    }
    write_stdout_line(text)
}

/// Writes `line` and a newline to stderr, ignoring write failures.
pub(super) fn write_stderr_line(line: &str) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

/// Writes all of `bytes` to `stdout`, reporting a broken pipe as an outcome rather than an error.
fn write_all_stdout(stdout: &mut impl Write, bytes: &[u8]) -> Result<StdoutWrite, ExitFailure> {
    match stdout.write_all(bytes) {
        Ok(()) => Ok(StdoutWrite::Written),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(StdoutWrite::BrokenPipe),
        Err(error) => Err(ExitFailure::new(
            1,
            format!("failed to write stdout: {error}"),
        )),
    }
}

/// Computes the instant at which `timeout`, or the default, expires.
pub(super) fn timeout_deadline(timeout: Option<Duration>) -> Instant {
    Instant::now() + timeout.unwrap_or(DEFAULT_TIMEOUT)
}

/// Saturates `duration` to whole milliseconds.
pub(super) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds elapsed since `started_at`.
pub(super) fn elapsed_millis(started_at: Instant) -> u64 {
    duration_millis(started_at.elapsed())
}

/// Sleeps one automation poll interval before the next probe.
pub(super) fn sleep_poll_interval() {
    std::thread::sleep(POLL_INTERVAL);
}

/// Whether a pane's process is still running or has already exited.
pub(in crate::cli) enum PaneProcessState {
    Alive,
    Exited(PaneExitStatus),
}

/// Reports the liveness of the pane behind `target`, for slot and id references alike.
pub(in crate::cli) fn pane_process_state(
    connection: &mut Connection,
    target: &PaneTargetRef,
) -> Result<PaneProcessState, ExitFailure> {
    match target {
        PaneTargetRef::Slot(slot) => pane_process_state_for_slot(connection, slot),
        PaneTargetRef::Id {
            session_name,
            pane_id,
        } => pane_process_state_for_id(connection, session_name, *pane_id),
    }
}

/// Lists the panes of one window of `session_name` (or all its windows) rendered with `format`,
/// handing a server error back for the caller to judge and failing any other mismatch as a
/// protocol error described by `context`.
pub(super) fn list_panes_output(
    connection: &mut Connection,
    session_name: SessionName,
    window_index: Option<u32>,
    format: String,
    context: &str,
) -> Result<Result<CommandOutput, ErrorResponse>, ExitFailure> {
    match connection
        .list_panes_in_window(session_name, window_index, Some(format))
        .map_err(ExitFailure::from)?
    {
        Response::ListPanes(response) => Ok(Ok(response.output)),
        Response::Error(error) => Ok(Err(error)),
        other => Err(protocol_mismatch(&other, context)),
    }
}

/// The fields after the leading pane index pair on the first listed row naming `target`'s slot.
pub(super) fn slot_row_fields<'a>(
    listing: &'a str,
    target: &PaneTarget,
) -> Option<impl Iterator<Item = &'a str>> {
    listing
        .lines()
        .map(|line| line.split('\t'))
        .find_map(|mut fields| {
            listed_pane_index_matches_target(
                target,
                fields.next().unwrap_or_default(),
                fields.next().unwrap_or_default(),
            )
            .then_some(fields)
        })
}

/// Looks up the stable pane id currently occupying the positional slot `target`.
fn pane_id_for_slot(
    connection: &mut Connection,
    target: &PaneTarget,
    command_name: &'static str,
) -> Result<PaneId, ExitFailure> {
    let output = list_panes_output(
        connection,
        target.session_name().clone(),
        Some(target.window_index()),
        "#{pane_index}\t#{pane-base-index}\t#{pane_id}\n".to_owned(),
        "while resolving pane id",
    )?
    .map_err(|error| command_error(command_name, &error))?;
    let listing = String::from_utf8_lossy(output.stdout());
    slot_row_fields(&listing, target)
        .and_then(|mut fields| fields.next().and_then(parse_pane_id))
        .ok_or_else(|| {
            ExitFailure::new(1, format!("unable to resolve pane id for target {target}"))
        })
}

/// Reads pane liveness from a positional slot listing, treating a vanished pane as exited.
fn pane_process_state_for_slot(
    connection: &mut Connection,
    target: &PaneTarget,
) -> Result<PaneProcessState, ExitFailure> {
    let Ok(output) = list_panes_output(
        connection,
        target.session_name().clone(),
        Some(target.window_index()),
        "#{pane_index}\t#{pane-base-index}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_dead_signal}\n"
            .to_owned(),
        "for pane process state",
    )?
    else {
        return Ok(PaneProcessState::Exited(PaneExitStatus::stale()));
    };
    let listing = String::from_utf8_lossy(output.stdout());
    Ok(slot_row_fields(&listing, target).map_or_else(
        || PaneProcessState::Exited(PaneExitStatus::stale()),
        listed_process_state,
    ))
}

/// Reads pane liveness by pane id, treating a vanished pane as exited.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`#{pane_id}` and friends are tmux format placeholders sent over the wire, not Rust format arguments"
)]
fn pane_process_state_for_id(
    connection: &mut Connection,
    session_name: &SessionName,
    pane_id: PaneId,
) -> Result<PaneProcessState, ExitFailure> {
    let Ok(output) = list_panes_output(
        connection,
        session_name.clone(),
        None,
        "#{pane_id}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_dead_signal}\n".to_owned(),
        "for pane process state",
    )?
    else {
        return Ok(PaneProcessState::Exited(PaneExitStatus::stale()));
    };
    for line in String::from_utf8_lossy(output.stdout()).lines() {
        let mut fields = line.split('\t');
        if fields.next().and_then(parse_pane_id) == Some(pane_id) {
            return Ok(listed_process_state(fields));
        }
    }
    Ok(PaneProcessState::Exited(PaneExitStatus::stale()))
}

/// Reads a listed pane's `dead` flag, exit status and exit signal fields into its state.
fn listed_process_state<'a>(mut fields: impl Iterator<Item = &'a str>) -> PaneProcessState {
    if fields.next() != Some("1") {
        return PaneProcessState::Alive;
    }
    PaneProcessState::Exited(PaneExitStatus::known(
        parse_i32_field(fields.next()),
        parse_i32_field(fields.next()),
    ))
}

/// Parses a `%`-prefixed pane id such as `%3`, returning `None` for malformed input.
pub(super) fn parse_pane_id(value: &str) -> Option<PaneId> {
    value
        .strip_prefix('%')?
        .parse::<u32>()
        .ok()
        .map(PaneId::new)
}

/// Parses a present, non-empty tab-separated field as an `i32`.
pub(super) fn parse_i32_field(value: Option<&str>) -> Option<i32> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok())
}

/// Snapshot fixtures shared by the automation unit tests.
#[cfg(test)]
pub(super) mod fixtures {
    use rmux_proto::{PaneSnapshotCell, PaneSnapshotCursor, PaneSnapshotResponse};

    /// One cell `width` columns wide, or the padding cell that trails a wide glyph.
    fn cell(text: &str, width: u8, padding: bool) -> PaneSnapshotCell {
        PaneSnapshotCell {
            text: text.to_owned(),
            width,
            padding,
            attributes: 0,
            fg: 0,
            bg: 0,
            us: 0,
            link: 0,
        }
    }

    /// A one-row, four-column grid holding `A`, a wide `界` plus its padding cell, then `B`.
    pub(in crate::cli::automation) fn wide_glyph_snapshot() -> PaneSnapshotResponse {
        PaneSnapshotResponse {
            cols: 4,
            rows: 1,
            cells: vec![
                cell("A", 1, false),
                cell("界", 2, false),
                cell(" ", 0, true),
                cell("B", 1, false),
            ],
            cursor: PaneSnapshotCursor {
                row: 0,
                col: 0,
                visible: false,
                style: 0,
            },
            revision: 1,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::fixtures::wide_glyph_snapshot;
    use super::{find_visible_text, visible_lines};

    #[test]
    fn visible_text_coordinates_use_terminal_columns_for_wide_cells() {
        let snapshot = wide_glyph_snapshot();

        assert_eq!(visible_lines(&snapshot), vec!["A界B"]);
        let matches = find_visible_text(&snapshot, "界B");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 0);
        assert_eq!(matches[0].col, 1);
        assert_eq!(matches[0].end_col, 4);
        assert_eq!(matches[0].text, "界B");
    }
}
