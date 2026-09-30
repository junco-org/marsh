use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::RequestHandler;
use crate::client_names::control_client_name;
use crate::control::{ControlModeUpgrade, ControlServerEvent, CONTROL_SERVER_EVENT_CAPACITY};
use crate::pane_io::AttachControl;
use rmux_core::LifecycleEvent;
use rmux_proto::{
    ControlMode, DeleteBufferRequest, DetachClientRequest, DisplayMessageExtRequest,
    DisplayMessageRequest, HookLifecycle, HookName, KillSessionRequest, KillWindowRequest,
    NewSessionRequest, NewWindowRequest, RenameSessionRequest, RenameWindowRequest, Request,
    Response, ScopeSelector, SelectWindowRequest, SessionName, SetBufferRequest, SetHookRequest,
    ShowOptionsRequest, SwitchClientRequest, Target, WindowTarget,
};
use tokio::sync::mpsc;

use crate::test_fixtures::{Fixture, Quiet, SessionSpec, TestRequest};
use crate::test_names::session_name;

fn drain_control_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(ControlServerEvent::Notification(line)) => lines.push(line),
            Ok(
                ControlServerEvent::SessionChanged(_)
                | ControlServerEvent::SessionChangedAt { .. }
                | ControlServerEvent::Refresh,
            ) => {}
            Ok(ControlServerEvent::Exit(reason)) => {
                panic!("unexpected control exit: {reason:?}");
            }
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
    lines
}

fn collect_control_events(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<ControlServerEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn full_control_server_event_queue_defers_removal_until_transport_finishes() {
    let handler = RequestHandler::new();
    let requester_pid = 4242;
    let attached_session = SessionSpec::create(&handler, "full-control-event-queue").await;
    let attached_session_id = handler.session_id_for_test(&attached_session).await;
    let (event_tx, mut event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let closing = Arc::new(AtomicBool::new(false));
    let control_id = handler
        .register_control_with_closing(
            requester_pid,
            ControlModeUpgrade {
                initial_command_count: 0,
                mode: ControlMode::Plain,
                terminal_context: crate::outer_terminal::OuterTerminalContext::default(),
            },
            event_tx,
            Arc::clone(&closing),
        )
        .await;
    handler
        .set_control_session(requester_pid, Some(attached_session.clone()))
        .await
        .expect("control session set succeeds");
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ControlServerEvent::SessionChanged(Some(ref session_name)))
            | Ok(ControlServerEvent::SessionChangedAt {
                ref session_name,
                ..
            })
            if session_name == &attached_session
    ));
    let mut lifecycle = handler.subscribe_lifecycle_events();

    for index in 0..CONTROL_SERVER_EVENT_CAPACITY {
        handler
            .send_control_notification_to(requester_pid, format!("%message queued-{index}"))
            .await;
    }

    assert_eq!(event_rx.len(), CONTROL_SERVER_EVENT_CAPACITY);
    assert_eq!(event_rx.max_capacity(), CONTROL_SERVER_EVENT_CAPACITY);
    assert!(!closing.load(Ordering::SeqCst));
    assert!(handler.is_control_client(requester_pid).await);

    handler
        .send_control_notification_to(requester_pid, "%message overflow".to_owned())
        .await;

    assert_eq!(event_rx.len(), CONTROL_SERVER_EVENT_CAPACITY);
    assert!(!event_rx.is_closed());
    assert!(closing.load(Ordering::SeqCst));
    assert!(!handler.is_control_client(requester_pid).await);
    {
        let active_control = handler.active_control.lock().await;
        let active = active_control
            .by_pid
            .get(&requester_pid)
            .expect("closing control stays registered until its transport finishes");
        assert_eq!(active.id, control_id);
        assert_eq!(active.session_id, Some(attached_session_id));
    }
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ControlServerEvent::Notification(line)) if line == "%message queued-0"
    ));
    handler
        .send_control_notification_to(requester_pid, "%message after-closing".to_owned())
        .await;
    assert_eq!(
        event_rx.len(),
        CONTROL_SERVER_EVENT_CAPACITY - 1,
        "closing clients reject later server events even after capacity becomes available"
    );

    handler.finish_control(requester_pid, control_id).await;

    assert!(event_rx.is_closed());
    assert!(!handler
        .active_control
        .lock()
        .await
        .by_pid
        .contains_key(&requester_pid));
    let detached = tokio::time::timeout(Duration::from_secs(1), lifecycle.recv())
        .await
        .expect("transport finish publishes client-detached")
        .expect("lifecycle channel remains open");
    assert_eq!(detached.control_session_identity, Some(attached_session_id));
    assert!(matches!(
        detached.event,
        LifecycleEvent::ClientDetached {
            session_name,
            client_name: Some(client_name),
        } if session_name == attached_session
            && client_name == control_client_name(requester_pid)
    ));
}

async fn dispatch_as(handler: &RequestHandler, requester_pid: u32, request: Request) -> Response {
    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    let outcome = handler.dispatch(requester_pid, request).await;

    handler
        .drain_lifecycle_hooks_for_test(&mut lifecycle_events)
        .await;

    outcome.response
}

/// Returns the `#{client_name}` `list-clients` reports for `pid`.
async fn listed_client_name(handler: &RequestHandler, pid: u32) -> String {
    let response = handler
        .handle(Request::ListClients(Box::new(
            rmux_proto::ListClientsRequest {
                format: Some("#{client_pid}|#{client_name}".to_owned()),
                target_session: None,
                filter: None,
                sort_order: None,
                reversed: false,
            },
        )))
        .await;
    let Response::ListClients(response) = response else {
        panic!("expected list-clients response");
    };
    let prefix = format!("{pid}|");
    String::from_utf8(response.output.stdout().to_vec())
        .expect("list-clients output is utf-8")
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .expect("list-clients reports the client")
}

async fn prepared_client_session_changed(
    handler: &RequestHandler,
    session_name: SessionName,
    session_id: rmux_proto::SessionId,
    client_name: &str,
) -> super::QueuedLifecycleEvent {
    let mut events = handler.subscribe_lifecycle_events();
    handler
        .emit_for_session_identity(
            LifecycleEvent::ClientSessionChanged {
                session_name,
                client_name: Some(client_name.to_owned()),
            },
            session_id,
        )
        .await;
    events
        .recv()
        .await
        .expect("exact client-session-changed event queued")
}

#[tokio::test]
async fn control_switch_client_sends_self_and_other_session_notifications() {
    let handler = RequestHandler::new();
    // Quiet: a real shell's first prompt would rename a window automatically, into these drains.
    let alpha = SessionSpec::create(&handler, Quiet("alpha")).await;
    let beta = SessionSpec::create(&handler, Quiet("beta")).await;

    let (_, mut self_rx) = handler
        .register_utf8_control_for_test(101, Some(&alpha))
        .await;
    let (_, mut other_rx) = handler
        .register_utf8_control_for_test(202, Some(&alpha))
        .await;
    let (_, mut detached_rx) = handler.register_utf8_control_for_test(303, None).await;
    let _ = drain_control_notifications(&mut self_rx);
    let _ = drain_control_notifications(&mut other_rx);
    let _ = drain_control_notifications(&mut detached_rx);

    let response = dispatch_as(
        &handler,
        101,
        Request::SwitchClient(SwitchClientRequest {
            target: beta.clone(),
        }),
    )
    .await;

    assert_eq!(
        response,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );

    let beta_id = handler.session_id_for_test(&beta).await.as_u32();
    assert_eq!(
        drain_control_notifications(&mut self_rx),
        vec![format!("%session-changed ${beta_id} {beta}")]
    );
    assert_eq!(
        drain_control_notifications(&mut other_rx),
        vec![format!(
            "%client-session-changed client-101 ${beta_id} {beta}"
        )]
    );
    assert!(drain_control_notifications(&mut detached_rx).is_empty());
}

/// Frozen tmux 3.7b, measured 2026-07-25 with two `-C` clients (pids
/// 74711/74712) on session `alpha`: after 74711 runs `switch-client -t beta`
/// and then detaches, the surviving client is told
///
///     %client-session-changed client-74711 $1 beta
///     %client-detached client-74711
///
/// The token is the same name `list-clients -F '#{client_name}'` reports for
/// that client. A frontend keys its client table on that name, so the two
/// surfaces must never spell the same client differently.
#[tokio::test]
async fn control_notifications_name_clients_the_way_list_clients_does() {
    let handler = RequestHandler::new();
    // Quiet: a real shell's first prompt would rename a window automatically, into these drains.
    let alpha = SessionSpec::create(&handler, Quiet("alpha")).await;
    let beta = SessionSpec::create(&handler, Quiet("beta")).await;

    let switching_pid = 74_711;
    let (_, mut switching_rx) = handler
        .register_utf8_control_for_test(switching_pid, Some(&alpha))
        .await;
    let (_, mut watching_rx) = handler
        .register_utf8_control_for_test(74_712, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut switching_rx);
    let _ = drain_control_notifications(&mut watching_rx);

    let listed = listed_client_name(&handler, switching_pid).await;
    assert_eq!(listed, format!("client-{switching_pid}"));

    let response = dispatch_as(
        &handler,
        switching_pid,
        Request::SwitchClient(SwitchClientRequest {
            target: beta.clone(),
        }),
    )
    .await;
    assert_eq!(
        response,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );

    let beta_id = handler.session_id_for_test(&beta).await.as_u32();
    assert_eq!(
        drain_control_notifications(&mut watching_rx),
        vec![format!(
            "%client-session-changed {listed} ${beta_id} {beta}"
        )]
    );
    assert_eq!(
        drain_control_notifications(&mut switching_rx),
        vec![format!("%session-changed ${beta_id} {beta}")],
        "the switching client still recognises its own move"
    );

    let response = dispatch_as(
        &handler,
        switching_pid,
        Request::DetachClient(DetachClientRequest),
    )
    .await;
    assert_eq!(
        response,
        Response::DetachClient(rmux_proto::DetachClientResponse)
    );
    assert_eq!(
        drain_control_notifications(&mut watching_rx),
        vec![format!("%client-detached {listed}")]
    );
}

#[tokio::test]
async fn control_window_notifications_follow_each_clients_session_visibility() {
    let handler = RequestHandler::new();
    // Quiet: a real shell's first prompt would rename @0/@1 automatically, into these drains.
    let alpha = SessionSpec::create(&handler, Quiet("alpha")).await;
    let beta = SessionSpec::create(&handler, Quiet("beta")).await;

    let (_, mut alpha_rx) = handler
        .register_utf8_control_for_test(410, Some(&alpha))
        .await;
    let (_, mut beta_rx) = handler
        .register_utf8_control_for_test(420, Some(&beta))
        .await;
    let _ = drain_control_notifications(&mut alpha_rx);
    let _ = drain_control_notifications(&mut beta_rx);

    let target = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let window_id = handler.window_id_for_test(&target).await.as_u32();

    assert_eq!(
        drain_control_notifications(&mut alpha_rx),
        vec![format!("%window-add @{window_id}")]
    );
    assert_eq!(
        drain_control_notifications(&mut beta_rx),
        vec![format!("%unlinked-window-add @{window_id}")]
    );

    TestRequest::send_ok(
        &handler,
        RenameWindowRequest {
            target: target.clone(),
            name: "build".to_owned(),
        },
    )
    .await;

    assert_eq!(
        drain_control_notifications(&mut alpha_rx),
        vec![format!("%window-renamed @{window_id} build")]
    );
    assert_eq!(
        drain_control_notifications(&mut beta_rx),
        vec![format!("%unlinked-window-renamed @{window_id} build")]
    );

    TestRequest::send_ok(
        &handler,
        RenameWindowRequest {
            target: target.clone(),
            name: "bad\n%output %1 injected".to_owned(),
        },
    )
    .await;

    assert_eq!(
        drain_control_notifications(&mut alpha_rx),
        vec![format!(
            "%window-renamed @{window_id} bad\\012%output %1 injected"
        )]
    );
    assert_eq!(
        drain_control_notifications(&mut beta_rx),
        vec![format!(
            "%unlinked-window-renamed @{window_id} bad\\012%output %1 injected"
        )]
    );
}

#[tokio::test]
async fn window_close_notifications_follow_each_clients_session_visibility() {
    let handler = RequestHandler::new();
    // Quiet: a real shell's first prompt would rename @0/@1 automatically, into this drain.
    let alpha = SessionSpec::create(&handler, Quiet("alpha")).await;
    let beta = SessionSpec::create(&handler, Quiet("beta")).await;

    let (_, mut alpha_rx) = handler
        .register_utf8_control_for_test(430, Some(&alpha))
        .await;
    let (_, mut beta_rx) = handler
        .register_utf8_control_for_test(440, Some(&beta))
        .await;
    let _ = drain_control_notifications(&mut alpha_rx);
    let _ = drain_control_notifications(&mut beta_rx);

    let target = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let window_id = handler.window_id_for_test(&target).await.as_u32();
    let _ = drain_control_notifications(&mut alpha_rx);
    let _ = drain_control_notifications(&mut beta_rx);

    TestRequest::send_ok(&handler, KillWindowRequest::fixture(target)).await;

    assert_eq!(
        drain_control_notifications(&mut alpha_rx),
        vec![format!("%unlinked-window-close @{window_id}")]
    );
    assert_eq!(
        drain_control_notifications(&mut beta_rx),
        vec![format!("%unlinked-window-close @{window_id}")]
    );
}

#[tokio::test]
async fn killing_the_only_window_notifies_surviving_control_in_tmux_order() {
    let handler = RequestHandler::new();
    // Quiet: a real shell's first prompt would rename @1 automatically, into this drain.
    let alpha = SessionSpec::create(&handler, Quiet("alpha")).await;
    let beta = SessionSpec::create(&handler, Quiet("beta")).await;

    let alpha_window_id = handler
        .window_id_for_test(&WindowTarget::new(alpha.clone()))
        .await
        .as_u32();

    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(450, Some(&beta))
        .await;
    let _ = drain_control_notifications(&mut control_rx);

    TestRequest::send_ok(
        &handler,
        KillWindowRequest::fixture(WindowTarget::with_window(alpha.clone(), 0)),
    )
    .await;
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&alpha)
        .is_none());
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec![
            format!("%unlinked-window-close @{alpha_window_id}"),
            "%sessions-changed".to_owned(),
        ]
    );
}

#[tokio::test]
async fn paste_buffer_notifications_use_the_buffer_name() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(510, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);

    TestRequest::send_ok(&handler, SetBufferRequest::fixture(("named", b"hello"))).await;
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%paste-buffer-changed named".to_owned()]
    );

    TestRequest::send_ok(
        &handler,
        DeleteBufferRequest {
            name: Some("named".to_owned()),
        },
    )
    .await;
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%paste-buffer-deleted named".to_owned()]
    );

    TestRequest::send_ok(&handler, SetBufferRequest::fixture(("bad\nname", b"hello"))).await;
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%paste-buffer-changed bad\\012name".to_owned()]
    );

    TestRequest::send_ok(
        &handler,
        DeleteBufferRequest {
            name: Some("bad\nname".to_owned()),
        },
    )
    .await;
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%paste-buffer-deleted bad\\012name".to_owned()]
    );
}

#[tokio::test]
async fn sessions_changed_notifications_reach_control_clients_with_and_without_sessions() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;
    let beta = session_name("beta");

    let (_, mut attached_rx) = handler
        .register_utf8_control_for_test(520, Some(&alpha))
        .await;
    let (_, mut detached_rx) = handler.register_utf8_control_for_test(530, None).await;
    let _ = drain_control_notifications(&mut attached_rx);
    let _ = drain_control_notifications(&mut detached_rx);

    SessionSpec::create(&handler, &beta).await;
    let beta_window_id = handler
        .window_id_for_test(&WindowTarget::new(beta.clone()))
        .await
        .as_u32();
    assert_eq!(
        drain_control_notifications(&mut attached_rx),
        vec![
            format!("%unlinked-window-add @{beta_window_id}"),
            "%sessions-changed".to_owned(),
        ]
    );
    assert_eq!(
        drain_control_notifications(&mut detached_rx),
        vec!["%sessions-changed".to_owned()]
    );

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(beta)).await;
    assert_eq!(
        drain_control_notifications(&mut attached_rx),
        vec![
            "%sessions-changed".to_owned(),
            format!("%unlinked-window-close @{beta_window_id}")
        ]
    );
    assert_eq!(
        drain_control_notifications(&mut detached_rx),
        vec!["%sessions-changed".to_owned()]
    );
}

#[tokio::test]
async fn session_renamed_notifications_include_session_id_and_new_name() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;
    let beta = session_name("beta");

    let (_, mut attached_rx) = handler
        .register_utf8_control_for_test(540, Some(&alpha))
        .await;
    let (_, mut detached_rx) = handler.register_utf8_control_for_test(550, None).await;
    let _ = drain_control_notifications(&mut attached_rx);
    let _ = drain_control_notifications(&mut detached_rx);

    let alpha_id = handler.session_id_for_test(&alpha).await.as_u32();
    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: alpha,
            new_name: beta.clone(),
        },
    )
    .await;

    let expected = vec![format!("%session-renamed ${alpha_id} {beta}")];
    assert_eq!(drain_control_notifications(&mut attached_rx), expected);
    assert_eq!(
        drain_control_notifications(&mut detached_rx),
        vec![format!("%session-renamed ${alpha_id} {beta}")]
    );
}

#[tokio::test]
async fn session_window_changed_notifications_are_broadcast_to_all_control_clients() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let target = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let window_id = handler.window_id_for_test(&target).await.as_u32();
    let session_id = handler.session_id_for_test(&alpha).await.as_u32();

    let (_, mut attached_rx) = handler
        .register_utf8_control_for_test(560, Some(&alpha))
        .await;
    let (_, mut detached_rx) = handler.register_utf8_control_for_test(570, None).await;
    let _ = drain_control_notifications(&mut attached_rx);
    let _ = drain_control_notifications(&mut detached_rx);

    TestRequest::send_ok(&handler, SelectWindowRequest { target }).await;

    let expected = vec![format!(
        "%session-window-changed ${session_id} @{window_id}"
    )];
    assert_eq!(drain_control_notifications(&mut attached_rx), expected);
    assert_eq!(
        drain_control_notifications(&mut detached_rx),
        vec![format!(
            "%session-window-changed ${session_id} @{window_id}"
        )]
    );
}

#[tokio::test]
async fn detached_control_clients_skip_session_scoped_window_notifications() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let (_, mut attached_rx) = handler
        .register_utf8_control_for_test(580, Some(&alpha))
        .await;
    let (_, mut detached_rx) = handler.register_utf8_control_for_test(590, None).await;
    let _ = drain_control_notifications(&mut attached_rx);
    let _ = drain_control_notifications(&mut detached_rx);

    let target = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let window_id = handler.window_id_for_test(&target).await.as_u32();

    assert_eq!(
        drain_control_notifications(&mut attached_rx),
        vec![format!("%window-add @{window_id}")]
    );
    assert!(drain_control_notifications(&mut detached_rx).is_empty());
}

#[tokio::test]
async fn display_message_for_control_client_uses_message_notification() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;
    let detached = SessionSpec::create(&handler, "detached").await;

    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(610, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);

    let response = dispatch_as(
        &handler,
        610,
        Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Session(alpha.clone())),
            print: false,
            ..Fixture::fixture(
                "hello\t#{session_name}|#{client_session}|#{client_name}|#{client_width}|#{client_height}",
            )
        }),
    )
    .await;

    assert_eq!(
        response,
        Response::DisplayMessage(rmux_proto::DisplayMessageResponse::no_output())
    );
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message hello\\talpha|alpha|client-610|80|".to_owned()]
    );

    let response = dispatch_as(
        &handler,
        610,
        Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Session(detached.clone())),
            print: false,
            ..Fixture::fixture(
                "#{session_name}|#{client_session}|#{client_name}|#{client_width}|#{client_height}",
            )
        }),
    )
    .await;
    assert!(matches!(response, Response::DisplayMessage(_)));
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message detached|alpha|client-610|80|".to_owned()]
    );

    let response = dispatch_as(
        &handler,
        99_610,
        Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Session(alpha)),
            print: false,
            ..Fixture::fixture("external #{client_session}|#{client_name}")
        }),
    )
    .await;
    assert!(matches!(response, Response::DisplayMessage(_)));
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message external alpha|client-610".to_owned()]
    );

    let response = dispatch_as(
        &handler,
        99_611,
        Request::DisplayMessageExt(Box::new(DisplayMessageExtRequest {
            target: Some(Target::Session(detached.clone())),
            print: true,
            target_client: Some("610".to_owned()),
            ..Fixture::fixture(
                "#{session_name}|#{client_session}|#{client_name}|#{client_width}|#{client_height}",
            )
        })),
    )
    .await;
    assert_eq!(
        response.command_output().map(|output| output.stdout()),
        Some(b"detached|alpha|client-610|80|\n".as_slice())
    );

    let response = dispatch_as(
        &handler,
        99_612,
        Request::DisplayMessageExt(Box::new(DisplayMessageExtRequest {
            target: Some(Target::Session(detached.clone())),
            target_client: Some("610".to_owned()),
            ..Fixture::fixture("targeted #{client_session}|#{client_name}")
        })),
    )
    .await;
    assert!(matches!(response, Response::DisplayMessage(_)));
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message targeted alpha|client-610".to_owned()]
    );

    let (_, mut second_alpha_rx) = handler
        .register_utf8_control_for_test(611, Some(&session_name("alpha")))
        .await;
    let (_, mut beta_rx) = handler
        .register_utf8_control_for_test(612, Some(&detached))
        .await;
    let _ = drain_control_notifications(&mut second_alpha_rx);
    let _ = drain_control_notifications(&mut beta_rx);
    let pane_id = handler
        .state
        .lock()
        .await
        .sessions
        .session(&session_name("alpha"))
        .and_then(rmux_core::Session::active_pane_id)
        .expect("alpha active pane");
    let response = handler
        .handle_display_message_for_stable_pane(
            99_613,
            pane_id,
            DisplayMessageRequest {
                target: Some(Target::Session(session_name("alpha"))),
                print: false,
                ..Fixture::fixture("stable #{client_session}|#{client_name}")
            },
        )
        .await;
    assert!(matches!(response, Response::DisplayMessage(_)));
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message stable alpha|client-611".to_owned()]
    );
    assert_eq!(
        drain_control_notifications(&mut second_alpha_rx),
        vec!["%message stable alpha|client-611".to_owned()]
    );
    assert!(drain_control_notifications(&mut beta_rx).is_empty());
}

#[tokio::test]
async fn rejected_control_display_message_is_not_added_to_show_messages() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "full-display-message-queue").await;

    let requester_pid = 620;
    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(requester_pid, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);
    for index in 0..CONTROL_SERVER_EVENT_CAPACITY {
        handler
            .send_control_notification_to(requester_pid, format!("%message queued-{index}"))
            .await;
    }
    assert_eq!(control_rx.len(), CONTROL_SERVER_EVENT_CAPACITY);

    let response = dispatch_as(
        &handler,
        99_620,
        Request::DisplayMessageExt(Box::new(DisplayMessageExtRequest {
            target: Some(Target::Session(alpha)),
            target_client: Some(requester_pid.to_string()),
            ..Fixture::fixture("must-not-be-logged")
        })),
    )
    .await;

    assert!(matches!(response, Response::DisplayMessage(_)));
    assert!(handler.state.lock().await.message_log.is_empty());
}

#[tokio::test]
async fn rejected_session_control_message_is_not_added_to_show_messages() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "full-session-display-queue").await;

    let requester_pid = 621;
    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(requester_pid, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);
    for index in 0..CONTROL_SERVER_EVENT_CAPACITY {
        handler
            .send_control_notification_to(requester_pid, format!("%message queued-{index}"))
            .await;
    }
    let pane_id = handler
        .state
        .lock()
        .await
        .sessions
        .session(&alpha)
        .and_then(rmux_core::Session::active_pane_id)
        .expect("active pane");

    let response = handler
        .handle_display_message_for_stable_pane(
            99_621,
            pane_id,
            DisplayMessageRequest {
                target: Some(Target::Session(alpha)),
                print: false,
                ..Fixture::fixture("must-not-be-logged")
            },
        )
        .await;

    assert!(matches!(response, Response::DisplayMessage(_)));
    assert!(handler.state.lock().await.message_log.is_empty());
}

#[tokio::test]
async fn display_message_orders_attached_and_control_clients_by_tmux_activity_semantics() {
    // tmux 3.7b uses registration as the control client's initial activity,
    // but commands read from control mode do not update client_activity.
    // Later accepted attached input therefore keeps the attached client ahead.
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let attach_pid = 620;
    let mut attach_rx = handler.attach_client(attach_pid, &alpha).await;
    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(621, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);

    let request = || {
        Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Session(alpha.clone())),
            print: false,
            ..Fixture::fixture("activity")
        })
    };
    assert!(matches!(
        dispatch_as(&handler, 99_620, request()).await,
        Response::DisplayMessage(_)
    ));
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%message activity".to_owned()]
    );
    assert!(attach_rx.try_recv().is_err());

    let activity_sequence = handler.next_client_activity_sequence();
    assert!(handler
        .active_attach
        .lock()
        .await
        .record_client_activity(attach_pid, activity_sequence));
    assert!(matches!(
        dispatch_as(&handler, 99_621, request()).await,
        Response::DisplayMessage(_)
    ));
    assert!(drain_control_notifications(&mut control_rx).is_empty());
    assert!(matches!(
        attach_rx.recv().await,
        Some(AttachControl::Overlay(_))
    ));

    let control_id = handler
        .active_control
        .lock()
        .await
        .by_pid
        .get(&621)
        .expect("control client remains active")
        .id;
    let commands = handler
        .parse_control_commands("display-message -p control-activity")
        .await
        .expect("control command parses");
    let result = handler
        .execute_control_commands_identity(621, control_id, commands)
        .await;
    assert!(result.error.is_none(), "{:?}", result.error);
    assert!(matches!(
        dispatch_as(&handler, 99_622, request()).await,
        Response::DisplayMessage(_)
    ));
    assert!(drain_control_notifications(&mut control_rx).is_empty());
    assert!(matches!(
        attach_rx.recv().await,
        Some(AttachControl::Overlay(_))
    ));
}

#[tokio::test]
async fn startup_config_errors_are_queued_as_percent_config_error_notifications() {
    let handler = RequestHandler::new();
    handler
        .startup_config_errors
        .lock()
        .await
        .push(rmux_proto::RmuxError::Server(
            "first startup error\nsecond startup error".to_owned(),
        ));

    let (_, mut control_rx) = handler.register_utf8_control_for_test(710, None).await;

    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec![
            "%config-error first startup error".to_owned(),
            "%config-error second startup error".to_owned(),
        ]
    );
}

#[tokio::test]
async fn startup_config_errors_do_not_block_first_regular_command() {
    let handler = RequestHandler::new();
    handler
        .startup_config_errors
        .lock()
        .await
        .push(rmux_proto::RmuxError::Server(
            "startup config failed".to_owned(),
        ));

    let response = dispatch_as(
        &handler,
        711,
        Request::NewSession(NewSessionRequest::fixture("alpha")),
    )
    .await;

    assert!(matches!(response, Response::NewSession(_)));

    let (_, mut control_rx) = handler.register_utf8_control_for_test(711, None).await;
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec!["%config-error startup config failed".to_owned()]
    );
}

#[tokio::test]
async fn control_detach_exits_self_and_notifies_other_controls() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let (_, mut self_rx) = handler
        .register_utf8_control_for_test(810, Some(&alpha))
        .await;
    let (_, mut other_rx) = handler
        .register_utf8_control_for_test(820, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut self_rx);
    let _ = drain_control_notifications(&mut other_rx);

    let response = dispatch_as(&handler, 810, Request::DetachClient(DetachClientRequest)).await;
    assert_eq!(
        response,
        Response::DetachClient(rmux_proto::DetachClientResponse)
    );

    let self_events = collect_control_events(&mut self_rx);
    assert_eq!(self_events.len(), 1, "{self_events:?}");
    assert!(matches!(self_events[0], ControlServerEvent::Exit(None)));
    assert_eq!(
        drain_control_notifications(&mut other_rx),
        vec!["%client-detached client-810".to_owned()]
    );
}

#[tokio::test]
async fn hook_commands_emit_distinct_lifecycle_control_notifications() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, "alpha").await;

    let (_, mut control_rx) = handler
        .register_utf8_control_for_test(910, Some(&alpha))
        .await;
    let _ = drain_control_notifications(&mut control_rx);

    TestRequest::send_ok(
        &handler,
        SetHookRequest {
            lifecycle: HookLifecycle::OneShot,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::AfterShowOptions,
                "new-session -d -s beta",
            ))
        },
    )
    .await;

    let response = handler
        .handle(Request::ShowOptions(ShowOptionsRequest {
            scope: rmux_proto::OptionScopeSelector::SessionGlobal,
            name: None,
            value_only: false,
            include_inherited: true,
            quiet: false,
            include_hooks: false,
        }))
        .await;
    assert!(matches!(response, Response::ShowOptions(_)));
    let beta_window_id = handler
        .window_id_for_test(&WindowTarget::new(session_name("beta")))
        .await
        .as_u32();
    assert_eq!(
        drain_control_notifications(&mut control_rx),
        vec![
            format!("%unlinked-window-add @{beta_window_id}"),
            "%sessions-changed".to_owned(),
        ]
    );

    let has_beta = handler
        .handle(Request::HasSession(rmux_proto::HasSessionRequest {
            target: session_name("beta"),
        }))
        .await;
    assert_eq!(
        has_beta,
        Response::HasSession(rmux_proto::HasSessionResponse { exists: true })
    );
}

#[tokio::test]
async fn exact_client_attached_event_follows_rename_and_name_reuse_by_session_id() {
    let handler = RequestHandler::new();
    let original = SessionSpec::create(&handler, "client-attached-original").await;
    let renamed = session_name("client-attached-renamed");
    let original_id = handler.session_id_for_test(&original).await;

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: original.clone(),
            new_name: renamed.clone(),
        },
    )
    .await;
    SessionSpec::create(&handler, &original).await;

    let mut events = handler.subscribe_lifecycle_events();
    handler
        .emit_client_attached_identity(control_client_name(9_901), original, original_id)
        .await;
    let queued = events
        .recv()
        .await
        .expect("exact client-attached event queued");
    assert_eq!(queued.control_session_identity, Some(original_id));
    assert!(matches!(
        queued.event,
        LifecycleEvent::ClientAttached { session_name, .. } if session_name == renamed
    ));
}

#[tokio::test]
async fn client_session_changed_notification_follows_rename_not_reused_name() {
    let handler = RequestHandler::new();
    let original = SessionSpec::create(&handler, "notify-session-original").await;
    let renamed = session_name("notify-session-renamed");
    let observer = SessionSpec::create(&handler, "notify-session-observer").await;
    let original_id = handler.session_id_for_test(&original).await;
    let (_, mut observer_rx) = handler
        .register_utf8_control_for_test(9_903, Some(&observer))
        .await;
    let _ = drain_control_notifications(&mut observer_rx);

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: original.clone(),
            new_name: renamed.clone(),
        },
    )
    .await;
    SessionSpec::create(&handler, &original).await;
    let _ = drain_control_notifications(&mut observer_rx);

    handler
        .emit_client_session_changed(control_client_name(9_902), original, original_id)
        .await;
    assert_eq!(
        drain_control_notifications(&mut observer_rx),
        vec![format!(
            "%client-session-changed client-9902 ${} {renamed}",
            original_id.as_u32()
        )]
    );
}

#[tokio::test]
async fn deactivated_lifecycle_dispatch_still_delivers_control_effects() {
    let handler = RequestHandler::new();
    let attached = session_name("notify-after-lifecycle-shutdown");
    let observer = session_name("notify-after-lifecycle-shutdown-observer");
    SessionSpec::create(&handler, &attached).await;
    SessionSpec::create(&handler, &observer).await;
    let attached_id = handler.session_id_for_test(&attached).await;
    let queued = {
        let mut state = handler.state.lock().await;
        let mut queued = super::prepare_lifecycle_event(
            &mut state,
            &LifecycleEvent::ClientSessionChanged {
                session_name: attached.clone(),
                client_name: Some(control_client_name(9_910)),
            },
        );
        queued.control_session_identity = Some(attached_id);
        queued
    };

    let lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test activates the lifecycle queue once");
    handler.lifecycle_dispatch.deactivate();
    drop(lifecycle_events);
    let (_, mut observer_rx) = handler
        .register_utf8_control_for_test(9_911, Some(&observer))
        .await;
    let _ = drain_control_notifications(&mut observer_rx);

    handler.emit_prepared(queued.clone()).await;

    let expected = vec![format!(
        "%client-session-changed client-9910 ${} {attached}",
        attached_id.as_u32()
    )];
    assert_eq!(drain_control_notifications(&mut observer_rx), expected);

    handler.emit_prepared_and_wait(queued).await;
    assert_eq!(drain_control_notifications(&mut observer_rx), expected);
}

#[tokio::test]
async fn hooks_disabled_client_session_changed_skips_deleted_reused_session() {
    let handler = RequestHandler::new();
    let replaced = session_name("notify-session-replaced");
    let observer = session_name("notify-session-disabled-observer");
    SessionSpec::create(&handler, &replaced).await;
    SessionSpec::create(&handler, &observer).await;
    let replaced_id = handler.session_id_for_test(&replaced).await;
    let queued =
        prepared_client_session_changed(&handler, replaced.clone(), replaced_id, "9904").await;
    let (_, mut observer_rx) = handler
        .register_utf8_control_for_test(9_905, Some(&observer))
        .await;
    let _ = drain_control_notifications(&mut observer_rx);

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&replaced)).await;
    SessionSpec::create(&handler, &replaced).await;
    let _ = drain_control_notifications(&mut observer_rx);

    crate::hook_runtime::with_hook_execution(
        crate::hook_runtime::HookExecutionContext::lifecycle(
            rmux_proto::HookName::ClientSessionChanged,
        ),
        Vec::new(),
        async {
            handler.emit_prepared(queued).await;
        },
    )
    .await;
    assert!(drain_control_notifications(&mut observer_rx).is_empty());
}

#[tokio::test]
async fn control_notification_delivery_cannot_jump_to_reused_pid_registration() {
    let handler = RequestHandler::new();
    let requester_pid = 9_906;
    let (old_id, mut old_rx) = handler.register_control_for_test(requester_pid, None).await;
    let queued = {
        let mut state = handler.state.lock().await;
        super::prepare_lifecycle_event(
            &mut state,
            &LifecycleEvent::PasteBufferChanged {
                buffer_name: "recipient-aba".to_owned(),
            },
        )
    };
    let pause = handler.install_control_notification_delivery_pause();
    let dispatch_handler = handler.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_handler
            .dispatch_control_notifications(&queued)
            .await;
    });
    pause.reached.notified().await;

    let replacement_handler = handler.clone();
    let replacement = tokio::spawn(async move {
        replacement_handler
            .register_control_for_test(requester_pid, None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !replacement.is_finished(),
        "replacement registration waits for the identity-locked delivery"
    );

    pause.release.notify_one();
    dispatch.await.expect("notification dispatch completes");
    let (replacement_id, mut replacement_rx) = replacement
        .await
        .expect("replacement registration completes");
    assert_ne!(replacement_id, old_id);
    assert!(collect_control_events(&mut old_rx).iter().any(|event| {
        matches!(
            event,
            ControlServerEvent::Notification(line)
                if line == "%paste-buffer-changed recipient-aba"
        )
    }));
    assert!(drain_control_notifications(&mut replacement_rx).is_empty());
}
