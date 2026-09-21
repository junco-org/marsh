use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use rmux_client::{connect, Connection};
use rmux_proto::{
    PaneId, PaneSnapshotCell, PaneSnapshotResponse, PaneTarget, PaneTargetRef, Response,
};
use serde_json::{json, Value};

use crate::cli_args::TargetSpec;
use crate::cli_response::tmux_cli_error_message;

use super::super::{listed_pane_index_matches_target, resolve_pane_target_or_current, ExitFailure};
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

/// Connects to the server socket, mapping client errors into a CLI `ExitFailure`.
pub(super) fn connect_cli(socket_path: &Path) -> Result<Connection, ExitFailure> {
    connect(socket_path).map_err(|error| ExitFailure::from_client_connect(socket_path, error))
}

/// Resolves the positional pane slot for `target`, defaulting to the current pane.
pub(super) fn resolve_pane_slot(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &'static str,
) -> Result<PaneTarget, ExitFailure> {
    resolve_pane_target_or_current(connection, target, command_name)
}

/// Resolves `target` to a pane reference pinned by pane id, immune to later renumbering.
pub(super) fn resolve_stable_pane_target(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &'static str,
) -> Result<PaneTargetRef, ExitFailure> {
    let slot = resolve_pane_slot(connection, target, command_name)?;
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

/// Resolves `target` to a stable pane reference for automation commands.
pub(super) fn resolve_pane_ref(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &'static str,
) -> Result<PaneTargetRef, ExitFailure> {
    resolve_stable_pane_target(connection, target, command_name)
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
        Response::Error(error) => Err(ExitFailure::new(
            1,
            tmux_cli_error_message("pane-snapshot", &error.error),
        )),
        other => Err(ExitFailure::new(
            1,
            format!(
                "protocol error: unexpected '{}' response for pane-snapshot",
                other.command_name()
            ),
        )),
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
        let Some(relative) = haystack.get(search_start..).and_then(|tail| tail.find(needle)) else {
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
pub(super) fn write_json(value: &Value) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|error| ExitFailure::new(1, format!("failed to encode JSON: {error}")))?;
    write_all_stdout(&mut stdout, b"\n")?;
    Ok(0)
}

/// Writes raw bytes to stdout and yields the success exit code.
pub(super) fn write_stdout_bytes(bytes: &[u8]) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    write_all_stdout(&mut stdout, bytes)?;
    Ok(0)
}

/// Whether a stdout write landed or the downstream reader had already hung up.
pub(super) enum StdoutWrite {
    Written,
    BrokenPipe,
}

/// Reports whether stdout has hung up, via a non-blocking `poll` status probe.
#[cfg(unix)]
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

/// Reports whether stdout has hung up on non-Unix platforms.
#[cfg(not(unix))]
pub(super) fn stdout_closed() -> bool {
    stdout_closed_impl()
}

/// Detects a broken Windows stdout pipe via a zero-byte `WriteFile` probe.
#[cfg(windows)]
fn stdout_closed_impl() -> bool {
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, WriteFile, FILE_TYPE_PIPE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::Pipes::{GetNamedPipeHandleStateW, PIPE_READMODE_MESSAGE};

    // Windows does not expose poll(POLLOUT|POLLHUP) for anonymous stdout pipes.
    // Anonymous shell pipes are byte-mode named pipes under the hood. Query the
    // read mode first so the broken-pipe probe never sends a zero-byte message
    // into a message-mode downstream reader.
    // SAFETY: GetStdHandle reads the process stdout pseudo-handle and does not
    // require ownership transfer from Rust.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return true;
    }
    // SAFETY: `handle` was returned by GetStdHandle and is only queried.
    if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
        return false;
    }

    let mut state = 0;
    // SAFETY: `handle` is a pipe handle owned by the process stdout table.
    // `state` is a valid out pointer, the other optional outputs are null, and
    // no ownership transfer occurs.
    let state_ok = unsafe {
        GetNamedPipeHandleStateW(
            handle,
            &mut state,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if state_ok == 0 {
        let error = {
            // SAFETY: GetLastError reads the calling thread's last-error slot
            // set by the immediately preceding GetNamedPipeHandleStateW call.
            unsafe { GetLastError() }
        };
        if matches!(
            error,
            ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
        ) {
            return true;
        }
    } else if state & PIPE_READMODE_MESSAGE != 0 {
        return false;
    }

    let byte = 0u8;
    let mut written = 0u32;
    // SAFETY: `handle` is the process stdout pipe; the buffer pointer is valid,
    // and the byte count is zero so no memory is read from it. On byte-mode
    // pipes this does not enqueue payload for the downstream reader, but it
    // still reports broken-pipe state.
    let ok = unsafe {
        WriteFile(
            handle,
            &byte as *const u8,
            0,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return false;
    }
    matches!(
        // SAFETY: GetLastError reads the calling thread's last-error slot set
        // by the immediately preceding WriteFile call.
        unsafe { GetLastError() },
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
    )
}

/// Assumes stdout stays open on platforms with no hangup probe.
#[cfg(not(any(unix, windows)))]
fn stdout_closed_impl() -> bool {
    false
}

/// Writes bytes to stdout, reporting a broken pipe as an outcome rather than an error.
pub(super) fn write_stdout_bytes_or_broken_pipe(bytes: &[u8]) -> Result<StdoutWrite, ExitFailure> {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(bytes) {
        Ok(()) => Ok(StdoutWrite::Written),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(StdoutWrite::BrokenPipe),
        Err(error) => Err(ExitFailure::new(
            1,
            format!("failed to write stdout: {error}"),
        )),
    }
}

/// Writes `line` and a newline to stdout, yielding the success exit code.
pub(super) fn write_stdout_line(line: &str) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    write_all_stdout(&mut stdout, line.as_bytes())?;
    write_all_stdout(&mut stdout, b"\n")?;
    Ok(0)
}

/// Writes `line` and a newline to stderr, ignoring write failures.
pub(super) fn write_stderr_line(line: &str) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

/// Writes all of `bytes` to `stdout`, treating a broken pipe as success.
fn write_all_stdout(stdout: &mut impl Write, bytes: &[u8]) -> Result<(), ExitFailure> {
    match stdout.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
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

/// Looks up the stable pane id currently occupying the positional slot `target`.
fn pane_id_for_slot(
    connection: &mut Connection,
    target: &PaneTarget,
    command_name: &'static str,
) -> Result<PaneId, ExitFailure> {
    let response = connection
        .list_panes_in_window(
            target.session_name().clone(),
            Some(target.window_index()),
            Some("#{pane_index}\t#{pane-base-index}\t#{pane_id}\n".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = match response {
        Response::ListPanes(response) => response.output,
        Response::Error(error) => {
            return Err(ExitFailure::new(
                1,
                tmux_cli_error_message(command_name, &error.error),
            ));
        }
        other => {
            return Err(ExitFailure::new(
                1,
                format!(
                    "protocol error: unexpected '{}' response while resolving pane id",
                    other.command_name()
                ),
            ));
        }
    };
    for line in String::from_utf8_lossy(output.stdout()).lines() {
        let mut fields = line.split('\t');
        if !listed_pane_index_matches_target(
            target,
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
        ) {
            continue;
        }
        if let Some(pane_id) = fields.next().and_then(parse_pane_id) {
            return Ok(pane_id);
        }
        break;
    }
    Err(ExitFailure::new(
        1,
        format!("unable to resolve pane id for target {target}"),
    ))
}

/// Reads pane liveness from a positional slot listing, treating a vanished pane as exited.
fn pane_process_state_for_slot(
    connection: &mut Connection,
    target: &PaneTarget,
) -> Result<PaneProcessState, ExitFailure> {
    let response = connection
        .list_panes_in_window(
            target.session_name().clone(),
            Some(target.window_index()),
            Some(
                "#{pane_index}\t#{pane-base-index}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_dead_signal}\n"
                    .to_owned(),
            ),
        )
        .map_err(ExitFailure::from)?;
    let output = match response {
        Response::ListPanes(response) => response.output,
        Response::Error(_) => {
            return Ok(PaneProcessState::Exited(PaneExitStatus::stale()));
        }
        other => {
            return Err(ExitFailure::new(
                1,
                format!(
                    "protocol error: unexpected '{}' response for pane process state",
                    other.command_name()
                ),
            ));
        }
    };
    let text = String::from_utf8_lossy(output.stdout());
    for line in text.lines() {
        let mut fields = line.split('\t');
        if !listed_pane_index_matches_target(
            target,
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
        ) {
            continue;
        }
        if fields.next() == Some("1") {
            return Ok(PaneProcessState::Exited(PaneExitStatus::known(
                parse_i32_field(fields.next()),
                parse_i32_field(fields.next()),
            )));
        }
        return Ok(PaneProcessState::Alive);
    }
    Ok(PaneProcessState::Exited(PaneExitStatus::stale()))
}

/// Reads pane liveness by pane id, treating a vanished pane as exited.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`#{pane_id}` and friends are tmux format placeholders sent over the wire, not Rust format arguments"
)]
fn pane_process_state_for_id(
    connection: &mut Connection,
    session_name: &rmux_proto::SessionName,
    pane_id: PaneId,
) -> Result<PaneProcessState, ExitFailure> {
    let response = connection
        .list_panes_in_window(
            session_name.clone(),
            None,
            Some("#{pane_id}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_dead_signal}\n".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = match response {
        Response::ListPanes(response) => response.output,
        Response::Error(_) => {
            return Ok(PaneProcessState::Exited(PaneExitStatus::stale()));
        }
        other => {
            return Err(ExitFailure::new(
                1,
                format!(
                    "protocol error: unexpected '{}' response for pane process state",
                    other.command_name()
                ),
            ));
        }
    };
    let text = String::from_utf8_lossy(output.stdout());
    for line in text.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.first().and_then(|value| parse_pane_id(value)) != Some(pane_id) {
            continue;
        }
        if fields.get(1).copied() == Some("1") {
            return Ok(PaneProcessState::Exited(PaneExitStatus::known(
                parse_i32_field(fields.get(2).copied()),
                parse_i32_field(fields.get(3).copied()),
            )));
        }
        return Ok(PaneProcessState::Alive);
    }
    Ok(PaneProcessState::Exited(PaneExitStatus::stale()))
}

/// Parses a `%`-prefixed pane id, returning `None` for malformed input.
fn parse_pane_id(value: &str) -> Option<PaneId> {
    value
        .strip_prefix('%')?
        .parse::<u32>()
        .ok()
        .map(PaneId::new)
}

/// Parses a present, non-empty tab-separated field as an `i32`.
fn parse_i32_field(value: Option<&str>) -> Option<i32> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use rmux_proto::{PaneSnapshotCell, PaneSnapshotCursor, PaneSnapshotResponse};

    use super::{find_visible_text, visible_lines};

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

    fn snapshot(cells: Vec<PaneSnapshotCell>) -> PaneSnapshotResponse {
        PaneSnapshotResponse {
            cols: 4,
            rows: 1,
            cells,
            cursor: PaneSnapshotCursor {
                row: 0,
                col: 0,
                visible: false,
                style: 0,
            },
            revision: 1,
        }
    }

    #[test]
    fn visible_text_coordinates_use_terminal_columns_for_wide_cells() {
        let snapshot = snapshot(vec![
            cell("A", 1, false),
            cell("界", 2, false),
            cell(" ", 0, true),
            cell("B", 1, false),
        ]);

        assert_eq!(visible_lines(&snapshot), vec!["A界B"]);
        let matches = find_visible_text(&snapshot, "界B");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 0);
        assert_eq!(matches[0].col, 1);
        assert_eq!(matches[0].end_col, 4);
        assert_eq!(matches[0].text, "界B");
    }
}
