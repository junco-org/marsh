use super::*;

use super::super::mode_tree_model::{
    ChooseTreeTarget, ModeTreeActionIdentity, ModeTreeDeferredAction,
};
use crate::handler::prompt_support::PromptInputEvent;
use rmux_proto::{
    CopyModeRequest, DeleteBufferRequest, HookName, KillSessionRequest, KillWindowRequest,
    LinkWindowRequest, MoveWindowRequest, NewWindowRequest, RefreshClientRequest,
    RenameSessionRequest, ResizePaneRequest, RespawnPaneRequest, RotateWindowRequest,
    SetBufferRequest, SetHookRequest, WindowTarget,
};

async fn choose_buffer_action_fixture(
    label: &str,
    attach_pid_offset: u32,
) -> (
    RequestHandler,
    u32,
    mpsc::UnboundedReceiver<crate::pane_io::AttachControl>,
) {
    let handler = RequestHandler::new();
    let session_name = SessionName::new(label).expect("valid session");
    handler.create_session(&session_name).await;
    for (name, content) in [
        ("stale", b"old".to_vec()),
        ("delete-me", b"delete".to_vec()),
        ("keep", b"safe".to_vec()),
    ] {
        handler
            .handle_ok(SetBufferRequest::fixture((name, content)))
            .await;
    }

    let attach_pid = std::process::id().saturating_add(attach_pid_offset);
    let control_rx = handler.attach_client(attach_pid, session_name).await;
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    (handler, attach_pid, control_rx)
}

async fn zoomed_choose_tree_fixture(
    label: &str,
    attach_pid_offset: u32,
) -> (
    RequestHandler,
    SessionName,
    u32,
    mpsc::UnboundedReceiver<crate::pane_io::AttachControl>,
) {
    let handler = RequestHandler::new();
    let session_name = SessionName::new(label).expect("valid session");
    handler.create_session(&session_name).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(session_name.clone(), 0, 0))
        })
        .await;
    let attach_pid = std::process::id().saturating_add(attach_pid_offset);
    let control_rx = handler.attach_client(attach_pid, &session_name).await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-Z"]).await;
    assert!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(0))
            .is_some_and(rmux_core::Window::is_zoomed),
        "choose-tree -Z zooms the host window"
    );
    (handler, session_name, attach_pid, control_rx)
}

fn mouse_event_at_row(y: u16) -> crate::input_keys::MouseForwardEvent {
    mouse_event_at(0, y)
}

fn mouse_event_at(x: u16, y: u16) -> crate::input_keys::MouseForwardEvent {
    crate::input_keys::MouseForwardEvent {
        b: 0,
        lb: 0,
        x,
        y,
        lx: x,
        ly: y,
        sgr_b: 0,
        sgr_type: 'M',
        ignore: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mode_tree_build_and_mouse_use_the_host_split_geometry() {
    let label = "choose-buffer-split-geometry";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, mut control_rx) = choose_buffer_action_fixture(label, 81).await;
    // A real attached client reads its control channel, and this one is about to acquire three
    // panes whose prompts each draw at it. The sender bounds a client's *pending deep render
    // switches* at four and closes a client that exceeds them, so a fixture that never reads is
    // evicted part-way through — which is what the "choose-buffer remains active" failure was
    // actually reporting: not a dismissed mode tree, a removed client.
    let _drain = tokio::spawn(async move { while control_rx.recv().await.is_some() {} });
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(session_name.clone(), 0, 0))
        })
        .await;

    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Vertical,
            ..Fixture::fixture(PaneTarget::with_window(session_name.clone(), 0, 0))
        })
        .await;

    let mut mode = with_mode_tree(&handler, attach_pid, |mode| mode.clone()).await;
    mode.preview_mode = PreviewMode::Off;
    mode.scroll = 0;
    mode.selected_id = None;
    let build = handler
        .build_mode_tree(&mut mode, attach_pid)
        .await
        .expect("choose-buffer rebuild succeeds");
    let geometry = handler
        .mode_tree_content_geometry(&mode)
        .await
        .expect("host geometry resolves");
    assert!(geometry.cols() < 80, "the host must remain split");
    assert!(geometry.rows() < 23, "the host height must remain split");
    assert_eq!(mode.last_list_rows, usize::from(geometry.rows()));
    let first = build.visible.first().cloned().expect("first content row");
    handler
        .store_mode_tree_state(attach_pid, mode)
        .await
        .expect("mode-tree state stores");

    let outside_x = geometry.x().saturating_add(geometry.cols());
    assert!(!handler
        .handle_mode_tree_mouse_event(attach_pid, mouse_event_at(outside_x, geometry.y()))
        .await
        .expect("adjacent-pane click is ignored"));
    assert!(handler
        .handle_mode_tree_mouse_event(attach_pid, mouse_event_at(geometry.x(), geometry.y()),)
        .await
        .expect("host-pane click succeeds"));
    assert_eq!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&attach_pid)
            .and_then(|active| active.mode_tree.as_ref())
            .and_then(|mode| mode.selected_id.clone()),
        Some(first)
    );
}

#[tokio::test]
async fn mode_tree_reserves_numeric_status_at_top_and_bottom() {
    let label = "choose-buffer-multi-line-status";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _control_rx) = choose_buffer_action_fixture(label, 79).await;
    handler
        .set_option(ScopeSelector::Global, OptionName::Status, "3")
        .await;

    let mut mode = with_mode_tree(&handler, attach_pid, |mode| mode.clone()).await;
    mode.preview_mode = PreviewMode::Off;
    let build = handler
        .build_mode_tree(&mut mode, attach_pid)
        .await
        .expect("choose-buffer rebuild succeeds");

    assert_eq!(mode.last_list_rows, 21);
    assert_eq!(
        handler
            .mode_tree_content_rows(&mode)
            .await
            .expect("mode-tree geometry resolves"),
        21
    );
    let overlay = {
        let state = handler.state.lock().await;
        render_mode_tree_overlay(&state, &mode, &build)
    };
    assert!(frame_visits_row(&overlay, 21));
    assert!(!frame_visits_row(&overlay, 22));

    let clear = handler
        .mode_tree_overlay_clear_frame_for_test(&session_name)
        .await
        .expect("mode-tree clear frame resolves");
    assert!(frame_visits_row(&clear, 21));
    assert!(!frame_visits_row(&clear, 22));

    handler
        .set_option(ScopeSelector::Global, OptionName::StatusPosition, "top")
        .await;
    let overlay = {
        let state = handler.state.lock().await;
        render_mode_tree_overlay(&state, &mode, &build)
    };
    for status_row in 1..=3 {
        assert!(!frame_visits_row(&overlay, status_row));
    }
    assert!(frame_visits_row(&overlay, 4));
    assert!(frame_visits_row(&overlay, 24));

    let clear = handler
        .mode_tree_overlay_clear_frame_for_test(&session_name)
        .await
        .expect("mode-tree clear frame resolves");
    for status_row in 1..=3 {
        assert!(!frame_visits_row(&clear, status_row));
    }
    assert!(frame_visits_row(&clear, 4));
    assert!(frame_visits_row(&clear, 24));
}

#[tokio::test]
async fn mode_tree_mouse_uses_content_rows_below_top_status() {
    let label = "choose-buffer-top-status-mouse";
    let (handler, attach_pid, _control_rx) = choose_buffer_action_fixture(label, 80).await;
    handler
        .set_option(ScopeSelector::Global, OptionName::Status, "3")
        .await;
    handler
        .set_option(ScopeSelector::Global, OptionName::StatusPosition, "top")
        .await;

    for index in 0..24 {
        handler
            .handle_ok(SetBufferRequest::fixture((
                format!("mouse-row-{index:02}"),
                vec![u8::try_from(index).expect("test index fits in u8")],
            )))
            .await;
    }

    let mut mode = with_mode_tree(&handler, attach_pid, |mode| mode.clone()).await;
    mode.preview_mode = PreviewMode::Off;
    mode.scroll = 0;
    mode.selected_id = None;
    let build = handler
        .build_mode_tree(&mut mode, attach_pid)
        .await
        .expect("choose-buffer rebuild succeeds");
    let expected_first = build.visible.first().cloned().expect("first content row");
    let expected_last = build.visible.get(20).cloned().expect("last content row");
    mode.selected_id = Some(expected_first.clone());
    handler
        .store_mode_tree_state(attach_pid, mode)
        .await
        .expect("mode-tree state stores");

    assert!(!handler
        .handle_mode_tree_mouse_event(attach_pid, mouse_event_at_row(2))
        .await
        .expect("status-row click is ignored"));
    assert!(handler
        .handle_mode_tree_mouse_event(attach_pid, mouse_event_at_row(3))
        .await
        .expect("first content-row click succeeds"));
    assert_eq!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&attach_pid)
            .and_then(|active| active.mode_tree.as_ref())
            .and_then(|mode| mode.selected_id.clone()),
        Some(expected_first)
    );

    assert!(handler
        .handle_mode_tree_mouse_event(attach_pid, mouse_event_at_row(23))
        .await
        .expect("last content-row click succeeds"));
    assert_eq!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&attach_pid)
            .and_then(|active| active.mode_tree.as_ref())
            .and_then(|mode| mode.selected_id.clone()),
        Some(expected_last)
    );
}

async fn replace_stale_buffer(handler: &RequestHandler) {
    handler
        .handle_ok(DeleteBufferRequest {
            name: Some("stale".to_owned()),
        })
        .await;
    handler
        .handle_ok(SetBufferRequest::fixture((
            "stale",
            b"replacement".to_vec(),
        )))
        .await;
}

async fn choose_buffer_item_id(
    handler: &RequestHandler,
    attach_pid: u32,
    buffer_name: &str,
) -> String {
    let mut mode = with_mode_tree(handler, attach_pid, |mode| mode.clone()).await;
    handler
        .build_mode_tree(&mut mode, attach_pid)
        .await
        .expect("choose-buffer rebuild succeeds")
        .items
        .values()
        .find(|item| {
            matches!(
                &item.action,
                ModeTreeAction::Buffer { name, .. } if name == buffer_name
            )
        })
        .map(|item| item.id.clone())
        .expect("buffer item exists")
}

#[tokio::test]
async fn choose_buffer_accept_rejects_a_reconnected_host_before_paste() {
    let label = "choose-buffer-host-generation-write";
    let session_name = SessionName::new(label).expect("valid session");
    let target = rmux_proto::PaneTarget::with_window(session_name.clone(), 0, 0);
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 70).await;
    handler.start_attached_input_capture_for_test(&target).await;
    let keep_id = choose_buffer_item_id(&handler, attach_pid, "keep").await;
    let old_identity = {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&attach_pid)
            .expect("original host remains attached");
        active
            .mode_tree
            .as_mut()
            .expect("original choose-buffer remains active")
            .selected_id = Some(keep_id);
        super::super::mode_tree_model::ModeTreeActionIdentity::new(
            attach_pid,
            active.id,
            active.mode_tree_state_id,
        )
    };

    let pause =
        super::super::mode_tree_buffer_actions::install_mode_tree_buffer_paste_pause(attach_pid);
    let accepting_handler = handler.clone();
    let accept = tokio::spawn(async move {
        accepting_handler
            .accept_mode_tree_selection(attach_pid)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("accept reaches the pre-write identity pause");

    let (replacement_tx, mut replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    assert_ne!(replacement_attach_id, old_identity.attach_id());
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    let replacement_state_id = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("replacement host is attached")
        .mode_tree_state_id;
    while replacement_rx.try_recv().is_ok() {}

    pause.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), accept)
        .await
        .expect("stale accept completes")
        .expect("stale accept task joins");
    assert!(result.is_err(), "stale host generation must fail closed");
    assert_eq!(
        handler.attached_input_capture_for_test(&target).await,
        Some(Vec::new()),
        "the stale host must not paste into the pane"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("replacement host survives");
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.mode_tree_state_id, replacement_state_id);
    assert!(
        replacement.mode_tree.is_some(),
        "the stale accept must not dismiss the replacement tree"
    );
    drop(active_attach);
    while let Ok(control) = replacement_rx.try_recv() {
        assert!(
            !matches!(
                control,
                crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
            ),
            "the stale accept advanced the replacement overlay state"
        );
    }
}

#[tokio::test]
async fn choose_buffer_p_key_rejects_a_same_pid_reconnect_before_paste() {
    let label = "choose-buffer-p-key-host-generation";
    let session_name = SessionName::new(label).expect("valid session");
    let target = rmux_proto::PaneTarget::with_window(session_name.clone(), 0, 0);
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 76).await;
    handler.start_attached_input_capture_for_test(&target).await;
    let keep_id = choose_buffer_item_id(&handler, attach_pid, "keep").await;
    let old_attach_id = {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&attach_pid)
            .expect("original host remains attached");
        active
            .mode_tree
            .as_mut()
            .expect("original choose-buffer remains active")
            .selected_id = Some(keep_id);
        active.id
    };

    let pause =
        super::super::mode_tree_buffer_actions::install_mode_tree_buffer_paste_pause(attach_pid);
    let key_handler = handler.clone();
    let key = tokio::spawn(async move {
        key_handler
            .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('p'))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("p reaches the pre-write identity pause");

    let (replacement_tx, mut replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    assert_ne!(replacement_attach_id, old_attach_id);
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    let replacement_state_id = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("replacement host is attached")
        .mode_tree_state_id;
    while replacement_rx.try_recv().is_ok() {}

    pause.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), key)
        .await
        .expect("stale p key completes")
        .expect("stale p key task joins");
    assert!(result.is_err(), "the stale p key must fail closed");
    assert_eq!(
        handler.attached_input_capture_for_test(&target).await,
        Some(Vec::new()),
        "the stale p key must not paste into the pane"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("replacement host survives");
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.mode_tree_state_id, replacement_state_id);
    assert!(
        replacement.mode_tree.is_some(),
        "the stale p key must not dismiss the replacement tree"
    );
    drop(active_attach);
    while let Ok(control) = replacement_rx.try_recv() {
        assert!(
            !matches!(
                control,
                crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
            ),
            "the stale p key advanced the replacement overlay state"
        );
    }
}

#[tokio::test]
async fn choose_buffer_d_key_rejects_a_same_pid_reconnect_before_delete() {
    let label = "choose-buffer-d-key-host-generation";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 77).await;
    let keep_id = choose_buffer_item_id(&handler, attach_pid, "keep").await;
    let old_attach_id = {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&attach_pid)
            .expect("original host remains attached");
        active
            .mode_tree
            .as_mut()
            .expect("original choose-buffer remains active")
            .selected_id = Some(keep_id);
        active.id
    };

    let pause =
        super::super::mode_tree_buffer_actions::install_mode_tree_buffer_delete_pause(attach_pid);
    let key_handler = handler.clone();
    let key = tokio::spawn(async move {
        key_handler
            .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('d'))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("d reaches the pre-delete identity pause");

    let (replacement_tx, mut replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    assert_ne!(replacement_attach_id, old_attach_id);
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    let replacement_state_id = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("replacement host is attached")
        .mode_tree_state_id;
    while replacement_rx.try_recv().is_ok() {}

    pause.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), key)
        .await
        .expect("stale d key completes")
        .expect("stale d key task joins");
    assert!(result.is_err(), "the stale d key must fail closed");
    assert_eq!(
        handler.state.lock().await.buffers.get("keep"),
        Some(&b"safe"[..]),
        "the stale d key must not delete the selected buffer"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("replacement host survives");
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.mode_tree_state_id, replacement_state_id);
    assert!(
        replacement.mode_tree.is_some(),
        "the stale d key must not dismiss the replacement tree"
    );
    drop(active_attach);
    while let Ok(control) = replacement_rx.try_recv() {
        assert!(
            !matches!(
                control,
                crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
            ),
            "the stale d key advanced the replacement overlay state"
        );
    }
}

#[tokio::test]
async fn choose_buffer_stale_host_identity_cannot_dismiss_a_replacement_tree() {
    let label = "choose-buffer-host-generation-dismiss";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 71).await;
    let old_identity = {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("original host remains attached");
        super::super::mode_tree_model::ModeTreeActionIdentity::new(
            attach_pid,
            active.id,
            active.mode_tree_state_id,
        )
    };

    let (replacement_tx, _replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;

    assert!(handler
        .dismiss_mode_tree_with_refresh_for_action_identity(old_identity)
        .await
        .is_err());
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("replacement host survives");
    assert_eq!(replacement.id, replacement_attach_id);
    assert!(replacement.mode_tree.is_some());
}

#[tokio::test]
async fn closing_host_identity_cannot_dismiss_the_shared_mode_tree() {
    let (handler, attach_pid, mut control_rx) =
        choose_buffer_action_fixture("choose-buffer-closing-host-dismiss", 75).await;
    let (attach_id, mode_tree_state_id) = {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("choose-buffer host remains registered");
        active
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        (active.id, active.mode_tree_state_id)
    };
    while control_rx.try_recv().is_ok() {}

    assert!(handler
        .dismiss_mode_tree_for_client_identity(attach_pid, attach_id)
        .await
        .is_err());

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("closing host remains registered for teardown");
    assert_eq!(active.mode_tree_state_id, mode_tree_state_id);
    assert!(
        active.mode_tree.is_some(),
        "a stale closing action must not dismiss the shared tree"
    );
    drop(active_attach);
    while let Ok(control) = control_rx.try_recv() {
        assert!(
            !matches!(
                control,
                crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
            ),
            "a closing action advanced the shared mode-tree generation"
        );
    }
}

#[tokio::test]
async fn choose_buffer_delete_does_not_fall_back_after_all_tags_go_stale() {
    let (handler, attach_pid, _control_rx) =
        choose_buffer_action_fixture("choose-buffer-stale-tag", 72).await;
    let stale_id = choose_buffer_item_id(&handler, attach_pid, "stale").await;
    let keep_id = choose_buffer_item_id(&handler, attach_pid, "keep").await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(keep_id);
        mode.tagged.insert(stale_id);
    })
    .await;
    replace_stale_buffer(&handler).await;

    handler
        .perform_buffer_delete(attach_pid)
        .await
        .expect("stale tagged delete is a no-op");

    let state = handler.state.lock().await;
    assert_eq!(state.buffers.get("stale"), Some(&b"replacement"[..]));
    assert_eq!(state.buffers.get("keep"), Some(&b"safe"[..]));
    drop(state);
    assert!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&attach_pid)
            .is_some_and(|active| active.mode_tree.is_some()),
        "stale identity keeps choose-buffer open for explicit review"
    );
}

#[tokio::test]
async fn choose_buffer_paste_delete_does_not_fall_back_after_selection_goes_stale() {
    let (handler, attach_pid, _control_rx) =
        choose_buffer_action_fixture("choose-buffer-stale-selection", 73).await;
    let stale_id = choose_buffer_item_id(&handler, attach_pid, "stale").await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(stale_id)
    })
    .await;
    replace_stale_buffer(&handler).await;

    handler
        .perform_buffer_paste(attach_pid, true)
        .await
        .expect("stale selected paste-delete is a no-op");

    let state = handler.state.lock().await;
    assert_eq!(state.buffers.get("stale"), Some(&b"replacement"[..]));
    assert_eq!(state.buffers.get("keep"), Some(&b"safe"[..]));
    drop(state);
    assert!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&attach_pid)
            .is_some_and(|active| active.mode_tree.is_some()),
        "stale ordered paste keeps choose-buffer open for explicit review"
    );
}

#[tokio::test]
async fn choose_buffer_confirmation_uses_captured_buffer_instances() {
    let (handler, attach_pid, mut control_rx) =
        choose_buffer_action_fixture("choose-buffer-confirm-snapshot", 74).await;
    let stale_id = choose_buffer_item_id(&handler, attach_pid, "stale").await;
    let delete_id = choose_buffer_item_id(&handler, attach_pid, "delete-me").await;
    let keep_id = choose_buffer_item_id(&handler, attach_pid, "keep").await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(keep_id);
        mode.tagged.insert(stale_id);
        mode.tagged.insert(delete_id);
    })
    .await;
    handler
        .handle_attached_live_input_for_test(attach_pid, b"x")
        .await
        .expect("delete confirmation opens");
    assert_eq!(
        handler
            .attached_prompt_render(attach_pid)
            .await
            .expect("delete confirmation is active")
            .prompt,
        "delete selected buffers?"
    );

    replace_stale_buffer(&handler).await;

    while control_rx.try_recv().is_ok() {}
    handler
        .handle_attached_live_input_for_test(attach_pid, b"y")
        .await
        .expect("confirmation input succeeds");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match control_rx.recv().await {
                Some(crate::pane_io::AttachControl::Overlay(_)) => break,
                Some(_) => {}
                None => panic!("attach control channel closed before action refresh"),
            }
        }
    })
    .await
    .expect("captured buffer actions finish");

    let state = handler.state.lock().await;
    assert!(state.buffers.get("delete-me").is_none());
    assert_eq!(state.buffers.get("stale"), Some(&b"replacement"[..]));
    assert_eq!(state.buffers.get("keep"), Some(&b"safe"[..]));
}

#[tokio::test]
async fn choose_tree_kill_pane_drains_after_kill_pane_inline_hook() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("choose-tree-after-kill").expect("valid session");
    handler.create_session(&alpha).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(&alpha)
        })
        .await;
    handler
        .handle_ok(SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::AfterKillPane,
            "set-buffer -b tree-after-kill fired",
        )))
        .await;
    let (session_id, window_id, window_occurrence_id, pane_id, pane_output_generation) = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window.pane(1).expect("second pane exists");
        (
            session.id(),
            window.id(),
            state
                .window_link_occurrence_id(&alpha, 0)
                .expect("window occurrence exists"),
            pane.id(),
            state.pane_output_generation_for_target(
                &rmux_proto::PaneTarget::with_window(alpha.clone(), 0, pane.index()),
                pane.id(),
            ),
        )
    };

    handler
        .perform_tree_kill_actions(
            std::process::id(),
            vec![ModeTreeAction::pane_tree_target(
                rmux_proto::PaneTarget::with_window(alpha, 0, 1),
                session_id,
                window_id,
                window_occurrence_id,
                pane_id,
                pane_output_generation,
            )],
        )
        .await
        .expect("choose-tree pane kill succeeds");

    let state = handler.state.lock().await;
    assert_eq!(state.buffers.get("tree-after-kill"), Some(&b"fired"[..]));
}

#[tokio::test]
async fn choose_tree_tagged_pane_kills_follow_stable_ids_after_renumbering() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("choose-tree-tagged-stable-panes").expect("valid session");
    handler.create_session(&alpha).await;
    for _ in 0..2 {
        handler
            .handle_ok(SplitWindowRequest {
                direction: SplitDirection::Horizontal,
                ..Fixture::fixture(&alpha)
            })
            .await;
    }

    let (session_id, window_id, window_occurrence_id, pane_targets, surviving_pane_id) = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let mut panes = window
            .panes()
            .iter()
            .map(|pane| {
                (
                    pane.index(),
                    pane.id(),
                    state.pane_output_generation_for_target(
                        &rmux_proto::PaneTarget::with_window(alpha.clone(), 0, pane.index()),
                        pane.id(),
                    ),
                )
            })
            .collect::<Vec<_>>();
        panes.sort_by_key(|(pane_index, _, _)| *pane_index);
        assert_eq!(panes.len(), 3);
        (
            session.id(),
            window.id(),
            state
                .window_link_occurrence_id(&alpha, 0)
                .expect("window occurrence exists"),
            panes[..2].to_vec(),
            panes[2].1,
        )
    };
    let actions = pane_targets
        .into_iter()
        .map(|(pane_index, pane_id, pane_output_generation)| {
            ModeTreeAction::pane_tree_target(
                rmux_proto::PaneTarget::with_window(alpha.clone(), 0, pane_index),
                session_id,
                window_id,
                window_occurrence_id,
                pane_id,
                pane_output_generation,
            )
        })
        .collect();

    handler
        .perform_tree_kill_actions(std::process::id(), actions)
        .await
        .expect("tagged pane kills succeed after the first pane renumbers the second");

    let state = handler.state.lock().await;
    let panes = state
        .sessions
        .session(&alpha)
        .expect("session survives")
        .window_at(0)
        .expect("window survives")
        .panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id(), surviving_pane_id);
}

#[tokio::test]
async fn choose_tree_tagged_link_aliases_do_not_abort_later_distinct_kills() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("choose-tree-link-alpha").expect("valid session");
    let beta = SessionName::new("choose-tree-link-beta").expect("valid session");
    let gamma = SessionName::new("choose-tree-link-gamma").expect("valid session");
    for session in [&alpha, &beta, &gamma] {
        handler.create_session(session).await;
    }
    for session in [&alpha, &gamma] {
        handler
            .create_window(NewWindowRequest {
                target_window_index: Some(1),
                ..Fixture::fixture(session)
            })
            .await;
    }
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(beta.clone(), 1),
        )))
        .await;

    let actions = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        [&alpha, &beta, &gamma]
            .into_iter()
            .zip([0_u32, 1, 0])
            .map(|(session_name, window_index)| {
                let session = state
                    .sessions
                    .session(session_name)
                    .expect("tagged session exists");
                let window = session
                    .window_at(window_index)
                    .expect("tagged window exists");
                ModeTreeAction::window_tree_target(
                    session_name.clone(),
                    session.id(),
                    window_index,
                    window.id(),
                    state
                        .window_link_occurrence_id(session_name, window_index)
                        .expect("tagged window occurrence exists"),
                )
            })
            .collect::<Vec<_>>()
    };

    handler
        .perform_tree_kill_actions(std::process::id(), actions)
        .await
        .expect("duplicate linked aliases are one stable kill target");

    let state = handler.state.lock().await;
    let alpha_session = state
        .sessions
        .session(&alpha)
        .expect("alpha keeps window one");
    assert!(alpha_session.window_at(0).is_none());
    assert!(alpha_session.window_at(1).is_some());
    let gamma_session = state
        .sessions
        .session(&gamma)
        .expect("gamma keeps window one");
    assert!(gamma_session.window_at(0).is_none());
    assert!(gamma_session.window_at(1).is_some());
    let beta_session = state
        .sessions
        .session(&beta)
        .expect("beta keeps window zero");
    assert!(beta_session.window_at(0).is_some());
    assert!(beta_session.window_at(1).is_none());
}

#[tokio::test]
async fn choose_tree_stale_session_action_fails_closed_after_name_reuse() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("choose-tree-session-aba").expect("valid session");
    handler.create_session(&alpha).await;
    let old_session_id = handler.session_id_for_test(&alpha).await;
    let stale_action = ModeTreeAction::session_tree_target(alpha.clone(), old_session_id);

    handler.handle_ok(KillSessionRequest::fixture(&alpha)).await;
    handler.create_session(&alpha).await;
    let replacement_session_id = handler.session_id_for_test(&alpha).await;
    assert_ne!(replacement_session_id, old_session_id);

    let error = handler
        .perform_tree_kill_actions(std::process::id(), vec![stale_action])
        .await
        .expect_err("stale session tree action must fail closed");
    assert!(matches!(error, RmuxError::SessionNotFound(_)), "{error:?}");
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&alpha)
            .map(rmux_core::Session::id),
        Some(replacement_session_id)
    );
}

#[tokio::test]
async fn choose_tree_tagged_kill_skips_stale_identity_and_finishes_live_batch() {
    let handler = RequestHandler::new();
    let stale_name = SessionName::new("a-choose-tree-stale-batch").expect("valid session");
    let live_name = SessionName::new("z-choose-tree-live-batch").expect("valid session");
    handler.create_session(&stale_name).await;
    handler.create_session(&live_name).await;
    let stale_id = handler.session_id_for_test(&stale_name).await;
    let live_id = handler.session_id_for_test(&live_name).await;
    let stale_action = ModeTreeAction::session_tree_target(stale_name.clone(), stale_id);
    let live_action = ModeTreeAction::session_tree_target(live_name.clone(), live_id);

    handler
        .handle_ok(KillSessionRequest::fixture(&stale_name))
        .await;
    handler.create_session(&stale_name).await;
    let replacement_id = handler.session_id_for_test(&stale_name).await;
    assert_ne!(replacement_id, stale_id);

    handler
        .perform_tree_kill_tagged_actions(std::process::id(), vec![stale_action, live_action])
        .await
        .expect("stale tagged target is skipped without aborting the live batch");

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&stale_name)
            .map(rmux_core::Session::id),
        Some(replacement_id),
        "the replacement behind the stale identity must survive"
    );
    assert!(
        state.sessions.session(&live_name).is_none(),
        "the later live tagged target must still be processed"
    );
}

#[tokio::test]
async fn choose_tree_kill_current_does_not_fall_back_after_session_aba() {
    use super::super::mode_tree_order::session_item_id;

    let handler = RequestHandler::new();
    let host = SessionName::new("choose-tree-aba-host").expect("valid session");
    let alpha = SessionName::new("choose-tree-aba-target").expect("valid session");
    handler.create_session(&host).await;
    handler.create_session(&alpha).await;
    let old_session_id = handler.session_id_for_test(&alpha).await;

    let attach_pid = std::process::id().saturating_add(41);
    let _control_rx = handler.attach_client(attach_pid, host).await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-s"]).await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(session_item_id(old_session_id));
    })
    .await;

    handler.handle_ok(KillSessionRequest::fixture(&alpha)).await;
    handler.create_session(&alpha).await;
    let replacement_session_id = handler.session_id_for_test(&alpha).await;
    assert_ne!(replacement_session_id, old_session_id);

    handler
        .perform_tree_kill_current(attach_pid)
        .await
        .expect("stale current selection is a no-op");
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&alpha)
            .map(rmux_core::Session::id),
        Some(replacement_session_id)
    );
}

#[tokio::test]
async fn choose_tree_stale_window_action_fails_closed_after_index_reuse() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("choose-tree-window-aba").expect("valid session");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let (session_id, old_window_id, old_window_occurrence_id) = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        let session = state.sessions.session(&alpha).expect("session exists");
        (
            session.id(),
            session.window_at(1).expect("old window exists").id(),
            state
                .window_link_occurrence_id(&alpha, 1)
                .expect("old window occurrence exists"),
        )
    };
    let stale_action = ModeTreeAction::window_tree_target(
        alpha.clone(),
        session_id,
        1,
        old_window_id,
        old_window_occurrence_id,
    );

    handler
        .handle_ok(KillWindowRequest::fixture(WindowTarget::with_window(
            alpha.clone(),
            1,
        )))
        .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let replacement_window_id = handler
        .window_id_for_test(&WindowTarget::with_window(alpha.clone(), 1))
        .await;
    assert_ne!(replacement_window_id, old_window_id);

    let error = handler
        .perform_tree_kill_actions(std::process::id(), vec![stale_action])
        .await
        .expect_err("stale window tree action must fail closed");
    assert!(
        matches!(error, RmuxError::InvalidTarget { .. }),
        "{error:?}"
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(1))
            .map(rmux_core::Window::id),
        Some(replacement_window_id)
    );
}

#[tokio::test]
async fn rename_session_rekeys_active_choose_tree_host_before_refresh() {
    let handler = RequestHandler::new();
    let original = SessionName::new("choose-tree-rename-original").expect("valid session");
    let renamed = SessionName::new("choose-tree-rename-renamed").expect("valid session");
    handler.create_session(&original).await;

    let attach_pid = std::process::id().saturating_add(652);
    let _control_rx = handler.attach_client(attach_pid, &original).await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-s"]).await;
    let stable_selected_id = with_mode_tree(&handler, attach_pid, |mode| {
        let selected_id = mode
            .selected_id
            .clone()
            .expect("choose-tree selects a stable identity row");
        mode.tagged.insert(selected_id.clone());
        selected_id
    })
    .await;

    handler
        .handle_ok(RenameSessionRequest {
            target: original,
            new_name: renamed.clone(),
        })
        .await;

    {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("attached choose-tree client survives rename");
        assert_eq!(active.session_name, renamed);
        assert_eq!(
            active
                .mode_tree
                .as_ref()
                .expect("choose-tree remains active")
                .session_name,
            renamed,
            "the persistent choose-tree host must follow the session identity"
        );
        assert_eq!(
            active
                .mode_tree
                .as_ref()
                .expect("choose-tree remains active")
                .selected_id
                .as_ref(),
            Some(&stable_selected_id),
            "SessionId-based selection must remain stable across a name-only rename"
        );
        assert!(
            active
                .mode_tree
                .as_ref()
                .expect("choose-tree remains active")
                .tagged
                .contains(&stable_selected_id),
            "SessionId-based tags must survive the rebuild without stale name keys"
        );
    }

    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("q dismisses choose-tree after rename"));
    assert!(
        handler.active_attach.lock().await.by_pid[&attach_pid]
            .mode_tree
            .is_none(),
        "q must not leave an invisible input-capturing tree after rename"
    );
}

#[tokio::test]
async fn refresh_client_replays_active_mode_tree_after_base_switch() {
    let handler = RequestHandler::new();
    let session = SessionName::new("refresh-client-mode-tree").expect("valid session");
    handler.create_session(&session).await;
    let attach_pid = std::process::id();
    let mut control_rx = handler.attach_client(attach_pid, session).await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-s"]).await;
    while control_rx.try_recv().is_ok() {}

    handler.handle_ok(RefreshClientRequest::fixture(None)).await;

    let mut saw_switch = false;
    let mut replayed_tree = None;
    while let Ok(control) = control_rx.try_recv() {
        match control {
            crate::pane_io::AttachControl::Switch(_) => saw_switch = true,
            crate::pane_io::AttachControl::Overlay(frame)
                if saw_switch && frame.persistent_state_id.is_some() =>
            {
                replayed_tree = Some(frame);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_switch, "refresh-client must queue the base Switch");
    let frame = replayed_tree.expect("mode-tree overlay must follow the base Switch");
    assert!(frame.persistent);
    assert!(!frame.frame.is_empty());
    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("replayed choose-tree accepts q"));
}

async fn choose_tree_identity_guard_fixture(
    label: &str,
    attach_pid_offset: u32,
) -> (
    RequestHandler,
    u32,
    ModeTreeActionIdentity,
    ChooseTreeTarget,
    mpsc::UnboundedReceiver<crate::pane_io::AttachControl>,
) {
    let handler = RequestHandler::new();
    let session_name = SessionName::new(label).expect("valid session");
    handler.create_session(&session_name).await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session_name)
        })
        .await;
    let (session_id, window_id, window_occurrence_id) = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        let session = state
            .sessions
            .session(&session_name)
            .expect("choose-tree session exists");
        (
            session.id(),
            session.window_at(1).expect("target window exists").id(),
            state
                .window_link_occurrence_id(&session_name, 1)
                .expect("target window occurrence exists"),
        )
    };
    let attach_pid = std::process::id().saturating_add(attach_pid_offset);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let attach_id = handler
        .register_attach(attach_pid, session_name.clone(), control_tx)
        .await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-w"]).await;
    let action_identity = {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("choose-tree host remains attached");
        assert_eq!(active.id, attach_id);
        ModeTreeActionIdentity::new(attach_pid, active.id, active.mode_tree_state_id)
    };
    (
        handler,
        attach_pid,
        action_identity,
        ChooseTreeTarget {
            session_name,
            session_id,
            window_index: Some(1),
            window_id: Some(window_id),
            window_occurrence_id: Some(window_occurrence_id),
            pane_index: None,
            pane_id: None,
            pane_output_generation: None,
        },
        control_rx,
    )
}

#[tokio::test]
async fn choose_tree_session_switch_resizes_the_target_for_the_attached_client() {
    let handler = RequestHandler::new();
    let host = SessionName::new("choose-tree-resize-host").expect("valid session");
    let target = SessionName::new("choose-tree-resize-target").expect("valid session");
    handler.create_session(&host).await;
    handler.create_session(&target).await;
    let target_session_id = handler.session_id_for_test(&target).await;

    let attach_pid = std::process::id().saturating_add(654);
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let attach_id = handler
        .register_attach(attach_pid, host.clone(), control_tx)
        .await;
    let client_size = TerminalSize {
        cols: 120,
        rows: 40,
    };
    handler
        .handle_attached_resize(attach_pid, client_size)
        .await
        .expect("attached client resize succeeds");
    while control_rx.try_recv().is_ok() {}

    open_mode_tree(&handler, attach_pid, &["choose-tree", "-s"]).await;
    let action_identity = {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("choose-tree host remains attached");
        assert_eq!(active.id, attach_id);
        ModeTreeActionIdentity::new(attach_pid, active.id, active.mode_tree_state_id)
    };

    handler
        .apply_choose_tree_default_target(
            action_identity,
            ChooseTreeTarget {
                session_name: target.clone(),
                session_id: target_session_id,
                window_index: None,
                window_id: None,
                window_occurrence_id: None,
                pane_index: None,
                pane_id: None,
                pane_output_generation: None,
            },
        )
        .await
        .expect("choose-tree switches sessions");

    let state = handler.state.lock().await;
    let target_session = state.sessions.session(&target).expect("target survives");
    assert_eq!(
        target_session.window().size(),
        TerminalSize {
            cols: client_size.cols,
            rows: client_size.rows - 1,
        }
    );
}

#[tokio::test]
async fn choose_tree_stale_mode_tree_generation_cannot_select_or_dismiss() {
    let (handler, attach_pid, stale_identity, target, mut control_rx) =
        choose_tree_identity_guard_fixture("choose-tree-stale-host-state", 652).await;
    let replacement_mode = with_mode_tree(&handler, attach_pid, |mode| mode.clone()).await;
    handler
        .store_mode_tree_state(attach_pid, replacement_mode)
        .await
        .expect("replacement mode-tree generation stores");
    let replacement_state_id = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("choose-tree host remains attached")
        .mode_tree_state_id;
    assert_ne!(replacement_state_id, stale_identity.state_id());
    while control_rx.try_recv().is_ok() {}

    assert!(handler
        .apply_choose_tree_default_target(stale_identity, target)
        .await
        .is_err());

    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&SessionName::new("choose-tree-stale-host-state").expect("valid session"))
            .expect("session survives")
            .active_window_index(),
        0,
        "stale generation must not select the target window"
    );
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("host survives");
    assert_eq!(active.mode_tree_state_id, replacement_state_id);
    assert!(
        active.mode_tree.is_some(),
        "replacement tree must remain open"
    );
    drop(active_attach);
    while let Ok(control) = control_rx.try_recv() {
        assert!(!matches!(
            control,
            crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
        ));
    }
}

#[tokio::test]
async fn choose_tree_closing_host_cannot_select_or_dismiss() {
    let (handler, attach_pid, identity, target, mut control_rx) =
        choose_tree_identity_guard_fixture("choose-tree-closing-host", 653).await;
    {
        let active_attach = handler.active_attach.lock().await;
        active_attach
            .by_pid
            .get(&attach_pid)
            .expect("choose-tree host remains attached")
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    while control_rx.try_recv().is_ok() {}

    assert!(handler
        .apply_choose_tree_default_target(identity, target)
        .await
        .is_err());

    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&SessionName::new("choose-tree-closing-host").expect("valid session"))
            .expect("session survives")
            .active_window_index(),
        0,
        "closing host must not select the target window"
    );
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("closing host remains registered for teardown");
    assert_eq!(active.mode_tree_state_id, identity.state_id());
    assert!(active.mode_tree.is_some(), "shared tree must remain open");
    drop(active_attach);
    while let Ok(control) = control_rx.try_recv() {
        assert!(!matches!(
            control,
            crate::pane_io::AttachControl::AdvancePersistentOverlayState(_)
        ));
    }
}

#[tokio::test]
async fn choose_tree_default_switch_rejects_a_reconnected_host_at_the_commit_lock() {
    let handler = RequestHandler::new();
    let host = SessionName::new("choose-tree-host-generation-alpha").expect("valid session");
    let target = SessionName::new("choose-tree-host-generation-beta").expect("valid session");
    handler.create_session(&host).await;
    handler.create_session(&target).await;
    let target_session_id = handler.session_id_for_test(&target).await;

    let attach_pid = std::process::id().saturating_add(651);
    let (old_tx, _old_rx) = mpsc::unbounded_channel();
    let old_attach_id = handler
        .register_attach(attach_pid, host.clone(), old_tx)
        .await;
    open_mode_tree(&handler, attach_pid, &["choose-tree", "-s"]).await;
    let old_action_identity = {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("choose-tree host remains attached");
        assert_eq!(active.id, old_attach_id);
        ModeTreeActionIdentity::new(attach_pid, active.id, active.mode_tree_state_id)
    };

    let pause = handler.install_attached_size_selection_pause();
    let switch_handler = handler.clone();
    let switch_target = target.clone();
    let switch = tokio::spawn(async move {
        switch_handler
            .apply_choose_tree_default_target(
                old_action_identity,
                ChooseTreeTarget {
                    session_name: switch_target,
                    session_id: target_session_id,
                    window_index: None,
                    window_id: None,
                    window_occurrence_id: None,
                    pane_index: None,
                    pane_id: None,
                    pane_output_generation: None,
                },
            )
            .await
    });

    pause.reached.notified().await;
    let (replacement_tx, mut replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, host.clone(), replacement_tx)
        .await;
    assert_ne!(replacement_attach_id, old_attach_id);
    pause.release.notify_one();

    assert!(
        switch.await.expect("switch task joins").is_err(),
        "stale host generation must fail closed"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&attach_pid)
        .expect("replacement host survives");
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.session_name, host);
    drop(active_attach);
    while let Ok(control) = replacement_rx.try_recv() {
        assert!(
            !matches!(control, crate::pane_io::AttachControl::Switch(_)),
            "stale choose-tree switch reached the replacement host"
        );
    }
}

#[tokio::test]
async fn choose_tree_zw_runs_direct_command_only_on_accept() {
    let handler = RequestHandler::new();
    let attach_pid = std::process::id();
    let alpha = SessionName::new("alpha").expect("valid session");

    handler.create_session(&alpha).await;

    let _control_rx = handler.attach_client(attach_pid, &alpha).await;

    open_mode_tree(
        &handler,
        attach_pid,
        &["choose-tree", "-Zw", "set-buffer", "-b", "chosen", "%%"],
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert!(
            state.buffers.get("chosen").is_none(),
            "direct command must not run before accept"
        );
    }

    handler
        .accept_mode_tree_selection(attach_pid)
        .await
        .expect("selection accept succeeds");

    let state = handler.state.lock().await;
    let chosen = state
        .buffers
        .get("chosen")
        .expect("buffer created on accept");
    assert_eq!(String::from_utf8_lossy(chosen), "=alpha:0.");
}

#[tokio::test]
async fn choose_client_from_unattached_request_activates_mode_tree_on_all_attaches() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("alpha").expect("valid session");

    handler.create_session(&alpha).await;

    let first_pid = std::process::id();
    let second_pid = first_pid.saturating_add(1);
    let mut first_rx = handler.attach_client(first_pid, &alpha).await;
    let mut second_rx = handler.attach_client(second_pid, alpha).await;

    open_mode_tree(&handler, first_pid.saturating_add(10), &["choose-client"]).await;

    let active_attach = handler.active_attach.lock().await;
    assert!(active_attach
        .by_pid
        .get(&first_pid)
        .and_then(|active| active.mode_tree.as_ref())
        .is_some());
    assert!(active_attach
        .by_pid
        .get(&second_pid)
        .and_then(|active| active.mode_tree.as_ref())
        .is_some());
    drop(active_attach);

    let first_overlay = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let Some(control) = first_rx.recv().await else {
                panic!("first attach control channel closed before choose-client overlay");
            };
            if matches!(control, crate::pane_io::AttachControl::Overlay(_)) {
                break;
            }
        }
    })
    .await;
    assert!(
        first_overlay.is_ok(),
        "first attach did not receive choose-client overlay"
    );

    let second_overlay = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let Some(control) = second_rx.recv().await else {
                panic!("second attach control channel closed before choose-client overlay");
            };
            if matches!(control, crate::pane_io::AttachControl::Overlay(_)) {
                break;
            }
        }
    })
    .await;
    assert!(
        second_overlay.is_ok(),
        "second attach did not receive choose-client overlay"
    );
}

#[tokio::test]
async fn mode_tree_commands_without_attached_client_mark_target_pane_mode() {
    for (command, expected_mode, needs_buffer) in [
        ("choose-tree -t alpha:0.0", "tree-mode", false),
        ("find-window -t alpha:0.0 sleep", "tree-mode", false),
        ("find-window -talpha:0.0 sleep", "tree-mode", false),
        ("customize-mode -t alpha:0.0", "options-mode", false),
        ("choose-buffer -t alpha:0.0", "buffer-mode", true),
    ] {
        let handler = RequestHandler::new();
        let alpha = SessionName::new("alpha").expect("valid session");
        handler.create_session(&alpha).await;

        if needs_buffer {
            handler
                .handle_ok(SetBufferRequest::fixture(("buf", b"hello".to_vec())))
                .await;
        }

        open_mode_tree(
            &handler,
            std::process::id().saturating_add(10),
            &command.split_whitespace().collect::<Vec<_>>(),
        )
        .await;

        let target = rmux_proto::PaneTarget::with_window(alpha, 0, 0);
        let state = handler.state.lock().await;
        let transcript = state.transcript_handle(&target).expect("target transcript");
        let mode = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned")
            .pane_mode_name();
        assert_eq!(mode, Some(expected_mode), "command should enter mode");
    }
}

#[tokio::test]
async fn mode_tree_temporarily_hides_modal_scrollbar_and_restores_copy_mode_geometry() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("mode-tree-modal-stack").expect("valid session");
    let target = rmux_proto::PaneTarget::with_window(alpha.clone(), 0, 0);
    handler
        .create_session((&alpha, TerminalSize::new(20, 8)))
        .await;
    for (scope, option, value) in [
        (
            ScopeSelector::Session(alpha.clone()),
            OptionName::Status,
            "off",
        ),
        (
            ScopeSelector::Window(rmux_proto::WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::PaneScrollbars,
            "modal",
        ),
        (
            ScopeSelector::Window(rmux_proto::WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::PaneScrollbarsStyle,
            "width=2,pad=1",
        ),
    ] {
        handler.set_option(scope, option, value).await;
    }
    handler.handle_ok(CopyModeRequest::fixture(&target)).await;

    assert_eq!(handler.pane_terminal_size_for_test(&target).await.cols, 17);

    open_mode_tree(
        &handler,
        std::process::id().saturating_add(11),
        &["choose-tree", "-t", "mode-tree-modal-stack:0.0"],
    )
    .await;

    // Oracle tmux 3.7b: mode-tree becomes the top mode, hides the modal
    // scrollbar and expands the PTY; dismissing it restores copy-mode and the
    // scrollbar reservation.
    assert_eq!(handler.pane_terminal_size_for_test(&target).await.cols, 20);
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .transcript_handle(&target)
            .expect("target transcript")
            .lock()
            .expect("pane transcript mutex")
            .pane_mode_name(),
        Some("tree-mode")
    );

    assert!(handler
        .clear_mode_tree_for_target(&target)
        .await
        .expect("mode-tree clears"));
    assert_eq!(handler.pane_terminal_size_for_test(&target).await.cols, 17);
    let state = handler.state.lock().await;
    assert!(state
        .pane_copy_mode_summary(&alpha, rmux_core::PaneId::new(0))
        .is_some());
}

#[tokio::test]
async fn choose_tree_zw_defers_parse_errors_until_accept() {
    let handler = RequestHandler::new();
    let attach_pid = std::process::id();
    let alpha = SessionName::new("alpha").expect("valid session");

    handler.create_session(&alpha).await;

    let _control_rx = handler.attach_client(attach_pid, alpha).await;

    open_mode_tree(&handler, attach_pid, &["choose-tree", "-Zw", "{"]).await;

    let err = handler
        .accept_mode_tree_selection(attach_pid)
        .await
        .expect_err("parse error should surface when accepting");
    let RmuxError::Server(message) = err else {
        panic!("expected server parse error");
    };
    assert!(message.starts_with("mode-tree command parse failed:"));
}

#[tokio::test]
async fn mode_tree_navigation_rejects_a_same_pid_replacement_tree() {
    use super::super::mode_tree_test_support::{
        install_mode_tree_identity_pause, ModeTreeIdentityPausePoint,
    };

    let label = "mode-tree-navigation-attach-identity";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 901).await;
    let pause = install_mode_tree_identity_pause(ModeTreeIdentityPausePoint::Store(attach_pid));
    let input_handler = handler.clone();
    let input = tokio::spawn(async move {
        input_handler
            .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('j'))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("navigation reaches its identity commit");

    let (replacement_tx, _replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    let (replacement_state_id, replacement_selection) = {
        let active_attach = handler.active_attach.lock().await;
        let replacement = &active_attach.by_pid[&attach_pid];
        (
            replacement.mode_tree_state_id,
            replacement
                .mode_tree
                .as_ref()
                .and_then(|mode| mode.selected_id.clone()),
        )
    };
    pause.release.notify_one();

    assert!(
        input.await.expect("navigation task joins").is_err(),
        "the stale navigation must fail closed"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = &active_attach.by_pid[&attach_pid];
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.mode_tree_state_id, replacement_state_id);
    assert_eq!(
        replacement
            .mode_tree
            .as_ref()
            .and_then(|mode| mode.selected_id.clone()),
        replacement_selection
    );
}

#[tokio::test]
async fn mode_tree_prompt_callback_rejects_a_same_pid_replacement_tree() {
    let label = "mode-tree-prompt-attach-identity";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 902).await;
    let stale_identity = handler
        .current_mode_tree_action_identity(attach_pid)
        .await
        .expect("original tree identity");

    let (replacement_tx, _replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name, replacement_tx)
        .await;
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    let replacement_state_id =
        handler.active_attach.lock().await.by_pid[&attach_pid].mode_tree_state_id;

    assert!(
        handler
            .apply_mode_tree_filter(stale_identity, "must-not-apply".to_owned())
            .await
            .is_err(),
        "the stale prompt callback must fail closed"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = &active_attach.by_pid[&attach_pid];
    assert_eq!(replacement.id, replacement_attach_id);
    assert_eq!(replacement.mode_tree_state_id, replacement_state_id);
    assert_eq!(
        replacement
            .mode_tree
            .as_ref()
            .and_then(|mode| mode.filter_text.as_deref()),
        None
    );
}

#[tokio::test]
async fn customize_unset_revalidates_requester_at_option_mutation_lock() {
    use super::super::mode_tree_test_support::{
        install_mode_tree_identity_pause, ModeTreeIdentityPausePoint,
    };

    let handler = RequestHandler::new();
    let session_name = SessionName::new("customize-option-requester-aba").expect("valid session");
    handler.create_session(&session_name).await;
    handler
        .set_option(
            ScopeSelector::Session(session_name.clone()),
            OptionName::Status,
            "off",
        )
        .await;
    let attach_pid = std::process::id().saturating_add(907);
    let _control_rx = handler.attach_client(attach_pid, &session_name).await;
    open_mode_tree(&handler, attach_pid, &["customize-mode"]).await;
    let identity = handler
        .current_mode_tree_action_identity(attach_pid)
        .await
        .expect("original customize identity");
    let selected_id = {
        let mut mode = handler
            .mode_tree_for_action_identity(identity)
            .await
            .expect("customize tree remains active");
        handler
            .build_mode_tree(&mut mode, attach_pid)
            .await
            .expect("customize tree builds")
            .items
            .values()
            .find(|item| {
                matches!(
                    &item.action,
                    ModeTreeAction::CustomizeOption {
                        scope: rmux_proto::types::OptionScopeSelector::Session(name),
                        name: option_name,
                    } if name == &session_name && option_name == "status"
                )
            })
            .map(|item| item.id.clone())
            .expect("session status option is listed")
    };
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(selected_id)
    })
    .await;

    let pause = install_mode_tree_identity_pause(ModeTreeIdentityPausePoint::Mutation(attach_pid));
    let action_handler = handler.clone();
    let action = tokio::spawn(async move {
        action_handler
            .perform_customize_unset_for_identity(identity)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("customize unset reaches its final identity check");

    let _replacement_rx = handler.attach_client(attach_pid, &session_name).await;
    open_mode_tree(&handler, attach_pid, &["customize-mode"]).await;
    pause.release.notify_one();

    assert!(
        action.await.expect("customize task joins").is_err(),
        "the stale requester must fail closed"
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .options
            .session_value(&session_name, OptionName::Status),
        Some("off"),
        "the stale customize action must not unset the option"
    );
}

#[tokio::test]
async fn customize_key_mutations_reject_a_replaced_requester() {
    let handler = RequestHandler::new();
    let session_name = SessionName::new("customize-key-requester-aba").expect("valid session");
    handler.create_session(&session_name).await;
    let attach_pid = std::process::id().saturating_add(908);
    let _control_rx = handler.attach_client(attach_pid, &session_name).await;
    open_mode_tree(&handler, attach_pid, &["customize-mode"]).await;
    let stale_identity = handler
        .current_mode_tree_action_identity(attach_pid)
        .await
        .expect("original customize identity");

    let _replacement_rx = handler.attach_client(attach_pid, session_name).await;
    open_mode_tree(&handler, attach_pid, &["customize-mode"]).await;

    let bind = handler
        .handle_bind_key_for_mode_tree(
            rmux_proto::BindKeyRequest {
                table_name: "root".to_owned(),
                key: "x".to_owned(),
                note: None,
                repeat: false,
                command: Some(vec!["display-message no".to_owned()]),
            },
            stale_identity,
        )
        .await;
    assert!(matches!(bind, Response::Error(_)));

    let unbind = handler
        .handle_unbind_key_for_mode_tree(
            rmux_proto::UnbindKeyRequest {
                table_name: "root".to_owned(),
                all: false,
                key: Some("x".to_owned()),
                quiet: true,
            },
            stale_identity,
        )
        .await;
    assert!(matches!(unbind, Response::Error(_)));

    let key = rmux_core::key_string_lookup_string("x").expect("valid key");
    assert!(handler
        .reset_key_binding_for_mode_tree("root", key, stale_identity)
        .await
        .is_err());
}

#[tokio::test]
async fn confirmed_mode_tree_action_rejects_a_same_pid_replacement() {
    use super::super::mode_tree_test_support::{
        install_mode_tree_identity_pause, ModeTreeIdentityPausePoint,
    };

    let label = "mode-tree-confirm-attach-identity";
    let session_name = SessionName::new(label).expect("valid session");
    let (handler, attach_pid, _old_rx) = choose_buffer_action_fixture(label, 903).await;
    let (identity, origin, action) = {
        let mut mode = handler
            .mode_tree_for_action_identity(
                handler
                    .current_mode_tree_action_identity(attach_pid)
                    .await
                    .expect("original tree identity"),
            )
            .await
            .expect("original tree state");
        let identity = handler
            .current_mode_tree_action_identity(attach_pid)
            .await
            .expect("original tree identity");
        let action = handler
            .build_mode_tree(&mut mode, attach_pid)
            .await
            .expect("buffer tree builds")
            .items
            .values()
            .find(|item| {
                matches!(&item.action, ModeTreeAction::Buffer { name, .. } if name == "keep")
            })
            .map(|item| item.action.clone())
            .expect("keep buffer action exists");
        (identity, mode.origin.clone(), action)
    };
    let pause =
        install_mode_tree_identity_pause(ModeTreeIdentityPausePoint::DeferredAction(attach_pid));
    let action_handler = handler.clone();
    let action_task = tokio::spawn(async move {
        action_handler
            .execute_mode_tree_deferred_action(
                identity,
                &origin,
                ModeTreeDeferredAction::DeleteBuffers {
                    targets: vec![action],
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("deferred action reaches its identity commit");

    let _replacement_rx = handler.attach_client(attach_pid, session_name).await;
    open_mode_tree(&handler, attach_pid, &["choose-buffer"]).await;
    pause.release.notify_one();

    assert!(
        action_task
            .await
            .expect("deferred action task joins")
            .is_err(),
        "the stale confirmed action must fail closed"
    );
    assert_eq!(
        handler.state.lock().await.buffers.get("keep"),
        Some(&b"safe"[..])
    );
}

#[tokio::test]
async fn mode_tree_dismisses_after_host_respawn_and_restores_zoom() {
    let label = "mode-tree-respawned-host-identity";
    let (handler, session_name, attach_pid, _control_rx) =
        zoomed_choose_tree_fixture(label, 904).await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 1);

    handler.handle_ok(RespawnPaneRequest::fixture(target)).await;

    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("q dismisses a tree whose host output was respawned"));
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session survives");
    assert!(!session.window_at(0).expect("window survives").is_zoomed());
    assert_eq!(
        state
            .transcript_handle(&PaneTarget::with_window(session_name.clone(), 0, 1))
            .expect("respawned transcript exists")
            .lock()
            .expect("pane transcript mutex")
            .pane_mode_name(),
        None
    );
    drop(state);
    assert!(handler.active_attach.lock().await.by_pid[&attach_pid]
        .mode_tree
        .is_none());
}

#[tokio::test]
async fn mode_tree_dismisses_after_host_relink_and_restores_zoom() {
    let label = "mode-tree-relinked-host-occurrence";
    let (handler, session_name, attach_pid, _control_rx) =
        zoomed_choose_tree_fixture(label, 906).await;
    let original_window_id = {
        let mut state = handler.state.lock().await;
        let window_id = state
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(0))
            .expect("host window exists")
            .id();
        state
            .link_window(LinkWindowRequest::fixture((
                WindowTarget::with_window(session_name.clone(), 0),
                WindowTarget::with_window(session_name.clone(), 2),
            )))
            .expect("host window links");
        state
            .unlink_window(
                rmux_proto::WindowTarget::with_window(session_name.clone(), 0),
                false,
            )
            .expect("old host occurrence unlinks");
        state
            .link_window(LinkWindowRequest::fixture((
                WindowTarget::with_window(session_name.clone(), 2),
                WindowTarget::with_window(session_name.clone(), 0),
            )))
            .expect("replacement host occurrence links");
        window_id
    };
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(0))
            .map(rmux_core::Window::id),
        Some(original_window_id),
        "the slot reuses the same window and pane identities"
    );

    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("q dismisses a tree whose host occurrence was relinked"));
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session survives");
    assert!(
        [0, 2].into_iter().all(|index| session
            .window_at(index)
            .is_some_and(|window| !window.is_zoomed())),
        "dismissal restores zoom across linked aliases"
    );
    assert_eq!(
        state
            .transcript_handle(&PaneTarget::with_window(session_name.clone(), 0, 1))
            .expect("relinked host transcript exists")
            .lock()
            .expect("pane transcript mutex")
            .pane_mode_name(),
        None
    );
    drop(state);
    assert!(handler.active_attach.lock().await.by_pid[&attach_pid]
        .mode_tree
        .is_none());
}

struct MovedModeTreeHostFixture {
    handler: RequestHandler,
    source: SessionName,
    destination: SessionName,
    attach_pid: u32,
    moved_target: PaneTarget,
    moved_window_id: rmux_proto::WindowId,
    control_rx: mpsc::UnboundedReceiver<crate::pane_io::AttachControl>,
}

fn drain_moved_mode_tree_controls(fixture: &mut MovedModeTreeHostFixture) {
    while fixture.control_rx.try_recv().is_ok() {}
}

async fn moved_mode_tree_host_fixture(
    label: &str,
    attach_pid_offset: u32,
) -> MovedModeTreeHostFixture {
    let (handler, source, attach_pid, mut control_rx) =
        zoomed_choose_tree_fixture(label, attach_pid_offset).await;
    let destination = SessionName::new(format!("{label}-destination")).expect("valid session");
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source)
        })
        .await;
    handler.create_session(&destination).await;
    let (moved_window_id, moved_pane_id) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&source)
            .and_then(|session| session.window_at(0))
            .expect("zoomed host window exists");
        (
            window.id(),
            window.active_pane().expect("host pane exists").id(),
        )
    };

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(destination.clone(), 1),
        )))
        .await;
    let moved_target = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&destination)
            .and_then(|session| session.window_at(1))
            .filter(|window| window.id() == moved_window_id)
            .expect("the exact host window moved between sessions");
        let pane = window
            .panes()
            .iter()
            .find(|pane| pane.id() == moved_pane_id)
            .expect("the exact host pane moved with its window");
        assert!(
            window.is_zoomed(),
            "the moved host remains zoomed before dismissal"
        );
        PaneTarget::with_window(destination.clone(), 1, pane.index())
    };
    while control_rx.try_recv().is_ok() {}

    MovedModeTreeHostFixture {
        handler,
        source,
        destination,
        attach_pid,
        moved_target,
        moved_window_id,
        control_rx,
    }
}

#[tokio::test]
async fn mode_tree_dismisses_after_host_window_moves_between_sessions() {
    let fixture = moved_mode_tree_host_fixture("mode-tree-moved-host", 908).await;

    assert!(fixture
        .handler
        .handle_mode_tree_key_event(fixture.attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("q dismisses after the host moves between sessions"));

    let state = fixture.handler.state.lock().await;
    let moved_window = state
        .sessions
        .session(&fixture.destination)
        .and_then(|session| session.window_at(fixture.moved_target.window_index()))
        .filter(|window| window.id() == fixture.moved_window_id)
        .expect("the exact moved window survives");
    assert!(
        !moved_window.is_zoomed(),
        "dismissal restores the moved window zoom"
    );
    assert_eq!(
        state
            .transcript_handle(&fixture.moved_target)
            .expect("moved host transcript exists")
            .lock()
            .expect("pane transcript mutex")
            .pane_mode_name(),
        None
    );
    drop(state);
    assert!(
        fixture.handler.active_attach.lock().await.by_pid[&fixture.attach_pid]
            .mode_tree
            .is_none()
    );
}

#[tokio::test]
async fn mode_tree_moved_host_cleanup_does_not_mutate_a_recreated_source_slot() {
    let mut fixture = moved_mode_tree_host_fixture("mode-tree-moved-host-aba", 909).await;
    fixture
        .handler
        .create_window(NewWindowRequest {
            target_window_index: Some(0),
            ..Fixture::fixture(&fixture.source)
        })
        .await;
    drain_moved_mode_tree_controls(&mut fixture);
    fixture
        .handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(fixture.source.clone(), 0, 0))
        })
        .await;
    drain_moved_mode_tree_controls(&mut fixture);
    let replacement_target = PaneTarget::with_window(fixture.source.clone(), 0, 1);
    let replacement_window_id = fixture
        .handler
        .state
        .lock()
        .await
        .sessions
        .session(&fixture.source)
        .and_then(|session| session.window_at(0))
        .expect("replacement source window exists")
        .id();
    assert_ne!(replacement_window_id, fixture.moved_window_id);
    fixture
        .handler
        .handle_ok(ResizePaneRequest {
            target: replacement_target,
            adjustment: rmux_proto::ResizePaneAdjustment::Zoom,
        })
        .await;
    drain_moved_mode_tree_controls(&mut fixture);

    assert!(fixture
        .handler
        .handle_mode_tree_key_event(fixture.attach_pid, PromptInputEvent::Escape)
        .await
        .expect("Escape dismisses after source-slot recreation"));

    let state = fixture.handler.state.lock().await;
    let moved_window = state
        .sessions
        .session(&fixture.destination)
        .and_then(|session| session.window_at(fixture.moved_target.window_index()))
        .filter(|window| window.id() == fixture.moved_window_id)
        .expect("the exact moved window survives");
    assert!(
        !moved_window.is_zoomed(),
        "the exact moved window is restored"
    );
    let replacement_window = state
        .sessions
        .session(&fixture.source)
        .and_then(|session| session.window_at(0))
        .filter(|window| window.id() == replacement_window_id)
        .expect("the recreated source slot survives");
    assert!(
        replacement_window.is_zoomed(),
        "cleanup must not fall back to the recreated source slot"
    );
}

#[tokio::test]
async fn mode_tree_dismisses_after_host_pane_renumber_without_rezooming() {
    let label = "mode-tree-renumbered-host-pane";
    let (handler, session_name, attach_pid, _control_rx) =
        zoomed_choose_tree_fixture(label, 907).await;
    handler
        .handle_ok(RotateWindowRequest {
            target: WindowTarget::with_window(session_name.clone(), 0),
            direction: rmux_proto::RotateWindowDirection::Down,
            restore_zoom: false,
        })
        .await;

    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Escape)
        .await
        .expect("Escape dismisses after the host pane is renumbered"));
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session survives");
    assert!(!session.window_at(0).expect("window survives").is_zoomed());
    assert_eq!(
        state
            .transcript_handle(&PaneTarget::with_window(session_name.clone(), 0, 0))
            .expect("renumbered host transcript exists")
            .lock()
            .expect("pane transcript mutex")
            .pane_mode_name(),
        None
    );
    drop(state);
    assert!(handler.active_attach.lock().await.by_pid[&attach_pid]
        .mode_tree
        .is_none());
}

#[tokio::test]
async fn mode_tree_activation_rejects_a_same_pid_replacement_without_zoom_leak() {
    use super::super::mode_tree_test_support::{
        install_mode_tree_identity_pause, ModeTreeIdentityPausePoint,
    };

    let handler = RequestHandler::new();
    let session_name =
        SessionName::new("mode-tree-activation-attach-identity").expect("valid session");
    handler.create_session(&session_name).await;
    let attach_pid = std::process::id().saturating_add(905);
    let _old_rx = handler.attach_client(attach_pid, &session_name).await;
    let command = parse_mode_tree(&["choose-tree", "-Z"]);
    let pause =
        install_mode_tree_identity_pause(ModeTreeIdentityPausePoint::Activation(attach_pid));
    let opening_handler = handler.clone();
    let opening = tokio::spawn(async move {
        opening_handler
            .execute_queued_mode_tree(
                attach_pid,
                command,
                &QueueExecutionContext::without_caller_cwd(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("activation reaches its identity commit");

    let (replacement_tx, _replacement_rx) = mpsc::unbounded_channel();
    let replacement_attach_id = handler
        .register_attach(attach_pid, session_name.clone(), replacement_tx)
        .await;
    pause.release.notify_one();

    assert!(
        opening
            .await
            .expect("mode-tree activation task joins")
            .is_err(),
        "the stale activation must fail closed"
    );
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session survives");
    assert!(
        session
            .window_at(session.active_window_index())
            .is_some_and(|window| !window.is_zoomed()),
        "rejected activation must not leak zoom"
    );
    drop(state);
    let active_attach = handler.active_attach.lock().await;
    let replacement = &active_attach.by_pid[&attach_pid];
    assert_eq!(replacement.id, replacement_attach_id);
    assert!(replacement.mode_tree.is_none());
}
