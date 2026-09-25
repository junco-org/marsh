use super::*;

async fn active_pane_id(handler: &RequestHandler, session: &SessionName) -> rmux_proto::PaneId {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(window.active_pane_index()))
        .expect("active pane exists")
        .id()
}

async fn select_missing_adjacent(handler: &RequestHandler, session: &SessionName) -> Response {
    handler
        .handle(Request::SelectPaneAdjacent(SelectPaneAdjacentRequest {
            target: PaneTarget::with_window(session.clone(), 0, 0),
            direction: SelectPaneDirection::Right,
            preserve_zoom: false,
        }))
        .await
}

async fn select_active_stable_pane(
    handler: &RequestHandler,
    session: &SessionName,
    pane_id: rmux_proto::PaneId,
) -> Response {
    handler
        .handle(Request::PaneSelect(PaneSelectRequest {
            target: PaneTargetRef::by_id(session.clone(), pane_id),
            title: None,
        }))
        .await
}

#[tokio::test]
async fn unchanged_adjacent_and_stable_selects_do_not_resize_the_runtime() {
    let handler = RequestHandler::new();
    let session = session_name("selection-noop-runtime");
    create_session(&handler, session.as_str()).await;
    handler.wait_for_initial_panes_for_test().await;
    let pane_id = active_pane_id(&handler, &session).await;
    let resize_count_before = {
        let mut state = handler.state.lock().await;
        let resize_count = state.window_runtime_resize_count_for_test();
        state.fail_next_resize_for_test();
        resize_count
    };

    let adjacent = select_missing_adjacent(&handler, &session).await;
    assert!(matches!(adjacent, Response::SelectPane(_)), "{adjacent:?}");
    let stable = select_active_stable_pane(&handler, &session, pane_id).await;
    assert!(matches!(stable, Response::SelectPane(_)), "{stable:?}");
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .window_runtime_resize_count_for_test(),
        resize_count_before,
        "unchanged selection paths must not resize their runtime"
    );
}
