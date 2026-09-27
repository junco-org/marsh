use super::*;

#[tokio::test]
async fn forward_attach_clears_persistent_overlay_with_fresh_switch_frame() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name, b"BASE-OLD", None));

    attach.assert_initial_render("BASE-OLD").await;

    control_tx
        .send(persistent_overlay_control(b"MENU-OLD", 0, 1, 7))
        .expect("send initial persistent overlay");
    let _ = read_attach_data_until(&mut attach.peer, b"MENU-OLD").await;

    control_tx
        .send(AttachControl::AdvancePersistentOverlayState(8))
        .expect("send overlay state advance");
    control_tx
        .send(persistent_overlay_control(b"", 0, 2, 8))
        .expect("send persistent overlay clear");
    control_tx
        .send(switch_control(&session_name, b"BASE-FRESH", None))
        .expect("send refreshed attach target");

    let refresh = read_attach_data_until(&mut attach.peer, b"BASE-FRESH").await;
    let refresh_text = String::from_utf8_lossy(&refresh);
    assert!(
        !refresh_text.contains("BASE-OLD"),
        "overlay teardown must not paint stale base before the fresh switch: {refresh_text:?}"
    );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_does_not_paint_stale_base_while_overlay_dismiss_refresh_is_pending() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name, b"STALE-BASE", None));

    let _ = read_attach_data_until(&mut attach.peer, b"STALE-BASE").await;
    control_tx
        .send(persistent_overlay_control(b"MENU-OLD", 0, 1, 7))
        .expect("send initial persistent overlay");
    let _ = read_attach_data_until(&mut attach.peer, b"MENU-OLD").await;

    control_tx
        .send(AttachControl::AdvancePersistentOverlayState(8))
        .expect("send overlay state advance");
    let pending_bytes = read_attach_data_for(&mut attach.peer, Duration::from_millis(100)).await;
    let pending_text = String::from_utf8_lossy(&pending_bytes);
    assert!(
        !pending_text.contains("STALE-BASE"),
        "state advance must wait for the fresh switch instead of repainting a stale base: {pending_text:?}"
    );

    control_tx
        .send(switch_control(&session_name, b"FRESH-BASE", None))
        .expect("send refreshed attach target");
    let refresh = read_attach_data_until(&mut attach.peer, b"FRESH-BASE").await;
    let refresh_text = String::from_utf8_lossy(&refresh);
    assert!(
        !refresh_text.contains("STALE-BASE"),
        "overlay teardown must be resolved by the fresh switch: {refresh_text:?}"
    );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_dismiss_epoch_rejects_queued_stale_tree_frames() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name, b"BASE-BEFORE-TREE", None));

    let _ = read_attach_data_until(&mut attach.peer, b"BASE-BEFORE-TREE").await;
    control_tx
        .send(persistent_overlay_control(b"TREE-STATE-1", 0, 1, 1))
        .expect("send state-1 tree frame");
    let _ = read_attach_data_until(&mut attach.peer, b"TREE-STATE-1").await;

    // Dismissal publishes the epoch before it can finish producing the fresh
    // base repaint. Concurrent refresh work may still enqueue state-1 frames;
    // the attach loop must reject all of them once state 2 is visible.
    attach.persistent_overlay_epoch.store(2, Ordering::SeqCst);
    control_tx
        .send(switch_control(
            &session_name,
            b"STALE-BASE-STATE-1",
            Some(1),
        ))
        .expect("send stale state-1 switch");
    control_tx
        .send(persistent_overlay_control(b"STALE-TREE-STATE-1", 1, 2, 1))
        .expect("send stale state-1 overlay");
    control_tx
        .send(switch_control(&session_name, b"BASE-AFTER-DISMISS", None))
        .expect("send fresh base switch");

    let refreshed = read_attach_data_until(&mut attach.peer, b"BASE-AFTER-DISMISS").await;
    let refreshed_text = String::from_utf8_lossy(&refreshed);
    assert!(
        !refreshed_text.contains("STALE-BASE-STATE-1")
            && !refreshed_text.contains("STALE-TREE-STATE-1")
            && !refreshed_text.contains("TREE-STATE-1"),
        "state-2 dismissal must fence every queued state-1 frame before the fresh base: {refreshed_text:?}"
    );

    control_tx
        .send(switch_control(
            &session_name,
            b"STALE-BASE-AFTER-FRESH",
            Some(1),
        ))
        .expect("send late stale state-1 switch");
    control_tx
        .send(persistent_overlay_control(
            b"STALE-TREE-AFTER-FRESH",
            2,
            3,
            1,
        ))
        .expect("send late stale state-1 overlay");
    control_tx
        .send(AttachControl::Write(b"AFTER-STALE-MARKER".to_vec()))
        .expect("send reliable marker after stale controls");

    let after_stale = read_attach_data_until(&mut attach.peer, b"AFTER-STALE-MARKER").await;
    let after_stale_text = String::from_utf8_lossy(&after_stale);
    assert!(
        !after_stale_text.contains("STALE-BASE-AFTER-FRESH")
            && !after_stale_text.contains("STALE-TREE-AFTER-FRESH"),
        "state-2 dismissal must keep fencing state-1 frames after the fresh base: {after_stale_text:?}"
    );

    attach.assert_stops_healthy().await;
}

/// Forwards a target passing Kitty graphics and/or sixel through, shows a persistent overlay,
/// and asserts the `kind` passthrough published under it stays `hidden` until the overlay
/// clears and is then `flushed`.
async fn assert_passthrough_deferred_until_overlay_clears(
    kind: &str,
    kitty_graphics_passthrough: bool,
    sixel_passthrough: bool,
    passthrough: TerminalPassthrough,
    hidden: &[u8],
    flushed: &[u8],
) {
    let pane_output = pane_output_channel();
    let target = test_attach_target_with_protocols(
        &session_name("alpha"),
        b"BASE-0",
        &pane_output,
        kitty_graphics_passthrough,
        sixel_passthrough,
    );
    let (mut attach, control_tx) = AttachForwarder::unregistered(target);

    let _ = read_attach_data_until(&mut attach.peer, b"BASE-0").await;
    control_tx
        .send(persistent_overlay_control(b"MENU", 0, 1, 7))
        .expect("send persistent overlay");
    let _ = read_attach_data_until(&mut attach.peer, b"MENU").await;

    pane_output.send_for_generation_with_passthroughs(None, b"tick".to_vec(), vec![passthrough]);
    let pending = read_attach_data_for(&mut attach.peer, Duration::from_millis(100)).await;
    assert!(
        !contains_bytes(&pending, hidden),
        "{kind} passthrough should not be emitted while overlay is visible: {pending:?}"
    );

    control_tx
        .send(persistent_overlay_control(b"", 0, 2, 8))
        .expect("send persistent overlay clear");

    let rendered = read_attach_data_until(&mut attach.peer, flushed).await;
    assert!(
        contains_bytes(&rendered, flushed),
        "deferred {kind} passthrough should flush after overlay clears: {rendered:?}"
    );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_defers_kitty_passthroughs_until_persistent_overlay_clears() {
    assert_passthrough_deferred_until_overlay_clears(
        "kitty",
        true,
        false,
        TerminalPassthrough::kitty_graphics(0, 0, b"Gf=100;AAA"),
        b"\x1b_G",
        b"\x1b_Gf=100;AAA\x1b\\",
    )
    .await;
}

#[tokio::test]
async fn forward_attach_defers_sixel_passthroughs_until_persistent_overlay_clears() {
    assert_passthrough_deferred_until_overlay_clears(
        "sixel",
        false,
        true,
        TerminalPassthrough::sixel(0, 0, b"q#0!10~"),
        b"\x1bPq#0!10~\x1b\\",
        b"\x1bPq#0!10~\x1b\\",
    )
    .await;
}

#[tokio::test]
async fn forward_attach_drops_kitty_passthroughs_when_target_gate_is_disabled() {
    let pane_output = pane_output_channel();
    let target =
        test_attach_target_with_output(&session_name("alpha"), b"BASE-0", &pane_output, false);
    let (mut attach, _control_tx) = AttachForwarder::unregistered(target);

    let _ = read_attach_data_until(&mut attach.peer, b"BASE-0").await;
    pane_output.send_for_generation_with_passthroughs(
        None,
        b"tick".to_vec(),
        vec![TerminalPassthrough::kitty_graphics(0, 0, b"Gf=100;AAA")],
    );

    let pending = read_attach_data_for(&mut attach.peer, Duration::from_millis(100)).await;
    assert!(
        !contains_bytes(&pending, b"\x1b_G"),
        "disabled kitty passthrough target should never emit ESC_G: {pending:?}"
    );

    attach.assert_stops_healthy().await;
}
