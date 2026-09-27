use super::*;

async fn expect_attach_exited(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    context: &str,
) {
    timeout(Duration::from_secs(2), async {
        while let Some(control) = control_rx.recv().await {
            if matches!(control, AttachControl::Exited) {
                return;
            }
        }
        panic!("attach control channel closed before Exited: {context}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for attached client exit: {context}"));
}

#[tokio::test]
async fn move_window_last_source_session_exits_attached_client() {
    let handler = RequestHandler::new();
    let source = create_session(&handler, "move-attached-source").await;
    let destination = create_session(&handler, "move-attached-destination").await;
    let attach_pid = 81_001;
    let mut control_rx = handler.attach_client(attach_pid, &source).await;
    drain_attach_controls(&mut control_rx).await;

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(destination, 1),
        )))
        .await;
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&source)
        .is_none());

    expect_attach_exited(&mut control_rx, "single removed source session").await;
    assert!(
        !handler
            .active_attach
            .lock()
            .await
            .by_pid
            .contains_key(&attach_pid),
        "removed source session must not leave a stale attached client"
    );
}

#[tokio::test]
async fn move_window_session_target_exits_source_and_refreshes_destination_attaches() {
    let handler = RequestHandler::new();
    let source = create_session(&handler, "move-session-target-attached-source").await;
    let destination = create_session(&handler, "move-session-target-attached-destination").await;
    let source_pid = 81_031;
    let destination_pid = 81_032;
    let mut source_rx = handler.attach_client(source_pid, &source).await;
    let mut destination_rx = handler.attach_client(destination_pid, &destination).await;
    drain_attach_controls(&mut source_rx).await;
    drain_attach_controls(&mut destination_rx).await;

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            &destination,
        )))
        .await;

    expect_attach_exited(&mut source_rx, "removed source with session-only target").await;
    let destination_refresh = timeout(Duration::from_secs(2), destination_rx.recv())
        .await
        .expect("destination attached client is refreshed")
        .expect("destination attach channel stays open");
    let AttachControl::Switch(target) = destination_refresh else {
        panic!("expected destination Switch, got {destination_refresh:?}");
    };
    let target = target.into_target();
    assert_eq!(target.session_name, destination);
    let active_attach = handler.active_attach.lock().await;
    assert!(!active_attach.by_pid.contains_key(&source_pid));
    assert!(active_attach.by_pid.contains_key(&destination_pid));
    drop(active_attach);
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&source)
        .is_none());
}

#[tokio::test]
async fn move_window_last_source_group_exits_all_attached_clients() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "move-attached-group-owner").await;
    let peer = create_grouped_session(&handler, "move-attached-group-peer", &owner).await;
    let destination = create_session(&handler, "move-attached-group-destination").await;
    let owner_pid = 81_011;
    let peer_pid = 81_012;
    let mut owner_rx = handler.attach_client(owner_pid, &owner).await;
    let mut peer_rx = handler.attach_client(peer_pid, &peer).await;
    drain_attach_controls(&mut owner_rx).await;
    drain_attach_controls(&mut peer_rx).await;

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(peer.clone(), 0),
            WindowTarget::with_window(destination, 1),
        )))
        .await;
    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&owner).is_none());
        assert!(state.sessions.session(&peer).is_none());
    }

    expect_attach_exited(&mut owner_rx, "removed source group owner").await;
    expect_attach_exited(&mut peer_rx, "removed source group peer").await;
    let active_attach = handler.active_attach.lock().await;
    assert!(!active_attach.by_pid.contains_key(&owner_pid));
    assert!(!active_attach.by_pid.contains_key(&peer_pid));
}

#[tokio::test]
async fn move_window_source_session_with_remaining_window_keeps_attached_client() {
    let handler = RequestHandler::new();
    let source = create_session(&handler, "move-attached-surviving-source").await;
    insert_window(&handler, &source, 1).await;
    let destination = create_session(&handler, "move-attached-surviving-destination").await;
    let attach_pid = 81_021;
    let mut control_rx = handler.attach_client(attach_pid, &source).await;
    drain_attach_controls(&mut control_rx).await;

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 1),
            WindowTarget::with_window(destination, 1),
        )))
        .await;

    let refresh = timeout(Duration::from_secs(2), control_rx.recv())
        .await
        .expect("surviving source attached client is refreshed")
        .expect("surviving source attach channel stays open");
    assert!(
        matches!(refresh, AttachControl::Switch(_)),
        "surviving source should refresh instead of exit, got {refresh:?}"
    );
    assert!(handler
        .active_attach
        .lock()
        .await
        .by_pid
        .contains_key(&attach_pid));
    let state = handler.state.lock().await;
    let source_session = state
        .sessions
        .session(&source)
        .expect("source session survives with its remaining window");
    assert_eq!(
        source_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
}

#[tokio::test]
async fn move_window_preserves_unrelated_and_grouped_peer_silence_deadlines() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-silence-alpha").await;
    insert_window(&handler, &alpha, 1).await;
    let beta = create_grouped_session(&handler, "move-silence-beta", &alpha).await;
    enable_global_monitor_silence(&handler).await;

    let unrelated = WindowTarget::with_window(alpha.clone(), 0);
    let peer_source = WindowTarget::with_window(beta.clone(), 1);
    let unrelated_before = handler
        .silence_timer_snapshot_for_test(&unrelated)
        .expect("unrelated timer is armed");
    let peer_before = handler
        .silence_timer_snapshot_for_test(&peer_source)
        .expect("grouped peer timer is armed");
    let peer_identity_before = handler
        .silence_timer_identity_for_test(&peer_source)
        .expect("grouped peer timer has stable identity");

    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 1),
            WindowTarget::with_window(alpha.clone(), 3),
        )))
        .await;

    assert_eq!(
        handler.silence_timer_snapshot_for_test(&unrelated),
        Some(unrelated_before),
        "move-window must not rearm an unrelated window"
    );
    let peer_destination = WindowTarget::with_window(beta, 3);
    let peer_after = handler
        .silence_timer_snapshot_for_test(&peer_destination)
        .expect("grouped peer timer follows the move");
    assert_eq!(
        peer_after.1, peer_before.1,
        "the grouped peer deadline follows the moved WindowId"
    );
    assert!(peer_after.0 > peer_before.0);
    let peer_identity_after = handler
        .silence_timer_identity_for_test(&peer_destination)
        .expect("moved grouped peer keeps an identity");
    assert_eq!(
        (peer_identity_after.0, peer_identity_after.1),
        (peer_identity_before.0, peer_identity_before.1)
    );
    assert!(peer_identity_after.2 > peer_identity_before.2);
    assert_eq!(handler.silence_timer_snapshot_for_test(&peer_source), None);
}

#[tokio::test]
async fn move_window_preserves_distinct_duplicate_alias_silence_deadlines() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-duplicate-silence").await;
    link_duplicate_window(&handler, &alpha, 0, 2).await;
    enable_global_monitor_silence(&handler).await;

    let source = WindowTarget::with_window(alpha.clone(), 0);
    let sibling = WindowTarget::with_window(alpha.clone(), 2);
    let destination = WindowTarget::with_window(alpha.clone(), 3);
    let base_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    handler.replace_silence_timer_deadline_for_test(&source, base_deadline);
    handler
        .replace_silence_timer_deadline_for_test(&sibling, base_deadline + Duration::from_secs(7));
    let source_before = handler
        .silence_timer_snapshot_for_test(&source)
        .expect("source alias timer is armed");
    let sibling_before = handler
        .silence_timer_snapshot_for_test(&sibling)
        .expect("sibling alias timer is armed");

    handler
        .handle_ok(MoveWindowRequest::fixture((&source, &destination)))
        .await;

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&sibling),
        Some(sibling_before),
        "the untouched duplicate alias must keep its exact timer"
    );
    let destination_after = handler
        .silence_timer_snapshot_for_test(&destination)
        .expect("moved duplicate alias timer follows its slot");
    assert_eq!(destination_after.1, source_before.1);
    assert!(destination_after.0 > source_before.0);
}

#[tokio::test]
async fn move_window_kill_duplicate_alias_preserves_source_silence_deadline() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-kill-duplicate-silence").await;
    link_duplicate_window(&handler, &alpha, 0, 2).await;
    enable_global_monitor_silence(&handler).await;

    let source = WindowTarget::with_window(alpha.clone(), 0);
    let destination = WindowTarget::with_window(alpha.clone(), 2);
    let base_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    handler.replace_silence_timer_deadline_for_test(&source, base_deadline);
    handler.replace_silence_timer_deadline_for_test(
        &destination,
        base_deadline + Duration::from_secs(11),
    );
    let source_before = handler
        .silence_timer_snapshot_for_test(&source)
        .expect("source alias timer is armed");
    let destination_before = handler
        .silence_timer_snapshot_for_test(&destination)
        .expect("destination alias timer is armed");

    handler
        .handle_ok(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((&source, &destination))
        })
        .await;

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let destination_after = handler
        .silence_timer_snapshot_for_test(&destination)
        .expect("moved source timer replaces the killed alias timer");
    assert_eq!(destination_after.1, source_before.1);
    assert_ne!(destination_after.1, destination_before.1);
    assert!(destination_after.0 > source_before.0.max(destination_before.0));
}

#[tokio::test]
async fn move_window_kill_duplicate_alias_moves_group_peer_alerts_by_occurrence() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "move-alert-duplicate-owner").await;
    link_duplicate_window(&handler, &owner, 0, 2).await;
    let peer = create_grouped_session(&handler, "move-alert-duplicate-peer", &owner).await;
    {
        let mut state = handler.state.lock().await;
        let peer_session = state
            .sessions
            .session_mut(&peer)
            .expect("group peer exists");
        assert!(peer_session.add_winlink_alert_flags(0, rmux_core::WINLINK_BELL));
        assert!(peer_session.add_winlink_alert_flags(2, rmux_core::WINLINK_ACTIVITY));
    }

    handler
        .handle_ok(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(owner.clone(), 0),
                WindowTarget::with_window(owner, 2),
            ))
        })
        .await;

    let state = handler.state.lock().await;
    let peer_session = state.sessions.session(&peer).expect("group peer survives");
    assert!(peer_session.window_at(0).is_none());
    assert_eq!(
        peer_session.winlink_alert_flags(2),
        rmux_core::WINLINK_BELL,
        "the moved source occurrence must replace the killed destination occurrence"
    );
}

#[tokio::test]
async fn move_window_reindex_remaps_group_peer_duplicate_alias_alerts_by_occurrence() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "reindex-alert-duplicate-owner").await;
    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
        )))
        .await;
    link_duplicate_window(&handler, &owner, 1, 2).await;
    let peer = create_grouped_session(&handler, "reindex-alert-duplicate-peer", &owner).await;
    {
        let mut state = handler.state.lock().await;
        let peer_session = state
            .sessions
            .session_mut(&peer)
            .expect("group peer exists");
        assert!(peer_session.add_winlink_alert_flags(1, rmux_core::WINLINK_ACTIVITY));
        assert!(peer_session.add_winlink_alert_flags(2, rmux_core::WINLINK_BELL));
    }

    handler
        .handle_ok(MoveWindowRequest {
            source: None,
            renumber: true,
            ..Fixture::fixture((WindowTarget::with_window(owner.clone(), 0), owner))
        })
        .await;

    let state = handler.state.lock().await;
    let peer_session = state.sessions.session(&peer).expect("group peer survives");
    assert_eq!(
        peer_session.winlink_alert_flags(0),
        rmux_core::WINLINK_ACTIVITY
    );
    assert_eq!(peer_session.winlink_alert_flags(1), rmux_core::WINLINK_BELL);
    assert!(peer_session.window_at(2).is_none());
}

#[tokio::test]
async fn move_window_relative_remaps_group_peer_duplicate_alias_alerts_by_occurrence() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "relative-alert-duplicate-owner").await;
    link_duplicate_window(&handler, &owner, 0, 1).await;
    let peer = create_grouped_session(&handler, "relative-alert-duplicate-peer", &owner).await;
    {
        let mut state = handler.state.lock().await;
        let peer_session = state
            .sessions
            .session_mut(&peer)
            .expect("group peer exists");
        peer_session
            .select_window(1)
            .expect("peer selects the second winlink");
        peer_session
            .select_window(0)
            .expect("peer returns to the first winlink");
        assert!(peer_session.add_winlink_alert_flags(0, rmux_core::WINLINK_ACTIVITY));
        assert!(peer_session.add_winlink_alert_flags(1, rmux_core::WINLINK_BELL));
    }

    handler
        .handle_ok(MoveWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(owner.clone(), 1),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    let state = handler.state.lock().await;
    let peer_session = state.sessions.session(&peer).expect("group peer survives");
    assert_eq!(peer_session.active_window_index(), 0);
    assert_eq!(peer_session.last_window_index(), Some(1));
    assert_eq!(peer_session.winlink_alert_flags(0), rmux_core::WINLINK_BELL);
    assert_eq!(
        peer_session.winlink_alert_flags(1),
        rmux_core::WINLINK_ACTIVITY
    );
}

#[tokio::test]
async fn move_window_kill_duplicate_alias_emits_only_source_unlinked() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-kill-duplicate-lifecycle").await;
    let source = WindowTarget::with_window(alpha.clone(), 0);
    let destination = WindowTarget::with_window(alpha.clone(), 2);
    link_duplicate_window(&handler, &alpha, 0, 2).await;
    let original_window_id = {
        let state = handler.state.lock().await;
        let source_window_id = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .map(rmux_core::Window::id)
            .expect("source alias exists");
        assert_eq!(
            state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(2))
                .map(rmux_core::Window::id),
            Some(source_window_id)
        );
        assert_eq!(state.window_link_count(&alpha, 0), 2);
        source_window_id
    };
    let mut events = handler.subscribe_lifecycle_events();

    handler
        .handle_ok(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((&source, &destination))
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session survives");
    assert!(session.window_at(0).is_none());
    assert_eq!(
        session.window_at(2).map(rmux_core::Window::id),
        Some(original_window_id)
    );
    assert_eq!(state.window_link_count(&alpha, 2), 1);
    drop(state);

    let mut linked_count = 0;
    let mut unlinked_count = 0;
    while let Ok(event) = events.try_recv() {
        match event.event {
            rmux_core::LifecycleEvent::WindowLinked { .. } => linked_count += 1,
            rmux_core::LifecycleEvent::WindowUnlinked { window_id, .. } => {
                assert_eq!(window_id, Some(original_window_id.as_u32()));
                unlinked_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(unlinked_count, 1);
    assert_eq!(
        linked_count, 0,
        "the destination already linked this WindowId before move-window -k"
    );
}

#[tokio::test]
async fn move_window_across_sessions_preserves_silence_deadline_and_identity() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-cross-silence-alpha").await;
    let beta = create_session(&handler, "move-cross-silence-beta").await;
    insert_window(&handler, &alpha, 1).await;
    enable_global_monitor_silence(&handler).await;

    let source = WindowTarget::with_window(alpha.clone(), 1);
    let destination = WindowTarget::with_window(beta.clone(), 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    handler.replace_silence_timer_deadline_for_test(&source, deadline);
    let source_before = handler
        .silence_timer_snapshot_for_test(&source)
        .expect("cross-session source timer is armed");
    let source_identity = handler
        .silence_timer_identity_for_test(&source)
        .expect("cross-session source identity exists");
    let destination_session_id = handler.session_id_for_test(&beta).await;

    handler
        .handle_ok(MoveWindowRequest::fixture((&source, &destination)))
        .await;

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let destination_after = handler
        .silence_timer_snapshot_for_test(&destination)
        .expect("cross-session destination timer exists");
    assert_eq!(destination_after.1, source_before.1);
    assert!(destination_after.0 > source_before.0);
    let destination_identity = handler
        .silence_timer_identity_for_test(&destination)
        .expect("cross-session destination identity exists");
    assert_eq!(destination_identity.0, destination_session_id);
    assert_eq!(destination_identity.1, source_identity.1);
}

#[tokio::test]
async fn move_window_does_not_rearm_expired_unrelated_silence_timer() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-expired-silence").await;
    insert_window(&handler, &alpha, 1).await;
    enable_global_monitor_silence(&handler).await;

    let expired = WindowTarget::with_window(alpha.clone(), 0);
    let source = WindowTarget::with_window(alpha.clone(), 1);
    let destination = WindowTarget::with_window(alpha.clone(), 2);
    let source_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    handler.replace_silence_timer_deadline_for_test(&source, source_deadline);
    let source_before = handler
        .silence_timer_snapshot_for_test(&source)
        .expect("move source timer is armed");
    let expired_identity = handler
        .silence_timer_identity_for_test(&expired)
        .expect("timer to expire is armed");
    handler
        .expire_silence_timer_for_test(
            expired.clone(),
            expired_identity.0,
            expired_identity.1,
            expired_identity.2,
        )
        .await;
    assert_eq!(handler.silence_timer_snapshot_for_test(&expired), None);

    handler
        .handle_ok(MoveWindowRequest::fixture((&source, &destination)))
        .await;

    assert_eq!(
        handler.silence_timer_snapshot_for_test(&expired),
        None,
        "a structural mutation must not restart an already-fired timer"
    );
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let destination_after = handler
        .silence_timer_snapshot_for_test(&destination)
        .expect("moved timer follows the source window");
    assert_eq!(destination_after.1, source_before.1);
}

#[tokio::test]
async fn move_window_across_sessions_arms_timer_when_monitor_silence_becomes_nonzero() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-cross-monitor-zero").await;
    let beta = create_session(&handler, "move-cross-monitor-sixty").await;
    insert_window(&handler, &alpha, 1).await;
    handler
        .set_option(
            ScopeSelector::Session(beta.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;

    let source = WindowTarget::with_window(alpha.clone(), 1);
    let destination = WindowTarget::with_window(beta.clone(), 1);
    let unrelated_destination = WindowTarget::with_window(beta.clone(), 0);
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let unrelated_before = handler
        .silence_timer_snapshot_for_test(&unrelated_destination)
        .expect("existing destination timer is armed");

    handler
        .handle_ok(MoveWindowRequest::fixture((&source, &destination)))
        .await;

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    assert!(
        handler
            .silence_timer_snapshot_for_test(&destination)
            .is_some(),
        "the destination session's nonzero option must arm the moved window"
    );
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&unrelated_destination),
        Some(unrelated_before),
        "arming the moved window must not restart an existing destination timer"
    );
}

async fn assert_cross_session_move_preserves_expired_silence_alert(kill_destination: bool) {
    let label = if kill_destination { "kill" } else { "empty" };
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, format!("move-alert-{label}-alpha")).await;
    let beta = create_session(&handler, format!("move-alert-{label}-beta")).await;
    insert_window(&handler, &alpha, 1).await;
    enable_global_monitor_silence(&handler).await;

    let source = WindowTarget::with_window(alpha.clone(), 1);
    let destination_index = if kill_destination { 0 } else { 1 };
    let destination = WindowTarget::with_window(beta.clone(), destination_index);
    if kill_destination {
        assert!(
            handler
                .silence_timer_snapshot_for_test(&destination)
                .is_some(),
            "the occupied destination starts with its own timer"
        );
    } else {
        assert_eq!(handler.silence_timer_snapshot_for_test(&destination), None);
    }
    let source_identity = handler
        .silence_timer_identity_for_test(&source)
        .expect("source timer identity exists before expiry");
    handler
        .expire_silence_timer_for_test(
            source.clone(),
            source_identity.0,
            source_identity.1,
            source_identity.2,
        )
        .await;
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    {
        let state = handler.state.lock().await;
        assert!(
            state
                .sessions
                .session(&alpha)
                .expect("source session exists before move")
                .winlink_alert_flags(source.window_index())
                .contains(rmux_core::WINLINK_SILENCE),
            "the real timer expiry marks the source winlink silent"
        );
    }

    handler
        .handle_ok(MoveWindowRequest {
            kill_destination,
            ..Fixture::fixture((&source, &destination))
        })
        .await;

    {
        let state = handler.state.lock().await;
        let source_session = state
            .sessions
            .session(&alpha)
            .expect("source session survives the move");
        assert!(source_session.window_at(source.window_index()).is_none());
        assert!(source_session
            .winlink_alert_flags(source.window_index())
            .is_empty());
        let destination_session = state
            .sessions
            .session(&beta)
            .expect("destination session survives the move");
        assert_eq!(
            destination_session
                .window_at(destination.window_index())
                .expect("moved window exists at destination")
                .id(),
            source_identity.1,
        );
        assert!(
            destination_session
                .winlink_alert_flags(destination.window_index())
                .contains(rmux_core::WINLINK_SILENCE),
            "the silence flag follows the moved WindowId"
        );
    }
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&destination),
        None,
        "moving an already-expired window must not arm a second timer"
    );
}

#[tokio::test]
async fn move_window_across_sessions_preserves_expired_silence_alert_for_empty_and_killed_slots() {
    assert_cross_session_move_preserves_expired_silence_alert(false).await;
    assert_cross_session_move_preserves_expired_silence_alert(true).await;
}

#[tokio::test]
async fn move_window_across_sessions_migrates_the_terminal_ownership_map() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 1).await;

    let moved_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .expect("alpha should exist")
            .window_at(1)
            .expect("window 1 should exist")
            .pane(0)
            .expect("pane 0 should exist")
            .id()
    };

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest::fixture((
                WindowTarget::with_window(alpha.clone(), 1),
                WindowTarget::with_window(beta.clone(), 4),
            )))
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: beta.clone(),
            target: Some(WindowTarget::with_window(beta.clone(), 4)),
        }
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should exist");
    let beta_session = state.sessions.session(&beta).expect("beta should exist");
    assert_eq!(
        alpha_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 4]
    );
    assert_eq!(
        beta_session
            .window_at(4)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(moved_pane_id)
    );
    state
        .pane_profile_in_window(&beta, 4, 0)
        .expect("moved pane terminal should exist in the destination session");
    assert_eq!(
        state.pane_profile_in_window(&alpha, 1, 0).unwrap_err(),
        rmux_proto::RmuxError::invalid_target("alpha:1", "window index does not exist in session")
    );
}

#[tokio::test]
async fn move_window_within_session_moves_linked_slot_metadata() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;

    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(beta.clone(), 1),
        )))
        .await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(alpha.clone(), 2),
            )))
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: Some(WindowTarget::with_window(alpha.clone(), 2)),
        }
    );

    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&alpha, 2), 2);
        assert_eq!(state.window_link_count(&beta, 1), 2);
        assert_eq!(state.window_link_count(&alpha, 0), 1);
        assert_eq!(
            state.window_linked_sessions_list(&beta, 1),
            vec![alpha.clone(), beta.clone()]
        );
    }

    handler
        .handle_ok(RenameWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 1),
            name: "logs".to_owned(),
        })
        .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(2))
            .and_then(|window| window.name()),
        Some("logs")
    );
    assert_eq!(
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.name()),
        Some("logs")
    );
}

#[tokio::test]
async fn move_window_from_group_peer_moves_runtime_state_and_removes_empty_group() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_grouped_session(&handler, "beta", &alpha).await;
    let gamma = create_session(&handler, "gamma").await;

    let moved_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("grouped pane should exist")
    };

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest::fixture((
                WindowTarget::with_window(beta.clone(), 0),
                WindowTarget::with_window(gamma.clone(), 1),
            )))
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: gamma.clone(),
            target: Some(WindowTarget::with_window(gamma.clone(), 1)),
        }
    );

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alpha).is_none());
    assert!(state.sessions.session(&beta).is_none());
    assert_eq!(
        state
            .sessions
            .session(&gamma)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(moved_pane_id)
    );
    state
        .pane_profile_in_window(&gamma, 1, 0)
        .expect("moved group pane terminal should live in the destination session");
}

#[tokio::test]
async fn move_window_rejects_cross_session_move_within_same_session_group() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_grouped_session(&handler, "beta", &alpha).await;

    let shared_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("grouped pane should exist before move-window")
    };

    let response = handler
        .handle(Request::MoveWindow(MoveWindowRequest::fixture((
            WindowTarget::with_window(beta.clone(), 0),
            WindowTarget::with_window(alpha.clone(), 5),
        ))))
        .await;

    assert!(
        matches!(&response, Response::Error(error) if error.error.to_string().contains("sessions are grouped")),
        "expected grouped-session rejection, got {response:?}"
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should remain");
    let beta_session = state.sessions.session(&beta).expect("beta should remain");
    assert_eq!(
        alpha_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(alpha_session.pane_id_in_window(0, 0), Some(shared_pane_id));
    assert_eq!(beta_session.pane_id_in_window(0, 0), Some(shared_pane_id));
    state
        .pane_profile_in_window(&alpha, 0, 0)
        .expect("alpha pane terminal should remain");
    state
        .pane_profile_in_window(&beta, 0, 0)
        .expect("beta grouped pane terminal should remain");
}

#[tokio::test]
async fn move_window_relative_rejects_cross_session_move_within_same_session_group() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_grouped_session(&handler, "beta", &alpha).await;

    let response = handler
        .handle(Request::MoveWindow(MoveWindowRequest {
            after: true,
            ..Fixture::fixture((
                WindowTarget::with_window(beta.clone(), 0),
                WindowTarget::with_window(alpha.clone(), 0),
            ))
        }))
        .await;

    assert!(
        matches!(&response, Response::Error(error) if error.error.to_string().contains("sessions are grouped")),
        "expected grouped-session rejection, got {response:?}"
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should remain");
    let beta_session = state.sessions.session(&beta).expect("beta should remain");
    assert_eq!(
        alpha_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
}

#[tokio::test]
async fn move_window_from_group_peer_linked_source_removes_empty_group() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_grouped_session(&handler, "beta", &alpha).await;
    let gamma = create_session(&handler, "gamma").await;
    let delta = create_session(&handler, "delta").await;

    let linked_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("grouped linked pane should exist")
    };

    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(gamma.clone(), 1),
        )))
        .await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest::fixture((
                WindowTarget::with_window(beta.clone(), 0),
                WindowTarget::with_window(delta.clone(), 1),
            )))
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: delta.clone(),
            target: Some(WindowTarget::with_window(delta.clone(), 1)),
        }
    );

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alpha).is_none());
    assert!(state.sessions.session(&beta).is_none());
    assert_eq!(
        state
            .sessions
            .session(&delta)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(linked_pane_id)
    );
    assert_eq!(
        state
            .sessions
            .session(&gamma)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(linked_pane_id)
    );
    state
        .pane_profile_in_window(&delta, 1, 0)
        .expect("moved linked pane should live in the target runtime");
    state
        .pane_profile_in_window(&gamma, 1, 0)
        .expect("surviving linked peer should keep runtime access");
    assert_eq!(state.window_link_count(&delta, 1), 2);
    assert_eq!(state.window_link_count(&gamma, 1), 2);
}

#[tokio::test]
async fn move_window_kill_destination_preserves_surviving_linked_window_runtime() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let gamma = create_session(&handler, "gamma").await;
    let delta = create_session(&handler, "delta").await;

    let (source_pane_id, linked_pane_id) = {
        let state = handler.state.lock().await;
        (
            state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(0))
                .and_then(|window| window.pane(0))
                .map(|pane| pane.id())
                .expect("alpha pane should exist"),
            state
                .sessions
                .session(&gamma)
                .and_then(|session| session.window_at(0))
                .and_then(|window| window.pane(0))
                .map(|pane| pane.id())
                .expect("gamma pane should exist"),
        )
    };

    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(gamma.clone(), 0),
            WindowTarget::with_window(delta.clone(), 1),
        )))
        .await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 0),
                    WindowTarget::with_window(gamma.clone(), 0),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: gamma.clone(),
            target: Some(WindowTarget::with_window(gamma.clone(), 0)),
        }
    );

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alpha).is_none());
    assert_eq!(
        state
            .sessions
            .session(&gamma)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(source_pane_id)
    );
    assert_eq!(
        state
            .sessions
            .session(&delta)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(linked_pane_id)
    );
    state
        .pane_profile_in_window(&gamma, 0, 0)
        .expect("moved source pane should live in gamma");
    state
        .pane_profile_in_window(&delta, 1, 0)
        .expect("surviving linked pane should keep a runtime after overwrite");
    assert_eq!(state.window_link_count(&gamma, 0), 1);
    assert_eq!(state.window_link_count(&delta, 1), 1);
}

#[tokio::test]
async fn move_window_within_session_kill_destination_preserves_surviving_linked_runtime() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 2).await;

    let (source_pane_id, linked_pane_id) = {
        let state = handler.state.lock().await;
        (
            state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(2))
                .and_then(|window| window.pane(0))
                .map(|pane| pane.id())
                .expect("alpha:2 pane should exist"),
            state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(0))
                .and_then(|window| window.pane(0))
                .map(|pane| pane.id())
                .expect("alpha:0 pane should exist"),
        )
    };

    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(beta.clone(), 1),
        )))
        .await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 2),
                    WindowTarget::with_window(alpha.clone(), 0),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: Some(WindowTarget::with_window(alpha.clone(), 0)),
        }
    );

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(source_pane_id)
    );
    assert_eq!(
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(linked_pane_id)
    );
    state
        .pane_profile_in_window(&alpha, 0, 0)
        .expect("moved source pane should remain available");
    state
        .pane_profile_in_window(&beta, 1, 0)
        .expect("surviving linked peer should keep its runtime");
    assert_eq!(state.window_link_count(&alpha, 0), 1);
    assert_eq!(state.window_link_count(&beta, 1), 1);
}

#[tokio::test]
async fn move_window_within_session_restores_the_killed_destination_when_resize_fails() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;

    let (source_pane_id, destination_pane_id, stable_source, stable_destination) = {
        let mut state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha should exist");
        let source_pane_id = session
            .window_at(0)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("window 0 pane should exist");
        let destination_pane_id = session
            .window_at(1)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("window 1 pane should exist");
        let stable_source = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Pane(PaneTarget::with_window(alpha.clone(), 0, 0)),
        )
        .expect("capture alpha:0.0 identity");
        let stable_destination = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Pane(PaneTarget::with_window(alpha.clone(), 1, 0)),
        )
        .expect("capture alpha:1.0 identity");
        (
            source_pane_id,
            destination_pane_id,
            stable_source,
            stable_destination,
        )
    };

    {
        let mut state = handler.state.lock().await;
        state.fail_next_resize_for_test();
    }

    let response = handler
        .handle(Request::MoveWindow(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(alpha.clone(), 1),
            ))
        }))
        .await;

    assert_eq!(
        response,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Server(
                "injected pane terminal resize failure".to_owned()
            ),
        })
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(session.pane_id_in_window(0, 0), Some(source_pane_id));
    assert_eq!(session.pane_id_in_window(1, 0), Some(destination_pane_id));
    state
        .pane_profile_in_window(&alpha, 0, 0)
        .expect("source pane terminal should be restored");
    state
        .pane_profile_in_window(&alpha, 1, 0)
        .expect("destination pane terminal should be restored");
    assert!(
        stable_source.is_current(&state),
        "rollback must restore the source pane's tenancy stamp, not just its index"
    );
    assert!(
        stable_destination.is_current(&state),
        "rollback must restore the killed destination's tenancy stamp"
    );
}

#[tokio::test]
async fn move_window_reindex_compacts_sparse_window_indices() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 3).await;
    insert_window(&handler, &alpha, 7).await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                source: None,
                renumber: true,
                ..Fixture::fixture((WindowTarget::with_window(alpha.clone(), 0), &alpha))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: None,
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[tokio::test]
async fn move_window_reindex_with_source_ignores_source_and_renumbers_target_session() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 3).await;
    insert_window(&handler, &beta, 4).await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                renumber: true,
                ..Fixture::fixture((WindowTarget::with_window(alpha.clone(), 3), &beta))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: beta.clone(),
            target: None,
        }
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should exist");
    assert!(alpha_session.window_at(3).is_some());
    let beta_session = state.sessions.session(&beta).expect("beta should exist");
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn move_window_reindex_ignores_source_in_target_without_window_lifecycle_events() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "reindex-source-in-target").await;
    insert_window(&handler, &alpha, 3).await;
    insert_window(&handler, &alpha, 7).await;
    let mut events = handler.subscribe_lifecycle_events();

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                renumber: true,
                ..Fixture::fixture((WindowTarget::with_window(alpha.clone(), 7), &alpha))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: None,
        }
    );
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .expect("target session survives reindex")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    drop(state);

    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event.event,
                rmux_core::LifecycleEvent::WindowLinked { .. }
                    | rmux_core::LifecycleEvent::WindowUnlinked { .. }
            ),
            "move-window -r must not synthesize window lifecycle events: {event:?}"
        );
    }
}

#[tokio::test]
async fn move_window_reindex_with_window_target_renumbers_target_session() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 5).await;
    insert_window(&handler, &alpha, 9).await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                source: None,
                renumber: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 0),
                    WindowTarget::with_window(alpha.clone(), 9),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: None,
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[tokio::test]
async fn move_window_reindex_with_source_and_window_target_ignores_source() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 2).await;
    insert_window(&handler, &alpha, 5).await;
    insert_window(&handler, &beta, 4).await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                renumber: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 5),
                    WindowTarget::with_window(beta.clone(), 4),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: beta.clone(),
            target: None,
        }
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        alpha_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 2, 5]
    );
    let beta_session = state.sessions.session(&beta).expect("beta should exist");
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn move_window_after_source_already_after_target_matches_tmux_gap_shape() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;

    let (source_pane_id, trailing_pane_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha should exist");
        (
            session
                .pane_id_in_window(1, 0)
                .expect("source pane should exist"),
            session
                .pane_id_in_window(2, 0)
                .expect("trailing pane should exist"),
        )
    };

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                detached: false,
                after: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 1),
                    WindowTarget::with_window(alpha.clone(), 0),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: Some(WindowTarget::with_window(alpha.clone(), 1)),
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 3]
    );
    assert_eq!(session.pane_id_in_window(1, 0), Some(source_pane_id));
    assert_eq!(session.pane_id_in_window(3, 0), Some(trailing_pane_id));
    assert_eq!(session.active_window_index(), 1);
}

#[tokio::test]
async fn move_window_before_source_is_target_matches_tmux_gap_shape() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;

    let (source_pane_id, next_pane_id, trailing_pane_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha should exist");
        (
            session
                .pane_id_in_window(0, 0)
                .expect("source pane should exist"),
            session
                .pane_id_in_window(1, 0)
                .expect("next pane should exist"),
            session
                .pane_id_in_window(2, 0)
                .expect("trailing pane should exist"),
        )
    };

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                detached: false,
                before: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 0),
                    WindowTarget::with_window(alpha.clone(), 0),
                ))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: Some(WindowTarget::with_window(alpha.clone(), 0)),
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 2, 3]
    );
    assert_eq!(session.pane_id_in_window(0, 0), Some(source_pane_id));
    assert_eq!(session.pane_id_in_window(2, 0), Some(next_pane_id));
    assert_eq!(session.pane_id_in_window(3, 0), Some(trailing_pane_id));
    assert_eq!(session.active_window_index(), 0);
}

#[tokio::test]
async fn move_window_reindex_starts_at_base_index() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 3).await;
    insert_window(&handler, &alpha, 7).await;

    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::BaseIndex,
            "2",
        )
        .await;

    assert_eq!(
        handler
            .handle_ok(MoveWindowRequest {
                source: None,
                renumber: true,
                ..Fixture::fixture((WindowTarget::with_window(alpha.clone(), 0), &alpha))
            })
            .await,
        rmux_proto::MoveWindowResponse {
            session_name: alpha.clone(),
            target: None,
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
}

#[tokio::test]
async fn move_window_reindex_remaps_window_metadata() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 2).await;
    insert_window(&handler, &alpha, 3).await;

    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 3)),
            OptionName::WindowStyle,
            "fg=colour3",
        )
        .await;
    handler
        .handle_ok(rmux_proto::SetHookRequest::fixture((
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 3)),
            HookName::WindowLayoutChanged,
            "display-message remapped",
        )))
        .await;

    handler
        .handle_ok(MoveWindowRequest {
            source: None,
            renumber: true,
            ..Fixture::fixture((WindowTarget::with_window(alpha.clone(), 0), &alpha))
        })
        .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .options
            .resolve_for_window(&alpha, 2, OptionName::WindowStyle),
        Some("fg=colour3")
    );
    assert_eq!(
        state.hooks.window_command(
            &WindowTarget::with_window(alpha, 2),
            HookName::WindowLayoutChanged
        ),
        Some("display-message remapped")
    );
}

#[tokio::test]
async fn move_window_across_sessions_restores_terminal_ownership_when_resize_fails() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &beta, 4).await;

    let (moved_pane_id, replaced_pane_id, stable_moved, stable_replaced) = {
        let mut state = handler.state.lock().await;
        let alpha_session = state.sessions.session(&alpha).expect("alpha should exist");
        let moved_pane_id = alpha_session
            .window_at(1)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("alpha window 1 pane should exist");
        let beta_session = state.sessions.session(&beta).expect("beta should exist");
        let replaced_pane_id = beta_session
            .window_at(4)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("beta window 4 pane should exist");
        let stable_moved = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Pane(PaneTarget::with_window(alpha.clone(), 1, 0)),
        )
        .expect("capture alpha:1.0 identity");
        let stable_replaced = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Pane(PaneTarget::with_window(beta.clone(), 4, 0)),
        )
        .expect("capture beta:4.0 identity");
        (
            moved_pane_id,
            replaced_pane_id,
            stable_moved,
            stable_replaced,
        )
    };

    {
        let mut state = handler.state.lock().await;
        state.fail_next_resize_for_test();
    }

    let response = handler
        .handle(Request::MoveWindow(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 1),
                WindowTarget::with_window(beta.clone(), 4),
            ))
        }))
        .await;

    assert_eq!(
        response,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Server(
                "injected pane terminal resize failure".to_owned()
            ),
        })
    );

    let state = handler.state.lock().await;
    let alpha_session = state.sessions.session(&alpha).expect("alpha should exist");
    let beta_session = state.sessions.session(&beta).expect("beta should exist");
    assert_eq!(
        alpha_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 4]
    );
    assert_eq!(alpha_session.pane_id_in_window(1, 0), Some(moved_pane_id));
    assert_eq!(beta_session.pane_id_in_window(4, 0), Some(replaced_pane_id));
    state
        .pane_profile_in_window(&alpha, 1, 0)
        .expect("moved pane terminal should return to the source session");
    state
        .pane_profile_in_window(&beta, 4, 0)
        .expect("replaced pane terminal should return to the destination session");
    assert!(
        stable_moved.is_current(&state),
        "rollback must restore the moved pane's tenancy stamp in its source session"
    );
    assert!(
        stable_replaced.is_current(&state),
        "rollback must restore the replaced destination pane's tenancy stamp"
    );
}
