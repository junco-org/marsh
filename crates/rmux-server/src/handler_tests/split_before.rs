//! Regression tests for `SplitWindowRequest.before` (tmux `-b`).
//!
//! Kept out of the larger `panes.rs` test file so neither grows past the
//! 600-line ceiling.

use super::*;
use crate::test_fixtures::Fixture;

#[tokio::test]
async fn split_window_before_inserts_new_pane_on_the_leading_edge() {
    // Default (after) split with `Horizontal` puts the new pane on the *right*.
    // With `before: true` it lands on the *left* — same axis, opposite side.
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler
        .create_session((&alpha, TerminalSize::new(100, 50)))
        .await;

    handler
        .handle_ok(SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            before: true,
            ..Fixture::fixture(&alpha)
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    let pane_0 = session.window().pane(0).expect("pane 0 exists after split");
    let pane_1 = session.window().pane(1).expect("pane 1 exists after split");
    assert_eq!(pane_0.geometry().x(), 0, "leading pane sits at x=0");
    assert!(
        pane_1.geometry().x() > 0,
        "trailing pane sits past the divider"
    );
    assert_eq!(
        u32::from(pane_0.geometry().cols()) + u32::from(pane_1.geometry().cols()) + 1,
        100
    );
}
