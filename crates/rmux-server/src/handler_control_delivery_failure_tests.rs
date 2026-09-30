use crate::client_names::control_client_name;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmux_core::LifecycleEvent;
use rmux_proto::{
    ControlMode, KillSessionRequest, OptionName, RenameSessionRequest, ScopeSelector, SessionId,
    SessionName,
};
use tokio::sync::mpsc;

use super::{ControlClientIdentity, RequestHandler};
use crate::control::{ControlModeUpgrade, ControlServerEvent};
use crate::outer_terminal::OuterTerminalContext;
use crate::test_fixtures::{Fixture, SessionSpec, TestRequest};
use crate::test_names::session_name;

struct AttachedControl {
    pid: u32,
    control_id: u64,
    session_id: SessionId,
    closing: Arc<AtomicBool>,
    events: mpsc::Receiver<ControlServerEvent>,
}

async fn register_attached_control(
    handler: &RequestHandler,
    pid: u32,
    name: SessionName,
) -> AttachedControl {
    let session_id = handler.session_id_for_test(&name).await;
    let (event_tx, mut events) = mpsc::channel(1);
    let closing = Arc::new(AtomicBool::new(false));
    let control_id = handler
        .register_control_with_closing(
            pid,
            ControlModeUpgrade {
                initial_command_count: 0,
                mode: ControlMode::Plain,
                terminal_context: OuterTerminalContext::default(),
            },
            event_tx,
            Arc::clone(&closing),
        )
        .await;
    handler
        .set_control_session_identity(pid, name.clone(), session_id)
        .await
        .expect("control session set succeeds");
    assert!(matches!(
        events.try_recv(),
        Ok(ControlServerEvent::SessionChanged(Some(_))
            | ControlServerEvent::SessionChangedAt { .. })
    ));
    AttachedControl {
        pid,
        control_id,
        session_id,
        closing,
        events,
    }
}

#[tokio::test]
async fn refresh_delivery_failures_keep_exact_control_identities_until_finish() {
    let handler = RequestHandler::new();
    let by_name = session_name("refresh-failure-by-name");
    let by_session_id = session_name("refresh-failure-by-session-id");
    let by_client_id = session_name("refresh-failure-by-client-id");
    SessionSpec::create(&handler, &by_name).await;
    SessionSpec::create(&handler, &by_session_id).await;
    SessionSpec::create(&handler, &by_client_id).await;

    let mut controls = vec![
        register_attached_control(&handler, 43_001, by_name.clone()).await,
        register_attached_control(&handler, 43_002, by_session_id.clone()).await,
        register_attached_control(&handler, 43_003, by_client_id).await,
    ];
    for control in &mut controls {
        control.events.close();
    }

    handler.refresh_control_session(&by_name).await;
    handler
        .refresh_control_session_for_session_identity(&by_session_id, controls[1].session_id)
        .await;
    let exact_identity = ControlClientIdentity::new(controls[2].pid, controls[2].control_id);
    assert!(handler
        .refresh_control_client_for_identity(exact_identity)
        .await
        .is_err());

    for control in &controls {
        let active_control = handler.active_control.lock().await;
        let active = active_control
            .by_pid
            .get(&control.pid)
            .expect("refresh failure retains the exact control identity");
        assert_eq!(active.id, control.control_id);
        assert!(active.closing.load(Ordering::SeqCst));
        drop(active_control);
        handler
            .finish_control(control.pid, control.control_id)
            .await;
    }
    assert!(handler.active_control.lock().await.by_pid.is_empty());
}

#[tokio::test]
async fn failed_rename_delivery_tracks_the_committed_stable_session_until_finish() {
    let handler = RequestHandler::new();
    let original = session_name("rename-failure-original");
    let renamed = session_name("rename-failure-renamed");
    SessionSpec::create(&handler, &original).await;
    let session_id = handler.session_id_for_test(&original).await;
    let mut control = register_attached_control(&handler, 43_010, original.clone()).await;
    control.events.close();

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: original,
            new_name: renamed.clone(),
        },
    )
    .await;
    assert!(control.closing.load(Ordering::SeqCst));
    {
        let active_control = handler.active_control.lock().await;
        let active = active_control
            .by_pid
            .get(&control.pid)
            .expect("renamed closing control remains registered");
        assert_eq!(active.id, control.control_id);
        assert_eq!(active.session_name.as_ref(), Some(&renamed));
        assert_eq!(active.session_id, Some(session_id));
    }

    let mut lifecycle = handler.subscribe_lifecycle_events();
    handler
        .finish_control(control.pid, control.control_id)
        .await;
    let detached = tokio::time::timeout(Duration::from_secs(1), lifecycle.recv())
        .await
        .expect("transport finish publishes client-detached")
        .expect("lifecycle channel remains open");
    assert_eq!(detached.control_session_identity, Some(session_id));
    assert!(matches!(
        detached.event,
        LifecycleEvent::ClientDetached {
            session_name,
            client_name: Some(client_name),
        } if session_name == renamed && client_name == control_client_name(control.pid)
    ));
}

#[tokio::test]
async fn queue_attach_without_exact_identity_commits_event_identity_and_touch() {
    let handler = RequestHandler::new();
    let target = session_name("queue-attach-success-target");
    SessionSpec::create(&handler, &target).await;
    let target_id = handler.session_id_for_test(&target).await;
    let requester_pid = 43_019;
    let (event_tx, mut events) = mpsc::channel(1);
    let control_id = handler
        .register_control_with_closing(
            requester_pid,
            ControlModeUpgrade {
                initial_command_count: 0,
                mode: ControlMode::Plain,
                terminal_context: OuterTerminalContext::default(),
            },
            event_tx,
            Arc::new(AtomicBool::new(false)),
        )
        .await;

    assert!(handler
        .attach_control_session_for_queue(
            ControlClientIdentity::new(requester_pid, control_id),
            &target,
            Some(target_id),
        )
        .await
        .expect("queue attach succeeds"));
    assert!(matches!(
        events.try_recv(),
        Ok(ControlServerEvent::SessionChanged(Some(ref session_name))
            | ControlServerEvent::SessionChangedAt {
                ref session_name,
                ..
            }) if session_name == &target
    ));
    let active_control = handler.active_control.lock().await;
    let active = active_control
        .by_pid
        .get(&requester_pid)
        .expect("queue-attached control remains registered");
    assert_eq!(active.session_name.as_ref(), Some(&target));
    assert_eq!(active.session_id, Some(target_id));
    drop(active_control);
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&target)
        .expect("target survives")
        .last_attached_at()
        .is_some());
}

#[tokio::test]
async fn failed_queue_attach_and_destroy_switch_restore_their_source_identities() {
    let handler = RequestHandler::new();
    let source = session_name("delivery-failure-destroy-source");
    let target = session_name("delivery-failure-target");
    SessionSpec::create(&handler, &source).await;
    SessionSpec::create(&handler, &target).await;
    let source_id = handler.session_id_for_test(&source).await;
    let target_id = handler.session_id_for_test(&target).await;

    let unattached_pid = 43_020;
    let (unattached_tx, mut unattached_rx) = mpsc::channel(1);
    let unattached_closing = Arc::new(AtomicBool::new(false));
    let unattached_id = handler
        .register_control_with_closing(
            unattached_pid,
            ControlModeUpgrade {
                initial_command_count: 0,
                mode: ControlMode::Plain,
                terminal_context: OuterTerminalContext::default(),
            },
            unattached_tx,
            Arc::clone(&unattached_closing),
        )
        .await;
    unattached_rx.close();
    let unattached_identity = ControlClientIdentity::new(unattached_pid, unattached_id);
    assert!(handler
        .attach_control_session_for_queue(unattached_identity, &target, Some(target_id))
        .await
        .is_err());

    let mut switched = register_attached_control(&handler, 43_021, source.clone()).await;
    switched.events.close();
    assert_eq!(
        handler
            .switch_control_session_after_destroy(
                switched.pid,
                switched.control_id,
                source_id,
                target_id,
            )
            .await,
        None
    );

    {
        let active_control = handler.active_control.lock().await;
        let unattached = active_control
            .by_pid
            .get(&unattached_pid)
            .expect("failed queue attach remains registered");
        assert_eq!(unattached.id, unattached_id);
        assert_eq!(
            (unattached.session_name.as_ref(), unattached.session_id),
            (None, None)
        );
        assert!(unattached.closing.load(Ordering::SeqCst));
        let destroy_switch = active_control
            .by_pid
            .get(&switched.pid)
            .expect("failed destroy switch remains registered");
        assert_eq!(destroy_switch.id, switched.control_id);
        assert_eq!(destroy_switch.session_name.as_ref(), Some(&source));
        assert_eq!(destroy_switch.session_id, Some(source_id));
        assert_eq!(destroy_switch.last_session, None);
        assert_eq!(destroy_switch.last_session_id, None);
        assert!(destroy_switch.closing.load(Ordering::SeqCst));
    }
    assert!(unattached_closing.load(Ordering::SeqCst));

    let mut lifecycle = handler.subscribe_lifecycle_events();
    handler.finish_control(unattached_pid, unattached_id).await;
    handler
        .finish_control(switched.pid, switched.control_id)
        .await;
    let detached = tokio::time::timeout(Duration::from_secs(1), lifecycle.recv())
        .await
        .expect("destroy-switch transport finish publishes client-detached")
        .expect("lifecycle channel remains open");
    assert_eq!(detached.control_session_identity, Some(source_id));
    assert!(matches!(
        detached.event,
        LifecycleEvent::ClientDetached {
            session_name,
            client_name: Some(client_name),
        } if session_name == source && client_name == control_client_name(switched.pid)
    ));
    assert!(matches!(
        lifecycle.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn stale_closing_control_does_not_keep_recreated_destroy_unattached_session_alive() {
    let handler = RequestHandler::new();
    let session = session_name("delivery-failure-recreated-destroy-unattached");
    SessionSpec::create(&handler, &session).await;
    let old_session_id = handler.session_id_for_test(&session).await;
    let mut stale = register_attached_control(&handler, 43_030, session.clone()).await;
    stale.events.close();

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&session)).await;
    assert!(stale.closing.load(Ordering::SeqCst));

    SessionSpec::create(&handler, &session).await;
    let replacement_session_id = handler.session_id_for_test(&session).await;
    assert_ne!(replacement_session_id, old_session_id);
    handler
        .store_option_for_test(
            ScopeSelector::Session(session.clone()),
            OptionName::DestroyUnattached,
            "on",
        )
        .await;
    let replacement = register_attached_control(&handler, 43_031, session.clone()).await;

    handler
        .finish_control(replacement.pid, replacement.control_id)
        .await;

    assert!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&session)
            .is_none(),
        "a stale same-name control identity must not keep the replacement session alive"
    );
    {
        let active_control = handler.active_control.lock().await;
        let active = active_control
            .by_pid
            .get(&stale.pid)
            .expect("stale control remains owned by its unfinished transport");
        assert_eq!(active.id, stale.control_id);
        assert_eq!(active.session_id, Some(old_session_id));
    }

    handler.finish_control(stale.pid, stale.control_id).await;
    assert!(handler.active_control.lock().await.by_pid.is_empty());
}
