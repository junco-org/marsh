use std::cell::RefCell;
use std::path::{Path, PathBuf};

use rmux_client::{ClientError, Connection, connect};
use rmux_proto::{
    CAPABILITY_CLI_RUNTIME_COMMAND_EXPANSION, CommandOutput,
    INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH, INTERNAL_LIST_WINDOWS_ALL_EXECUTION_PATH,
    PaneTarget, ResolveTargetType, Response, RmuxError, Target,
    encode_internal_runtime_command_arguments,
};

use crate::cli_response::{expect_command_output, expect_command_success};

use super::ExitFailure;
use super::aux_command::write_stdout;

thread_local! {
    /// Per-thread reusable connection for a batch of CLI commands over one socket.
    static COMMAND_CONNECTION_CACHE: RefCell<Option<CommandConnectionCache>> =
        const { RefCell::new(None) };
}

/// A socket path bound to a lazily opened connection reused across CLI commands.
struct CommandConnectionCache {
    socket_path: PathBuf,
    connection: Option<Connection>,
}

/// Drop guard restoring the thread-local command connection cache it displaced.
struct CommandConnectionCacheReset {
    previous: Option<CommandConnectionCache>,
}

impl Drop for CommandConnectionCacheReset {
    /// Puts the previously installed cache back, dropping the batch connection.
    fn drop(&mut self) {
        let previous = self.previous.take();
        COMMAND_CONNECTION_CACHE.with(|cache| {
            let _ = cache.replace(previous);
        });
    }
}

/// Runs `run` with a fresh connection cache bound to `socket_path`, restoring the old one after.
pub(super) fn with_command_connection_cache<R>(socket_path: &Path, run: impl FnOnce() -> R) -> R {
    let previous = COMMAND_CONNECTION_CACHE.with(|cache| {
        cache.replace(Some(CommandConnectionCache {
            socket_path: socket_path.to_path_buf(),
            connection: None,
        }))
    });
    let _reset = CommandConnectionCacheReset { previous };
    run()
}

/// Sends one command over a cached or fresh connection and reports its success exit code.
pub(crate) fn run_command<F>(
    socket_path: &Path,
    command_name: &'static str,
    send: F,
) -> Result<i32, ExitFailure>
where
    F: FnOnce(&mut Connection) -> Result<Response, ClientError>,
{
    let response = with_command_connection(socket_path, |connection| {
        send(connection).map_err(ExitFailure::from)
    })?;
    finish_command_success(response, command_name)
}

/// Whether the CLI may resolve targets itself, unless `RMUX_DISABLE_CLI_TARGET_ACTIONS` is set.
pub(crate) fn cli_target_actions_enabled() -> bool {
    std::env::var_os("RMUX_DISABLE_CLI_TARGET_ACTIONS").is_none()
}

/// Whether the server rejected the target-aware request and the untargeted form must be retried.
pub(crate) const fn target_action_needs_legacy_retry(
    response: &Result<Response, ClientError>,
) -> bool {
    matches!(
        response,
        Ok(Response::Error(error)) if matches!(error.error, RmuxError::Decode(_))
    )
}

/// Like `target_action_needs_legacy_retry`, but also retries a capture that hit an unexpected EOF.
pub(crate) const fn capture_target_action_needs_legacy_retry(
    response: &Result<Response, ClientError>,
) -> bool {
    target_action_needs_legacy_retry(response)
        || matches!(response, Err(ClientError::UnexpectedEof))
}

/// Sends one command and writes the response's command output to stdout instead of checking it.
pub(crate) fn run_payload_command<F>(
    socket_path: &Path,
    command_name: &'static str,
    send: F,
) -> Result<i32, ExitFailure>
where
    F: FnOnce(&mut Connection) -> Result<Response, ClientError>,
{
    let response = with_command_connection(socket_path, |connection| {
        send(connection).map_err(ExitFailure::from)
    })?;
    let output = expect_command_output(&response, command_name)?;
    write_command_output(output)?;
    Ok(0)
}

/// Variant of `run_command` whose sender already maps client errors to an `ExitFailure`.
pub(crate) fn run_command_resolved<F>(
    socket_path: &Path,
    command_name: &'static str,
    send: F,
) -> Result<i32, ExitFailure>
where
    F: FnOnce(&mut Connection) -> Result<Response, ExitFailure>,
{
    let response = with_command_connection(socket_path, send)?;
    finish_command_success(response, command_name)
}

/// Variant of `run_payload_command` whose sender already maps client errors to an `ExitFailure`.
pub(crate) fn run_payload_command_resolved<F>(
    socket_path: &Path,
    command_name: &'static str,
    send: F,
) -> Result<i32, ExitFailure>
where
    F: FnOnce(&mut Connection) -> Result<Response, ExitFailure>,
{
    let response = with_command_connection(socket_path, send)?;
    let output = expect_command_output(&response, command_name)?;
    write_command_output(output)?;
    Ok(0)
}

/// Runs a tmux-style command by queueing it on the server over a new or cached connection.
pub(super) fn run_queued_server_command(
    socket_path: &Path,
    command_name: &'static str,
    queue_command: String,
) -> Result<i32, ExitFailure> {
    let response = with_command_connection(socket_path, |connection| {
        queued_server_command_response(connection, queue_command, None)
    })?;
    finish_queued_server_command(command_name, response)
}

/// Queues a server command on an already open connection, ignoring the socket path.
pub(super) fn run_queued_server_command_with_connection(
    connection: &mut Connection,
    _socket_path: &Path,
    command_name: &'static str,
    queue_command: String,
) -> Result<i32, ExitFailure> {
    let response = queued_server_command_response(connection, queue_command, None)?;
    finish_queued_server_command(command_name, response)
}

/// Queues a server command on an open connection so the server runs it against `target`.
pub(super) fn run_queued_server_command_at_target_with_connection(
    connection: &mut Connection,
    command_name: &'static str,
    queue_command: String,
    target: PaneTarget,
) -> Result<i32, ExitFailure> {
    let response = queued_server_command_response(connection, queue_command, Some(target))?;
    finish_queued_server_command(command_name, response)
}

/// Runs the internal all-sessions `list-windows` execution path on an open connection.
pub(super) fn run_list_windows_all_server_command_with_connection(
    connection: &mut Connection,
    arguments: &[String],
) -> Result<i32, ExitFailure> {
    let response = list_windows_all_server_command_response(connection, arguments)?;
    finish_queued_server_command("list-windows", response)
}

/// Runs internal all-sessions `list-windows` and returns its exit status with captured output.
pub(super) fn capture_list_windows_all_server_command_with_connection(
    connection: &mut Connection,
    arguments: &[String],
) -> Result<(i32, CommandOutput), ExitFailure> {
    let response = list_windows_all_server_command_response(connection, arguments)?;
    let output = response
        .command_output()
        .cloned()
        .unwrap_or_else(|| CommandOutput::from_stdout(Vec::new()));
    if let Some(exit_status) = queued_server_command_nonzero_exit("list-windows", &response)? {
        return Ok((exit_status, output));
    }
    expect_command_success(response, "list-windows")
        .map_err(|error| normalize_queued_direct_error("list-windows", error))?;
    Ok((0, output))
}

/// Sends `arguments` to the server's internal all-sessions `list-windows` execution path.
fn list_windows_all_server_command_response(
    connection: &mut Connection,
    arguments: &[String],
) -> Result<Response, ExitFailure> {
    let payload = encode_internal_runtime_command_arguments(arguments).map_err(|error| {
        ExitFailure::new(
            1,
            format!("failed to encode internal list-windows arguments: {error}"),
        )
    })?;
    connection
        .source_file(
            vec![INTERNAL_LIST_WINDOWS_ALL_EXECUTION_PATH.to_owned()],
            false,
            false,
            false,
            false,
            None,
            Some(payload),
        )
        .map_err(ExitFailure::from)
}

/// Queues `queue_command`, using the canonical execution path when the server expands commands.
fn queued_server_command_response(
    connection: &mut Connection,
    queue_command: String,
    target: Option<PaneTarget>,
) -> Result<Response, ExitFailure> {
    let source_path = if connection
        .supports_capability(CAPABILITY_CLI_RUNTIME_COMMAND_EXPANSION)
        .map_err(ExitFailure::from)?
    {
        INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH
    } else {
        "-"
    };
    connection
        .source_file(
            vec![source_path.to_owned()],
            false,
            false,
            false,
            false,
            target,
            Some(queue_command),
        )
        .map_err(ExitFailure::from)
}

/// Turns a queued command's response into an exit code, preferring its reported failure status.
fn finish_queued_server_command(
    command_name: &'static str,
    response: Response,
) -> Result<i32, ExitFailure> {
    if let Some(exit_status) = queued_server_command_nonzero_exit(command_name, &response)? {
        return Ok(exit_status);
    }
    finish_command_success(response, command_name)
        .map_err(|error| normalize_queued_direct_error(command_name, error))
}

/// Extracts a queued command's failure, printing output and reporting diagnostics as errors.
fn queued_server_command_nonzero_exit(
    command_name: &'static str,
    response: &Response,
) -> Result<Option<i32>, ExitFailure> {
    if let Response::SourceFile(source) = &response {
        if source.exit_status().unwrap_or(0) != 0 && !source.stderr().is_empty() {
            if let Some(output) = source.command_output() {
                write_command_output(output)?;
            }
            let message = String::from_utf8_lossy(source.stderr())
                .trim_end_matches('\n')
                .to_owned();
            return Err(ExitFailure::new(source.exit_status().unwrap_or(1), message));
        }
    }
    let source_stdout_may_be_diagnostic = match &response {
        Response::SourceFile(source) if matches!(command_name, "if-shell" | "list-windows") => {
            source.exit_status().is_some_and(|status| status != 0)
        }
        _ => true,
    };
    if let Some(output) = response
        .command_output()
        .filter(|output| source_stdout_may_be_diagnostic && !output.stdout().is_empty())
    {
        let rendered = String::from_utf8_lossy(output.stdout());
        if let Some(message) = strip_source_file_stdin_line_prefix(&rendered) {
            let mut message = message.to_owned();
            while message.ends_with("\n\n") {
                message.pop();
            }
            while message.ends_with('\n') {
                message.pop();
            }
            return Err(ExitFailure::new(1, message));
        }
    }
    if let Response::SourceFile(source) = &response {
        if let Some(exit_status) = source.exit_status().filter(|status| *status != 0) {
            if let Some(output) = source.command_output() {
                write_command_output(output)?;
            }
            return Ok(Some(exit_status));
        }
    }
    Ok(None)
}

/// Runs `run` on the cached connection for `socket_path`, or a fresh one, dropping it on error.
fn with_command_connection<F, R>(socket_path: &Path, run: F) -> Result<R, ExitFailure>
where
    F: FnOnce(&mut Connection) -> Result<R, ExitFailure>,
{
    let run = match COMMAND_CONNECTION_CACHE.with(|cache| {
        let mut slot = cache.borrow_mut();
        let Some(entry) = slot
            .as_mut()
            .filter(|entry| entry.socket_path == socket_path)
        else {
            return Err(run);
        };
        let mut connection = match entry.connection.take() {
            Some(connection) => connection,
            None => match connect(socket_path) {
                Ok(connection) => connection,
                Err(error) => {
                    return Ok(Err(ExitFailure::from_client_connect(socket_path, error)));
                }
            },
        };
        let result = run(&mut connection);
        if result.is_ok() {
            entry.connection = Some(connection);
        }
        Ok(result)
    }) {
        Ok(result) => return result,
        Err(run) => run,
    };

    let mut connection = connect(socket_path)
        .map_err(|error| ExitFailure::from_client_connect(socket_path, error))?;
    run(&mut connection)
}

/// Resolves the pane this process was launched inside, when the environment names a live one.
pub(crate) fn inherited_pane_target(
    connection: &mut Connection,
    socket_path: &Path,
) -> Result<Option<PaneTarget>, ExitFailure> {
    let Some(pane_id) = inherited_pane_id(socket_path) else {
        return Ok(None);
    };
    let response = connection
        .resolve_target(Some(pane_id), ResolveTargetType::Pane, false, false)
        .map_err(ExitFailure::from)?;
    match response {
        Response::ResolveTarget(response) => match response.target {
            Target::Pane(target) => Ok(Some(target)),
            _ => Ok(None),
        },
        Response::Error(_) => Ok(None),
        _ => Ok(None),
    }
}

/// The inherited `RMUX_PANE` or `TMUX_PANE` id, only when it belongs to this socket.
fn inherited_pane_id(socket_path: &Path) -> Option<String> {
    if !rmux_env_socket_matches(socket_path) {
        return None;
    }
    std::env::var("RMUX_PANE")
        .ok()
        .or_else(|| std::env::var("TMUX_PANE").ok())
        .filter(|value| value.starts_with('%'))
}

/// Whether the inherited `RMUX` environment variable names the socket being addressed.
fn rmux_env_socket_matches(socket_path: &Path) -> bool {
    std::env::var("RMUX")
        .ok()
        .and_then(|value| rmux_socket_path_from_env(&value))
        .is_some_and(|inherited| rmux_os::path::socket_paths_match(&inherited, socket_path))
}

/// Extracts the socket path from an `RMUX` environment value of the form `path,...`.
pub(super) fn rmux_socket_path_from_env(value: &str) -> Option<PathBuf> {
    let path = value.split_once(',').map_or(value, |(path, _)| path);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Strips the server's synthetic stdin-source location from a directly invoked command's error.
fn normalize_queued_direct_error(command_name: &str, error: ExitFailure) -> ExitFailure {
    if command_name == "source-file" {
        return error;
    }
    strip_source_file_location(error)
}

/// Drops a leading `-:<line>: ` stdin source-file location from `error`'s message.
pub(super) fn strip_source_file_location(error: ExitFailure) -> ExitFailure {
    let Some(message) = strip_source_file_stdin_line_prefix(error.message()) else {
        return error;
    };
    ExitFailure::new(error.exit_code(), message.to_owned())
}

/// Returns the message after a leading `-:<line>: ` stdin source-file location prefix.
fn strip_source_file_stdin_line_prefix(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("-:")?;
    let (line, message) = rest.split_once(": ")?;
    line.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then_some(message)
}

/// Checks a successful response, prints any command output, and yields exit code `0`.
pub(super) fn finish_command_success(
    response: Response,
    command_name: &'static str,
) -> Result<i32, ExitFailure> {
    let output = response.command_output().cloned();
    expect_command_success(response, command_name)?;
    if let Some(output) = output {
        write_command_output(&output)?;
    }
    Ok(0)
}

/// Writes captured stdout to the process stdout, treating a broken pipe as success.
pub(super) fn write_command_output(output: &CommandOutput) -> Result<(), ExitFailure> {
    write_stdout(output.stdout(), "command").map(drop)
}

/// Prints `lines` as newline-terminated output, emitting nothing when the list is empty.
pub(super) fn write_lines_output(lines: &[String]) -> Result<i32, ExitFailure> {
    let text = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    write_stdout(text.as_bytes(), "command")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use rmux_client::ClientError;
    use rmux_proto::{ErrorResponse, Response, RmuxError};

    use super::{
        capture_target_action_needs_legacy_retry, strip_source_file_stdin_line_prefix,
        target_action_needs_legacy_retry,
    };

    #[test]
    fn target_action_retry_is_limited_to_protocol_decode_failures() {
        assert!(target_action_needs_legacy_retry(&Ok(Response::Error(
            ErrorResponse {
                error: RmuxError::Decode("unknown variant index".to_owned()),
            },
        ))));
        assert!(!target_action_needs_legacy_retry(&Err(
            ClientError::UnexpectedEof,
        )));
        assert!(capture_target_action_needs_legacy_retry(&Err(
            ClientError::UnexpectedEof,
        )));
        assert!(!target_action_needs_legacy_retry(&Ok(Response::Error(
            ErrorResponse {
                error: RmuxError::InvalidTarget {
                    value: "alpha:0.99".to_owned(),
                    reason: "can't find pane: 99".to_owned(),
                },
            },
        ))));
    }

    #[test]
    fn alias_fallback_errors_strip_synthetic_source_file_prefix() {
        assert_eq!(
            strip_source_file_stdin_line_prefix("-:1: unknown command: nope"),
            Some("unknown command: nope")
        );
        assert_eq!(
            strip_source_file_stdin_line_prefix("unknown command: nope"),
            None
        );
    }
}
