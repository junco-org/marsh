use super::*;

#[tokio::test]
async fn parsed_queue_uses_current_target_for_rename_window_session_and_last_window() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("w1".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    handler
        .handle_ok(rmux_proto::SelectWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
        })
        .await;

    for command in [
        "rename-window renamed",
        "last-window",
        "rename-session beta",
    ] {
        let parsed = CommandParser::new().parse(command).expect("command parses");
        handler
            .execute_parsed_commands(std::process::id(), parsed, pane_context(&alpha, 1))
            .await
            .unwrap_or_else(|error| {
                panic!("{command} should succeed with current target: {error}")
            });
    }

    let state = handler.state.lock().await;
    let beta = session_name("beta");
    let session = state
        .sessions
        .session(&beta)
        .expect("renamed session exists");
    assert_eq!(
        session.window_at(1).expect("window 1 exists").name(),
        Some("renamed")
    );
    assert_eq!(
        session.active_window_index(),
        0,
        "last-window should return to window 0"
    );
}

#[tokio::test]
async fn parsed_queue_select_window_navigation_flags_use_window_session() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha-select-flags");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("w1".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;

    let current_pane = PaneTarget::with_window(alpha.clone(), 0, 0);
    let context = TargetFindContext::from_target(Target::Pane(current_pane));
    let state = handler.state.lock().await;
    let cases = [
        (
            vec![
                "-n".to_owned(),
                "-t".to_owned(),
                "alpha-select-flags:1".to_owned(),
            ],
            Request::NextWindow(NextWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }),
        ),
        (
            vec![
                "-p".to_owned(),
                "-t".to_owned(),
                "alpha-select-flags:1".to_owned(),
            ],
            Request::PreviousWindow(PreviousWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }),
        ),
        (
            vec![
                "-l".to_owned(),
                "-t".to_owned(),
                "alpha-select-flags:1".to_owned(),
            ],
            Request::LastWindow(LastWindowRequest {
                target: alpha.clone(),
            }),
        ),
        (
            vec![
                "-T".to_owned(),
                "-t".to_owned(),
                "alpha-select-flags:0".to_owned(),
            ],
            Request::LastWindow(LastWindowRequest {
                target: alpha.clone(),
            }),
        ),
        (
            vec![
                "-T".to_owned(),
                "-t".to_owned(),
                "alpha-select-flags:1".to_owned(),
            ],
            Request::SelectWindow(rmux_proto::SelectWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), 1),
            }),
        ),
    ];

    for (arguments, expected) in cases {
        let parsed = parse_request_from_parts(
            "select-window".to_owned(),
            arguments,
            None,
            &state.sessions,
            &state.options,
            &context,
        )
        .expect("select-window navigation flag should parse");
        assert_eq!(parsed, expected);
    }
}

#[tokio::test]
async fn parsed_queue_select_window_repeated_navigation_flags_follow_tmux_priority() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha-select-priority");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("w1".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;

    let current_pane = PaneTarget::with_window(alpha.clone(), 0, 0);
    let context = TargetFindContext::from_target(Target::Pane(current_pane));
    let state = handler.state.lock().await;
    for arguments in [
        vec![
            "-n".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            "alpha-select-priority:1".to_owned(),
        ],
        vec![
            "-p".to_owned(),
            "-n".to_owned(),
            "-t".to_owned(),
            "alpha-select-priority:1".to_owned(),
        ],
    ] {
        let parsed = parse_request_from_parts(
            "select-window".to_owned(),
            arguments,
            None,
            &state.sessions,
            &state.options,
            &context,
        )
        .expect("select-window repeated navigation flags should parse");
        assert_eq!(
            parsed,
            Request::NextWindow(NextWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            })
        );
    }
}

#[tokio::test]
async fn parsed_queue_list_panes_resolves_bare_window_name_in_current_session() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("editor".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    handler
        .handle_ok(SplitWindowRequest::fixture(PaneTarget::with_window(
            alpha.clone(),
            1,
            0,
        )))
        .await;

    let command = CommandParser::new()
        .parse("list-panes -t editor -F '#{pane_index}'")
        .expect("list-panes command parses");
    let output = handler
        .execute_parsed_commands(std::process::id(), command, pane_context(&alpha, 1))
        .await
        .expect("list-panes should resolve editor as a window name");

    assert_eq!(String::from_utf8_lossy(&output.stdout), "0\n1\n");
}

#[tokio::test]
async fn parsed_queue_join_move_swap_without_source_fall_back_to_current_pane() {
    for command in [
        "join-pane -t alpha-transfer-default:0.0",
        "move-pane -t alpha-transfer-default:0.0",
        "swap-pane -t alpha-transfer-default:0.0",
    ] {
        let handler = RequestHandler::new();
        let alpha = session_name("alpha-transfer-default");
        handler
            .create_session((&alpha, TerminalSize::new(100, 24)))
            .await;
        handler
            .handle_ok(SplitWindowRequest::fixture(PaneTarget::with_window(
                alpha.clone(),
                0,
                0,
            )))
            .await;

        let parsed = CommandParser::new().parse(command).expect("command parses");
        let output = handler
            .execute_parsed_commands(
                std::process::id(),
                parsed,
                QueueExecutionContext::without_caller_cwd()
                    .with_current_target(Some(Target::Pane(PaneTarget::with_window(alpha, 0, 1)))),
            )
            .await;
        assert!(
            output.is_ok(),
            "{command} should use the current pane when -s is omitted: {output:?}"
        );
    }
}

#[tokio::test]
async fn parsed_queue_uses_current_target_for_more_default_targeted_commands() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;

    let current_pane = PaneTarget::with_window(alpha.clone(), 0, 0);
    let current_window = WindowTarget::with_window(alpha.clone(), 0);
    let context = TargetFindContext::from_target(Target::Pane(current_pane.clone()));
    let state = handler.state.lock().await;

    let cases = [
        (
            "kill-window",
            Vec::new(),
            Request::KillWindow(KillWindowRequest::fixture(&current_window)),
        ),
        (
            "rotate-window",
            vec!["-U".to_owned()],
            Request::RotateWindow(RotateWindowRequest {
                target: current_window.clone(),
                direction: RotateWindowDirection::Up,
                restore_zoom: false,
            }),
        ),
        (
            "break-pane",
            vec!["-d".to_owned()],
            Request::BreakPane(Box::new(BreakPaneRequest::fixture(&current_pane))),
        ),
        (
            "respawn-window",
            vec![
                "-k".to_owned(),
                "--".to_owned(),
                "printf".to_owned(),
                "hello".to_owned(),
            ],
            Request::RespawnWindow(Box::new(RespawnWindowRequest {
                target: current_window.clone(),
                kill: true,
                environment: None,
                command: Some(vec!["printf".to_owned(), "hello".to_owned()]),
                start_directory: None,
            })),
        ),
        (
            "respawn-window",
            vec!["-k".to_owned()],
            Request::RespawnWindow(Box::new(RespawnWindowRequest {
                target: current_window.clone(),
                kill: true,
                environment: None,
                command: None,
                start_directory: None,
            })),
        ),
        (
            "respawn-pane",
            vec![
                "-k".to_owned(),
                "--".to_owned(),
                "printf".to_owned(),
                "hello".to_owned(),
            ],
            Request::RespawnPane(Box::new(RespawnPaneRequest {
                command: Some(vec!["printf".to_owned(), "hello".to_owned()]),
                ..Fixture::fixture(&current_pane)
            })),
        ),
        (
            "respawn-pane",
            vec!["-k".to_owned()],
            Request::RespawnPane(Box::new(RespawnPaneRequest::fixture(&current_pane))),
        ),
        (
            "swap-pane",
            vec!["-U".to_owned()],
            Request::SwapPane(SwapPaneRequest {
                direction: Some(SwapPaneDirection::Up),
                detached: false,
                ..Fixture::fixture((&current_pane, &current_pane))
            }),
        ),
    ];

    for (command, arguments, expected) in cases {
        let parsed = parse_request_from_parts(
            command.to_owned(),
            arguments,
            None,
            &state.sessions,
            &state.options,
            &context,
        )
        .unwrap_or_else(|error| panic!("{command} should use the current target: {error}"));
        assert_eq!(parsed, expected, "unexpected parsed request for {command}");
    }
}

#[tokio::test]
async fn parsed_queue_resolves_attached_short_target_values_for_select_pane() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        })
        .await;

    let parsed = CommandParser::new()
        .parse("select-pane -t:.+")
        .expect("command parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 1),
            ))),
        )
        .await
        .expect("select-pane binding should resolve");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session
            .window_at(0)
            .expect("window exists")
            .active_pane_index(),
        0,
        "attached short-form -t target should resolve relative pane targets"
    );
}

#[tokio::test]
async fn parsed_queue_resolves_select_pane_mark_against_the_current_pane() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        })
        .await;

    let parsed = CommandParser::new()
        .parse("select-pane -m")
        .expect("command parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 1),
            ))),
        )
        .await
        .expect("select-pane -m should resolve against the current pane");

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .marked_pane_target()
            .expect("marked pane target exists")
            .to_string(),
        "alpha:0.1"
    );
}

#[tokio::test]
async fn parsed_queue_uses_marked_pane_as_default_swap_source() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        })
        .await;

    let (pane_zero_id, pane_one_id) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("window exists");
        (
            window.pane(0).expect("pane 0 exists").id(),
            window.pane(1).expect("pane 1 exists").id(),
        )
    };

    let mark = CommandParser::new()
        .parse("select-pane -m")
        .expect("mark command parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            mark,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 1),
            ))),
        )
        .await
        .expect("mark pane 1");

    let swap = CommandParser::new()
        .parse("swap-pane -t alpha:0.0")
        .expect("swap command parses");
    handler
        .execute_parsed_commands(std::process::id(), swap, pane_context(&alpha, 0))
        .await
        .expect("swap-pane should use the marked pane as source");

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(0))
        .expect("window exists");
    assert_eq!(window.pane(0).expect("pane 0 exists").id(), pane_one_id);
    assert_eq!(window.pane(1).expect("pane 1 exists").id(), pane_zero_id);
}

#[tokio::test]
async fn parsed_queue_uses_marked_pane_window_as_default_swap_window_source() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .create_window(NewWindowRequest {
            name: Some("marked-window".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;

    let mark = CommandParser::new()
        .parse("select-pane -m")
        .expect("mark command parses");
    handler
        .execute_parsed_commands(std::process::id(), mark, pane_context(&alpha, 1))
        .await
        .expect("mark pane in window 1");

    let swap = CommandParser::new()
        .parse("swap-window -t alpha:0")
        .expect("swap-window command parses");
    handler
        .execute_parsed_commands(std::process::id(), swap, pane_context(&alpha, 0))
        .await
        .expect("swap-window should use the marked pane window as source");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session.window_at(0).expect("window 0 exists").name(),
        Some("marked-window")
    );
}

#[tokio::test]
async fn parsed_queue_resolves_explicit_marked_pane_targets() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        })
        .await;

    let mark = CommandParser::new()
        .parse("select-pane -m")
        .expect("mark command parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            mark,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 1),
            ))),
        )
        .await
        .expect("mark pane 1");

    let command = CommandParser::new()
        .parse("display-message -p -t ~ '#{pane_index}'")
        .expect("display-message command parses");
    let output = handler
        .execute_parsed_commands(std::process::id(), command, pane_context(&alpha, 0))
        .await
        .expect("display-message should resolve marked pane");

    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n");
}

#[tokio::test]
async fn marked_pane_prefers_original_linked_window_slot() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session_name in [&alpha, &beta] {
        handler.create_session(session_name).await;
    }

    handler
        .handle_ok(LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        })
        .await;

    let mark = CommandParser::new()
        .parse("select-pane -t alpha:0.0 -m")
        .expect("mark command parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), mark)
        .await
        .expect("mark linked pane through alpha");

    let command = CommandParser::new()
        .parse("display-message -p -t '{marked}' '#{session_name}:#{window_index}.#{pane_index}'")
        .expect("display-message command parses");
    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), command)
        .await
        .expect("marked pane should resolve through the marked slot");

    assert_eq!(String::from_utf8_lossy(&output.stdout), "alpha:0.0\n");
}

#[tokio::test]
async fn parsed_queue_display_message_canfail_target_uses_empty_context_without_current_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(alpha).await;

    let command = CommandParser::new()
        .parse("display-message -p -t nosuch '#{session_name}:#{window_index}.#{pane_index}'")
        .expect("display-message command parses");
    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), command)
        .await
        .expect("display-message should tolerate a missing canfail target");

    assert_eq!(output.stdout(), b":.\n");
}

#[tokio::test]
async fn parsed_queue_display_message_canfail_target_falls_back_to_queue_current_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;

    let command = CommandParser::new()
        .parse("display-message -p -t '{up-of}' '#{session_name}:#{window_index}.#{pane_index}'")
        .expect("display-message command parses");
    let output = handler
        .execute_parsed_commands(std::process::id(), command, pane_context(&alpha, 0))
        .await
        .expect("display-message should fall back to queue current target");

    assert_eq!(output.stdout(), b"alpha:0.0\n");
}
