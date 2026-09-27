use super::*;

#[tokio::test]
async fn display_message_pane_synchronized_reflects_window_option() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::SynchronizePanes,
            "on",
        )
        .await;

    let output = handler
        .display_print(PaneTarget::with_window(alpha, 0, 0), "#{pane_synchronized}")
        .await;
    assert_eq!(output, b"1\n");
}
