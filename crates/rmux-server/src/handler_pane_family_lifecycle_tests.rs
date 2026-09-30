use super::pane_group_transfer_tests::pane_id;
use super::RequestHandler;
use crate::pane_io::{AttachControl, PaneExitEvent};
use crate::test_fixtures::{Fixture, Grouped, SessionSpec, SubscribeRequest, TestRequest};
use rmux_core::LifecycleEvent;
use rmux_proto::{
    BreakPaneRequest, HookName, KillPaneRequest, KillSessionRequest, LinkWindowRequest,
    NewWindowRequest, PaneKillRequest, PaneOutputCursorRequest, PaneTarget, PaneTargetRef, Request,
    Response, ScopeSelector, SessionName, SetHookRequest, SubscribePaneOutputRefRequest,
    WindowTarget,
};
use std::time::Duration;

#[path = "handler_pane_family_lifecycle_tests/inactive_winlink_resize.rs"]
mod inactive_winlink_resize;
#[path = "handler_pane_family_lifecycle_tests/resize_mutation_reconciliation.rs"]
mod resize_mutation_reconciliation;
#[path = "handler_pane_family_lifecycle_tests/resize_policy.rs"]
mod resize_policy;

type LifecycleKey = (HookName, SessionName);

#[derive(Debug, Default)]
struct PaneKillSubscriptionRekeyPause {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

static PANE_KILL_SUBSCRIPTION_REKEY_PAUSE: std::sync::Mutex<
    Option<(SessionName, std::sync::Arc<PaneKillSubscriptionRekeyPause>)>,
> = std::sync::Mutex::new(None);

fn install_pane_kill_subscription_rekey_pause(
    session_name: SessionName,
) -> std::sync::Arc<PaneKillSubscriptionRekeyPause> {
    let pause = std::sync::Arc::new(PaneKillSubscriptionRekeyPause::default());
    *PANE_KILL_SUBSCRIPTION_REKEY_PAUSE
        .lock()
        .expect("pane-kill subscription rekey pause lock") = Some((session_name, pause.clone()));
    pause
}

pub(super) async fn pause_before_pane_kill_subscription_rekey(session_name: &SessionName) {
    let pause = PANE_KILL_SUBSCRIPTION_REKEY_PAUSE
        .lock()
        .expect("pane-kill subscription rekey pause lock")
        .as_ref()
        .filter(|(paused_session, _)| paused_session == session_name)
        .map(|(_, pause)| pause.clone());
    let Some(pause) = pause else {
        return;
    };
    pause.reached.notify_one();
    pause.release.notified().await;
    let mut installed = PANE_KILL_SUBSCRIPTION_REKEY_PAUSE
        .lock()
        .expect("pane-kill subscription rekey pause lock");
    if installed
        .as_ref()
        .is_some_and(|(_, current)| std::sync::Arc::ptr_eq(current, &pause))
    {
        installed.take();
    }
}

async fn install_family_hooks(handler: &RequestHandler) {
    for (hook, command) in [
        (HookName::PaneExited, "display-message family-pane-exited"),
        (
            HookName::WindowUnlinked,
            "display-message family-window-unlinked",
        ),
        (
            HookName::SessionClosed,
            "display-message family-session-closed",
        ),
    ] {
        TestRequest::send_ok(
            handler,
            SetHookRequest::fixture((ScopeSelector::Global, hook, command)),
        )
        .await;
    }
}

fn lifecycle_key(event: &super::QueuedLifecycleEvent) -> Option<LifecycleKey> {
    match &event.event {
        LifecycleEvent::PaneExited { target, .. } => {
            Some((HookName::PaneExited, target.session_name().clone()))
        }
        LifecycleEvent::WindowUnlinked { session_name, .. } => {
            Some((HookName::WindowUnlinked, session_name.clone()))
        }
        LifecycleEvent::SessionClosed { session_name, .. } => {
            Some((HookName::SessionClosed, session_name.clone()))
        }
        _ => None,
    }
}

fn expected_hook_command(hook: HookName) -> &'static str {
    match hook {
        HookName::PaneExited => "display-message family-pane-exited",
        HookName::WindowUnlinked => "display-message family-window-unlinked",
        HookName::SessionClosed => "display-message family-session-closed",
        _ => unreachable!("family lifecycle filter only returns measured hooks"),
    }
}

fn assert_lifecycle_batch(
    events: &mut tokio::sync::broadcast::Receiver<super::QueuedLifecycleEvent>,
    expected: &[LifecycleKey],
) {
    let actual = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| lifecycle_key(&event).map(|key| (key, event.hooks)))
        .collect::<Vec<_>>();
    assert_eq!(
        actual.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    for ((hook, _), dispatches) in actual {
        assert_eq!(
            dispatches
                .iter()
                .filter(|dispatch| dispatch.command() == expected_hook_command(hook))
                .count(),
            1,
            "every measured lifecycle event must retain its global hook dispatch"
        );
    }
}

async fn create_grouped_last_pane_family(
    handler: &RequestHandler,
    label: &str,
) -> (SessionName, SessionName, SessionName, rmux_core::PaneId) {
    let keeper = SessionSpec::create(handler, format!("{label}-keeper")).await;
    let owner = SessionSpec::create(handler, format!("{label}-owner")).await;
    let peer = SessionSpec::create(handler, Grouped(format!("{label}-peer"), &owner)).await;
    let family_pane_id = {
        let state = handler.state.lock().await;
        assert_eq!(state.window_linked_session_count(&owner, 0), 2);
        pane_id(&state, &owner, 0, 0)
    };
    (keeper, owner, peer, family_pane_id)
}

fn grouped_kill_order(owner: &SessionName, peer: &SessionName) -> Vec<LifecycleKey> {
    vec![
        (HookName::WindowUnlinked, owner.clone()),
        (HookName::SessionClosed, owner.clone()),
        (HookName::SessionClosed, peer.clone()),
        (HookName::WindowUnlinked, peer.clone()),
    ]
}

#[tokio::test]
async fn direct_grouped_last_pane_kill_matches_tmux_3_7b_family_lifecycle() {
    let handler = RequestHandler::new();
    let (keeper, owner, peer, _) =
        create_grouped_last_pane_family(&handler, "direct-family-kill").await;
    install_family_hooks(&handler).await;
    let mut events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(owner.clone(), 0, 0),
            kill_all_except: false,
        },
    )
    .await;

    assert_lifecycle_batch(&mut events, &grouped_kill_order(&owner, &peer));
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_none());
    assert!(state.sessions.session(&peer).is_none());
    assert!(state.sessions.session(&keeper).is_some());
}

#[tokio::test]
async fn pane_id_grouped_last_pane_removes_only_addressed_alias_product_divergence() {
    let handler = RequestHandler::new();
    let (keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-family-kill").await;
    install_family_hooks(&handler).await;
    let mut events = handler.subscribe_lifecycle_events();

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    assert_lifecycle_batch(
        &mut events,
        &[
            (HookName::WindowUnlinked, peer.clone()),
            (HookName::SessionClosed, peer.clone()),
        ],
    );
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_some());
    assert!(state.sessions.session(&peer).is_none());
    assert!(state.sessions.session(&keeper).is_some());
    state
        .ensure_panes_exist(&owner, &[family_pane_id])
        .expect("the surviving group owner retains the shared pane runtime");
}

#[tokio::test]
async fn pane_id_grouped_last_pane_with_real_winlink_preserves_surviving_family() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-linked-group").await;
    let linked_survivor = SessionSpec::create(&handler, "pane-id-linked-survivor").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(linked_survivor.clone(), 1),
        )),
    )
    .await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_some());
    assert!(state.sessions.session(&peer).is_none());
    assert!(state
        .sessions
        .session(&linked_survivor)
        .and_then(|session| session.window_at(1))
        .is_some());
    state
        .ensure_panes_exist(&owner, &[family_pane_id])
        .expect("the surviving real winlink family retains the shared pane runtime");
}

#[tokio::test]
async fn pane_id_grouped_runtime_owner_with_real_winlink_transfers_surviving_family() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-linked-owner").await;
    let linked_survivor = SessionSpec::create(&handler, "pane-id-linked-owner-survivor").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(linked_survivor.clone(), 1),
        )),
    )
    .await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(owner.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_none());
    assert!(state.sessions.session(&peer).is_some());
    assert!(state
        .sessions
        .session(&linked_survivor)
        .and_then(|session| session.window_at(1))
        .is_some());
    state
        .ensure_panes_exist(&peer, &[family_pane_id])
        .expect("the surviving peer owns the linked pane runtime");
}

#[tokio::test]
async fn pane_id_duplicate_winlink_removes_only_the_resolved_alias() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "pane-id-duplicate-alias").await;
    let _keeper = SessionSpec::create(&handler, "pane-id-duplicate-keeper").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(source.clone(), 1),
        )),
    )
    .await;
    let pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &source, 0, 0)
    };

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(source.clone(), pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&source).expect("session survives");
    assert_eq!(session.windows().len(), 1);
    state
        .ensure_panes_exist(&source, &[pane_id])
        .expect("the unresolved duplicate alias retains the pane runtime");
}

#[tokio::test]
async fn pane_id_grouped_alias_removal_preserves_live_output_subscription() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-subscription").await;
    let connection_id = 4242;
    let subscribed = SubscribeRequest::subscribe_ok(
        &handler,
        connection_id,
        SubscribePaneOutputRefRequest::fixture((&owner, family_pane_id)),
    )
    .await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let cursor = handler
        .handle_pane_output_cursor(
            connection_id,
            PaneOutputCursorRequest {
                subscription_id: subscribed.subscription_id,
                max_events: Some(8),
            },
        )
        .await;
    assert!(
        matches!(cursor, Response::PaneOutputCursor(_)),
        "subscription to the still-live pane must survive alias removal: {cursor:?}"
    );
}

#[tokio::test]
async fn pane_id_runtime_owner_alias_removal_rekeys_live_output_subscription() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-owner-subscription").await;
    let connection_id = 4243;
    let subscribed = SubscribeRequest::subscribe_ok(
        &handler,
        connection_id,
        SubscribePaneOutputRefRequest::fixture((&owner, family_pane_id)),
    )
    .await;

    let removed_owner = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(owner, family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(
        matches!(removed_owner, Response::KillPane(_)),
        "{removed_owner:?}"
    );
    let live_cursor = handler
        .handle_pane_output_cursor(
            connection_id,
            PaneOutputCursorRequest {
                subscription_id: subscribed.subscription_id,
                max_events: Some(8),
            },
        )
        .await;
    assert!(
        matches!(live_cursor, Response::PaneOutputCursor(_)),
        "{live_cursor:?}"
    );

    let removed_runtime = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer, family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(
        matches!(removed_runtime, Response::KillPane(_)),
        "{removed_runtime:?}"
    );
    let cursor_during_drain = handler
        .handle_pane_output_cursor(
            connection_id,
            PaneOutputCursorRequest {
                subscription_id: subscribed.subscription_id,
                max_events: Some(8),
            },
        )
        .await;
    assert!(
        matches!(cursor_during_drain, Response::PaneOutputCursor(_)),
        "subscription must remain readable during pane-output drain: {cursor_during_drain:?}"
    );

    let draining_key = handler
        .pane_output_subscription_key_for_test(subscribed.subscription_id)
        .expect("rekeyed subscription remains registered during pane-output drain");
    handler
        .subscriptions
        .lock()
        .expect("subscription registry mutex must not be poisoned")
        .expire_pane_drain(&draining_key, std::time::Instant::now());

    let closed_cursor = handler
        .handle_pane_output_cursor(
            connection_id,
            PaneOutputCursorRequest {
                subscription_id: subscribed.subscription_id,
                max_events: Some(8),
            },
        )
        .await;
    assert!(
        matches!(
            closed_cursor,
            Response::Error(ref error) if error.error.to_string().contains("subscription not found")
        ),
        "expired pane-output drain must remove the subscription: {closed_cursor:?}"
    );
}

#[tokio::test]
async fn pane_id_owner_rekey_commits_before_following_owner_transfer() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-rekey-order").await;
    let survivor =
        SessionSpec::create(&handler, Grouped("pane-id-rekey-order-survivor", &owner)).await;
    let subscribed = SubscribeRequest::subscribe_ok(
        &handler,
        4245,
        SubscribePaneOutputRefRequest::fixture((&owner, family_pane_id)),
    )
    .await;

    let pause = install_pane_kill_subscription_rekey_pause(owner.clone());
    let pane_kill_handler = handler.clone();
    let pane_kill_owner = owner.clone();
    let pane_kill = tokio::spawn(async move {
        pane_kill_handler
            .handle(Request::PaneKill(PaneKillRequest {
                target: PaneTargetRef::by_id(pane_kill_owner, family_pane_id),
                kill_all_except: false,
            }))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("pane-kill reaches its state-locked subscription rekey");

    let next_transfer_handler = handler.clone();
    let next_transfer_peer = peer.clone();
    let next_transfer = tokio::spawn(async move {
        next_transfer_handler
            .handle(Request::KillSession(KillSessionRequest::fixture(
                next_transfer_peer,
            )))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !next_transfer.is_finished(),
        "the B -> C transfer must wait until pane-kill commits A -> B"
    );

    pause.release.notify_one();
    let pane_kill_response = tokio::time::timeout(Duration::from_secs(2), pane_kill)
        .await
        .expect("pane-kill rekey completes before timeout")
        .expect("pane-kill task joins");
    let next_transfer_response = tokio::time::timeout(Duration::from_secs(2), next_transfer)
        .await
        .expect("following owner transfer completes before timeout")
        .expect("following owner-transfer task joins");
    assert!(
        matches!(pane_kill_response, Response::KillPane(_)),
        "{pane_kill_response:?}"
    );
    assert!(
        matches!(next_transfer_response, Response::KillSession(_)),
        "{next_transfer_response:?}"
    );

    let subscription_key = handler
        .pane_output_subscription_key_for_test(subscribed.subscription_id)
        .expect("live subscription remains registered");
    assert_eq!(
        subscription_key.runtime_session_name(),
        &survivor,
        "the subscription must follow both A -> B and B -> C owner transfers"
    );
}

#[tokio::test]
async fn pane_id_grouped_multiwindow_refreshes_mutated_owner() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create(&handler, "pane-id-multi-owner").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let peer = SessionSpec::create(&handler, Grouped("pane-id-multi-peer", &owner)).await;
    handler.wait_for_initial_panes_for_test().await;
    let closing_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &peer, 0, 0)
    };

    let mut control_rx = handler.attach_client(4343, &owner).await;
    while tokio::time::timeout(Duration::from_millis(500), control_rx.recv())
        .await
        .is_ok()
    {}

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), closing_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let control = tokio::time::timeout(Duration::from_millis(250), control_rx.recv())
        .await
        .expect("mutated non-addressed owner must be refreshed")
        .expect("owner attach remains connected");
    assert!(matches!(control, AttachControl::Switch(_)), "{control:?}");
}

#[tokio::test]
async fn duplicate_alias_last_pane_kill_removes_the_complete_link_family() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "duplicate-last-pane-kill").await;
    let keeper = SessionSpec::create(&handler, "duplicate-last-pane-keeper").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(source.clone(), 1),
        )),
    )
    .await;
    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&source, 0), 2);
        assert_eq!(state.window_linked_session_count(&source, 0), 1);
    }

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(source.clone(), 0, 0),
            kill_all_except: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&source).is_none());
    assert!(state.sessions.session(&keeper).is_some());
}

#[tokio::test]
async fn duplicate_alias_last_pane_break_moves_one_alias_and_preserves_the_linked_peer() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "duplicate-last-pane-break").await;
    let destination = SessionSpec::create(&handler, "duplicate-last-pane-destination").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(source.clone(), 1),
        )),
    )
    .await;
    let source_pane_id = {
        let state = handler.state.lock().await;
        pane_id(&state, &source, 0, 0)
    };
    let mut events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source.clone(), 0, 0),
            WindowTarget::with_window(destination.clone(), 1),
        )),
    )
    .await;

    let state = handler.state.lock().await;
    let source_session = state
        .sessions
        .session(&source)
        .expect("the unbroken alias keeps the source session alive");
    assert!(source_session.window_at(0).is_none());
    let source_alias = source_session.window_at(1).expect("source alias survives");
    let destination_window = state
        .sessions
        .session(&destination)
        .and_then(|session| session.window_at(1))
        .expect("broken alias moved to the destination");
    assert_eq!(source_alias.id(), destination_window.id());
    assert_eq!(
        source_alias.panes().first().map(rmux_core::Pane::id),
        Some(source_pane_id)
    );
    assert_eq!(pane_id(&state, &destination, 1, 0), source_pane_id);
    assert_eq!(state.window_link_count(&destination, 1), 2);
    drop(state);

    let unlinked_targets = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.event {
            LifecycleEvent::WindowUnlinked { target, .. } => target,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        unlinked_targets,
        vec![WindowTarget::with_window(source.clone(), 0)],
        "the surviving alias must not emit WindowUnlinked"
    );
}

#[tokio::test]
async fn natural_grouped_last_pane_exit_matches_tmux_3_7b_family_lifecycle() {
    let handler = RequestHandler::new();
    let (keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "natural-family-exit").await;
    install_family_hooks(&handler).await;
    let mut events = handler.subscribe_lifecycle_events();
    let target = PaneTarget::with_window(owner.clone(), 0, 0);
    {
        let mut state = handler.state.lock().await;
        state
            .mark_pane_dead_without_exit_details(&target)
            .expect("mark grouped pane naturally exited");
    }

    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            owner.clone(),
            family_pane_id,
            None,
        ))
        .await;

    let mut expected = vec![(HookName::PaneExited, owner.clone())];
    expected.extend(grouped_kill_order(&owner, &peer));
    assert_lifecycle_batch(&mut events, &expected);
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_none());
    assert!(state.sessions.session(&peer).is_none());
    assert!(state.sessions.session(&keeper).is_some());
}

#[tokio::test]
async fn natural_single_last_pane_exit_preserves_existing_lifecycle_order() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "natural-single-source").await;
    let keeper = SessionSpec::create(&handler, "natural-single-keeper").await;
    handler.wait_for_initial_panes_for_test().await;
    install_family_hooks(&handler).await;
    let mut events = handler.subscribe_lifecycle_events();
    let target = PaneTarget::with_window(source.clone(), 0, 0);
    let source_pane_id = {
        let mut state = handler.state.lock().await;
        let source_pane_id = pane_id(&state, &source, 0, 0);
        state
            .mark_pane_dead_without_exit_details(&target)
            .expect("mark single pane naturally exited");
        source_pane_id
    };

    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            source.clone(),
            source_pane_id,
            None,
        ))
        .await;

    assert_lifecycle_batch(
        &mut events,
        &[
            (HookName::PaneExited, source.clone()),
            (HookName::WindowUnlinked, source.clone()),
            (HookName::SessionClosed, source.clone()),
        ],
    );
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&source).is_none());
    assert!(state.sessions.session(&keeper).is_some());
}

#[tokio::test]
async fn natural_linked_last_pane_exit_emits_each_tmux_3_7b_window_unlinked_hook() {
    let handler = RequestHandler::new();
    let source = SessionSpec::create(&handler, "natural-linked-source").await;
    let survivor = SessionSpec::create(&handler, "natural-linked-survivor").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(survivor.clone(), 1),
        )),
    )
    .await;
    handler.wait_for_initial_panes_for_test().await;
    install_family_hooks(&handler).await;
    let mut events = handler.subscribe_lifecycle_events();
    let target = PaneTarget::with_window(source.clone(), 0, 0);
    let linked_pane_id = {
        let mut state = handler.state.lock().await;
        let linked_pane_id = pane_id(&state, &source, 0, 0);
        state
            .mark_pane_dead_without_exit_details(&target)
            .expect("mark linked pane naturally exited");
        linked_pane_id
    };

    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            source.clone(),
            linked_pane_id,
            None,
        ))
        .await;

    assert_lifecycle_batch(
        &mut events,
        &[
            (HookName::PaneExited, source.clone()),
            (HookName::WindowUnlinked, source.clone()),
            (HookName::SessionClosed, source.clone()),
            (HookName::WindowUnlinked, survivor.clone()),
        ],
    );
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&source).is_none());
    let survivor_session = state
        .sessions
        .session(&survivor)
        .expect("surviving linked session remains");
    assert_eq!(
        survivor_session
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0]
    );
}
