use super::{HandlerState, RequestHandler};
use rmux_proto::{
    BreakPaneRequest, JoinPaneRequest, LinkWindowRequest, MovePaneRequest, NewWindowRequest,
    OptionName, OptionScopeSelector, PaneTarget, Request, Response, ScopeSelector,
    SplitWindowRequest, WindowTarget,
};

use crate::test_fixtures::{Fixture, Grouped, SessionSpec, TestRequest};

const USER_OPTION: &str = "@pane-transfer-window";
const KNOWN_OPTION: &str = "monitor-silence";

async fn set_window_metadata(handler: &RequestHandler, target: &WindowTarget, marker: &str) {
    handler
        .set_option_by_name(
            OptionScopeSelector::Window(target.clone()),
            USER_OPTION,
            marker,
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Window(target.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
}

fn explicit_window_value(
    state: &HandlerState,
    target: &WindowTarget,
    name: &str,
) -> Option<String> {
    state
        .options
        .explicit_value_by_name(&OptionScopeSelector::Window(target.clone()), name)
        .expect("valid window option")
        .1
}

fn assert_window_metadata(
    state: &HandlerState,
    target: &WindowTarget,
    marker: Option<&str>,
    monitor_silence: Option<&str>,
) {
    assert_eq!(
        explicit_window_value(state, target, USER_OPTION).as_deref(),
        marker,
        "unexpected user option at {target}"
    );
    assert_eq!(
        explicit_window_value(state, target, KNOWN_OPTION).as_deref(),
        monitor_silence,
        "unexpected known option at {target}"
    );
}

async fn mark_auto_named(handler: &RequestHandler, target: &WindowTarget) {
    handler
        .state
        .lock()
        .await
        .mark_auto_named_window(target.session_name(), target.window_index());
}

async fn run_destroying_same_session_transfer(move_pane: bool) {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(
        &handler,
        if move_pane {
            "metadata-move"
        } else {
            "metadata-join"
        },
    )
    .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session)
        })
        .await;
    let source_window = WindowTarget::with_window(session.clone(), 1);
    let target_window = WindowTarget::with_window(session.clone(), 0);
    set_window_metadata(&handler, &source_window, "discarded").await;
    mark_auto_named(&handler, &source_window).await;

    let source = PaneTarget::with_window(session.clone(), 1, 0);
    let target = PaneTarget::with_window(session.clone(), 0, 0);
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

    {
        let state = handler.state.lock().await;
        assert_window_metadata(&state, &source_window, None, None);
        assert_window_metadata(&state, &target_window, None, None);
        assert!(!state.tracks_auto_named_window(&session, 1));
    }
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session)
        })
        .await;
    let state = handler.state.lock().await;
    assert_window_metadata(&state, &source_window, None, None);
}

// Oracle probe 2026-07-12, pinned tmux 3.7b: a destroyed join/move source
// does not donate options to the target and a later window at that index is fresh.
#[tokio::test]
async fn join_and_move_drop_destroyed_source_window_metadata() {
    run_destroying_same_session_transfer(false).await;
    run_destroying_same_session_transfer(true).await;
}

#[tokio::test]
async fn cross_session_join_drops_destroyed_source_window_metadata() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "metadata-cross-join-source").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let destination_session =
        SessionSpec::create(&handler, "metadata-cross-join-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 1);
    let destination_window = WindowTarget::with_window(destination_session.clone(), 0);
    set_window_metadata(&handler, &source_window, "discarded").await;
    set_window_metadata(&handler, &destination_window, "destination").await;
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        JoinPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 1, 0),
            PaneTarget::with_window(destination_session, 0, 0),
        )),
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert_window_metadata(&state, &source_window, None, None);
        assert_window_metadata(&state, &destination_window, Some("destination"), Some("60"));
        assert!(!state.tracks_auto_named_window(&source_session, 1));
    }
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let state = handler.state.lock().await;
    assert_window_metadata(&state, &source_window, None, None);
}

#[tokio::test]
async fn grouped_join_clears_destroyed_source_metadata_from_every_peer() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create(&handler, "metadata-group-owner").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let peer = SessionSpec::create(&handler, Grouped("metadata-group-peer", &owner)).await;
    let source_window = WindowTarget::with_window(owner.clone(), 1);
    set_window_metadata(&handler, &source_window, "discarded").await;
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        JoinPaneRequest::fixture((
            PaneTarget::with_window(owner.clone(), 1, 0),
            PaneTarget::with_window(owner.clone(), 0, 0),
        )),
    )
    .await;

    {
        let state = handler.state.lock().await;
        for session in [&owner, &peer] {
            let target = WindowTarget::with_window(session.clone(), 1);
            assert_window_metadata(&state, &target, None, None);
            assert!(!state.tracks_auto_named_window(session, 1));
        }
    }
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let state = handler.state.lock().await;
    for session in [&owner, &peer] {
        assert_window_metadata(
            &state,
            &WindowTarget::with_window(session.clone(), 1),
            None,
            None,
        );
    }
}

// Oracle probe 2026-07-12, pinned tmux 3.7b: breaking the only pane moves
// the existing window, including user and known window options, across sessions.
#[tokio::test]
async fn cross_session_single_pane_break_moves_window_metadata() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "metadata-break-source").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let destination_session = SessionSpec::create(&handler, "metadata-break-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 1);
    let destination_window = WindowTarget::with_window(destination_session.clone(), 1);
    set_window_metadata(&handler, &source_window, "moved").await;
    mark_auto_named(&handler, &source_window).await;
    let source_window_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&source_session)
            .and_then(|session| session.window_at(1))
            .map(rmux_core::Window::id)
            .expect("source window exists")
    };
    assert!(handler
        .silence_timer_snapshot_for_test(&source_window)
        .is_some());

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 1, 0),
            &destination_window,
        )),
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert_window_metadata(&state, &destination_window, Some("moved"), Some("60"));
        assert_window_metadata(&state, &source_window, None, None);
        assert_eq!(
            state
                .sessions
                .session(destination_window.session_name())
                .and_then(|session| session.window_at(1))
                .map(rmux_core::Window::id),
            Some(source_window_id)
        );
        assert!(state.tracks_auto_named_window(destination_window.session_name(), 1));
        assert!(!state.tracks_auto_named_window(&source_session, 1));
    }
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&source_window),
        None
    );
    assert!(handler
        .silence_timer_snapshot_for_test(&destination_window)
        .is_some());
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let state = handler.state.lock().await;
    assert_window_metadata(&state, &source_window, None, None);
}

#[tokio::test]
async fn cross_session_single_pane_break_explicit_name_clears_automatic_tracking() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "named-break-source").await;
    let destination_session = SessionSpec::create(&handler, "named-break-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 0);
    let destination_window = WindowTarget::with_window(destination_session.clone(), 1);
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            name: Some("pinned".to_owned()),
            ..Fixture::fixture((
                PaneTarget::with_window(source_session, 0, 0),
                &destination_window,
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&destination_session)
        .and_then(|session| session.window_at(1))
        .expect("explicitly named destination window exists");
    assert_eq!(window.name(), Some("pinned"));
    assert!(!window.automatic_rename());
    assert!(!state.tracks_auto_named_window(&destination_session, 1));
}

#[tokio::test]
async fn same_session_single_pane_break_explicit_name_clears_automatic_tracking() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, "named-same-break").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session)
        })
        .await;
    let source_window = WindowTarget::with_window(session.clone(), 1);
    let destination_window = WindowTarget::with_window(session.clone(), 3);
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            name: Some("pinned".to_owned()),
            ..Fixture::fixture((
                PaneTarget::with_window(session.clone(), 1, 0),
                destination_window,
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&session)
        .and_then(|session| session.window_at(3))
        .expect("explicitly named destination window exists");
    assert_eq!(window.name(), Some("pinned"));
    assert!(!window.automatic_rename());
    assert!(!state.tracks_auto_named_window(&session, 3));
}

#[tokio::test]
async fn linked_last_pane_break_explicit_name_clears_family_automatic_tracking() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "named-linked-break-source").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let linked_session = SessionSpec::create(&handler, "named-linked-break-peer").await;
    let destination_session = SessionSpec::create(&handler, "named-linked-break-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 1);
    let linked_window = WindowTarget::with_window(linked_session.clone(), 1);
    let destination_window = WindowTarget::with_window(destination_session.clone(), 1);
    mark_auto_named(&handler, &source_window).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source_window, &linked_window)),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            name: Some("pinned".to_owned()),
            ..Fixture::fixture((
                PaneTarget::with_window(source_session, 1, 0),
                &destination_window,
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    for target in [&destination_window, &linked_window] {
        let window = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .unwrap_or_else(|| panic!("explicitly named linked window {target} exists"));
        assert_eq!(window.name(), Some("pinned"), "unexpected name at {target}");
        assert!(
            !window.automatic_rename(),
            "auto rename enabled at {target}"
        );
        assert!(!state.tracks_auto_named_window(target.session_name(), target.window_index()));
    }
}

#[tokio::test]
async fn linked_single_pane_break_moves_metadata_to_the_new_alias() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "metadata-linked-source").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_session)
        })
        .await;
    let linked_session = SessionSpec::create(&handler, "metadata-linked-peer").await;
    let destination_session = SessionSpec::create(&handler, "metadata-linked-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 1);
    let linked_window = WindowTarget::with_window(linked_session.clone(), 1);
    let destination_window = WindowTarget::with_window(destination_session, 1);
    set_window_metadata(&handler, &source_window, "linked").await;
    mark_auto_named(&handler, &source_window).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source_window, &linked_window)),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 1, 0),
            &destination_window,
        )),
    )
    .await;

    let state = handler.state.lock().await;
    assert_window_metadata(&state, &destination_window, Some("linked"), Some("60"));
    assert_window_metadata(&state, &linked_window, Some("linked"), Some("60"));
    assert_window_metadata(&state, &source_window, None, None);
    assert!(state.tracks_auto_named_window(destination_window.session_name(), 1));
    assert!(state.tracks_auto_named_window(&linked_session, 1));
    assert!(!state.tracks_auto_named_window(&source_session, 1));
}

#[tokio::test]
async fn same_session_single_pane_break_keeps_moving_window_metadata() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, "metadata-same-break").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session)
        })
        .await;
    let source_window = WindowTarget::with_window(session.clone(), 1);
    let destination_window = WindowTarget::with_window(session.clone(), 3);
    set_window_metadata(&handler, &source_window, "same-session").await;
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(session.clone(), 1, 0),
            &destination_window,
        )),
    )
    .await;

    let state = handler.state.lock().await;
    assert_window_metadata(
        &state,
        &destination_window,
        Some("same-session"),
        Some("60"),
    );
    assert_window_metadata(&state, &source_window, None, None);
    assert!(state.tracks_auto_named_window(&session, 3));
    assert!(!state.tracks_auto_named_window(&session, 1));
}

// Oracle probe 2026-07-12, pinned tmux 3.7b: breaking one pane from a
// multi-pane window creates a fresh window; the source keeps its options.
#[tokio::test]
async fn cross_session_multi_pane_break_does_not_copy_window_metadata() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create(&handler, "metadata-multi-source").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&source_session)).await;
    let destination_session = SessionSpec::create(&handler, "metadata-multi-destination").await;
    let source_window = WindowTarget::with_window(source_session.clone(), 0);
    let destination_window = WindowTarget::with_window(destination_session, 1);
    set_window_metadata(&handler, &source_window, "source-only").await;
    mark_auto_named(&handler, &source_window).await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 0, 1),
            &destination_window,
        )),
    )
    .await;

    let state = handler.state.lock().await;
    assert_window_metadata(&state, &source_window, Some("source-only"), Some("60"));
    assert_window_metadata(&state, &destination_window, None, None);
    assert!(state.tracks_auto_named_window(&source_session, 0));
}
