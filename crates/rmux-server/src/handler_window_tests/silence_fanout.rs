use super::*;
use crate::test_fixtures::TestRequest;

fn timer_snapshot(handler: &RequestHandler, target: &WindowTarget) -> (u64, tokio::time::Instant) {
    handler
        .silence_timer_snapshot_for_test(target)
        .expect("silence timer is armed")
}

async fn settled_timer_snapshot(
    handler: &RequestHandler,
    target: &WindowTarget,
) -> (u64, tokio::time::Instant) {
    // Observe the production timer until any in-flight pane startup activity
    // has finished re-arming it. Do not replace the deadline here: doing so
    // would manufacture the baseline that these fanout tests are meant to
    // preserve.
    let settle_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut previous = timer_snapshot(handler, target);
    let mut stable_since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let current = timer_snapshot(handler, target);
        let now = tokio::time::Instant::now();
        if current == previous {
            if now.duration_since(stable_since) >= Duration::from_millis(500) {
                return current;
            }
        } else {
            previous = current;
            stable_since = now;
        }
        if now >= settle_deadline {
            panic!(
                "silence timer did not settle naturally for {target}; last snapshot: {previous:?}"
            );
        }
    }
}

async fn create_destination_group(
    handler: &RequestHandler,
    owner_name: &str,
    peer_name: &str,
) -> (SessionName, SessionName) {
    let owner = create_session(handler, owner_name).await;
    let peer = create_grouped_session(handler, peer_name, &owner).await;
    (owner, peer)
}

async fn expire_session_window_zero(handler: &RequestHandler, session: &SessionName) {
    let target = WindowTarget::with_window(session.clone(), 0);
    let identity = handler
        .silence_timer_identity_for_test(&target)
        .expect("silence timer is armed before expiry");
    handler
        .expire_silence_timer_for_test(target, identity.0, identity.1, identity.2)
        .await;
}

#[tokio::test]
async fn new_group_peer_arms_fresh_when_matching_source_alias_is_unmonitored() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "new-peer-unmonitored-owner").await;
    enable_global_monitor_silence(&handler).await;

    handler
        .set_option(
            ScopeSelector::Session(owner.clone()),
            OptionName::MonitorSilence,
            "0",
        )
        .await;
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&WindowTarget::with_window(owner.clone(), 0)),
        None
    );

    let peer = create_grouped_session(&handler, "new-peer-unmonitored-peer", &owner).await;
    assert!(
        handler
            .silence_timer_snapshot_for_test(&WindowTarget::with_window(peer, 0))
            .is_some(),
        "an unmonitored source is not an expired source; the monitored peer arms fresh"
    );
}

async fn assert_new_group_peer_uses_requested_template_silence_state(
    label: &str,
    template_is_expired: bool,
) {
    let handler = RequestHandler::new();
    let first = create_session(&handler, format!("a-{label}")).await;
    let template = create_grouped_session(&handler, format!("b-{label}"), &first).await;
    enable_global_monitor_silence(&handler).await;

    if template_is_expired {
        handler
            .set_option(
                ScopeSelector::Session(first.clone()),
                OptionName::MonitorSilence,
                "0",
            )
            .await;
        expire_session_window_zero(&handler, &template).await;
    } else {
        expire_session_window_zero(&handler, &first).await;
        handler
            .set_option(
                ScopeSelector::Session(template.clone()),
                OptionName::MonitorSilence,
                "0",
            )
            .await;
    }

    let created = create_grouped_session(&handler, format!("c-{label}"), &template).await;
    let created_target = WindowTarget::with_window(created.clone(), 0);
    assert_eq!(
        handler
            .silence_timer_snapshot_for_test(&created_target)
            .is_some(),
        !template_is_expired,
        "the new peer must inherit the requested template's monitored state"
    );
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&created)
            .expect("new grouped peer exists")
            .winlink_alert_flags(0)
            .contains(rmux_core::WINLINK_SILENCE),
        template_is_expired,
        "the cloned flag and inherited timer state must describe the same template"
    );
}

#[tokio::test]
async fn new_group_peer_prefers_requested_template_over_alphabetical_group_member() {
    assert_new_group_peer_uses_requested_template_silence_state("template-expired", true).await;
    assert_new_group_peer_uses_requested_template_silence_state("template-unmonitored", false)
        .await;
}

#[tokio::test]
async fn link_window_fans_out_source_silence_deadline_only_to_new_group_aliases() {
    let handler = RequestHandler::new();
    let source_session = create_session(&handler, "link-fanout-source").await;
    let external_session = create_session(&handler, "link-fanout-external").await;
    let source = WindowTarget::with_window(source_session, 0);
    let external = WindowTarget::with_window(external_session, 1);
    TestRequest::send_ok(&handler, LinkWindowRequest::fixture((&source, &external))).await;
    let (owner, peer) = create_destination_group(
        &handler,
        "link-fanout-destination-owner",
        "link-fanout-destination-peer",
    )
    .await;
    enable_global_monitor_silence(&handler).await;

    let source_before = settled_timer_snapshot(&handler, &source).await;
    let external_before = settled_timer_snapshot(&handler, &external).await;
    let owner_unrelated = WindowTarget::with_window(owner.clone(), 0);
    let peer_unrelated = WindowTarget::with_window(peer.clone(), 0);
    let owner_unrelated_before = settled_timer_snapshot(&handler, &owner_unrelated).await;
    let peer_unrelated_before = settled_timer_snapshot(&handler, &peer_unrelated).await;
    for (label, snapshot) in [
        ("external alias", external_before),
        ("owner unrelated", owner_unrelated_before),
        ("peer unrelated", peer_unrelated_before),
    ] {
        assert_ne!(
            snapshot.1, source_before.1,
            "{label} needs a distinct natural deadline for this preservation assertion to be meaningful"
        );
    }

    let owner_destination = WindowTarget::with_window(owner, 1);
    let peer_destination = WindowTarget::with_window(peer, 1);
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source, &owner_destination)),
    )
    .await;

    assert_eq!(timer_snapshot(&handler, &source), source_before);
    assert_eq!(timer_snapshot(&handler, &external), external_before);
    assert_eq!(
        timer_snapshot(&handler, &owner_unrelated),
        owner_unrelated_before
    );
    assert_eq!(
        timer_snapshot(&handler, &peer_unrelated),
        peer_unrelated_before
    );
    for destination in [owner_destination, peer_destination] {
        assert_eq!(
            timer_snapshot(&handler, &destination).1,
            source_before.1,
            "new destination alias {destination} inherits the addressed source deadline"
        );
    }
}

#[tokio::test]
async fn link_window_kill_clears_replaced_group_alerts_before_deadline_fanout() {
    let handler = RequestHandler::new();
    let source_session = create_session(&handler, "link-kill-alert-source").await;
    let source = WindowTarget::with_window(source_session, 0);
    let (owner, peer) =
        create_destination_group(&handler, "link-kill-alert-owner", "link-kill-alert-peer").await;
    enable_global_monitor_silence(&handler).await;
    let source_before = settled_timer_snapshot(&handler, &source).await;
    let stale_flags = rmux_core::WINLINK_ALERTFLAGS;
    {
        let mut state = handler.state.lock().await;
        for session_name in [&owner, &peer] {
            assert!(state
                .sessions
                .session_mut(session_name)
                .expect("destination group member exists")
                .add_winlink_alert_flags(0, stale_flags));
        }
    }

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((source, WindowTarget::with_window(owner.clone(), 0)))
        },
    )
    .await;

    let state = handler.state.lock().await;
    for session_name in [owner, peer] {
        let target = WindowTarget::with_window(session_name.clone(), 0);
        assert!(
            state
                .sessions
                .session(&session_name)
                .expect("destination group member survives")
                .winlink_alert_flags(0)
                .is_empty(),
            "alerts from the replaced WindowId must not survive at {target}"
        );
        assert_eq!(
            timer_snapshot(&handler, &target).1,
            source_before.1,
            "the active source deadline still fans out to {target}"
        );
    }
}

#[tokio::test]
async fn link_window_fans_out_expired_silence_state_without_rearming_group_aliases() {
    let handler = RequestHandler::new();
    let source_session = create_session(&handler, "link-expired-fanout-source").await;
    let source = WindowTarget::with_window(source_session, 0);
    let (owner, peer) = create_destination_group(
        &handler,
        "link-expired-fanout-owner",
        "link-expired-fanout-peer",
    )
    .await;
    enable_global_monitor_silence(&handler).await;

    let identity = handler
        .silence_timer_identity_for_test(&source)
        .expect("source silence timer is armed before expiry");
    handler
        .expire_silence_timer_for_test(source.clone(), identity.0, identity.1, identity.2)
        .await;
    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);

    let owner_destination = WindowTarget::with_window(owner, 1);
    let peer_destination = WindowTarget::with_window(peer, 1);
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((source, &owner_destination)),
    )
    .await;

    let state = handler.state.lock().await;
    for destination in [owner_destination, peer_destination] {
        assert_eq!(
            handler.silence_timer_snapshot_for_test(&destination),
            None,
            "expired alias {destination} must not be rearmed"
        );
        assert!(
            state
                .sessions
                .session(destination.session_name())
                .expect("destination group member survives")
                .winlink_alert_flags(destination.window_index())
                .contains(rmux_core::WINLINK_SILENCE),
            "expired silence flag must fan out to {destination}"
        );
    }
}

#[tokio::test]
async fn move_window_fans_out_source_silence_deadline_without_touching_external_alias() {
    let handler = RequestHandler::new();
    let source_session = create_session(&handler, "move-fanout-source").await;
    let external_session = create_session(&handler, "move-fanout-external").await;
    let source = WindowTarget::with_window(source_session, 0);
    let external = WindowTarget::with_window(external_session, 1);
    TestRequest::send_ok(&handler, LinkWindowRequest::fixture((&source, &external))).await;
    let (owner, peer) = create_destination_group(
        &handler,
        "move-fanout-destination-owner",
        "move-fanout-destination-peer",
    )
    .await;
    enable_global_monitor_silence(&handler).await;

    let source_before = settled_timer_snapshot(&handler, &source).await;
    let external_before = settled_timer_snapshot(&handler, &external).await;
    let owner_unrelated = WindowTarget::with_window(owner.clone(), 0);
    let peer_unrelated = WindowTarget::with_window(peer.clone(), 0);
    let owner_unrelated_before = settled_timer_snapshot(&handler, &owner_unrelated).await;
    let peer_unrelated_before = settled_timer_snapshot(&handler, &peer_unrelated).await;
    for (label, snapshot) in [
        ("external alias", external_before),
        ("owner unrelated", owner_unrelated_before),
        ("peer unrelated", peer_unrelated_before),
    ] {
        assert_ne!(
            snapshot.1, source_before.1,
            "{label} needs a distinct natural deadline for this preservation assertion to be meaningful"
        );
    }
    let owner_destination = WindowTarget::with_window(owner.clone(), 1);
    let peer_destination = WindowTarget::with_window(peer, 1);

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest::fixture((&source, &owner_destination)),
    )
    .await;

    assert_eq!(handler.silence_timer_snapshot_for_test(&source), None);
    assert_eq!(timer_snapshot(&handler, &external), external_before);
    assert_eq!(
        timer_snapshot(&handler, &owner_unrelated),
        owner_unrelated_before
    );
    assert_eq!(
        timer_snapshot(&handler, &peer_unrelated),
        peer_unrelated_before
    );
    for destination in [owner_destination, peer_destination] {
        assert_eq!(
            timer_snapshot(&handler, &destination).1,
            source_before.1,
            "new destination alias {destination} inherits the moved source deadline"
        );
    }
}

#[tokio::test]
async fn move_window_fanout_never_overwrites_a_represented_group_peer_deadline() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "move-represented-deadline-owner").await;
    insert_window(&handler, &owner, 1).await;
    let peer = create_grouped_session(&handler, "move-represented-deadline-peer", &owner).await;
    enable_global_monitor_silence(&handler).await;

    let owner_source = WindowTarget::with_window(owner.clone(), 0);
    let peer_source = WindowTarget::with_window(peer.clone(), 0);
    let owner_unrelated = WindowTarget::with_window(owner.clone(), 1);
    let peer_unrelated = WindowTarget::with_window(peer.clone(), 1);
    let owner_source_before = settled_timer_snapshot(&handler, &owner_source).await;
    let peer_source_before = settled_timer_snapshot(&handler, &peer_source).await;
    let owner_unrelated_before = settled_timer_snapshot(&handler, &owner_unrelated).await;
    let peer_unrelated_before = settled_timer_snapshot(&handler, &peer_unrelated).await;
    assert_ne!(
        owner_source_before.1, peer_source_before.1,
        "represented aliases need distinct natural deadlines to prove the peer wins"
    );

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest::fixture((owner_source, WindowTarget::with_window(owner.clone(), 2))),
    )
    .await;

    assert_eq!(
        timer_snapshot(&handler, &WindowTarget::with_window(owner, 2)).1,
        owner_source_before.1
    );
    assert_eq!(
        timer_snapshot(&handler, &WindowTarget::with_window(peer, 2)).1,
        peer_source_before.1,
        "the peer's represented timer wins over the addressed source fanout"
    );
    assert_eq!(
        timer_snapshot(&handler, &owner_unrelated),
        owner_unrelated_before
    );
    assert_eq!(
        timer_snapshot(&handler, &peer_unrelated),
        peer_unrelated_before
    );
}

#[tokio::test]
async fn swap_window_fans_out_each_addressed_deadline_without_touching_external_links() {
    let handler = RequestHandler::new();
    let source_session = create_session(&handler, "swap-fanout-source").await;
    let source_external_session = create_session(&handler, "swap-fanout-source-external").await;
    let target_external_session = create_session(&handler, "swap-fanout-target-external").await;
    let source = WindowTarget::with_window(source_session, 0);
    let source_external = WindowTarget::with_window(source_external_session, 1);
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&source, &source_external)),
    )
    .await;
    let (owner, peer) = create_destination_group(
        &handler,
        "swap-fanout-destination-owner",
        "swap-fanout-destination-peer",
    )
    .await;
    let owner_target = WindowTarget::with_window(owner.clone(), 0);
    let target_external = WindowTarget::with_window(target_external_session, 1);
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&owner_target, &target_external)),
    )
    .await;
    enable_global_monitor_silence(&handler).await;

    let source_before = settled_timer_snapshot(&handler, &source).await;
    let source_external_before = settled_timer_snapshot(&handler, &source_external).await;
    let owner_before = settled_timer_snapshot(&handler, &owner_target).await;
    let peer_target = WindowTarget::with_window(peer, 0);
    let _peer_before = settled_timer_snapshot(&handler, &peer_target).await;
    let target_external_before = settled_timer_snapshot(&handler, &target_external).await;
    assert_ne!(
        source_before.1, owner_before.1,
        "addressed windows need distinct natural deadlines to prove the swap"
    );
    for (label, snapshot) in [
        ("source external alias", source_external_before),
        ("target external alias", target_external_before),
    ] {
        assert_ne!(
            snapshot.1, source_before.1,
            "{label} needs a distinct deadline from the source to prove it was untouched"
        );
        assert_ne!(
            snapshot.1, owner_before.1,
            "{label} needs a distinct deadline from the target to prove it was untouched"
        );
    }

    TestRequest::send_ok(
        &handler,
        SwapWindowRequest {
            source: source.clone(),
            target: owner_target.clone(),
            detached: true,
        },
    )
    .await;

    assert_eq!(timer_snapshot(&handler, &source).1, owner_before.1);
    assert_eq!(timer_snapshot(&handler, &owner_target).1, source_before.1);
    assert_eq!(timer_snapshot(&handler, &peer_target).1, source_before.1);
    assert_eq!(
        timer_snapshot(&handler, &source_external),
        source_external_before
    );
    assert_eq!(
        timer_snapshot(&handler, &target_external),
        target_external_before
    );
}
