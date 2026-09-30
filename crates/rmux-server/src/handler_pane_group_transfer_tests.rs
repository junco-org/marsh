use super::RequestHandler;
use rmux_proto::{
    BreakPaneRequest, JoinPaneRequest, MovePaneRequest, NewWindowRequest, OptionName,
    PaneOptionGetRequest, PaneOptionSetRequest, PaneTarget, PaneTargetRef, Request, Response,
    ScopeSelector, SessionName, SetOptionMode, SplitWindowRequest, SwapPaneRequest, WindowTarget,
};

use crate::test_fixtures::{Fixture, Grouped, SessionSpec, TestRequest};

pub(super) async fn create_session(handler: &RequestHandler, name: &str) -> SessionName {
    SessionSpec::create(handler, name).await
}

pub(super) async fn create_grouped_session(
    handler: &RequestHandler,
    name: &str,
    group_target: &SessionName,
) -> SessionName {
    SessionSpec::create(handler, Grouped(name, group_target)).await
}

async fn create_group_with_two_panes(
    handler: &RequestHandler,
    owner_name: &str,
    peer_name: &str,
) -> (SessionName, SessionName) {
    let owner = SessionSpec::create(handler, owner_name).await;
    TestRequest::send_ok(handler, SplitWindowRequest::fixture(&owner)).await;
    let peer = SessionSpec::create(handler, Grouped(peer_name, &owner)).await;
    (owner, peer)
}

pub(super) async fn split_session(handler: &RequestHandler, session_name: &SessionName) {
    TestRequest::send_ok(handler, SplitWindowRequest::fixture(session_name)).await;
}

async fn assert_intra_window_transfer_preserves_unrelated_silence_timer(
    label: &str,
    move_pane: bool,
) {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, label).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&session)).await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session)
        })
        .await;

    let unrelated = WindowTarget::with_window(session.clone(), 1);
    handler
        .set_option(
            ScopeSelector::Window(unrelated.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    let before = handler
        .silence_timer_snapshot_for_test(&unrelated)
        .expect("unrelated window timer is armed before the pane transfer");

    let source = PaneTarget::with_window(session.clone(), 0, 1);
    let target = PaneTarget::with_window(session, 0, 0);
    let response = if move_pane {
        handler
            .handle(Request::MovePane(MovePaneRequest::fixture((
                source, target,
            ))))
            .await
    } else {
        handler
            .handle(Request::JoinPane(JoinPaneRequest::fixture((
                source, target,
            ))))
            .await
    };
    assert!(
        matches!(response, Response::JoinPane(_) | Response::MovePane(_)),
        "{response:?}"
    );
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&unrelated),
        Some(before),
        "an intra-window pane transfer must not restart an unrelated window timer"
    );
}

#[tokio::test]
async fn intra_window_join_and_move_preserve_unrelated_silence_deadlines() {
    assert_intra_window_transfer_preserves_unrelated_silence_timer("join-unrelated-silence", false)
        .await;
    assert_intra_window_transfer_preserves_unrelated_silence_timer("move-unrelated-silence", true)
        .await;
}

#[tokio::test]
async fn grouped_break_preserves_peer_timer_and_arms_the_new_peer_window() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create(&handler, "break-silence-owner").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&owner)).await;
    let peer = SessionSpec::create(&handler, Grouped("break-silence-peer", &owner)).await;

    handler
        .set_option(
            ScopeSelector::Session(peer.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    let source_peer = WindowTarget::with_window(peer.clone(), 0);
    let before = handler
        .silence_timer_snapshot_for_test(&source_peer)
        .expect("group peer source timer is armed before break-pane");

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(owner.clone(), 0, 1),
            WindowTarget::with_window(peer.clone(), 1),
        )),
    )
    .await;
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&source_peer),
        Some(before),
        "break-pane must preserve the existing group peer deadline"
    );
    assert!(
        handler
            .silence_timer_snapshot_for_test(&WindowTarget::with_window(peer, 1))
            .is_some(),
        "break-pane arms the newly-created peer window from its session option"
    );
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&WindowTarget::with_window(owner, 1)),
        None,
        "mixed group options must not arm the owner peer"
    );
}

pub(super) fn pane_id(
    state: &super::HandlerState,
    session_name: &SessionName,
    window_index: u32,
    pane_index: u32,
) -> rmux_core::PaneId {
    state
        .sessions
        .session(session_name)
        .and_then(|session| session.pane_id_in_window(window_index, pane_index))
        .expect("pane exists")
}

fn session_contains_pane(session: &rmux_core::Session, pane_id: rmux_core::PaneId) -> bool {
    session
        .windows()
        .values()
        .any(|window| window.panes().iter().any(|pane| pane.id() == pane_id))
}

pub(super) fn pane_ids(
    state: &super::HandlerState,
    session_name: &SessionName,
    window_index: u32,
) -> Vec<rmux_core::PaneId> {
    state
        .sessions
        .session(session_name)
        .and_then(|session| session.window_at(window_index))
        .expect("window exists")
        .panes()
        .iter()
        .map(rmux_core::Pane::id)
        .collect()
}

pub(super) async fn set_pane_option(
    handler: &RequestHandler,
    target: PaneTarget,
    name: &str,
    value: &str,
) {
    let response = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: PaneTargetRef::slot(target),
            name: name.to_owned(),
            value: Some(value.to_owned()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(response, Response::PaneOptionSet(_)),
        "{response:?}"
    );
}

pub(super) async fn pane_option(
    handler: &RequestHandler,
    target: PaneTarget,
    name: &str,
) -> Option<String> {
    match handler
        .handle(Request::PaneOptionGet(PaneOptionGetRequest {
            target: PaneTargetRef::slot(target),
            name: name.to_owned(),
        }))
        .await
    {
        Response::PaneOptionGet(response) => response.value,
        response => panic!("pane-option-get failed: {response:?}"),
    }
}

#[tokio::test]
async fn swap_pane_between_aliases_of_the_same_group_mutates_shared_state_once() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "same-group-swap-owner", "same-group-swap-peer")
            .await;
    let before = {
        let state = handler.state.lock().await;
        pane_ids(&state, &owner, 0)
    };
    set_pane_option(
        &handler,
        PaneTarget::with_window(peer.clone(), 0, 0),
        "@same-group-swap",
        "tracked",
    )
    .await;

    TestRequest::send_ok(
        &handler,
        SwapPaneRequest::fixture((
            PaneTarget::with_window(owner.clone(), 0, 0),
            PaneTarget::with_window(peer.clone(), 0, 1),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    let expected = vec![before[1], before[0]];
    assert_eq!(pane_ids(&state, &owner, 0), expected);
    assert_eq!(pane_ids(&state, &peer, 0), expected);
    for pane_index in 0..2 {
        state
            .pane_profile_in_window(&peer, 0, pane_index)
            .expect("shared runtime terminal remains reachable through peer alias");
    }
    drop(state);

    for session_name in [&owner, &peer] {
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 0, 1),
                "@same-group-swap",
            )
            .await,
            Some("tracked".to_owned()),
        );
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 0, 0),
                "@same-group-swap",
            )
            .await,
            None,
        );
    }
}

#[tokio::test]
async fn join_pane_between_aliases_of_the_same_group_uses_single_session_semantics() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "same-group-join-owner", "same-group-join-peer")
            .await;
    set_pane_option(
        &handler,
        PaneTarget::with_window(owner.clone(), 0, 1),
        "@same-group-join",
        "tracked",
    )
    .await;

    let response = TestRequest::send_ok(
        &handler,
        JoinPaneRequest::fixture((
            PaneTarget::with_window(owner.clone(), 0, 1),
            PaneTarget::with_window(peer.clone(), 0, 0),
        )),
    )
    .await;
    assert_eq!(response.target.session_name(), &peer);
    let moved_index = response.target.pane_index();

    let state = handler.state.lock().await;
    assert_eq!(pane_ids(&state, &owner, 0), pane_ids(&state, &peer, 0));
    assert_eq!(pane_ids(&state, &peer, 0).len(), 2);
    state
        .pane_profile_in_window(
            &peer,
            response.target.window_index(),
            response.target.pane_index(),
        )
        .expect("joined pane remains backed by the shared runtime terminal");
    drop(state);

    for session_name in [&owner, &peer] {
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 0, moved_index),
                "@same-group-join",
            )
            .await,
            Some("tracked".to_owned()),
        );
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 0, u32::from(moved_index == 0)),
                "@same-group-join",
            )
            .await,
            None,
        );
    }
}

#[tokio::test]
async fn non_detached_join_between_group_aliases_selects_the_destination_alias() {
    let handler = RequestHandler::new();
    let (owner, peer) = create_group_with_two_panes(
        &handler,
        "same-group-join-select-owner",
        "same-group-join-select-peer",
    )
    .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;

    TestRequest::send_ok(
        &handler,
        JoinPaneRequest {
            detached: false,
            ..Fixture::fixture((
                PaneTarget::with_window(owner.clone(), 0, 1),
                PaneTarget::with_window(peer.clone(), 1, 0),
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&owner)
            .map(rmux_core::Session::active_window_index),
        Some(0),
    );
    assert_eq!(
        state
            .sessions
            .session(&peer)
            .map(rmux_core::Session::active_window_index),
        Some(1),
    );
}

#[tokio::test]
async fn break_pane_between_aliases_of_the_same_group_uses_single_session_semantics() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "same-group-break-owner", "same-group-break-peer")
            .await;
    let moved_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &owner, 0, 1)
    };
    set_pane_option(
        &handler,
        PaneTarget::with_window(peer.clone(), 0, 1),
        "@same-group-break",
        "tracked",
    )
    .await;

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(owner.clone(), 0, 1),
            WindowTarget::with_window(peer.clone(), 1),
        )),
    )
    .await;
    assert_eq!(response.target.session_name(), &peer);

    let state = handler.state.lock().await;
    assert_eq!(pane_ids(&state, &owner, 0), pane_ids(&state, &peer, 0));
    assert_eq!(pane_ids(&state, &owner, 1), vec![moved_pane_id]);
    assert_eq!(pane_ids(&state, &peer, 1), vec![moved_pane_id]);
    state
        .pane_profile_in_window(&peer, 1, 0)
        .expect("broken pane remains backed by the shared runtime terminal");
    drop(state);

    for session_name in [&owner, &peer] {
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 1, 0),
                "@same-group-break",
            )
            .await,
            Some("tracked".to_owned()),
        );
        assert_eq!(
            pane_option(
                &handler,
                PaneTarget::with_window(session_name.clone(), 0, 0),
                "@same-group-break",
            )
            .await,
            None,
        );
    }
}

#[tokio::test]
async fn non_detached_break_between_group_aliases_selects_the_destination_alias() {
    let handler = RequestHandler::new();
    let (owner, peer) = create_group_with_two_panes(
        &handler,
        "same-group-break-select-owner",
        "same-group-break-select-peer",
    )
    .await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            detached: false,
            ..Fixture::fixture((
                PaneTarget::with_window(owner.clone(), 0, 1),
                WindowTarget::with_window(peer.clone(), 1),
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&owner)
            .map(rmux_core::Session::active_window_index),
        Some(0),
    );
    assert_eq!(
        state
            .sessions
            .session(&peer)
            .map(rmux_core::Session::active_window_index),
        Some(1),
    );
}

#[tokio::test]
async fn break_last_pane_between_group_aliases_matches_tmux_rejection() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create(&handler, "last-pane-group-break-owner").await;
    let peer = SessionSpec::create(&handler, Grouped("last-pane-group-break-peer", &owner)).await;
    let (owner_before, peer_before) = {
        let state = handler.state.lock().await;
        (
            state
                .sessions
                .session(&owner)
                .expect("owner exists")
                .clone(),
            state.sessions.session(&peer).expect("peer exists").clone(),
        )
    };

    for detached in [true, false] {
        let response = handler
            .handle(Request::BreakPane(Box::new(BreakPaneRequest {
                detached,
                ..Fixture::fixture((
                    PaneTarget::with_window(owner.clone(), 0, 0),
                    WindowTarget::with_window(peer.clone(), 1),
                ))
            })))
            .await;
        assert!(
            matches!(&response, Response::Error(error) if error.error.to_string().contains("sessions are grouped")),
            "expected grouped-session rejection, got {response:?}"
        );
    }

    let state = handler.state.lock().await;
    assert_eq!(state.sessions.session(&owner), Some(&owner_before));
    assert_eq!(state.sessions.session(&peer), Some(&peer_before));
    state
        .pane_profile_in_window(&peer, 0, 0)
        .expect("rejected break must preserve grouped runtime terminal");
}

#[tokio::test]
async fn join_pane_from_group_peer_moves_the_runtime_owned_pane() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "join-group-owner", "join-group-peer").await;
    let target = SessionSpec::create(&handler, "join-group-target").await;
    let moved_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &peer, 0, 1)
    };

    let response = TestRequest::send_ok(
        &handler,
        JoinPaneRequest::fixture((
            PaneTarget::with_window(peer.clone(), 0, 1),
            PaneTarget::with_window(target.clone(), 0, 0),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    for group_member in [&owner, &peer] {
        assert!(
            state
                .sessions
                .session(group_member)
                .is_some_and(|session| !session_contains_pane(session, moved_pane_id)),
            "moved pane must leave grouped member {group_member}"
        );
    }
    assert_eq!(
        pane_id(
            &state,
            &target,
            response.target.window_index(),
            response.target.pane_index(),
        ),
        moved_pane_id
    );
    state
        .pane_profile_in_window(
            &target,
            response.target.window_index(),
            response.target.pane_index(),
        )
        .expect("moved pane terminal follows the model into target runtime");
}

#[tokio::test]
async fn break_pane_from_group_peer_moves_the_runtime_owned_pane() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "break-group-owner", "break-group-peer").await;
    let target = SessionSpec::create(&handler, "break-group-target").await;
    let moved_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &peer, 0, 1)
    };

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(peer.clone(), 0, 1),
            WindowTarget::with_window(target.clone(), 1),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    for group_member in [&owner, &peer] {
        assert!(
            state
                .sessions
                .session(group_member)
                .is_some_and(|session| !session_contains_pane(session, moved_pane_id)),
            "moved pane must leave grouped member {group_member}"
        );
    }
    assert_eq!(
        pane_id(
            &state,
            &target,
            response.target.window_index(),
            response.target.pane_index(),
        ),
        moved_pane_id
    );
    state
        .pane_profile_in_window(
            &target,
            response.target.window_index(),
            response.target.pane_index(),
        )
        .expect("broken pane terminal follows the model into target runtime");
}

#[tokio::test]
async fn swap_pane_from_group_peer_swaps_runtime_owned_panes() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "swap-group-owner", "swap-group-peer").await;
    let target = SessionSpec::create(&handler, "swap-group-target").await;
    let (source_pane_id, target_pane_id) = {
        let state = handler.state.lock().await;
        (pane_id(&state, &peer, 0, 0), pane_id(&state, &target, 0, 0))
    };

    TestRequest::send_ok(
        &handler,
        SwapPaneRequest::fixture((
            PaneTarget::with_window(peer.clone(), 0, 0),
            PaneTarget::with_window(target.clone(), 0, 0),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    for group_member in [&owner, &peer] {
        assert_eq!(pane_id(&state, group_member, 0, 0), target_pane_id);
        state
            .pane_profile_in_window(group_member, 0, 0)
            .expect("target terminal must move into the shared group runtime");
    }
    assert_eq!(pane_id(&state, &target, 0, 0), source_pane_id);
    state
        .pane_profile_in_window(&target, 0, 0)
        .expect("group-owned source terminal must move into target runtime");
}

#[tokio::test]
async fn join_pane_into_group_peer_moves_into_the_runtime_owner() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "join-destination-source").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&source)).await;
    let owner = SessionSpec::create(&handler, "join-destination-owner").await;
    let peer = SessionSpec::create(&handler, Grouped("join-destination-peer", &owner)).await;
    let moved_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &source, 0, 1)
    };

    let response = TestRequest::send_ok(
        &handler,
        JoinPaneRequest::fixture((
            PaneTarget::with_window(source.clone(), 0, 1),
            PaneTarget::with_window(peer.clone(), 0, 0),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    for group_member in [&owner, &peer] {
        assert_eq!(
            pane_id(
                &state,
                group_member,
                response.target.window_index(),
                response.target.pane_index(),
            ),
            moved_pane_id
        );
        state
            .pane_profile_in_window(
                group_member,
                response.target.window_index(),
                response.target.pane_index(),
            )
            .expect("joined pane must be reachable through each grouped destination alias");
    }
}

#[tokio::test]
async fn break_pane_into_group_peer_moves_into_the_runtime_owner() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "break-destination-source").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&source)).await;
    let owner = SessionSpec::create(&handler, "break-destination-owner").await;
    let peer = SessionSpec::create(&handler, Grouped("break-destination-peer", &owner)).await;
    let moved_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &source, 0, 1)
    };

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source, 0, 1),
            WindowTarget::with_window(peer.clone(), 1),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    for group_member in [&owner, &peer] {
        assert_eq!(
            pane_id(
                &state,
                group_member,
                response.target.window_index(),
                response.target.pane_index(),
            ),
            moved_pane_id
        );
        state
            .pane_profile_in_window(
                group_member,
                response.target.window_index(),
                response.target.pane_index(),
            )
            .expect("broken pane must be reachable through each grouped destination alias");
    }
}

#[tokio::test]
async fn grouped_peer_cross_session_swap_rollback_restores_model_and_runtimes() {
    let handler = RequestHandler::new();
    let (owner, peer) =
        create_group_with_two_panes(&handler, "swap-rollback-owner", "swap-rollback-peer").await;
    let target = SessionSpec::create(&handler, "swap-rollback-target").await;
    let (owner_before, peer_before, target_before) = {
        let mut state = handler.state.lock().await;
        let snapshots = (
            state
                .sessions
                .session(&owner)
                .expect("owner exists")
                .clone(),
            state.sessions.session(&peer).expect("peer exists").clone(),
            state
                .sessions
                .session(&target)
                .expect("target exists")
                .clone(),
        );
        state.fail_next_resize_for_test();
        snapshots
    };

    let response = handler
        .handle(Request::SwapPane(SwapPaneRequest::fixture((
            PaneTarget::with_window(peer.clone(), 0, 0),
            PaneTarget::with_window(target.clone(), 0, 0),
        ))))
        .await;
    assert!(
        matches!(&response, Response::Error(error) if error.error.to_string().contains("injected pane terminal resize failure")),
        "expected injected rollback path, got {response:?}"
    );

    let state = handler.state.lock().await;
    assert_eq!(state.sessions.session(&owner), Some(&owner_before));
    assert_eq!(state.sessions.session(&peer), Some(&peer_before));
    assert_eq!(state.sessions.session(&target), Some(&target_before));
    state
        .pane_profile_in_window(&peer, 0, 0)
        .expect("group runtime terminal must be restored");
    state
        .pane_profile_in_window(&target, 0, 0)
        .expect("standalone target terminal must be restored");
}
