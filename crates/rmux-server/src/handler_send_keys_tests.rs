use super::super::RequestHandler;
use super::session_name;
use crate::input_keys::{
    encode_key, encode_mouse_event, ExtendedKeyFormat, MouseForwardEvent, MAX_SGR_MOUSE_FRAME_BYTES,
};
use crate::mouse::{AttachedMouseEvent, MouseLocation};
use crate::pane_io::AttachControl;
use crate::test_fixtures::{quiet_command, wait_until, Fixture, Quiet, SessionSpec, TestRequest};
use rmux_core::{input::mode, key_string_lookup_string};
use rmux_proto::{
    BindKeyRequest, CopyModeRequest, DisplayMessageExtRequest, ErrorResponse, HookName,
    ListKeysRequest, ListPanesRequest, NewSessionExtRequest, OptionName, PaneBroadcastInputRequest,
    PaneId, PaneTarget, PaneTargetRef, Request, Response, RmuxError, ScopeSelector,
    SelectPaneRequest, SendKeysExtRequest, SendKeysRequest, SendKeysResponse, SendPrefixRequest,
    SendPrefixResponse, SetHookRequest, SetOptionRequest, ShowBufferRequest, SplitDirection,
    SplitWindowRequest, SwitchClientExtRequest, Target, TerminalSize, UnbindKeyRequest,
    WindowTarget, DEFAULT_MAX_FRAME_LENGTH,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[path = "handler_send_keys_tests/basic_dispatch.rs"]
mod basic_dispatch;

#[path = "handler_send_keys_tests/target_client.rs"]
mod target_client;

#[path = "handler_send_keys_tests/bindings_timeouts.rs"]
mod bindings_timeouts;

use super::super::input_capture::RawPaneInputProbe;

#[path = "handler_send_keys_tests/live_attach.rs"]
mod live_attach;

#[path = "handler_send_keys_tests/read_only_detach.rs"]
mod read_only_detach;

#[path = "handler_send_keys_tests/read_only_navigation_security.rs"]
mod read_only_navigation_security;

#[path = "handler_send_keys_tests/kitty_keyboard.rs"]
mod kitty_keyboard;

#[path = "handler_send_keys_tests/bracketed_paste_live.rs"]
mod bracketed_paste_live;

#[path = "handler_send_keys_tests/bracketed_paste_large.rs"]
mod bracketed_paste_large;

#[path = "handler_send_keys_tests/bracketed_paste_final_sink.rs"]
mod bracketed_paste_final_sink;

#[path = "handler_send_keys_tests/kitty_graphics_live.rs"]
mod kitty_graphics_live;

#[path = "handler_send_keys_tests/palette_modal.rs"]
mod palette_modal;

#[path = "handler_send_keys_tests/synchronize_panes.rs"]
mod synchronize_panes;

#[path = "handler_send_keys_tests/attached_input_bounds.rs"]
mod attached_input_bounds;

#[path = "handler_send_keys_tests/mouse_copy_mode.rs"]
mod mouse_copy_mode;

#[path = "handler_send_keys_tests/copy_mode_mouse_origin.rs"]
mod copy_mode_mouse_origin;

#[path = "handler_send_keys_tests/copy_mode_vi.rs"]
mod copy_mode_vi;

#[path = "handler_send_keys_tests/key_table_precedence.rs"]
mod key_table_precedence;

#[path = "handler_send_keys_tests/combined_input_credit.rs"]
mod combined_input_credit;

async fn handle_boxed(handler: &RequestHandler, request: Request) -> Response {
    Box::pin(handler.handle(request)).await
}

/// A detached 80x24 session whose pane runs `/bin/bash`, the shell these tests' key encodings
/// assume.
async fn create_send_keys_test_session(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
) {
    handler
        .store_option_for_test(ScopeSelector::Global, OptionName::DefaultShell, "/bin/bash")
        .await;
    SessionSpec::create(handler, session).await;
}

// Like create_send_keys_test_session but the pane runs an inert, silent command
// and we block until its terminal has finished starting, so a subsequent
// transcript write is the only content in the pane.
async fn create_quiet_input_session(handler: &RequestHandler, session: &rmux_proto::SessionName) {
    SessionSpec::create_started(handler, Quiet(session)).await;
}

/// Splits the active pane of `session` side by side (`split-window -h`).
async fn split_window_horizontally(handler: &RequestHandler, session: &rmux_proto::SessionName) {
    TestRequest::send_ok(
        handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(session)
        },
    )
    .await;
}

/// Binds `key` in `table` to `command`, without a note or repeat.
async fn bind(handler: &RequestHandler, table: &str, key: &str, command: &[&str]) {
    let command = command.iter().copied();
    TestRequest::send_ok(handler, BindKeyRequest::fixture((table, key, command))).await;
}

/// Read a user option back the way the issue reporter did (`show-options -gv`).
async fn probe_value(handler: &RequestHandler, name: &str) -> String {
    let response = handler
        .handle(Request::ShowOptions(rmux_proto::ShowOptionsRequest {
            scope: rmux_proto::OptionScopeSelector::SessionGlobal,
            name: Some(name.to_owned()),
            value_only: true,
            include_inherited: false,
            quiet: true,
            include_hooks: false,
        }))
        .await;
    let Response::ShowOptions(response) = response else {
        panic!("expected show-options response, got {response:?}");
    };
    String::from_utf8(response.command_output().stdout().to_vec())
        .expect("option value is utf-8")
        .trim()
        .to_owned()
}

/// Sets `mode-keys` on window 0 of `session`.
async fn set_mode_keys(handler: &RequestHandler, session: &rmux_proto::SessionName, value: &str) {
    let window = WindowTarget::with_window(session.clone(), 0);
    handler
        .set_option(ScopeSelector::Window(window), OptionName::ModeKeys, value)
        .await;
}

/// The first row `list-panes -F format` renders for `target`'s session, trimmed.
async fn first_pane_row(
    handler: &RequestHandler,
    target: &PaneTarget,
    format: &str,
) -> Option<String> {
    let listed = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: target.session_name().clone(),
            format: Some(format.to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })))
        .await;
    let output = listed
        .command_output()
        .expect("list-panes returns command output");
    String::from_utf8_lossy(output.stdout())
        .lines()
        .next()
        .map(|row| row.trim().to_owned())
}

/// The pane's mode as `#{pane_mode}` renders it. An out-of-mode pane renders an
/// empty value, so an absent row and an empty row mean the same thing.
async fn pane_mode(handler: &RequestHandler, target: &PaneTarget) -> String {
    first_pane_row(handler, target, "#{pane_mode}")
        .await
        .unwrap_or_default()
}

/// `list-keys` of `table` (every table for `None`), unfiltered and including unnoted keys.
fn list_keys_request(table: Option<&str>) -> ListKeysRequest {
    ListKeysRequest {
        table_name: table.map(str::to_owned),
        first_only: false,
        notes: false,
        include_unnoted: true,
        reversed: false,
        format: None,
        sort_order: None,
        prefix: None,
        key: None,
    }
}

/// Feeds `bytes` to the transcript of `session`'s first pane as if that pane had printed them.
async fn append_pane_output(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
    bytes: &[u8],
) {
    let mut state = handler.state.lock().await;
    state
        .append_bytes_to_pane_transcript_for_test(session, 0, 0, bytes)
        .expect("pane transcript update");
}

/// The key table attached client `requester_pid` is in, `None` for the default one.
async fn client_key_table(handler: &RequestHandler, requester_pid: u32) -> Option<String> {
    handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered")
        .key_table_name
        .clone()
}

/// The session attached client `requester_pid` currently shows.
async fn active_session_name(
    handler: &RequestHandler,
    requester_pid: u32,
) -> rmux_proto::SessionName {
    handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered")
        .session_name
        .clone()
}

/// Attaches client `requester_pid` to `session` read-only and answers with its controls.
async fn register_read_only_attach(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &rmux_proto::SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    let control_rx = handler.attach_client(requester_pid, session).await;
    let mut active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get_mut(&requester_pid)
        .expect("read-only attach is active");
    active.can_write = false;
    active.flags = active.flags.with_read_only();
    control_rx
}

/// The body of the bracketed paste `bytes`, without its delimiters.
fn bracketed_paste_body(bytes: &[u8]) -> &[u8] {
    &bytes[b"\x1b[200~".len()..bytes.len() - b"\x1b[201~".len()]
}
