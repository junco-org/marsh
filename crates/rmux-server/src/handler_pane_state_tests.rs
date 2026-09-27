use std::{
    sync::{Arc, Barrier},
    time::Duration,
};

use super::RequestHandler;
use crate::pane_io::PaneExitEvent;
use crate::pane_state_journal::{
    PaneStateChange, PANE_STATE_JOURNAL_BYTE_CAPACITY, PANE_STATE_JOURNAL_CAPACITY,
};
use rmux_core::{events::SubscriptionLimits, PaneId};
use rmux_proto::{
    encode_frame, ErrorResponse, KillSessionRequest, KillWindowRequest, LinkWindowRequest,
    MoveWindowRequest, NewWindowRequest, OptionScopeSelector, PaneKillRequest,
    PaneOptionGetRequest, PaneOptionSetRequest, PaneStateClosedReason, PaneStateCursorRequest,
    PaneStateEventDto, PaneTarget, PaneTargetRef, Request, RespawnPaneRequest,
    RespawnWindowRequest, Response, RmuxError, SelectPaneRequest, SessionName,
    SetOptionByNameRequest, SetOptionMode, SourceFileRequest, SplitWindowRequest,
    SubscribePaneStateRequest, UnlinkWindowRequest, WindowTarget,
};

#[path = "handler_pane_state_tests/foreground_watch_lifecycle.rs"]
mod foreground_watch_lifecycle;

use crate::test_fixtures::Fixture;

async fn create_session_with_pane(
    handler: &RequestHandler,
    name: &str,
) -> (SessionName, PaneTarget, PaneId) {
    let session = handler.create_session(name).await;
    let target = PaneTarget::new(session.clone(), 0);
    let pane_id = pane_id_at(handler, &target).await;
    (session, target, pane_id)
}

async fn create_window_with_pane(
    handler: &RequestHandler,
    session: &SessionName,
    window_index: u32,
) -> (PaneTarget, PaneId) {
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(window_index),
            ..Fixture::fixture(session)
        })
        .await;
    let target = PaneTarget::with_window(session.clone(), window_index, 0);
    let pane_id = pane_id_at(handler, &target).await;
    (target, pane_id)
}

async fn pane_id_at(handler: &RequestHandler, target: &PaneTarget) -> PaneId {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .and_then(|window| window.pane(target.pane_index()))
        .map(|pane| pane.id())
        .expect("target pane exists")
}

async fn subscribe(
    handler: &RequestHandler,
    connection_id: u64,
    target: PaneTarget,
    include_title: bool,
    include_options: bool,
) -> rmux_proto::PaneStateSubscriptionId {
    handler
        .subscribe_ok(
            connection_id,
            SubscribePaneStateRequest {
                include_title,
                include_options,
                ..Fixture::fixture(target)
            },
        )
        .await
        .subscription_id
}

fn expect_single_pane_state_event(response: Response) -> PaneStateEventDto {
    match response {
        Response::PaneStateCursor(mut response) => {
            assert_eq!(
                response.events.len(),
                1,
                "expected exactly one pane-state event, got {:?}",
                response.events
            );
            response.events.remove(0)
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn pane_state_cursor_delivers_matching_pane_events_with_global_revisions() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-revisions").await;
    let subscription_id = subscribe(&handler, 91, target, true, true).await;

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::TitleChanged {
            old: "old".to_owned(),
            new: "new".to_owned(),
        },
    );
    handler.record_pane_state_change(
        PaneId::new(999),
        Some(1),
        PaneStateChange::TitleChanged {
            old: "other-old".to_owned(),
            new: "other-new".to_owned(),
        },
    );
    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::OptionSet {
            name: "@agent.kind".to_owned(),
            old: None,
            new: "assistant".to_owned(),
        },
    );

    match handler
        .read_pane_state_cursor_for_test(91, subscription_id, 0)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.next_revision, 3);
            assert_eq!(response.events.len(), 2);
            assert!(matches!(
                &response.events[0],
                PaneStateEventDto::TitleChanged {
                    revision: 1,
                    pane_id: event_pane_id,
                    new_title,
                    ..
                } if *event_pane_id == pane_id && new_title == "new"
            ));
            assert!(matches!(
                &response.events[1],
                PaneStateEventDto::OptionSet {
                    revision: 3,
                    pane_id: event_pane_id,
                    name,
                    new_value,
                    ..
                } if *event_pane_id == pane_id
                    && name == "@agent.kind"
                    && new_value == "assistant"
            ));
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn pane_state_cursor_lag_returns_rebased_snapshot() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) = create_session_with_pane(&handler, "pane-state-lag").await;
    let subscription_id = subscribe(&handler, 92, target, true, false).await;

    for index in 0..=PANE_STATE_JOURNAL_CAPACITY {
        handler.record_pane_state_change(
            pane_id,
            Some(1),
            PaneStateChange::TitleChanged {
                old: index.to_string(),
                new: (index + 1).to_string(),
            },
        );
    }

    match handler
        .read_pane_state_cursor_for_test(92, subscription_id, 0)
        .await
    {
        Response::PaneStateLag(response) => {
            assert_eq!(response.subscription_id, subscription_id);
            assert_eq!(response.missed_from_revision, 0);
            assert!(response.resume_revision > 0);
            assert_eq!(
                response.snapshot.revision,
                (PANE_STATE_JOURNAL_CAPACITY + 1) as u64
            );
            assert!(response.snapshot.title.is_some());
        }
        response => panic!("expected pane-state lag response, got {response:?}"),
    }
}

#[tokio::test]
async fn pane_state_closed_reason_variants_are_delivered_before_end_of_stream() {
    let handler = RequestHandler::new();
    for (index, reason) in [
        ("exited", PaneStateClosedReason::Exited),
        ("died-kept", PaneStateClosedReason::DiedKept),
        ("killed", PaneStateClosedReason::Killed),
    ] {
        let (_session, target, pane_id) =
            create_session_with_pane(&handler, &format!("pane-state-closed-{index}")).await;
        let subscription_id = subscribe(&handler, 93, target, false, false).await;
        handler.record_pane_state_change(pane_id, Some(1), PaneStateChange::Closed { reason });

        match handler
            .read_pane_state_cursor_for_test(93, subscription_id, 0)
            .await
        {
            Response::PaneStateCursor(response) => {
                assert_eq!(response.events.len(), 1);
                assert!(matches!(
                    response.events.as_slice(),
                    [PaneStateEventDto::Closed {
                        reason: event_reason,
                        ..
                    }] if *event_reason == reason
                ));
            }
            response => panic!("pane-state cursor failed: {response:?}"),
        }

        match handler
            .read_pane_state_cursor_for_test(93, subscription_id, 1)
            .await
        {
            Response::Error(error) => assert!(matches!(
                error.error,
                RmuxError::Server(message) if message == "subscription not found"
            )),
            response => panic!("closed subscription should be forgotten, got {response:?}"),
        }
    }
}

#[tokio::test]
async fn duplicate_closed_for_same_pane_is_suppressed_until_reopened() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-duplicate-closed").await;
    let subscription_id = subscribe(&handler, 99, target.clone(), false, false).await;

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::DiedKept,
        },
    );
    handler.record_pane_state_change(
        pane_id,
        None,
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::Killed,
        },
    );

    match handler
        .read_pane_state_cursor_for_test(99, subscription_id, 0)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(matches!(
                response.events.as_slice(),
                [PaneStateEventDto::Closed {
                    reason: PaneStateClosedReason::DiedKept,
                    ..
                }]
            ));
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }

    handler.reopen_pane_state(pane_id);
    let after_reopen_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    let reopened_subscription_id = subscribe(&handler, 100, target, false, false).await;
    handler.record_pane_state_change(
        pane_id,
        Some(2),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::Killed,
        },
    );

    match handler
        .read_pane_state_cursor_for_test(100, reopened_subscription_id, after_reopen_revision)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(matches!(
                response.events.as_slice(),
                [PaneStateEventDto::Closed {
                    pane_id: event_pane_id,
                    reason: PaneStateClosedReason::Killed,
                    ..
                }] if *event_pane_id == pane_id
            ));
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn pane_state_late_subscription_after_died_kept_receives_killed_close() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-late-died-kept-killed").await;

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::DiedKept,
        },
    );
    let first_closed_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    let subscription_id = subscribe(&handler, 106, target, false, false).await;
    handler.record_pane_state_change(
        pane_id,
        None,
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::Killed,
        },
    );

    match handler
        .read_pane_state_cursor_for_test(106, subscription_id, first_closed_revision)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(matches!(
                response.events.as_slice(),
                [PaneStateEventDto::Closed {
                    pane_id: event_pane_id,
                    reason: PaneStateClosedReason::Killed,
                    ..
                }] if *event_pane_id == pane_id
            ));
        }
        response => panic!("late subscription should receive final killed close, got {response:?}"),
    }
}

#[tokio::test]
async fn respawn_pane_reopens_pane_state_before_future_close() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-respawn-pane-reopen").await;

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::DiedKept,
        },
    );

    handler
        .handle_ok(RespawnPaneRequest::fixture(&target))
        .await;

    let after_respawn_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    let subscription_id = subscribe(&handler, 104, target, false, false).await;
    handler.record_pane_state_change(
        pane_id,
        Some(2),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::Killed,
        },
    );

    match handler
        .read_pane_state_cursor_for_test(104, subscription_id, after_respawn_revision)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(matches!(
                response.events.as_slice(),
                [PaneStateEventDto::Closed {
                    pane_id: event_pane_id,
                    reason: PaneStateClosedReason::Killed,
                    ..
                }] if *event_pane_id == pane_id
            ));
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn subscription_after_kept_exit_and_respawn_receives_new_generation_events() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-respawn-generation").await;

    let remain_on_exit = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: PaneTargetRef::slot(target.clone()),
            name: "remain-on-exit".to_owned(),
            value: Some("on".to_owned()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(remain_on_exit, Response::PaneOptionSet(_)),
        "{remain_on_exit:?}"
    );

    let generation = mark_pane_exited(&handler, &target).await;
    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            session,
            pane_id,
            Some(generation),
        ))
        .await;

    handler
        .handle_ok(RespawnPaneRequest::fixture(&target))
        .await;

    let subscription_id = subscribe(&handler, 107, target.clone(), false, true).await;
    let after_subscription = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    let changed = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: PaneTargetRef::slot(target),
            name: "@post-respawn".to_owned(),
            value: Some("visible".to_owned()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(matches!(changed, Response::PaneOptionSet(_)), "{changed:?}");

    match handler
        .read_pane_state_cursor_for_test(107, subscription_id, after_subscription)
        .await
    {
        Response::PaneStateCursor(response) => assert!(matches!(
            response.events.as_slice(),
            [PaneStateEventDto::OptionSet {
                name, new_value, ..
            }] if name == "@post-respawn" && new_value == "visible"
        )),
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn respawn_window_reopens_retained_pane_state_before_future_close() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-respawn-window-reopen").await;

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::DiedKept,
        },
    );

    handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(session, 0),
            kill: true,
            environment: None,
            command: None,
            start_directory: None,
        })
        .await;

    let after_respawn_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    let subscription_id = subscribe(&handler, 105, target, false, false).await;
    handler.record_pane_state_change(
        pane_id,
        Some(2),
        PaneStateChange::Closed {
            reason: PaneStateClosedReason::Killed,
        },
    );

    match handler
        .read_pane_state_cursor_for_test(105, subscription_id, after_respawn_revision)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(matches!(
                response.events.as_slice(),
                [PaneStateEventDto::Closed {
                    pane_id: event_pane_id,
                    reason: PaneStateClosedReason::Killed,
                    ..
                }] if *event_pane_id == pane_id
            ));
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn pane_state_wait_cursor_advances_past_filtered_events_on_timeout() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-filtered-wait").await;
    let subscription_id = subscribe(&handler, 94, target, false, false).await;
    let after_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();

    handler.record_pane_state_change(
        pane_id,
        Some(1),
        PaneStateChange::OptionSet {
            name: "@agent.kind".to_owned(),
            old: None,
            new: "assistant".to_owned(),
        },
    );
    let expected_revision = handler
        .pane_state_journal
        .lock()
        .expect("pane-state journal lock should not be poisoned")
        .current_revision();
    assert_eq!(expected_revision, after_revision.saturating_add(1));

    match handler
        .handle_pane_state_cursor(
            94,
            PaneStateCursorRequest {
                subscription_id,
                after_revision,
                wait: true,
                max_events: Some(16),
            },
        )
        .await
    {
        Response::PaneStateCursor(response) => {
            assert!(
                response.events.is_empty(),
                "filtered wait must stay empty, got {:?}",
                response.events
            );
            assert_eq!(
                response.next_revision, expected_revision,
                "filtered wait must advance exactly past the filtered event"
            );
        }
        response => panic!("pane-state wait cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn set_option_by_name_pane_origin_emits_pane_state_option_event() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-set-option-origin").await;
    let subscription_id = subscribe(&handler, 980, target.clone(), false, true).await;

    handler
        .set_option_by_name(
            OptionScopeSelector::Pane(target),
            "@d2.set-option",
            "direct",
        )
        .await;

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(980, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::OptionSet {
            pane_id: event_pane_id,
            name,
            old_value: None,
            new_value,
            ..
        } if event_pane_id == pane_id && name == "@d2.set-option" && new_value == "direct"
    ));
}

#[tokio::test]
async fn pane_option_set_sdk_origin_emits_pane_state_option_event() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-sdk-option-origin").await;
    let subscription_id = subscribe(&handler, 981, target.clone(), false, true).await;

    let response = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: PaneTargetRef::by_id(session, pane_id),
            name: "@d2.sdk".to_owned(),
            value: Some("sdk".to_owned()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(response, Response::PaneOptionSet(_)),
        "{response:?}"
    );

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(981, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::OptionSet {
            pane_id: event_pane_id,
            name,
            old_value: None,
            new_value,
            ..
        } if event_pane_id == pane_id && name == "@d2.sdk" && new_value == "sdk"
    ));
}

#[tokio::test]
async fn oversized_pane_option_response_fails_before_committing() {
    let handler = RequestHandler::new();
    let (session, _target, pane_id) =
        create_session_with_pane(&handler, "pane-option-frame-preflight").await;
    let target = PaneTargetRef::by_id(session, pane_id);
    let first = "A".repeat(4_300_000);
    let second = "B".repeat(4_300_000);

    let response = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: target.clone(),
            name: "@large".to_owned(),
            value: Some(first.clone()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(response, Response::PaneOptionSet(_)),
        "{response:?}"
    );

    let response = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: target.clone(),
            name: "@large".to_owned(),
            value: Some(second),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(
            response,
            Response::Error(ErrorResponse {
                error: RmuxError::FrameTooLarge { .. }
            })
        ),
        "{response:?}"
    );

    let response = handler
        .handle(Request::PaneOptionGet(PaneOptionGetRequest {
            target,
            name: "@large".to_owned(),
        }))
        .await;
    match response {
        Response::PaneOptionGet(response) => {
            assert_eq!(response.value.as_deref(), Some(first.as_str()))
        }
        response => panic!("pane option get failed: {response:?}"),
    }
}

#[tokio::test]
async fn pane_option_set_sdk_origin_records_option_event_when_resize_fails() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-sdk-option-resize-error").await;
    let subscription_id = subscribe(&handler, 984, target.clone(), false, true).await;
    {
        let mut state = handler.state.lock().await;
        state.fail_next_resize_for_test();
    }

    let response = handler
        .handle(Request::PaneOptionSet(PaneOptionSetRequest {
            target: PaneTargetRef::by_id(session, pane_id),
            name: "pane-border-status".to_owned(),
            value: Some("top".to_owned()),
            mode: SetOptionMode::Replace,
            unset: false,
        }))
        .await;
    assert!(
        matches!(response, Response::Error(ErrorResponse { error: RmuxError::Server(ref message) }) if message == "injected pane terminal resize failure"),
        "{response:?}"
    );

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(984, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::OptionSet {
            pane_id: event_pane_id,
            name,
            old_value: None,
            new_value,
            ..
        } if event_pane_id == pane_id && name == "pane-border-status" && new_value == "top"
    ));
}

#[tokio::test]
async fn select_pane_title_origin_emits_pane_state_title_event() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-select-title-origin").await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    let subscription_id = subscribe(&handler, 982, target.clone(), true, false).await;

    handler
        .handle_ok(SelectPaneRequest {
            title: Some("selected-title".to_owned()),
            ..Fixture::fixture(target)
        })
        .await;

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(982, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::TitleChanged {
            pane_id: event_pane_id,
            new_title,
            ..
        } if event_pane_id == pane_id && new_title == "selected-title"
    ));
}

#[tokio::test]
async fn oversized_title_history_rebases_to_a_frameable_snapshot() {
    let handler = RequestHandler::new();
    let (_session, target, _pane_id) =
        create_session_with_pane(&handler, "pane-state-large-title-rebase").await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    let subscription_id = subscribe(&handler, 985, target.clone(), true, false).await;

    let first_title = "a".repeat(PANE_STATE_JOURNAL_BYTE_CAPACITY + 1024);
    handler
        .handle_ok(SelectPaneRequest {
            title: Some(first_title.clone()),
            ..Fixture::fixture(&target)
        })
        .await;

    let first_resume = match handler
        .read_pane_state_cursor_for_test(985, subscription_id, 0)
        .await
    {
        Response::PaneStateLag(response) => {
            assert_eq!(
                response.snapshot.title.as_deref(),
                Some(first_title.as_str())
            );
            encode_frame(&Response::PaneStateLag(response.clone()))
                .expect("large rebased snapshot stays below the detached frame limit");
            response.resume_revision
        }
        response => panic!("oversized retained event must rebase: {response:?}"),
    };

    let second_title = "b".repeat(PANE_STATE_JOURNAL_BYTE_CAPACITY + 1024);
    handler
        .handle_ok(SelectPaneRequest {
            title: Some(second_title.clone()),
            ..Fixture::fixture(target)
        })
        .await;
    match handler
        .read_pane_state_cursor_for_test(985, subscription_id, first_resume)
        .await
    {
        Response::PaneStateLag(response) => {
            assert_eq!(
                response.snapshot.title.as_deref(),
                Some(second_title.as_str())
            );
            encode_frame(&Response::PaneStateLag(response))
                .expect("repeated large titles must not close the SDK transport");
        }
        response => panic!("second oversized retained event must rebase: {response:?}"),
    }
}

#[tokio::test]
async fn oversized_initial_option_snapshot_fails_without_leaking_subscription() {
    let handler = RequestHandler::new();
    let (_session, target, _pane_id) =
        create_session_with_pane(&handler, "pane-state-large-option-initial").await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    handler
        .set_option_by_name(
            OptionScopeSelector::Pane(target.clone()),
            "@large-initial",
            &"x".repeat(super::pane_state_support::PANE_STATE_SNAPSHOT_OPTION_BYTE_LIMIT + 1),
        )
        .await;

    let response = handler
        .handle_subscribe_pane_state(
            986,
            SubscribePaneStateRequest {
                include_options: true,
                ..Fixture::fixture(&target)
            },
        )
        .await;
    let Response::Error(ErrorResponse { error }) = response else {
        panic!("oversized initial snapshot must fail explicitly: {response:?}");
    };
    assert!(
        error.to_string().contains("snapshot options exceed"),
        "{error}"
    );

    handler
        .handle_ok(SetOptionByNameRequest {
            value: None,
            unset: true,
            ..Fixture::fixture((
                OptionScopeSelector::Pane(target.clone()),
                "@large-initial",
                "",
            ))
        })
        .await;
    let _subscription_id = subscribe(&handler, 986, target, false, true).await;
}

#[tokio::test]
async fn oversized_lag_snapshot_closes_the_subscription_instead_of_looping() {
    let handler = RequestHandler::new();
    let (_session, target, _pane_id) =
        create_session_with_pane(&handler, "pane-state-large-option-lag").await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    let subscription_id = subscribe(&handler, 987, target.clone(), false, true).await;

    handler
        .set_option_by_name(
            OptionScopeSelector::Pane(target),
            "@large-lag",
            &"y".repeat(PANE_STATE_JOURNAL_BYTE_CAPACITY + 1024),
        )
        .await;

    let response = handler
        .read_pane_state_cursor_for_test(987, subscription_id, 0)
        .await;
    let Response::Error(ErrorResponse { error }) = response else {
        panic!("oversized lag snapshot must fail explicitly: {response:?}");
    };
    assert!(
        error.to_string().contains("snapshot options exceed"),
        "{error}"
    );

    let response = handler
        .read_pane_state_cursor_for_test(987, subscription_id, 0)
        .await;
    let Response::Error(ErrorResponse { error }) = response else {
        panic!("failed lag snapshot must close its subscription: {response:?}");
    };
    assert_eq!(
        error,
        RmuxError::Server("subscription not found".to_owned())
    );
}

#[tokio::test]
async fn select_pane_style_origin_emits_pane_state_option_event() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-select-style-origin").await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    let subscription_id = subscribe(&handler, 983, target.clone(), false, true).await;
    let resize_count_before = {
        let mut state = handler.state.lock().await;
        let resize_count = state.window_runtime_resize_count_for_test();
        state.fail_next_resize_for_test();
        resize_count
    };

    handler
        .handle_ok(SelectPaneRequest {
            style: Some("fg=red".to_owned()),
            ..Fixture::fixture(target)
        })
        .await;
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .window_runtime_resize_count_for_test(),
        resize_count_before,
        "re-selecting the active pane must not resize its unchanged runtime"
    );

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(983, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::OptionSet {
            pane_id: event_pane_id,
            name,
            old_value: None,
            new_value,
            ..
        } if event_pane_id == pane_id && name == "window-style" && new_value == "fg=red"
    ));
}

#[tokio::test]
async fn source_file_pane_option_origin_emits_pane_state_option_event() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-source-file-origin").await;
    let subscription_id = subscribe(&handler, 984, target.clone(), false, true).await;
    let source = format!("set-option -p -t {target} @d2.source stdin\n");

    handler
        .handle_ok(SourceFileRequest {
            target: Some(target),
            stdin: Some(source),
            ..Fixture::fixture(["-"])
        })
        .await;

    let event = expect_single_pane_state_event(
        handler
            .read_pane_state_cursor_for_test(984, subscription_id, 0)
            .await,
    );
    assert!(matches!(
        event,
        PaneStateEventDto::OptionSet {
            pane_id: event_pane_id,
            name,
            old_value: None,
            new_value,
            ..
        } if event_pane_id == pane_id && name == "@d2.source" && new_value == "stdin"
    ));
}

#[tokio::test]
async fn pane_state_revisions_are_strictly_increasing_under_concurrent_recording() {
    let handler = RequestHandler::new();
    let (_session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-concurrent-revisions").await;
    let subscription_id = subscribe(&handler, 985, target, false, true).await;
    let barrier = Arc::new(Barrier::new(101));
    let mut tasks = Vec::new();

    for index in 0_u64..100 {
        let task_handler = handler.clone();
        let task_barrier = barrier.clone();
        tasks.push(std::thread::spawn(move || {
            task_barrier.wait();
            task_handler.record_pane_state_change(
                pane_id,
                Some(1),
                PaneStateChange::OptionSet {
                    name: format!("@d2.concurrent.{index}"),
                    old: None,
                    new: index.to_string(),
                },
            );
        }));
    }

    barrier.wait();
    for task in tasks {
        task.join()
            .expect("concurrent pane-state task should finish");
    }

    let response = handler
        .handle_pane_state_cursor(
            985,
            PaneStateCursorRequest {
                subscription_id,
                after_revision: 0,
                wait: false,
                max_events: Some(128),
            },
        )
        .await;
    let events = match response {
        Response::PaneStateCursor(response) => response.events,
        response => panic!("pane-state cursor failed: {response:?}"),
    };
    assert_eq!(events.len(), 100, "expected all concurrent events");

    let revisions = events
        .iter()
        .map(|event| match event {
            PaneStateEventDto::OptionSet { revision, .. } => *revision,
            event => panic!("expected option event, got {event:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        revisions.windows(2).all(|pair| pair[0] < pair[1]),
        "revisions must be delivered in strict order: {revisions:?}"
    );
    let mut sorted = revisions;
    sorted.sort_unstable();
    assert_eq!(sorted, (1_u64..=100).collect::<Vec<_>>());
}

#[tokio::test]
async fn pane_state_subscriptions_obey_connection_limits() {
    let handler = RequestHandler::with_owner_uid_and_subscription_limits(
        0,
        SubscriptionLimits::new(1, 16, 16, Duration::from_secs(60)),
    );
    let (_session, target, _pane_id) =
        create_session_with_pane(&handler, "pane-state-subscription-limit").await;
    let _subscription_id = subscribe(&handler, 98, target.clone(), false, false).await;

    match handler
        .handle_subscribe_pane_state(
            98,
            SubscribePaneStateRequest {
                include_title: true,
                include_options: true,
                ..Fixture::fixture(target)
            },
        )
        .await
    {
        Response::Error(error) => assert!(
            error
                .error
                .to_string()
                .contains("pane state subscription limit exceeded for connection"),
            "{error:?}"
        ),
        response => panic!("second pane-state subscription should hit the limit: {response:?}"),
    }
}

#[tokio::test]
async fn kill_session_emits_closed_for_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-kill-session").await;
    let subscription_id = subscribe(&handler, 95, target, false, false).await;

    handler
        .handle_ok(KillSessionRequest::fixture(session))
        .await;

    assert_closed_event(&handler, 95, subscription_id, pane_id).await;
}

#[tokio::test]
async fn kill_window_emits_closed_for_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, _initial_target, _initial_pane_id) =
        create_session_with_pane(&handler, "pane-state-kill-window").await;
    let (target, pane_id) = create_window_with_pane(&handler, &session, 1).await;
    let subscription_id = subscribe(&handler, 96, target, false, false).await;

    handler
        .handle_ok(KillWindowRequest::fixture(WindowTarget::with_window(
            session, 1,
        )))
        .await;

    assert_closed_event(&handler, 96, subscription_id, pane_id).await;
}

#[tokio::test]
async fn respawn_window_kill_emits_closed_for_destroyed_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, split_target, split_pane_id) =
        create_session_with_split_pane(&handler, "pane-state-respawn-window-destroyed").await;
    let subscription_id = subscribe(&handler, 106, split_target, false, false).await;

    handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(session, 0),
            kill: true,
            environment: None,
            command: None,
            start_directory: None,
        })
        .await;

    assert_closed_event(&handler, 106, subscription_id, split_pane_id).await;
}

#[tokio::test]
async fn link_window_replacement_emits_closed_for_replaced_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (alpha, _alpha_target, _alpha_pane_id) =
        create_session_with_pane(&handler, "pane-state-link-source").await;
    let (beta, beta_target, beta_pane_id) =
        create_session_with_pane(&handler, "pane-state-link-dest").await;
    let subscription_id = subscribe(&handler, 101, beta_target, false, false).await;

    handler
        .handle_ok(LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha, 0),
                WindowTarget::with_window(beta, 0),
            ))
        })
        .await;

    assert_closed_event(&handler, 101, subscription_id, beta_pane_id).await;
}

#[tokio::test]
async fn move_window_replacement_emits_closed_for_replaced_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (alpha, _alpha_target, _alpha_pane_id) =
        create_session_with_pane(&handler, "pane-state-move-source").await;
    let (_source_target, _source_pane_id) = create_window_with_pane(&handler, &alpha, 1).await;
    let (beta, beta_target, beta_pane_id) =
        create_session_with_pane(&handler, "pane-state-move-dest").await;
    let subscription_id = subscribe(&handler, 102, beta_target, false, false).await;

    handler
        .handle_ok(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha, 1),
                WindowTarget::with_window(beta, 0),
            ))
        })
        .await;

    assert_closed_event(&handler, 102, subscription_id, beta_pane_id).await;
}

#[tokio::test]
async fn move_window_replacement_keeps_linked_destination_pane_state_open() {
    let handler = RequestHandler::new();
    let (alpha, _alpha_target, _alpha_pane_id) =
        create_session_with_pane(&handler, "pane-state-move-linked-source").await;
    let (_source_target, _source_pane_id) = create_window_with_pane(&handler, &alpha, 1).await;
    let (beta, beta_target, beta_pane_id) =
        create_session_with_pane(&handler, "pane-state-move-linked-dest").await;
    let (gamma, _gamma_target, _gamma_pane_id) =
        create_session_with_pane(&handler, "pane-state-move-linked-peer").await;

    handler
        .handle_ok(LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(beta.clone(), 0),
                WindowTarget::with_window(gamma, 0),
            ))
        })
        .await;

    let subscription_id = subscribe(&handler, 107, beta_target, false, false).await;
    handler
        .handle_ok(MoveWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha, 1),
                WindowTarget::with_window(beta, 0),
            ))
        })
        .await;

    match handler
        .read_pane_state_cursor_for_test(107, subscription_id, 0)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert!(
                response.events.is_empty(),
                "linked destination pane {beta_pane_id:?} must not be closed by move-window -k: {:?}",
                response.events
            );
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}

#[tokio::test]
async fn unlink_window_kill_if_last_emits_closed_for_removed_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, _initial_target, _initial_pane_id) =
        create_session_with_pane(&handler, "pane-state-unlink-last").await;
    let (target, pane_id) = create_window_with_pane(&handler, &session, 1).await;
    let subscription_id = subscribe(&handler, 103, target, false, false).await;

    handler
        .handle_ok(UnlinkWindowRequest {
            target: WindowTarget::with_window(session, 1),
            kill_if_last: true,
        })
        .await;

    assert_closed_event(&handler, 103, subscription_id, pane_id).await;
}

#[tokio::test]
async fn pane_kill_ref_emits_closed_for_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, target, pane_id) =
        create_session_with_pane(&handler, "pane-state-pane-kill").await;
    let subscription_id = subscribe(&handler, 97, target, false, false).await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(session, pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    assert_closed_event(&handler, 97, subscription_id, pane_id).await;
}

#[tokio::test]
async fn pane_exit_emits_closed_for_pane_state_subscribers() {
    let handler = RequestHandler::new();
    let (session, split_target, split_pane_id) =
        create_session_with_split_pane(&handler, "pane-state-pane-exit").await;
    let subscription_id = subscribe(&handler, 108, split_target.clone(), false, false).await;

    let generation = mark_pane_exited(&handler, &split_target).await;
    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            session.clone(),
            split_pane_id,
            Some(generation),
        ))
        .await;

    assert_eq!(window_pane_count(&handler, &session).await, 1);
    assert_closed_event_with_reason(
        &handler,
        108,
        subscription_id,
        split_pane_id,
        PaneStateClosedReason::Exited,
    )
    .await;
}

#[tokio::test]
async fn pane_exit_retries_a_failed_teardown_and_still_emits_closed() {
    let handler = RequestHandler::new();
    let (session, split_target, split_pane_id) =
        create_session_with_split_pane(&handler, "pane-state-pane-exit-retry").await;
    let subscription_id = subscribe(&handler, 109, split_target.clone(), false, false).await;
    let mut lifecycle_events = handler.subscribe_lifecycle_events();

    let generation = mark_pane_exited(&handler, &split_target).await;
    handler.state.lock().await.fail_next_resize_for_test();
    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            session.clone(),
            split_pane_id,
            Some(generation),
        ))
        .await;

    assert_closed_event_with_reason(
        &handler,
        109,
        subscription_id,
        split_pane_id,
        PaneStateClosedReason::Exited,
    )
    .await;
    assert!(
        exited_pane_ids(&mut lifecycle_events).contains(&split_pane_id.as_u32()),
        "a transient teardown failure must not swallow PaneExited"
    );
    assert_eq!(
        window_pane_count(&handler, &session).await,
        1,
        "a transient teardown failure must not leave the exited pane in the window"
    );
}

#[tokio::test]
async fn pane_exit_falls_back_to_kept_dead_when_the_teardown_keeps_failing() {
    let handler = RequestHandler::new();
    let (session, split_target, split_pane_id) =
        create_session_with_split_pane(&handler, "pane-state-pane-exit-stuck").await;
    let subscription_id = subscribe(&handler, 110, split_target.clone(), false, false).await;

    let generation = mark_pane_exited(&handler, &split_target).await;
    handler.state.lock().await.fail_resizes_for_test(usize::MAX);
    // Auto-advance the retry backoff instead of sleeping through the budget.
    tokio::time::pause();
    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            session.clone(),
            split_pane_id,
            Some(generation),
        ))
        .await;
    tokio::time::resume();

    assert_eq!(window_pane_count(&handler, &session).await, 2);
    assert_closed_event_with_reason(
        &handler,
        110,
        subscription_id,
        split_pane_id,
        PaneStateClosedReason::DiedKept,
    )
    .await;
}

async fn create_session_with_split_pane(
    handler: &RequestHandler,
    name: &str,
) -> (SessionName, PaneTarget, PaneId) {
    let session = handler.create_session(name).await;
    let split_target = handler
        .handle_ok(SplitWindowRequest::fixture(&session))
        .await
        .pane;
    let split_pane_id = pane_id_at(handler, &split_target).await;
    (session, split_target, split_pane_id)
}

async fn mark_pane_exited(handler: &RequestHandler, target: &PaneTarget) -> u64 {
    let pane_id = pane_id_at(handler, target).await;
    let mut state = handler.state.lock().await;
    let generation = state.pane_output_generation_for_target(target, pane_id);
    state
        .mark_pane_dead_without_exit_details(target)
        .expect("mark pane dead");
    generation
}

fn exited_pane_ids(
    events: &mut tokio::sync::broadcast::Receiver<crate::handler::QueuedLifecycleEvent>,
) -> Vec<u32> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.event {
            rmux_core::LifecycleEvent::PaneExited { pane_id, .. } => pane_id,
            _ => None,
        })
        .collect()
}

async fn window_pane_count(handler: &RequestHandler, session: &SessionName) -> usize {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session)
        .and_then(|session| session.window_at(0))
        .map(|window| window.pane_count())
        .expect("window exists")
}

async fn assert_closed_event(
    handler: &RequestHandler,
    connection_id: u64,
    subscription_id: rmux_proto::PaneStateSubscriptionId,
    pane_id: PaneId,
) {
    assert_closed_event_with_reason(
        handler,
        connection_id,
        subscription_id,
        pane_id,
        PaneStateClosedReason::Killed,
    )
    .await;
}

async fn assert_closed_event_with_reason(
    handler: &RequestHandler,
    connection_id: u64,
    subscription_id: rmux_proto::PaneStateSubscriptionId,
    pane_id: PaneId,
    expected_reason: PaneStateClosedReason,
) {
    match handler
        .read_pane_state_cursor_for_test(connection_id, subscription_id, 0)
        .await
    {
        Response::PaneStateCursor(response) => {
            assert_eq!(response.events.len(), 1);
            assert!(
                matches!(
                    &response.events[0],
                    PaneStateEventDto::Closed {
                        pane_id: event_pane_id,
                        reason,
                        ..
                    } if *event_pane_id == pane_id && *reason == expected_reason
                ),
                "{:?}",
                response.events
            );
        }
        response => panic!("pane-state cursor failed: {response:?}"),
    }
}
