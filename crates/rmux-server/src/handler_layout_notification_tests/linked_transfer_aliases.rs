use rmux_core::LifecycleEvent;
use rmux_proto::{
    HookName, LinkWindowRequest, ScopeSelector, SessionName, SetHookRequest, WindowTarget,
};
use tokio::sync::oneshot;

use super::{
    create_session, run_control_command, run_detached_command, settle_control_notifications,
    wait_for_buffer_text, RequestHandler,
};
use crate::test_fixtures::{Fixture, Grouped, SessionSpec, TestRequest};

#[tokio::test]
async fn linked_and_grouped_join_move_keep_requested_resize_alias() {
    for operation in ["join-pane", "move-pane"] {
        for family in ["linked", "grouped"] {
            assert_transfer_alias(operation, family).await;
        }
    }
}

async fn assert_transfer_alias(operation: &str, family: &str) {
    let label = format!("{operation}-{family}");
    let alpha = session_name(&format!("resize-alias-alpha-{label}"));
    let beta = session_name(&format!("resize-alias-beta-{label}"));
    let source = session_name(&format!("resize-alias-source-{label}"));
    let buffer = format!("resize-alias-events-{label}");
    let handler = RequestHandler::new();
    let lifecycle_dispatch = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns lifecycle dispatch");
    let (hook_shutdown_tx, hook_shutdown_rx) = oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_dispatch, hook_shutdown_rx)
            .await;
    });

    create_session(&handler, &alpha).await;
    if family == "grouped" {
        SessionSpec::create(&handler, Grouped(&beta, &alpha)).await;
    } else {
        create_session(&handler, &beta).await;
        TestRequest::send_ok(
            &handler,
            LinkWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(alpha.clone(), 0),
                    WindowTarget::with_window(beta.clone(), 0),
                ))
            },
        )
        .await;
    }
    let requested = alias_opposite_hashmap_first(&handler, &alpha, &beta).await;
    create_session(&handler, &source).await;
    run_detached_command(
        &handler,
        &format!("set-window-option -t {requested}:0 window-size manual"),
    )
    .await;
    run_detached_command(
        &handler,
        &format!("resize-window -t {requested}:0 -x 1 -y 1"),
    )
    .await;
    run_detached_command(&handler, &format!("set-buffer -b {buffer} ''")).await;
    let resize_hook = format!("run-shell -C 'set-buffer -b {buffer} #{{session_name}}'");
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::WindowResized,
            resize_hook.as_str(),
        )),
    )
    .await;

    let control_pid = std::process::id();
    let (_, mut notifications) = handler
        .register_control_for_test(control_pid, Some(&requested))
        .await;
    let _ = settle_control_notifications(&mut notifications).await;
    let mut lifecycle_events = handler.subscribe_lifecycle_events();
    let command = format!("{operation} -h -d -s {source}:0.0 -t {requested}:0.0");
    run_control_command(&handler, control_pid, &command).await;
    let control_lines = settle_control_notifications(&mut notifications).await;
    let hook_events = wait_for_buffer_text(&handler, &buffer, requested.as_str()).await;
    let mut events = Vec::new();
    while let Ok(Ok(event)) =
        tokio::time::timeout(super::CONTROL_NOTIFICATION_POLL, lifecycle_events.recv()).await
    {
        events.push(event.event);
    }

    let resized_targets = events
        .iter()
        .filter_map(|event| match event {
            LifecycleEvent::WindowResized { target } => Some(target.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        resized_targets,
        vec![WindowTarget::with_window(requested.clone(), 0)],
        "{label}: lifecycle resize must retain the requested alias; events={events:?}"
    );
    assert_eq!(
        hook_events,
        Some(requested.to_string()),
        "{label}: the hook format context must retain the requested session alias"
    );
    assert_eq!(
        control_lines
            .iter()
            .filter(|line| line.starts_with("%layout-change "))
            .count(),
        1,
        "{label}: linked/grouped aliases must still receive one control layout"
    );

    let _ = hook_shutdown_tx.send(());
    hook_task.await.expect("lifecycle hook task");
}

async fn alias_opposite_hashmap_first(
    handler: &RequestHandler,
    alpha: &SessionName,
    beta: &SessionName,
) -> SessionName {
    let state = handler.state.lock().await;
    let shared_window_id = state
        .sessions
        .session(alpha)
        .expect("alpha")
        .window_at(0)
        .expect("alpha window")
        .id();
    let first = state
        .sessions
        .iter()
        .find_map(|(session_name, session)| {
            session
                .window_at(0)
                .is_some_and(|window| window.id() == shared_window_id)
                .then(|| session_name.clone())
        })
        .expect("shared alias");
    if first == *alpha {
        beta.clone()
    } else {
        alpha.clone()
    }
}

use crate::test_names::session_name;
