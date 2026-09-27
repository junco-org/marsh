use super::RequestHandler;
use rmux_core::{AlertFlags, WINLINK_ACTIVITY, WINLINK_BELL};
use rmux_proto::{
    BreakPaneRequest, LinkWindowRequest, MoveWindowRequest, NewSessionExtRequest, NewWindowRequest,
    PaneTarget, SessionName, SplitWindowExtRequest, WindowTarget,
};

use crate::test_fixtures::{Fixture, Grouped};
use crate::test_names::session_name;

async fn create_session(handler: &RequestHandler, name: &str) -> SessionName {
    let request = NewSessionExtRequest::fixture(session_name(name));
    handler.create_started_session(request).await
}

async fn create_duplicate_group(
    handler: &RequestHandler,
    label: &str,
) -> (SessionName, SessionName) {
    let owner = create_session(handler, &format!("{label}-owner")).await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
        )))
        .await;
    let peer = session_name(&format!("{label}-peer"));
    let peer = handler.create_session(Grouped(peer, &owner)).await;
    (owner, peer)
}

async fn seed_peer_duplicate_flags(handler: &RequestHandler, peer: &SessionName) {
    let mut state = handler.state.lock().await;
    let peer_session = state
        .sessions
        .session_mut(peer)
        .expect("group peer exists before insertion");
    assert!(peer_session.add_winlink_alert_flags(0, WINLINK_BELL));
    assert!(peer_session.add_winlink_alert_flags(1, WINLINK_ACTIVITY));
}

async fn assert_peer_duplicate_flags_shifted(handler: &RequestHandler, peer: &SessionName) {
    let state = handler.state.lock().await;
    let peer_session = state
        .sessions
        .session(peer)
        .expect("group peer survives insertion");
    assert_eq!(peer_session.winlink_alert_flags(0), AlertFlags::empty());
    assert_eq!(peer_session.winlink_alert_flags(1), WINLINK_BELL);
    assert_eq!(peer_session.winlink_alert_flags(2), WINLINK_ACTIVITY);
}

#[tokio::test]
async fn grouped_new_window_insertion_preserves_peer_duplicate_alias_winlink_flags() {
    let handler = RequestHandler::new();
    let (owner, peer) = create_duplicate_group(&handler, "new-window-insert-alerts").await;
    seed_peer_duplicate_flags(&handler, &peer).await;

    handler
        .handle_ok(NewWindowRequest {
            target_window_index: Some(0),
            insert_at_target: true,
            ..Fixture::fixture(owner)
        })
        .await;

    assert_peer_duplicate_flags_shifted(&handler, &peer).await;
}

#[tokio::test]
async fn grouped_link_window_insertion_preserves_peer_duplicate_alias_winlink_flags() {
    let handler = RequestHandler::new();
    let (owner, peer) = create_duplicate_group(&handler, "link-window-insert-alerts").await;
    let source = create_session(&handler, "link-window-insert-source").await;
    seed_peer_duplicate_flags(&handler, &peer).await;

    handler
        .handle_ok(LinkWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(source, 0),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    assert_peer_duplicate_flags_shifted(&handler, &peer).await;
}

#[tokio::test]
async fn grouped_break_pane_insertion_preserves_peer_duplicate_alias_winlink_flags() {
    let handler = RequestHandler::new();
    let (owner, peer) = create_duplicate_group(&handler, "group-break-insert-alerts").await;
    handler
        .handle_ok(SplitWindowExtRequest {
            detached: true,
            ..Fixture::fixture(PaneTarget::with_window(owner.clone(), 0, 0))
        })
        .await;
    seed_peer_duplicate_flags(&handler, &peer).await;

    handler
        .handle_ok(BreakPaneRequest {
            before: true,
            ..Fixture::fixture((
                PaneTarget::with_window(owner.clone(), 1, 1),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    assert_peer_duplicate_flags_shifted(&handler, &peer).await;
}

async fn assert_cross_session_break_preserves_flags(label: &str, linked_source: bool) {
    let handler = RequestHandler::new();
    let (owner, peer) = create_duplicate_group(&handler, label).await;
    let source = create_session(&handler, &format!("{label}-source")).await;
    if linked_source {
        handler
            .handle_ok(LinkWindowRequest::fixture((
                WindowTarget::with_window(source.clone(), 0),
                WindowTarget::with_window(source.clone(), 1),
            )))
            .await;
    }
    seed_peer_duplicate_flags(&handler, &peer).await;

    handler
        .handle_ok(BreakPaneRequest {
            before: true,
            ..Fixture::fixture((
                PaneTarget::with_window(source, 0, 0),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    assert_peer_duplicate_flags_shifted(&handler, &peer).await;
}

#[tokio::test]
async fn cross_session_break_pane_insertion_preserves_peer_duplicate_alias_winlink_flags() {
    assert_cross_session_break_preserves_flags("cross-break-insert-alerts", false).await;
}

#[tokio::test]
async fn linked_last_break_pane_insertion_preserves_peer_duplicate_alias_winlink_flags() {
    assert_cross_session_break_preserves_flags("linked-break-insert-alerts", true).await;
}

async fn assert_relative_move_preserves_flags(label: &str, linked_source: bool) {
    let handler = RequestHandler::new();
    let (owner, peer) = create_duplicate_group(&handler, label).await;
    let source = create_session(&handler, &format!("{label}-source")).await;
    if linked_source {
        handler
            .handle_ok(LinkWindowRequest::fixture((
                WindowTarget::with_window(source.clone(), 0),
                WindowTarget::with_window(source.clone(), 1),
            )))
            .await;
    }
    seed_peer_duplicate_flags(&handler, &peer).await;

    handler
        .handle_ok(MoveWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(source, 0),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    assert_peer_duplicate_flags_shifted(&handler, &peer).await;
}

#[tokio::test]
async fn cross_session_relative_move_preserves_peer_duplicate_alias_winlink_flags() {
    assert_relative_move_preserves_flags("cross-move-insert-alerts", false).await;
}

#[tokio::test]
async fn linked_relative_move_preserves_peer_duplicate_alias_winlink_flags() {
    assert_relative_move_preserves_flags("linked-move-insert-alerts", true).await;
}
