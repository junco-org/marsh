use super::RequestHandler;
use crate::pane_io::AttachControl;
use rmux_proto::{
    HookLifecycle, HookName, KillPaneRequest, KillSessionRequest, KillWindowRequest,
    LastPaneRequest, LastWindowRequest, LayoutName, LinkWindowRequest, ListPanesRequest,
    ListWindowsRequest, MoveWindowRequest, NewSessionExtRequest, NewWindowRequest,
    NextWindowRequest, OptionName, PaneSelectRequest, PaneTarget, PaneTargetRef,
    PreviousWindowRequest, ProcessCommand, RenameSessionRequest, RenameWindowRequest, Request,
    ResizeWindowAdjustment, ResizeWindowRequest, ResolveTargetRequest, ResolveTargetType,
    RespawnWindowRequest, Response, RotateWindowDirection, RotateWindowRequest, ScopeSelector,
    SelectLayoutRequest, SelectLayoutTarget, SelectPaneAdjacentRequest, SelectPaneDirection,
    SelectPaneRequest, SelectWindowRequest, SessionName, SetOptionMode, SetOptionRequest,
    SplitDirection, SplitWindowRequest, SwapWindowRequest, Target, TerminalSize,
    UnlinkWindowRequest, WindowTarget,
};
use std::fs;
use std::path::Path;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

use crate::test_fixtures::{
    quiet_command, unique_temp_path, wait_for_file_contents, wait_until, Fixture, Owned,
    SessionSpec, TestRequest,
};
use crate::test_names::session_name;
use crate::test_shell::sh_quote_path;

/// The size of every session the window tests create.
const WINDOW_TEST_SIZE: TerminalSize = TerminalSize {
    cols: 120,
    rows: 40,
};

fn window_respawn_replay_command(output: &Path, tag: &str) -> String {
    format!(
        "printf '%s:%s:{tag}\\n' \"$(pwd)\" \"$RMUX_RESPAWN\" >> {}; sleep 60",
        sh_quote_path(output)
    )
}

fn window_respawn_shell_identity_command(output: &Path, tag: &str) -> String {
    format!(
        "printf '%s:%s:{tag}\\n' \"${{0##*/}}\" \"$SHELL\" >> {}; sleep 60",
        sh_quote_path(output)
    )
}

/// Waits until `path` holds exactly these respawn probe lines, in order.
///
/// Each expectation is `(seed-relative directory, `RMUX_RESPAWN` value, command tag)`, and the
/// directory is matched by *suffix* rather than in full. A job runs inside a snapshot of the
/// seed, so its own `pwd` is `<snapshot root>/<uid>/<seed-relative directory>` and the uid is
/// allocated by the spawn — no test can name the whole string beforehand. The seed-relative part
/// is both what the caller asked for and what a regression loses: a start directory that was
/// dropped opens at the snapshot root instead, where the path ends with the uid.
async fn wait_for_window_respawn_probe(path: &Path, expected: &[(&str, &str, &str)]) {
    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || {
            let contents = fs::read_to_string(path).unwrap_or_default();
            let lines = contents.lines().collect::<Vec<_>>();
            if lines.len() == expected.len()
                && lines
                    .iter()
                    .zip(expected)
                    .all(|(line, expectation)| window_respawn_probe_line_matches(line, expectation))
            {
                Ok(())
            } else {
                Err(contents)
            }
        },
    )
    .await
    .unwrap_or_else(|contents| {
        panic!(
            "timed out waiting for {} to hold {expected:?}, got {contents:?}",
            path.display()
        )
    });
}

/// Whether one probe line reports the expected directory, environment and command tag.
fn window_respawn_probe_line_matches(line: &str, expected: &(&str, &str, &str)) -> bool {
    let (directory, environment, tag) = *expected;
    let mut fields = line.splitn(3, ':');
    let (Some(reported_directory), Some(reported_environment), Some(reported_tag)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    reported_directory.ends_with(&format!("/{directory}"))
        && reported_environment == environment
        && reported_tag == tag
}

/// Creates the detached 120x40 session `name`, whose first pane runs [`quiet_command`].
async fn create_session(handler: &RequestHandler, name: impl Owned<SessionName>) -> SessionName {
    SessionSpec::create(
        handler,
        NewSessionExtRequest {
            size: Some(WINDOW_TEST_SIZE),
            command: Some(quiet_command()),
            ..Fixture::fixture(name)
        },
    )
    .await
}

/// Creates the detached 120x40 session `name` in the group of `group`; its pane runs the shell.
async fn create_grouped_session(
    handler: &RequestHandler,
    name: impl Owned<SessionName>,
    group: impl Owned<SessionName>,
) -> SessionName {
    SessionSpec::create(
        handler,
        NewSessionExtRequest {
            size: Some(WINDOW_TEST_SIZE),
            group_target: Some(group.owned()),
            ..Fixture::fixture(name)
        },
    )
    .await
}

async fn enable_global_monitor_silence(handler: &RequestHandler) {
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;
}

async fn insert_window(handler: &RequestHandler, session_name: &SessionName, window_index: u32) {
    let mut state = handler.state.lock().await;
    let pane_id = state.sessions.allocate_pane_id();
    {
        let session = state
            .sessions
            .session_mut(session_name)
            .expect("session should exist");
        session
            .insert_window_with_initial_pane_with_id(
                window_index,
                TerminalSize { cols: 90, rows: 30 },
                pane_id,
            )
            .expect("window insert succeeds");
    }
    state
        .insert_window_terminal(
            session_name,
            window_index,
            crate::pane_terminals::WindowSpawnOptions {
                start_directory: None,
                command: None,
                socket_path: Path::new("/tmp/rmux-test.sock"),
                spawn_environment: None,
                environment_overrides: None,
                respawn_shell: None,
                respawn_environment: None,
            },
        )
        .await
        .expect("window terminal insert succeeds");
}

async fn link_duplicate_window(
    handler: &RequestHandler,
    session_name: &SessionName,
    source_index: u32,
    destination_index: u32,
) {
    TestRequest::send_ok(
        handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(session_name.clone(), source_index),
            WindowTarget::with_window(session_name.clone(), destination_index),
        )),
    )
    .await;
}

fn assert_refresh(control: AttachControl) {
    assert!(matches!(control, AttachControl::Switch(_)));
}

async fn drain_attach_controls(control_rx: &mut mpsc::UnboundedReceiver<AttachControl>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let idle = remaining.min(Duration::from_millis(250));
        match timeout(idle, control_rx.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

async fn drain_attach_control_pair(
    first: &mut mpsc::UnboundedReceiver<AttachControl>,
    second: &mut mpsc::UnboundedReceiver<AttachControl>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let idle = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        tokio::select! {
            message = first.recv() => {
                if message.is_none() {
                    break;
                }
            }
            message = second.recv() => {
                if message.is_none() {
                    break;
                }
            }
            () = tokio::time::sleep(idle) => break,
        }
    }
}

#[path = "handler_window_tests/lifecycle.rs"]
mod lifecycle;

#[path = "handler_window_tests/renumber.rs"]
mod renumber;

#[path = "handler_window_tests/listing_refresh.rs"]
mod listing_refresh;

#[path = "handler_window_tests/move_window.rs"]
mod move_window;

#[path = "handler_window_tests/relative_group_transactions.rs"]
mod relative_group_transactions;
#[path = "handler_window_tests/relative_metadata.rs"]
mod relative_metadata;

#[path = "handler_window_tests/swap_rotate.rs"]
mod swap_rotate;

#[path = "handler_window_tests/swap_selection_notifications.rs"]
mod swap_selection_notifications;

#[path = "handler_window_tests/silence_fanout.rs"]
mod silence_fanout;

#[path = "handler_window_tests/link_unlink.rs"]
mod link_unlink;

#[path = "handler_window_tests/active_selection.rs"]
mod active_selection;

#[path = "handler_window_tests/linked_pane_selection.rs"]
mod linked_pane_selection;

#[path = "handler_window_tests/pane_selection_hooks.rs"]
mod pane_selection_hooks;

#[path = "handler_window_tests/pane_selection_noop.rs"]
mod pane_selection_noop;

#[path = "handler_window_tests/linked_window_mutations.rs"]
mod linked_window_mutations;

#[path = "handler_window_tests/resize_respawn.rs"]
mod resize_respawn;

#[path = "handler_window_tests/resize_window_client_size.rs"]
mod resize_window_client_size;

#[path = "handler_window_tests/respawn_linked_refresh.rs"]
mod respawn_linked_refresh;

#[path = "handler_window_tests/respawn_linked_guard.rs"]
mod respawn_linked_guard;
