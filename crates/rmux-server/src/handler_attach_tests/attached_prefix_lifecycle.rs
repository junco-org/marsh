use super::*;
use crate::test_fixtures::{wait_for_file_contents, wait_until};
use crate::test_shell::command_quote;
use rmux_proto::BindKeyRequest;

const PROMPT_NEW_WINDOW_INPUT: &[u8] =
    b"\x02:new-window -- 'printf ISSUE8_WINDOW_READY; sleep 30'\r";

async fn bind_attached_prompt_test_key(handler: &RequestHandler, key: &str, command: Vec<String>) {
    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached prompt target-client regression".to_owned()),
            ..Fixture::fixture(("prefix", key, command))
        })
        .await;
}

#[tokio::test]
async fn attached_prefix_d_dispatches_detach_client() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02d")
        .await
        .expect("prefix d dispatches");

    // Entering and leaving the prefix key table now repaints the status bar
    // (so #{client_prefix} can show a prefix indicator), so the Detach control
    // may be preceded by status-refresh Write frames; scan past them.
    recv_matching_attach_control(&mut control_rx, "prefix d detach", |control| {
        matches!(control, AttachControl::Detach)
    })
    .await;
}

#[tokio::test]
async fn attached_prefix_d_dispatches_detach_client_across_separate_reads() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02")
        .await
        .expect("prefix key input");
    handler
        .handle_attached_live_input_for_test(requester_pid, b"d")
        .await
        .expect("prefix d input");

    recv_matching_attach_control(&mut control_rx, "split prefix d detach", |control| {
        matches!(control, AttachControl::Detach)
    })
    .await;
}

#[tokio::test]
async fn attached_send_prefix_then_does_not_detach() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02")
        .await
        .expect("prefix key input");
    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02d")
        .await
        .expect("send-prefix then d input");

    while let Ok(control) = control_rx.try_recv() {
        assert!(
            !matches!(control, AttachControl::Detach),
            "C-b C-b d must send a literal prefix followed by d, not detach"
        );
    }
}

#[tokio::test]
async fn attached_prefix_c_creates_window_across_separate_reads() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02")
        .await
        .expect("prefix key input");
    handler
        .handle_attached_live_input_for_test(requester_pid, b"c")
        .await
        .expect("prefix c input");

    assert_eq!(
        active_windows(&handler, &alpha).await,
        "0:0\n1:1\n",
        "C-b c must still create a new window when keys arrive in separate reads"
    );
}

#[tokio::test]
async fn attached_command_prompt_can_chain_choose_tree_overlay() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let prompted = session_name("prompted");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("prompt-then-choose-tree".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "X",
                [
                    "command-prompt",
                    "-p",
                    "name:",
                    "new-session -d -s '%%' ; choose-tree -Zs",
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02X")
        .await
        .expect("prefix X opens command-prompt");
    wait_for_attach_output_containing(&mut control_rx, "name:").await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"prompted\r")
        .await
        .expect("prompt response opens choose-tree");

    let rendered = wait_for_attach_output_containing(&mut control_rx, "sort:").await;
    assert!(
        rendered.contains("alpha") && rendered.contains("prompted"),
        "choose-tree should render both sessions after prompt continuation, got:\n{rendered}"
    );
    {
        let state = handler.state.lock().await;
        assert!(
            state.sessions.session(&prompted).is_some(),
            "prompt continuation should create the requested session"
        );
    }

    handler
        .handle_attached_live_input_for_test(requester_pid, b"q")
        .await
        .expect("q exits chained choose-tree");
    wait_until(
        ATTACH_LIFECYCLE_TIMEOUT,
        Duration::from_millis(25),
        async || {
            let active_attach = handler.active_attach.lock().await;
            let mode_active = active_attach
                .by_pid
                .get(&requester_pid)
                .is_some_and(|active| active.mode_tree.is_some());
            if mode_active {
                Err(())
            } else {
                Ok(())
            }
        },
    )
    .await
    .unwrap_or_else(|()| panic!("chained choose-tree did not exit after q"));
}

#[tokio::test]
async fn attached_foreground_prompts_resolve_explicit_target_client() {
    let handler = RequestHandler::new();
    let owner_pid = u32::MAX - 501;
    let target_pid = u32::MAX - 502;
    let alpha = session_name("attached-prompt-explicit-target");
    let _owner_rx = create_attached_session(&handler, owner_pid, &alpha).await;
    let _target_rx = handler.attach_client(target_pid, &alpha).await;

    bind_attached_prompt_test_key(
        &handler,
        "X",
        vec![
            "command-prompt".to_owned(),
            "-t".to_owned(),
            target_pid.to_string(),
            "-p".to_owned(),
            "target-name:".to_owned(),
            "set-option -g -F @targeted-command-prompt '%%:#{client_name}'".to_owned(),
        ],
    )
    .await;
    handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02X")
        .await
        .expect("owner opens command-prompt on target client");
    assert!(!handler.prompt_active(owner_pid).await);
    assert!(handler.prompt_active(target_pid).await);

    handler
        .handle_attached_live_input_for_test(target_pid, b"target-value\r")
        .await
        .expect("target client submits command-prompt");
    wait_for_global_option_value(
        &handler,
        "@targeted-command-prompt",
        &format!(
            "target-value:{}",
            crate::handler::attached_client_name(owner_pid)
        ),
    )
    .await;

    bind_attached_prompt_test_key(
        &handler,
        "Y",
        vec![
            "confirm-before".to_owned(),
            "-t".to_owned(),
            target_pid.to_string(),
            "-p".to_owned(),
            "confirm-target?".to_owned(),
            "set-option -g -F @targeted-confirm-before '#{client_name}'".to_owned(),
        ],
    )
    .await;
    handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02Y")
        .await
        .expect("owner opens confirm-before on target client");
    assert!(!handler.prompt_active(owner_pid).await);
    assert!(handler.prompt_active(target_pid).await);

    handler
        .handle_attached_live_input_for_test(target_pid, b"y")
        .await
        .expect("target client accepts confirm-before");
    wait_for_global_option_value(
        &handler,
        "@targeted-confirm-before",
        &crate::handler::attached_client_name(owner_pid),
    )
    .await;
}

#[tokio::test]
async fn attached_foreground_prompts_reject_unknown_target_client() {
    let handler = RequestHandler::new();
    let owner_pid = u32::MAX - 503;
    let alpha = session_name("attached-prompt-unknown-target");
    let _owner_rx = create_attached_session(&handler, owner_pid, &alpha).await;
    let missing_pid = 999_999_u32;

    bind_attached_prompt_test_key(
        &handler,
        "X",
        vec![
            "command-prompt".to_owned(),
            "-t".to_owned(),
            missing_pid.to_string(),
            "-p".to_owned(),
            "missing:".to_owned(),
            "set-option -g @missing-command-prompt reached".to_owned(),
        ],
    )
    .await;
    let error = handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02X")
        .await
        .expect_err("unknown command-prompt target must fail closed");
    assert!(
        error
            .to_string()
            .contains(&format!("can't find client: {missing_pid}")),
        "unexpected command-prompt error: {error}"
    );
    assert!(!handler.prompt_active(owner_pid).await);

    bind_attached_prompt_test_key(
        &handler,
        "Y",
        vec![
            "confirm-before".to_owned(),
            "-t".to_owned(),
            missing_pid.to_string(),
            "-p".to_owned(),
            "missing?".to_owned(),
            "set-option -g @missing-confirm-before reached".to_owned(),
        ],
    )
    .await;
    let error = handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02Y")
        .await
        .expect_err("unknown confirm-before target must fail closed");
    assert!(
        error
            .to_string()
            .contains(&format!("can't find client: {missing_pid}")),
        "unexpected confirm-before error: {error}"
    );
    assert!(!handler.prompt_active(owner_pid).await);
}

#[tokio::test]
async fn attached_foreground_prompts_without_target_stay_on_binding_owner() {
    let handler = RequestHandler::new();
    let owner_pid = u32::MAX - 504;
    let other_pid = u32::MAX - 505;
    let alpha = session_name("attached-prompt-default-target");
    let _owner_rx = create_attached_session(&handler, owner_pid, &alpha).await;
    let _other_rx = handler.attach_client(other_pid, &alpha).await;

    bind_attached_prompt_test_key(
        &handler,
        "X",
        vec![
            "command-prompt".to_owned(),
            "-p".to_owned(),
            "owner-name:".to_owned(),
            "set-option -g @owner-command-prompt '%%'".to_owned(),
        ],
    )
    .await;
    handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02X")
        .await
        .expect("owner opens its command-prompt");
    assert!(handler.prompt_active(owner_pid).await);
    assert!(!handler.prompt_active(other_pid).await);

    handler
        .handle_attached_live_input_for_test(owner_pid, b"owner-value\r")
        .await
        .expect("owner submits command-prompt");
    wait_for_global_option_value(&handler, "@owner-command-prompt", "owner-value").await;

    bind_attached_prompt_test_key(
        &handler,
        "Y",
        vec![
            "confirm-before".to_owned(),
            "-p".to_owned(),
            "confirm-owner?".to_owned(),
            "set-option -g @owner-confirm-before yes".to_owned(),
        ],
    )
    .await;
    handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02Y")
        .await
        .expect("owner opens its confirm-before");
    assert!(handler.prompt_active(owner_pid).await);
    assert!(!handler.prompt_active(other_pid).await);

    handler
        .handle_attached_live_input_for_test(owner_pid, b"y")
        .await
        .expect("owner accepts confirm-before");
    wait_for_global_option_value(&handler, "@owner-confirm-before", "yes").await;
}

#[tokio::test]
async fn targeted_attached_prompt_completion_rejects_replaced_binding_owner() {
    let handler = RequestHandler::new();
    let owner_pid = u32::MAX - 506;
    let target_pid = u32::MAX - 507;
    let alpha = session_name("attached-prompt-replaced-owner");
    let mut original_owner_rx = create_attached_session(&handler, owner_pid, &alpha).await;
    let _target_rx = handler.attach_client(target_pid, &alpha).await;

    bind_attached_prompt_test_key(
        &handler,
        "X",
        vec![
            "command-prompt".to_owned(),
            "-t".to_owned(),
            target_pid.to_string(),
            "-p".to_owned(),
            "target-name:".to_owned(),
            "set-option -g @replaced-prompt-owner should-not-run".to_owned(),
        ],
    )
    .await;
    handler
        .handle_attached_live_input_for_test(owner_pid, b"\x02X")
        .await
        .expect("owner opens command-prompt on target client");
    assert!(handler.prompt_active(target_pid).await);

    let _replacement_rx = handler.attach_client(owner_pid, alpha).await;
    recv_matching_attach_control(
        &mut original_owner_rx,
        "original prompt owner replacement",
        |control| matches!(control, AttachControl::Detach),
    )
    .await;

    handler
        .handle_attached_live_input_for_test(target_pid, b"accepted\r")
        .await
        .expect("target client submits after owner replacement");
    assert!(!handler.prompt_active(target_pid).await);
    sleep(Duration::from_millis(100)).await;

    let response = handler
        .handle(Request::ShowOptions(rmux_proto::ShowOptionsRequest {
            scope: rmux_proto::OptionScopeSelector::SessionGlobal,
            name: Some("@replaced-prompt-owner".to_owned()),
            value_only: true,
            include_inherited: false,
            quiet: true,
            include_hooks: false,
        }))
        .await;
    let output = response
        .command_output()
        .expect("quiet show-options returns command output");
    assert_eq!(
        output.stdout(),
        b"",
        "stale owner continuation must fail closed"
    );
}

#[tokio::test]
async fn attached_binding_run_shell_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 71;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let root = std::env::temp_dir().join(format!(
        "rmux-attached-client-name-{}-{requester_pid}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("client-name temp root");
    let output_path = root.join("client-name.txt");
    let shell_command = client_name_file_shell_command(&output_path);

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name".to_owned()),
            ..Fixture::fixture(("prefix", "T", ["run-shell", "-b", &shell_command]))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02T")
        .await
        .expect("prefix T dispatches run-shell binding");

    wait_for_file_contents(
        &output_path,
        &crate::handler::attached_client_name(requester_pid),
    )
    .await;
}

#[tokio::test]
async fn attached_binding_new_window_shell_command_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 73;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let root = std::env::temp_dir().join(format!(
        "rmux-attached-new-window-client-name-{}-{requester_pid}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("new-window client-name temp root");
    let output_path = root.join("client-name.txt");
    let pane_command = client_name_file_pane_command(&output_path);

    let mut command = vec!["new-window".to_owned(), "-d".to_owned(), "--".to_owned()];
    command.extend(pane_command);
    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name-new-window".to_owned()),
            ..Fixture::fixture(("prefix", "V", command))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02V")
        .await
        .expect("prefix V dispatches new-window binding");

    let expected_client = crate::handler::attached_client_name(requester_pid);
    wait_for_file_contents(&output_path, &expected_client).await;
}

#[tokio::test]
async fn attached_binding_split_window_shell_command_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 74;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let root = std::env::temp_dir().join(format!(
        "rmux-attached-split-window-client-name-{}-{requester_pid}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("split-window client-name temp root");
    let output_path = root.join("client-name.txt");
    let pane_command = client_name_file_pane_command(&output_path);

    let mut command = vec!["split-window".to_owned(), "-d".to_owned(), "--".to_owned()];
    command.extend(pane_command);
    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name-split-window".to_owned()),
            ..Fixture::fixture(("prefix", "W", command))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02W")
        .await
        .expect("prefix W dispatches split-window binding");

    let expected_client = crate::handler::attached_client_name(requester_pid);
    wait_for_file_contents(&output_path, &expected_client).await;
}

#[tokio::test]
async fn attached_binding_set_option_format_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 75;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name-set-option".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "Y",
                [
                    "set-option",
                    "-g",
                    "-F",
                    "@attached-client-context",
                    "#{client_name}:#{session_name}:#{pane_index}",
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02Y")
        .await
        .expect("prefix Y dispatches set-option binding");

    wait_for_global_option_value(
        &handler,
        "@attached-client-context",
        &format!(
            "{}:alpha:0",
            crate::handler::attached_client_name(requester_pid)
        ),
    )
    .await;
}

#[tokio::test]
async fn attached_binding_source_file_preserves_client_context() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 76;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let root = std::env::temp_dir().join(format!(
        "rmux-attached-source-client-context-{}-{requester_pid}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("source-file client context temp root");
    let source_path = root.join("client-context.conf");
    let run_shell_path = root.join("run-shell-client-name.txt");
    let new_window_path = root.join("new-window-client-name.txt");
    let split_window_path = root.join("split-window-client-name.txt");

    let source = format!(
        "set-option -g -F @source-client-context '{}'\n\
         if-shell -F '{}' '{}' '{}'\n\
         run-shell -b {}\n",
        "#{client_name}:#{session_name}:#{pane_index}",
        "#{client_name}",
        "set-buffer -b source-client-if-shell yes",
        "set-buffer -b source-client-if-shell no",
        command_quote(&client_name_file_shell_command(&run_shell_path)),
    );
    let source = format!(
        "{source}new-window -d -- {}\n\
         split-window -d -- {}\n",
        quote_command_arguments(&client_name_file_pane_command(&new_window_path)),
        quote_command_arguments(&client_name_file_pane_command(&split_window_path)),
    );
    std::fs::write(&source_path, source).expect("source-file client context config");

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-context-source-file".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "Z",
                ["source-file", &source_path.to_string_lossy()],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02Z")
        .await
        .expect("prefix Z dispatches source-file binding");

    let expected_client = crate::handler::attached_client_name(requester_pid);
    wait_for_global_option_value(
        &handler,
        "@source-client-context",
        &format!("{expected_client}:alpha:0"),
    )
    .await;
    handler
        .wait_for_buffer("source-client-if-shell", "yes")
        .await;
    wait_for_file_contents(&run_shell_path, &expected_client).await;
    wait_for_file_contents(&new_window_path, &expected_client).await;
    wait_for_file_contents(&split_window_path, &expected_client).await;
}

#[tokio::test]
async fn attached_binding_two_clients_get_distinct_client_names() {
    let handler = RequestHandler::new();
    let first_pid = u32::MAX - 77;
    let second_pid = u32::MAX - 78;
    let alpha = session_name("alpha");
    let _first_rx = create_attached_session(&handler, first_pid, &alpha).await;
    let _second_rx = handler.attach_client(second_pid, &alpha).await;

    let root = std::env::temp_dir().join(format!(
        "rmux-attached-two-client-names-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("two-client name temp root");
    let shell_command = client_name_file_shell_command(&root.join("#{client_name}.txt"));

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-two-client-names".to_owned()),
            ..Fixture::fixture(("prefix", "X", ["run-shell", "-b", &shell_command]))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(first_pid, b"\x02X")
        .await
        .expect("first client dispatches two-client binding");
    handler
        .handle_attached_live_input_for_test(second_pid, b"\x02X")
        .await
        .expect("second client dispatches two-client binding");

    let first_name = crate::handler::attached_client_name(first_pid);
    let second_name = crate::handler::attached_client_name(second_pid);
    assert_ne!(first_name, second_name);
    wait_for_file_contents(&root.join(format!("{first_name}.txt")), &first_name).await;
    wait_for_file_contents(&root.join(format!("{second_name}.txt")), &second_name).await;
}

#[tokio::test]
async fn attached_binding_if_shell_condition_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 72;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let buffer_name = "attached-client-name-if-shell";

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name-if-shell".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "U",
                [
                    "if-shell",
                    "-F",
                    "#{client_name}",
                    &format!("set-buffer -b {buffer_name} yes"),
                    &format!("set-buffer -b {buffer_name} no"),
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02U")
        .await
        .expect("prefix U dispatches if-shell binding");

    handler.wait_for_buffer(buffer_name, "yes").await;
}

#[tokio::test]
async fn attached_binding_if_shell_branch_expands_client_name() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 73;
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("attached-client-name-if-shell-branch".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "V",
                [
                    "if-shell",
                    "-F",
                    "1",
                    "set-option -g -F @if-shell-branch-client '#{client_name}'",
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02V")
        .await
        .expect("prefix V dispatches if-shell binding");

    wait_for_global_option_value(
        &handler,
        "@if-shell-branch-client",
        &crate::handler::attached_client_name(requester_pid),
    )
    .await;
}

#[tokio::test]
async fn attached_single_switch_queue_completes_after_session_transition() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 78;
    let alpha = session_name("single-switch-alpha");
    let beta = session_name("single-switch-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;
    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let commands = handler
        .parse_control_commands(&format!("switch-client -t {beta}"))
        .await
        .expect("single switch-client queue parses");

    crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha,
        identity.session_id(),
        handler.execute_parsed_commands_for_test(requester_pid, commands),
    )
    .await
    .expect("single switch-client queue must not fail after its final item");

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert_eq!(active.session_name, beta);
}

#[tokio::test]
async fn attached_key_table_only_switch_allows_its_queue_tail() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 82;
    let alpha = session_name("key-table-switch-alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let commands = handler
        .parse_control_commands("switch-client -T root ; new-window -d")
        .await
        .expect("key-table-only switch queue parses");
    crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha.clone(),
        identity.session_id(),
        handler.execute_parsed_commands_for_test(requester_pid, commands),
    )
    .await
    .expect("key-table-only switch allows its queue tail");

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert_eq!(active.session_name, alpha);
    assert_eq!(active.key_table_name.as_deref(), Some("root"));
    drop(active_attach);
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .expect("attached session survives")
            .windows()
            .len(),
        2,
        "the suffix must continue after a key-table-only switch response"
    );
}

#[tokio::test]
async fn attached_read_only_toggle_switch_allows_a_read_only_queue_tail() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 85;
    let alpha = session_name("read-only-switch-alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let commands = handler
        .parse_control_commands("switch-client -r ; display-message -p queue-tail")
        .await
        .expect("read-only switch queue parses");
    crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha,
        identity.session_id(),
        handler.execute_parsed_commands_for_test(requester_pid, commands),
    )
    .await
    .expect("read-only switch allows a read-only queue tail");

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert!(
        active
            .flags
            .contains(crate::client_flags::ClientFlags::READONLY),
        "switch-client -r must still toggle the client flag"
    );
}

#[tokio::test]
async fn attached_binding_switch_client_rebases_its_command_queue() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 74;
    let alpha = session_name("binding-switch-alpha");
    let beta = session_name("binding-switch-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("switch-client-queue".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "W",
                [
                    "switch-client",
                    "-t",
                    beta.as_str(),
                    ";",
                    "new-window",
                    "-d",
                    ";",
                    "set-buffer",
                    "-b",
                    "switch-tail",
                    "done",
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02W")
        .await
        .expect("prefix W dispatches switch-client queue");

    handler.wait_for_buffer("switch-tail", "done").await;
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert_eq!(active.session_name, beta);
    drop(active_attach);
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .expect("source session survives")
            .windows()
            .len(),
        1,
        "implicit suffix commands must stop targeting the source session"
    );
    assert_eq!(
        state
            .sessions
            .session(&beta)
            .expect("switched session survives")
            .windows()
            .len(),
        2,
        "implicit suffix commands must use the switched session cursor"
    );
}

#[tokio::test]
async fn attached_switch_rebases_wrappers_but_preserves_suffix_targets() {
    for entry_path in [
        "source-file",
        "if-shell",
        "run-shell",
        "run-shell-suffix-target",
    ] {
        let handler = RequestHandler::new();
        let requester_pid = match entry_path {
            "source-file" => u32::MAX - 90,
            "if-shell" => u32::MAX - 91,
            "run-shell" => u32::MAX - 92,
            "run-shell-suffix-target" => u32::MAX - 88,
            _ => unreachable!("enumerated entry path"),
        };
        let alpha = session_name(&format!("explicit-{entry_path}-alpha"));
        let beta = session_name(&format!("explicit-{entry_path}-beta"));
        let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
        handler.create_session(&beta).await;

        let suffix_has_explicit_target = entry_path == "run-shell-suffix-target";
        let nested = if suffix_has_explicit_target {
            format!("switch-client -t {beta} ; new-window -d -t {alpha}")
        } else {
            format!("switch-client -t {beta} ; new-window -d")
        };
        let command = match entry_path {
            "source-file" => {
                let source_path = std::env::temp_dir().join(format!(
                    "rmux-explicit-source-target-{}-{requester_pid}.conf",
                    std::process::id()
                ));
                std::fs::write(&source_path, &nested).expect("explicit source-file fixture");
                format!(
                    "source-file -t {alpha}:0.0 {}",
                    command_quote(&source_path.to_string_lossy())
                )
            }
            "if-shell" => format!("if-shell -F -t {alpha}:0.0 1 {{ {nested} }}"),
            "run-shell" | "run-shell-suffix-target" => {
                format!("run-shell -C -t {alpha}:0.0 {}", command_quote(&nested))
            }
            _ => unreachable!("enumerated entry path"),
        };
        let identity = handler.active_attach_identity_for_test(requester_pid).await;
        let commands = handler
            .parse_control_commands(&command)
            .await
            .expect("explicit nested queue parses");

        crate::handler::with_expected_attach_and_session_identity(
            identity,
            alpha.clone(),
            identity.session_id(),
            handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
        .expect("nested queue completes after attached switch");

        let active_attach = handler.active_attach.lock().await;
        let expected_alpha_windows = if suffix_has_explicit_target { 2 } else { 1 };
        let expected_beta_windows = if suffix_has_explicit_target { 1 } else { 2 };
        assert_eq!(
            active_attach
                .by_pid
                .get(&requester_pid)
                .expect("attached client remains registered")
                .session_name,
            beta,
            "{entry_path} switch must still move the attached client"
        );
        drop(active_attach);
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .sessions
                .session(&alpha)
                .expect("explicit target session survives")
                .windows()
                .len(),
            expected_alpha_windows,
            "{entry_path} must honor the suffix command's effective target"
        );
        assert_eq!(
            state
                .sessions
                .session(&beta)
                .expect("switched session survives")
                .windows()
                .len(),
            expected_beta_windows,
            "{entry_path} must distinguish wrapper context from a suffix command target"
        );
    }
}

#[tokio::test]
async fn attached_run_shell_inherited_target_rebases_after_switch() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 89;
    let alpha = session_name("inherited-run-shell-alpha");
    let beta = session_name("inherited-run-shell-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("inherited run-shell target rebase".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "W",
                [
                    "run-shell",
                    "-C",
                    &format!("switch-client -t {beta} ; new-window -d"),
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02W")
        .await
        .expect("inherited run-shell binding dispatches");

    let active_attach = handler.active_attach.lock().await;
    assert_eq!(
        active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client remains registered")
            .session_name,
        beta
    );
    drop(active_attach);
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .expect("source session survives")
            .windows()
            .len(),
        1,
        "an inherited run-shell target must not pin the queue tail to the source session"
    );
    assert_eq!(
        state
            .sessions
            .session(&beta)
            .expect("switched session survives")
            .windows()
            .len(),
        2,
        "the run-shell queue tail must follow the switched attached cursor"
    );
}

#[tokio::test]
async fn attached_attach_session_rebases_every_queue_entry_path() {
    for entry_path in ["direct", "source-file", "if-shell"] {
        let handler = RequestHandler::new();
        let requester_pid = match entry_path {
            "direct" => u32::MAX - 93,
            "source-file" => u32::MAX - 94,
            "if-shell" => u32::MAX - 95,
            _ => unreachable!("enumerated entry path"),
        };
        let alpha = session_name(&format!("attach-{entry_path}-alpha"));
        let beta = session_name(&format!("attach-{entry_path}-beta"));
        let buffer_name = format!("attach-{entry_path}-tail");
        let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
        handler.create_session(&beta).await;

        let nested = format!("attach-session -t {beta} ; set-buffer -b {buffer_name} done");
        let command = match entry_path {
            "direct" => nested,
            "source-file" => {
                let source_path = std::env::temp_dir().join(format!(
                    "rmux-attached-attach-session-{}-{requester_pid}.conf",
                    std::process::id()
                ));
                std::fs::write(&source_path, &nested).expect("attach-session source fixture");
                format!(
                    "source-file {}",
                    command_quote(&source_path.to_string_lossy())
                )
            }
            "if-shell" => format!("if-shell -F 1 {{ {nested} }}"),
            _ => unreachable!("enumerated entry path"),
        };
        let identity = handler.active_attach_identity_for_test(requester_pid).await;
        let commands = handler
            .parse_control_commands(&command)
            .await
            .expect("attach-session queue parses");

        crate::handler::with_expected_attach_and_session_identity(
            identity,
            alpha,
            identity.session_id(),
            handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
        .expect("attach-session queue must continue after its session transition");

        handler.wait_for_buffer(&buffer_name, "done").await;
        let active_attach = handler.active_attach.lock().await;
        assert_eq!(
            active_attach
                .by_pid
                .get(&requester_pid)
                .expect("attached client remains registered")
                .session_name,
            beta,
            "{entry_path} attach-session must move the attached client"
        );
    }
}

#[tokio::test]
async fn attached_switch_response_race_fails_closed_before_queue_continuation() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 79;
    let alpha = session_name("switch-race-alpha");
    let beta = session_name("switch-race-beta");
    let gamma = session_name("switch-race-gamma");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    // A live pane keeps refreshing its attached clients, and a refresh drops any client whose
    // control channel has stopped being served — which is what a real client would look like if
    // it had gone away. This test never reads the channel, so it has to be drained for it, or
    // the very attach the race is about is pruned before the assertions run.
    let _control_drain = tokio::spawn(async move { while control_rx.recv().await.is_some() {} });
    for session in [&beta, &gamma] {
        handler.create_session(session).await;
    }

    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let pause = handler.install_attached_queue_switch_response_pause(identity);
    let commands = handler
        .parse_control_commands(&format!(
            "switch-client -c {requester_pid} -t {beta} ; new-window -d"
        ))
        .await
        .expect("racing switch-client queue parses");
    let queue_handler = handler.clone();
    let queue_alpha = alpha.clone();
    let queue = tokio::spawn(async move {
        crate::handler::with_expected_attach_and_session_identity(
            identity,
            queue_alpha,
            identity.session_id(),
            queue_handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
    });

    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, pause.reached.notified())
        .await
        .expect("first switch reaches response correlation pause");
    handler
        .handle_ok(SwitchClientRequest {
            target: gamma.clone(),
        })
        .await;
    pause.release.notify_one();

    let error = queue
        .await
        .expect("racing queue task joins")
        .expect_err("stale beta response must fail closed");
    assert!(
        matches!(
            error,
            RmuxError::Server(ref message)
                if message.contains("switch-client response no longer matches")
        ),
        "unexpected fail-close error: {error:?}"
    );
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert_eq!(active.session_name, gamma);
    drop(active_attach);
    let state = handler.state.lock().await;
    for session in [&alpha, &beta, &gamma] {
        assert_eq!(
            state
                .sessions
                .session(session)
                .expect("race session survives")
                .windows()
                .len(),
            1,
            "the queue suffix must not run after a stale switch response"
        );
    }
}

#[tokio::test]
async fn attached_same_session_switch_race_uses_the_committed_pane_target() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 83;
    let alpha = session_name("same-session-switch-race");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let control_backlog = {
        let active_attach = handler.active_attach.lock().await;
        active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client exists")
            .control_backlog
            .clone()
    };
    // Service the fixture's attach transport while switch-response correlation is paused.
    let control_drain = tokio::spawn(async move {
        while let Some(control) = control_rx.recv().await {
            crate::pane_io::release_attach_control_backlog(
                &control_backlog,
                control.received_backlog_units(),
            );
        }
    });
    handler
        .handle_ok(SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            ..Fixture::fixture(&alpha)
        })
        .await;
    let (pane_zero_id, pane_one_id) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .expect("race session exists")
            .window_at(0)
            .expect("race window exists");
        (
            window.pane(0).expect("pane zero exists").id(),
            window.pane(1).expect("pane one exists").id(),
        )
    };

    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let pause = handler.install_attached_queue_switch_response_pause(identity);
    let commands = handler
        .parse_control_commands(&format!(
            "switch-client -c {requester_pid} -t {alpha}:0.0 ; kill-pane"
        ))
        .await
        .expect("same-session racing queue parses");
    let queue_handler = handler.clone();
    let queue_alpha = alpha.clone();
    let queue = tokio::spawn(async move {
        crate::handler::with_expected_attach_and_session_identity(
            identity,
            queue_alpha,
            identity.session_id(),
            queue_handler.execute_parsed_commands_for_test(requester_pid, commands),
        )
        .await
    });

    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, pause.reached.notified())
        .await
        .expect("first pane switch reaches response correlation pause");
    let raced = handler
        .handle(Request::SwitchClientExt3(Box::new(
            rmux_proto::request::SwitchClientExt3Request {
                target_client: Some(requester_pid.to_string()),
                target: Some(format!("{alpha}:0.1")),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                sort_order: None,
                skip_environment_update: false,
                zoom: false,
            },
        )))
        .await;
    assert!(matches!(raced, Response::SwitchClient(_)), "{raced:?}");
    pause.release.notify_one();
    queue
        .await
        .expect("same-session racing queue task joins")
        .expect("same-session selection change does not invalidate the committed target");

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&alpha)
        .expect("race session survives")
        .window_at(0)
        .expect("race window survives");
    assert!(
        window.panes().iter().all(|pane| pane.id() != pane_zero_id),
        "the queue tail must act on the pane committed by its own switch"
    );
    assert_eq!(
        window
            .panes()
            .iter()
            .map(|pane| pane.id())
            .collect::<Vec<_>>(),
        vec![pane_one_id],
        "a later same-session switch must not redirect the earlier queue tail"
    );
    control_drain.abort();
}

#[tokio::test]
async fn attached_switch_rebase_rejects_a_stale_committed_pane_identity() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 84;
    let alpha = session_name("stale-committed-pane");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let (window_id, pane_id) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .expect("stale-target session exists")
            .window_at(0)
            .expect("stale-target window exists");
        (
            window.id(),
            window.pane(0).expect("stale-target pane exists").id(),
        )
    };
    let stale = crate::handler::attach_support::AttachedSwitchCommittedTarget {
        target: PaneTarget::new(alpha.clone(), 0),
        session_id: identity.session_id(),
        window_id,
        pane_id: rmux_proto::PaneId::new(pane_id.as_u32().saturating_add(1)),
    };
    let error = crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha.clone(),
        identity.session_id(),
        crate::handler::rebase_expected_attach_session_after_switch(
            &handler,
            requester_pid,
            crate::handler::client_support::SwitchManagedClientIdentity::Attach {
                pid: requester_pid,
                attach_id: identity.attach_id(),
            },
            &alpha,
            Some(stale),
        ),
    )
    .await
    .expect_err("a reused pane slot with a different stable identity must fail closed");
    assert!(
        matches!(
            error,
            RmuxError::Server(ref message)
                if message.contains("switch-client response no longer matches")
        ),
        "unexpected stale-target error: {error:?}"
    );
}

#[tokio::test]
async fn attached_switch_for_other_client_preserves_requester_queue_cursor() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 80;
    let other_pid = u32::MAX - 81;
    let alpha = session_name("other-switch-alpha");
    let beta = session_name("other-switch-beta");
    let gamma = session_name("other-switch-gamma");
    let delta = session_name("other-switch-delta");
    let _requester_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    for session in [&beta, &gamma, &delta] {
        handler.create_session(session).await;
    }
    let (other_tx, _other_rx) = mpsc::unbounded_channel();
    let other_attach_id = handler
        .register_attach(other_pid, gamma.clone(), other_tx)
        .await;

    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let commands = handler
        .parse_control_commands(&format!(
            "switch-client -c {other_pid} -t {delta} ; new-window -d"
        ))
        .await
        .expect("other-client switch queue parses");
    let context = crate::handler::scripting_support::QueueExecutionContext::without_caller_cwd()
        .with_implicit_current_target(Some(rmux_proto::Target::Pane(PaneTarget::new(
            beta.clone(),
            0,
        ))));
    crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha.clone(),
        identity.session_id(),
        handler.execute_parsed_commands(requester_pid, commands, context),
    )
    .await
    .expect("switching another client preserves the requester queue");

    let active_attach = handler.active_attach.lock().await;
    assert_eq!(
        active_attach
            .by_pid
            .get(&requester_pid)
            .expect("requester remains attached")
            .session_name,
        alpha
    );
    assert_eq!(
        active_attach
            .by_pid
            .get(&other_pid)
            .expect("other client remains attached")
            .session_name,
        delta
    );
    drop(active_attach);
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .expect("requester session survives")
            .windows()
            .len(),
        1,
        "-c other-client must not rebase the requester onto its attached session"
    );
    assert_eq!(
        state
            .sessions
            .session(&beta)
            .expect("captured queue target survives")
            .windows()
            .len(),
        2,
        "the suffix must retain the requester's pre-switch queue target"
    );
    assert_eq!(
        state
            .sessions
            .session(&delta)
            .expect("other client target survives")
            .windows()
            .len(),
        1,
        "the other client's switch target must not become the requester queue target"
    );
    drop(state);

    let reswitched = handler
        .handle(Request::SwitchClientExt3(Box::new(
            rmux_proto::request::SwitchClientExt3Request {
                target_client: Some(other_pid.to_string()),
                target: Some(gamma.to_string()),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                sort_order: None,
                skip_environment_update: false,
                zoom: false,
            },
        )))
        .await;
    assert!(matches!(reswitched, Response::SwitchClient(_)));
    let stale_other_response = crate::handler::with_expected_attach_and_session_identity(
        identity,
        alpha,
        identity.session_id(),
        crate::handler::rebase_expected_attach_session_after_switch(
            &handler,
            requester_pid,
            crate::handler::client_support::SwitchManagedClientIdentity::Attach {
                pid: other_pid,
                attach_id: other_attach_id,
            },
            &delta,
            None,
        ),
    )
    .await
    .expect("another client's later switch must not fail the requester queue");
    assert!(
        stale_other_response.is_none(),
        "another client must never rebase the requester queue"
    );
}

#[tokio::test]
async fn attached_binding_allows_an_explicit_cross_session_target() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 75;
    let alpha = session_name("binding-cross-alpha");
    let beta = session_name("binding-cross-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("explicit-cross-session-target".to_owned()),
            ..Fixture::fixture((
                "prefix",
                "Y",
                [
                    "kill-session",
                    "-t",
                    beta.as_str(),
                    ";",
                    "set-buffer",
                    "-b",
                    "cross-tail",
                    "done",
                ],
            ))
        })
        .await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02Y")
        .await
        .expect("prefix Y dispatches cross-session queue");

    handler.wait_for_buffer("cross-tail", "done").await;
    let state = handler.state.lock().await;
    assert!(state.sessions.contains_session(&alpha));
    assert!(!state.sessions.contains_session(&beta));
}

#[tokio::test]
async fn attached_command_prompt_switch_client_rebases_its_continuation() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 76;
    let alpha = session_name("prompt-switch-alpha");
    let beta = session_name("prompt-switch-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;

    let input = format!(
        "\x02:switch-client -t {} ; set-buffer -b prompt-switch-tail done\r",
        beta
    );
    handler
        .handle_attached_live_input_for_test(requester_pid, input.as_bytes())
        .await
        .expect("attached command prompt accepts switch-client queue");

    handler.wait_for_buffer("prompt-switch-tail", "done").await;
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client remains registered");
    assert_eq!(active.session_name, beta);
}

#[tokio::test]
async fn attached_command_prompt_attach_session_rebases_its_continuation() {
    let handler = RequestHandler::new();
    let requester_pid = u32::MAX - 97;
    let alpha = session_name("prompt-attach-alpha");
    let beta = session_name("prompt-attach-beta");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;
    handler.create_session(&beta).await;

    let input = format!(
        "\x02:attach-session -t {} ; set-buffer -b prompt-attach-tail done\r",
        beta
    );
    handler
        .handle_attached_live_input_for_test(requester_pid, input.as_bytes())
        .await
        .expect("attached command prompt accepts attach-session queue");

    handler.wait_for_buffer("prompt-attach-tail", "done").await;
    let active_attach = handler.active_attach.lock().await;
    assert_eq!(
        active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client remains registered")
            .session_name,
        beta
    );
}

#[tokio::test]
async fn attached_command_prompt_renames_current_session() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02:rename-session beta\r")
        .await
        .expect("prefix command prompt input");

    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || {
            let state = handler.state.lock().await;
            if state.sessions.contains_session(&beta) {
                assert!(!state.sessions.contains_session(&alpha));
                Ok(())
            } else {
                Err(())
            }
        },
    )
    .await
    .unwrap_or_else(|()| panic!("timed out waiting for command prompt rename-session"));

    let frame = wait_for_switch_frame_containing(&mut control_rx, "[beta]").await;
    assert!(
        !frame.contains("[alpha]"),
        "renamed session status must not keep old name: {frame:?}"
    );
}

#[tokio::test]
async fn attached_command_prompt_can_create_window_from_same_read() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, PROMPT_NEW_WINDOW_INPUT)
        .await
        .expect("prefix command prompt input");

    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || {
            let windows = active_windows(&handler, &alpha).await;
            if windows == "0:0\n1:1\n" {
                Ok(())
            } else {
                Err(windows)
            }
        },
    )
    .await
    .unwrap_or_else(|windows| {
        panic!("timed out waiting for prompt-created window, got {windows:?}")
    });

    let target = PaneTarget::with_window(alpha.clone(), 1, 0);
    wait_for_capture_containing(
        &handler,
        target,
        "ISSUE8_WINDOW_READY",
        "prompt-created window should publish its first output",
    )
    .await;
    handler.refresh_attached_session(&alpha).await;

    let frame = wait_for_attach_output_containing(&mut control_rx, "ISSUE8_WINDOW_READY").await;
    assert!(
        frame.contains("ISSUE8_WINDOW_READY"),
        "prompt-created window must render its first output, got {frame:?}"
    );
}

#[tokio::test]
async fn attached_exit_notifies_after_command_prompt_rename_session() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02:rename-session beta\r")
        .await
        .expect("prefix command prompt input");

    let _ = wait_for_switch_frame_containing(&mut control_rx, "[beta]").await;
    prepare_attached_shell_prompt(&handler, &PaneTarget::new(beta.clone(), 0)).await;
    drain_attach_controls(&mut control_rx);

    handler
        .handle_attached_live_input_for_test(requester_pid, b"exit\r")
        .await
        .expect("exit input after rename-session");

    recv_matching_attach_control(
        &mut control_rx,
        "attach exit notification after renamed exit",
        |control| matches!(control, AttachControl::Exited),
    )
    .await;
    wait_for_session_removed(&handler, &beta).await;
}

#[tokio::test]
async fn attached_session_status_updates_after_external_rename() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_ok(RenameSessionRequest {
            target: alpha,
            new_name: beta,
        })
        .await;

    let frame = wait_for_switch_frame_containing(&mut control_rx, "[beta]").await;
    assert!(
        !frame.contains("[alpha]"),
        "externally renamed session status must not keep old name: {frame:?}"
    );
}

#[tokio::test]
async fn attached_prefix_confirm_accepts_following_key_in_same_read_after_split() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let _control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02%")
        .await
        .expect("prefix split input");
    wait_for_active_panes(&handler, &alpha, "0:0\n1:1\n").await;

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02xy")
        .await
        .expect("prefix confirm input");
    wait_for_active_panes(&handler, &alpha, "0:1\n").await;
}

#[tokio::test]
async fn attached_kill_last_pane_exits_the_session() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    let killed = handler
        .handle(Request::KillPane(rmux_proto::KillPaneRequest {
            target: PaneTarget::new(alpha.clone(), 0),
            kill_all_except: false,
        }))
        .await;
    assert_eq!(
        killed,
        Response::KillPane(rmux_proto::KillPaneResponse {
            target: PaneTarget::new(alpha.clone(), 0),
            window_destroyed: true,
        })
    );

    recv_matching_attach_control(&mut control_rx, "attach exit notification", |control| {
        matches!(control, AttachControl::Exited)
    })
    .await;
    wait_for_session_removed(&handler, &alpha).await;
}

async fn wait_for_global_option_value(handler: &RequestHandler, name: &str, expected: &str) {
    let expected_stdout = format!("{expected}\n").into_bytes();
    wait_until(
        ATTACH_LIFECYCLE_TIMEOUT,
        Duration::from_millis(25),
        async || {
            let response = handler
                .handle(Request::ShowOptions(rmux_proto::ShowOptionsRequest {
                    scope: rmux_proto::OptionScopeSelector::SessionGlobal,
                    name: Some(name.to_owned()),
                    value_only: true,
                    include_inherited: false,
                    quiet: false,
                    include_hooks: false,
                }))
                .await;
            match response.command_output() {
                Some(output) if output.stdout() == expected_stdout => Ok(()),
                _ => Err(response),
            }
        },
    )
    .await
    .unwrap_or_else(|response| {
        panic!(
            "timed out waiting for option {name:?} to be {expected:?}; last response: {response:?}"
        )
    });
}

async fn wait_for_active_panes(handler: &RequestHandler, session: &SessionName, expected: &str) {
    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || {
            let panes = active_panes(handler, session).await;
            if panes == expected {
                Ok(())
            } else {
                Err(panes)
            }
        },
    )
    .await
    .unwrap_or_else(|panes| {
        panic!("timed out waiting for active panes {expected:?}, got {panes:?}")
    });
}

fn quote_command_arguments(values: &[String]) -> String {
    values
        .iter()
        .map(|value| command_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn client_name_file_shell_command(path: &Path) -> String {
    format!(
        "printf %s \"#{{client_name}}\" > {}",
        crate::test_shell::sh_quote_path(path)
    )
}

fn client_name_file_pane_command(path: &Path) -> Vec<String> {
    vec![client_name_file_shell_command(path)]
}

async fn wait_for_switch_frame_containing(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + ATTACH_LIFECYCLE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let control = match tokio::time::timeout(
            remaining.min(Duration::from_millis(250)),
            control_rx.recv(),
        )
        .await
        {
            Ok(Some(control)) => control,
            Ok(None) => panic!("attach refresh channel closed"),
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out after {:?} waiting for attach frame containing {expected:?}",
                    ATTACH_LIFECYCLE_TIMEOUT
                );
                continue;
            }
        };
        if let AttachControl::Switch(target) = control {
            let frame = String::from_utf8(target.into_target().render_frame)
                .expect("render frame is utf-8");
            if frame.contains(expected) {
                return frame;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {:?} waiting for attach frame containing {expected:?}",
            ATTACH_LIFECYCLE_TIMEOUT
        );
    }
}

async fn wait_for_attach_output_containing(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + ATTACH_LIFECYCLE_TIMEOUT;
    let mut seen = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let control = match tokio::time::timeout(
            remaining.min(Duration::from_millis(250)),
            control_rx.recv(),
        )
        .await
        {
            Ok(Some(control)) => control,
            Ok(None) => panic!("attach refresh channel closed"),
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out after {:?} waiting for attach output containing {expected:?}; saw {seen:?}",
                    ATTACH_LIFECYCLE_TIMEOUT
                );
                continue;
            }
        };
        match control {
            AttachControl::Switch(target) => {
                let target = target.into_target();
                seen.push_str(&String::from_utf8_lossy(&target.render_frame));
            }
            AttachControl::Overlay(frame) => {
                seen.push_str(&String::from_utf8_lossy(&frame.frame));
            }
            AttachControl::Write(bytes) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
            _ => {}
        }
        if seen.contains(expected) {
            return seen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {:?} waiting for attach output containing {expected:?}; saw {seen:?}",
            ATTACH_LIFECYCLE_TIMEOUT
        );
    }
}

#[tokio::test]
async fn attached_resize_resizes_session_and_refreshes_status_frame() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_resize(requester_pid, TerminalSize::new(132, 43))
        .await
        .expect("attached resize succeeds");

    {
        let client_size = {
            let active_attach = handler.active_attach.lock().await;
            active_attach
                .by_pid
                .get(&requester_pid)
                .expect("attached client is tracked")
                .client_size
        };
        let state = handler.state.lock().await;
        let size = state
            .sessions
            .session(&alpha)
            .expect("session exists")
            .window()
            .size();
        assert_eq!(client_size, TerminalSize::new(132, 43));
        assert_eq!(size, TerminalSize::new(132, 42));
    }
    assert_eq!(
        handler
            .pane_terminal_size_for_test(&PaneTarget::with_window(alpha.clone(), 0, 0))
            .await,
        TerminalSize::new(132, 42)
    );
    let frame = recv_render_frame(&mut control_rx, "resize refresh").await;
    assert!(
        frame.contains("[alpha]"),
        "resize should redraw status for the attached client, got {frame:?}"
    );
}

#[tokio::test]
async fn attached_refresh_renders_each_client_at_its_own_size() {
    let handler = RequestHandler::new();
    let local_pid = 101;
    let browser_pid = 202;
    let alpha = session_name("alpha");
    let mut local_rx = create_attached_session(&handler, local_pid, &alpha).await;
    let mut browser_rx = handler.attach_client(browser_pid, &alpha).await;

    handler
        .handle_attached_resize(browser_pid, TerminalSize::new(132, 43))
        .await
        .expect("browser resize succeeds");

    let local_frame = recv_render_frame(&mut local_rx, "local refresh").await;
    let browser_frame = recv_render_frame(&mut browser_rx, "browser refresh").await;
    assert!(
        local_frame.contains("\x1b[24;1H"),
        "local attach must keep a 24-row status line, got {local_frame:?}"
    );
    assert!(
        !local_frame.contains("\x1b[43;1H"),
        "local attach must not receive browser-sized redraws, got {local_frame:?}"
    );
    assert!(
        browser_frame.contains("\x1b[43;1H"),
        "browser attach should render at the browser-requested height, got {browser_frame:?}"
    );

    handler
        .refresh_attached_client_status(local_pid, &alpha)
        .await
        .expect("status refresh succeeds");
    let local_status = match recv_attach_control(&mut local_rx, "local status refresh").await {
        AttachControl::Write(bytes) => String::from_utf8(bytes).expect("status is utf-8"),
        other => panic!("expected status write, got {other:?}"),
    };
    assert!(
        local_status.contains("\x1b[24;1H"),
        "periodic status refresh must keep the local client height, got {local_status:?}"
    );
    assert!(
        !local_status.contains("\x1b[43;1H"),
        "periodic status refresh must not use the browser height, got {local_status:?}"
    );
}

#[tokio::test]
async fn attached_resize_ignores_zero_sized_terminal_reports() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    handler
        .handle_attached_resize(requester_pid, TerminalSize { cols: 0, rows: 0 })
        .await
        .expect("zero-sized resize is ignored");

    let (client_size, session_size) = {
        let active_attach = handler.active_attach.lock().await;
        let client_size = active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client is tracked")
            .client_size;
        drop(active_attach);

        let state = handler.state.lock().await;
        let session_size = state
            .sessions
            .session(&alpha)
            .expect("session exists")
            .window()
            .size();
        (client_size, session_size)
    };

    assert_eq!(client_size, TerminalSize { cols: 80, rows: 24 });
    assert_eq!(session_size, TerminalSize { cols: 80, rows: 24 });
    assert!(
        control_rx.try_recv().is_err(),
        "ignored zero-sized resize must not emit a refresh frame"
    );
}
