use super::*;
use crate::client_names::attached_client_name;
use crate::test_fixtures::{SessionSpec, TestRequest};
use rmux_proto::{HookName, RefreshClientRequest};

#[tokio::test]
async fn attached_resize_emits_client_resized_hook_with_client_context() {
    let handler = RequestHandler::new();
    let session = session_name("resize-hook");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;
    let (_attach_id, _rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(100, 30)).await;

    handler
        .set_global_hook(
            HookName::ClientResized,
            &format!(
                "if-shell -F '#{{==:#{{hook_client}}:#{{hook_session_name}},{}:resize-hook}}' 'set-buffer -b client-resized ok' 'set-buffer -b client-resized bad'",
                attached_client_name(101)
            ),
        )
        .await;

    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    handler
        .handle_attached_resize(101, TerminalSize::new(132, 37))
        .await
        .expect("client resize succeeds");
    drain_lifecycle_events(&handler, &mut lifecycle_events).await;

    handler.wait_for_buffer("client-resized", "ok").await;
}

#[tokio::test]
async fn attached_resize_does_not_emit_client_resized_hook_when_size_is_unchanged() {
    let handler = RequestHandler::new();
    let session = session_name("resize-hook-noop");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;
    let (_attach_id, _rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(100, 30)).await;

    handler
        .set_global_hook(
            HookName::ClientResized,
            "set-buffer -b client-resized-noop bad",
        )
        .await;

    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    handler
        .handle_attached_resize(101, TerminalSize::new(100, 30))
        .await
        .expect("unchanged client resize succeeds");
    drain_lifecycle_events(&handler, &mut lifecycle_events).await;

    let maybe_content = {
        let state = handler.state.lock().await;
        state
            .buffers
            .show(Some("client-resized-noop"))
            .ok()
            .map(|(_, content)| content.to_vec())
    };
    assert_eq!(
        maybe_content, None,
        "client-resized hook must not run when the client dimensions did not change"
    );
}

#[tokio::test]
async fn window_size_policy_reconciles_attached_client_sizes() {
    let handler = RequestHandler::new();
    let session = session_name("resize-policy");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(120, 40)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(120, 40)),
        "default latest policy should use the most recently attached client"
    );

    let (_small_id, _small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(80, 20)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "latest policy should follow the newest attach"
    );

    handler.set_window_size_policy(&session, 0, "largest").await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(120, 40)),
        "largest policy must select the largest live attached client"
    );

    handler
        .set_window_size_policy(&session, 0, "smallest")
        .await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "smallest policy must select the smallest live attached client"
    );

    handler.set_window_size_policy(&session, 0, "manual").await;
    handler
        .handle_attached_resize(101, TerminalSize::new(140, 45))
        .await
        .expect("manual client resize is accepted");
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "manual policy must not auto-resize the window"
    );

    handler.set_window_size_policy(&session, 0, "latest").await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(140, 45)),
        "latest policy should use the most recently resized client"
    );
}

#[tokio::test]
async fn refresh_client_ignore_size_transitions_reconcile_largest_policy() {
    let handler = RequestHandler::new();
    let session = session_name("refresh-ignore-size-largest");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(120, 40)).await;
    let (_small_id, _small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(80, 20)).await;
    handler.set_window_size_policy(&session, 0, "largest").await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(120, 40))
    );

    let ignored = handler
        .handle(refresh_client_flags_request(101, Some("ignore-size"), None))
        .await;
    assert!(matches!(ignored, Response::RefreshClient(_)), "{ignored:?}");
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "adding ignore-size must immediately remove the client from largest-policy candidates"
    );

    let restored = handler
        .handle(refresh_client_flags_request(
            101,
            None,
            Some("!ignore-size"),
        ))
        .await;
    assert!(
        matches!(restored, Response::RefreshClient(_)),
        "{restored:?}"
    );
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(120, 40)),
        "removing ignore-size through the -F alias must restore the client as a candidate"
    );
}

#[tokio::test]
async fn refresh_client_ignore_size_reconcile_preserves_same_pid_replacement_identity() {
    let handler = RequestHandler::new();
    let alpha = session_name("refresh-ignore-size-race-alpha");
    let beta = session_name("refresh-ignore-size-race-beta");
    SessionSpec::create(&handler, (&alpha, TerminalSize::new(100, 30))).await;
    SessionSpec::create(&handler, (&beta, TerminalSize::new(90, 25))).await;

    let (original_id, _original_rx) =
        register_sized_attach(&handler, 303, &alpha, TerminalSize::new(120, 40)).await;
    let (_small_id, _small_rx) =
        register_sized_attach(&handler, 404, &alpha, TerminalSize::new(80, 20)).await;
    handler.set_window_size_policy(&alpha, 0, "largest").await;

    let pause = handler.install_attached_size_selection_pause();
    let refresh_handler = handler.clone();
    let refresh = tokio::spawn(async move {
        refresh_handler
            .handle(refresh_client_flags_request(303, Some("ignore-size"), None))
            .await
    });
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, pause.reached.notified())
        .await
        .expect("refresh-client reaches identity-safe size selection");

    let (replacement_id, _replacement_rx) =
        register_sized_attach(&handler, 303, &beta, TerminalSize::new(95, 26)).await;
    assert_ne!(replacement_id, original_id);
    pause.release.notify_one();

    assert!(matches!(
        refresh.await.expect("refresh-client task joins"),
        Response::Error(_)
    ));
    assert_eq!(
        attached_session_size(&handler, &alpha).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "the original session must reconcile after its large client is replaced"
    );
    assert_eq!(
        attached_session_size(&handler, &beta).await,
        content_size_for_default_status(TerminalSize::new(95, 26)),
        "the stale refresh must never resize the replacement client's session"
    );
    let active_attach = handler.active_attach.lock().await;
    let replacement = active_attach
        .by_pid
        .get(&303)
        .expect("same-pid replacement survives");
    assert_eq!(replacement.id, replacement_id);
    assert_eq!(replacement.session_name, beta);
    assert!(!replacement.flags.contains(ClientFlags::IGNORESIZE));
}

#[tokio::test]
async fn largest_and_smallest_window_size_policies_compose_dimensions_like_tmux() {
    let handler = RequestHandler::new();
    let session = session_name("resize-policy-dimensions");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_wide_id, _wide_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(120, 20)).await;
    let (_tall_id, _tall_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(80, 50)).await;

    handler.set_window_size_policy(&session, 0, "largest").await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(120, 50)),
        "largest policy must take the maximum width and maximum height independently"
    );

    handler
        .set_window_size_policy(&session, 0, "smallest")
        .await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(80, 20)),
        "smallest policy must take the minimum width and minimum height independently"
    );
}

#[tokio::test]
async fn attach_session_initial_client_size_respects_window_size_policy() {
    let handler = RequestHandler::new();
    let session = session_name("attach-resize-policy");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;
    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;

    handler.set_window_size_policy(&session, 0, "largest").await;
    let outcome = handler
        .dispatch(
            202,
            attach_session_request(&session, TerminalSize::new(72, 18)),
        )
        .await;
    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(outcome.attach.is_some());
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "initial attach must not shrink a largest-policy window below the largest live client"
    );

    handler
        .set_window_size_policy(&session, 0, "smallest")
        .await;
    let outcome = handler
        .dispatch(
            303,
            attach_session_request(&session, TerminalSize::new(72, 18)),
        )
        .await;
    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(outcome.attach.is_some());
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18)),
        "initial attach must be considered by smallest-policy selection"
    );

    let manual = session_name("attach-resize-manual");
    SessionSpec::create(&handler, (&manual, TerminalSize::new(100, 30))).await;
    handler.set_window_size_policy(&manual, 0, "manual").await;
    let outcome = handler
        .dispatch(
            404,
            attach_session_request(&manual, TerminalSize::new(132, 37)),
        )
        .await;
    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(outcome.attach.is_some());
    assert_eq!(
        attached_session_size(&handler, &manual).await,
        TerminalSize::new(100, 30),
        "manual policy must ignore the initial attach client size"
    );
}

#[tokio::test]
async fn latest_window_size_recovers_when_small_client_finishes() {
    let handler = RequestHandler::new();
    let mut events = handler.subscribe_lifecycle_events();
    let session = session_name("resize-finish");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (small_id, _small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(72, 18)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18))
    );

    handler.finish_attach(202, small_id).await;

    wait_for_client_detached_event(&mut events, &attached_client_name(202)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "latest policy must fall back to the remaining latest live client"
    );
}

#[tokio::test]
async fn refresh_prunes_dead_attach_and_recomputes_latest_size() {
    let handler = RequestHandler::new();
    let mut events = handler.subscribe_lifecycle_events();
    let session = session_name("resize-stale");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (_small_id, small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(72, 18)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18))
    );

    drop(small_rx);
    handler.refresh_attached_session(&session).await;

    wait_for_client_detached_event(&mut events, &attached_client_name(202)).await;
    assert!(
        !handler.active_attach.lock().await.by_pid.contains_key(&202),
        "dead attach must be removed before size reconciliation"
    );
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "stale smallest client must not keep the window stuck small"
    );
}

#[tokio::test]
async fn targeted_refresh_prunes_dead_attach_and_recomputes_latest_size() {
    let handler = RequestHandler::new();
    let mut events = handler.subscribe_lifecycle_events();
    let session = session_name("resize-stale-targeted");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (_small_id, small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(72, 18)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18))
    );

    drop(small_rx);
    handler.refresh_attached_client(202, &session).await;

    wait_for_client_detached_event(&mut events, &attached_client_name(202)).await;
    assert!(
        !handler.active_attach.lock().await.by_pid.contains_key(&202),
        "targeted refresh must remove dead attach clients through the shared stale-client path"
    );
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "targeted refresh must not leave latest policy stuck on the stale client size"
    );
}

#[tokio::test]
async fn targeted_base_refresh_prunes_dead_attach_and_recomputes_latest_size() {
    let handler = RequestHandler::new();
    let mut events = handler.subscribe_lifecycle_events();
    let session = session_name("resize-stale-base");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (_small_id, small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(72, 18)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18))
    );

    drop(small_rx);
    handler
        .refresh_attached_client_base_only(202, &session)
        .await;

    wait_for_client_detached_event(&mut events, &attached_client_name(202)).await;
    assert!(
        !handler.active_attach.lock().await.by_pid.contains_key(&202),
        "targeted base refresh must remove dead attach clients through the shared stale-client path"
    );
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "targeted base refresh must not leave latest policy stuck on the stale client size"
    );
}

#[tokio::test]
async fn ignore_size_clients_update_their_render_size_without_resizing_session() {
    let handler = RequestHandler::new();
    let session = session_name("resize-ignore");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (_ignored_id, _ignored_rx) = register_sized_attach_with_flags(
        &handler,
        202,
        &session,
        TerminalSize::new(90, 22),
        ClientFlags::IGNORESIZE,
    )
    .await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "ignore-size attach must not become the latest window-size candidate"
    );

    handler
        .handle_attached_resize(202, TerminalSize::new(72, 18))
        .await
        .expect("ignore-size client resize still updates client metadata");

    {
        let active_attach = handler.active_attach.lock().await;
        let ignored = active_attach
            .by_pid
            .get(&202)
            .expect("ignore-size client remains attached");
        assert_eq!(ignored.client_size, TerminalSize::new(72, 18));
    }
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "ignore-size client resize must not resize the session"
    );
}

#[tokio::test]
async fn read_only_initial_attach_size_is_not_a_window_size_candidate() {
    let handler = RequestHandler::new();
    let session = session_name("attach-readonly-size");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let outcome = handler
        .dispatch(
            202,
            Request::AttachSessionExt2(Box::new(AttachSessionExt2Request {
                read_only: true,
                ..attach_session_ext2(&session, TerminalSize::new(72, 18))
            })),
        )
        .await;
    assert!(matches!(outcome.response, Response::AttachSession(_)));
    assert!(outcome.attach.is_some());
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "read-only implies ignore-size and must not shrink the session during attach"
    );
}

#[tokio::test]
async fn detach_client_recomputes_window_size_before_detached_event() {
    let handler = RequestHandler::new();
    let mut events = handler.subscribe_lifecycle_events();
    let session = session_name("resize-detach-command");
    SessionSpec::create(&handler, (&session, TerminalSize::new(100, 30))).await;

    let (_large_id, _large_rx) =
        register_sized_attach(&handler, 101, &session, TerminalSize::new(160, 40)).await;
    let (_small_id, mut small_rx) =
        register_sized_attach(&handler, 202, &session, TerminalSize::new(72, 18)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(72, 18))
    );

    let response = handler
        .handle(Request::DetachClientExt(DetachClientExtRequest {
            target_client: Some("202".to_owned()),
            all_other_clients: false,
            target_session: None,
            kill_on_detach: false,
            exec_command: None,
        }))
        .await;
    assert_eq!(
        response,
        Response::DetachClient(rmux_proto::DetachClientResponse)
    );
    let _ = recv_matching_attach_control(&mut small_rx, "detach control", |control| {
        matches!(control, AttachControl::Detach)
    })
    .await;
    wait_for_client_detached_event(&mut events, &attached_client_name(202)).await;
    assert_eq!(
        attached_session_size(&handler, &session).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "closing detached clients must be excluded from latest-policy resize selection immediately"
    );
}

#[tokio::test]
async fn aggressive_resize_tracks_only_linked_windows_that_are_current() {
    let handler = RequestHandler::new();
    let alpha = session_name("aggr-alpha");
    let beta = session_name("aggr-beta");
    SessionSpec::create(&handler, (&alpha, TerminalSize::new(100, 30))).await;
    SessionSpec::create(&handler, (&beta, TerminalSize::new(100, 30))).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SelectWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 1),
        },
    )
    .await;
    let (_alpha_id, _alpha_rx) =
        register_sized_attach(&handler, 101, &alpha, TerminalSize::new(160, 40)).await;
    let (_beta_id, _beta_rx) =
        register_sized_attach(&handler, 202, &beta, TerminalSize::new(72, 18)).await;

    handler.set_window_size_policy(&alpha, 0, "smallest").await;
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::AggressiveResize,
            "on",
        )
        .await;
    handler
        .reconcile_attached_session_size_and_emit(&alpha)
        .await
        .expect("aggressive linked current sessions reconcile");
    assert_eq!(
        attached_session_size(&handler, &alpha).await,
        content_size_for_default_status(TerminalSize::new(72, 18)),
        "aggressive-resize must include other sessions where the linked window is current"
    );

    TestRequest::send_ok(
        &handler,
        SelectWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 0),
        },
    )
    .await;
    assert_eq!(
        attached_session_size(&handler, &alpha).await,
        content_size_for_default_status(TerminalSize::new(160, 40)),
        "selecting away from the linked window must recompute affected aggressive-resize sessions"
    );
}

#[tokio::test]
async fn different_requester_pids_reject_ambiguous_cross_process_attach_control() {
    let handler = RequestHandler::new();
    let first_owner_pid = 101;
    let second_owner_pid = 303;
    let intruder_pid = 202;
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let gamma = session_name("gamma");

    for session in [&alpha, &beta, &gamma] {
        SessionSpec::create(&handler, session).await;
    }

    let mut first_rx = handler.attach_client(first_owner_pid, alpha).await;
    let mut second_rx = handler.attach_client(second_owner_pid, beta).await;

    let switched = handler
        .dispatch(
            intruder_pid,
            Request::SwitchClient(SwitchClientRequest { target: gamma }),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Server(
                "switch-client requires an unambiguous attached client".to_owned(),
            ),
        })
    );

    let detached = handler
        .dispatch(
            intruder_pid,
            Request::DetachClient(rmux_proto::DetachClientRequest),
        )
        .await
        .response;
    assert_eq!(
        detached,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Server(
                "detach-client requires an unambiguous attached client".to_owned(),
            ),
        })
    );

    assert!(matches!(first_rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(matches!(second_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn attach_session_without_target_prefers_an_unattached_session() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    let _control_rx = handler.attach_client(101, alpha).await;

    let outcome = handler
        .dispatch(
            202,
            Request::AttachSessionExt(AttachSessionExtRequest {
                target: None,
                detach_other_clients: false,
                kill_other_clients: false,
                read_only: false,
                skip_environment_update: false,
                flags: None,
            }),
        )
        .await;

    assert_eq!(
        outcome.response,
        Response::AttachSession(AttachSessionResponse { session_name: beta })
    );
    assert!(outcome.attach.is_some());
}

#[tokio::test]
async fn attach_session_without_target_prefers_the_most_recent_unattached_session() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    sleep(Duration::from_secs(1)).await;
    let (control_tx, _control_rx) = mpsc::unbounded_channel();
    let attach_id = handler.register_attach(101, beta.clone(), control_tx).await;
    handler.finish_attach(101, attach_id).await;

    let outcome = handler
        .dispatch(
            202,
            Request::AttachSessionExt(AttachSessionExtRequest {
                target: None,
                detach_other_clients: false,
                kill_other_clients: false,
                read_only: false,
                skip_environment_update: false,
                flags: None,
            }),
        )
        .await;

    assert_eq!(
        outcome.response,
        Response::AttachSession(AttachSessionResponse { session_name: beta })
    );
    assert!(outcome.attach.is_some());
}

#[tokio::test]
async fn switch_client_last_session_recalls_the_previous_session() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    let mut control_rx = handler.attach_client(requester_pid, &alpha).await;

    let switched = handler
        .dispatch(
            requester_pid,
            Request::SwitchClientExt2(Box::new(SwitchClientExt2Request {
                target: Some(beta.clone()),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                flags: None,
                sort_order: None,
                skip_environment_update: false,
            })),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );
    let _ = recv_matching_attach_control(&mut control_rx, "switch to beta", |control| {
        matches!(control, AttachControl::Switch(_))
    })
    .await;

    let switched_back = handler
        .dispatch(
            requester_pid,
            Request::SwitchClientExt2(Box::new(SwitchClientExt2Request {
                target: None,
                key_table: None,
                last_session: true,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                flags: None,
                sort_order: None,
                skip_environment_update: false,
            })),
        )
        .await
        .response;
    assert_eq!(
        switched_back,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: alpha,
        })
    );
    let _ = recv_matching_attach_control(&mut control_rx, "switch back to alpha", |control| {
        matches!(control, AttachControl::Switch(_))
    })
    .await;
}

#[tokio::test]
async fn kill_session_clears_attached_last_session_references() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    let mut control_rx = handler.attach_client(requester_pid, &alpha).await;

    let switched = handler
        .dispatch(
            requester_pid,
            Request::SwitchClientExt2(Box::new(SwitchClientExt2Request {
                target: Some(beta.clone()),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                flags: None,
                sort_order: None,
                skip_environment_update: false,
            })),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );
    let _ = recv_matching_attach_control(&mut control_rx, "switch to beta", |control| {
        matches!(control, AttachControl::Switch(_))
    })
    .await;

    {
        let active_attach = handler.active_attach.lock().await;
        assert_eq!(
            active_attach
                .last_session_for_client(requester_pid)
                .expect("attached client exists"),
            Some(alpha.clone())
        );
    }

    let response = handler
        .handle(Request::KillSession(KillSessionRequest::fixture(alpha)))
        .await;
    assert_eq!(
        response,
        Response::KillSession(rmux_proto::KillSessionResponse { existed: true })
    );

    let active_attach = handler.active_attach.lock().await;
    assert_eq!(
        active_attach
            .last_session_for_client(requester_pid)
            .expect("attached client survives on beta"),
        None
    );
}

#[tokio::test]
async fn kill_session_detach_on_destroy_off_switches_every_attached_client() {
    let handler = RequestHandler::new();
    let gamma = session_name("destroy-switch-gamma");
    let beta = session_name("destroy-switch-beta");
    let alpha = session_name("destroy-switch-alpha");
    for session in [&gamma, &beta, &alpha] {
        SessionSpec::create(&handler, session).await;
    }
    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::DetachOnDestroy,
            "off",
        )
        .await;

    let (_, mut first_rx) =
        register_sized_attach(&handler, 91_801, &alpha, TerminalSize::new(90, 30)).await;
    let (_, mut second_rx) =
        register_sized_attach(&handler, 91_802, &alpha, TerminalSize::new(100, 35)).await;
    while first_rx.try_recv().is_ok() {}
    while second_rx.try_recv().is_ok() {}

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&alpha)).await;

    for (receiver, label) in [(&mut first_rx, "first"), (&mut second_rx, "second")] {
        let target = recv_switch_target(receiver, label).await;
        assert_eq!(target.session_name, beta);
    }
    let active_attach = handler.active_attach.lock().await;
    for attach_pid in [91_801, 91_802] {
        let active = active_attach
            .by_pid
            .get(&attach_pid)
            .expect("destroy switch preserves attached client");
        assert_eq!(active.session_name, beta);
        assert_ne!(active.session_id, rmux_proto::SessionId::new(0));
        assert!(!active.closing.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn no_detach_on_destroy_client_flag_overrides_default_detach() {
    let handler = RequestHandler::new();
    let beta = session_name("destroy-flag-beta");
    let alpha = session_name("destroy-flag-alpha");
    for session in [&beta, &alpha] {
        SessionSpec::create(&handler, session).await;
    }
    let (_, mut control_rx) = register_sized_attach_with_flags(
        &handler,
        91_803,
        &alpha,
        TerminalSize::new(90, 30),
        ClientFlags::NO_DETACH_ON_DESTROY,
    )
    .await;
    while control_rx.try_recv().is_ok() {}

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(alpha)).await;
    let target = recv_switch_target(&mut control_rx, "no-detach client flag").await;
    assert_eq!(target.session_name, beta);
}

#[tokio::test]
async fn destroy_switch_target_name_reuse_fails_closed() {
    let handler = RequestHandler::new();
    let gamma = session_name("destroy-reuse-gamma");
    let beta = session_name("destroy-reuse-beta");
    let alpha = session_name("destroy-reuse-alpha");
    for session in [&gamma, &beta, &alpha] {
        SessionSpec::create(&handler, session).await;
    }
    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::DetachOnDestroy,
            "off",
        )
        .await;
    let (_, mut control_rx) =
        register_sized_attach(&handler, 91_804, &alpha, TerminalSize::new(90, 30)).await;
    while control_rx.try_recv().is_ok() {}

    let original_beta_id = handler.session_id_for_test(&beta).await;
    let pause = handler.install_attached_size_selection_pause();
    let kill_handler = handler.clone();
    let kill_alpha = tokio::spawn(async move {
        kill_handler
            .handle(Request::KillSession(KillSessionRequest::fixture(alpha)))
            .await
    });
    pause.reached.notified().await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&beta)).await;
    SessionSpec::create(&handler, &beta).await;
    let replacement_beta_id = handler.session_id_for_test(&beta).await;
    assert_ne!(replacement_beta_id, original_beta_id);

    pause.release.notify_one();
    let response = kill_alpha.await.expect("kill alpha task joins");
    assert!(matches!(response, Response::KillSession(_)), "{response:?}");
    assert!(matches!(
        recv_attach_control(&mut control_rx, "stale destroy target fallback").await,
        AttachControl::Exited
    ));
    assert!(!handler
        .active_attach
        .lock()
        .await
        .by_pid
        .contains_key(&91_804));
}

#[tokio::test]
async fn concurrent_manual_switch_wins_over_destroy_switch() {
    let handler = RequestHandler::new();
    let gamma = session_name("destroy-race-gamma");
    let beta = session_name("destroy-race-beta");
    let alpha = session_name("destroy-race-alpha");
    for session in [&gamma, &beta, &alpha] {
        SessionSpec::create(&handler, session).await;
    }
    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::DetachOnDestroy,
            "off",
        )
        .await;
    let (_, mut control_rx) =
        register_sized_attach(&handler, 91_805, &alpha, TerminalSize::new(90, 30)).await;
    while control_rx.try_recv().is_ok() {}

    let pause = handler.install_attached_size_selection_pause();
    let kill_handler = handler.clone();
    let kill_alpha = tokio::spawn(async move {
        kill_handler
            .handle(Request::KillSession(KillSessionRequest::fixture(alpha)))
            .await
    });
    pause.reached.notified().await;

    let switched = handler
        .dispatch(
            91_805,
            Request::SwitchClientExt2(Box::new(SwitchClientExt2Request {
                target: Some(gamma.clone()),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                flags: None,
                sort_order: None,
                skip_environment_update: false,
            })),
        )
        .await
        .response;
    assert!(
        matches!(switched, Response::SwitchClient(_)),
        "{switched:?}"
    );
    let target = recv_moved_switch_target(&mut control_rx, "manual switch during destroy").await;
    assert_eq!(target.session_name, gamma);

    pause.release.notify_one();
    let response = kill_alpha.await.expect("kill alpha task joins");
    assert!(matches!(response, Response::KillSession(_)), "{response:?}");
    // The losing destroy-switch must deliver nothing. What still arrives is the live pane's own
    // render refresh for the session this client just moved to: an `AttachControl::Switch`
    // carrying its current target, which `is_coalescible_render_switch` identifies. A delivered
    // destroy-switch would be a target *change*, so that is what is refused here.
    while let Ok(control) = control_rx.try_recv() {
        assert!(
            control.is_coalescible_render_switch(),
            "the losing destroy-switch must not reach the client, got {control:?}"
        );
    }
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&91_805)
        .expect("manual switch preserves attached client");
    assert_eq!(active.session_name, gamma);
    assert!(!active.closing.load(Ordering::SeqCst));
}

fn refresh_client_flags_request(
    target_pid: u32,
    flags: Option<&str>,
    flags_alias: Option<&str>,
) -> Request {
    Request::RefreshClient(Box::new(RefreshClientRequest {
        flags: flags.map(str::to_owned),
        flags_alias: flags_alias.map(str::to_owned),
        ..Fixture::fixture(Some(target_pid.to_string()))
    }))
}

async fn attached_session_size(handler: &RequestHandler, session: &SessionName) -> TerminalSize {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session)
        .expect("session exists")
        .window()
        .size()
}

const fn content_size_for_default_status(terminal_size: TerminalSize) -> TerminalSize {
    TerminalSize {
        cols: terminal_size.cols,
        rows: terminal_size.rows.saturating_sub(1),
    }
}

async fn wait_for_client_detached_event(
    events: &mut tokio::sync::broadcast::Receiver<
        super::super::lifecycle_support::QueuedLifecycleEvent,
    >,
    client_name: &str,
) {
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("lifecycle event");
            if matches!(
                event.event,
                rmux_core::LifecycleEvent::ClientDetached { client_name: Some(ref name), .. }
                    if name == client_name
            ) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for client-detached event");
}

async fn drain_lifecycle_events(
    handler: &RequestHandler,
    events: &mut tokio::sync::broadcast::Receiver<
        super::super::lifecycle_support::QueuedLifecycleEvent,
    >,
) {
    loop {
        match events.try_recv() {
            Ok(event) => handler.dispatch_lifecycle_hook(event).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
        }
    }
}
