use super::*;
use crate::handler::with_expected_attach_and_session_identity;

#[tokio::test]
async fn if_shell_format_mode_dispatches_selected_rmux_command() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            else_command: Some("set-buffer -b chosen wrong".to_owned()),
            ..Fixture::fixture(("1", "set-buffer -b chosen selected"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());

    let response = handler.handle(show_buffer_request("chosen")).await;
    assert_eq!(
        response
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"selected"
    );
}

#[tokio::test]
async fn queued_if_shell_returns_nested_command_output() {
    let handler = RequestHandler::new();
    handler
        .handle_ok(SetBufferRequest::fixture((
            "queued-selected",
            b"queued-output",
        )))
        .await;

    let parsed = handler
        .parse_control_commands("if-shell -F 1 'show-buffer -b queued-selected'")
        .await
        .expect("queued if-shell parses");
    let output = handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::new(None)
                .with_client_name(Some("queued-if-shell-client".to_owned())),
        )
        .await
        .expect("queued if-shell executes");

    assert_eq!(output.stdout(), b"queued-output");
}

#[tokio::test]
async fn if_shell_format_mode_ignores_background_flag_like_tmux() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_if_shell(
            std::process::id(),
            IfShellRequest {
                format_mode: true,
                background: true,
                ..Fixture::fixture(("1", "display-message -p marker"))
            },
        )
        .await;

    assert_eq!(
        response
            .command_output()
            .expect("format-mode if-shell returns inline output")
            .stdout(),
        b"marker\n"
    );
    assert_eq!(
        handler
            .active_detached_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn background_if_shell_keeps_detached_write_access_after_response() {
    let handler = RequestHandler::new();
    let requester_pid = 424_006;

    {
        let _access =
            handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
        let response = handler
            .dispatch(
                requester_pid,
                IfShellRequest {
                    background: true,
                    ..Fixture::fixture((
                        delayed_true_shell_condition(),
                        "set-buffer -b bg-if-shell ok",
                    ))
                }
                .into_request(),
            )
            .await
            .response;
        assert_eq!(
            response,
            Response::IfShell(rmux_proto::IfShellResponse::no_output())
        );
    }

    wait_for_named_buffer(&handler, "bg-if-shell", b"ok").await;
}

#[tokio::test]
async fn background_if_shell_request_rejects_a_reused_control_registration() {
    let handler = RequestHandler::new();
    let requester_pid = 424_106;
    let original = session_name("if-shell-request-control-original");
    let replacement = session_name("if-shell-request-control-replacement");
    let wait_channel = "if-shell-request-control-registration-reuse";
    handler.create_session(&original).await;
    handler.create_session(&replacement).await;
    let (original_control_id, original_events) = handler
        .register_control_for_test(requester_pid, Some(&original))
        .await;

    let response = with_control_queue_identity(
        ControlClientIdentity::new(requester_pid, original_control_id),
        handler.handle_if_shell(
            requester_pid,
            IfShellRequest {
                background: true,
                ..Fixture::fixture((
                    delayed_true_shell_condition(),
                    format!(
                        "wait-for {wait_channel} ; kill-session -t {}",
                        replacement.as_str()
                    ),
                ))
            },
        ),
    )
    .await;
    assert_eq!(
        response,
        Response::IfShell(rmux_proto::IfShellResponse::no_output())
    );
    wait_for_background_waiter(&handler, wait_channel).await;

    let (_replacement_control_id, replacement_events) = handler
        .register_control_for_test(requester_pid, Some(&replacement))
        .await;
    release_background_waiter(&handler, wait_channel).await;

    assert_sessions_survive_background_control_reuse(&handler, &original, &replacement).await;
    drop((original_events, replacement_events));
}

#[tokio::test]
async fn queued_background_if_shell_keeps_detached_write_access_after_response() {
    let handler = RequestHandler::new();
    let requester_pid = 424_007;
    let parsed = CommandParser::new()
        .parse(&format!(
            "if-shell -b {} 'set-buffer -b bg-queued-if-shell ok'",
            command_quote(&delayed_true_shell_condition())
        ))
        .expect("background if-shell command parses");

    {
        let _access =
            handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
        let output = handler
            .execute_parsed_commands_for_test(requester_pid, parsed)
            .await
            .expect("background if-shell dispatch succeeds");
        assert!(output.stdout().is_empty());
    }

    wait_for_named_buffer(&handler, "bg-queued-if-shell", b"ok").await;
}

#[tokio::test]
async fn queued_if_shell_rejects_unknown_option_before_condition() {
    let handler = RequestHandler::new();
    let parsed = CommandParser::new()
        .parse("if-shell -Q true { set-buffer -b queued-unknown mutated }")
        .expect("generic parser preserves the unknown if-shell option");

    let error = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect_err("queued if-shell must reject an unknown option before its condition");

    assert_eq!(
        error,
        RmuxError::Server("command if-shell: unknown flag -Q".to_owned())
    );
    assert!(matches!(
        handler.handle(show_buffer_request("queued-unknown")).await,
        Response::Error(_)
    ));
}

#[tokio::test]
async fn queued_background_if_shell_rejects_a_reused_control_registration() {
    let handler = RequestHandler::new();
    let requester_pid = 424_107;
    let original = session_name("if-shell-queue-control-original");
    let replacement = session_name("if-shell-queue-control-replacement");
    let wait_channel = "if-shell-queue-control-registration-reuse";
    handler.create_session(&original).await;
    handler.create_session(&replacement).await;
    let (original_control_id, original_events) = handler
        .register_control_for_test(requester_pid, Some(&original))
        .await;

    let commands = CommandParser::new()
        .parse(&format!(
            "if-shell -b {} {{ wait-for {wait_channel} ; kill-session -t {} }}",
            command_quote(&delayed_true_shell_condition()),
            replacement.as_str()
        ))
        .expect("background queued if-shell command parses");
    let result = handler
        .execute_control_commands_identity(requester_pid, original_control_id, commands)
        .await;
    assert!(result.error.is_none(), "{result:?}");
    wait_for_background_waiter(&handler, wait_channel).await;

    let (_replacement_control_id, replacement_events) = handler
        .register_control_for_test(requester_pid, Some(&replacement))
        .await;
    release_background_waiter(&handler, wait_channel).await;

    assert_sessions_survive_background_control_reuse(&handler, &original, &replacement).await;
    drop((original_events, replacement_events));
}

async fn assert_background_if_shell_rejects_reused_attach_registration(queued: bool) {
    let handler = RequestHandler::new();
    let requester_pid = if queued { 424_208 } else { 424_207 };
    let suffix = if queued { "queue" } else { "request" };
    let original = session_name(&format!("if-shell-{suffix}-attach-original"));
    let replacement = session_name(&format!("if-shell-{suffix}-attach-replacement"));
    let wait_channel = format!("if-shell-{suffix}-attach-registration-reuse");
    handler.create_session(&original).await;
    handler.create_session(&replacement).await;
    let _original_control_rx = handler.attach_client(requester_pid, &original).await;
    let original_identity = handler.active_attach_identity_for_test(requester_pid).await;

    if queued {
        let commands = CommandParser::new()
            .parse(&format!(
                "if-shell -b {} {{ wait-for {wait_channel} ; detach-client }}",
                command_quote(&delayed_true_shell_condition())
            ))
            .expect("background queued if-shell command parses");
        let output = with_expected_attach_and_session_identity(
            original_identity,
            original.clone(),
            original_identity.session_id(),
            handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
        .expect("background queued if-shell dispatch succeeds");
        assert!(output.stdout().is_empty());
    } else {
        let response = with_expected_attach_and_session_identity(
            original_identity,
            original.clone(),
            original_identity.session_id(),
            handler.handle_if_shell(
                requester_pid,
                IfShellRequest {
                    background: true,
                    ..Fixture::fixture((
                        delayed_true_shell_condition(),
                        format!("wait-for {wait_channel} ; detach-client"),
                    ))
                },
            ),
        )
        .await;
        assert_eq!(
            response,
            Response::IfShell(rmux_proto::IfShellResponse::no_output())
        );
    }
    wait_for_background_waiter(&handler, &wait_channel).await;

    let mut replacement_control_rx = handler.attach_client(requester_pid, &replacement).await;
    let replacement_identity = handler.active_attach_identity_for_test(requester_pid).await;
    while replacement_control_rx.try_recv().is_ok() {}

    release_background_waiter(&handler, &wait_channel).await;
    wait_for_detached_request_count(&handler, 0).await;
    assert!(
        handler
            .current_live_attach_input(replacement_identity)
            .await,
        "stale background if-shell must not detach the same-PID replacement"
    );
    while let Ok(control) = replacement_control_rx.try_recv() {
        assert!(
            !matches!(control, AttachControl::Detach),
            "stale background if-shell detached the replacement registration"
        );
    }

    let state = handler.state.lock().await;
    assert!(state.sessions.contains_session(&original));
    assert!(state.sessions.contains_session(&replacement));
}

#[tokio::test]
async fn background_if_shell_rejects_a_reused_attach_registration() {
    assert_background_if_shell_rejects_reused_attach_registration(false).await;
    assert_background_if_shell_rejects_reused_attach_registration(true).await;
}

#[tokio::test]
async fn background_if_shell_queue_survives_a_same_registration_session_switch() {
    let handler = RequestHandler::new();
    let requester_pid = 424_308;
    let alpha = session_name("if-shell-attach-switch-alpha");
    let beta = session_name("if-shell-attach-switch-beta");
    let wait_channel = "if-shell-attach-session-switch";
    let followed_window_name = "if-shell-followed-attached-session";
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    let _control_rx = handler.attach_client(requester_pid, &alpha).await;
    let identity = handler.active_attach_identity_for_test(requester_pid).await;

    let commands = CommandParser::new()
        .parse(&format!(
            "if-shell -b {} {{ wait-for {wait_channel} ; rename-window {followed_window_name} }} ; switch-client -t {beta}",
            command_quote(&delayed_true_shell_condition())
        ))
        .expect("background if-shell and attached switch parse");
    let output = with_expected_attach_and_session_identity(
        identity,
        alpha.clone(),
        identity.session_id(),
        handler.execute_parsed_commands_for_test(requester_pid, commands),
    )
    .await
    .expect("same-registration attached switch keeps the outer queue valid");
    assert!(output.stdout().is_empty());

    let switched_identity = handler.active_attach_identity_for_test(requester_pid).await;
    assert_eq!(switched_identity.attach_id(), identity.attach_id());
    assert_eq!(
        handler
            .active_attach
            .lock()
            .await
            .by_pid
            .get(&requester_pid)
            .expect("attached registration survives")
            .session_name,
        beta
    );
    wait_for_background_waiter(&handler, wait_channel).await;
    replace_background_identity_session(&handler, alpha.clone()).await;
    release_background_waiter(&handler, wait_channel).await;
    wait_for_active_window_name(&handler, &beta, followed_window_name).await;
    let state = handler.state.lock().await;
    let replacement = state
        .sessions
        .session(&alpha)
        .expect("replacement alpha exists");
    assert_ne!(
        replacement
            .window_at(replacement.active_window_index())
            .and_then(rmux_core::Window::name),
        Some(followed_window_name),
        "stale background context mutated the replacement alpha session"
    );
}

async fn assert_explicit_background_if_shell_target_survives_switch(queued: bool) {
    let handler = RequestHandler::new();
    let requester_pid = if queued { 424_310 } else { 424_309 };
    let suffix = if queued { "queue" } else { "request" };
    let alpha = session_name(&format!("if-shell-explicit-{suffix}-alpha"));
    let beta = session_name(&format!("if-shell-explicit-{suffix}-beta"));
    let gamma = session_name(&format!("if-shell-explicit-{suffix}-gamma"));
    let wait_channel = format!("if-shell-explicit-{suffix}-wait");
    let expected_window_name = format!("if-shell-explicit-{suffix}-target");
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    handler.create_session(&gamma).await;
    let _control_rx = handler.attach_client(requester_pid, &alpha).await;
    let identity = handler.active_attach_identity_for_test(requester_pid).await;

    if queued {
        let commands = CommandParser::new()
            .parse(&format!(
                "if-shell -b -t {gamma}:0.0 {} {{ wait-for {wait_channel} ; rename-window {expected_window_name} }}",
                command_quote(builtin_true_shell_condition())
            ))
            .expect("queued explicit background if-shell parses");
        with_expected_attach_and_session_identity(
            identity,
            alpha.clone(),
            identity.session_id(),
            handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
        .expect("queued explicit background if-shell starts");
    } else {
        let response = with_expected_attach_and_session_identity(
            identity,
            alpha.clone(),
            identity.session_id(),
            handler.handle_if_shell(
                requester_pid,
                IfShellRequest {
                    target: Some(Target::Pane(PaneTarget::with_window(gamma.clone(), 0, 0))),
                    background: true,
                    ..Fixture::fixture((
                        builtin_true_shell_condition(),
                        format!("wait-for {wait_channel} ; rename-window {expected_window_name}"),
                    ))
                },
            ),
        )
        .await;
        assert_eq!(
            response,
            Response::IfShell(rmux_proto::IfShellResponse::no_output())
        );
    }

    wait_for_background_waiter(&handler, &wait_channel).await;
    let switch = CommandParser::new()
        .parse(&format!("switch-client -t {beta}"))
        .expect("attached switch parses");
    with_expected_attach_and_session_identity(
        identity,
        alpha,
        identity.session_id(),
        handler.execute_parsed_commands_for_test(requester_pid, switch),
    )
    .await
    .expect("attached client switches while explicit background command waits");
    release_background_waiter(&handler, &wait_channel).await;

    wait_for_active_window_name(&handler, &gamma, &expected_window_name).await;
    let state = handler.state.lock().await;
    assert_ne!(
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(session.active_window_index()))
            .and_then(rmux_core::Window::name),
        Some(expected_window_name.as_str()),
        "explicit background if-shell target must not rebase onto the attached session"
    );
}

#[tokio::test]
async fn explicit_background_if_shell_targets_survive_attached_switch() {
    assert_explicit_background_if_shell_target_survives_switch(false).await;
    assert_explicit_background_if_shell_target_survives_switch(true).await;
}

#[tokio::test]
async fn background_if_shell_is_tracked_as_detached_request_until_finished() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            background: true,
            ..Fixture::fixture(("sleep 0.2; true", "display-message done"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
    wait_for_detached_request_count(&handler, 1).await;
    wait_for_detached_request_count(&handler, 0).await;
}

#[tokio::test]
async fn if_shell_format_mode_expands_socket_path_without_target() {
    let handler = RequestHandler::new();
    handler.set_socket_path("/tmp/rmux-test.sock");

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            else_command: Some("set-buffer -b chosen wrong".to_owned()),
            ..Fixture::fixture(("#{socket_path}", "set-buffer -b chosen selected"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());

    let response = handler.handle(show_buffer_request("chosen")).await;
    assert_eq!(
        response
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"selected"
    );
}

#[tokio::test]
async fn if_shell_format_mode_treats_zero_prefixed_values_as_false_like_tmux() {
    let handler = RequestHandler::new();

    for condition in ["00", "09", "01", "0abc", "0.0"] {
        let buffer = format!("chosen-{condition}");
        let response = handler
            .handle_ok(IfShellRequest {
                format_mode: true,
                else_command: Some(format!("set-buffer -b {buffer} fallback")),
                ..Fixture::fixture((condition, format!("set-buffer -b {buffer} selected")))
            })
            .await;
        assert_eq!(response, rmux_proto::IfShellResponse::no_output());

        let response = handler.handle(show_buffer_request(&buffer)).await;
        assert_eq!(
            response
                .command_output()
                .expect("show-buffer output")
                .stdout(),
            b"fallback",
            "condition {condition:?} should be false"
        );
    }
}

#[tokio::test]
async fn if_shell_format_mode_without_target_uses_preferred_session_context() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            else_command: Some("set-buffer -b chosen wrong".to_owned()),
            ..Fixture::fixture(("#{session_name}", "set-buffer -b chosen selected"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());

    let response = handler.handle(show_buffer_request("chosen")).await;
    assert_eq!(
        response
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"selected"
    );
}

#[tokio::test]
async fn if_shell_missing_explicit_target_is_nonfatal() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(alpha).await;

    let response = handler
        .handle(
            IfShellRequest {
                format_mode: true,
                target: Some(Target::Session(session_name("missing"))),
                ..Fixture::fixture(("1", "display-message -p '#{session_name}'"))
            }
            .into_request(),
        )
        .await;

    assert_eq!(
        response
            .command_output()
            .expect("if-shell nested command output")
            .stdout(),
        b"alpha\n"
    );
}

#[tokio::test]
async fn source_file_if_shell_true_executes_brace_command_list() {
    let handler = RequestHandler::new();
    // The caller cwd starts both the managed reader that resolves `main.conf` and the sourced
    // `if-shell` condition, so it has to name a directory inside this handler's own seed.
    let root = seed_scratch_dir(&handler, "if-shell-true-brace")
        .path()
        .to_path_buf();
    let config = root.join("main.conf");
    write_config(&config, "if-shell true { set-buffer -b chosen selected }\n");

    let response = handler
        .handle(source_file_request(
            vec!["main.conf".to_owned()],
            Some(root.clone()),
        ))
        .await;
    assert_eq!(
        response,
        Response::SourceFile(rmux_proto::SourceFileResponse::no_output())
    );

    let response = handler.handle(show_buffer_request("chosen")).await;
    assert_eq!(
        response
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"selected"
    );
}

#[tokio::test]
async fn if_shell_pane_id_target_resolves_like_display_message() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    handler.create_session(&alpha).await;

    let display_message = CommandParser::new()
        .parse("display-message -p -t %0 OKDM")
        .expect("display-message parses");
    let display_output = handler
        .execute_parsed_commands(
            std::process::id(),
            display_message,
            QueueExecutionContext::without_caller_cwd(),
        )
        .await
        .expect("display-message -t %0 should resolve");
    assert_eq!(String::from_utf8_lossy(&display_output.stdout), "OKDM\n");

    let if_shell = CommandParser::new()
        .parse("if-shell -F -t %0 1 \"display-message -p XOK\"")
        .expect("if-shell parses");
    let if_shell_output = handler
        .execute_parsed_commands(
            std::process::id(),
            if_shell,
            QueueExecutionContext::without_caller_cwd(),
        )
        .await
        .expect("if-shell -t %0 should resolve");
    assert_eq!(String::from_utf8_lossy(&if_shell_output.stdout), "XOK\n");
}

#[tokio::test]
async fn queued_if_shell_target_becomes_branch_current_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("if-shell -F -t beta:0.0 1 { new-window -d -n nested }")
        .expect("if-shell parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 0),
            ))),
        )
        .await
        .expect("if-shell branch should execute");

    let state = handler.state.lock().await;
    let alpha_windows = state
        .sessions
        .session(&alpha)
        .expect("alpha exists")
        .windows()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let beta_session = state.sessions.session(&beta).expect("beta exists");
    assert_eq!(alpha_windows, vec![0]);
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        beta_session
            .window_at(1)
            .expect("nested window exists")
            .name(),
        Some("nested")
    );
}

#[tokio::test]
async fn queued_if_shell_accepts_compact_format_target_with_attached_value() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("if-shell -Ft= 1 { display-message -p '#{session_name}' }")
        .expect("if-shell compact target parses");
    let output = handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd()
                .with_current_target(Some(Target::Session(beta)))
                .with_mouse_target(Some(Target::Window(WindowTarget::with_window(alpha, 0)))),
        )
        .await
        .expect("compact if-shell branch should execute");
    assert_eq!(output.stdout(), b"alpha\n");
}

#[tokio::test]
async fn queued_if_shell_compact_mouse_target_falls_back_to_current_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("if-shell -Ft= 1 { display-message -p '#{session_name}' }")
        .expect("if-shell compact target parses");
    let output = handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd()
                .with_current_target(Some(Target::Session(beta))),
        )
        .await
        .expect("compact if-shell branch should execute without mouse context");
    assert_eq!(output.stdout(), b"beta\n");
}

#[tokio::test]
async fn queued_if_shell_separated_mouse_target_falls_back_to_current_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("if-shell -F -t = 1 { display-message -p '#{session_name}' }")
        .expect("if-shell separated target parses");
    let output = handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd()
                .with_current_target(Some(Target::Session(beta))),
        )
        .await
        .expect("separated if-shell branch should execute without mouse context");
    assert_eq!(output.stdout(), b"beta\n");
}

#[tokio::test]
async fn queued_if_shell_accepts_compact_format_target_with_next_argument() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    for session in [&alpha, &beta] {
        handler.create_session(session).await;
    }

    let parsed = CommandParser::new()
        .parse("if-shell -Ft beta:0.0 1 { new-window -d -n compact }")
        .expect("if-shell compact target parses");
    handler
        .execute_parsed_commands(
            std::process::id(),
            parsed,
            QueueExecutionContext::without_caller_cwd().with_current_target(Some(Target::Pane(
                PaneTarget::with_window(alpha.clone(), 0, 0),
            ))),
        )
        .await
        .expect("compact if-shell branch should execute");

    let state = handler.state.lock().await;
    let alpha_windows = state
        .sessions
        .session(&alpha)
        .expect("alpha exists")
        .windows()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let beta_session = state.sessions.session(&beta).expect("beta exists");
    assert_eq!(alpha_windows, vec![0]);
    assert_eq!(
        beta_session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        beta_session
            .window_at(1)
            .expect("compact window exists")
            .name(),
        Some("compact")
    );
}

#[tokio::test]
async fn if_shell_false_without_else_is_a_successful_noop() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture(("0", "set-buffer impossible"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
}

#[tokio::test]
async fn scripted_pane_commands_accept_session_targets_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;

    handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture(("1", "copy-mode -t alpha"))
        })
        .await;

    let output = handler
        .display_print(PaneTarget::new(alpha, 0), "#{pane_in_mode}")
        .await;
    assert_eq!(output, b"1\n");
}

#[tokio::test]
async fn if_shell_shell_mode_uses_bin_sh_environment_and_caller_cwd() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let root = temp_root("if-shell-shell-mode");
    let marker = root.join("shell-used.txt");
    let shell_path = root.join("record-shell.sh");
    // The caller's directory is separate from the two host files above, and inside the seed: the
    // predicate is a managed job, and a job can only be opened over a directory this daemon
    // leases. The recording shell and its marker stay on the host, named by absolute path, so
    // "the configured default-shell never ran" is still a claim about the real filesystem.
    let caller_cwd = seed_scratch_dir(&handler, "if-shell-shell-mode");

    write_executable_script(
        &shell_path,
        &format!(
            "#!/bin/sh\nprintf used > {}\nexec /bin/sh \"$@\"\n",
            sh_quote_path(&marker)
        ),
    );

    handler.create_session(&alpha).await;
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultShell,
            &shell_path.to_string_lossy(),
        )
        .await;
    handler
        .handle_ok(SetEnvironmentRequest {
            scope: ScopeSelector::Session(alpha.clone()),
            name: "FOO".to_owned(),
            value: "bar".to_owned(),
            mode: None,
            hidden: false,
            format: false,
        })
        .await;

    // A job's own directory is the *snapshot* of the seed — `<root>/.marsh/seed/snap/
    // <uid>/<seed-relative dir>` — and the uid is minted per job, so no absolute path
    // this test could write down would ever equal `$PWD`. The seed-relative tail is the
    // part that is knowable, and it is also the part the regression would lose: a start
    // directory that was ignored opens at the snapshot root, where the suffix is absent.
    let condition = format!(
        "test \"$FOO\" = bar && test \"${{PWD%/{relative}}}\" != \"$PWD\"",
        relative = caller_cwd.relative()
    );
    let response = handler
        .handle_ok(IfShellRequest {
            else_command: Some("set-buffer -b chosen no".to_owned()),
            target: Some(Target::Session(alpha)),
            caller_cwd: Some(caller_cwd.path().to_path_buf()),
            ..Fixture::fixture((condition, "set-buffer -b chosen yes"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
    assert_eq!(
        handler
            .handle(show_buffer_request("chosen"))
            .await
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"yes"
    );
    assert!(
        !marker.exists(),
        "if-shell should not execute default-shell for tmux jobs"
    );
}

#[tokio::test]
async fn if_shell_nested_set_buffer_accepts_hyphen_prefixed_content_after_separator() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture(("1", "set-buffer -b hyphen -- -value"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());

    let response = handler.handle(show_buffer_request("hyphen")).await;
    assert_eq!(
        response
            .command_output()
            .expect("show-buffer output")
            .stdout(),
        b"-value"
    );
}

#[tokio::test]
async fn if_shell_nested_set_buffer_rejects_hyphen_prefixed_content_without_separator() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(
            IfShellRequest {
                format_mode: true,
                ..Fixture::fixture(("1", "set-buffer -b rejected -value"))
            }
            .into_request(),
        )
        .await;

    let Response::Error(error) = response else {
        panic!("unknown nested set-buffer flag must fail: {response:?}");
    };
    assert_eq!(
        error.error,
        RmuxError::Server("command set-buffer: unknown flag -v".to_owned())
    );
    assert!(matches!(
        handler.handle(show_buffer_request("rejected")).await,
        Response::Error(_)
    ));
}

#[tokio::test]
async fn if_shell_nested_wait_for_accepts_hyphen_prefixed_channel_after_separator() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture(("1", "wait-for -S -- -channel"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
}

#[tokio::test]
async fn if_shell_nested_run_shell_accepts_double_dash_before_command() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture((
                "1",
                format!("run-shell -- {}", command_quote(&shell_success_command())),
            ))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
}

#[tokio::test]
async fn if_shell_string_mode_runs_multiple_commands_in_one_group() {
    let handler = RequestHandler::new();

    let response = handler
        .handle_ok(IfShellRequest {
            format_mode: true,
            ..Fixture::fixture(("1", "set-buffer -b one first; set-buffer -b two second"))
        })
        .await;

    assert_eq!(response, rmux_proto::IfShellResponse::no_output());
    assert_eq!(
        handler
            .handle(show_buffer_request("one"))
            .await
            .command_output()
            .expect("one buffer output")
            .stdout(),
        b"first"
    );
    assert_eq!(
        handler
            .handle(show_buffer_request("two"))
            .await
            .command_output()
            .expect("two buffer output")
            .stdout(),
        b"second"
    );
}

#[tokio::test]
async fn if_shell_inserted_assignments_apply_before_parent_queue_tail() {
    let handler = RequestHandler::new();
    let parsed = CommandParser::new()
        .parse("if-shell -F 1 { FOO=bar } ; run-shell \"exit 0\"")
        .expect("commands parse");

    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("queue succeeds");

    assert!(output.stdout().is_empty());

    let state = handler.state.lock().await;
    assert_eq!(state.environment.global_value("FOO"), Some("bar"));
}
