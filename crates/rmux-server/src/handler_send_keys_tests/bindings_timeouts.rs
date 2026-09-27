use super::*;

#[tokio::test]
async fn send_prefix_reports_the_configured_prefix_key() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;

    let response = handler
        .handle(Request::SendPrefix(SendPrefixRequest {
            target: Some(PaneTarget::new(alpha, 0)),
            secondary: false,
        }))
        .await;
    assert!(matches!(
        response,
        Response::SendPrefix(ref success) if success.key == "C-b"
    ));
}

#[tokio::test]
async fn bind_key_without_a_command_requires_an_existing_binding() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::BindKey(Box::new(BindKeyRequest {
            note: Some("missing".to_owned()),
            repeat: true,
            command: None,
            ..Fixture::fixture(("root", "User1000", std::iter::empty::<&str>()))
        })))
        .await;

    assert!(matches!(response, Response::Error(_)));
}

#[tokio::test]
async fn bind_key_without_a_command_updates_note_and_repeat_in_place() {
    let handler = RequestHandler::new();

    handler
        .handle_ok(BindKeyRequest {
            note: Some("updated note".to_owned()),
            repeat: true,
            command: None,
            ..Fixture::fixture(("prefix", "C-b", std::iter::empty::<&str>()))
        })
        .await;

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            format: Some("#{key_note}|#{key_repeat}|#{key_command}".to_owned()),
            ..list_keys_request(Some("prefix"))
        })))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };
    let stdout = String::from_utf8(response.command_output().stdout().to_vec()).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line == "updated note|1|send-prefix"),
        "{stdout:?}"
    );
}

#[tokio::test]
async fn list_keys_notes_render_effective_prefix_column() {
    let handler = RequestHandler::new();

    handler
        .set_option(ScopeSelector::Global, OptionName::Prefix, "C-a")
        .await;

    for request in [
        BindKeyRequest {
            note: Some("note text".to_owned()),
            ..Fixture::fixture(("prefix", "X", ["display-message", "hi"]))
        },
        BindKeyRequest {
            note: Some("root note".to_owned()),
            ..Fixture::fixture(("root", "F12", ["display-message", "root"]))
        },
    ] {
        handler.handle_ok(request).await;
    }

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            notes: true,
            include_unnoted: false,
            ..list_keys_request(None)
        })))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };
    let stdout = String::from_utf8(response.command_output().stdout().to_vec()).unwrap();
    assert!(stdout.contains("C-a X       note text\n"), "{stdout:?}");
    assert!(stdout.contains("    F12     root note\n"), "{stdout:?}");

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            notes: true,
            include_unnoted: false,
            prefix: Some("PFX".to_owned()),
            ..list_keys_request(None)
        })))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };
    let stdout = String::from_utf8(response.command_output().stdout().to_vec()).unwrap();
    assert!(stdout.contains("PFXX       note text\n"), "{stdout:?}");
    assert!(stdout.contains("   F12     root note\n"), "{stdout:?}");
}

#[tokio::test]
async fn list_keys_single_key_filter_is_silent_for_explicit_table() {
    let handler = RequestHandler::new();

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            key: Some("C-b".to_owned()),
            ..list_keys_request(Some("prefix"))
        })))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };

    let stdout = String::from_utf8(response.command_output().stdout().to_vec()).unwrap();
    assert_eq!(stdout, "");
    assert_eq!(response.match_count, 0);
}

#[tokio::test]
async fn list_keys_single_key_filter_matches_valid_key_across_tables() {
    let handler = RequestHandler::new();

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            key: Some("C-b".to_owned()),
            ..list_keys_request(None)
        })))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };

    let stdout = String::from_utf8(response.command_output().stdout().to_vec()).unwrap();
    assert!(
        stdout.contains("-T copy-mode    C-b send-keys -X cursor-left"),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("-T copy-mode-vi C-b send-keys -X page-up"),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("-T prefix       C-b send-prefix"),
        "{stdout:?}"
    );
    assert_eq!(response.match_count, 3);
}

#[tokio::test]
async fn list_keys_single_key_filter_errors_when_key_syntax_is_invalid() {
    let handler = RequestHandler::new();

    let listed = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            key: Some("NotAKey".to_owned()),
            ..list_keys_request(Some("prefix"))
        })))
        .await;

    assert_eq!(
        listed,
        Response::Error(ErrorResponse {
            error: RmuxError::Server("invalid key: NotAKey".to_owned())
        })
    );
}

#[tokio::test]
async fn list_keys_rejects_unknown_sort_orders() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::ListKeys(Box::new(ListKeysRequest {
            sort_order: Some("bogus".to_owned()),
            ..list_keys_request(None)
        })))
        .await;

    assert!(matches!(response, Response::Error(_)));
}

#[tokio::test]
async fn repeating_non_repeat_lookup_restarts_in_the_default_table() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = std::process::id();

    handler.create_session(&alpha).await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;

    for request in [
        BindKeyRequest {
            note: Some("root".to_owned()),
            ..Fixture::fixture(("root", "x", ["set-buffer", "-b", "dispatch-source", "root"]))
        },
        BindKeyRequest {
            note: Some("repeat".to_owned()),
            repeat: true,
            ..Fixture::fixture(("my-table", "r", ["set-buffer", "-b", "repeat-hit", "yes"]))
        },
        BindKeyRequest {
            note: Some("custom".to_owned()),
            ..Fixture::fixture((
                "my-table",
                "x",
                ["set-buffer", "-b", "dispatch-source", "custom"],
            ))
        },
    ] {
        handler.handle_ok(request).await;
    }

    let switched = handler
        .handle(Request::SwitchClientExt(SwitchClientExtRequest {
            target: None,
            key_table: Some("my-table".to_owned()),
        }))
        .await;
    assert!(matches!(switched, Response::SwitchClient(_)));

    let dispatched = handler
        .handle(Request::SendKeysExt(SendKeysExtRequest::fixture((
            PaneTarget::new(alpha, 0),
            ["r", "x"],
        ))))
        .await;
    assert_eq!(
        dispatched,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );

    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("dispatch-source".to_owned()),
        }))
        .await;
    let Response::ShowBuffer(response) = shown else {
        panic!("expected show-buffer response");
    };
    assert_eq!(response.command_output().stdout(), b"root");
}

#[tokio::test]
async fn prefix_timeout_clears_the_prefix_table_without_waiting_for_the_next_key() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = std::process::id();

    handler.create_session(&alpha).await;

    handler
        .set_option(ScopeSelector::Global, OptionName::PrefixTimeout, "25")
        .await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;

    let dispatched = handler
        .handle(Request::SendKeysExt(SendKeysExtRequest::fixture((
            PaneTarget::new(alpha, 0),
            ["C-b"],
        ))))
        .await;
    assert_eq!(
        dispatched,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    );

    assert_eq!(
        client_key_table(&handler, requester_pid).await.as_deref(),
        Some("prefix")
    );

    sleep(Duration::from_millis(100)).await;

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client should remain registered");
    assert_eq!(active.key_table_name, None);
    assert_eq!(active.key_table_set_at, None);
    assert!(!active.repeat_active);
    assert_eq!(active.repeat_deadline, None);
}

#[tokio::test]
async fn repeat_timeout_clears_custom_key_tables_without_waiting_for_the_next_key() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = std::process::id();

    handler.create_session(&alpha).await;

    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::RepeatTime,
            "25",
        )
        .await;

    let _control_rx = handler.attach_client(requester_pid, &alpha).await;

    handler
        .handle_ok(BindKeyRequest {
            note: Some("repeat".to_owned()),
            repeat: true,
            ..Fixture::fixture(("my-table", "r", ["set-buffer", "-b", "repeat-hit", "yes"]))
        })
        .await;

    let switched = handler
        .handle(Request::SwitchClientExt(SwitchClientExtRequest {
            target: None,
            key_table: Some("my-table".to_owned()),
        }))
        .await;
    assert!(matches!(switched, Response::SwitchClient(_)));

    let dispatched = handler
        .handle(Request::SendKeysExt(SendKeysExtRequest::fixture((
            PaneTarget::new(alpha, 0),
            ["r"],
        ))))
        .await;
    assert_eq!(
        dispatched,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    );

    {
        let active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client should remain registered");
        assert_eq!(active.key_table_name.as_deref(), Some("my-table"));
        assert!(active.repeat_active);
        assert!(active.repeat_deadline.is_some());
    }

    sleep(Duration::from_millis(100)).await;

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&requester_pid)
        .expect("attached client should remain registered");
    assert_eq!(active.key_table_name, None);
    assert_eq!(active.key_table_set_at, None);
    assert!(!active.repeat_active);
    assert_eq!(active.repeat_deadline, None);
}

#[tokio::test]
async fn unbind_key_all_removes_active_bindings_without_dropping_default_tables() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::UnbindKey(UnbindKeyRequest {
            table_name: "prefix".to_owned(),
            all: true,
            key: None,
            quiet: false,
        }))
        .await;
    assert!(matches!(
        response,
        Response::UnbindKey(ref success) if success.removed && success.all
    ));

    let listed = handler
        .handle(Request::ListKeys(Box::new(list_keys_request(Some(
            "prefix",
        )))))
        .await;
    let Response::ListKeys(response) = listed else {
        panic!("expected list-keys response");
    };
    assert_eq!(response.match_count, 0);

    handler
        .handle_ok(BindKeyRequest {
            note: Some("user".to_owned()),
            ..Fixture::fixture(("prefix", "User1000", ["send-prefix"]))
        })
        .await;
}
