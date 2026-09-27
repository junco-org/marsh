use super::*;
use rmux_proto::MoveWindowTarget;

#[tokio::test]
async fn move_window_without_s_moves_current_window_to_first_free_index() {
    let (handler, session) = handler_with_three_windows("move-current-free").await;
    let source_pane_id = pane_id_at(&handler, &session, 2).await;

    let parsed = CommandParser::new()
        .parse("move-window")
        .expect("commands parse");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&session, 2))
        .await
        .expect("move-window should use current window source and first free index");

    let state = handler.state.lock().await;
    let session_state = state.sessions.session(&session).expect("session exists");
    assert_eq!(
        session_state.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 3]
    );
    assert_eq!(pane_id_in(session_state, 3), source_pane_id);
}

#[tokio::test]
async fn move_window_after_uses_current_window_as_source() {
    let (handler, session) = handler_with_three_windows("move-current-after").await;
    let source_pane_id = pane_id_at(&handler, &session, 2).await;

    let parsed = CommandParser::new()
        .parse("move-window -a -t move-current-after:0")
        .expect("commands parse");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&session, 2))
        .await
        .expect("move-window -a should use current window source");

    let state = handler.state.lock().await;
    let session_state = state.sessions.session(&session).expect("session exists");
    assert_eq!(pane_id_in(session_state, 1), source_pane_id);
}

#[tokio::test]
async fn move_window_before_uses_current_window_as_source() {
    let (handler, session) = handler_with_three_windows("move-current-before").await;
    let source_pane_id = pane_id_at(&handler, &session, 2).await;

    let parsed = CommandParser::new()
        .parse("move-window -b -t move-current-before:0")
        .expect("commands parse");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&session, 2))
        .await
        .expect("move-window -b should use current window source");

    let state = handler.state.lock().await;
    let session_state = state.sessions.session(&session).expect("session exists");
    assert_eq!(pane_id_in(session_state, 0), source_pane_id);
}

#[tokio::test]
async fn move_window_relative_collision_uses_tmux_error_shape() {
    let (handler, session) = handler_with_three_windows("move-current-collision").await;

    let parsed = CommandParser::new()
        .parse("move-window -t move-current-collision:1")
        .expect("commands parse");
    let error = handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&session, 2))
        .await
        .expect_err("occupied relative target should fail");

    assert_eq!(error, RmuxError::Server("index in use: 1".to_owned()));
}

#[tokio::test]
async fn move_window_trailing_colon_target_uses_first_free_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("move-colon-alpha");
    let beta = session_name("move-colon-beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("move-window -s move-colon-alpha:0 -t move-colon-beta:")
        .expect("commands parse");
    assert_eq!(
        parsed.commands()[0].to_tmux_string(),
        "move-window -s move-colon-alpha:0 -t move-colon-beta:"
    );
    {
        let state = handler.state.lock().await;
        let request = parse_request_from_parts(
            "move-window".to_owned(),
            vec![
                "-s".to_owned(),
                "move-colon-alpha:0".to_owned(),
                "-t".to_owned(),
                "move-colon-beta:".to_owned(),
            ],
            None,
            &state.sessions,
            &state.options,
            &TargetFindContext::new(Some(Target::Pane(PaneTarget::with_window(
                beta.clone(),
                0,
                0,
            )))),
        )
        .expect("request parses");
        let Request::MoveWindow(request) = request else {
            panic!("expected move-window request");
        };
        assert_eq!(
            request.target,
            MoveWindowTarget::Session(beta.clone()),
            "trailing colon should stay a session target until execution"
        );
    }
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&beta, 0))
        .await
        .expect("trailing colon target should use first free window index");

    let state = handler.state.lock().await;
    let beta_state = state.sessions.session(&beta).expect("beta session exists");
    assert_eq!(
        beta_state.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

async fn handler_with_three_windows(name: &str) -> (RequestHandler, SessionName) {
    let handler = RequestHandler::new();
    let session = handler.create_session(name).await;
    for window_name in ["b", "c"] {
        handler
            .create_window(NewWindowRequest {
                name: Some(window_name.to_owned()),
                ..Fixture::fixture(&session)
            })
            .await;
    }
    (handler, session)
}

async fn pane_id_at(
    handler: &RequestHandler,
    session_name: &SessionName,
    window_index: u32,
) -> Option<PaneId> {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session_name)
        .and_then(|session| pane_id_in(session, window_index))
}

fn pane_id_in(session: &rmux_core::Session, window_index: u32) -> Option<PaneId> {
    session
        .window_at(window_index)
        .and_then(|window| window.pane(0))
        .map(|pane| pane.id())
}
