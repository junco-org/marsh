use std::time::Duration;

use rmux_core::LifecycleEvent;
use rmux_proto::{
    HookLifecycle, HookName, LinkWindowRequest, OptionName, RenameWindowRequest, ScopeSelector,
    SetHookRequest, WindowTarget,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{QueuedLifecycleEvent, RequestHandler};
use crate::control::ControlServerEvent;
use crate::pane_io::PaneAlertEvent;
use crate::test_fixtures::{Fixture, Quiet};

fn drain_control_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut notifications = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ControlServerEvent::Notification(line) = event {
            notifications.push(line);
        }
    }
    notifications
}

fn assert_control_rename_before_hook(
    rx: &mut mpsc::Receiver<ControlServerEvent>,
    expected_rename: &str,
    hook_buffer: &str,
) {
    let notifications = drain_control_notifications(rx);
    let rename_notifications = notifications
        .iter()
        .filter(|line| {
            line.starts_with("%window-renamed ") || line.starts_with("%unlinked-window-renamed ")
        })
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(rename_notifications, vec![expected_rename]);
    let rename_position = notifications
        .iter()
        .position(|line| line == expected_rename)
        .expect("rename notification exists");
    let hook_position = notifications
        .iter()
        .position(|line| line == &format!("%paste-buffer-changed {hook_buffer}"))
        .expect("hook side effect notification exists");
    assert!(
        rename_position < hook_position,
        "control rename notification must precede the hook side effect: {notifications:?}"
    );
}

async fn recv_window_renamed(
    events: &mut broadcast::Receiver<QueuedLifecycleEvent>,
) -> QueuedLifecycleEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("lifecycle event channel remains open");
            if matches!(event.event, LifecycleEvent::WindowRenamed { .. }) {
                return event;
            }
        }
    })
    .await
    .expect("window-renamed lifecycle event should be published")
}

fn assert_no_additional_window_renamed(events: &mut broadcast::Receiver<QueuedLifecycleEvent>) {
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.event, LifecycleEvent::WindowRenamed { .. }),
            "rename mutation published a duplicate window-renamed event"
        );
    }
}

fn assert_hook_window_name(event: &QueuedLifecycleEvent, expected: &str) {
    assert_eq!(
        event
            .formats
            .iter()
            .find_map(|(name, value)| (name == "hook_window_name").then_some(value.as_str())),
        Some(expected)
    );
}

async fn active_pane_id(handler: &RequestHandler, target: &WindowTarget) -> rmux_proto::PaneId {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .and_then(rmux_core::Window::active_pane)
        .map(rmux_core::Pane::id)
        .expect("active pane exists")
}

async fn assert_linked_window_names(
    handler: &RequestHandler,
    targets: &[WindowTarget],
    expected: &str,
) {
    let state = handler.state.lock().await;
    for target in targets {
        let name = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .and_then(rmux_core::Window::name);
        assert_eq!(name, Some(expected), "unexpected name for {target}");
    }
}

#[tokio::test]
async fn automatic_and_manual_renames_publish_one_link_aware_event_before_the_hook() {
    // Oracle probe 2026-07-26, pinned tmux 3.7b: both automatic and manual
    // renames publish one linked/unlinked control notification per client,
    // followed by exactly one window-renamed hook.
    let handler = RequestHandler::new();
    let alpha = handler
        .create_started_session(Quiet("auto-rename-alpha"))
        .await;
    let beta = handler
        .create_started_session(Quiet("auto-rename-beta"))
        .await;
    let gamma = handler
        .create_started_session(Quiet("auto-rename-gamma"))
        .await;
    let alpha_target = WindowTarget::with_window(alpha.clone(), 0);
    let gamma_target = WindowTarget::with_window(gamma.clone(), 1);
    handler
        .handle_ok(LinkWindowRequest::fixture((&alpha_target, &gamma_target)))
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alpha_target.clone()),
            OptionName::AutomaticRenameFormat,
            "automatic-oracle",
        )
        .await;
    handler
        .handle_ok(SetHookRequest {
            lifecycle: HookLifecycle::OneShot,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::WindowRenamed,
                "set-buffer -b automatic-rename-hook fired",
            ))
        })
        .await;
    let lifecycle_dispatch = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns lifecycle hook dispatch");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_dispatch, shutdown_rx)
            .await;
    });

    let (_, mut alpha_control) = handler
        .register_control_for_test(51_001, Some(&alpha))
        .await;
    let (_, mut beta_control) = handler.register_control_for_test(51_002, Some(&beta)).await;
    let (_, mut gamma_control) = handler
        .register_control_for_test(51_003, Some(&gamma))
        .await;
    let _ = drain_control_notifications(&mut alpha_control);
    let _ = drain_control_notifications(&mut beta_control);
    let _ = drain_control_notifications(&mut gamma_control);
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let pane_id = active_pane_id(&handler, &alpha_target).await;
    let shared_window_id = handler.window_id_for_test(&alpha_target).await.as_u32();

    handler
        .handle_pane_alert_event(PaneAlertEvent {
            queue_activity_alert: true,
            ..Fixture::fixture((&alpha, pane_id))
        })
        .await;

    let automatic_event = recv_window_renamed(&mut lifecycle).await;
    handler
        .wait_for_buffer("automatic-rename-hook", "fired")
        .await;
    assert_eq!(automatic_event.hooks.len(), 1);
    assert_control_rename_before_hook(
        &mut alpha_control,
        &format!("%window-renamed @{shared_window_id} automatic-oracle"),
        "automatic-rename-hook",
    );
    assert_control_rename_before_hook(
        &mut beta_control,
        &format!("%unlinked-window-renamed @{shared_window_id} automatic-oracle"),
        "automatic-rename-hook",
    );
    assert_control_rename_before_hook(
        &mut gamma_control,
        &format!("%window-renamed @{shared_window_id} automatic-oracle"),
        "automatic-rename-hook",
    );
    assert_linked_window_names(
        &handler,
        &[alpha_target.clone(), gamma_target.clone()],
        "automatic-oracle",
    )
    .await;
    assert_hook_window_name(&automatic_event, "automatic-oracle");
    assert_no_additional_window_renamed(&mut lifecycle);

    handler
        .handle_ok(SetHookRequest {
            lifecycle: HookLifecycle::OneShot,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::WindowRenamed,
                "set-buffer -b manual-rename-hook fired",
            ))
        })
        .await;
    handler
        .handle_ok(RenameWindowRequest {
            target: alpha_target,
            name: "manual-oracle".to_owned(),
        })
        .await;

    let manual_event = recv_window_renamed(&mut lifecycle).await;
    handler.wait_for_buffer("manual-rename-hook", "fired").await;
    assert_eq!(manual_event.hooks.len(), 1);
    assert_control_rename_before_hook(
        &mut alpha_control,
        &format!("%window-renamed @{shared_window_id} manual-oracle"),
        "manual-rename-hook",
    );
    assert_control_rename_before_hook(
        &mut beta_control,
        &format!("%unlinked-window-renamed @{shared_window_id} manual-oracle"),
        "manual-rename-hook",
    );
    assert_control_rename_before_hook(
        &mut gamma_control,
        &format!("%window-renamed @{shared_window_id} manual-oracle"),
        "manual-rename-hook",
    );
    assert_linked_window_names(&handler, &[gamma_target], "manual-oracle").await;
    assert_hook_window_name(&manual_event, "manual-oracle");
    assert_no_additional_window_renamed(&mut lifecycle);

    shutdown_tx.send(()).expect("hook dispatcher stays alive");
    hook_task.await.expect("hook dispatcher shuts down cleanly");
}
