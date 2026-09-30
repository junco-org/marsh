use std::time::Duration;

use rmux_core::LifecycleEvent;
use rmux_proto::{
    HookLifecycle, HookName, Request, ScopeSelector, SessionName, SetHookMutationRequest,
    ShowBufferRequest, WaitForMode, WaitForRequest,
};
use tokio::sync::oneshot;

use super::*;

use crate::test_fixtures::{Fixture, SessionSpec, TestRequest};

fn spawn_lifecycle_consumer(handler: &RequestHandler) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test activates the lifecycle dispatch receiver once");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let consumer_handler = handler.clone();
    let consumer = tokio::spawn(async move {
        consumer_handler
            .consume_lifecycle_hooks(events, shutdown_rx)
            .await;
    });
    (shutdown, consumer)
}

async fn emit_focus(handler: &RequestHandler, session_name: &SessionName, client_name: &str) {
    handler
        .emit_lifecycle_event_for_test(LifecycleEvent::ClientFocusIn {
            session_name: session_name.clone(),
            client_name: Some(client_name.to_owned()),
        })
        .await;
}

async fn wait_for_hook_block(handler: &RequestHandler, channel: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handler.wait_for_counts(channel) == (1, 0, false) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hook reaches its deterministic wait-for seam");
}

async fn seal_final_lifecycle_boundaries(handler: &RequestHandler) {
    handler.close_and_drain_lifecycle_producers().await;
    handler.close_and_drain_post_commit_operations().await;
    handler.seal_and_wait_for_lifecycle_publications().await;
}

#[tokio::test]
async fn full_outbox_and_stuck_hook_are_forced_at_the_shared_deadline() {
    let handler = RequestHandler::with_lifecycle_dispatch_capacity_for_test(1);
    let session = SessionSpec::create(&handler, "shutdown-full-outbox").await;
    handler
        .set_global_hook(
            HookName::ClientFocusIn,
            "wait-for shutdown-full-outbox-hook",
        )
        .await;
    let (hook_shutdown, mut hook_task) = spawn_lifecycle_consumer(&handler);

    emit_focus(&handler, &session, "accepted").await;
    wait_for_hook_block(&handler, "shutdown-full-outbox-hook").await;
    emit_focus(&handler, &session, "queued").await;

    let mut observed = handler.subscribe_lifecycle_events();
    let blocked_handler = handler.clone();
    let blocked_session = session.clone();
    let blocked_publication = tokio::spawn(async move {
        emit_focus(&blocked_handler, &blocked_session, "blocked").await;
    });
    tokio::time::timeout(Duration::from_secs(2), observed.recv())
        .await
        .expect("blocked publication reaches the saturated outbox")
        .expect("lifecycle broadcast remains active");
    tokio::task::yield_now().await;
    assert!(
        !blocked_publication.is_finished(),
        "the third publication must wait for outbox capacity"
    );
    let following_event = handler
        .prepare_lifecycle_event_for_test(LifecycleEvent::ClientFocusIn {
            session_name: session.clone(),
            client_name: Some("ordered-behind".to_owned()),
        })
        .await;
    let following_handler = handler.clone();
    let following_publication = tokio::spawn(async move {
        following_handler
            .emit_prepared_lifecycle_event_for_test(following_event)
            .await;
    });
    tokio::task::yield_now().await;
    assert!(
        !following_publication.is_finished(),
        "the synchronously reserved fourth ticket waits behind the blocked commit turn"
    );

    let deadline = Instant::now() + Duration::from_millis(50);
    let outcome =
        drain_lifecycle_hooks_before_deadline(&handler, hook_shutdown, &mut hook_task, deadline)
            .await;

    assert_eq!(outcome, LifecycleHookShutdownOutcome::Forced);
    assert!(
        Instant::now() >= deadline,
        "fallback waits for its deadline"
    );
    blocked_publication
        .await
        .expect("blocked publication is released when the receiver drops");
    following_publication
        .await
        .expect("the following commit ticket advances after receiver drop");
    assert!(hook_task.is_finished());
    assert_eq!(
        handler.wait_for_counts("shutdown-full-outbox-hook"),
        (0, 0, false),
        "aborting the accepted hook removes its wait-for waiter"
    );

    // The forced path already repeated these barriers. A second repetition proves that the
    // shutdown recovery remains idempotent after the queue receiver is gone.
    handler.close_normal_and_drain_lifecycle_producers().await;
    handler
        .close_normal_and_drain_post_commit_operations()
        .await;
    handler.close_and_wait_for_lifecycle_publications().await;
    seal_final_lifecycle_boundaries(&handler).await;
}

#[tokio::test]
async fn normal_shutdown_drains_already_accepted_and_queued_hooks() {
    let handler = RequestHandler::with_lifecycle_dispatch_capacity_for_test(1);
    let session = SessionSpec::create(&handler, "shutdown-normal-outbox").await;
    for (command, lifecycle, append) in [
        (
            "wait-for shutdown-normal-outbox-hook",
            HookLifecycle::OneShot,
            false,
        ),
        (
            "set-buffer -b shutdown-normal-outbox drained",
            HookLifecycle::Persistent,
            true,
        ),
    ] {
        TestRequest::send_ok(
            &handler,
            SetHookMutationRequest {
                lifecycle,
                append,
                ..Fixture::fixture((ScopeSelector::Global, HookName::ClientFocusIn, command))
            },
        )
        .await;
    }
    let (hook_shutdown, mut hook_task) = spawn_lifecycle_consumer(&handler);

    emit_focus(&handler, &session, "accepted").await;
    wait_for_hook_block(&handler, "shutdown-normal-outbox-hook").await;
    emit_focus(&handler, &session, "queued").await;
    TestRequest::send_ok(
        &handler,
        WaitForRequest::fixture(("shutdown-normal-outbox-hook", WaitForMode::Signal)),
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(2);
    let outcome =
        drain_lifecycle_hooks_before_deadline(&handler, hook_shutdown, &mut hook_task, deadline)
            .await;

    assert_eq!(outcome, LifecycleHookShutdownOutcome::Drained);
    assert!(Instant::now() < deadline);
    assert!(hook_task.is_finished());
    let response = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("shutdown-normal-outbox".to_owned()),
        }))
        .await;
    assert_eq!(
        response
            .command_output()
            .expect("drained hook creates its buffer")
            .stdout(),
        b"drained"
    );
    seal_final_lifecycle_boundaries(&handler).await;
}

#[tokio::test]
async fn ready_drain_wins_when_it_ties_the_deadline() {
    assert_eq!(
        complete_before_deadline(Instant::now(), async { 7_u8 }).await,
        Some(7)
    );
}
