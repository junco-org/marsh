use super::relative_group_transactions::{assert_markers, marker, set_marker};
use super::*;
use crate::test_fixtures::TestRequest;

#[tokio::test]
async fn move_window_before_preserves_duplicate_linked_winlink_metadata() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "move-linked-metadata").await;
    insert_window(&handler, &alpha, 1).await;
    link_duplicate_window(&handler, &alpha, 1, 2).await;
    insert_window(&handler, &alpha, 3).await;

    set_marker(&handler, &alpha, 0, "root").await;
    set_marker(&handler, &alpha, 1, "linked").await;
    set_marker(&handler, &alpha, 3, "mover").await;

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 3),
                WindowTarget::with_window(alpha.clone(), 0),
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert_markers(
        &state,
        &alpha,
        &[Some("mover"), Some("root"), Some("linked"), Some("linked")],
    );
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session.window_at(2).map(rmux_core::Window::id),
        session.window_at(3).map(rmux_core::Window::id)
    );
}

#[tokio::test]
async fn grouped_move_window_keeps_window_options_with_shared_windows() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "group-move-owner").await;
    insert_window(&handler, &owner, 1).await;
    insert_window(&handler, &owner, 2).await;
    for (window_index, value) in [(0, "root"), (1, "anchor"), (2, "mover")] {
        set_marker(&handler, &owner, window_index, value).await;
    }

    let peer = create_grouped_session(&handler, "group-move-peer", &owner).await;
    {
        let state = handler.state.lock().await;
        assert_markers(
            &state,
            &peer,
            &[Some("root"), Some("anchor"), Some("mover")],
        );
    }
    set_marker(&handler, &peer, 1, "anchor-updated").await;
    {
        let state = handler.state.lock().await;
        assert_eq!(marker(&state, &owner, 1), Some("anchor-updated".to_owned()));
    }

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(owner.clone(), 2),
                WindowTarget::with_window(owner.clone(), 0),
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    for session_name in [&owner, &peer] {
        assert_markers(
            &state,
            session_name,
            &[Some("mover"), Some("root"), Some("anchor-updated")],
        );
    }
}

#[tokio::test]
async fn new_window_before_rekeys_existing_window_metadata() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "new-before-metadata").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;
    for (window_index, value) in [(0, "root"), (1, "one"), (2, "two")] {
        set_marker(&handler, &alpha, window_index, value).await;
    }

    handler
        .create_window(NewWindowRequest {
            name: Some("inserted".to_owned()),
            command: Some(quiet_command()),
            target_window_index: Some(0),
            insert_at_target: true,
            ..Fixture::fixture(&alpha)
        })
        .await;

    let state = handler.state.lock().await;
    assert_markers(
        &state,
        &alpha,
        &[None, Some("root"), Some("one"), Some("two")],
    );
}

#[tokio::test]
async fn link_window_before_rekeys_existing_window_metadata() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "link-before-target").await;
    let beta = create_session(&handler, "link-before-source").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;
    for (window_index, value) in [(0, "root"), (1, "one"), (2, "two")] {
        set_marker(&handler, &alpha, window_index, value).await;
    }
    set_marker(&handler, &beta, 0, "linked").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(beta, 0),
                WindowTarget::with_window(alpha.clone(), 0),
            ))
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert_markers(
        &state,
        &alpha,
        &[Some("linked"), Some("root"), Some("one"), Some("two")],
    );
}
