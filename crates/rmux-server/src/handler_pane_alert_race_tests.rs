use std::collections::HashSet;

use super::{QueuedLifecycleEvent, RequestHandler};
use crate::pane_io::{AttachControl, PaneAlertEvent};
use rmux_core::{LifecycleEvent, PaneId, WINDOW_BELL, WINLINK_ACTIVITY, WINLINK_BELL};
use rmux_proto::{
    HookName, KillSessionRequest, LinkWindowRequest, OptionName, OptionScopeSelector,
    ScopeSelector, SessionName, SplitWindowExtRequest, WaitForMode, WaitForRequest, WindowId,
    WindowTarget,
};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{timeout, Duration, Instant};

use crate::test_fixtures::{quiet_command, Fixture, Grouped, Quiet};
use crate::test_names::session_name;

async fn pane_identity(
    handler: &RequestHandler,
    target: &WindowTarget,
) -> (PaneId, Option<u64>, WindowId) {
    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .expect("pane-alert target exists");
    let pane_id = window.active_pane().expect("active pane exists").id();
    (
        pane_id,
        Some(state.pane_output_generation(target.session_name(), pane_id)),
        window.id(),
    )
}

fn pane_event(
    session_name: SessionName,
    pane_id: PaneId,
    generation: Option<u64>,
) -> PaneAlertEvent {
    PaneAlertEvent {
        bell_count: 1,
        title_changed: true,
        clipboard_set: true,
        queue_activity_alert: true,
        generation,
        ..Fixture::fixture((session_name, pane_id))
    }
}

async fn dispatch_expected_hooks(
    handler: &RequestHandler,
    receiver: &mut broadcast::Receiver<QueuedLifecycleEvent>,
    expected: &[HookName],
) {
    let mut remaining = expected.to_vec();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !remaining.is_empty() {
        let event = timeout(
            deadline.saturating_duration_since(Instant::now()),
            receiver.recv(),
        )
        .await
        .expect("expected lifecycle hook before timeout")
        .expect("lifecycle channel remains open");
        if let Some(position) = remaining.iter().position(|hook| *hook == event.hook_name) {
            remaining.remove(position);
            handler.dispatch_lifecycle_hook(event).await;
        }
    }
}

fn buffer_text(state: &crate::pane_terminals::HandlerState, name: &str) -> Option<String> {
    state
        .buffers
        .show(Some(name))
        .ok()
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
}

fn collect_alert_hook_targets(
    receiver: &mut broadcast::Receiver<QueuedLifecycleEvent>,
) -> (HashSet<WindowTarget>, HashSet<WindowTarget>) {
    let mut activity_targets = HashSet::new();
    let mut bell_targets = HashSet::new();
    loop {
        match receiver.try_recv() {
            Ok(event) => match event.event {
                LifecycleEvent::AlertActivity { target } => {
                    activity_targets.insert(target);
                }
                LifecycleEvent::AlertBell { target } => {
                    bell_targets.insert(target);
                }
                _ => {}
            },
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("alert lifecycle receiver lagged by {skipped} events");
            }
        }
    }
    (activity_targets, bell_targets)
}

async fn drain_controls(receiver: &mut mpsc::UnboundedReceiver<AttachControl>) {
    loop {
        match timeout(Duration::from_millis(20), receiver.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return,
        }
    }
}

/// Asserts that no *alert effect* reached this client, tolerating an ordinary render.
///
/// "The control channel stayed empty" stopped being the same claim as "no alert reached this
/// session". A live pane draws, and that draw reaches an attached client on this very channel as
/// a `Switch` carrying the target the client is *already* showing — which is what
/// [`AttachControl::is_coalescible_render_switch`] identifies. An alert effect is none of those:
/// it is a bell write, an overlay, a refresh, or a switch to somewhere else, and every one of
/// them still fails here.
async fn assert_no_alert_effects(
    receiver: &mut mpsc::UnboundedReceiver<AttachControl>,
    what: &str,
) {
    while let Ok(Some(control)) = timeout(Duration::from_millis(150), receiver.recv()).await {
        assert!(
            control.is_coalescible_render_switch(),
            "{what}, got {control:?}"
        );
    }
}

#[tokio::test]
async fn queued_pane_hook_does_not_block_an_unrelated_pane_alert() {
    let handler = RequestHandler::new();
    let hooked_session = handler
        .create_started_session(Quiet("pane-alert-queued-hook"))
        .await;
    let live_session = handler
        .create_started_session(Quiet("pane-alert-live-after-hook"))
        .await;
    let hooked_target = WindowTarget::with_window(hooked_session.clone(), 0);
    let live_target = WindowTarget::with_window(live_session.clone(), 0);
    handler
        .set_option(
            ScopeSelector::Window(live_target.clone()),
            OptionName::MonitorBell,
            "on",
        )
        .await;
    handler
        .set_global_hook(
            HookName::PaneTitleChanged,
            "set-buffer -b queued-title-hook fired",
        )
        .await;
    let (hooked_pane_id, hooked_generation, _) = pane_identity(&handler, &hooked_target).await;
    let (live_pane_id, live_generation, _) = pane_identity(&handler, &live_target).await;
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let _undrained_lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns the lifecycle dispatch receiver");

    handler.pane_alert_callback()(PaneAlertEvent {
        title_changed: true,
        generation: hooked_generation,
        ..Fixture::fixture((hooked_session, hooked_pane_id))
    });
    let event = timeout(Duration::from_secs(2), lifecycle.recv())
        .await
        .expect("pane hook is queued before the deadline")
        .expect("lifecycle channel remains open");
    assert_eq!(event.hook_name, HookName::PaneTitleChanged);

    // Leave the hook dispatch receiver undrained. Normal pane-alert delivery
    // queues lifecycle work but does not wait for the hook command to run, so
    // a slow hook cannot retain the pane-alert serialization lock.
    handler.pane_alert_callback()(PaneAlertEvent {
        bell_count: 1,
        generation: live_generation,
        ..Fixture::fixture((&live_session, live_pane_id))
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let bell_applied = {
            let state = handler.state.lock().await;
            state
                .sessions
                .session(&live_session)
                .is_some_and(|session| {
                    session
                        .winlink_alert_flags(live_target.window_index())
                        .contains(WINLINK_BELL)
                })
        };
        if bell_applied {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unrelated pane bell remains blocked behind an unexecuted hook"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn exiting_pane_hook_wait_does_not_block_unrelated_pane_alert_flush() {
    let handler = RequestHandler::new();
    let exiting_session = handler
        .create_started_session(Quiet("pane-alert-exit-hook"))
        .await;
    let live_session = handler
        .create_started_session(Quiet("pane-alert-live-peer"))
        .await;
    let exiting_target = WindowTarget::with_window(exiting_session.clone(), 0);
    let live_target = WindowTarget::with_window(live_session.clone(), 0);
    handler
        .set_option(
            ScopeSelector::Window(live_target.clone()),
            OptionName::MonitorBell,
            "on",
        )
        .await;
    handler
        .set_global_hook(
            HookName::PaneTitleChanged,
            "set-buffer -b exit-title-hook fired",
        )
        .await;
    let (exiting_pane_id, exiting_generation, _) = pane_identity(&handler, &exiting_target).await;
    let (live_pane_id, live_generation, _) = pane_identity(&handler, &live_target).await;
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns the lifecycle dispatch receiver");

    handler.pane_alert_callback()(PaneAlertEvent {
        title_changed: true,
        generation: exiting_generation,
        ..Fixture::fixture((exiting_session, exiting_pane_id))
    });
    let exit_handler = handler.clone();
    let mut exit_flush = tokio::spawn(async move {
        exit_handler
            .flush_pending_pane_alert_for_exit(exiting_pane_id, exiting_generation)
            .await;
    });

    let event = timeout(Duration::from_secs(2), lifecycle.recv())
        .await
        .expect("exiting pane hook is queued before the deadline")
        .expect("lifecycle channel remains open");
    assert_eq!(event.hook_name, HookName::PaneTitleChanged);
    assert!(
        !exit_flush.is_finished(),
        "exit flush waits for its ordered lifecycle hook"
    );

    handler.pane_alert_callback()(PaneAlertEvent {
        bell_count: 1,
        generation: live_generation,
        ..Fixture::fixture((&live_session, live_pane_id))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let bell_applied = {
            let state = handler.state.lock().await;
            state
                .sessions
                .session(&live_session)
                .is_some_and(|session| {
                    session
                        .winlink_alert_flags(live_target.window_index())
                        .contains(WINLINK_BELL)
                })
        };
        if bell_applied {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unrelated pane bell remains globally blocked by the exiting hook"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !exit_flush.is_finished(),
        "unrelated alert applies while the exiting pane still waits for its hook"
    );

    let (hook_shutdown, hook_shutdown_rx) = tokio::sync::oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_events, hook_shutdown_rx)
            .await;
    });
    timeout(Duration::from_secs(2), &mut exit_flush)
        .await
        .expect("exit flush completes after its hook is dispatched")
        .expect("exit flush task joins");
    {
        let state = handler.state.lock().await;
        assert_eq!(
            buffer_text(&state, "exit-title-hook").as_deref(),
            Some("fired"),
            "ordered hook completes before the exit flush returns"
        );
    }
    let _ = hook_shutdown.send(());
    timeout(Duration::from_secs(2), hook_task)
        .await
        .expect("lifecycle hook consumer stops")
        .expect("lifecycle hook consumer joins");
}

#[tokio::test]
async fn pane_alert_reserves_each_lifecycle_position_immediately_before_emission() {
    const WAIT_CHANNEL: &str = "pane-alert-per-event-sequencing";

    let handler = RequestHandler::new();
    let session = handler
        .create_started_session(Quiet("pane-alert-per-event-sequencing"))
        .await;
    handler
        .set_option_by_name(OptionScopeSelector::ServerGlobal, "set-clipboard", "on")
        .await;
    handler
        .set_global_hook(
            HookName::PaneTitleChanged,
            &format!("wait-for {WAIT_CHANNEL}"),
        )
        .await;
    handler
        .set_global_hook(
            HookName::PaneSetClipboard,
            "set-buffer -b second-pane-hook finished",
        )
        .await;
    let target = WindowTarget::with_window(session.clone(), 0);
    let (pane_id, generation, _) = pane_identity(&handler, &target).await;
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns the lifecycle dispatch receiver");
    let (hook_shutdown, hook_shutdown_rx) = tokio::sync::oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_events, hook_shutdown_rx)
            .await;
    });

    handler.pane_alert_callback()(PaneAlertEvent {
        title_changed: true,
        clipboard_set: true,
        generation,
        ..Fixture::fixture((session, pane_id))
    });
    let flush_handler = handler.clone();
    let mut flush = tokio::spawn(async move {
        flush_handler
            .flush_pending_pane_alert_for_exit(pane_id, generation)
            .await;
    });

    timeout(Duration::from_secs(2), async {
        loop {
            let waiting = handler
                .wait_for
                .lock()
                .expect("wait-for store remains available")
                .waiter_counts(WAIT_CHANNEL)
                .0;
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first pane hook reaches its deterministic wait seam");

    timeout(
        Duration::from_millis(250),
        handler.store_buffer(None, b"nested-between-pane-events".to_vec()),
    )
    .await
    .expect("the next pane event must not reserve ahead of an intervening publication")
    .expect("intervening buffer publication succeeds");

    handler
        .handle_ok(WaitForRequest::fixture((WAIT_CHANNEL, WaitForMode::Signal)))
        .await;
    timeout(Duration::from_secs(2), &mut flush)
        .await
        .expect("pane alert flush completes without a lifecycle ticket cycle")
        .expect("pane alert flush task joins");

    let mut observed = Vec::new();
    while observed.len() < 3 {
        let event = timeout(Duration::from_secs(2), lifecycle.recv())
            .await
            .expect("ordered lifecycle event arrives")
            .expect("lifecycle channel remains open");
        if matches!(
            event.hook_name,
            HookName::PaneTitleChanged | HookName::PasteBufferChanged | HookName::PaneSetClipboard
        ) {
            observed.push(event.hook_name);
        }
    }
    assert_eq!(
        observed,
        vec![
            HookName::PaneTitleChanged,
            HookName::PasteBufferChanged,
            HookName::PaneSetClipboard,
        ]
    );
    {
        let state = handler.state.lock().await;
        assert_eq!(
            buffer_text(&state, "second-pane-hook").as_deref(),
            Some("finished")
        );
    }
    let _ = hook_shutdown.send(());
    timeout(Duration::from_secs(2), hook_task)
        .await
        .expect("lifecycle hook consumer stops")
        .expect("lifecycle hook consumer joins");
}

#[tokio::test]
async fn exiting_pane_activity_cannot_rearm_silence_after_newer_same_window_activity() {
    let handler = RequestHandler::new();
    let session = handler
        .create_started_session(Quiet("pane-alert-exit-silence-order"))
        .await;
    let window_target = WindowTarget::with_window(session.clone(), 0);
    let split = handler
        .handle_ok(SplitWindowExtRequest {
            command: Some(quiet_command()),
            detached: true,
            ..Fixture::fixture(&session)
        })
        .await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&split.pane)
        .await;
    handler
        .set_option(
            ScopeSelector::Window(window_target.clone()),
            OptionName::MonitorSilence,
            "60",
        )
        .await;
    handler
        .set_global_hook(
            HookName::PaneTitleChanged,
            "set-buffer -b exit-silence-hook fired",
        )
        .await;

    let (exiting_pane_id, exiting_generation, live_pane_id, live_generation) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&session)
            .and_then(|current| current.window_at(window_target.window_index()))
            .expect("two-pane alert window exists");
        let exiting_pane_id = window.pane(0).expect("exiting pane exists").id();
        let live_pane_id = window.pane(1).expect("live pane exists").id();
        (
            exiting_pane_id,
            Some(state.pane_output_generation(&session, exiting_pane_id)),
            live_pane_id,
            Some(state.pane_output_generation(&session, live_pane_id)),
        )
    };
    let initial_timer = handler
        .silence_timer_snapshot_for_test(&window_target)
        .expect("monitor-silence timer starts armed");
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns the lifecycle dispatch receiver");

    handler.pane_alert_callback()(PaneAlertEvent {
        title_changed: true,
        queue_activity_alert: true,
        generation: exiting_generation,
        ..Fixture::fixture((&session, exiting_pane_id))
    });
    let exit_handler = handler.clone();
    let mut exit_flush = tokio::spawn(async move {
        exit_handler
            .flush_pending_pane_alert_for_exit(exiting_pane_id, exiting_generation)
            .await;
    });

    let event = timeout(Duration::from_secs(2), lifecycle.recv())
        .await
        .expect("exiting pane title hook is queued")
        .expect("lifecycle channel remains open");
    assert_eq!(event.hook_name, HookName::PaneTitleChanged);
    assert!(!exit_flush.is_finished(), "exit flush waits for its hook");
    let exit_timer = handler
        .silence_timer_snapshot_for_test(&window_target)
        .expect("exit activity keeps the silence timer armed");
    assert_eq!(
        exit_timer.0,
        initial_timer.0.saturating_add(1),
        "the exiting activity resets silence exactly once before its hook wait"
    );

    handler.pane_alert_callback()(PaneAlertEvent {
        queue_activity_alert: true,
        generation: live_generation,
        ..Fixture::fixture((session, live_pane_id))
    });
    let newer_timer = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = handler
                .silence_timer_snapshot_for_test(&window_target)
                .expect("same-window activity keeps the timer armed");
            if snapshot.0 > exit_timer.0 {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("newer same-window activity resets silence while the hook waits");
    assert!(
        newer_timer.1 > exit_timer.1,
        "the newer activity owns a strictly later silence deadline"
    );
    assert!(
        !exit_flush.is_finished(),
        "newer activity applies before the exiting hook completes"
    );

    let (hook_shutdown, hook_shutdown_rx) = tokio::sync::oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_events, hook_shutdown_rx)
            .await;
    });
    timeout(Duration::from_secs(2), &mut exit_flush)
        .await
        .expect("exit flush completes after its hook")
        .expect("exit flush task joins");
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&window_target),
        Some(newer_timer),
        "the older exiting batch must not rearm over newer same-window activity"
    );
    {
        let state = handler.state.lock().await;
        assert_eq!(
            buffer_text(&state, "exit-silence-hook").as_deref(),
            Some("fired"),
            "the ordered title hook still executes"
        );
    }
    let _ = hook_shutdown.send(());
    timeout(Duration::from_secs(2), hook_task)
        .await
        .expect("lifecycle hook consumer stops")
        .expect("lifecycle hook consumer joins");
}

#[tokio::test]
async fn pane_alert_reaches_every_linked_and_grouped_window_alias_once() {
    let handler = RequestHandler::new();
    let owner = handler
        .create_started_session(Quiet("m-pane-alert-family-owner"))
        .await;
    let peer = handler
        .create_session(Grouped("z-pane-alert-family-peer", &owner))
        .await;
    let external = handler
        .create_started_session(Quiet("a-pane-alert-family-external"))
        .await;
    let owner_target = WindowTarget::with_window(owner.clone(), 0);
    let peer_target = WindowTarget::with_window(peer.clone(), 0);
    let external_target = WindowTarget::with_window(external.clone(), 1);
    handler
        .handle_ok(LinkWindowRequest::fixture((
            &owner_target,
            &external_target,
        )))
        .await;
    let family_targets = vec![owner_target.clone(), peer_target, external_target];

    for target in &family_targets {
        for (option, value) in [
            (OptionName::MonitorActivity, "on"),
            (OptionName::MonitorBell, "on"),
            (OptionName::MonitorSilence, "60"),
        ] {
            handler
                .set_option(ScopeSelector::Window(target.clone()), option, value)
                .await;
        }
    }
    for session in [owner.clone(), peer, external] {
        for option in [OptionName::ActivityAction, OptionName::BellAction] {
            handler
                .set_option(ScopeSelector::Session(session.clone()), option, "any")
                .await;
        }
    }
    let timer_generations = family_targets
        .iter()
        .map(|target| {
            handler
                .silence_timer_generation_for_test(target)
                .expect("family silence timer is armed")
        })
        .collect::<Vec<_>>();
    let (pane_id, generation, window_id) = pane_identity(&handler, &owner_target).await;
    let mut lifecycle = handler.subscribe_lifecycle_events();

    handler
        .handle_pane_alert_event(pane_event(owner, pane_id, generation))
        .await;

    {
        let state = handler.state.lock().await;
        for target in &family_targets {
            let session = state
                .sessions
                .session(target.session_name())
                .expect("family session survives");
            assert_eq!(
                session
                    .window_at(target.window_index())
                    .expect("family window survives")
                    .id(),
                window_id
            );
            let flags = session.winlink_alert_flags(target.window_index());
            assert!(
                flags.contains(WINLINK_ACTIVITY),
                "activity flag for {target}"
            );
            assert!(flags.contains(WINLINK_BELL), "bell flag for {target}");
        }
    }
    for (target, previous_generation) in family_targets.iter().zip(timer_generations) {
        assert_eq!(
            handler.silence_timer_generation_for_test(target),
            Some(previous_generation.saturating_add(1)),
            "one pane-alert batch resets the family silence timer once for {target}"
        );
    }
    let expected_targets = family_targets.into_iter().collect::<HashSet<_>>();
    let (activity_targets, bell_targets) = collect_alert_hook_targets(&mut lifecycle);
    assert_eq!(activity_targets, expected_targets);
    assert_eq!(bell_targets, expected_targets);
}

#[tokio::test]
async fn pane_alert_survives_an_earlier_alias_added_between_prepare_and_apply() {
    let handler = RequestHandler::new();
    let owner = handler
        .create_started_session(Quiet("z-pane-alert-added-owner"))
        .await;
    let alias = handler
        .create_started_session(Quiet("a-pane-alert-added-alias"))
        .await;
    let owner_target = WindowTarget::with_window(owner.clone(), 0);
    let alias_target = WindowTarget::with_window(alias.clone(), 1);
    for option in [OptionName::MonitorActivity, OptionName::MonitorBell] {
        handler
            .set_option(ScopeSelector::Window(owner_target.clone()), option, "on")
            .await;
    }
    for session in [owner.clone(), alias] {
        for option in [OptionName::ActivityAction, OptionName::BellAction] {
            handler
                .set_option(ScopeSelector::Session(session.clone()), option, "any")
                .await;
        }
    }
    let (pane_id, generation, window_id) = pane_identity(&handler, &owner_target).await;
    let prepared = handler
        .prepare_pane_alert_event(pane_event(owner, pane_id, generation))
        .await
        .expect("pane alert prepares before the alias exists");

    handler
        .handle_ok(LinkWindowRequest::fixture((&owner_target, &alias_target)))
        .await;
    for option in [OptionName::MonitorActivity, OptionName::MonitorBell] {
        handler
            .set_option(ScopeSelector::Window(alias_target.clone()), option, "on")
            .await;
    }
    let mut lifecycle = handler.subscribe_lifecycle_events();
    handler
        .apply_prepared_pane_alert_events(vec![prepared])
        .await;

    let expected_targets = [owner_target, alias_target]
        .into_iter()
        .collect::<HashSet<_>>();
    {
        let state = handler.state.lock().await;
        for target in &expected_targets {
            let session = state
                .sessions
                .session(target.session_name())
                .expect("alias session survives");
            assert_eq!(
                session
                    .window_at(target.window_index())
                    .expect("linked alias survives")
                    .id(),
                window_id
            );
            let flags = session.winlink_alert_flags(target.window_index());
            assert!(
                flags.contains(WINLINK_ACTIVITY),
                "activity flag for {target}"
            );
            assert!(flags.contains(WINLINK_BELL), "bell flag for {target}");
        }
    }
    let (activity_targets, bell_targets) = collect_alert_hook_targets(&mut lifecycle);
    assert_eq!(activity_targets, expected_targets);
    assert_eq!(bell_targets, expected_targets);
}

#[tokio::test]
async fn pane_alert_reindex_keeps_hooks_name_and_flags_on_the_original_window_id() {
    let handler = RequestHandler::new();
    let destination = handler
        .create_started_session(Quiet("pane-alert-reindex-destination"))
        .await;
    let alerted = handler.create_started_window(Quiet(&destination)).await;
    let source = handler
        .create_started_session(Quiet("pane-alert-reindex-source"))
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::MonitorActivity,
            "on",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::AutomaticRenameFormat,
            "stable-pane-alert-name",
        )
        .await;
    handler
        .set_option_by_name(OptionScopeSelector::ServerGlobal, "set-clipboard", "on")
        .await;
    let (pane_id, generation, alerted_window_id) = pane_identity(&handler, &alerted).await;
    for (hook, buffer) in [
        (HookName::PaneTitleChanged, "stable-pane-title"),
        (HookName::PaneSetClipboard, "stable-pane-clipboard"),
    ] {
        handler
            .set_global_hook(
                hook,
                &format!(
                    "if-shell -F '#{{==:#{{window_id}}:#{{window_index}},{alerted_window_id}:2}}' 'set-buffer -b {buffer} ok' 'set-buffer -b {buffer} bad'"
                ),
            )
            .await;
    }
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let pause = handler.install_pane_alert_apply_pause();
    let task_handler = handler.clone();
    let task_session = destination.clone();
    let mut task = tokio::spawn(async move {
        task_handler
            .handle_pane_alert_event(pane_event(task_session, pane_id, generation))
            .await;
    });
    timeout(Duration::from_secs(3), pause.reached.notified())
        .await
        .expect("pane alert reaches final-apply pause");

    handler
        .handle_ok(LinkWindowRequest {
            after: true,
            ..Fixture::fixture((
                WindowTarget::with_window(source, 0),
                WindowTarget::with_window(destination.clone(), 0),
            ))
        })
        .await;
    pause.release.notify_one();
    timeout(Duration::from_secs(5), &mut task)
        .await
        .expect("pane alert finishes after reindex")
        .expect("pane alert task succeeds");

    dispatch_expected_hooks(
        &handler,
        &mut lifecycle,
        &[HookName::PaneTitleChanged, HookName::PaneSetClipboard],
    )
    .await;
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&destination)
        .expect("destination survives");
    let inserted = session.window_at(1).expect("inserted window exists");
    let moved = session.window_at(2).expect("alerted window moved");
    assert_eq!(moved.id(), alerted_window_id);
    assert_ne!(inserted.id(), alerted_window_id);
    assert_eq!(moved.name(), Some("stable-pane-alert-name"));
    assert_ne!(inserted.name(), Some("stable-pane-alert-name"));
    let moved_flags = session.winlink_alert_flags(2);
    assert!(moved_flags.contains(WINLINK_ACTIVITY));
    assert!(moved_flags.contains(WINLINK_BELL));
    assert!(!session
        .winlink_alert_flags(1)
        .intersects(WINLINK_ACTIVITY.union(WINLINK_BELL)));
    assert_eq!(
        buffer_text(&state, "stable-pane-title").as_deref(),
        Some("ok")
    );
    assert_eq!(
        buffer_text(&state, "stable-pane-clipboard").as_deref(),
        Some("ok")
    );
}

#[tokio::test]
async fn pane_alert_replacement_fails_closed_before_hooks_name_or_flags_reach_reused_slot() {
    let handler = RequestHandler::new();
    let destination = handler
        .create_started_session(Quiet("pane-alert-replace-destination"))
        .await;
    let alerted = handler.create_started_window(Quiet(&destination)).await;
    let source = handler
        .create_started_session(Quiet("pane-alert-replace-source"))
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::MonitorActivity,
            "on",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::AutomaticRenameFormat,
            "stale-pane-alert-name",
        )
        .await;
    handler
        .set_option_by_name(OptionScopeSelector::ServerGlobal, "set-clipboard", "on")
        .await;
    let (pane_id, generation, alerted_window_id) = pane_identity(&handler, &alerted).await;
    for (hook, buffer) in [
        (HookName::PaneTitleChanged, "stale-pane-title"),
        (HookName::PaneSetClipboard, "stale-pane-clipboard"),
    ] {
        handler
            .set_global_hook(hook, &format!("set-buffer -b {buffer} wrong-target"))
            .await;
    }
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let pause = handler.install_pane_alert_apply_pause();
    let task_handler = handler.clone();
    let task_session = destination.clone();
    let mut task = tokio::spawn(async move {
        task_handler
            .handle_pane_alert_event(pane_event(task_session, pane_id, generation))
            .await;
    });
    timeout(Duration::from_secs(3), pause.reached.notified())
        .await
        .expect("pane alert reaches replacement pause");

    handler
        .handle_ok(LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((WindowTarget::with_window(source, 0), &alerted))
        })
        .await;
    pause.release.notify_one();
    timeout(Duration::from_secs(5), &mut task)
        .await
        .expect("pane alert finishes after replacement")
        .expect("pane alert task succeeds");

    dispatch_expected_hooks(
        &handler,
        &mut lifecycle,
        &[HookName::PaneTitleChanged, HookName::PaneSetClipboard],
    )
    .await;
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&destination)
        .expect("destination survives");
    let replacement = session
        .window_at(alerted.window_index())
        .expect("replacement occupies old slot");
    assert_ne!(replacement.id(), alerted_window_id);
    assert_ne!(replacement.name(), Some("stale-pane-alert-name"));
    assert!(!session
        .winlink_alert_flags(alerted.window_index())
        .intersects(WINLINK_ACTIVITY.union(WINLINK_BELL)));
    assert_eq!(buffer_text(&state, "stale-pane-title"), None);
    assert_eq!(buffer_text(&state, "stale-pane-clipboard"), None);
}

#[tokio::test]
async fn alert_plan_effects_follow_session_id_through_hook_rename_and_name_reuse() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_started_session(Quiet("alert-plan-alpha"))
        .await;
    let alerted = handler.create_started_window(Quiet(&alpha)).await;
    let beta = session_name("alert-plan-beta");
    let original_session_id = handler.session_id_for_test(&alpha).await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::MonitorBell,
            "on",
        )
        .await;
    for (option, value) in [
        (OptionName::BellAction, "any"),
        (OptionName::VisualBell, "both"),
    ] {
        handler
            .set_option(ScopeSelector::Session(alpha.clone()), option, value)
            .await;
    }
    handler
        .set_global_hook(
            HookName::AlertBell,
            &format!("rename-session -t {alpha} {beta}"),
        )
        .await;
    let mut beta_rx = handler.attach_client(710, &alpha).await;
    drain_controls(&mut beta_rx).await;
    let mut lifecycle = handler.subscribe_lifecycle_events();
    let pause = handler.install_alert_plan_effect_pause();
    let task_handler = handler.clone();
    let task_target = alerted.clone();
    let mut task = tokio::spawn(async move {
        task_handler
            .alerts_queue_window(task_target, WINDOW_BELL)
            .await;
    });
    timeout(Duration::from_secs(3), pause.reached.notified())
        .await
        .expect("alert plan pauses after hook enqueue");
    dispatch_expected_hooks(&handler, &mut lifecycle, &[HookName::AlertBell]).await;
    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&alpha).is_none());
        assert_eq!(
            state
                .sessions
                .session(&beta)
                .expect("hook renamed beta")
                .id(),
            original_session_id
        );
    }

    let reused_alpha = handler.create_started_session(Quiet(&alpha)).await;
    let mut alpha_rx = handler.attach_client(711, reused_alpha).await;
    drain_controls(&mut beta_rx).await;
    drain_controls(&mut alpha_rx).await;
    pause.release.notify_one();
    timeout(Duration::from_secs(5), &mut task)
        .await
        .expect("renamed alert plan finishes")
        .expect("alert plan task succeeds");

    let deadline = Instant::now() + Duration::from_secs(3);
    let (mut bell, mut overlay, mut refresh) = (false, false, false);
    while !(bell && overlay && refresh) {
        let control = timeout(
            deadline.saturating_duration_since(Instant::now()),
            beta_rx.recv(),
        )
        .await
        .expect("original session receives all alert effects")
        .expect("original client stays attached");
        match control {
            AttachControl::Write(bytes) if bytes == vec![0x07] => bell = true,
            AttachControl::Overlay(_) => overlay = true,
            AttachControl::Refresh | AttachControl::Switch(_) => refresh = true,
            _ => {}
        }
    }
    assert!(
        timeout(Duration::from_millis(150), alpha_rx.recv())
            .await
            .is_err(),
        "reused alpha incarnation must receive no old alert-plan effect"
    );
}

#[tokio::test]
async fn alert_plan_effects_fail_closed_after_session_destroy_and_name_reuse() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_started_session(Quiet("alert-plan-destroy-alpha"))
        .await;
    let alerted = handler.create_started_window(Quiet(&alpha)).await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::MonitorBell,
            "on",
        )
        .await;
    for (option, value) in [
        (OptionName::BellAction, "any"),
        (OptionName::VisualBell, "both"),
    ] {
        handler
            .set_option(ScopeSelector::Session(alpha.clone()), option, value)
            .await;
    }
    let pause = handler.install_alert_plan_effect_pause();
    let task_handler = handler.clone();
    let mut task = tokio::spawn(async move {
        task_handler.alerts_queue_window(alerted, WINDOW_BELL).await;
    });
    timeout(Duration::from_secs(3), pause.reached.notified())
        .await
        .expect("alert plan pauses before effects");
    handler.handle_ok(KillSessionRequest::fixture(&alpha)).await;
    let reused_alpha = handler.create_started_session(Quiet(&alpha)).await;
    let mut control_rx = handler.attach_client(712, reused_alpha).await;
    drain_controls(&mut control_rx).await;
    pause.release.notify_one();
    timeout(Duration::from_secs(5), &mut task)
        .await
        .expect("destroyed alert plan finishes")
        .expect("alert plan task succeeds");
    assert_no_alert_effects(
        &mut control_rx,
        "new alpha incarnation must receive no destroyed-session alert effect",
    )
    .await;
}

#[tokio::test]
async fn alert_overlay_fails_closed_when_session_name_is_reused_after_resolution() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_started_session(Quiet("alert-overlay-reuse"))
        .await;
    let alerted = handler.create_started_window(Quiet(&alpha)).await;
    handler
        .set_option(
            ScopeSelector::Window(alerted.clone()),
            OptionName::MonitorBell,
            "on",
        )
        .await;
    for (option, value) in [
        (OptionName::BellAction, "any"),
        (OptionName::VisualBell, "on"),
    ] {
        handler
            .set_option(ScopeSelector::Session(alpha.clone()), option, value)
            .await;
    }

    let pause = handler.install_alert_overlay_identity_pause();
    let task_handler = handler.clone();
    let mut alert = tokio::spawn(async move {
        task_handler.alerts_queue_window(alerted, WINDOW_BELL).await;
    });
    timeout(Duration::from_secs(3), pause.wait_until_reached())
        .await
        .expect("alert overlay pauses after resolving the stable session");

    handler.handle_ok(KillSessionRequest::fixture(&alpha)).await;
    let replacement = handler.create_started_session(Quiet(&alpha)).await;
    let mut control_rx = handler.attach_client(713, replacement).await;
    drain_controls(&mut control_rx).await;

    pause.release();
    timeout(Duration::from_secs(5), &mut alert)
        .await
        .expect("stale alert overlay finishes")
        .expect("alert overlay task succeeds");
    assert_no_alert_effects(
        &mut control_rx,
        "replacement session must receive no stale alert overlay",
    )
    .await;
}
