use rmux_proto::{
    BreakPaneRequest, JoinPaneRequest, LinkWindowRequest, MovePaneRequest, MoveWindowRequest,
    NewWindowRequest, PaneKillRequest, PaneTargetRef, SplitWindowRequest, SwapPaneRequest,
    SwapWindowRequest, UnlinkWindowRequest, WindowTarget,
};

use crate::test_fixtures::Grouped;

use super::*;

const CONNECTION_ID: u64 = 81;

#[derive(Clone, Copy, Debug)]
enum TransferCase {
    Swap,
    Join,
    Move,
    Break,
}

impl TransferCase {
    const fn label(self) -> &'static str {
        match self {
            Self::Swap => "swap",
            Self::Join => "join",
            Self::Move => "move",
            Self::Break => "break",
        }
    }
}

#[tokio::test]
async fn swap_rekeys_existing_pane_output_subscription() {
    assert_subscription_follows_transfer(TransferCase::Swap).await;
}

#[tokio::test]
async fn join_rekeys_existing_pane_output_subscription() {
    assert_subscription_follows_transfer(TransferCase::Join).await;
}

#[tokio::test]
async fn move_rekeys_existing_pane_output_subscription() {
    assert_subscription_follows_transfer(TransferCase::Move).await;
}

#[tokio::test]
async fn break_rekeys_existing_pane_output_subscription() {
    assert_subscription_follows_transfer(TransferCase::Break).await;
}

#[tokio::test]
async fn swap_between_group_aliases_rekeys_subscription_to_linked_runtime_owner() {
    let handler = RequestHandler::new();
    let owner = handler
        .create_session("subscription-group-swap-owner")
        .await;
    handler.handle_ok(SplitWindowRequest::fixture(&owner)).await;
    let linked_owner = handler
        .create_session("subscription-group-swap-linked")
        .await;
    handler
        .handle_ok(SplitWindowRequest::fixture(&linked_owner))
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(linked_owner.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
        )))
        .await;
    let peer = handler
        .create_session(Grouped("subscription-group-swap-peer", &owner))
        .await;
    handler.wait_for_initial_panes_for_test().await;

    let source = PaneTarget::with_window(owner.clone(), 0, 0);
    let (subscription_id, pane_id) = subscribe_by_id(&handler, CONNECTION_ID, &source).await;
    handler
        .handle_ok(SwapPaneRequest::fixture((
            source,
            PaneTarget::with_window(peer, 1, 0),
        )))
        .await;

    assert_window_owner_transfer(
        &handler,
        subscription_id,
        pane_id,
        linked_owner,
        "group-alias swap-pane",
    )
    .await;
}

#[tokio::test]
async fn unlink_window_rekeys_subscription_when_runtime_owner_slot_is_removed() {
    let handler = RequestHandler::new();
    let owner = handler
        .create_session(Sizeless("subscription-unlink-owner"))
        .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let external = handler
        .create_session(Sizeless("subscription-unlink-external"))
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )))
        .await;

    let source = PaneTarget::with_window(owner.clone(), 0, 0);
    let (subscription_id, pane_id) = subscribe_by_id(&handler, CONNECTION_ID, &source).await;
    handler
        .handle_ok(UnlinkWindowRequest {
            target: WindowTarget::with_window(owner, 0),
            kill_if_last: false,
        })
        .await;

    assert_window_owner_transfer(
        &handler,
        subscription_id,
        pane_id,
        external,
        "unlink-window",
    )
    .await;
}

#[tokio::test]
async fn link_window_k_rekeys_subscription_for_detached_destination_runtime() {
    let handler = RequestHandler::new();
    let owner = handler
        .create_session(Sizeless("subscription-link-owner"))
        .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let external = handler
        .create_session(Sizeless("subscription-link-external"))
        .await;
    let replacement = handler
        .create_session(Sizeless("subscription-link-replacement"))
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )))
        .await;

    let source = PaneTarget::with_window(owner.clone(), 0, 0);
    let (subscription_id, pane_id) = subscribe_by_id(&handler, CONNECTION_ID, &source).await;
    handler
        .handle_ok(LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(replacement, 0),
                WindowTarget::with_window(owner, 0),
            ))
        })
        .await;

    assert_window_owner_transfer(
        &handler,
        subscription_id,
        pane_id,
        external,
        "link-window -k",
    )
    .await;
}

#[tokio::test]
async fn move_window_rekeys_subscription_across_sessions() {
    let handler = RequestHandler::new();
    let source_name = handler
        .create_session(Sizeless("subscription-move-window-source"))
        .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&source_name)
        })
        .await;
    let target_name = handler
        .create_session(Sizeless("subscription-move-window-target"))
        .await;

    let source = PaneTarget::with_window(source_name.clone(), 0, 0);
    let (subscription_id, pane_id) = subscribe_by_id(&handler, CONNECTION_ID, &source).await;
    handler
        .handle_ok(MoveWindowRequest::fixture((
            WindowTarget::with_window(source_name, 0),
            WindowTarget::with_window(target_name.clone(), 1),
        )))
        .await;

    assert_window_owner_transfer(
        &handler,
        subscription_id,
        pane_id,
        target_name,
        "move-window",
    )
    .await;
}

#[tokio::test]
async fn swap_window_rekeys_subscription_across_sessions() {
    let handler = RequestHandler::new();
    let source_name = handler
        .create_session(Sizeless("subscription-swap-window-source"))
        .await;
    let target_name = handler
        .create_session(Sizeless("subscription-swap-window-target"))
        .await;

    let source = PaneTarget::with_window(source_name.clone(), 0, 0);
    let (subscription_id, pane_id) = subscribe_by_id(&handler, CONNECTION_ID, &source).await;
    handler
        .handle_ok(SwapWindowRequest {
            source: WindowTarget::with_window(source_name, 0),
            target: WindowTarget::with_window(target_name.clone(), 0),
            detached: true,
        })
        .await;

    assert_window_owner_transfer(
        &handler,
        subscription_id,
        pane_id,
        target_name,
        "swap-window",
    )
    .await;
}

async fn assert_subscription_follows_transfer(case: TransferCase) {
    let handler = RequestHandler::new();
    let source_name =
        SessionName::new(format!("subscription-{}-source", case.label())).expect("valid source");
    let target_name =
        SessionName::new(format!("subscription-{}-target", case.label())).expect("valid target");
    handler.create_session(Sizeless(&source_name)).await;
    handler.create_session(Sizeless(&target_name)).await;

    let source_target = PaneTarget::with_window(source_name.clone(), 0, 0);
    let target_target = PaneTarget::with_window(target_name.clone(), 0, 0);
    let (subscription_id, source_pane_id) =
        subscribe_by_id(&handler, CONNECTION_ID, &source_target).await;

    let response = match case {
        TransferCase::Swap => {
            handler
                .handle(Request::SwapPane(SwapPaneRequest {
                    detached: false,
                    ..Fixture::fixture((source_target, target_target))
                }))
                .await
        }
        TransferCase::Join => {
            handler
                .handle(Request::JoinPane(JoinPaneRequest {
                    detached: false,
                    ..Fixture::fixture((source_target, target_target))
                }))
                .await
        }
        TransferCase::Move => {
            handler
                .handle(Request::MovePane(MovePaneRequest {
                    detached: false,
                    ..Fixture::fixture((source_target, target_target))
                }))
                .await
        }
        TransferCase::Break => {
            handler
                .handle(Request::BreakPane(Box::new(BreakPaneRequest {
                    detached: false,
                    ..Fixture::fixture((
                        source_target,
                        WindowTarget::with_window(target_name.clone(), 1),
                    ))
                })))
                .await
        }
    };
    assert_transfer_succeeded(case, &response);

    let moved_target = pane_target_for_id(&handler, &target_name, source_pane_id)
        .await
        .expect("moved pane is reachable by stable id");
    let canonical_key = {
        let state = handler.state.lock().await;
        state
            .pane_output_subscription_key_for_pane_id(source_pane_id)
            .expect("moved pane has a canonical output key")
    };
    let registered_key = handler
        .pane_output_subscription_key_for_test(subscription_id)
        .expect("subscription survives the transfer");
    assert_eq!(
        registered_key,
        canonical_key,
        "{} canonical key",
        case.label()
    );
    assert_eq!(
        registered_key.runtime_session_name(),
        &target_name,
        "{} moves the output owner to the destination session",
        case.label()
    );

    let expected = format!("{}-after-transfer", case.label()).into_bytes();
    handler
        .send_pane_output_for_test(&moved_target, expected.clone())
        .await;
    let cursor = handler
        .handle_pane_output_cursor(
            CONNECTION_ID,
            PaneOutputCursorRequest {
                subscription_id,
                max_events: Some(16),
            },
        )
        .await;
    let Response::PaneOutputCursor(cursor) = cursor else {
        panic!(
            "{} moved subscription should remain readable: {cursor:?}",
            case.label()
        );
    };
    assert!(
        cursor.events.iter().any(|event| event.bytes == expected),
        "{} receiver must remain attached to the moved sender",
        case.label()
    );

    let killed = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(target_name, source_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(
        matches!(killed, Response::KillPane(_)),
        "{} moved pane cleanup should succeed: {killed:?}",
        case.label()
    );
    assert_killed_subscription_drains_then_expires(&handler, subscription_id, case.label()).await;
}

async fn assert_window_owner_transfer(
    handler: &RequestHandler,
    subscription_id: rmux_proto::PaneOutputSubscriptionId,
    pane_id: PaneId,
    destination_session: SessionName,
    label: &str,
) {
    let moved_target = pane_target_for_id(handler, &destination_session, pane_id)
        .await
        .expect("moved pane is reachable by stable id");
    let canonical_key = {
        let state = handler.state.lock().await;
        state
            .pane_output_subscription_key_for_pane_id(pane_id)
            .expect("moved window pane has a canonical output key")
    };
    let registered_key = handler
        .pane_output_subscription_key_for_test(subscription_id)
        .expect("subscription survives the window owner transfer");
    assert_eq!(registered_key, canonical_key, "{label} canonical key");
    assert_eq!(
        registered_key.runtime_session_name(),
        &destination_session,
        "{label} moves the output owner to the surviving destination"
    );

    let expected = format!("{label}-after-transfer").into_bytes();
    handler
        .send_pane_output_for_test(&moved_target, expected.clone())
        .await;
    let cursor = handler
        .handle_pane_output_cursor(
            CONNECTION_ID,
            PaneOutputCursorRequest {
                subscription_id,
                max_events: Some(16),
            },
        )
        .await;
    let Response::PaneOutputCursor(cursor) = cursor else {
        panic!("{label} moved subscription should remain readable: {cursor:?}");
    };
    assert!(
        cursor.events.iter().any(|event| event.bytes == expected),
        "{label} receiver follows the moved sender"
    );

    let killed = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(destination_session, pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(
        matches!(killed, Response::KillPane(_)),
        "{label}: {killed:?}"
    );
    assert_killed_subscription_drains_then_expires(handler, subscription_id, label).await;
}

async fn assert_killed_subscription_drains_then_expires(
    handler: &RequestHandler,
    subscription_id: rmux_proto::PaneOutputSubscriptionId,
    label: &str,
) {
    let pane = handler
        .pane_output_subscription_key_for_test(subscription_id)
        .expect("rekeyed subscription remains registered during pane-output drain");
    assert!(
        handler
            .subscriptions
            .lock()
            .expect("subscription registry mutex must not be poisoned")
            .pane_is_draining(&pane),
        "{label} kill must start the rekeyed subscription drain"
    );

    let cursor_during_drain = handler
        .handle_pane_output_cursor(
            CONNECTION_ID,
            PaneOutputCursorRequest {
                subscription_id,
                max_events: Some(1),
            },
        )
        .await;
    assert!(
        matches!(cursor_during_drain, Response::PaneOutputCursor(_)),
        "{label} rekeyed subscription must remain readable during pane-output drain: {cursor_during_drain:?}"
    );

    handler
        .subscriptions
        .lock()
        .expect("subscription registry mutex must not be poisoned")
        .expire_pane_drain(&pane, std::time::Instant::now());

    let cursor_after_expiration = handler
        .handle_pane_output_cursor(
            CONNECTION_ID,
            PaneOutputCursorRequest {
                subscription_id,
                max_events: Some(1),
            },
        )
        .await;
    assert!(
        matches!(
            cursor_after_expiration,
            Response::Error(ref error) if error.error.to_string().contains("subscription not found")
        ),
        "{label} drain expiration must clean the rekeyed record: {cursor_after_expiration:?}"
    );
}

fn assert_transfer_succeeded(case: TransferCase, response: &Response) {
    let succeeded = matches!(
        (case, response),
        (TransferCase::Swap, Response::SwapPane(_))
            | (TransferCase::Join, Response::JoinPane(_))
            | (TransferCase::Move, Response::MovePane(_))
            | (TransferCase::Break, Response::BreakPane(_))
    );
    assert!(succeeded, "{} transfer failed: {response:?}", case.label());
}
