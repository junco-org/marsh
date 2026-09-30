use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

#[tokio::test]
async fn parsed_queue_resolves_bare_select_pane_against_the_current_pane() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;

    let parsed = CommandParser::new()
        .parse("select-pane")
        .expect("command parses");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&alpha, 0))
        .await
        .expect("bare select-pane should resolve against the current pane");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session
            .window_at(0)
            .expect("window exists")
            .active_pane_index(),
        0,
        "bare select-pane should fall back to the current pane target"
    );
}

#[tokio::test]
async fn parsed_queue_select_pane_title_sets_target_title_without_selecting_it() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::with_window(alpha.clone(), 0, 0)),
    )
    .await;

    let parsed = CommandParser::new()
        .parse("select-pane -t alpha:0.1 -T build-logs")
        .expect("command parses");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&alpha, 0))
        .await
        .expect("select-pane -T should execute");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session
            .window_at(0)
            .expect("window exists")
            .active_pane_index(),
        0,
        "select-pane -T must not select an inactive target"
    );
    let pane_id = session
        .pane_id_in_window(0, 1)
        .expect("pane 1 id should exist");
    let screen_state = state
        .pane_screen_state(&alpha, pane_id)
        .expect("pane 1 screen state should exist");
    assert_eq!(screen_state.title, "build-logs");
}

#[tokio::test]
async fn parsed_queue_select_pane_style_sets_target_style_and_selects_it() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let styled = PaneTarget::with_window(alpha.clone(), 0, 1);

    SessionSpec::create(&handler, &alpha).await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;

    let parsed = CommandParser::new()
        .parse("select-pane -talpha:0.1 -Pfg=blue,bg=red")
        .expect("command parses");
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&alpha, 0))
        .await
        .expect("compact select-pane -P should execute");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session
            .window_at(0)
            .expect("window exists")
            .active_pane_index(),
        1,
        "select-pane -P must select the target pane"
    );
    assert_eq!(
        state.options.pane_value(&styled, OptionName::WindowStyle),
        Some("fg=blue,bg=red")
    );
}

#[tokio::test]
async fn parsed_queue_resolves_move_window_renumber_target_as_session() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let parsed = CommandParser::new()
        .parse("move-window -r -t alp")
        .expect("commands parse");

    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("queue command succeeds");
}

#[tokio::test]
async fn parsed_queue_uses_current_target_for_move_window_renumber_without_t() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let parsed = CommandParser::new()
        .parse("move-window -r")
        .expect("commands parse");

    handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd()
                .with_current_target(Some(Target::Window(WindowTarget::with_window(alpha, 0)))),
        )
        .await
        .expect("move-window -r should use the current session target");
}

#[tokio::test]
async fn parsed_queue_move_window_after_inserts_before_unlinking_source() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    for name in ["b", "c"] {
        handler
            .create_window(NewWindowRequest {
                name: Some(name.to_owned()),
                ..Fixture::fixture(&alpha)
            })
            .await;
    }
    let source_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .expect("session exists")
            .window_at(0)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("source pane exists")
    };

    let parsed = CommandParser::new()
        .parse("move-window -a -s alpha:0 -t alpha:1")
        .expect("commands parse");
    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("move-window -a should execute through the scripting queue");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        session
            .window_at(2)
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id()),
        Some(source_pane_id)
    );
}

#[tokio::test]
async fn parsed_queue_resize_window_balanced_flags_use_attached_client_sizes() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    let mut control_receivers = Vec::new();
    for (pid, size) in [
        (
            201,
            TerminalSize {
                cols: 160,
                rows: 40,
            },
        ),
        (202, TerminalSize { cols: 72, rows: 18 }),
    ] {
        control_receivers.push(handler.attach_client(pid, &alpha).await);
        let mut active_attach = handler.active_attach.lock().await;
        active_attach
            .by_pid
            .get_mut(&pid)
            .expect("registered attach exists")
            .set_declared_client_size(size);
    }

    let grow = CommandParser::new()
        .parse("resize-window -A -t alpha:0")
        .expect("resize-window -A parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), grow)
        .await
        .expect("resize-window -A should execute");
    {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("window exists after -A");
        // Outer terminals of 160x40 and 72x18 with `status` defaulting to `on`
        // give content candidates of 160x39 and 72x17, and each dimension keeps
        // its own extreme. tmux 3.7b measured with real PTY clients of both
        // sizes: `-A` lands on 160x39 and `-a` on 72x17.
        assert_eq!(
            window.size(),
            TerminalSize {
                cols: 160,
                rows: 39
            }
        );
    }

    let shrink = CommandParser::new()
        .parse("resize-window -a -t alpha:0")
        .expect("resize-window -a parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), shrink)
        .await
        .expect("resize-window -a should execute");

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(0))
        .expect("window exists after -a");
    assert_eq!(window.size(), TerminalSize { cols: 72, rows: 17 });
}

#[tokio::test]
async fn parsed_queue_new_window_accepts_nonexistent_target_window_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    let parsed = CommandParser::new()
        .parse("new-window -d -t alpha:5 -n five")
        .expect("commands parse");

    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("new-window should create the requested window index");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert!(session.window_at(5).is_some());
    assert_eq!(
        session.window_at(5).and_then(|window| window.name()),
        Some("five")
    );
}

#[tokio::test]
async fn parsed_queue_new_window_rejects_oversized_target_window_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, alpha).await;
    let parsed = CommandParser::new()
        .parse("new-window -d -t alpha:99999999999999999999 -n huge")
        .expect("commands parse");

    let error = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect_err("oversized window index should be rejected without panicking");
    assert!(
        error.to_string().contains("unsigned 32-bit integer"),
        "{error}"
    );
}

#[tokio::test]
async fn parsed_queue_rejects_pane_component_for_window_index_targets() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, alpha).await;
    let parsed = CommandParser::new()
        .parse("break-pane -s alpha:0.0 -t alpha:9.0")
        .expect("commands parse");

    let error = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect_err("pane component is invalid for window-index lookup");

    assert_eq!(
        error,
        RmuxError::invalid_target("alpha:9.0", "can't specify pane here")
    );
}

#[tokio::test]
async fn parsed_queue_exposes_gated_mouse_target_errors() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, session_name("alpha")).await;
    let parsed = CommandParser::new()
        .parse("display-message -p -t '{mouse}' hello")
        .expect("commands parse");

    let error = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect_err("mouse target is deferred");

    assert!(
        error
            .to_string()
            .contains("target form {mouse} is recognized"),
        "{error}"
    );
}

#[tokio::test]
async fn parsed_queue_resolves_mouse_targets_when_context_carries_mouse_state() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    let parsed = CommandParser::new()
        .parse("display-message -p -t '=' '#{session_name}:#{window_index}:#{pane_index}'")
        .expect("commands parse");

    let output = handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd()
                .with_current_target(Some(Target::Session(alpha.clone())))
                .with_mouse_target(Some(Target::Window(WindowTarget::with_window(alpha, 0)))),
        )
        .await
        .expect("mouse target resolves through queued command");

    assert_eq!(output.stdout(), b"alpha:0:0\n");
}
