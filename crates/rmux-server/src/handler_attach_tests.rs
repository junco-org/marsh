use super::attach_support::{AttachRegistration, ClientFlags};
use super::RequestHandler;
use crate::input_keys::{MouseForwardEvent, MAX_SGR_MOUSE_FRAME_BYTES};
use crate::mouse::{AttachedMouseEvent, MouseLocation};
use crate::outer_terminal::OuterTerminalContext;
use crate::pane_io::AttachControl;
use crate::server_access::current_owner_uid;
use rmux_core::{input::InputParser, Screen};
use rmux_proto::request::{
    AttachSessionExt2Request, AttachSessionExt3Request, AttachSessionExtRequest,
    NewSessionExtRequest, SplitWindowExtRequest, SwitchClientExt2Request,
};
use rmux_proto::{
    AttachSessionResponse, AttachedKeystroke, CapturePaneRequest, CopyModeRequest,
    DetachClientExtRequest, DetachClientRequest, ErrorResponse, KeyDispatched, KillSessionRequest,
    LayoutName, LinkWindowRequest, ListPanesRequest, ListWindowsRequest, NewSessionRequest,
    NewWindowRequest, OptionName, PaneTarget, RenameSessionRequest, Request, ResizePaneAdjustment,
    ResolveTargetRequest, ResolveTargetType, Response, RmuxError, ScopeSelector,
    SelectLayoutRequest, SelectLayoutTarget, SelectPaneRequest, SelectWindowRequest,
    SendKeysRequest, SessionName, SetOptionMode, SetOptionRequest, SplitWindowRequest,
    SwitchClientRequest, Target, TerminalSize, WindowTarget, CAPABILITY_ATTACH_RENDER,
};
use rmux_pty::{ChildCommand, TerminalSize as PtyTerminalSize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::sleep;

const ATTACH_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);

use crate::test_fixtures::{
    Fixture, Grouped, Quiet, SessionSpec, TestRequest, DEFAULT_SHELL_WINDOW_NAME,
};
use crate::test_names::session_name;

fn default_shell_pane_status() -> String {
    format!("{DEFAULT_SHELL_WINDOW_NAME}|0|\n")
}

fn take_render_frame(control: AttachControl) -> String {
    match control {
        AttachControl::Switch(target) => String::from_utf8(target.into_target().render_frame)
            .expect("render frame must be utf-8"),
        AttachControl::Detach => panic!("expected a switch refresh"),
        AttachControl::Exited => panic!("expected a switch refresh"),
        AttachControl::DetachKill => panic!("expected a switch refresh"),
        AttachControl::DetachExecShellCommand(_) => panic!("expected a switch refresh"),
        AttachControl::InteractiveInput => panic!("expected a switch refresh"),
        AttachControl::Refresh => panic!("expected a switch refresh"),
        AttachControl::Overlay(_) => panic!("expected a switch refresh"),
        AttachControl::Write(_) => panic!("expected a switch refresh"),
        AttachControl::ClipboardWrite { .. } => panic!("expected a switch refresh"),
        AttachControl::LockShellCommand(_) => panic!("expected a switch refresh"),
        AttachControl::AdvancePersistentOverlayState(_) => panic!("expected a switch refresh"),
        AttachControl::Suspend => panic!("expected a switch refresh"),
    }
}

fn take_switch_target(control: AttachControl) -> crate::pane_io::AttachTarget {
    match control {
        AttachControl::Switch(target) => *target.into_target(),
        other => panic!("expected a switch refresh, got {other:?}"),
    }
}

async fn recv_attach_control(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) -> AttachControl {
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, control_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for attach control: {context}"))
        .unwrap_or_else(|| panic!("attach control channel closed while waiting for {context}"))
}

async fn recv_render_frame(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) -> String {
    take_render_frame(recv_attach_control(control_rx, context).await)
}

async fn recv_switch_target(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) -> crate::pane_io::AttachTarget {
    take_switch_target(recv_attach_control(control_rx, context).await)
}

/// Receives the switch that *moved* this client, skipping the renders it is already watching.
///
/// A live pane's shell draws a prompt, and the refresh that follows reaches the attached client
/// as an `AttachControl::Switch` carrying the target it is already showing — the same control a
/// real move uses. Taking the first `Switch` therefore reports the pre-move session, and
/// [`AttachControl::is_coalescible_render_switch`] is exactly the distinction between the two.
async fn recv_moved_switch_target(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) -> crate::pane_io::AttachTarget {
    let control = recv_matching_attach_control(control_rx, context, |control| {
        !control.is_coalescible_render_switch()
    })
    .await;
    take_switch_target(control)
}

/// Receives the switch that moved this client into `joined`, ignoring the renders it was
/// already being sent.
///
/// Stronger than [`recv_moved_switch_target`] where the move's own target is a plain render —
/// migrating between two aliases of one linked window keeps the same live pane, so the move need
/// not be a handover and `is_coalescible_render_switch` can no longer separate it from a prompt
/// repaint. The session named on the target can: until the move commits, this client is still
/// registered against the session it is leaving, so every refresh it can receive names that one.
/// The first switch naming `joined` is therefore the move itself.
async fn recv_switch_target_into_session(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
    joined: &SessionName,
) -> crate::pane_io::AttachTarget {
    let control = recv_matching_attach_control(control_rx, context, |control| {
        matches!(
            control,
            AttachControl::Switch(target)
                if target
                    .with_target(|target| &target.session_name == joined)
                    .unwrap_or(false)
        )
    })
    .await;
    take_switch_target(control)
}

async fn recv_matching_attach_control(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
    matches: impl Fn(&AttachControl) -> bool,
) -> AttachControl {
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, async {
        while let Some(control) = control_rx.recv().await {
            if matches(&control) {
                return control;
            }
        }
        panic!("attach control channel closed while waiting for {context}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for attach control: {context}"))
}

async fn create_attached_session(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    handler
        .store_option_for_test(ScopeSelector::Global, OptionName::DefaultShell, "/bin/bash")
        .await;
    SessionSpec::create(handler, session).await;
    handler.attach_client(requester_pid, session).await
}

/// Creates the same attached session as [`create_attached_session`], with the
/// pane shell pinned to a UTF-8 locale.
///
/// A fixture that types a multi-byte character into a real shell cannot inherit
/// whatever `LC_CTYPE` the host account happens to export: a shell started in a
/// single-byte locale discards those bytes in its own line editor, before rmux
/// is ever observed handling them.
async fn create_attached_session_in_utf8_locale(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    handler
        .store_option_for_test(ScopeSelector::Global, OptionName::DefaultShell, "/bin/bash")
        .await;
    SessionSpec::create(
        handler,
        NewSessionRequest {
            environment: utf8_locale::fixture_environment(),
            ..Fixture::fixture(session)
        },
    )
    .await;
    handler.attach_client(requester_pid, session).await
}

#[tokio::test]
async fn web_render_refreshes_are_marked_pending_before_building_switches() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, "web-refresh-coalesce").await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach_with_access(
            77,
            session.clone(),
            None,
            AttachRegistration {
                render_stream: true,
                ..Fixture::fixture((control_tx, current_owner_uid()))
            },
        )
        .await
        .expect("attach registration succeeds");

    handler.refresh_attached_session(&session).await;
    handler.refresh_attached_session(&session).await;

    assert!(matches!(control_rx.try_recv(), Ok(AttachControl::Refresh)));
    assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach.by_pid.get(&77).expect("attach is active");
    assert!(active.render_refresh_pending);
}

#[tokio::test]
async fn refresh_attached_session_removes_clients_over_backlog_limit() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, "refresh-backlog").await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let control_backlog = Arc::new(AtomicUsize::new(
        super::attach_support::ATTACH_CONTROL_BACKLOG_LIMIT,
    ));
    let closing = Arc::new(AtomicBool::new(false));
    handler
        .register_attach_with_access(
            77,
            session.clone(),
            None,
            AttachRegistration {
                control_backlog: control_backlog.clone(),
                closing: closing.clone(),
                ..Fixture::fixture((control_tx, current_owner_uid()))
            },
        )
        .await
        .expect("attach registration succeeds");

    handler.refresh_attached_session(&session).await;

    assert!(closing.load(Ordering::SeqCst));
    assert!(!handler.active_attach.lock().await.by_pid.contains_key(&77));
    assert!(matches!(control_rx.try_recv(), Ok(AttachControl::Detach)));
    assert_eq!(
        control_backlog.load(Ordering::Acquire),
        super::attach_support::ATTACH_CONTROL_BACKLOG_LIMIT + 1,
        "saturation should enqueue only one accounted terminal detach sentinel"
    );
}

async fn create_line_exiting_attached_session(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    let marker = format!("RMUX_LINE_EXIT_READY_{}", std::process::id());
    SessionSpec::create(
        handler,
        NewSessionExtRequest {
            command: Some(line_exiting_command(&marker)),
            ..Fixture::fixture(session)
        },
    )
    .await;
    let target = PaneTarget::new(session.clone(), 0);
    wait_for_capture_containing(
        handler,
        target.clone(),
        &marker,
        "the attached-exit fixture should reach its input loop",
    )
    .await;
    handler
        .replace_transcript_for_test(&target, TerminalSize { cols: 80, rows: 24 }, b"")
        .await;
    handler.attach_client(requester_pid, session).await
}

async fn create_quiet_attached_session(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    SessionSpec::create(handler, Quiet(session)).await;
    handler.attach_client(requester_pid, session).await
}

fn line_exiting_command(marker: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "printf '%s\\n' '{marker}'; \
             while IFS= read -r line; do \
                 if [ \"$line\" = exit ] || [ \"$line\" = RMUX_EXIT ]; then \
                     printf 'logout\\n'; \
                     exit 0; \
                 fi; \
             done"
        ),
    ]
}

fn quiet_ready_command(marker: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("printf '{marker}\\n'; sleep 60"),
    ]
}

async fn active_panes(handler: &RequestHandler, session: &SessionName) -> String {
    let response = TestRequest::send_ok(
        handler,
        ListPanesRequest {
            target: session.clone(),
            format: Some("#{pane_index}:#{pane_active}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        },
    )
    .await;
    String::from_utf8(response.output.stdout().to_vec()).expect("list-panes stdout is utf-8")
}

async fn active_windows(handler: &RequestHandler, session: &SessionName) -> String {
    let response = TestRequest::send_ok(
        handler,
        ListWindowsRequest {
            target: session.clone(),
            format: Some("#{window_index}:#{window_active}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    String::from_utf8(response.output.stdout().to_vec()).expect("list-windows stdout is utf-8")
}

async fn current_layout(handler: &RequestHandler, session: &SessionName) -> LayoutName {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session)
        .expect("session exists")
        .window()
        .layout()
}

async fn select_layout(handler: &RequestHandler, session: &SessionName, layout: LayoutName) {
    assert!(matches!(
        handler
            .handle(Request::SelectLayout(SelectLayoutRequest {
                target: SelectLayoutTarget::Window(WindowTarget::new(session.clone())),
                layout,
            }))
            .await,
        Response::SelectLayout(_)
    ));
}

async fn pane_mode_status(handler: &RequestHandler, session: &SessionName) -> String {
    let response = TestRequest::send_ok(
        handler,
        ListPanesRequest {
            target: session.clone(),
            format: Some(
                "#{pane_in_mode}:#{pane_mode}:#{search_present}:#{selection_present}".to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        },
    )
    .await;
    String::from_utf8(response.output.stdout().to_vec()).expect("list-panes stdout is utf-8")
}

async fn display_target_format(
    handler: &RequestHandler,
    target: PaneTarget,
    format: &str,
) -> String {
    String::from_utf8(handler.display_print(target, format).await)
        .expect("display-message stdout is utf-8")
}

fn drain_attach_controls(control_rx: &mut mpsc::UnboundedReceiver<AttachControl>) {
    while control_rx.try_recv().is_ok() {}
}

fn bounded_unterminated_sgr_mouse_input() -> Vec<u8> {
    let mut bytes = b"\x1b[<".to_vec();
    bytes.resize(MAX_SGR_MOUSE_FRAME_BYTES, b'1');
    bytes
}

async fn recv_overlay_frame(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) -> String {
    let overlay = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let AttachControl::Overlay(overlay) = control_rx.recv().await.expect(context) {
                break overlay;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for overlay: {context}"));
    String::from_utf8_lossy(&overlay.frame).into_owned()
}

async fn capture_pane_print(handler: &RequestHandler, target: PaneTarget) -> String {
    let output = TestRequest::send_ok(handler, CapturePaneRequest::fixture(target))
        .await
        .output
        .expect("capture-pane -p should return command output");
    String::from_utf8(output.stdout().to_vec()).expect("capture-pane stdout is utf-8")
}

async fn wait_for_capture_containing(
    handler: &RequestHandler,
    target: PaneTarget,
    needle: &str,
    context: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let capture = capture_pane_print(handler, target.clone()).await;
        if capture.contains(needle) {
            return capture;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "{context}, got {capture:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

async fn prepare_attached_shell_prompt(handler: &RequestHandler, target: &PaneTarget) {
    for command in attached_shell_prompt_commands() {
        TestRequest::send_ok(
            handler,
            SendKeysRequest {
                target: target.clone(),
                keys: vec![command, "Enter".to_owned()],
            },
        )
        .await;
    }
    wait_for_capture_containing(
        handler,
        target.clone(),
        attached_shell_prompt_ready_needle(),
        "attached shell prompt must be ready",
    )
    .await;
}

fn attached_shell_prompt_commands() -> [String; 2] {
    ["export PS1='PROMPT> '", "clear"].map(str::to_owned)
}

fn attached_shell_prompt_ready_needle() -> &'static str {
    "PROMPT>"
}

async fn wait_for_session_removed(handler: &RequestHandler, session_name: &SessionName) {
    let deadline = tokio::time::Instant::now() + ATTACH_LIFECYCLE_TIMEOUT;
    loop {
        let exists = {
            let state = handler.state.lock().await;
            state.sessions.session(session_name).is_some()
        };
        if !exists {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for session {session_name} to be removed"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

/// Registers `requester_pid` on `session` as a writable client that declared `size`, then
/// reports that size, as a sized attach publishes itself.
async fn register_sized_attach(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
    size: TerminalSize,
) -> (u64, mpsc::UnboundedReceiver<AttachControl>) {
    register_sized_attach_with_flags(
        handler,
        requester_pid,
        session,
        size,
        ClientFlags::default(),
    )
    .await
}

async fn register_sized_attach_with_flags(
    handler: &RequestHandler,
    requester_pid: u32,
    session: &SessionName,
    size: TerminalSize,
    flags: ClientFlags,
) -> (u64, mpsc::UnboundedReceiver<AttachControl>) {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let attach_id = handler
        .register_attach_with_access(
            requester_pid,
            session.clone(),
            None,
            AttachRegistration {
                flags,
                client_size: Some(size),
                ..Fixture::fixture((control_tx, current_owner_uid()))
            },
        )
        .await
        .expect("attach registration succeeds");
    handler
        .handle_attached_resize(requester_pid, size)
        .await
        .expect("initial attached client size is accepted");
    (attach_id, control_rx)
}

/// A writable `attach-session -t session` that declares `client_size`, every other option off.
fn attach_session_ext2(
    session: &SessionName,
    client_size: TerminalSize,
) -> AttachSessionExt2Request {
    AttachSessionExt2Request {
        target: Some(session.clone()),
        target_spec: Some(session.to_string()),
        detach_other_clients: false,
        kill_other_clients: false,
        read_only: false,
        skip_environment_update: false,
        flags: None,
        working_directory: None,
        client_terminal: rmux_proto::ClientTerminalContext::default(),
        client_size: Some(client_size),
    }
}

/// [`attach_session_ext2`] as the request to dispatch.
fn attach_session_request(session: &SessionName, client_size: TerminalSize) -> Request {
    Request::AttachSessionExt2(Box::new(attach_session_ext2(session, client_size)))
}

async fn set_vi_mode_keys(handler: &RequestHandler, session: &SessionName) {
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(session.clone(), 0)),
            OptionName::ModeKeys,
            "vi",
        )
        .await;
}

async fn table_references(handler: &RequestHandler, table_name: &str) -> Option<usize> {
    handler
        .state
        .lock()
        .await
        .key_bindings
        .table(table_name)
        .map(|table| table.references())
}

/// Reads one session's current position in the recency order.
async fn session_recency(
    handler: &RequestHandler,
    session: &SessionName,
) -> rmux_core::SessionRecency {
    handler
        .state
        .lock()
        .await
        .sessions
        .session(session)
        .expect("session exists")
        .recency()
}

use super::input_capture::RawPaneInputProbe;

#[path = "handler_attach_tests/utf8_locale.rs"]
mod utf8_locale;

#[path = "handler_attach_tests/lifecycle.rs"]
mod lifecycle;

#[path = "handler_attach_tests/attached_help.rs"]
mod attached_help;
#[path = "handler_attach_tests/prefix_navigation.rs"]
mod prefix_navigation;

#[path = "handler_attach_tests/display_panes.rs"]
mod display_panes;
#[path = "handler_attach_tests/display_panes_identity.rs"]
mod display_panes_identity;

#[path = "handler_attach_tests/copy_mode_keys.rs"]
mod copy_mode_keys;

#[path = "handler_attach_tests/copy_mode_render.rs"]
mod copy_mode_render;

#[path = "handler_attach_tests/copy_mode_motion.rs"]
mod copy_mode_motion;

#[path = "handler_attach_tests/copy_mode_search.rs"]
mod copy_mode_search;

#[path = "handler_attach_tests/copy_mode_selection_yank.rs"]
mod copy_mode_selection_yank;

#[path = "handler_attach_tests/mode_tree_clock.rs"]
mod mode_tree_clock;

#[path = "handler_attach_tests/attach_mutations.rs"]
mod attach_mutations;

#[path = "handler_attach_tests/attach_render.rs"]
mod attach_render;

#[path = "handler_attach_tests/attaching_identity.rs"]
mod attaching_identity;
#[path = "handler_attach_tests/set_titles.rs"]
mod set_titles;
#[path = "handler_attach_tests/set_titles_client_context.rs"]
mod set_titles_client_context;
#[path = "handler_attach_tests/set_titles_generation.rs"]
mod set_titles_generation;
#[path = "handler_attach_tests/set_titles_overlay.rs"]
mod set_titles_overlay;
#[path = "handler_attach_tests/set_titles_path.rs"]
mod set_titles_path;
#[path = "handler_attach_tests/set_titles_support.rs"]
mod set_titles_support;
#[path = "handler_attach_tests/set_titles_switch.rs"]
mod set_titles_switch;

#[path = "handler_attach_tests/attached_prefix_lifecycle.rs"]
mod attached_prefix_lifecycle;

#[path = "handler_attach_tests/key_table_timer_shutdown.rs"]
mod key_table_timer_shutdown;

#[path = "handler_attach_tests/key_table_identity_regressions.rs"]
mod key_table_identity_regressions;

#[path = "handler_attach_tests/cleanup_identity.rs"]
mod cleanup_identity;
#[path = "handler_attach_tests/client_name_lifecycle.rs"]
mod client_name_lifecycle;
#[path = "handler_attach_tests/multi_client.rs"]
mod multi_client;
#[path = "handler_attach_tests/resize_selection_race.rs"]
mod resize_selection_race;
#[path = "handler_attach_tests/window_geometry.rs"]
mod window_geometry;

#[path = "handler_attach_tests/server_lifecycle.rs"]
mod server_lifecycle;

#[path = "handler_attach_tests/lock_identity_regressions.rs"]
mod lock_identity_regressions;

#[path = "handler_attach_tests/client_security.rs"]
mod client_security;

#[path = "handler_attach_tests/attached_count_identity.rs"]
mod attached_count_identity;

#[path = "handler_attach_tests/session_recency.rs"]
mod session_recency;

#[path = "handler_attach_tests/session_recency_input.rs"]
mod session_recency_input;
#[path = "handler_attach_tests/sizeless_geometry.rs"]
mod sizeless_geometry;
#[path = "handler_attach_tests/switch_frame_geometry.rs"]
mod switch_frame_geometry;
#[path = "handler_attach_tests/switch_latest_recency.rs"]
mod switch_latest_recency;

#[path = "handler_attach_tests/combined_switch_recency.rs"]
mod combined_switch_recency;

#[path = "handler_attach_tests/combined_registration_recency.rs"]
mod combined_registration_recency;
