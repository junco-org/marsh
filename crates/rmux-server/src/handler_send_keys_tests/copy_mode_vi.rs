use super::*;

#[tokio::test]
async fn send_keys_uses_copy_mode_vi_default_bindings() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::new(alpha.clone(), 0);

    create_send_keys_test_session(&handler, &alpha).await;
    handler
        .replace_transcript_for_test(
            &target,
            TerminalSize { cols: 80, rows: 24 },
            b"alpha\r\nbeta\r\n",
        )
        .await;

    set_mode_keys(&handler, &alpha, "vi").await;
    handler.handle_ok(CopyModeRequest::fixture(&target)).await;

    let response = handler
        .handle(Request::SendKeys(SendKeysRequest {
            target,
            keys: vec!["g".to_owned(), "V".to_owned(), "Enter".to_owned()],
        }))
        .await;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 3 })
    );

    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest { name: None }))
        .await;
    let Response::ShowBuffer(response) = shown else {
        panic!("expected show-buffer response");
    };
    assert_eq!(response.command_output().stdout(), b"alpha\n");
}
