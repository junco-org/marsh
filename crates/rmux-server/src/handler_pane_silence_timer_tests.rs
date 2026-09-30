use super::lifecycle_producer_tasks::run_registered_lifecycle_producer;
use super::RequestHandler;
use crate::pane_io::PaneExitEvent;
use rmux_core::{LifecycleEvent, PaneId, WINLINK_SILENCE};
use rmux_proto::{
    BreakPaneRequest, KillPaneRequest, LinkWindowRequest, OptionName, PaneKillRequest, PaneTarget,
    PaneTargetRef, Request, Response, ScopeSelector, SessionName, SplitWindowExtRequest,
    WindowTarget,
};

use crate::test_fixtures::{quiet_command, Fixture, Grouped, Quiet, SessionSpec, TestRequest};

async fn split_quiet_window(
    handler: &RequestHandler,
    window: &WindowTarget,
) -> (PaneTarget, PaneId) {
    let split = TestRequest::send_ok(
        handler,
        SplitWindowExtRequest {
            command: Some(quiet_command()),
            detached: true,
            ..Fixture::fixture(PaneTarget::with_window(
                window.session_name().clone(),
                window.window_index(),
                0,
            ))
        },
    )
    .await
    .pane;
    handler
        .wait_for_pane_startup_to_finish_for_test(&split)
        .await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(split.session_name())
            .and_then(|session| session.window_at(split.window_index()))
            .and_then(|window| window.pane(split.pane_index()))
            .map(|pane| pane.id())
            .expect("split pane exists")
    };
    (split, pane_id)
}

async fn setup_non_last_pane_case(
    handler: &RequestHandler,
    value: &str,
) -> (SessionName, WindowTarget, PaneTarget, PaneId) {
    let session = SessionSpec::create_started(handler, Quiet(value)).await;
    let monitored = WindowTarget::with_window(session.clone(), 0);
    let second_window = handler.create_started_window(Quiet(&session)).await;
    let (split_pane, pane_id) = split_quiet_window(handler, &second_window).await;
    handler
        .set_option(
            ScopeSelector::Window(monitored.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    (session, monitored, split_pane, pane_id)
}

fn timer_snapshot(handler: &RequestHandler, target: &WindowTarget) -> (u64, tokio::time::Instant) {
    handler
        .silence_timer_snapshot_for_test(target)
        .expect("monitored window has a silence timer")
}

fn spawn_registered_silence_expiry(
    handler: &RequestHandler,
    target: WindowTarget,
) -> tokio::task::JoinHandle<Option<()>> {
    let (session_id, window_id, generation) = handler
        .silence_timer_identity_for_test(&target)
        .expect("monitor-silence arms the timer before the expiry race");
    let registration = handler
        .reserve_lifecycle_producer_task("test-silence-expiry")
        .expect("test expiry producer is admitted");
    let handler = handler.clone();
    tokio::spawn(async move {
        run_registered_lifecycle_producer(registration, async move {
            handler
                .expire_silence_timer_for_test(target, session_id, window_id, generation)
                .await;
        })
        .await
    })
}

#[tokio::test]
async fn normal_shutdown_waits_for_silence_timer_handoff_then_cancels_it() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create_started(&handler, Quiet("silence-shutdown-handoff")).await;
    let target = WindowTarget::with_window(session.clone(), 0);
    let pause = handler.install_pre_admitted_producer_spawn_pause("rmux-silence-timer");
    let (runtime_release_tx, runtime_release_rx) = tokio::sync::oneshot::channel();
    let installer_handler = handler.clone();
    let installer_target = target.clone();
    let installer = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("silence timer installer runtime")
            .block_on(async move {
                installer_handler
                    .set_option(
                        ScopeSelector::Window(installer_target),
                        OptionName::MonitorSilence,
                        "60",
                    )
                    .await;
                let _ = runtime_release_rx.await;
            });
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("timer installer reaches the pre-spawn handoff");
    let close_handler = handler.clone();
    let close = tokio::spawn(async move {
        close_handler
            .close_normal_and_drain_lifecycle_producers()
            .await;
    });
    handler
        .wait_until_normal_lifecycle_producers_closing_for_test()
        .await;
    assert!(
        !close.is_finished(),
        "shutdown must wait until the timer state is installed or rolled back"
    );

    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), close)
        .await
        .expect("normal producer drain is bounded")
        .expect("normal producer drain task joins");
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&target),
        None,
        "cancellation cleanup removes the exact installed timer"
    );
    let state = handler.state.lock().await;
    assert!(
        !state
            .sessions
            .session(&session)
            .expect("session remains present")
            .winlink_alert_flags(0)
            .contains(WINLINK_SILENCE),
        "a cancelled timer cannot publish a late silence alert"
    );
    drop(state);

    runtime_release_tx
        .send(())
        .expect("timer installer runtime remains alive");
    installer.join().expect("timer installer thread joins");
}

#[tokio::test]
async fn expired_silence_publication_outlives_its_mutation_guard() {
    let handler = RequestHandler::new();
    let session = SessionSpec::create_started(&handler, Quiet("silence-publication-handoff")).await;
    let target = WindowTarget::with_window(session.clone(), 0);
    handler
        .set_option(
            ScopeSelector::Window(target.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Session(session.clone()),
            OptionName::SilenceAction,
            "none",
        )
        .await;
    let pause = handler.install_alert_plan_effect_pause();

    let expiry = spawn_registered_silence_expiry(&handler, target.clone());
    if tokio::time::timeout(std::time::Duration::from_secs(2), pause.reached.notified())
        .await
        .is_err()
    {
        let snapshot = handler.silence_timer_snapshot_for_test(&target);
        let flags = handler
            .state
            .lock()
            .await
            .sessions
            .session(&session)
            .expect("session remains present")
            .winlink_alert_flags(0);
        panic!("expired timer did not reach alert effects: timer={snapshot:?}, flags={flags:?}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), expiry)
        .await
        .expect("expiry producer finishes after publication handoff")
        .expect("expiry producer task joins");

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handler.close_normal_and_drain_lifecycle_producers(),
    )
    .await
    .expect("timer releases its mutation guard after handing off publication");
    let publication_handler = handler.clone();
    let publication_drain = tokio::spawn(async move {
        publication_handler
            .close_and_wait_for_lifecycle_publications()
            .await;
    });
    tokio::task::yield_now().await;
    assert!(
        !publication_drain.is_finished(),
        "the alert batch remains owned after its timer mutation finishes"
    );

    pause.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), publication_drain)
        .await
        .expect("publication drain finishes after alert effects")
        .expect("publication drain task joins");
    assert_eq!(handler.silence_timer_snapshot_for_test(&target), None);
    let state = handler.state.lock().await;
    assert!(
        state
            .sessions
            .session(&session)
            .expect("session remains present")
            .winlink_alert_flags(0)
            .contains(WINLINK_SILENCE),
        "the admitted expiry commits its silence flag before handoff"
    );
}

#[tokio::test]
async fn full_lifecycle_outbox_cannot_hold_a_silence_mutation_open() {
    let handler = RequestHandler::with_lifecycle_dispatch_capacity_for_test(1);
    let session = SessionSpec::create_started(&handler, Quiet("silence-full-outbox")).await;
    let target = WindowTarget::with_window(session.clone(), 0);
    handler
        .set_option(
            ScopeSelector::Window(target.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Session(session.clone()),
            OptionName::SilenceAction,
            "any",
        )
        .await;
    let lifecycle_receiver = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test activates the lifecycle outbox receiver");
    let filler = handler
        .prepare_lifecycle_event_for_test(LifecycleEvent::ClientFocusIn {
            session_name: session.clone(),
            client_name: Some("fill-silence-outbox".to_owned()),
        })
        .await;
    handler.emit_prepared_lifecycle_event_for_test(filler).await;

    let mut observed = handler.subscribe_lifecycle_events();
    let expiry = spawn_registered_silence_expiry(&handler, target.clone());
    tokio::time::timeout(std::time::Duration::from_secs(2), expiry)
        .await
        .expect("expiry mutation hands publication to a separate task")
        .expect("expiry producer task joins");
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), observed.recv())
        .await
        .expect("silence event reaches the saturated outbox")
        .expect("lifecycle broadcast remains active");
    assert!(matches!(
        event.event,
        LifecycleEvent::AlertSilence { target: event_target } if event_target == target
    ));

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handler.close_normal_and_drain_lifecycle_producers(),
    )
    .await
    .expect("full outbox cannot retain the timer mutation guard");
    let publication_handler = handler.clone();
    let publication_drain = tokio::spawn(async move {
        publication_handler
            .close_and_wait_for_lifecycle_publications()
            .await;
    });
    tokio::task::yield_now().await;
    assert!(
        !publication_drain.is_finished(),
        "the saturated outbox still owns the handed-off publication"
    );

    handler.deactivate_lifecycle_dispatch_for_shutdown();
    drop(lifecycle_receiver);
    tokio::time::timeout(std::time::Duration::from_secs(2), publication_drain)
        .await
        .expect("receiver drop releases the handed-off silence publication")
        .expect("publication drain task joins");
}

#[tokio::test]
async fn direct_non_last_pane_kill_preserves_other_window_silence_deadline() {
    let handler = RequestHandler::new();
    let (_session, monitored, split_pane, _pane_id) =
        setup_non_last_pane_case(&handler, "silence-direct-pane-kill").await;
    let before = timer_snapshot(&handler, &monitored);

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: split_pane,
            kill_all_except: false,
        },
    )
    .await;

    assert_eq!(timer_snapshot(&handler, &monitored), before);
}

#[tokio::test]
async fn pane_id_non_last_kill_preserves_other_window_silence_deadline() {
    let handler = RequestHandler::new();
    let (session, monitored, _split_pane, pane_id) =
        setup_non_last_pane_case(&handler, "silence-pane-id-kill").await;
    let before = timer_snapshot(&handler, &monitored);

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(session, pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    assert_eq!(timer_snapshot(&handler, &monitored), before);
}

#[tokio::test]
async fn natural_non_last_pane_exit_preserves_other_window_silence_deadline() {
    let handler = RequestHandler::new();
    let (session, monitored, split_pane, pane_id) =
        setup_non_last_pane_case(&handler, "silence-natural-pane-exit").await;
    let before = timer_snapshot(&handler, &monitored);
    {
        let mut state = handler.state.lock().await;
        state
            .mark_pane_dead_without_exit_details(&split_pane)
            .expect("mark pane naturally exited");
    }

    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(session, pane_id, None))
        .await;

    assert_eq!(timer_snapshot(&handler, &monitored), before);
}

#[tokio::test]
async fn last_pane_window_kill_removes_grouped_alias_timers_and_preserves_survivors() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create_started(&handler, Quiet("silence-last-pane-owner")).await;
    let removed_owner = handler.create_started_window(Quiet(&owner)).await;
    let peer = SessionSpec::create(&handler, Grouped("silence-last-pane-peer", &owner)).await;
    let survivor_owner = WindowTarget::with_window(owner.clone(), 0);
    let survivor_peer = WindowTarget::with_window(peer.clone(), 0);
    let removed_peer = WindowTarget::with_window(peer.clone(), removed_owner.window_index());
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;
    let survivor_owner_before = timer_snapshot(&handler, &survivor_owner);
    let survivor_peer_before = timer_snapshot(&handler, &survivor_peer);
    assert!(handler
        .silence_timer_snapshot_for_test(&removed_owner)
        .is_some());
    assert!(handler
        .silence_timer_snapshot_for_test(&removed_peer)
        .is_some());

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(owner.clone(), removed_owner.window_index(), 0),
            kill_all_except: false,
        },
    )
    .await;

    assert_eq!(
        handler.silence_timer_snapshot_for_test(&removed_owner),
        None
    );
    assert_eq!(handler.silence_timer_snapshot_for_test(&removed_peer), None);
    assert_eq!(
        timer_snapshot(&handler, &survivor_owner),
        survivor_owner_before
    );
    assert_eq!(
        timer_snapshot(&handler, &survivor_peer),
        survivor_peer_before
    );
}

#[tokio::test]
async fn break_last_pane_across_sessions_preserves_silence_deadline_and_identity() {
    let handler = RequestHandler::new();
    let source_session = SessionSpec::create_started(&handler, Quiet("silence-break-source")).await;
    let destination_session =
        SessionSpec::create_started(&handler, Quiet("silence-break-destination")).await;
    let source = WindowTarget::with_window(source_session.clone(), 0);
    let unrelated = WindowTarget::with_window(destination_session.clone(), 0);
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(121);
    handler.replace_silence_timer_deadline_for_test(&source, deadline);
    let source_before = timer_snapshot(&handler, &source);
    let source_identity_before = handler
        .silence_timer_identity_for_test(&source)
        .expect("source timer has stable identity");
    let unrelated_before = timer_snapshot(&handler, &unrelated);
    let destination_session_id = handler.session_id_for_test(&destination_session).await;

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 0, 0),
            WindowTarget::with_window(destination_session.clone(), 1),
        )),
    )
    .await;
    let destination = WindowTarget::with_window(destination_session, 1);
    assert_eq!(
        response.target,
        PaneTarget::with_window(destination.session_name().clone(), 1, 0)
    );

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let destination_after = timer_snapshot(&handler, &destination);
    assert_eq!(destination_after.1, source_before.1);
    assert!(destination_after.0 > source_before.0);
    let destination_identity = handler
        .silence_timer_identity_for_test(&destination)
        .expect("destination timer keeps the moved identity");
    assert_eq!(destination_identity.0, destination_session_id);
    assert_eq!(destination_identity.1, source_identity_before.1);
    assert!(destination_identity.2 > source_identity_before.2);
    assert_eq!(timer_snapshot(&handler, &unrelated), unrelated_before);
}

async fn assert_expired_single_pane_break_does_not_rearm(
    label: &str,
    attach_source: bool,
    expect_silence_flag: bool,
) {
    let handler = RequestHandler::new();
    let source_session =
        SessionSpec::create_started(&handler, Quiet(&format!("{label}-source"))).await;
    let source = WindowTarget::with_window(source_session.clone(), 0);
    let owner = SessionSpec::create_started(&handler, Quiet(&format!("{label}-owner"))).await;
    let peer = SessionSpec::create(&handler, Grouped(&format!("{label}-peer"), &owner)).await;
    handler.wait_for_initial_panes_for_test().await;
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;
    let _attached_control = if attach_source {
        Some(handler.attach_client(918, &source_session).await)
    } else {
        None
    };

    let expired_identity = handler
        .silence_timer_identity_for_test(&source)
        .expect("source timer is armed before expiry");
    handler
        .expire_silence_timer_for_test(
            source.clone(),
            expired_identity.0,
            expired_identity.1,
            expired_identity.2,
        )
        .await;
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    {
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .sessions
                .session(&source_session)
                .expect("source session exists before break")
                .winlink_alert_flags(0)
                .contains(WINLINK_SILENCE),
            expect_silence_flag,
            "source winlink silence persistence must match its attached/current state"
        );
    }

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest::fixture((
            PaneTarget::with_window(source_session.clone(), 0, 0),
            WindowTarget::with_window(owner.clone(), 1),
        )),
    )
    .await;
    assert_eq!(
        response.target,
        PaneTarget::with_window(owner.clone(), 1, 0)
    );

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    let destinations = [
        WindowTarget::with_window(owner, 1),
        WindowTarget::with_window(peer, 1),
    ];
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&source_session).is_none());
    for destination in &destinations {
        let session = state
            .sessions
            .session(destination.session_name())
            .expect("destination group member survives");
        assert_eq!(
            session
                .window_at(destination.window_index())
                .expect("moved window exists in every destination group member")
                .id(),
            expired_identity.1
        );
        assert_eq!(
            session
                .winlink_alert_flags(destination.window_index())
                .contains(WINLINK_SILENCE),
            expect_silence_flag,
            "moved alias {destination} must preserve the source winlink flag state"
        );
        assert_eq!(
            handler.silence_timer_snapshot_for_test(destination),
            None,
            "structural break must not rearm already-fired alias {destination}"
        );
    }
}

#[tokio::test]
async fn expired_single_pane_break_moves_silence_without_rearming() {
    assert_expired_single_pane_break_does_not_rearm("silence-break-expired", false, true).await;
}

#[tokio::test]
async fn attached_current_expired_single_pane_break_does_not_rearm_without_flag() {
    assert_expired_single_pane_break_does_not_rearm("silence-break-attached-expired", true, false)
        .await;
}

#[tokio::test]
async fn grouped_break_reorders_distinct_duplicate_alias_silence_deadlines() {
    let handler = RequestHandler::new();
    let owner = SessionSpec::create_started(&handler, Quiet("silence-break-duplicate-owner")).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
        )),
    )
    .await;
    let peer = SessionSpec::create(&handler, Grouped("silence-break-duplicate-peer", &owner)).await;
    handler.wait_for_initial_panes_for_test().await;
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;

    let targets = [
        WindowTarget::with_window(owner.clone(), 0),
        WindowTarget::with_window(owner.clone(), 1),
        WindowTarget::with_window(peer.clone(), 0),
        WindowTarget::with_window(peer.clone(), 1),
    ];
    let now = tokio::time::Instant::now();
    for (offset, target) in targets.iter().enumerate() {
        handler.replace_silence_timer_deadline_for_test(
            target,
            now + tokio::time::Duration::from_secs(121 + offset as u64),
        );
    }
    let before = targets
        .clone()
        .map(|target| timer_snapshot(&handler, &target));

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            before: true,
            ..Fixture::fixture((
                PaneTarget::with_window(owner.clone(), 1, 0),
                WindowTarget::with_window(owner.clone(), 0),
            ))
        },
    )
    .await;
    assert_eq!(response.target, PaneTarget::with_window(owner, 0, 0));

    for (target, expected) in [
        (&targets[0], before[1]),
        (&targets[1], before[0]),
        (&targets[2], before[3]),
        (&targets[3], before[2]),
    ] {
        let after = timer_snapshot(&handler, target);
        assert_eq!(after.1, expected.1, "deadline follows {target}");
        assert!(after.0 > expected.0, "generation advances for {target}");
    }
}

#[tokio::test]
async fn cross_session_multi_pane_break_preserves_shifted_duplicate_alias_deadlines() {
    let handler = RequestHandler::new();
    let source_session =
        SessionSpec::create_started(&handler, Quiet("silence-break-multi-source")).await;
    let source = WindowTarget::with_window(source_session.clone(), 0);
    let (moved_pane, _) = split_quiet_window(&handler, &source).await;
    let owner = SessionSpec::create_started(&handler, Quiet("silence-break-multi-owner")).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
        )),
    )
    .await;
    let peer = SessionSpec::create(&handler, Grouped("silence-break-multi-peer", &owner)).await;
    handler.wait_for_initial_panes_for_test().await;
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;

    let old_targets = [
        source.clone(),
        WindowTarget::with_window(owner.clone(), 0),
        WindowTarget::with_window(owner.clone(), 1),
        WindowTarget::with_window(peer.clone(), 0),
        WindowTarget::with_window(peer.clone(), 1),
    ];
    let base_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(121);
    for (offset, target) in old_targets.iter().enumerate() {
        handler.replace_silence_timer_deadline_for_test(
            target,
            base_deadline + tokio::time::Duration::from_secs(offset as u64 * 7),
        );
    }
    let before = old_targets
        .clone()
        .map(|target| timer_snapshot(&handler, &target));

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            before: true,
            ..Fixture::fixture((moved_pane, WindowTarget::with_window(owner.clone(), 0)))
        },
    )
    .await;
    assert_eq!(
        response.target,
        PaneTarget::with_window(owner.clone(), 0, 0)
    );

    assert_eq!(timer_snapshot(&handler, &source), before[0]);
    for (target, expected) in [
        (WindowTarget::with_window(owner.clone(), 1), before[1]),
        (WindowTarget::with_window(owner.clone(), 2), before[2]),
        (WindowTarget::with_window(peer.clone(), 1), before[3]),
        (WindowTarget::with_window(peer.clone(), 2), before[4]),
    ] {
        let after = timer_snapshot(&handler, &target);
        assert_eq!(after.1, expected.1, "deadline follows {target}");
        assert!(after.0 > expected.0, "generation advances for {target}");
    }

    let inserted_owner = WindowTarget::with_window(owner, 0);
    let inserted_peer = WindowTarget::with_window(peer, 0);
    let old_deadlines = before.map(|snapshot| snapshot.1);
    for target in [inserted_owner, inserted_peer] {
        let inserted = timer_snapshot(&handler, &target);
        assert!(
            !old_deadlines.contains(&inserted.1),
            "newly-created {target} must arm a fresh silence deadline"
        );
    }
}

#[tokio::test]
async fn cross_session_single_pane_break_preserves_moved_and_shifted_alias_deadlines() {
    let handler = RequestHandler::new();
    let source_session =
        SessionSpec::create_started(&handler, Quiet("silence-break-linked-source")).await;
    let source = WindowTarget::with_window(source_session.clone(), 0);
    let owner = SessionSpec::create_started(&handler, Quiet("silence-break-linked-owner")).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source, WindowTarget::with_window(owner.clone(), 1))),
    )
    .await;
    let peer = SessionSpec::create(&handler, Grouped("silence-break-linked-peer", &owner)).await;
    handler.wait_for_initial_panes_for_test().await;
    handler
        .set_option(ScopeSelector::Global, OptionName::MonitorSilence, "60")
        .await;

    let old_targets = [
        source.clone(),
        WindowTarget::with_window(owner.clone(), 0),
        WindowTarget::with_window(owner.clone(), 1),
        WindowTarget::with_window(peer.clone(), 0),
        WindowTarget::with_window(peer.clone(), 1),
    ];
    let base_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(121);
    for (offset, target) in old_targets.iter().enumerate() {
        handler.replace_silence_timer_deadline_for_test(
            target,
            base_deadline + tokio::time::Duration::from_secs(offset as u64 * 7),
        );
    }
    let before = old_targets
        .clone()
        .map(|target| timer_snapshot(&handler, &target));

    let response = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            before: true,
            ..Fixture::fixture((
                PaneTarget::with_window(source_session, 0, 0),
                WindowTarget::with_window(owner.clone(), 1),
            ))
        },
    )
    .await;
    assert_eq!(
        response.target,
        PaneTarget::with_window(owner.clone(), 1, 0)
    );
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);

    for (target, expected) in [
        (WindowTarget::with_window(owner.clone(), 1), before[0]),
        (WindowTarget::with_window(owner.clone(), 2), before[2]),
        (WindowTarget::with_window(peer.clone(), 2), before[4]),
    ] {
        let after = timer_snapshot(&handler, &target);
        assert_eq!(after.1, expected.1, "deadline follows {target}");
        assert!(after.0 > expected.0, "generation advances for {target}");
    }
    assert_eq!(
        timer_snapshot(&handler, &WindowTarget::with_window(owner.clone(), 0)),
        before[1]
    );
    assert_eq!(
        timer_snapshot(&handler, &WindowTarget::with_window(peer.clone(), 0)),
        before[3]
    );

    let inserted_peer = timer_snapshot(&handler, &WindowTarget::with_window(peer, 1));
    assert_eq!(
        inserted_peer.1, before[0].1,
        "the new non-syntactic group alias inherits the addressed source deadline"
    );
    assert!(inserted_peer.0 > before[0].0);
}
