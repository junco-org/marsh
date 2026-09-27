use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use super::control_support::{with_control_queue_identity, ControlClientIdentity};
use super::RequestHandler;
use crate::control::{ControlModeUpgrade, ControlServerEvent, CONTROL_SERVER_EVENT_CAPACITY};
use crate::handler::scripting_support::{parse_request_from_parts, QueueExecutionContext};
use crate::handler::ControlRegistration;
use crate::hook_runtime::with_hook_execution;
use crate::input_keys::MouseForwardEvent;
use crate::mouse::{AttachedMouseEvent, MouseLocation};
use crate::outer_terminal::OuterTerminalContext;
use crate::pane_io::AttachControl;
use crate::pane_terminals::seed_scratch_dir;
use crate::server_access::AccessMode;
use crate::test_fixtures::{quiet_command, unique_temp_path, Fixture, Quiet, TestRequest};
use crate::test_shell::{command_quote, sh_quote_path};
use rmux_core::command_parser::CommandParser;
use rmux_core::input::InputParser;
use rmux_core::{OptionStore, PaneId, Screen, SessionStore, TargetFindContext};
use rmux_os::identity::UserIdentity;
use rmux_proto::{
    encode_internal_runtime_command_arguments, BreakPaneRequest, HookName, IfShellRequest,
    KillSessionRequest, KillWindowRequest, LastWindowRequest, LinkWindowRequest,
    NewSessionExtRequest, NewWindowRequest, NextWindowRequest, OptionName, OptionScopeSelector,
    PaneTarget, PreviousWindowRequest, Request, RespawnPaneRequest, RespawnWindowRequest, Response,
    RmuxError, RotateWindowDirection, RotateWindowRequest, RunShellDelaySeconds, RunShellRequest,
    RunShellResponse, ScopeSelector, SelectPaneRequest, SessionName, SetBufferRequest,
    SetEnvironmentRequest, ShowBufferRequest, ShowEnvironmentRequest, ShowOptionsRequest,
    SourceFileRequest, SplitDirection, SplitWindowRequest, SwapPaneDirection, SwapPaneRequest,
    Target, TerminalSize, WaitForMode, WaitForRequest, WaitForResponse, WindowTarget,
    INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH, INTERNAL_PARSE_TIME_ASSIGNMENTS_PATH,
    INTERNAL_RUNTIME_COMMAND_EXPANSION_PATH,
};
use tokio::sync::mpsc;

use crate::test_names::session_name;

fn wait_for(channel: &str, mode: WaitForMode) -> Request {
    WaitForRequest::fixture((channel, mode)).into_request()
}

fn run_shell(command: &str, background: bool) -> Request {
    RunShellRequest {
        background,
        ..Fixture::fixture(command)
    }
    .into_request()
}

fn source_file_request(paths: Vec<String>, cwd: Option<PathBuf>) -> Request {
    SourceFileRequest {
        caller_cwd: cwd,
        ..Fixture::fixture(paths)
    }
    .into_request()
}

fn show_buffer_request(name: &str) -> Request {
    Request::ShowBuffer(ShowBufferRequest {
        name: Some(name.to_owned()),
    })
}

fn source_file_stdout_failure(response: Response) -> String {
    let Response::SourceFile(response) = response else {
        panic!("expected source-file failure response, got {response:?}");
    };
    assert_eq!(response.exit_status(), Some(1));
    assert!(
        response.stderr().is_empty(),
        "source-file parse diagnostics must stay on stdout: {:?}",
        response.stderr()
    );
    String::from_utf8(
        response
            .command_output()
            .expect("source-file failure should include stdout diagnostics")
            .stdout()
            .to_vec(),
    )
    .expect("source-file stdout diagnostic is UTF-8")
}

/// A host scratch path for a file this test only reads and writes itself.
///
/// Right for a configuration file `source-file` is pointed at: the path is an operand, resolved
/// by the managed reader against the host filesystem, and nothing is ever *opened over* it.
///
/// Wrong for a directory a pane is asked to start in. A pane opens over a snapshot of the one
/// seed this daemon leases, so a NAMED start directory that is a sibling of that seed is a place
/// the daemon genuinely cannot honour and refuses loudly. Use
/// [`seed_scratch_dir`](crate::pane_terminals::seed_scratch_dir) for those.
fn temp_root(label: &str) -> PathBuf {
    unique_temp_path(&format!("source-file-{label}"))
}

fn write_config(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("config parent directory");
    }
    fs::write(path, contents).expect("write config");
}

fn write_executable_script(path: &Path, contents: &str) {
    write_config(path, contents);
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script permissions");
}

/// Worker stack for tests whose queues nest command dispatch and attached rendering in one poll:
/// the daemon worker budget, independent of the test harness thread stack.
const DAEMON_TEST_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Runs `test` to completion on a current-thread runtime in a thread with the daemon's stack.
fn run_on_daemon_test_stack<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let worker = std::thread::Builder::new()
        .name("scripting-test".to_owned())
        .stack_size(DAEMON_TEST_STACK_SIZE)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scripting test runtime should build");
            runtime.block_on(test());
        })
        .expect("scripting test worker should spawn");
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

/// A session store holding 80x24 session `alpha`, and a find context whose current pane is
/// `alpha:0.0`.
fn parser_fixture() -> (SessionStore, TargetFindContext) {
    let alpha = session_name("alpha");
    let mut sessions = SessionStore::new();
    sessions
        .create_session(alpha.clone(), TerminalSize { cols: 80, rows: 24 })
        .expect("parser fixture session");
    let find_context =
        TargetFindContext::from_target(Target::Pane(PaneTarget::with_window(alpha, 0, 0)));
    (sessions, find_context)
}

fn parse_server_request(
    command: &str,
    arguments: &[&str],
    sessions: &SessionStore,
    find_context: &TargetFindContext,
) -> Result<Request, RmuxError> {
    parse_request_from_parts(
        command.to_owned(),
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        None,
        sessions,
        &OptionStore::default(),
        find_context,
    )
}

/// A queue context whose current target is pane 0 of `session_name:window_index`.
fn pane_context(session_name: &SessionName, window_index: u32) -> QueueExecutionContext {
    QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
        PaneTarget::with_window(session_name.clone(), window_index, 0),
    )))
}

/// Parses and runs `command` for this process, panicking if either step fails.
async fn execute(handler: &RequestHandler, command: &str) {
    let parsed = CommandParser::new().parse(command).expect("command parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .unwrap_or_else(|error| panic!("{command} should execute: {error}"));
}

/// Registers a plain control client for uid 1000, writable when `can_write`.
async fn register_control_client(
    handler: &RequestHandler,
    requester_pid: u32,
    can_write: bool,
) -> mpsc::Receiver<ControlServerEvent> {
    let (event_tx, event_rx) = mpsc::channel::<ControlServerEvent>(CONTROL_SERVER_EVENT_CAPACITY);
    handler
        .register_control_with_access(
            requester_pid,
            ControlModeUpgrade {
                initial_command_count: 0,
                mode: rmux_proto::ControlMode::Plain,
                terminal_context: OuterTerminalContext::default(),
            },
            ControlRegistration {
                event_tx,
                closing: Arc::new(AtomicBool::new(false)),
                uid: 1000,
                user: UserIdentity::Uid(1000),
                can_write,
            },
        )
        .await
        .expect("control registration succeeds");
    event_rx
}

/// A handler with started 20x6 quiet session `name`, and that session's first pane.
async fn mouse_fixture(name: &str) -> (RequestHandler, SessionName, PaneTarget) {
    let handler = RequestHandler::new();
    let session = handler
        .create_started_session(NewSessionExtRequest {
            size: Some(TerminalSize { cols: 20, rows: 6 }),
            command: Some(quiet_command()),
            ..Fixture::fixture(name)
        })
        .await;
    let target = PaneTarget::with_window(session.clone(), 0, 0);
    (handler, session, target)
}

/// A left-button press at (1, 1) inside pane 0 of `target`.
fn mouse_event(target: &PaneTarget) -> AttachedMouseEvent {
    AttachedMouseEvent {
        raw: MouseForwardEvent {
            b: 0,
            lb: 0,
            x: 1,
            y: 1,
            lx: 1,
            ly: 1,
            sgr_b: 0,
            sgr_type: 'M',
            ignore: false,
        },
        session_id: 1,
        window_id: Some(1),
        pane_id: Some(PaneId::new(0)),
        pane_target: Some(target.clone()),
        location: MouseLocation::Pane,
        status_at: None,
        status_lines: 0,
        ignore: false,
    }
}

/// Replaces the screen of 20x6 pane `target` with three known lines.
async fn seed_copy_mode_screen(handler: &RequestHandler, target: &PaneTarget) {
    let transcript = {
        let state = handler.state.lock().await;
        state.transcript_handle(target).expect("pane transcript")
    };
    let history_limit = transcript
        .lock()
        .expect("pane transcript mutex")
        .history_limit();
    let mut screen = Screen::new(TerminalSize { cols: 20, rows: 6 }, history_limit);
    let mut parser = InputParser::new();
    parser.parse(
        b"zero one two three\r\nalpha beta gamma\r\nomega sigma tau\r\n",
        &mut screen,
    );
    transcript
        .lock()
        .expect("pane transcript mutex")
        .set_screen_for_test(screen);
}

/// Enters copy mode on `target` with the cursor six cells into the top history line.
fn copy_cursor_command(target: &PaneTarget) -> String {
    format!(
        "copy-mode -t {target}; send-keys -Xt {target} history-top; \
         send-keys -Xt {target} start-of-line; send-keys -N6 -Xt {target} cursor-right"
    )
}

async fn selection_coordinates(
    handler: &RequestHandler,
    session: &SessionName,
) -> Option<(u32, usize)> {
    let state = handler.state.lock().await;
    state
        .pane_copy_mode_summary(session, PaneId::new(0))
        .and_then(|summary| summary.selection_start)
        .map(|position| (position.x, position.y))
}

/// Waits up to `timeout` for the background task `name` to stop running.
async fn wait_for_background_task(handler: &RequestHandler, name: &'static str, timeout: Duration) {
    tokio::task::yield_now().await;
    tokio::time::timeout(timeout, async {
        while handler.background_task_running_for_test(name) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("background task {name} did not finish"));
}

async fn wait_for_named_buffer(handler: &RequestHandler, name: &str, expected: &[u8]) {
    tokio::time::timeout(background_shell_test_timeout(), async {
        loop {
            if let Some(output) = handler
                .handle(show_buffer_request(name))
                .await
                .command_output()
            {
                if output.stdout() == expected {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("buffer {name:?} did not become {expected:?}"));
}

async fn wait_for_detached_request_count(handler: &RequestHandler, expected: usize) {
    tokio::time::timeout(background_shell_test_timeout(), async {
        loop {
            let active = handler
                .active_detached_requests
                .load(std::sync::atomic::Ordering::SeqCst);
            if active == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("detached request count did not become {expected}"));
}

fn background_shell_test_timeout() -> Duration {
    // Background shell startup competes with thousands of async tests in
    // the full server suite. Keep this as a bounded liveness budget, not a
    // scheduler-latency assertion.
    Duration::from_secs(8)
}

async fn replace_background_identity_session(handler: &RequestHandler, session_name: SessionName) {
    handler
        .handle_ok(KillSessionRequest::fixture(&session_name))
        .await;
    handler.create_session(session_name).await;
}

async fn wait_for_active_window_name(
    handler: &RequestHandler,
    session_name: &SessionName,
    expected: &str,
) {
    tokio::time::timeout(background_shell_test_timeout(), async {
        loop {
            let matches = {
                let state = handler.state.lock().await;
                state
                    .sessions
                    .session(session_name)
                    .and_then(|session| session.window_at(session.active_window_index()))
                    .and_then(rmux_core::Window::name)
                    == Some(expected)
            };
            if matches {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background command follows the active attached session");
}

async fn wait_for_background_waiter(handler: &RequestHandler, channel: &str) {
    tokio::time::timeout(background_shell_test_timeout(), async {
        loop {
            if handler.wait_for_counts(channel).0 == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background command reaches its wait-for seam");
}

async fn release_background_waiter(handler: &RequestHandler, channel: &str) {
    let response = handler.handle(wait_for(channel, WaitForMode::Signal)).await;
    assert_eq!(response, Response::WaitFor(WaitForResponse));
}

async fn assert_sessions_survive_background_control_reuse(
    handler: &RequestHandler,
    original: &SessionName,
    replacement: &SessionName,
) {
    wait_for_detached_request_count(handler, 0).await;
    let state = handler.state.lock().await;
    assert!(
        state.sessions.contains_session(original),
        "the stale background command must not mutate the original session"
    );
    assert!(
        state.sessions.contains_session(replacement),
        "the stale background command must not jump to the replacement registration"
    );
}

fn delayed_true_shell_condition() -> String {
    "sleep 0.05; true".to_owned()
}

fn builtin_true_shell_condition() -> &'static str {
    "true"
}

fn shell_print_command(text: &str) -> String {
    format!("printf {}", command_quote(text))
}

fn shell_print_then_exit_command(text: &str, code: u8) -> String {
    format!("printf {}; exit {code}", command_quote(text))
}

fn shell_stderr_command(text: &str) -> String {
    format!("printf {} >&2", command_quote(text))
}

fn shell_success_command() -> String {
    "true".to_owned()
}

#[path = "handler_scripting_tests/run_shell.rs"]
mod run_shell;

#[path = "handler_scripting_tests/source_file_core.rs"]
mod source_file_core;

#[path = "handler_scripting_tests/source_file_conditions.rs"]
mod source_file_conditions;

#[path = "handler_scripting_tests/if_shell.rs"]
mod if_shell;

#[path = "handler_scripting_tests/parsed_queue_core.rs"]
mod parsed_queue_core;

#[path = "handler_scripting_tests/detached_access.rs"]
mod detached_access;

#[path = "handler_scripting_tests/parsed_queue_cwd.rs"]
mod parsed_queue_cwd;

#[path = "handler_scripting_tests/queued_inventory.rs"]
mod queued_inventory;

#[path = "handler_scripting_tests/queue_exact_target.rs"]
mod queue_exact_target;

#[path = "handler_scripting_tests/parsed_queue_split.rs"]
mod parsed_queue_split;

#[path = "handler_scripting_tests/parsed_queue_targets.rs"]
mod parsed_queue_targets;

#[path = "handler_scripting_tests/parsed_queue_swap_window.rs"]
mod parsed_queue_swap_window;

#[path = "handler_scripting_tests/parsed_queue_windows_mouse.rs"]
mod parsed_queue_windows_mouse;

#[path = "handler_scripting_tests/parsed_queue_move_window_current.rs"]
mod parsed_queue_move_window_current;

#[path = "handler_scripting_tests/parsed_queue_select_zoom.rs"]
mod parsed_queue_select_zoom;

#[path = "handler_scripting_tests/parsed_queue_resize_trim.rs"]
mod parsed_queue_resize_trim;

#[path = "handler_scripting_tests/mouse_origin_copy_mode.rs"]
mod mouse_origin_copy_mode;

#[path = "handler_scripting_tests/prompt_mouse_origin.rs"]
mod prompt_mouse_origin;

#[path = "handler_scripting_tests/control_hooks_wait.rs"]
mod control_hooks_wait;

#[path = "handler_scripting_tests/list_windows_all.rs"]
mod list_windows_all;

#[path = "handler_scripting_tests/command_alias.rs"]
mod command_alias;

#[path = "handler_scripting_tests/command_blocks.rs"]
mod command_blocks;

#[path = "handler_scripting_tests/parser_flags.rs"]
mod parser_flags;

#[path = "handler_scripting_tests/parser_option_flags.rs"]
mod parser_option_flags;

#[path = "handler_scripting_tests/select_layout_flags.rs"]
mod select_layout_flags;
