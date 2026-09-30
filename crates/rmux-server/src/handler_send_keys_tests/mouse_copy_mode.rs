use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

#[tokio::test]
async fn copy_mode_mouse_drag_start_anchors_on_press_cell() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = std::process::id();
    let target = PaneTarget::new(alpha.clone(), 0);

    SessionSpec::create(&handler, (&alpha, TerminalSize::new(20, 5))).await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;

    let (window_id, pane_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window.pane(0).expect("pane exists");
        (window.id(), pane.id())
    };

    {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&requester_pid)
            .expect("attached client exists");
        active.mouse.current_event = Some(AttachedMouseEvent {
            raw: MouseForwardEvent {
                b: 32,
                lb: 0,
                x: 6,
                y: 1,
                lx: 1,
                ly: 1,
                sgr_b: 32,
                sgr_type: 'M',
                ignore: false,
            },
            session_id: 0,
            window_id: Some(window_id.as_u32()),
            pane_id: Some(pane_id),
            pane_target: Some(target.clone()),
            location: MouseLocation::Pane,
            status_at: None,
            status_lines: 0,
            ignore: false,
        });
    }

    TestRequest::send_ok(
        &handler,
        CopyModeRequest {
            mouse_drag_start: true,
            ..Fixture::fixture(target)
        },
    )
    .await;

    let summary = {
        let state = handler.state.lock().await;
        state
            .pane_copy_mode_summary(&alpha, pane_id)
            .expect("copy mode summary")
    };
    let selection_start = summary
        .selection_start
        .expect("copy-mode -M should set a selection anchor");
    assert_eq!(
        selection_start.x, 1,
        "copy-mode -M must anchor at the press cell, not the first drag cell"
    );
}

#[tokio::test]
async fn copy_mode_single_motion_drag_copies_from_press_to_motion_cell() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = std::process::id();
    let target = PaneTarget::new(alpha.clone(), 0);
    let size = TerminalSize { cols: 20, rows: 5 };

    SessionSpec::create_started(
        &handler,
        NewSessionExtRequest {
            size: Some(size),
            command: Some(quiet_command()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;
    handler
        .replace_transcript_for_test(&target, size, b"ABCDEF\r\n")
        .await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;

    let (window_id, pane_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window.pane(0).expect("pane exists");
        (window.id(), pane.id())
    };

    {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&requester_pid)
            .expect("attached client exists");
        active.mouse.current_event = Some(AttachedMouseEvent {
            raw: MouseForwardEvent {
                b: 32,
                lb: 0,
                x: 1,
                y: 0,
                lx: 0,
                ly: 0,
                sgr_b: 32,
                sgr_type: 'M',
                ignore: false,
            },
            session_id: 0,
            window_id: Some(window_id.as_u32()),
            pane_id: Some(pane_id),
            pane_target: Some(target.clone()),
            location: MouseLocation::Pane,
            status_at: None,
            status_lines: 0,
            ignore: false,
        });
    }

    TestRequest::send_ok(
        &handler,
        CopyModeRequest {
            mouse_drag_start: true,
            ..Fixture::fixture(&target)
        },
    )
    .await;

    let copied = handler
        .handle(Request::SendKeysExt(SendKeysExtRequest {
            dispatch_key_table: false,
            copy_mode_command: true,
            ..Fixture::fixture((target, ["copy-selection"]))
        }))
        .await;
    assert!(matches!(
        copied,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    ));

    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest { name: None }))
        .await;
    let Response::ShowBuffer(response) = shown else {
        panic!("expected show-buffer response, got {shown:?}");
    };
    assert_eq!(
        response.command_output().stdout(),
        b"A",
        "a quick one-motion mouse drag from A to B should copy A like tmux"
    );
}

#[tokio::test]
async fn copy_mode_mouse_entry_uses_left_scrollbar_content_origin() {
    let handler = RequestHandler::new();
    let alpha = session_name("copy-mode-left-scrollbar");
    let requester_pid = std::process::id();
    let target = PaneTarget::new(alpha.clone(), 0);
    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            size: Some(TerminalSize { cols: 20, rows: 5 }),
            command: Some(quiet_command()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;
    for (option, value) in [
        (OptionName::PaneScrollbars, "on"),
        (OptionName::PaneScrollbarsPosition, "left"),
        (OptionName::PaneScrollbarsStyle, "width=2,pad=1"),
    ] {
        handler
            .set_option(
                ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
                option,
                value,
            )
            .await;
    }
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    handler
        .replace_transcript_for_test(&target, TerminalSize { cols: 17, rows: 4 }, b"ABCDEF\r\n")
        .await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;
    let (window_id, pane_id) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .expect("session exists")
            .window();
        (window.id(), window.pane(0).expect("pane exists").id())
    };
    {
        let mut active_attach = handler.active_attach.lock().await;
        active_attach
            .by_pid
            .get_mut(&requester_pid)
            .expect("attached client exists")
            .mouse
            .current_event = Some(AttachedMouseEvent {
            raw: MouseForwardEvent {
                b: 32,
                lb: 0,
                // The two track cells and one pad cell precede content.
                x: 4,
                y: 0,
                lx: 3,
                ly: 0,
                sgr_b: 32,
                sgr_type: 'M',
                ignore: false,
            },
            session_id: 0,
            window_id: Some(window_id.as_u32()),
            pane_id: Some(pane_id),
            pane_target: Some(target.clone()),
            location: MouseLocation::Pane,
            status_at: None,
            status_lines: 0,
            ignore: false,
        });
    }

    TestRequest::send_ok(
        &handler,
        CopyModeRequest {
            mouse_drag_start: true,
            ..Fixture::fixture(&target)
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SendKeysExtRequest {
            dispatch_key_table: false,
            copy_mode_command: true,
            ..Fixture::fixture((target, ["copy-selection"]))
        },
    )
    .await;
    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest { name: None }))
        .await;
    let Response::ShowBuffer(response) = shown else {
        panic!("expected show-buffer response, got {shown:?}");
    };
    assert_eq!(
        response.command_output().stdout(),
        b"A",
        "the first content cell after a left scrollbar must remain copy-mode x=0"
    );
}
