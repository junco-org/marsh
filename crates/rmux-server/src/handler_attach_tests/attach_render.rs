use super::*;

#[tokio::test]
async fn session_target_refreshes_follow_the_current_active_window() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");

    handler
        .create_session((&alpha, TerminalSize::new(120, 40)))
        .await;

    {
        let mut state = handler.state.lock().await;
        let pane_id = state.sessions.allocate_pane_id();
        {
            let session = state
                .sessions
                .session_mut(&alpha)
                .expect("session should exist");
            session
                .insert_window_with_initial_pane_with_id(
                    5,
                    TerminalSize { cols: 90, rows: 30 },
                    pane_id,
                )
                .expect("window 5 insert succeeds");
            session
                .select_window(5)
                .expect("window 5 selection succeeds");
        }
        state
            .insert_window_terminal(
                &alpha,
                5,
                crate::pane_terminals::WindowSpawnOptions {
                    start_directory: None,
                    command: None,
                    socket_path: Path::new("/tmp/rmux-test.sock"),
                    spawn_environment: None,
                    environment_overrides: None,
                    respawn_shell: None,
                    respawn_environment: None,
                    shell_id: None,
                    follow_mux_lifetime: false,
                },
            )
            .await
            .expect("window 5 terminal insert succeeds");
    }

    let mut control_rx = handler.attach_client(requester_pid, &alpha).await;

    let split = handler
        .handle_ok(SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            ..Fixture::fixture(&alpha)
        })
        .await;
    assert_eq!(
        split,
        rmux_proto::SplitWindowResponse {
            pane: PaneTarget::with_window(alpha, 5, 1),
        }
    );
    let split_frame = recv_render_frame(&mut control_rx, "split refresh").await;
    assert!(split_frame.contains('│'));
}

#[tokio::test]
async fn attach_session_upgrade_renders_only_the_active_window() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler
        .create_session((&alpha, TerminalSize::new(120, 40)))
        .await;
    handler.handle_ok(SplitWindowRequest::fixture(&alpha)).await;

    let ready_marker = format!("RMUX_ATTACH_ACTIVE_READY_{}", std::process::id());
    let quiet_command = rmux_proto::ProcessCommand::Argv(quiet_ready_command(&ready_marker));
    let mut state = handler.state.lock().await;
    let pane_id = state.sessions.allocate_pane_id();
    let session = state.sessions.session_mut(&alpha).expect("session exists");
    session
        .insert_window_with_initial_pane_with_id(5, TerminalSize { cols: 90, rows: 30 }, pane_id)
        .expect("window 5 insert succeeds");
    session.select_window(5).expect("window 5 select succeeds");
    state
        .insert_window_terminal(
            &alpha,
            5,
            crate::pane_terminals::WindowSpawnOptions {
                start_directory: None,
                command: Some(&quiet_command),
                socket_path: Path::new("/tmp/rmux-test.sock"),
                spawn_environment: None,
                environment_overrides: None,
                respawn_shell: None,
                respawn_environment: None,
                shell_id: None,
                follow_mux_lifetime: false,
            },
        )
        .await
        .expect("window 5 terminal insert succeeds");
    drop(state);

    wait_for_capture_containing(
        &handler,
        PaneTarget::with_window(alpha.clone(), 5, 0),
        &ready_marker,
        "active window fixture should settle before transcript replacement",
    )
    .await;
    handler
        .replace_transcript_for_test(
            &PaneTarget::with_window(alpha.clone(), 5, 0),
            TerminalSize { cols: 90, rows: 30 },
            b"\x1b]0;pane-host\x07visible-active-pane\r\n",
        )
        .await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSession(rmux_proto::AttachSessionRequest { target: alpha }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let render_frame =
        String::from_utf8(outcome.attach.expect("attach upgrade").target.render_frame)
            .expect("render frame must be utf-8");
    assert!(render_frame.contains("[alpha]"));
    assert!(
        render_frame.contains("visible-active-pane"),
        "attach must replay the active pane screen, got {render_frame:?}"
    );
    {
        assert!(
            render_frame.contains("\"pane-host\""),
            "attach status must render the pane title in status-right, got {render_frame:?}"
        );
    }
    assert!(!render_frame.contains('┬'));
    assert!(!render_frame.contains('┴'));
    assert!(!render_frame.contains('│'));
}

#[tokio::test]
async fn attach_session_render_frame_positions_cursor_at_active_pane_cursor() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(Quiet(&alpha)).await;

    handler
        .replace_transcript_for_test(
            &PaneTarget::with_window(alpha.clone(), 0, 0),
            TerminalSize { cols: 80, rows: 23 },
            b"PROMPT> \x1b[1;9H",
        )
        .await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSession(rmux_proto::AttachSessionRequest { target: alpha }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let render_frame =
        String::from_utf8(outcome.attach.expect("attach upgrade").target.render_frame)
            .expect("render frame must be utf-8");
    assert!(
        render_frame.contains("\x1b[1;9H"),
        "attach frame must restore the active pane cursor, got {render_frame:?}"
    );
}

#[tokio::test]
async fn attach_session_active_pane_geometry_tracks_top_status_offset() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(Quiet(&alpha)).await;
    handler
        .set_option(ScopeSelector::Global, OptionName::StatusPosition, "top")
        .await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSession(rmux_proto::AttachSessionRequest { target: alpha }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let target = outcome.attach.expect("attach upgrade").target;
    assert_eq!(
        target.active_pane_geometry.y(),
        1,
        "kitty passthrough coordinates must share the renderer's top-status content offset"
    );
    assert_eq!(target.active_pane_geometry.rows(), 23);
}

#[tokio::test]
async fn attach_session_replays_all_visible_pane_screens() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let top_ready = "RMUX_ATTACH_REPLAY_TOP_READY";
    let bottom_ready = "RMUX_ATTACH_REPLAY_BOTTOM_READY";

    handler
        .create_session(NewSessionExtRequest {
            command: Some(quiet_ready_command(top_ready)),
            ..Fixture::fixture(&alpha)
        })
        .await;
    handler
        .handle_ok(SplitWindowExtRequest {
            command: Some(quiet_ready_command(bottom_ready)),
            ..Fixture::fixture(&alpha)
        })
        .await;
    wait_for_capture_containing(
        &handler,
        PaneTarget::with_window(alpha.clone(), 0, 0),
        top_ready,
        "top pane quiet command should be settled before transcript replacement",
    )
    .await;
    wait_for_capture_containing(
        &handler,
        PaneTarget::with_window(alpha.clone(), 0, 1),
        bottom_ready,
        "bottom pane quiet command should be settled before transcript replacement",
    )
    .await;

    handler
        .replace_transcript_for_test(
            &PaneTarget::with_window(alpha.clone(), 0, 0),
            TerminalSize { cols: 39, rows: 23 },
            b"left-pane\r\n",
        )
        .await;
    handler
        .replace_transcript_for_test(
            &PaneTarget::with_window(alpha.clone(), 0, 1),
            TerminalSize { cols: 40, rows: 23 },
            b"right-pane\r\n",
        )
        .await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSession(rmux_proto::AttachSessionRequest {
                target: alpha.clone(),
            }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let render_frame =
        String::from_utf8(outcome.attach.expect("attach upgrade").target.render_frame)
            .expect("render frame must be utf-8");
    assert!(
        render_frame.contains("left-pane"),
        "attach frame must include left pane transcript, got {render_frame:?}"
    );
    assert!(
        render_frame.contains("right-pane"),
        "attach frame must include right pane transcript, got {render_frame:?}"
    );
}

#[tokio::test]
async fn attach_session_uses_client_size_before_first_frame() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            attach_session_request(&alpha, TerminalSize { cols: 80, rows: 24 }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.terminal_size(), TerminalSize { cols: 80, rows: 24 });
    assert_eq!(session.window().size(), TerminalSize { cols: 80, rows: 23 });
    drop(state);
    assert_eq!(
        handler
            .pane_terminal_size_for_test(&PaneTarget::with_window(alpha.clone(), 0, 0))
            .await,
        TerminalSize { cols: 80, rows: 23 }
    );
}

#[tokio::test]
async fn attach_session_target_spec_selects_requested_window_and_pane_before_attach() {
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
        .handle_ok(SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 1, 0))
        })
        .await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSessionExt2(Box::new(AttachSessionExt2Request {
                target_spec: Some("alpha:1.1".to_owned()),
                ..attach_session_ext2(&alpha, TerminalSize { cols: 80, rows: 24 })
            })),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.active_window_index(), 1);
    assert_eq!(
        session
            .window_at(1)
            .expect("window 1 exists")
            .active_pane_index(),
        1
    );
}

#[tokio::test]
async fn legacy_attach_request_disables_render_stream_frames() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;

    let outcome = handler
        .dispatch(
            std::process::id(),
            attach_session_request(&alpha, TerminalSize { cols: 80, rows: 24 }),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(
        !outcome.attach.expect("attach upgrade").render_stream,
        "Ext2 clients cannot decode AttachMessage::Render"
    );
}

#[tokio::test]
async fn attach_render_capability_enables_render_stream_frames() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    handler.create_session(&alpha).await;

    let request = AttachSessionExt3Request::from_ext2(
        attach_session_ext2(&alpha, TerminalSize { cols: 80, rows: 24 }),
        vec![CAPABILITY_ATTACH_RENDER.to_owned()],
    );
    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSessionExt3(Box::new(request)),
        )
        .await;

    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(
        outcome.attach.expect("attach upgrade").render_stream,
        "Ext3 clients explicitly opted into AttachMessage::Render"
    );
}
