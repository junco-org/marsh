use rmux_proto::{
    NewWindowRequest, OptionName, PaneTarget, ProcessCommand, RespawnPaneRequest, ScopeSelector,
    SessionName, SplitWindowExtRequest, TerminalSize,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration, Instant};

use super::RequestHandler;
use crate::control::ControlServerEvent;
use crate::pane_io::AttachControl;
use crate::test_fixtures::{wait_until, Fixture};

const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
enum TargetClientKind {
    None,
    Pty,
    Control,
}

#[derive(Clone, Copy, Debug)]
struct SelectionScenario {
    mode: &'static str,
    client: TargetClientKind,
    panes: usize,
    windows: usize,
    repetitions: usize,
    pid_seed: u32,
}

struct ActiveSelection {
    target: PaneTarget,
    session_id: String,
    window_id: String,
    pane_id: String,
}

enum TargetClientGuard {
    None,
    Pty {
        _receiver: mpsc::UnboundedReceiver<AttachControl>,
    },
    Control {
        _receiver: mpsc::Receiver<ControlServerEvent>,
    },
}

struct SelectionFixture {
    handler: RequestHandler,
    target: SessionName,
    observer_rx: mpsc::Receiver<ControlServerEvent>,
    _target_client: TargetClientGuard,
}

fn sleeping_process() -> ProcessCommand {
    ProcessCommand::Argv(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "exec sleep 60".to_owned(),
    ])
}

fn exiting_process() -> ProcessCommand {
    ProcessCommand::Argv(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "exit 0".to_owned(),
    ])
}

async fn register_target_client(
    handler: &RequestHandler,
    pid: u32,
    session: &SessionName,
    kind: TargetClientKind,
) -> TargetClientGuard {
    match kind {
        TargetClientKind::None => TargetClientGuard::None,
        TargetClientKind::Pty => TargetClientGuard::Pty {
            _receiver: handler.attach_client(pid, session).await,
        },
        TargetClientKind::Control => {
            let (_, receiver) = handler.register_control_for_test(pid, Some(session)).await;
            TargetClientGuard::Control {
                _receiver: receiver,
            }
        }
    }
}

async fn fixture(scenario: SelectionScenario) -> SelectionFixture {
    let handler = RequestHandler::new();
    let observer = handler
        .create_session(("observer", TerminalSize::new(100, 40)))
        .await;
    let target = handler
        .create_session(("target", TerminalSize::new(100, 40)))
        .await;
    for _ in 1..scenario.panes {
        handler
            .handle_ok(SplitWindowExtRequest {
                process_command: Some(sleeping_process()),
                detached: true,
                ..Fixture::fixture(&target)
            })
            .await;
    }
    for _ in 1..scenario.windows {
        handler
            .create_window(NewWindowRequest {
                process_command: Some(sleeping_process()),
                ..Fixture::fixture(&target)
            })
            .await;
    }
    let window_indexes = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&target)
            .expect("target session exists")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>()
    };
    for window_index in window_indexes {
        handler
            .set_window_size_policy(&target, window_index, scenario.mode)
            .await;
    }

    let (_, observer_rx) = handler
        .register_control_for_test(scenario.pid_seed, Some(&observer))
        .await;
    let target_client =
        register_target_client(&handler, scenario.pid_seed + 1, &target, scenario.client).await;
    let mut fixture = SelectionFixture {
        handler,
        target,
        observer_rx,
        _target_client: target_client,
    };
    let _ = drain_relevant_notifications(&mut fixture.observer_rx);
    fixture
}

async fn active_selection(handler: &RequestHandler, session_name: &SessionName) -> ActiveSelection {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("target session survives");
    let window_index = session.active_window_index();
    let window = session
        .window_at(window_index)
        .expect("active window survives");
    let pane = window.active_pane().expect("active pane survives");
    ActiveSelection {
        target: PaneTarget::with_window(session_name.clone(), window_index, pane.index()),
        session_id: session.id().to_string(),
        window_id: window.id().to_string(),
        pane_id: pane.id().to_string(),
    }
}

async fn inactive_pane_target(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> (PaneTarget, String) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("target session exists");
    let window_index = session.active_window_index();
    let window = session
        .window_at(window_index)
        .expect("active window exists");
    let active_id = window.active_pane().expect("active pane exists").id();
    let pane = window
        .panes()
        .iter()
        .find(|pane| pane.id() != active_id)
        .expect("inactive pane exists");
    (
        PaneTarget::with_window(session_name.clone(), window_index, pane.index()),
        pane.id().to_string(),
    )
}

async fn inactive_window_pane_target(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> (PaneTarget, String) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("target session exists");
    let active_window_index = session.active_window_index();
    let (window_index, window) = session
        .windows()
        .iter()
        .find(|(index, _)| **index != active_window_index)
        .expect("inactive window exists");
    let pane = window.active_pane().expect("inactive window has a pane");
    (
        PaneTarget::with_window(session_name.clone(), *window_index, pane.index()),
        window.id().to_string(),
    )
}

async fn set_remain_on_exit(handler: &RequestHandler, target: PaneTarget, enabled: bool) {
    let value = if enabled { "on" } else { "off" };
    handler
        .set_option(ScopeSelector::Pane(target), OptionName::RemainOnExit, value)
        .await;
}

async fn exit_and_wait_removed(handler: &RequestHandler, target: PaneTarget) {
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .resolve_pane(&rmux_proto::Target::Pane(target.clone()))
            .expect("exit target exists")
            .id()
    };
    handler
        .handle_ok(RespawnPaneRequest {
            process_command: Some(exiting_process()),
            ..Fixture::fixture(target)
        })
        .await;

    wait_until(EXIT_TIMEOUT, Duration::from_millis(20), async || {
        let state = handler.state.lock().await;
        let present = state.sessions.iter().any(|(_, session)| {
            session
                .windows()
                .values()
                .any(|window| window.panes().iter().any(|pane| pane.id() == pane_id))
        });
        if present {
            Err(())
        } else {
            Ok(())
        }
    })
    .await
    .unwrap_or_else(|()| panic!("real exited process did not remove pane {pane_id}"));
}

async fn exit_and_wait_dead(
    handler: &RequestHandler,
    session_name: &SessionName,
    target: PaneTarget,
) {
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .resolve_pane(&rmux_proto::Target::Pane(target.clone()))
            .expect("exit target exists")
            .id()
    };
    handler
        .handle_ok(RespawnPaneRequest {
            process_command: Some(exiting_process()),
            ..Fixture::fixture(target)
        })
        .await;

    wait_until(EXIT_TIMEOUT, Duration::from_millis(20), async || {
        let state = handler.state.lock().await;
        if state.pane_is_dead(session_name, pane_id) {
            Ok(())
        } else {
            Err(())
        }
    })
    .await
    .unwrap_or_else(|()| panic!("real exited process did not become kept-dead pane {pane_id}"));
}

fn relevant_notification(event: ControlServerEvent) -> Option<String> {
    let ControlServerEvent::Notification(line) = event else {
        return None;
    };
    [
        "%session-window-changed ",
        "%window-pane-changed ",
        "%unlinked-window-close ",
        "%sessions-changed",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    .then_some(line)
}

fn drain_relevant_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Some(line) = relevant_notification(event) {
            lines.push(line);
        }
    }
    lines
}

async fn wait_for_relevant_notifications(
    rx: &mut mpsc::Receiver<ControlServerEvent>,
    minimum: usize,
) -> Vec<String> {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    let mut lines = Vec::new();
    while lines.len() < minimum {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out after receiving notifications {lines:?}"
        );
        let event = timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for control notification")
            .expect("control notification channel stays open");
        if let Some(line) = relevant_notification(event) {
            lines.push(line);
        }
    }
    sleep(Duration::from_millis(50)).await;
    lines.extend(drain_relevant_notifications(rx));
    lines
}

async fn assert_selection_silent(rx: &mut mpsc::Receiver<ControlServerEvent>) {
    sleep(Duration::from_millis(100)).await;
    let lines = drain_relevant_notifications(rx);
    assert!(
        !lines.iter().any(|line| {
            line.starts_with("%session-window-changed ")
                || line.starts_with("%window-pane-changed ")
        }),
        "unchanged selection emitted notifications: {lines:?}"
    );
}

#[tokio::test]
async fn natural_exit_active_pane_publishes_each_real_stable_change() {
    // tmux 3.7b, measured 2026-07-26: a natural exit that leaves its
    // window alive emits exactly one window-pane-changed for each real change.
    let scenarios = [
        SelectionScenario {
            mode: "manual",
            client: TargetClientKind::None,
            panes: 3,
            windows: 2,
            repetitions: 2,
            pid_seed: 81_000,
        },
        SelectionScenario {
            mode: "latest",
            client: TargetClientKind::Pty,
            panes: 2,
            windows: 3,
            repetitions: 1,
            pid_seed: 81_010,
        },
        SelectionScenario {
            mode: "largest",
            client: TargetClientKind::Control,
            panes: 3,
            windows: 3,
            repetitions: 2,
            pid_seed: 81_020,
        },
        SelectionScenario {
            mode: "smallest",
            client: TargetClientKind::None,
            panes: 2,
            windows: 2,
            repetitions: 1,
            pid_seed: 81_030,
        },
    ];

    for scenario in scenarios {
        let mut fixture = fixture(scenario).await;
        for application in 0..scenario.repetitions {
            let before = active_selection(&fixture.handler, &fixture.target).await;
            set_remain_on_exit(&fixture.handler, before.target.clone(), false).await;
            exit_and_wait_removed(&fixture.handler, before.target).await;
            let after = active_selection(&fixture.handler, &fixture.target).await;
            assert_eq!(after.session_id, before.session_id);
            assert_eq!(after.window_id, before.window_id);
            assert_ne!(after.pane_id, before.pane_id);
            assert_eq!(
                wait_for_relevant_notifications(&mut fixture.observer_rx, 1).await,
                vec![format!(
                    "%window-pane-changed {} {}",
                    after.window_id, after.pane_id
                )],
                "{scenario:?}, application {application}"
            );
        }
    }
}

#[tokio::test]
async fn natural_exit_active_last_pane_publishes_window_before_close() {
    // tmux 3.7b, measured 2026-07-26: when the active window disappears but
    // the session survives, session-window-changed precedes the close.
    let scenarios = [
        SelectionScenario {
            mode: "manual",
            client: TargetClientKind::None,
            panes: 1,
            windows: 3,
            repetitions: 2,
            pid_seed: 82_000,
        },
        SelectionScenario {
            mode: "latest",
            client: TargetClientKind::Pty,
            panes: 1,
            windows: 2,
            repetitions: 1,
            pid_seed: 82_010,
        },
        SelectionScenario {
            mode: "largest",
            client: TargetClientKind::Control,
            panes: 1,
            windows: 3,
            repetitions: 2,
            pid_seed: 82_020,
        },
        SelectionScenario {
            mode: "smallest",
            client: TargetClientKind::None,
            panes: 1,
            windows: 2,
            repetitions: 1,
            pid_seed: 82_030,
        },
    ];

    for scenario in scenarios {
        let mut fixture = fixture(scenario).await;
        for application in 0..scenario.repetitions {
            let before = active_selection(&fixture.handler, &fixture.target).await;
            set_remain_on_exit(&fixture.handler, before.target.clone(), false).await;
            exit_and_wait_removed(&fixture.handler, before.target).await;
            let after = active_selection(&fixture.handler, &fixture.target).await;
            assert_eq!(after.session_id, before.session_id);
            assert_ne!(after.window_id, before.window_id);
            assert_eq!(
                wait_for_relevant_notifications(&mut fixture.observer_rx, 2).await,
                vec![
                    format!(
                        "%session-window-changed {} {}",
                        after.session_id, after.window_id
                    ),
                    format!("%unlinked-window-close {}", before.window_id),
                ],
                "{scenario:?}, application {application}"
            );
        }
    }
}

#[tokio::test]
async fn natural_exit_inactive_and_kept_dead_selections_stay_silent() {
    let mut inactive_pane = fixture(SelectionScenario {
        mode: "manual",
        client: TargetClientKind::None,
        panes: 3,
        windows: 2,
        repetitions: 1,
        pid_seed: 83_000,
    })
    .await;
    let before = active_selection(&inactive_pane.handler, &inactive_pane.target).await;
    let (target, _) = inactive_pane_target(&inactive_pane.handler, &inactive_pane.target).await;
    set_remain_on_exit(&inactive_pane.handler, target.clone(), false).await;
    exit_and_wait_removed(&inactive_pane.handler, target).await;
    let after = active_selection(&inactive_pane.handler, &inactive_pane.target).await;
    assert_eq!(
        (after.session_id, after.window_id, after.pane_id),
        (before.session_id, before.window_id, before.pane_id)
    );
    assert_selection_silent(&mut inactive_pane.observer_rx).await;

    let mut inactive_window = fixture(SelectionScenario {
        mode: "latest",
        client: TargetClientKind::Pty,
        panes: 2,
        windows: 3,
        repetitions: 1,
        pid_seed: 83_010,
    })
    .await;
    let before = active_selection(&inactive_window.handler, &inactive_window.target).await;
    let (target, removed_window_id) =
        inactive_window_pane_target(&inactive_window.handler, &inactive_window.target).await;
    set_remain_on_exit(&inactive_window.handler, target.clone(), false).await;
    exit_and_wait_removed(&inactive_window.handler, target).await;
    let after = active_selection(&inactive_window.handler, &inactive_window.target).await;
    assert_eq!(
        (after.session_id, after.window_id, after.pane_id),
        (before.session_id, before.window_id, before.pane_id)
    );
    assert_eq!(
        wait_for_relevant_notifications(&mut inactive_window.observer_rx, 1).await,
        vec![format!("%unlinked-window-close {removed_window_id}")]
    );

    let mut kept_dead = fixture(SelectionScenario {
        mode: "largest",
        client: TargetClientKind::Control,
        panes: 2,
        windows: 3,
        repetitions: 1,
        pid_seed: 83_020,
    })
    .await;
    let before = active_selection(&kept_dead.handler, &kept_dead.target).await;
    set_remain_on_exit(&kept_dead.handler, before.target.clone(), true).await;
    exit_and_wait_dead(&kept_dead.handler, &kept_dead.target, before.target).await;
    let after = active_selection(&kept_dead.handler, &kept_dead.target).await;
    assert_eq!(
        (after.session_id, after.window_id, after.pane_id),
        (before.session_id, before.window_id, before.pane_id)
    );
    assert_selection_silent(&mut kept_dead.observer_rx).await;
}

#[tokio::test]
async fn natural_exit_final_session_has_no_replacement_selection() {
    let mut fixture = fixture(SelectionScenario {
        mode: "smallest",
        client: TargetClientKind::Control,
        panes: 1,
        windows: 1,
        repetitions: 1,
        pid_seed: 84_000,
    })
    .await;
    let before = active_selection(&fixture.handler, &fixture.target).await;
    set_remain_on_exit(&fixture.handler, before.target.clone(), false).await;
    exit_and_wait_removed(&fixture.handler, before.target).await;
    assert!(
        fixture
            .handler
            .state
            .lock()
            .await
            .sessions
            .session(&fixture.target)
            .is_none(),
        "final natural exit removes the target session"
    );
    assert_eq!(
        wait_for_relevant_notifications(&mut fixture.observer_rx, 2).await,
        vec![
            format!("%unlinked-window-close {}", before.window_id),
            "%sessions-changed".to_owned(),
        ]
    );
}
