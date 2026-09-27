use super::*;
use rmux_proto::request::SwitchClientExt3Request;
use rmux_proto::SplitWindowExtRequest;

/// One two-pane window shared by its owner, the owner's group peer and a linked peer, with
/// pane zero active in every alias.
pub(super) struct LinkedPaneFixture {
    pub(super) owner: SessionName,
    pub(super) grouped_peer: SessionName,
    pub(super) linked_peer: SessionName,
    pub(super) pane_one_id: rmux_proto::PaneId,
}

impl LinkedPaneFixture {
    /// Links `owner`'s window 0 into index 1 of a new `{label}-linked` session, then selects
    /// pane zero in all three aliases of that window.
    pub(super) async fn link(
        handler: &RequestHandler,
        owner: SessionName,
        grouped_peer: SessionName,
        label: &str,
    ) -> Self {
        let linked_peer = create_session(handler, format!("{label}-linked")).await;
        handler
            .handle_ok(LinkWindowRequest::fixture((
                WindowTarget::with_window(owner.clone(), 0),
                WindowTarget::with_window(linked_peer.clone(), 1),
            )))
            .await;

        let mut state = handler.state.lock().await;
        let pane_one_id = state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(1))
            .expect("fixture pane one exists")
            .id();
        let fixture = Self {
            owner,
            grouped_peer,
            linked_peer,
            pane_one_id,
        };
        for target in fixture.targets() {
            state
                .sessions
                .session_mut(target.session_name())
                .expect("fixture session exists")
                .select_pane_in_window(target.window_index(), 0)
                .expect("fixture pane zero selection succeeds");
        }
        fixture
    }

    /// The shared window in the owner, the group peer and the linked peer.
    pub(super) fn targets(&self) -> [WindowTarget; 3] {
        [
            WindowTarget::with_window(self.owner.clone(), 0),
            WindowTarget::with_window(self.grouped_peer.clone(), 0),
            WindowTarget::with_window(self.linked_peer.clone(), 1),
        ]
    }
}

async fn linked_two_pane_fixture(handler: &RequestHandler, label: &str) -> LinkedPaneFixture {
    let owner = create_session(handler, format!("{label}-owner")).await;
    let grouped_peer = create_grouped_session(handler, format!("{label}-grouped"), &owner).await;

    // The default command is an interactive shell whose startup output is
    // unbounded in time. Split the quiet command every other fixture pane runs
    // so this window stops producing pane activity once its panes are up.
    let split = handler
        .handle_ok(SplitWindowExtRequest {
            command: Some(quiet_command()),
            ..Fixture::fixture(&owner)
        })
        .await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&split.pane)
        .await;

    let fixture = LinkedPaneFixture::link(handler, owner, grouped_peer, label).await;
    settle_linked_fixture_activity(handler, &fixture).await;
    fixture
}

/// Waits until the fixture's shared window stops recording pane activity.
///
/// The fixture runs real pane processes, and output from a pane that is not its
/// session's active pane refreshes every attached client of the linked family:
/// `pane_alert_callback` coalesces such output for 50 ms and then calls
/// `refresh_attached_session` once per family member, which reaches every client
/// of those sessions. That refresh is correct and independent of the command
/// under test, so a client's control channel only reports what a command sent
/// once the panes behind it are quiet. Draining the channels is not enough on
/// its own: the drain stops after a fixed idle window while pane startup can
/// still be running.
async fn settle_linked_fixture_activity(handler: &RequestHandler, fixture: &LinkedPaneFixture) {
    const POLL_INTERVAL: Duration = Duration::from_millis(25);
    const STABLE_FOR: Duration = Duration::from_millis(300);
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

    let target = WindowTarget::with_window(fixture.owner.clone(), 0);
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    let mut previous = linked_fixture_activity(handler, &target).await;
    let mut stable_since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let current = linked_fixture_activity(handler, &target).await;
        let now = tokio::time::Instant::now();
        if current == previous {
            if now.duration_since(stable_since) >= STABLE_FOR {
                return;
            }
        } else {
            previous = current;
            stable_since = now;
        }
        assert!(
            now < deadline,
            "linked fixture pane activity did not settle for {target}: {previous:?}"
        );
    }
}

async fn linked_fixture_activity(handler: &RequestHandler, target: &WindowTarget) -> Vec<i64> {
    let state = handler.state.lock().await;
    let Some(window) = state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
    else {
        return Vec::new();
    };
    std::iter::once(window.activity_at())
        .chain(window.panes().iter().map(rmux_core::Pane::activity_at))
        .collect()
}

async fn assert_linked_active_pane(
    handler: &RequestHandler,
    fixture: &LinkedPaneFixture,
    expected: u32,
) {
    let state = handler.state.lock().await;
    for target in fixture.targets() {
        assert_eq!(
            state
                .sessions
                .session(target.session_name())
                .and_then(|session| session.window_at(target.window_index()))
                .expect("linked window alias exists")
                .active_pane_index(),
            expected,
            "active pane diverged for {target}"
        );
    }
}

/// Switches client `requester_pid` to pane one of the owner's shared window.
async fn switch_to_pane_one(
    handler: &RequestHandler,
    requester_pid: u32,
    fixture: &LinkedPaneFixture,
) {
    let response = handler
        .handle_switch_client_ext3(
            requester_pid,
            SwitchClientExt3Request {
                target_client: None,
                target: Some(format!("{}:0.1", fixture.owner)),
                key_table: None,
                last_session: false,
                next_session: false,
                previous_session: false,
                toggle_read_only: false,
                sort_order: None,
                skip_environment_update: false,
                zoom: false,
            },
        )
        .await;
    assert!(
        matches!(response, Response::SwitchClient(_)),
        "{response:?}"
    );
}

#[tokio::test]
async fn select_pane_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "select-linked").await;

    let mut control_rx = handler.attach_client(7_091, &fixture.grouped_peer).await;
    drain_attach_controls(&mut control_rx).await;

    let pane = PaneTarget::with_window(fixture.owner.clone(), 0, 1);
    handler.handle_ok(SelectPaneRequest::fixture(pane)).await;
    assert_linked_active_pane(&handler, &fixture, 1).await;

    assert_refresh(
        timeout(Duration::from_secs(2), control_rx.recv())
            .await
            .expect("attached grouped alias refresh is bounded")
            .expect("attached grouped alias remains registered"),
    );
}

#[tokio::test]
async fn sdk_select_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "sdk-select-linked").await;

    let response = handler
        .handle(Request::PaneSelect(PaneSelectRequest {
            target: PaneTargetRef::by_id(fixture.linked_peer.clone(), fixture.pane_one_id),
            title: None,
        }))
        .await;
    assert!(matches!(response, Response::SelectPane(_)), "{response:?}");
    assert_linked_active_pane(&handler, &fixture, 1).await;
}

#[tokio::test]
async fn adjacent_select_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "adjacent-linked").await;

    let response = handler
        .handle(Request::SelectPaneAdjacent(SelectPaneAdjacentRequest {
            target: PaneTarget::with_window(fixture.owner.clone(), 0, 0),
            direction: SelectPaneDirection::Down,
            preserve_zoom: false,
        }))
        .await;
    assert!(matches!(response, Response::SelectPane(_)), "{response:?}");
    assert_linked_active_pane(&handler, &fixture, 1).await;
}

#[tokio::test]
async fn last_pane_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "last-linked").await;

    let response = handler
        .handle(Request::LastPane(LastPaneRequest {
            target: WindowTarget::with_window(fixture.owner.clone(), 0),
            preserve_zoom: false,
            input_disabled: None,
        }))
        .await;
    assert!(matches!(response, Response::LastPane(_)), "{response:?}");
    assert_linked_active_pane(&handler, &fixture, 1).await;
}

#[tokio::test]
async fn attached_mouse_focus_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "mouse-select-linked").await;
    let requester_pid = std::process::id();
    let _control_rx = handler.attach_client(requester_pid, &fixture.owner).await;
    let identity = handler.active_attach_identity_for_test(requester_pid).await;
    let (session_id, window_id) = {
        let state = handler.state.lock().await;
        let session = state
            .sessions
            .session(&fixture.owner)
            .expect("fixture owner exists");
        (session.id(), session.window().id().as_u32())
    };

    handler
        .select_attached_mouse_focus(
            identity,
            &fixture.owner,
            session_id,
            window_id,
            fixture.pane_one_id,
        )
        .await
        .expect("attached mouse focus succeeds");
    assert_linked_active_pane(&handler, &fixture, 1).await;
}

#[tokio::test]
async fn switch_client_pane_target_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "switch-select-linked").await;
    let requester_pid = std::process::id();
    let mut control_rx = handler.attach_client(requester_pid, &fixture.owner).await;
    let mut peer_control_rx = handler
        .attach_client(requester_pid.saturating_add(1), &fixture.grouped_peer)
        .await;
    drain_attach_control_pair(&mut control_rx, &mut peer_control_rx).await;

    switch_to_pane_one(&handler, requester_pid, &fixture).await;
    assert_linked_active_pane(&handler, &fixture, 1).await;

    assert_refresh(
        timeout(Duration::from_secs(2), control_rx.recv())
            .await
            .expect("switched client update is bounded")
            .expect("switched client remains registered"),
    );
    assert_refresh(
        timeout(Duration::from_secs(2), peer_control_rx.recv())
            .await
            .expect("linked peer refresh is bounded")
            .expect("linked peer remains registered"),
    );
    let redundant = control_rx.try_recv();
    assert!(
        redundant.is_err(),
        "the switched client must not receive a redundant refresh: {redundant:?}"
    );
}

#[tokio::test]
async fn control_switch_client_pane_target_synchronizes_linked_and_grouped_window_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_two_pane_fixture(&handler, "control-switch-select-linked").await;
    let requester_pid = std::process::id().saturating_add(200);
    let (_, mut event_rx) = handler
        .register_control_for_test(requester_pid, Some(&fixture.owner))
        .await;
    while event_rx.try_recv().is_ok() {}

    let mut peer_control_rx = handler
        .attach_client(requester_pid.saturating_add(1), &fixture.grouped_peer)
        .await;
    drain_attach_controls(&mut peer_control_rx).await;

    switch_to_pane_one(&handler, requester_pid, &fixture).await;
    assert_linked_active_pane(&handler, &fixture, 1).await;

    assert_refresh(
        timeout(Duration::from_secs(2), peer_control_rx.recv())
            .await
            .expect("control switch linked peer refresh is bounded")
            .expect("linked peer remains registered"),
    );
}
