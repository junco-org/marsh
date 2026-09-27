use std::path::Path;

use rmux_proto::{PaneOutputSubscriptionId, PaneOutputSubscriptionStart, Response};
use serde_json::json;

use crate::cli_args::{CollectPaneOutputArgs, StreamPaneArgs};

use super::super::ExitFailure;
use super::super::target_resolution::connect_cli;
use super::common::{
    PaneProcessState, SCHEMA_VERSION, StdoutWrite, check_disabled, pane_process_state,
    pane_snapshot, resolve_pane_ref, response_error, sleep_poll_interval, stdout_closed,
    visible_lines, visible_text, write_json_line, write_stderr_line, write_stdout_bytes,
    write_stdout_bytes_or_broken_pipe,
};

/// Maximum pane output events requested per `pane_output_cursor` poll.
const CURSOR_BATCH_EVENTS: u16 = 128;
/// Byte ceiling after which a line-mode buffer is force-flushed without a newline.
const LINE_BUFFER_MAX: usize = 1_048_576;

/// Runs `stream-pane`, forwarding a pane's live output to stdout as raw bytes or whole lines.
pub(crate) fn run_stream_pane(
    args: &StreamPaneArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    check_disabled("RMUX_DISABLE_STREAM_PANE", "stream-pane")?;
    let mut connection = connect_cli(socket_path)?;
    let target = resolve_pane_ref(&mut connection, args.target.as_ref(), "stream-pane")?;
    let subscription_id = subscribe(
        &mut connection,
        target.clone(),
        PaneOutputSubscriptionStart::Oldest,
    )?;
    let line_mode = args.lines && !args.raw;
    let mut line_buffer = Vec::new();
    let mut line_buffer_force_flushed = false;
    let mut wrote_stdout = false;
    'stream: while !stdout_closed() {
        let batch = poll_output(&mut connection, subscription_id, "stream-pane", true)?;
        if batch.lag.is_some() {
            line_buffer.clear();
            line_buffer_force_flushed = false;
            if !wrote_stdout {
                match write_lag_snapshot_seed(&mut connection, target.clone(), line_mode)? {
                    LagSnapshotSeed::Written => wrote_stdout = true,
                    LagSnapshotSeed::BrokenPipe => break,
                    LagSnapshotSeed::Empty => {}
                }
            }
        }
        for bytes in batch.chunks {
            if line_mode {
                if write_lines(&mut line_buffer, &mut line_buffer_force_flushed, &bytes)? {
                    break 'stream;
                }
                wrote_stdout |= bytes.contains(&b'\n');
            } else {
                if matches!(
                    write_stdout_bytes_or_broken_pipe(&bytes)?,
                    StdoutWrite::BrokenPipe
                ) {
                    break 'stream;
                }
                wrote_stdout = true;
            }
        }
        if batch.saw_eof {
            if line_mode {
                flush_line_buffer(&mut line_buffer)?;
            }
            break;
        }
        sleep_poll_interval();
    }
    // Stdout hung up, a write hit a broken pipe, or the pane's output ended.
    let _ = connection.unsubscribe_pane_output(subscription_id);
    Ok(0)
}

/// Outcome of seeding stdout from a pane snapshot after a lag gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagSnapshotSeed {
    Empty,
    Written,
    BrokenPipe,
}

/// Writes the pane's current visible content once, so lagged output starts from a known state.
fn write_lag_snapshot_seed(
    connection: &mut rmux_client::Connection,
    target: rmux_proto::PaneTargetRef,
    line_mode: bool,
) -> Result<LagSnapshotSeed, ExitFailure> {
    let snapshot = pane_snapshot(connection, target)?;
    if line_mode {
        let mut wrote = false;
        for line in visible_lines(&snapshot) {
            if line.is_empty() {
                continue;
            }
            if matches!(write_line(line.into_bytes())?, StdoutWrite::BrokenPipe) {
                return Ok(LagSnapshotSeed::BrokenPipe);
            }
            wrote = true;
        }
        return Ok(if wrote {
            LagSnapshotSeed::Written
        } else {
            LagSnapshotSeed::Empty
        });
    }

    let seed = visible_text(&snapshot);
    if seed.is_empty() {
        return Ok(LagSnapshotSeed::Empty);
    }
    if matches!(
        write_stdout_bytes_or_broken_pipe(seed.as_bytes())?,
        StdoutWrite::BrokenPipe
    ) {
        return Ok(LagSnapshotSeed::BrokenPipe);
    }
    Ok(LagSnapshotSeed::Written)
}

/// Runs `collect-pane-output`, accumulating a pane's output until both EOF and process exit.
pub(crate) fn run_collect_pane_output(
    args: &CollectPaneOutputArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    check_disabled("RMUX_DISABLE_STREAM_PANE", "collect-pane-output")?;
    let mut connection = connect_cli(socket_path)?;
    let target_ref =
        resolve_pane_ref(&mut connection, args.target.as_ref(), "collect-pane-output")?;
    let subscription_id = subscribe(
        &mut connection,
        target_ref.clone(),
        PaneOutputSubscriptionStart::Oldest,
    )?;
    let mut output = Vec::new();
    let mut total_bytes: usize = 0;
    let mut truncated = false;
    let mut pane_exit = None;
    let mut saw_eof = false;
    let mut missed_events = 0_u64;
    let pane_exit = loop {
        let batch = poll_output(
            &mut connection,
            subscription_id,
            "collect-pane-output",
            true,
        )?;
        saw_eof |= batch.saw_eof;
        if let Some(lag) = batch.lag {
            missed_events = missed_events.saturating_add(lag.missed_events);
        }
        for bytes in batch.chunks {
            total_bytes = total_bytes.saturating_add(bytes.len());
            if output.len() < args.max_bytes {
                let remaining = args.max_bytes - output.len();
                let keep = bytes.len().min(remaining);
                output.extend_from_slice(&bytes[..keep]);
                truncated |= keep < bytes.len();
            } else {
                truncated = true;
            }
        }
        if pane_exit.is_none() {
            if let PaneProcessState::Exited(value) =
                pane_process_state(&mut connection, &target_ref)?
            {
                pane_exit = Some(value);
            }
        }
        if saw_eof {
            if let Some(value) = pane_exit.take() {
                break value;
            }
        }
        sleep_poll_interval();
    };
    let _ = connection.unsubscribe_pane_output(subscription_id);
    if missed_events > 0 {
        if args.json {
            return write_json_line(&json!({
                "schema_version": SCHEMA_VERSION,
                "ok": false,
                "error": "pane-output-lag",
                "bytes": total_bytes,
                "stored_bytes": output.len(),
                "truncated": truncated,
                "missed_events": missed_events,
                "pane_exit": pane_exit.json_value(),
            }))
            .map(|_| 1);
        }
        return Err(ExitFailure::new(
            1,
            format!(
                "collect-pane-output lost pane output due to lag; missed {missed_events} events"
            ),
        ));
    }
    if args.json {
        return write_json_line(&json!({
            "schema_version": SCHEMA_VERSION,
            "ok": true,
            "bytes": total_bytes,
            "stored_bytes": output.len(),
            "truncated": truncated,
            "output_utf8_lossy": String::from_utf8_lossy(&output),
            "pane_exit": pane_exit.json_value(),
        }));
    }
    write_stdout_bytes(&output)
}

/// Count of pane output events dropped before the subscriber could read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OutputLag {
    pub(super) missed_events: u64,
}

/// One poll's worth of pane output: byte chunks plus end-of-stream and lag markers.
pub(super) struct OutputBatch {
    pub(super) chunks: Vec<Vec<u8>>,
    pub(super) saw_eof: bool,
    pub(super) lag: Option<OutputLag>,
}

/// Opens a pane output subscription, translating protocol errors into CLI exit failures.
pub(super) fn subscribe(
    connection: &mut rmux_client::Connection,
    target: rmux_proto::PaneTargetRef,
    start: PaneOutputSubscriptionStart,
) -> Result<PaneOutputSubscriptionId, ExitFailure> {
    match connection
        .subscribe_pane_output_ref(target, start)
        .map_err(ExitFailure::from)?
    {
        Response::SubscribePaneOutput(response) => Ok(response.subscription_id),
        other => Err(response_error(&other, "stream-pane", "for stream-pane")),
    }
}

/// Polls the pane output cursor, splitting events into chunks, EOF, and lag, and reporting a
/// lag gap on stderr when `report_lag` is set.
pub(super) fn poll_output(
    connection: &mut rmux_client::Connection,
    subscription_id: PaneOutputSubscriptionId,
    command_name: &'static str,
    report_lag: bool,
) -> Result<OutputBatch, ExitFailure> {
    match connection
        .pane_output_cursor(subscription_id, Some(CURSOR_BATCH_EVENTS))
        .map_err(ExitFailure::from)?
    {
        Response::PaneOutputCursor(response) => {
            let mut saw_eof = false;
            let chunks = response
                .events
                .into_iter()
                .filter_map(|event| {
                    if event.bytes.is_empty() {
                        saw_eof = true;
                        None
                    } else {
                        Some(event.bytes)
                    }
                })
                .collect();
            Ok(OutputBatch {
                chunks,
                saw_eof,
                lag: None,
            })
        }
        Response::PaneOutputLag(response) => {
            if report_lag {
                write_stderr_line(&format!(
                    "{command_name}: pane output lagged; missed {} events",
                    response.lag.missed_events
                ));
            }
            Ok(OutputBatch {
                chunks: Vec::new(),
                saw_eof: false,
                lag: Some(OutputLag {
                    missed_events: response.lag.missed_events,
                }),
            })
        }
        other => Err(response_error(
            &other,
            command_name,
            &format!("for {command_name}"),
        )),
    }
}

/// Buffers incoming bytes into lines and writes each completed line, signalling a broken pipe.
fn write_lines(
    buffer: &mut Vec<u8>,
    force_flushed: &mut bool,
    bytes: &[u8],
) -> Result<bool, ExitFailure> {
    let mut broken_pipe = false;
    split_lines_bounded(buffer, force_flushed, bytes, |line| {
        broken_pipe |= matches!(write_line(line)?, StdoutWrite::BrokenPipe);
        Ok(())
    })?;
    Ok(broken_pipe)
}

/// Writes any trailing partial line left in the buffer.
fn flush_line_buffer(buffer: &mut Vec<u8>) -> Result<(), ExitFailure> {
    flush_line_buffer_into(buffer, |line| write_line(line).map(drop))
}

/// Hands the buffer's trailing partial line to `emit`, leaving the buffer empty.
fn flush_line_buffer_into<F>(buffer: &mut Vec<u8>, mut emit: F) -> Result<(), ExitFailure>
where
    F: FnMut(Vec<u8>) -> Result<(), ExitFailure>,
{
    if buffer.is_empty() {
        return Ok(());
    }
    emit(std::mem::take(buffer))
}

/// Writes one line as lossy `UTF-8` with its trailing `\r` stripped and a newline appended.
fn write_line(mut line: Vec<u8>) -> Result<StdoutWrite, ExitFailure> {
    if line.ends_with(b"\r") {
        line.pop();
    }
    let text = String::from_utf8_lossy(&line);
    if matches!(
        write_stdout_bytes_or_broken_pipe(text.as_bytes())?,
        StdoutWrite::BrokenPipe
    ) {
        return Ok(StdoutWrite::BrokenPipe);
    }
    write_stdout_bytes_or_broken_pipe(b"\n")
}

/// Splits bytes on newlines into `emit` calls, force-flushing lines longer than `LINE_BUFFER_MAX`.
fn split_lines_bounded<F>(
    buffer: &mut Vec<u8>,
    force_flushed: &mut bool,
    bytes: &[u8],
    mut emit: F,
) -> Result<(), ExitFailure>
where
    F: FnMut(Vec<u8>) -> Result<(), ExitFailure>,
{
    for byte in bytes {
        if *byte == b'\n' {
            if buffer.is_empty() && *force_flushed {
                *force_flushed = false;
                continue;
            }
            emit(std::mem::take(buffer))?;
            *force_flushed = false;
        } else {
            buffer.push(*byte);
            *force_flushed = false;
            if buffer.len() >= LINE_BUFFER_MAX {
                emit(std::mem::take(buffer))?;
                *force_flushed = true;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{LINE_BUFFER_MAX, flush_line_buffer_into, split_lines_bounded};

    #[test]
    fn line_stream_buffer_is_force_flushed_at_fixed_limit() {
        let mut buffer = Vec::new();
        let mut force_flushed = false;
        let bytes = vec![b'a'; LINE_BUFFER_MAX + 5];
        let mut lines = Vec::new();

        split_lines_bounded(&mut buffer, &mut force_flushed, &bytes, |line| {
            lines.push(line);
            Ok(())
        })
        .unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), LINE_BUFFER_MAX);
        assert_eq!(buffer.len(), 5);
    }

    #[test]
    fn line_stream_skips_newline_after_force_flush() {
        let mut buffer = Vec::new();
        let mut force_flushed = false;
        let bytes = vec![b'a'; LINE_BUFFER_MAX];
        let mut lines = Vec::new();

        split_lines_bounded(&mut buffer, &mut force_flushed, &bytes, |line| {
            lines.push(line);
            Ok(())
        })
        .unwrap();
        split_lines_bounded(&mut buffer, &mut force_flushed, b"\n", |line| {
            lines.push(line);
            Ok(())
        })
        .unwrap();

        assert_eq!(lines.len(), 1);
        assert!(buffer.is_empty());
        assert!(!force_flushed);
    }

    #[test]
    fn line_stream_flushes_final_partial_line_on_eof() {
        let mut buffer = b"FINAL".to_vec();
        let mut lines = Vec::new();

        flush_line_buffer_into(&mut buffer, |line| {
            lines.push(line);
            Ok(())
        })
        .unwrap();

        assert_eq!(lines, vec![b"FINAL".to_vec()]);
        assert!(buffer.is_empty());
    }
}
