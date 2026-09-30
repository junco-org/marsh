use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

#[tokio::test]
async fn moved_pane_stays_live_while_its_destroyed_source_window_retires() {
    let handler = RequestHandler::new();
    let mut state = handler.state.lock().await;
    let session_name = create_session_in_state(&mut state, "retained-pane-move");
    state
        .sessions
        .session_mut(&session_name)
        .expect("session exists")
        .create_window(terminal_size())
        .expect("create destination window");

    let source_pane = PaneTarget::with_window(session_name.clone(), 0, 0);
    let source_window = WindowTarget::with_window(session_name.clone(), 0);
    let pane_lease = state
        .capture_retained_pane_lifecycle_target(&source_pane)
        .expect("capture pane lease");
    let window_lease = state
        .capture_retained_window_lifecycle_target(&source_window)
        .expect("capture window lease");
    let pane_id = state
        .sessions
        .session(&session_name)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0))
        .expect("source pane exists")
        .id();

    state
        .sessions
        .session_mut(&session_name)
        .expect("session exists")
        .join_pane(
            SessionPaneTarget::new(0, 0),
            SessionPaneTarget::new(1, 0),
            PaneJoinOptions::new(SplitDirection::Vertical, false, false, false, None),
        )
        .expect("move last source pane into destination window");
    state.retire_removed_lifecycle_targets();

    let resolved_pane = match pane_lease.resolve(&state) {
        LeaseResolution::Live(Target::Pane(target)) => target,
        resolution => panic!("moved pane should stay live, got {resolution:?}"),
    };
    assert_eq!(resolved_pane.window_index(), 1);
    assert_eq!(
        state
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(resolved_pane.window_index()))
            .and_then(|window| window.pane(resolved_pane.pane_index()))
            .map(rmux_core::Pane::id),
        Some(pane_id)
    );
    assert_retired(&window_lease, &state);

    state.retire_respawned_lifecycle_panes(&[pane_id]);
    assert_retired(&pane_lease, &state);
}

#[tokio::test]
async fn retained_targets_follow_surviving_aliases_deterministically() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "retained-alias-alpha").await;
    let beta = SessionSpec::create(&handler, "retained-alias-beta").await;
    let gamma = SessionSpec::create(&handler, "retained-alias-gamma").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    for (session_name, window_index) in [(&gamma, 3), (&gamma, 2), (&beta, 1)] {
        TestRequest::send_ok(
            &handler,
            LinkWindowRequest::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(session_name.clone(), window_index),
            )),
        )
        .await;
    }

    let source_window = WindowTarget::with_window(alpha.clone(), 0);
    let source_pane = PaneTarget::with_window(alpha.clone(), 0, 0);
    let (window_lease, pane_lease, stable_window, stable_pane) = {
        let mut state = handler.state.lock().await;
        let stable_window = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Window(source_window.clone()),
        )
        .expect("capture stable window identity");
        let stable_pane = crate::handler::StableTargetIdentity::capture(
            &mut state,
            Target::Pane(source_pane.clone()),
        )
        .expect("capture stable pane identity");
        assert!(stable_window.is_current(&state));
        assert!(stable_pane.is_current(&state));
        let window_lease = state
            .capture_retained_window_lifecycle_target(&source_window)
            .expect("capture retained window alias");
        let pane_lease = state
            .capture_retained_pane_lifecycle_target(&source_pane)
            .expect("capture retained pane alias");
        (window_lease, pane_lease, stable_window, stable_pane)
    };

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: source_window,
            kill_if_last: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert!(
        !stable_window.is_current(&state),
        "the unlinked alpha:0 slot must not be reacquired by a surviving alias"
    );
    assert!(!stable_pane.is_current(&state));
    assert_eq!(stable_pane.resolve_current_pane_target(&state), None);
    assert_eq!(
        window_lease.resolve(&state),
        LeaseResolution::Live(Target::Window(WindowTarget::with_window(gamma.clone(), 2,)))
    );
    assert_eq!(
        pane_lease.resolve(&state),
        LeaseResolution::Live(Target::Pane(PaneTarget::with_window(gamma, 2, 0)))
    );
}

#[tokio::test]
async fn surviving_alias_becomes_the_retirement_slot_after_original_slot_reuse() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "retained-cursor-alpha").await;
    let beta = SessionSpec::create(&handler, "retained-cursor-beta").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let source = WindowTarget::with_window(alpha.clone(), 0);
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source, WindowTarget::with_window(beta.clone(), 1))),
    )
    .await;
    let lease = {
        let state = handler.state.lock().await;
        state
            .capture_retained_window_lifecycle_target(&source)
            .expect("capture retained aliased window")
    };

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: source,
            kill_if_last: false,
        },
    )
    .await;
    {
        let state = handler.state.lock().await;
        assert_eq!(
            lease.resolve(&state),
            LeaseResolution::Live(Target::Window(WindowTarget::with_window(beta.clone(), 1)))
        );
    }

    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(0),
            ..Fixture::fixture(alpha)
        })
        .await;

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(beta, 1),
            kill_if_last: true,
        },
    )
    .await;
    let state = handler.state.lock().await;
    assert_retired(&lease, &state);
}

#[tokio::test]
async fn respawn_boundary_retires_the_old_pane_lifetime_even_when_id_survives() {
    let handler = RequestHandler::new();
    let mut state = handler.state.lock().await;
    let session_name = create_session_in_state(&mut state, "retained-pane-respawn");
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    let lease = state
        .capture_retained_pane_lifecycle_target(&target)
        .expect("capture pane lease");
    let pane_id = state
        .sessions
        .session(&session_name)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0))
        .expect("pane exists")
        .id();

    state.retire_respawned_lifecycle_panes(&[pane_id]);

    assert_retired(&lease, &state);
    assert!(
        state
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .is_some_and(|pane| pane.id() == pane_id),
        "respawn retirement is a lifetime boundary, not numeric-id removal"
    );
}

#[tokio::test]
async fn same_transaction_slot_replacement_invalidates_instead_of_retargeting() {
    let handler = RequestHandler::new();
    let mut state = handler.state.lock().await;
    let session_name = create_session_in_state(&mut state, "retained-window-replacement");
    let target = WindowTarget::with_window(session_name.clone(), 0);
    let lease = state
        .capture_retained_window_lifecycle_target(&target)
        .expect("capture window lease");

    let session = state
        .sessions
        .session_mut(&session_name)
        .expect("session exists");
    session
        .remove_window_allowing_empty(0)
        .expect("remove original window");
    session
        .insert_window_with_initial_pane(0, terminal_size())
        .expect("replace numeric slot");
    state.retire_removed_lifecycle_targets();

    assert_eq!(lease.resolve(&state), LeaseResolution::Replaced);
}

#[tokio::test]
async fn retired_window_never_reacquires_a_later_numeric_slot_reuse() {
    let handler = RequestHandler::new();
    let mut state = handler.state.lock().await;
    let session_name = create_session_in_state(&mut state, "retained-window-reuse");
    let target = WindowTarget::with_window(session_name.clone(), 0);
    let lease = state
        .capture_retained_window_lifecycle_target(&target)
        .expect("capture window lease");
    assert!(matches!(lease.resolve(&state), LeaseResolution::Live(_)));

    state
        .sessions
        .session_mut(&session_name)
        .expect("session exists")
        .remove_window_allowing_empty(0)
        .expect("remove original window");
    state.retire_removed_lifecycle_targets();
    assert_retired(&lease, &state);

    state
        .sessions
        .session_mut(&session_name)
        .expect("session exists")
        .insert_window_with_initial_pane(0, terminal_size())
        .expect("reuse old numeric slot later");
    assert_retired(&lease, &state);
}
