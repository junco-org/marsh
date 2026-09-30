use super::*;
use crate::daemon::ShutdownHandle;
use rmux_core::events::SubscriptionLimits;
use rmux_proto::WebShareCreatedResponse;
use rmux_proto::{encode_attach_message, AttachMessage};
use rmux_proto::{
    CopyModeRequest, CreateWebShareRequest, HookName, KillPaneRequest, KillSessionRequest,
    LinkWindowRequest, ListWebSharesRequest, OptionName, PaneTarget, RenameSessionRequest, Request,
    Response, ScopeSelector, SessionName, SetHookRequest, SplitDirection, SplitWindowRequest,
    StopWebShareRequest, TerminalSize, WebShareScope, WindowTarget,
};
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout, Duration, Instant};

use crate::test_fixtures::{operator_token, spectator_token, Fixture, SessionSpec, TestRequest};

#[tokio::test]
async fn shutdown_rejects_web_pane_text_key_and_session_mutations() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-shutdown-rejected").await;
    let session_id = handler.session_id_for_test(&session_name).await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(
            PaneTarget::new(session_name.clone(), 0).into(),
        )),
    )
    .await;
    let open_token = spectator_token(&created);
    let pane_target = PaneTargetRef::by_id(session_name.clone(), PaneId::new(1));
    let session_target = crate::web::WebSessionTarget::new(session_name, session_id);
    let requester_pid = std::process::id();
    handler.close_normal_request_admission();

    let mut results = Vec::new();
    results.push(handler.open_web_share(&open_token, None).await.map(drop));
    results.push(
        handler
            .web_send_text(&pane_target, "blocked-text".to_owned())
            .await,
    );
    results.push(handler.web_send_key(&pane_target, "Enter".to_owned()).await);
    results.push(
        handler
            .web_session_logout(&session_target, requester_pid)
            .await,
    );
    results.push(
        handler
            .web_session_select_pane(&session_target, requester_pid, PaneId::new(1))
            .await,
    );
    results.push(
        handler
            .web_session_resize_pane(
                &session_target,
                requester_pid,
                PaneId::new(1),
                rmux_proto::ResizePaneAdjustment::NoOp,
            )
            .await,
    );
    results.push(
        handler
            .web_session_split_pane(&session_target, requester_pid, SplitDirection::Horizontal)
            .await,
    );
    results.push(
        handler
            .web_session_new_window(&session_target, requester_pid)
            .await,
    );
    results.push(
        handler
            .web_session_kill_active_pane(&session_target, requester_pid)
            .await,
    );
    results.push(
        handler
            .web_session_select_window(&session_target, requester_pid, 0)
            .await,
    );
    results.push(
        handler
            .web_session_select_window_for_view(&session_target, requester_pid, 0)
            .await
            .map(drop),
    );
    results.push(
        handler
            .web_session_rename_window(&session_target, requester_pid, 0, "blocked-name".to_owned())
            .await,
    );
    results.push(
        handler
            .web_session_kill_window(&session_target, requester_pid, 0)
            .await,
    );

    assert_eq!(
        results.len(),
        13,
        "all Web mutation entry points are covered"
    );
    for result in results {
        let error = result.expect_err("Web mutation must be rejected after quiesce closes");
        assert!(
            error.to_string().contains("server is shutting down"),
            "unexpected rejection: {error}"
        );
    }

    assert!(handler.normal_drain_requests_quiesced());
}

#[tokio::test]
async fn shutdown_drains_a_web_session_mutation_admitted_before_close() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-shutdown-drain").await;
    let session_target = crate::web::WebSessionTarget::new(
        session_name.clone(),
        handler.session_id_for_test(&session_name).await,
    );

    // Hold the first state lock needed by the operation. Admission happens
    // before that lock, making the close-vs-mutation ordering deterministic.
    let state = handler.state.lock().await;
    let mutation_handler = handler.clone();
    let mutation = tokio::spawn(async move {
        mutation_handler
            .web_session_new_window(&session_target, std::process::id())
            .await
    });
    timeout(Duration::from_secs(1), async {
        while handler.normal_drain_requests_quiesced() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Web mutation acquires Drain admission before its first state lock");

    handler.close_normal_request_admission();
    assert!(
        !handler.normal_drain_requests_quiesced(),
        "an admitted Web mutation must retain the shutdown barrier"
    );
    drop(state);

    mutation
        .await
        .expect("Web mutation task joins")
        .expect("the already-admitted Web mutation completes");
    timeout(Duration::from_secs(1), async {
        while !handler.normal_drain_requests_quiesced() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed Web mutation releases the Drain barrier");

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&session_name)
            .expect("session survives")
            .windows()
            .len(),
        2,
        "the admitted session mutation commits before shutdown can seal"
    );
}

#[tokio::test]
async fn web_new_window_uses_shared_initial_name_primitive_when_automatic_rename_is_off() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-initial-window-name").await;
    handler
        .set_option(ScopeSelector::Global, OptionName::AutomaticRename, "off")
        .await;
    let session_target = crate::web::WebSessionTarget::new(
        session_name.clone(),
        handler.session_id_for_test(&session_name).await,
    );

    handler
        .web_session_new_window(&session_target, std::process::id())
        .await
        .expect("Web new-window succeeds");

    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(&session_name)
        .and_then(|session| session.window_at(1))
        .expect("Web created a second window");
    let runtime_name = state
        .pane_runtime_window_name_in_window(&session_name, 1, 0)
        .expect("Web pane has runtime state")
        .expect("Web pane has a useful runtime name");
    assert_eq!(window.name(), Some(runtime_name.as_str()));
}

#[tokio::test]
async fn web_share_create_starts_lazy_listener() {
    let handler = handler_with_automatic_web_port();
    let session_name = SessionSpec::create(&handler, "lazy-start").await;

    let response = TestRequest::send_ok(
        &handler,
        WebShareRequest::Create(share_request(WebShareScope::Session(session_name))),
    )
    .await;

    assert!(matches!(
        *response,
        rmux_proto::WebShareResponse::Created(_)
    ));
}

#[tokio::test]
async fn web_share_config_starts_lazy_listener() {
    let handler = handler_with_automatic_web_port();

    let response = TestRequest::send_ok(
        &handler,
        WebShareRequest::Config(rmux_proto::WebShareConfigRequest),
    )
    .await;

    let rmux_proto::WebShareResponse::Config(config) = *response else {
        panic!("expected web-share config response");
    };
    assert_eq!(config.listener, handler.web_settings().listener());
}

#[tokio::test]
async fn implicit_web_share_port_falls_back_when_default_is_busy() {
    let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind blocker");
    let blocked_port = blocker.local_addr().expect("blocker addr").port();
    let handler = handler_with_web_settings(
        crate::web::WebShareSettings::from_options_with_port_explicit(blocked_port, None, false)
            .expect("web settings"),
    );

    let response = TestRequest::send_ok(
        &handler,
        WebShareRequest::Config(rmux_proto::WebShareConfigRequest),
    )
    .await;

    let rmux_proto::WebShareResponse::Config(config) = *response else {
        panic!("expected web-share config response");
    };
    assert_eq!(config.listener.host, "127.0.0.1");
    assert_ne!(config.listener.port, blocked_port);
}

#[tokio::test]
async fn concurrent_web_share_create_waits_for_lazy_listener_start() {
    let handler = handler_with_automatic_web_port();
    let alpha = SessionSpec::create(&handler, "lazy-alpha").await;
    let beta = SessionSpec::create(&handler, "lazy-beta").await;

    let (left, right) = tokio::join!(
        TestRequest::send_ok(
            &handler,
            WebShareRequest::Create(share_request(WebShareScope::Session(alpha)))
        ),
        TestRequest::send_ok(
            &handler,
            WebShareRequest::Create(share_request(WebShareScope::Session(beta)))
        ),
    );

    assert!(matches!(*left, rmux_proto::WebShareResponse::Created(_)));
    assert!(matches!(*right, rmux_proto::WebShareResponse::Created(_)));
    assert_eq!(list_shares(&handler).await.len(), 2);
}

#[tokio::test]
async fn failed_lazy_listener_start_does_not_create_share() {
    let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind blocker");
    let port = blocker.local_addr().expect("blocker addr").port();
    let handler = handler_with_web_port(port);
    let session_name = SessionSpec::create(&handler, "lazy-bind-failure").await;

    let response = handler
        .handle(Request::WebShare(Box::new(WebShareRequest::Create(
            share_request(WebShareScope::Session(session_name)),
        ))))
        .await;

    let Response::Error(error) = response else {
        panic!("expected listener startup failure");
    };
    assert!(error.error.to_string().contains("listener unavailable"));
    assert!(list_shares(&handler).await.is_empty());

    drop(blocker);
}

#[tokio::test]
async fn web_share_create_resolves_slot_target_to_stable_pane_id() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "alpha").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(
            rmux_proto::PaneTarget::new(session_name.clone(), 0).into(),
        )),
    )
    .await;
    assert!(matches!(
        created.scope,
        WebShareScope::Pane(PaneTargetRef::Id {
            session_name: ref actual,
            ..
        }) if actual == &session_name
    ));
    assert!(created
        .spectator_url
        .as_deref()
        .expect("spectator URL")
        .contains("#e=wss://share.example/share&t="));
}

#[tokio::test]
async fn stopped_ttl_share_wakes_its_expiry_waiter() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "ttl-stop-wakeup").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            ttl_seconds: Some(7 * 24 * 60 * 60),
            ..share_request(WebShareScope::Session(session_name))
        },
    )
    .await;
    let mut revoke_rx = handler
        .web_shares
        .expiry_revoke_receiver(&created.share_id)
        .expect("TTL share has an expiry cancellation receiver");

    let stopped = TestRequest::send_ok(
        &handler,
        WebShareRequest::Stop(StopWebShareRequest {
            share_id: created.share_id,
        }),
    )
    .await;
    assert!(matches!(
        stopped.as_ref(),
        rmux_proto::WebShareResponse::Stopped(stopped) if stopped.stopped
    ));
    timeout(Duration::from_millis(100), revoke_rx.changed())
        .await
        .expect("stopping the share must wake the seven-day expiry waiter")
        .expect("revoke sender remains valid through notification");
    assert_eq!(
        *revoke_rx.borrow(),
        Some(crate::web::WebShareRevokeReason::StoppedByOwner)
    );
}

#[tokio::test]
async fn tunnel_completion_revalidation_rejects_a_removed_target() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-stale-target").await;
    let resolved = handler
        .resolve_create_web_share(share_request(WebShareScope::Pane(
            PaneTarget::new(session_name.clone(), 0).into(),
        )))
        .await
        .expect("initial target resolves");

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(session_name)).await;

    let state = handler.state.lock().await;
    assert!(validate_resolved_web_target(&state, resolved.target()).is_err());
    drop(state);
    assert!(list_shares(&handler).await.is_empty());
}

#[tokio::test]
async fn web_session_share_drains_initial_attach_output() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            spectator: true,
            controls: true,
            ..share_request(WebShareScope::Session(session_name))
        },
    )
    .await;
    let operator_token = operator_token(&created);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(mut session_stream) = stream else {
        panic!("expected session web share stream");
    };
    let mut reader = session_stream.take_attach_reader();
    let event = timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("attach stream should produce initial output")
        .expect("attach read succeeds")
        .expect("initial attach output is present");
    assert!(matches!(event, WebSessionAttachEvent::Data(_)));
    assert_eq!(session_stream.snapshot.size.cols, 80);
    assert_eq!(session_stream.snapshot.size.rows, 24);
}

#[tokio::test]
async fn web_pane_stream_resnapshots_instead_of_forwarding_cross_boundary_rep() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-rep").await;
    let target = PaneTarget::new(session_name.clone(), 0);
    let (output, transcript) = {
        let state = handler.state.lock().await;
        (
            state
                .pane_output_for_target(&session_name, 0, 0)
                .expect("pane output"),
            state.transcript_handle(&target).expect("pane transcript"),
        )
    };
    crate::pane_io::publish_pane_bytes_for_test(&transcript, &output, b"X".to_vec());
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(target.clone().into())),
    )
    .await;
    let token = spectator_token(&created);
    let stream = handler
        .open_web_share(&token, None)
        .await
        .expect("pane web share opens");
    let WebShareStream::Pane(mut pane) = stream else {
        panic!("expected pane web share stream");
    };

    crate::pane_io::publish_pane_bytes_for_test(&transcript, &output, b"\x1b[2b".to_vec());

    assert!(matches!(
        pane.output.try_recv_observed(),
        Some(crate::pane_io::PaneObservationItem::Invalidated(invalidation))
            if invalidation.reason
                == crate::pane_io::PaneInvalidationReason::TranscriptMutation
    ));
    assert!(
        pane.output.try_recv_observed().is_none(),
        "the browser must not receive REP without parser INPUT_LAST"
    );
    let (snapshot, _) = handler
        .web_resnapshot(&pane.target)
        .await
        .expect("post-REP browser snapshot");
    let mut recovered = rmux_core::TerminalScreen::new(
        TerminalSize {
            cols: snapshot.cols,
            rows: snapshot.rows,
        },
        2_000,
    );
    recovered.feed(
        snapshot
            .recovery_keyframe
            .as_deref()
            .expect("pane resnapshot includes recovery keyframe"),
    );
    assert_eq!(
        recovered.screen().capture_transcript(
            rmux_core::ScreenCaptureRange::default(),
            rmux_core::GridRenderOptions::default(),
        ),
        transcript.lock().expect("transcript lock").capture_main(
            rmux_core::ScreenCaptureRange::default(),
            rmux_core::GridRenderOptions::default(),
        )
    );
}

/// A pane-scoped WebShare advertises a read-only *view* of the pane and has no
/// scrollback protocol at all (`PaneScroll` closes with `scroll_requires_session`).
/// The recovery keyframe must therefore replay the visible viewport only: rows
/// that already scrolled out of the pane before the link was handed out must
/// never reach a viewer's browser scrollback.
#[tokio::test]
async fn web_pane_snapshot_never_replays_scrolled_off_history() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-scrollback").await;
    let target = PaneTarget::new(session_name.clone(), 0);
    let (output, transcript) = {
        let state = handler.state.lock().await;
        (
            state
                .pane_output_for_target(&session_name, 0, 0)
                .expect("pane output"),
            state.transcript_handle(&target).expect("pane transcript"),
        )
    };
    // Secret scrolls out of the 24-row viewport, then `clear` blanks the screen.
    let mut bytes = b"AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI\r\n".to_vec();
    for line in 0..60_u32 {
        bytes.extend_from_slice(format!("filler {line}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[H\x1b[2J");
    crate::pane_io::publish_pane_bytes_for_test(&transcript, &output, bytes);

    let pane_target = PaneTargetRef::from(target);
    let (snapshot, _) = handler
        .web_resnapshot(&pane_target)
        .await
        .expect("web pane resnapshot");
    let keyframe = snapshot
        .recovery_keyframe
        .as_deref()
        .expect("pane resnapshot includes recovery keyframe");

    assert!(
        snapshot.history_rows_total > 0,
        "the pane must actually hold scrollback for this probe to mean anything"
    );
    assert!(
        !contains_subslice(keyframe, b"AWS_SECRET_ACCESS_KEY"),
        "scrolled-off scrollback leaked into the WebShare pane snapshot"
    );
    assert!(
        !contains_subslice(keyframe, b"filler 0"),
        "scrolled-off scrollback leaked into the WebShare pane snapshot"
    );
    assert_eq!(
        snapshot.history_rows_included, 0,
        "a web pane snapshot must report zero replayed history rows"
    );
}

#[tokio::test]
async fn web_session_attach_renders_rep_from_authoritative_screen_state() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-session-rep").await;
    let target = PaneTarget::new(session_name.clone(), 0);
    let (output, transcript) = {
        let state = handler.state.lock().await;
        (
            state
                .pane_output_for_target(&session_name, 0, 0)
                .expect("pane output"),
            state.transcript_handle(&target).expect("pane transcript"),
        )
    };
    crate::pane_io::publish_pane_bytes_for_test(&transcript, &output, b"X".to_vec());
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name)),
    )
    .await;
    let token = spectator_token(&created);
    let stream = handler
        .open_web_share(&token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(mut session) = stream else {
        panic!("expected session web share stream");
    };
    let mut reader = session.take_attach_reader();
    let _ = timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("initial attach frame")
        .expect("attach read")
        .expect("initial data");

    crate::pane_io::publish_pane_bytes_for_test(&transcript, &output, b"\x1b[2b".to_vec());
    let event = timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("post-REP attach render")
        .expect("attach read")
        .expect("post-REP data");
    let WebSessionAttachEvent::Data(frame) = event else {
        panic!("REP changes the rendered session surface");
    };
    assert!(
        !frame.windows(b"\x1b[2b".len()).any(|window| window == b"\x1b[2b"),
        "session Web clients start from a rendered keyframe, so REP must be rendered from the authoritative screen instead of forwarded raw"
    );
}

#[tokio::test]
async fn web_session_last_exit_drains_before_daemon_shutdown() {
    let handler = RequestHandler::new();
    let (shutdown_handle, mut shutdown_rx) = ShutdownHandle::new();
    handler.install_shutdown_handle(shutdown_handle);
    let session_name = SessionSpec::create(&handler, "websession-exit-drain").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let stream = handler
        .open_web_share(&spectator_token(&created), None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(mut session_stream) = stream else {
        panic!("expected session web share stream");
    };
    let attach_pid = session_stream.attach_pid();
    let control_tx = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("web session attach is registered")
        .control_tx
        .clone();
    let mut reader = session_stream.take_attach_reader();
    let _ = timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("attach stream should produce initial output")
        .expect("attach read succeeds");
    control_tx
        .send(AttachControl::Write(vec![b'x'; 128 * 1024]))
        .expect("fill the bounded in-process attach transport");
    tokio::task::yield_now().await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(session_name)).await;
    assert!(
        !handler.request_shutdown_if_pending(),
        "web attach wire drain must defer exit-empty shutdown"
    );
    assert!(
        timeout(Duration::from_millis(25), &mut shutdown_rx)
            .await
            .is_err(),
        "daemon shutdown must stay pending until the browser receives its exit frame"
    );

    let exited = timeout(Duration::from_secs(2), async {
        loop {
            match reader.read_event().await.expect("attach read succeeds") {
                Some(WebSessionAttachEvent::Data(bytes))
                    if bytes
                        .windows(b"[exited]\r\n".len())
                        .any(|window| window == b"[exited]\r\n") =>
                {
                    break;
                }
                Some(_) => continue,
                None => panic!("web attach closed before its exit frame"),
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "web attach exit frame should drain");

    timeout(Duration::from_millis(500), shutdown_rx)
        .await
        .expect("daemon should shut down after the web attach drains")
        .expect("shutdown receiver should complete cleanly");
}

#[tokio::test]
async fn web_session_attach_reader_emits_resize_events() {
    let (mut writer, reader) = tokio::io::duplex(128);
    let (reader, _) = tokio::io::split(reader);
    let mut reader = WebSessionAttachReader::new(reader);
    let frame = encode_attach_message(&AttachMessage::Resize(TerminalSize {
        cols: 100,
        rows: 30,
    }))
    .expect("resize attach message encodes");

    writer.write_all(&frame).await.expect("write attach frame");

    let event = timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("attach reader should observe resize")
        .expect("attach read succeeds")
        .expect("resize event is present");
    assert!(matches!(event, WebSessionAttachEvent::Resize));
}

#[tokio::test]
async fn web_session_operator_registers_writable_attach() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-write").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = operator_token(&created);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };
    assert!(session_stream.is_operator());
    assert!(session_stream.controls());

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .values()
        .find(|active| active.session_name == session_name)
        .expect("web session attach is registered");
    assert!(active.can_write);
    assert!(!active.flags.contains(ClientFlags::READONLY));
}

#[tokio::test]
async fn web_session_operator_without_browser_size_keeps_status_aware_geometry() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-sizeless-operator").await;
    seed_two_line_status_geometry(&handler, &session_name, 81_001).await;

    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = operator_token(&created);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(_session_stream) = stream else {
        panic!("expected session web share stream");
    };

    // A browser that never reported its size registers without one. Repeated
    // status changes must still take the status rows off the outer terminal
    // anchor exactly once, never off the already-subtracted content rows.
    for value in ["off", "2", "off", "2"] {
        handler.set_session_status(&session_name, value).await;
    }
    assert_eq!(
        session_window_size(&handler, &session_name).await,
        TerminalSize { cols: 80, rows: 22 },
        "a sizeless WebShare operator must not shrink the session on every status change"
    );
}

#[tokio::test]
async fn web_session_spectator_share_attach_ignores_browser_size() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-read-size").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let stream = handler
        .open_web_share(&spectator_token(&created), None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };
    assert!(!session_stream.is_operator());

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .values()
        .find(|active| active.session_name == session_name)
        .expect("web session attach is registered");
    assert!(!active.can_write);
    assert!(active.flags.contains(ClientFlags::READONLY));
    assert!(active.flags.contains(ClientFlags::IGNORESIZE));
}

#[tokio::test]
async fn web_session_snapshot_tracks_canonical_session_size() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-snapshot-size").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let stream = handler
        .open_web_share(&spectator_token(&created), None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };
    assert_eq!(
        session_stream.snapshot.size,
        TerminalSize { cols: 80, rows: 24 }
    );

    {
        let mut state = handler.state.lock().await;
        state
            .sessions
            .session_mut(&session_name)
            .expect("session exists")
            .resize_terminal(TerminalSize { cols: 60, rows: 10 });
    }

    let snapshot = handler
        .web_session_snapshot(session_stream.target())
        .await
        .expect("session snapshot refreshes");
    assert_eq!(snapshot.size, TerminalSize { cols: 60, rows: 10 });
}

#[tokio::test]
async fn web_session_snapshot_uses_content_geometry_without_reapplying_status_rows() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create_started(&handler, "websession-content-geometry").await;
    {
        let mut state = handler.state.lock().await;
        state
            .mutate_session_and_resize_window_terminal(&session_name, 0, |session| {
                session.resize_active_window_geometry(
                    TerminalSize { cols: 80, rows: 24 },
                    TerminalSize { cols: 80, rows: 21 },
                );
                Ok(())
            })
            .expect("content geometry resize succeeds");
    }
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&session_name)).await;
    let bottom_target = PaneTarget::with_window(session_name.clone(), 0, 1);
    handler
        .wait_for_pane_startup_to_finish_for_test(&bottom_target)
        .await;
    let (session_id, bottom_pane_id, transcript) = {
        let state = handler.state.lock().await;
        let session = state
            .sessions
            .session(&session_name)
            .expect("session exists");
        let pane = session
            .window()
            .pane(1)
            .expect("split creates a bottom pane");
        (
            session.id(),
            pane.id(),
            state
                .transcript_handle(&bottom_target)
                .expect("bottom pane transcript"),
        )
    };
    transcript.lock().expect("transcript lock").append_bytes(
        b"00\r\n01\r\n02\r\n03\r\n04\r\n05\r\n06\r\n07\r\n08\r\n09\r\n\
              10\r\n11\r\n12\r\n13\r\n14\r\n15\r\n16\r\n17\r\n18\r\n19\r\n\
              20\r\n21\r\n22\r\n23\r\n24\r\n25\r\n26\r\n27\r\n28\r\n29\r\n",
    );
    handler.set_session_status(&session_name, "3").await;

    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let stream = handler
        .open_web_share(&spectator_token(&created), None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };

    assert_eq!(
        session_stream.snapshot.size,
        TerminalSize { cols: 80, rows: 21 }
    );
    assert_eq!(
        session_stream.snapshot.view.size,
        session_stream.snapshot.size
    );
    assert_eq!(
        session_stream
            .snapshot
            .view
            .panes
            .iter()
            .map(|pane| pane.rows)
            .collect::<Vec<_>>(),
        vec![10, 10]
    );

    let bottom = session_stream
        .snapshot
        .view
        .panes
        .iter()
        .find(|pane| pane.id == bottom_pane_id.as_u32())
        .expect("bottom pane remains visible");
    let scroll_frame = handler
        .web_session_pane_scroll_frame(
            &crate::web::WebSessionTarget::new(session_name, session_id),
            bottom_pane_id,
            0,
            None,
        )
        .await
        .expect("Web pane scroll frame succeeds")
        .expect("scrollback produces a pane frame");
    assert_eq!(
        (scroll_frame.pane.x, scroll_frame.pane.y),
        (bottom.x, bottom.y)
    );
    assert_eq!(
        (scroll_frame.pane.cols, scroll_frame.pane.rows),
        (bottom.cols, bottom.rows)
    );
}

#[tokio::test]
async fn web_share_expiry_kills_session_after_unix_second_rounding_window() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-expire").await;
    create_share(
        &handler,
        CreateWebShareRequest {
            ttl_seconds: Some(1),
            kill_session_on_expire: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let removed = {
            let state = handler.state.lock().await;
            state.sessions.session(&session_name).is_none()
        };
        if removed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expired web-share did not kill its session"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn kill_session_prunes_web_session_share_before_name_reuse() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            spectator: true,
            controls: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = operator_token(&created);

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&session_name)).await;

    assert!(
        list_shares(&handler).await.is_empty(),
        "shares for a removed session should be pruned"
    );

    SessionSpec::create(&handler, &session_name).await;

    let error = handler
        .open_web_share(&operator_token, None)
        .await
        .err()
        .expect("old share must not attach to a recreated session");
    assert!(error.to_string().contains("does not exist"));
}

#[tokio::test]
async fn kill_session_pane_prune_preserves_recreated_name_share() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-session-aba").await;
    let (original_session_id, original_pane_id) = {
        let state = handler.state.lock().await;
        let session = state
            .sessions
            .session(&session_name)
            .expect("original session exists");
        (
            session.id(),
            session.active_pane_id().expect("original pane exists"),
        )
    };
    let original = create_share(
        &handler,
        share_request(WebShareScope::Pane(PaneTargetRef::by_id(
            session_name.clone(),
            original_pane_id,
        ))),
    )
    .await;
    let original_token = spectator_token(&original);
    let pause = handler.install_kill_session_web_prune_pause(session_name.clone());
    let kill_handler = handler.clone();
    let kill_session_name = session_name.clone();
    let kill = tokio::spawn(async move {
        kill_handler
            .handle(Request::KillSession(KillSessionRequest::fixture(
                kill_session_name,
            )))
            .await
    });
    timeout(Duration::from_secs(5), pause.reached.notified())
        .await
        .expect("kill-session reaches the Web prune pause");

    let recreate_handler = handler.clone();
    let recreate_session_name = session_name.clone();
    let recreate =
        tokio::spawn(
            async move { SessionSpec::create(&recreate_handler, recreate_session_name).await },
        );
    let (replacement_session_id, replacement_pane_id) = timeout(Duration::from_secs(5), async {
        loop {
            let replacement = {
                let state = handler.state.lock().await;
                state
                    .sessions
                    .session(&session_name)
                    .filter(|session| session.id() != original_session_id)
                    .map(|session| {
                        (
                            session.id(),
                            session.active_pane_id().expect("replacement pane exists"),
                        )
                    })
            };
            if let Some(replacement) = replacement {
                break replacement;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement session becomes visible before lifecycle publication");
    assert_ne!(replacement_session_id, original_session_id);
    assert_ne!(replacement_pane_id, original_pane_id);
    let replacement = timeout(
        Duration::from_secs(5),
        create_share(
            &handler,
            share_request(WebShareScope::Pane(PaneTargetRef::by_id(
                session_name.clone(),
                replacement_pane_id,
            ))),
        ),
    )
    .await
    .expect("replacement share is created before stale cleanup resumes");
    let replacement_token = spectator_token(&replacement);

    pause.release.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(5), kill)
            .await
            .expect("kill-session completes after Web prune resumes")
            .expect("kill-session task completes"),
        Response::KillSession(_)
    ));
    assert_eq!(
        timeout(Duration::from_secs(5), recreate)
            .await
            .expect("replacement lifecycle publication completes")
            .expect("replacement session task completes"),
        session_name
    );
    assert!(handler.open_web_share(&original_token, None).await.is_err());
    let replacement_stream = handler
        .open_web_share(&replacement_token, None)
        .await
        .expect("replacement pane share survives stale session cleanup");
    assert!(matches!(replacement_stream, WebShareStream::Pane(_)));
}

#[tokio::test]
async fn kill_session_revokes_origin_pane_share_when_real_winlink_survives() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create(&handler, "web-pane-linked-owner").await;
    let survivor = SessionSpec::create(&handler, "web-pane-linked-survivor").await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&owner)
            .expect("owner session exists")
            .active_pane_id()
            .expect("owner pane exists")
    };
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(PaneTargetRef::by_id(
            owner.clone(),
            pane_id,
        ))),
    )
    .await;
    let token = spectator_token(&created);

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(survivor.clone(), 1),
        )),
    )
    .await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(owner)).await;

    {
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .sessions
                .session(&survivor)
                .and_then(|session| session.window_at(1))
                .and_then(|window| window.pane(0))
                .map(|pane| pane.id()),
            Some(pane_id),
            "the true winlink must keep the pane alive independently of the share"
        );
    }
    assert!(list_shares(&handler).await.is_empty());
    assert!(handler.open_web_share(&token, None).await.is_err());
}

#[tokio::test]
async fn killing_last_pane_prunes_web_session_share() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-kill-pane").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let spectator_token = spectator_token(&created);

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::new(session_name.clone(), 0),
            kill_all_except: false,
        },
    )
    .await;

    assert!(
        list_shares(&handler).await.is_empty(),
        "session shares should be pruned when the last pane destroys the session"
    );

    let error = handler
        .open_web_share(&spectator_token, None)
        .await
        .err()
        .expect("old share must not attach after the session was destroyed");
    assert!(error.to_string().contains("does not exist"));
}

#[tokio::test]
async fn killing_one_shared_pane_revokes_only_its_stable_share() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-kill").await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(&session_name)
        },
    )
    .await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&session_name)
            .expect("session exists")
            .window()
            .pane(1)
            .expect("second pane exists")
            .id()
    };
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(PaneTargetRef::by_id(
            session_name.clone(),
            pane_id,
        ))),
    )
    .await;
    let token = spectator_token(&created);

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(session_name, 0, 1),
            kill_all_except: false,
        },
    )
    .await;
    assert!(list_shares(&handler).await.is_empty());
    assert!(handler.open_web_share(&token, None).await.is_err());
}

#[tokio::test]
async fn web_kill_pane_drains_after_kill_pane_inline_hook() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-after-kill-hook").await;
    let hook_target = SessionSpec::create(&handler, "web-after-kill-hook-target").await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(&session_name)
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Session(session_name.clone()),
            HookName::AfterKillPane,
            format!("new-window -d -t {hook_target}").as_str(),
        )),
    )
    .await;
    let session_target = crate::web::WebSessionTarget::new(
        session_name.clone(),
        handler.session_id_for_test(&session_name).await,
    );

    handler
        .web_session_kill_active_pane(&session_target, std::process::id())
        .await
        .expect("web pane kill succeeds");

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&hook_target)
            .expect("hook target survives")
            .windows()
            .len(),
        2,
        "the after-kill hook must run outside the Web request identity guard"
    );
}

#[tokio::test]
async fn web_session_identity_guard_preserves_recreated_name() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-session-identity-aba").await;
    let stale_session_id = handler.session_id_for_test(&session_name).await;
    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&session_name)).await;
    SessionSpec::create(&handler, &session_name).await;

    let response = super::super::web_request_identity::with_expected_session_identity(
        session_name.clone(),
        stale_session_id,
        handler.handle(Request::NewWindow(Box::new(NewWindowRequest::fixture(
            &session_name,
        )))),
    )
    .await;

    assert!(matches!(response, Response::Error(_)));
    let state = handler.state.lock().await;
    let replacement = state
        .sessions
        .session(&session_name)
        .expect("replacement session survives stale web request");
    assert_ne!(replacement.id(), stale_session_id);
    assert_eq!(replacement.windows().len(), 1);
}

#[tokio::test]
async fn web_window_identity_guard_preserves_recreated_slot() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-window-identity-aba").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session_name)
        })
        .await;
    let session_id = handler.session_id_for_test(&session_name).await;
    let stale_window_id = handler
        .window_id_for_test(&WindowTarget::with_window(session_name.clone(), 1))
        .await;
    TestRequest::send_ok(
        &handler,
        KillWindowRequest::fixture(WindowTarget::with_window(session_name.clone(), 1)),
    )
    .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&session_name)
        })
        .await;

    let response = super::super::web_request_identity::with_expected_window_identity(
        session_name.clone(),
        session_id,
        1,
        stale_window_id,
        handler.handle(Request::KillWindow(KillWindowRequest::fixture(
            WindowTarget::with_window(session_name.clone(), 1),
        ))),
    )
    .await;

    assert!(matches!(response, Response::Error(_)));
    let state = handler.state.lock().await;
    let replacement = state
        .sessions
        .session(&session_name)
        .and_then(|session| session.window_at(1))
        .expect("replacement window survives stale web request");
    assert_ne!(replacement.id(), stale_window_id);
}

#[tokio::test]
async fn renaming_session_rekeys_stable_pane_share() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-old").await;
    let renamed = SessionName::new("web-pane-new").expect("valid session name");
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(
            PaneTarget::new(session_name.clone(), 0).into(),
        )),
    )
    .await;
    let token = spectator_token(&created);

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: session_name,
            new_name: renamed.clone(),
        },
    )
    .await;
    let shares = list_shares(&handler).await;
    assert!(matches!(
        shares.as_slice(),
        [share]
            if matches!(&share.scope, WebShareScope::Pane(target) if target.session_name() == &renamed)
    ));
    assert!(handler.open_web_share(&token, None).await.is_ok());
}

#[tokio::test]
async fn open_pane_stream_follows_session_rename_by_stable_identity() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "web-pane-stream-old").await;
    let renamed = SessionName::new("web-pane-stream-new").expect("valid session name");
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(
            PaneTarget::new(session_name.clone(), 0).into(),
        )),
    )
    .await;
    let token = spectator_token(&created);
    let stream = handler
        .open_web_share(&token, None)
        .await
        .expect("pane stream opens before rename");
    let WebShareStream::Pane(mut pane) = stream else {
        panic!("expected pane stream");
    };

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: session_name,
            new_name: renamed.clone(),
        },
    )
    .await;

    let current = handler
        .current_web_pane_target(pane.session_id(), pane.target())
        .await
        .expect("open stream resolves renamed session by id");
    pane.set_target(current);
    assert_eq!(pane.target().session_name(), &renamed);
    assert!(handler.web_target_alive(pane.target()).await);
    assert!(handler.web_resnapshot(pane.target()).await.is_ok());
}

#[tokio::test]
async fn kill_session_on_expire_follows_renamed_session_id() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-expiry").await;
    let renamed_session = SessionName::new("websession-expiry-renamed").expect("valid session");
    create_share(
        &handler,
        CreateWebShareRequest {
            ttl_seconds: Some(6),
            operator: true,
            kill_session_on_expire: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: session_name.clone(),
            new_name: renamed_session.clone(),
        },
    )
    .await;

    timeout(Duration::from_secs(10), async {
        loop {
            let session_gone = {
                let state = handler.state.lock().await;
                state.sessions.session(&renamed_session).is_none()
            };
            if session_gone {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expiry task should kill the renamed session by id");

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&session_name).is_none());
    assert!(state.sessions.session(&renamed_session).is_none());
}

#[tokio::test]
async fn web_session_select_pane_uses_explicit_pane_id() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-select-pane").await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(&session_name)
        },
    )
    .await;
    let right_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&session_name)
            .expect("session exists")
            .window()
            .pane(1)
            .expect("right pane exists")
            .id()
    };
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            spectator: false,
            max_spectators: None,
            max_operators: None,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = operator_token(&created);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };

    handler
        .web_session_select_pane(
            session_stream.target(),
            session_stream.attach_pid(),
            right_pane_id,
        )
        .await
        .expect("pane selection succeeds");

    let state = handler.state.lock().await;
    let active = state
        .sessions
        .session(&session_name)
        .expect("session exists")
        .window()
        .active_pane()
        .expect("active pane exists")
        .id();
    assert_eq!(active, right_pane_id);
}

#[tokio::test]
async fn web_session_operator_resize_reaches_attached_session() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create(&handler, "websession-browser-resize").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            operator: true,
            spectator: false,
            max_spectators: None,
            max_operators: None,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = operator_token(&created);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(mut session_stream) = stream else {
        panic!("expected session web share stream");
    };

    session_stream
        .send_attach_resize(TerminalSize {
            cols: 100,
            rows: 40,
        })
        .await
        .expect("resize is written to attach stream");

    timeout(Duration::from_secs(2), async {
        loop {
            let geometry = {
                let state = handler.state.lock().await;
                let session = state
                    .sessions
                    .session(&session_name)
                    .expect("session exists");
                (session.terminal_size(), session.window().size())
            };
            if geometry
                == (
                    TerminalSize {
                        cols: 100,
                        rows: 40,
                    },
                    TerminalSize {
                        cols: 100,
                        rows: 39,
                    },
                )
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("browser resize reaches the attached session as external and content geometry");
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn web_session_pane_scroll_frame_degrades_when_recovery_metadata_is_bounded_out() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create_started(&handler, "websession-scroll-hyperlinks").await;
    let session_id = handler.session_id_for_test(&session_name).await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    let (pane_id, transcript) = {
        let state = handler.state.lock().await;
        (
            state
                .sessions
                .session(&session_name)
                .expect("session exists")
                .window()
                .active_pane()
                .expect("active pane exists")
                .id(),
            state.transcript_handle(&target).expect("pane transcript"),
        )
    };

    // Scrollback plus one hyperlink entry beyond the bounded Web recovery
    // contract: an `ls --hyperlink=auto` row or a presigned object URL.
    let mut payload = Vec::new();
    for row in 0..60 {
        payload.extend_from_slice(format!("line {row}\r\n").as_bytes());
    }
    let uri = format!(
        "https://example.test/{}",
        "x".repeat(crate::pane_recovery::MAX_RECOVERY_HYPERLINK_ENTRY_BYTES)
    );
    payload.extend_from_slice(format!("\x1b]8;;{uri}\x1b\\X\x1b]8;;\x1b\\").as_bytes());
    transcript
        .lock()
        .expect("transcript lock")
        .append_bytes(&payload);

    let session_target = crate::web::WebSessionTarget::new(session_name, session_id);
    let frame = handler
        .web_session_pane_scroll_frame(&session_target, pane_id, 0, None)
        .await;

    // The full-snapshot twin marks the view incomplete and keeps serving; the
    // scroll patch must fall back to it rather than fail the viewer socket.
    let frame = frame.expect("bounded-out recovery metadata must not fail the scroll frame");
    assert!(
        frame.is_none(),
        "an incomplete-metadata scroll patch must defer to a full snapshot"
    );

    // ... and that fallback must actually render the requested scroll.
    let snapshot = handler
        .web_session_snapshot_with_scrolls(
            &session_target,
            None,
            &HashMap::from([(pane_id, 0_usize)]),
        )
        .await
        .expect("the full snapshot fallback serves the same scroll");
    assert!(!snapshot.view.metadata_complete);
    let scrolled = snapshot
        .view
        .panes
        .iter()
        .find(|pane| pane.id == pane_id.as_u32())
        .expect("scrolled pane is in the view");
    assert!(scrolled.scroll_offset > 0);
}

#[tokio::test]
async fn web_session_snapshot_degrades_for_a_copy_mode_pane_with_bounded_out_metadata() {
    let handler = RequestHandler::new();
    let session_name = SessionSpec::create_started(&handler, "websession-copy-hyperlinks").await;
    let session_id = handler.session_id_for_test(&session_name).await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    let transcript = {
        let state = handler.state.lock().await;
        state.transcript_handle(&target).expect("pane transcript")
    };

    // Copy mode clones the pane screen, so the over-budget hyperlink table has
    // to exist before the mode is entered.
    let uri = format!(
        "https://example.test/{}",
        "x".repeat(crate::pane_recovery::MAX_RECOVERY_HYPERLINK_ENTRY_BYTES)
    );
    transcript
        .lock()
        .expect("transcript lock")
        .append_bytes(format!("\x1b]8;;{uri}\x1b\\X\x1b]8;;\x1b\\").as_bytes());
    TestRequest::send_ok(&handler, CopyModeRequest::fixture(target)).await;

    let session_target = crate::web::WebSessionTarget::new(session_name, session_id);
    let snapshot = handler
        .web_session_snapshot(&session_target)
        .await
        .expect("a copy-mode pane with bounded-out metadata still snapshots");

    // The non-copy-mode branch of the same render drops the over-budget
    // metadata instead of failing; copy mode must degrade the same way and
    // report the gap rather than fail the viewer's frame.
    assert!(!snapshot.view.metadata_complete);

    // Resetting the live pane's hyperlink storage does not reach copy mode's
    // frozen backing screen, which is what the viewer renders, so the reported
    // coverage must still follow the degraded render.
    let response = handler
        .handle(Request::ClearHistory(rmux_proto::ClearHistoryRequest {
            target: PaneTarget::with_window(session_target.name().clone(), 0, 0),
            reset_hyperlinks: true,
        }))
        .await;
    assert!(
        matches!(response, Response::ClearHistory(_)),
        "{response:?}"
    );
    let snapshot = handler
        .web_session_snapshot(&session_target)
        .await
        .expect("a copy-mode pane with bounded-out metadata still snapshots");

    assert!(!snapshot.view.metadata_complete);
}

async fn session_window_size(handler: &RequestHandler, session_name: &SessionName) -> TerminalSize {
    handler
        .state
        .lock()
        .await
        .sessions
        .session(session_name)
        .expect("session exists")
        .window()
        .size()
}

/// Leaves `session_name` with a two-line status, an 80x24 outer terminal and an
/// 80x22 content window, with no client still attached.
async fn seed_two_line_status_geometry(
    handler: &RequestHandler,
    session_name: &SessionName,
    attach_pid: u32,
) {
    handler.set_session_status(session_name, "2").await;
    let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
    let attach_id = handler
        .register_attach(attach_pid, session_name.clone(), control_tx)
        .await;
    handler
        .handle_attached_resize(attach_pid, TerminalSize { cols: 80, rows: 24 })
        .await
        .expect("declared terminal geometry seeds status-aware content size");
    handler.finish_attach(attach_pid, attach_id).await;
    assert_eq!(
        session_window_size(handler, session_name).await,
        TerminalSize { cols: 80, rows: 22 }
    );
}

async fn create_share(
    handler: &RequestHandler,
    request: CreateWebShareRequest,
) -> WebShareCreatedResponse {
    handler.mark_web_listener_available();
    let rmux_proto::WebShareResponse::Created(created) =
        *TestRequest::send_ok(handler, WebShareRequest::Create(request)).await
    else {
        panic!("expected created web-share response");
    };
    created
}

fn share_request(scope: WebShareScope) -> CreateWebShareRequest {
    CreateWebShareRequest {
        public_base_url: Some("https://share.example".to_owned()),
        max_spectators: Some(1),
        ..Fixture::fixture(scope)
    }
}

fn handler_with_web_port(port: u16) -> RequestHandler {
    handler_with_web_settings(
        crate::web::WebShareSettings::from_options(port, None).expect("web settings"),
    )
}

fn handler_with_automatic_web_port() -> RequestHandler {
    handler_with_web_settings(
        crate::web::WebShareSettings::from_options_with_port_explicit(
            unused_web_port(),
            None,
            false,
        )
        .expect("web settings"),
    )
}

fn handler_with_web_settings(settings: crate::web::WebShareSettings) -> RequestHandler {
    RequestHandler::with_owner_uid_subscription_limits_and_web_settings(
        current_owner_uid(),
        SubscriptionLimits::default(),
        settings,
    )
}

fn unused_web_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind free port probe");
    listener.local_addr().expect("free port addr").port()
}

async fn list_shares(handler: &RequestHandler) -> Vec<rmux_proto::WebShareSummary> {
    let rmux_proto::WebShareResponse::List(listed) =
        *TestRequest::send_ok(handler, WebShareRequest::List(ListWebSharesRequest)).await
    else {
        panic!("expected listed web-share response");
    };
    listed.shares
}
