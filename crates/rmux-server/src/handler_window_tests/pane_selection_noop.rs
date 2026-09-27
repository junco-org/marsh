use super::*;

#[tokio::test]
async fn unchanged_adjacent_and_stable_selects_do_not_resize_the_runtime() {
    let handler = RequestHandler::new();
    let session = create_session(&handler, "selection-noop-runtime").await;
    let pane_id = handler
        .state
        .lock()
        .await
        .sessions
        .session(&session)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(window.active_pane_index()))
        .expect("active pane exists")
        .id();
    let resize_count_before = {
        let mut state = handler.state.lock().await;
        let resize_count = state.window_runtime_resize_count_for_test();
        state.fail_next_resize_for_test();
        resize_count
    };

    let adjacent = handler
        .handle(Request::SelectPaneAdjacent(SelectPaneAdjacentRequest {
            target: PaneTarget::with_window(session.clone(), 0, 0),
            direction: SelectPaneDirection::Right,
            preserve_zoom: false,
        }))
        .await;
    assert!(matches!(adjacent, Response::SelectPane(_)), "{adjacent:?}");
    let stable = handler
        .handle(Request::PaneSelect(PaneSelectRequest {
            target: PaneTargetRef::by_id(session.clone(), pane_id),
            title: None,
        }))
        .await;
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
