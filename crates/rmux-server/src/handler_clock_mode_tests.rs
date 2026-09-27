use super::RequestHandler;
use crate::control::ControlServerEvent;
use crate::pane_io::AttachControl;
use crate::test_fixtures::Fixture;
use rmux_core::{input::mode, GridRenderOptions, ScreenCaptureRange};
use rmux_proto::request::NewSessionExtRequest;
use rmux_proto::{
    ClockModeRequest, DisplayMessageRequest, HookName, ListPanesRequest, OptionName, PaneTarget,
    RefreshClientRequest, Request, Response, ScopeSelector, SetHookRequest, ShowBufferRequest,
    Target, TerminalSize, WindowTarget,
};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

pub(super) async fn create_session(
    handler: &RequestHandler,
    name: &str,
    size: TerminalSize,
) -> PaneTarget {
    let ready_marker = "RCREADY";
    let session_name = handler
        .create_session(NewSessionExtRequest {
            size: Some(size),
            command: Some(quiet_clock_command(ready_marker)),
            ..Fixture::fixture(name)
        })
        .await;
    let target = PaneTarget::with_window(session_name, 0, 0);
    wait_for_transcript_containing(
        handler,
        &target,
        ready_marker,
        "quiet clock fixture should reach a stable frame",
    )
    .await;
    handler
        .replace_transcript_for_test(&target, size, b"")
        .await;
    target
}

fn quiet_clock_command(marker: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("printf '{marker}\\n'; sleep 60"),
    ]
}

async fn wait_for_transcript_containing(
    handler: &RequestHandler,
    target: &PaneTarget,
    needle: &str,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let capture = capture_transcript(handler, target).await;
        if capture.contains(needle) {
            return;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "{context}, got {capture:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn capture_transcript(handler: &RequestHandler, target: &PaneTarget) -> String {
    let transcript = {
        let state = handler.state.lock().await;
        state
            .transcript_handle(target)
            .expect("pane transcript exists")
    };
    let capture = transcript
        .lock()
        .expect("pane transcript mutex must not be poisoned")
        .capture_main(ScreenCaptureRange::default(), GridRenderOptions::default());
    String::from_utf8_lossy(&capture).into_owned()
}

async fn dispatch_as(handler: &RequestHandler, requester_pid: u32, request: Request) -> Response {
    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    let outcome = handler.dispatch(requester_pid, request).await;
    handler
        .drain_lifecycle_hooks_for_test(&mut lifecycle_events)
        .await;
    outcome.response
}

fn drain_control_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(ControlServerEvent::Notification(line)) => lines.push(line),
            Ok(
                ControlServerEvent::SessionChanged(_)
                | ControlServerEvent::SessionChangedAt { .. }
                | ControlServerEvent::Refresh,
            ) => {}
            Ok(ControlServerEvent::Exit(reason)) => {
                panic!("unexpected control exit: {reason:?}");
            }
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
    lines
}

async fn pane_id(handler: &RequestHandler, target: &PaneTarget) -> u32 {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .and_then(|window| window.pane(target.pane_index()))
        .expect("pane exists")
        .id()
        .as_u32()
}

async fn list_panes_text(handler: &RequestHandler, target: &PaneTarget, format: &str) -> String {
    let response = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: target.session_name().clone(),
            format: Some(format.to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })))
        .await;
    let output = response
        .command_output()
        .expect("list-panes returns command output");
    String::from_utf8_lossy(output.stdout()).into_owned()
}

async fn next_overlay(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) -> crate::pane_io::OverlayFrame {
    loop {
        match control_rx.recv().await {
            Some(AttachControl::Overlay(frame)) if frame.frame.is_empty() => {}
            Some(AttachControl::Overlay(frame)) => return frame,
            Some(AttachControl::AdvancePersistentOverlayState(_)) => {}
            Some(AttachControl::Switch(_)) => {}
            Some(AttachControl::Refresh) => {}
            Some(AttachControl::InteractiveInput) => {}
            Some(AttachControl::Detach) => panic!("unexpected detach"),
            Some(AttachControl::Exited) => panic!("unexpected exited"),
            Some(AttachControl::DetachKill) => panic!("unexpected detach kill"),
            Some(AttachControl::DetachExecShellCommand(_)) => panic!("unexpected detach exec"),
            Some(AttachControl::Write(_)) => {}
            Some(AttachControl::ClipboardWrite { .. }) => {}
            Some(AttachControl::LockShellCommand(_)) => {}
            Some(AttachControl::Suspend) => panic!("unexpected suspend"),
            None => panic!("attach control closed"),
        }
    }
}

async fn next_transient_overlay(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) -> crate::pane_io::OverlayFrame {
    loop {
        let frame = next_overlay(control_rx).await;
        if !frame.persistent {
            return frame;
        }
    }
}

async fn next_transient_overlay_matching(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    description: &str,
    mut matches: impl FnMut(&str) -> bool,
) -> crate::pane_io::OverlayFrame {
    let mut seen = Vec::new();
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let frame = next_transient_overlay(control_rx).await;
            let text = String::from_utf8_lossy(&frame.frame);
            if matches(&text) {
                return frame;
            }
            seen.push(text.into_owned());
        }
    })
    .await;
    match result {
        Ok(frame) => frame,
        Err(_) => panic!("timed out waiting for {description}; seen frames: {seen:?}"),
    }
}

#[tokio::test]
async fn clock_mode_overlay_uses_window_options_for_fallback_rendering() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "alpha", TerminalSize { cols: 11, rows: 5 }).await;
    let session = target.session_name().clone();
    let requester_pid = std::process::id();
    let mut control_rx = handler.attach_client(requester_pid, &session).await;

    for (option, value) in [
        (OptionName::ClockModeColour, "red"),
        (OptionName::ClockModeStyle, "12"),
    ] {
        handler
            .set_option(
                ScopeSelector::Window(WindowTarget::new(session.clone())),
                option,
                value,
            )
            .await;
    }

    let response = handler
        .handle(Request::ClockMode(ClockModeRequest {
            target: Some(target.clone()),
        }))
        .await;
    assert_eq!(
        response,
        Response::ClockMode(rmux_proto::ClockModeResponse {
            target: target.clone(),
            active: true,
        })
    );

    let overlay = next_overlay(&mut control_rx).await;
    let frame = String::from_utf8(overlay.frame).expect("overlay is utf-8");
    assert!(overlay.persistent);
    assert!(frame.contains("\u{1b}[?25l"));
    assert!(frame.contains("\u{1b}[31m"));
    assert!(frame.contains("AM") || frame.contains("PM"));
}

#[tokio::test]
async fn clock_tick_keeps_an_active_transient_message_visible() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "clock-transient-message",
        TerminalSize { cols: 24, rows: 8 },
    )
    .await;
    let session_name = target.session_name().clone();
    let requester_pid = std::process::id();
    let mut control_rx = handler.attach_client(requester_pid, &session_name).await;

    handler
        .handle_ok(ClockModeRequest {
            target: Some(target),
        })
        .await;
    let _ = next_overlay(&mut control_rx).await;
    while control_rx.try_recv().is_ok() {}

    handler
        .handle_ok(DisplayMessageRequest {
            target: Some(Target::Session(session_name.clone())),
            print: false,
            ..Fixture::fixture("still-visible")
        })
        .await;
    let message = next_transient_overlay(&mut control_rx).await;
    assert!(String::from_utf8_lossy(&message.frame).contains("still-visible"));

    handler
        .refresh_clock_overlays_for_session(&session_name)
        .await;
    let refreshed = next_overlay(&mut control_rx).await;
    assert!(refreshed.persistent);
    assert!(
        String::from_utf8_lossy(&refreshed.frame).contains("still-visible"),
        "clock refresh must compose the still-active status message"
    );
}

#[tokio::test]
async fn refresh_client_replays_active_clock_after_base_switch() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "refresh-client-clock",
        TerminalSize { cols: 20, rows: 8 },
    )
    .await;
    let requester_pid = std::process::id();
    let mut control_rx = handler
        .attach_client(requester_pid, target.session_name())
        .await;
    handler
        .handle_ok(ClockModeRequest {
            target: Some(target),
        })
        .await;
    let _ = next_overlay(&mut control_rx).await;
    while control_rx.try_recv().is_ok() {}

    handler.handle_ok(RefreshClientRequest::fixture(None)).await;

    let mut saw_switch = false;
    let mut replayed_clock = None;
    while let Ok(control) = control_rx.try_recv() {
        match control {
            AttachControl::Switch(_) => saw_switch = true,
            AttachControl::Overlay(frame) if saw_switch && frame.persistent => {
                replayed_clock = Some(frame);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_switch, "refresh-client must queue the base Switch");
    let frame = replayed_clock.expect("clock overlay must be queued after the base Switch");
    let rendered = String::from_utf8(frame.frame).expect("clock frame is utf-8");
    assert!(rendered.contains("\u{1b}[?25l"));
}

#[tokio::test]
async fn clock_mode_updates_pane_formats_and_exits_on_any_keypress() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "alpha", TerminalSize { cols: 32, rows: 8 }).await;
    let requester_pid = std::process::id();
    let mut control_rx = handler
        .attach_client(requester_pid, target.session_name())
        .await;

    handler
        .handle_ok(ClockModeRequest {
            target: Some(target.clone()),
        })
        .await;
    let _ = next_overlay(&mut control_rx).await;

    assert_eq!(
        list_panes_text(&handler, &target, "#{pane_in_mode} #{pane_mode}").await,
        "1 clock-mode\n"
    );

    handler
        .handle_attached_live_input_for_test(requester_pid, b"x")
        .await
        .expect("attached input succeeds");

    assert_eq!(
        list_panes_text(&handler, &target, "#{pane_in_mode} #{pane_mode}").await,
        "0 \n"
    );
}

#[tokio::test]
async fn clock_mode_exit_restores_underlying_hidden_cursor_state() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "alpha", TerminalSize { cols: 32, rows: 8 }).await;
    let requester_pid = std::process::id();
    let mut control_rx = handler
        .attach_client(requester_pid, target.session_name())
        .await;

    {
        let state = handler.state.lock().await;
        let transcript = state
            .transcript_handle(&target)
            .expect("pane transcript exists");
        let mut transcript = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned");
        transcript.append_bytes(b"\x1b[?25l");
    }
    {
        let state = handler.state.lock().await;
        let pane_id = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .and_then(|window| window.pane(target.pane_index()))
            .expect("pane exists")
            .id();
        let screen = state
            .pane_screen_state(target.session_name(), pane_id)
            .expect("pane screen state exists");
        assert_eq!(screen.mode & mode::MODE_CURSOR, 0);
    }

    handler
        .handle_ok(ClockModeRequest {
            target: Some(target.clone()),
        })
        .await;
    let _ = next_overlay(&mut control_rx).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"x")
        .await
        .expect("attached input succeeds");

    let restore = next_transient_overlay_matching(
        &mut control_rx,
        "clock mode restore frame with hidden cursor",
        |frame| frame.contains("\u{1b}[?25l") && !frame.contains("\u{1b}[?25h"),
    )
    .await;
    let frame = String::from_utf8(restore.frame).expect("restore frame is utf-8");
    assert!(frame.contains("\u{1b}[?25l"));
    assert!(!frame.contains("\u{1b}[?25h"));
}

#[tokio::test]
async fn clock_mode_exit_restores_visible_line_content() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "alpha", TerminalSize { cols: 16, rows: 3 }).await;
    let requester_pid = std::process::id();
    let mut control_rx = handler
        .attach_client(requester_pid, target.session_name())
        .await;

    {
        let state = handler.state.lock().await;
        let transcript = state
            .transcript_handle(&target)
            .expect("pane transcript exists");
        let mut transcript = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned");
        transcript.append_bytes(b"\x1b[31mred\r\nmore");
    }

    handler
        .handle_ok(ClockModeRequest {
            target: Some(target.clone()),
        })
        .await;
    let _ = next_overlay(&mut control_rx).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"q")
        .await
        .expect("attached input succeeds");

    let restore = next_transient_overlay_matching(
        &mut control_rx,
        "clock mode restore frame with visible line content",
        |frame| frame.contains("red") && frame.contains("more"),
    )
    .await;
    let frame = String::from_utf8(restore.frame).expect("restore frame is utf-8");
    assert!(frame.contains("red"));
    assert!(frame.contains("more"));
}

#[tokio::test]
async fn clock_mode_fires_hooks_and_control_notifications_on_entry_and_exit() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "alpha", TerminalSize { cols: 24, rows: 7 }).await;
    let requester_pid = std::process::id();
    let _control_rx = handler
        .attach_client(requester_pid, target.session_name())
        .await;
    handler
        .refresh_automatic_window_name_for_pane_target(&target)
        .await;
    let (_, mut notifications) = handler
        .register_utf8_control_for_test(700, Some(target.session_name()))
        .await;
    let _ = drain_control_notifications(&mut notifications);
    let (window_id, initial_window_name) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .expect("clock-mode window exists");
        (
            window.id().as_u32(),
            window.name().unwrap_or_default().to_owned(),
        )
    };

    handler
        .handle_ok(SetHookRequest::fixture((
            ScopeSelector::Pane(target.clone()),
            HookName::PaneModeChanged,
            "set-buffer -b pane-mode-hook ok",
        )))
        .await;

    let response = dispatch_as(
        &handler,
        requester_pid,
        Request::ClockMode(ClockModeRequest {
            target: Some(target.clone()),
        }),
    )
    .await;
    assert!(matches!(response, Response::ClockMode(_)));

    let pane_id = pane_id(&handler, &target).await;
    assert_eq!(
        drain_control_notifications(&mut notifications),
        vec![
            format!("%window-renamed @{window_id} [tmux]"),
            format!("%pane-mode-changed %{pane_id}"),
            "%paste-buffer-changed pane-mode-hook".to_owned(),
        ]
    );

    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("pane-mode-hook".to_owned()),
        }))
        .await;
    let Response::ShowBuffer(buffer) = shown else {
        panic!("expected show-buffer response");
    };
    assert_eq!(buffer.command_output().stdout(), b"ok");

    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    handler
        .handle_attached_live_input_for_test(requester_pid, b"q")
        .await
        .expect("attached input succeeds");
    handler
        .drain_lifecycle_hooks_for_test(&mut lifecycle_events)
        .await;

    assert_eq!(
        drain_control_notifications(&mut notifications),
        vec![
            format!("%window-renamed @{window_id} {initial_window_name}"),
            format!("%pane-mode-changed %{pane_id}"),
            "%paste-buffer-changed pane-mode-hook".to_owned(),
        ]
    );
}
