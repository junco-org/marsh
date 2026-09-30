use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

async fn handler_with_split_session(name: &str) -> (RequestHandler, SessionName) {
    let handler = RequestHandler::new();
    let session = SessionSpec::create(&handler, name).await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest::fixture(PaneTarget::with_window(session.clone(), 0, 0)),
    )
    .await;
    (handler, session)
}

async fn zoom_pane(handler: &RequestHandler, session: &SessionName, pane_index: u32) {
    TestRequest::send_ok(
        handler,
        rmux_proto::ResizePaneRequest {
            target: PaneTarget::with_window(session.clone(), 0, pane_index),
            adjustment: rmux_proto::ResizePaneAdjustment::Zoom,
        },
    )
    .await;
}

async fn assert_zoomed_active(handler: &RequestHandler, session: &SessionName, pane_index: u32) {
    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(session)
        .expect("session exists")
        .window_at(0)
        .expect("window exists");
    assert!(window.is_zoomed());
    assert_eq!(window.active_pane_index(), pane_index);
}

#[tokio::test]
async fn parsed_queue_select_pane_keep_zoom_preserves_zoom() {
    let (handler, session) = handler_with_split_session("select-zoom").await;
    zoom_pane(&handler, &session, 0).await;

    execute(&handler, "select-pane -Z -t select-zoom:0.1").await;

    assert_zoomed_active(&handler, &session, 1).await;
}

#[tokio::test]
async fn parsed_queue_select_pane_direction_keep_zoom_preserves_zoom() {
    let (handler, session) = handler_with_split_session("select-zoom-dir").await;
    zoom_pane(&handler, &session, 1).await;

    execute(&handler, "select-pane -Z -U -t select-zoom-dir:0.1").await;

    assert_zoomed_active(&handler, &session, 0).await;
}

#[tokio::test]
async fn parsed_queue_select_pane_last_keep_zoom_preserves_zoom() {
    let (handler, session) = handler_with_split_session("select-zoom-last").await;
    zoom_pane(&handler, &session, 1).await;

    execute(&handler, "select-pane -Z -l -t select-zoom-last:0.1").await;

    assert_zoomed_active(&handler, &session, 0).await;
}
