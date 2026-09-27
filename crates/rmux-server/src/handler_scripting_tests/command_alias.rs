use super::*;
use rmux_proto::{BindKeyRequest, ListKeysRequest};

async fn set_command_alias(handler: &RequestHandler, alias: &str) {
    handler
        .set_option(ScopeSelector::Global, OptionName::CommandAlias, alias)
        .await;
}

#[tokio::test]
async fn runtime_command_alias_option_drives_command_string_parser() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "say=display-message -p --").await;

    let parsed = handler
        .parse_command_string_one_group("say hello")
        .await
        .expect("runtime alias should parse");
    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("runtime alias should execute");

    assert_eq!(output.stdout(), b"hello\n");
}

#[tokio::test]
async fn runtime_command_alias_preserves_option_like_positional_values() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "literal=set-option -g @alias-value").await;

    let parsed = handler
        .parse_command_string_one_group("literal -tfoo ; show-options -gqv @alias-value")
        .await
        .expect("runtime alias should preserve its option-like appended argument");
    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("runtime alias should execute");

    assert_eq!(output.stdout(), b"-tfoo\n");
}

#[tokio::test]
async fn runtime_command_alias_option_drives_source_file_parser() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "sbuf=set-buffer -b aliased").await;

    let root = temp_root("command-alias");
    let config = root.join("main.conf");
    write_config(&config, "sbuf from-source\n");
    assert_eq!(
        handler
            .handle(source_file_request(
                vec!["main.conf".to_owned()],
                Some(root)
            ))
            .await,
        Response::SourceFile(rmux_proto::SourceFileResponse::no_output())
    );

    assert_eq!(
        handler
            .handle(show_buffer_request("aliased"))
            .await
            .command_output()
            .expect("aliased buffer output")
            .stdout(),
        b"from-source"
    );
}

#[tokio::test]
async fn internal_canonical_execution_does_not_expand_aliases_again() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "if-shell=display-message -p second").await;

    let response = handler
        .handle(
            SourceFileRequest {
                stdin: Some("if-shell -F 1 \"display-message -p first\"".to_owned()),
                ..Fixture::fixture([INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH])
            }
            .into_request(),
        )
        .await;

    assert_eq!(
        response
            .command_output()
            .expect("canonical queue output")
            .stdout(),
        b"first\n"
    );
}

#[tokio::test]
async fn internal_canonical_execution_accepts_explicit_target_context() {
    let handler = RequestHandler::new();
    let alpha = session_name("canonical-target");
    handler.create_session(&alpha).await;

    let response = handler
        .handle(
            SourceFileRequest {
                target: Some(PaneTarget::with_window(alpha, 0, 0)),
                stdin: Some("display-message -p '#{session_name}:#{window_index}'".to_owned()),
                ..Fixture::fixture([INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH])
            }
            .into_request(),
        )
        .await;

    assert_eq!(
        response
            .command_output()
            .expect("canonical target output")
            .stdout(),
        b"canonical-target:0\n"
    );
}

#[tokio::test]
async fn internal_canonical_execution_keeps_deferred_branch_aliases_dynamic() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "inner=display-message -p nested").await;

    let response = handler
        .handle(
            SourceFileRequest {
                stdin: Some("if-shell -F 1 inner".to_owned()),
                ..Fixture::fixture([INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH])
            }
            .into_request(),
        )
        .await;

    assert_eq!(
        response
            .command_output()
            .expect("deferred branch output")
            .stdout(),
        b"nested\n"
    );
}

#[tokio::test]
async fn internal_canonical_execution_rejects_malformed_shapes() {
    let handler = RequestHandler::new();
    let response = handler
        .handle(
            SourceFileRequest {
                stdin: Some("display-message -p no".to_owned()),
                ..Fixture::fixture([INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH, "-"])
            }
            .into_request(),
        )
        .await;

    let Response::Error(error) = response else {
        panic!("malformed internal canonical request should fail: {response:?}");
    };
    assert!(error
        .error
        .to_string()
        .contains("invalid internal source-file request path"));
}

#[tokio::test]
async fn runtime_command_alias_option_drives_hook_registration_parser() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "sbuf=set-buffer -b aliased").await;

    handler
        .set_global_hook(HookName::AfterSetBuffer, "sbuf from-hook")
        .await;
    handler
        .handle_ok(SetBufferRequest::fixture(("origin", b"origin")))
        .await;

    let output = handler.handle(show_buffer_request("aliased")).await;
    assert_eq!(
        output
            .command_output()
            .expect("hook-created aliased buffer output")
            .stdout(),
        b"from-hook"
    );

    let root = temp_root("command-alias-hook");
    let config = root.join("hook.conf");
    write_config(
        &config,
        "set-hook -g after-set-buffer 'sbuf from-source-hook'\n",
    );
    assert_eq!(
        handler
            .handle(source_file_request(
                vec!["hook.conf".to_owned()],
                Some(root)
            ))
            .await,
        Response::SourceFile(rmux_proto::SourceFileResponse::no_output())
    );
    handler
        .handle_ok(SetBufferRequest::fixture(("source-origin", b"origin")))
        .await;
    let output = handler.handle(show_buffer_request("aliased")).await;
    assert_eq!(
        output
            .command_output()
            .expect("source-hook-created aliased buffer output")
            .stdout(),
        b"from-source-hook"
    );
}

async fn listed_root_bindings(handler: &RequestHandler) -> String {
    let response = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            table_name: Some("root".to_owned()),
            first_only: false,
            notes: false,
            include_unnoted: true,
            reversed: false,
            format: None,
            sort_order: None,
            prefix: None,
            key: None,
        })))
        .await;
    let Response::ListKeys(response) = response else {
        panic!("expected list-keys success, got {response:?}");
    };
    String::from_utf8(response.command_output().stdout().to_vec()).expect("list-keys utf8")
}

#[tokio::test]
async fn runtime_command_alias_option_drives_binding_payload_parsers() {
    let handler = RequestHandler::new();
    set_command_alias(&handler, "sbuf=set-buffer -b aliased").await;

    for (key, command) in [
        (
            "F10",
            vec!["sbuf".to_owned(), "from-protocol-argv".to_owned()],
        ),
        ("F11", vec!["sbuf from-protocol-string".to_owned()]),
    ] {
        handler
            .handle_ok(BindKeyRequest::fixture(("root", key, command)))
            .await;
    }

    let parsed = handler
        .parse_command_string_one_group("bind-key -T root F12 sbuf from-queue")
        .await
        .expect("queued bind-key parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("queued bind-key executes");

    let root = temp_root("command-alias-bind-key");
    let config = root.join("main.conf");
    write_config(&config, "bind-key -T root F9 sbuf from-source-binding\n");
    assert_eq!(
        handler
            .handle(source_file_request(
                vec!["main.conf".to_owned()],
                Some(root)
            ))
            .await,
        Response::SourceFile(rmux_proto::SourceFileResponse::no_output())
    );

    let bindings = listed_root_bindings(&handler).await;
    for expected in [
        "set-buffer -b aliased from-protocol-argv",
        "set-buffer -b aliased from-protocol-string",
        "set-buffer -b aliased from-queue",
        "set-buffer -b aliased from-source-binding",
    ] {
        assert!(
            bindings.contains(expected),
            "missing canonical binding {expected:?} in {bindings:?}"
        );
    }
}
