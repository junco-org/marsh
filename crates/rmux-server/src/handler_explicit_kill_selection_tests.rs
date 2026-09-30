use std::collections::BTreeSet;

use rmux_core::PaneId;
use rmux_proto::{
    KillPaneRequest, LinkWindowRequest, NewWindowRequest, OptionName, PaneKillRequest, PaneTarget,
    PaneTargetRef, Request, Response, ScopeSelector, SelectWindowRequest, SessionName,
    SplitWindowRequest, WindowTarget,
};
use tokio::sync::mpsc;

use super::RequestHandler;
use crate::control::ControlServerEvent;
use crate::pane_io::AttachControl;
use crate::test_fixtures::{Fixture, Grouped, SessionSpec, TestRequest};

#[path = "handler_explicit_kill_selection_tests/entry_paths.rs"]
mod entry_paths;

const OBSERVER_PID: u32 = u32::MAX - 82_000;
const TARGET_PID: u32 = u32::MAX - 82_500;

#[derive(Clone, Copy, Debug)]
enum TargetClientKind {
    None,
    Pty,
    Control,
}

#[derive(Clone, Copy, Debug)]
enum KillSurface {
    Indexed,
    StableId,
}

enum TargetClientGuard {
    None,
    Pty {
        _events: mpsc::UnboundedReceiver<AttachControl>,
    },
    Control {
        _events: mpsc::Receiver<ControlServerEvent>,
    },
}

struct Scenario {
    handler: RequestHandler,
    target: SessionName,
    observer_pid: u32,
    observer_control_id: u64,
    observer_events: mpsc::Receiver<ControlServerEvent>,
    _target_client: TargetClientGuard,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionConsumer {
    session_id: String,
    active_window_id: String,
    window_ids: BTreeSet<String>,
}

struct OracleRemoval {
    target: PaneTarget,
    pane_id: PaneId,
    killed_window_id: String,
    expected_window_id: String,
    expected_window_ids: BTreeSet<String>,
    consumer: SelectionConsumer,
}

use crate::test_names::session_name;

/// Creates `name`; the `entry_paths` scenarios share it.
async fn new_session(handler: &RequestHandler, name: &SessionName) {
    SessionSpec::create(handler, name).await;
}

async fn set_window_size_mode(handler: &RequestHandler, session_name: &SessionName, mode: &str) {
    let indexes = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(session_name)
            .expect("target session exists")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>()
    };
    for index in indexes {
        handler
            .set_window_size_policy(session_name, index, mode)
            .await;
    }
}

async fn register_target_client(
    handler: &RequestHandler,
    target: &SessionName,
    client_kind: TargetClientKind,
    pid: u32,
) -> TargetClientGuard {
    match client_kind {
        TargetClientKind::None => TargetClientGuard::None,
        TargetClientKind::Pty => TargetClientGuard::Pty {
            _events: handler.attach_client(pid, target).await,
        },
        TargetClientKind::Control => {
            let (_, events) = handler.register_control_for_test(pid, Some(target)).await;
            TargetClientGuard::Control { _events: events }
        }
    }
}

async fn build_scenario(
    client_kind: TargetClientKind,
    window_count: usize,
    window_size_mode: &str,
    pid_offset: u32,
) -> Scenario {
    let handler = RequestHandler::new();
    let observer = session_name(&format!("observer-{pid_offset}"));
    let target = session_name(&format!("target-{pid_offset}"));
    SessionSpec::create(&handler, &observer).await;
    SessionSpec::create(&handler, &target).await;
    for _ in 1..window_count {
        handler.create_window(&target).await;
    }
    set_window_size_mode(&handler, &target, window_size_mode).await;
    let target_client =
        register_target_client(&handler, &target, client_kind, TARGET_PID - pid_offset).await;
    let observer_pid = OBSERVER_PID - pid_offset;
    let (observer_control_id, mut observer_events) = handler
        .register_control_for_test(observer_pid, Some(&observer))
        .await;
    let _ = relevant_notifications(&mut observer_events);
    Scenario {
        handler,
        target,
        observer_pid,
        observer_control_id,
        observer_events,
        _target_client: target_client,
    }
}

fn relevant_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(event) = rx.try_recv() {
        let ControlServerEvent::Notification(line) = event else {
            continue;
        };
        if [
            "%session-window-changed ",
            "%window-pane-changed ",
            "%unlinked-window-close ",
            "%sessions-changed",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            lines.push(line);
        }
    }
    lines
}

async fn capture_oracle_removal(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> OracleRemoval {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("target session exists");
    let active_index = session.active_window_index();
    let indexes = session.windows().keys().copied().collect::<Vec<_>>();
    assert!(indexes.len() > 1, "oracle removal requires a survivor");
    assert_eq!(
        session.last_window_index(),
        None,
        "this matrix measures the no-history oracle branch"
    );
    let expected_index = indexes
        .iter()
        .rev()
        .copied()
        .find(|index| *index < active_index)
        .or_else(|| {
            indexes
                .iter()
                .rev()
                .copied()
                .find(|index| *index > active_index)
        })
        .expect("oracle cyclic predecessor exists");
    let killed_window = session
        .window_at(active_index)
        .expect("active window exists");
    let killed_pane = killed_window.active_pane().expect("active pane exists");
    let expected_window = session
        .window_at(expected_index)
        .expect("oracle-selected window exists before mutation");
    let session_id = session.id().to_string();
    let killed_window_id = killed_window.id().to_string();
    let expected_window_id = expected_window.id().to_string();
    let window_ids = session
        .windows()
        .values()
        .map(|window| window.id().to_string())
        .collect::<BTreeSet<_>>();
    let expected_window_ids = window_ids
        .iter()
        .filter(|window_id| *window_id != &killed_window_id)
        .cloned()
        .collect();
    OracleRemoval {
        target: PaneTarget::with_window(session_name.clone(), active_index, killed_pane.index()),
        pane_id: killed_pane.id(),
        killed_window_id,
        expected_window_id: expected_window_id.clone(),
        expected_window_ids,
        consumer: SelectionConsumer {
            session_id,
            active_window_id: killed_window.id().to_string(),
            window_ids,
        },
    }
}

async fn invoke_surface(
    handler: &RequestHandler,
    session_name: &SessionName,
    oracle: &OracleRemoval,
    surface: KillSurface,
) {
    let response = match surface {
        KillSurface::Indexed => {
            handler
                .handle(Request::KillPane(KillPaneRequest {
                    target: oracle.target.clone(),
                    kill_all_except: false,
                }))
                .await
        }
        KillSurface::StableId => {
            handler
                .handle(Request::PaneKill(PaneKillRequest {
                    target: PaneTargetRef::by_id(session_name.clone(), oracle.pane_id),
                    kill_all_except: false,
                }))
                .await
        }
    };
    let Response::KillPane(response) = response else {
        panic!("{surface:?} failed: {response:?}");
    };
    assert!(response.window_destroyed, "{surface:?}: {response:?}");
}

fn apply_selection_events(consumer: &mut SelectionConsumer, lines: &[String]) {
    for line in lines {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("%session-window-changed") => {
                assert_eq!(
                    fields.next(),
                    Some(consumer.session_id.as_str()),
                    "selection event changed the snapshotted session identity"
                );
                consumer.active_window_id = fields
                    .next()
                    .expect("selection event has a window identity")
                    .to_owned();
            }
            Some("%unlinked-window-close") => {
                let closed = fields.next().expect("close event has a window identity");
                assert_ne!(
                    consumer.active_window_id, closed,
                    "consumer observed active-window deletion before replacement selection"
                );
                assert!(
                    consumer.window_ids.remove(closed),
                    "consumer observed an unknown or duplicate window close: {line}"
                );
            }
            Some("%sessions-changed" | "%window-pane-changed") => {}
            Some(other) => panic!("unexpected relevant notification {other}: {line}"),
            None => panic!("empty control notification"),
        }
    }
}

async fn assert_oracle_transition(scenario: &mut Scenario, oracle: OracleRemoval, label: &str) {
    let expected = vec![
        format!(
            "%session-window-changed {} {}",
            oracle.consumer.session_id, oracle.expected_window_id
        ),
        format!("%unlinked-window-close {}", oracle.killed_window_id),
    ];
    let actual = relevant_notifications(&mut scenario.observer_events);
    assert_eq!(actual, expected, "{label}");

    let mut consumer = oracle.consumer;
    apply_selection_events(&mut consumer, &actual);
    let actual_snapshot = {
        let state = scenario.handler.state.lock().await;
        let session = state
            .sessions
            .session(&scenario.target)
            .expect("target session survives");
        SelectionConsumer {
            session_id: session.id().to_string(),
            active_window_id: session.window().id().to_string(),
            window_ids: session
                .windows()
                .values()
                .map(|window| window.id().to_string())
                .collect(),
        }
    };
    assert_eq!(
        consumer, actual_snapshot,
        "{label}: snapshot plus events must converge without a rescan"
    );
    assert_eq!(
        actual_snapshot.active_window_id, oracle.expected_window_id,
        "{label}: final stable identity differs from the tmux oracle"
    );
    assert_eq!(
        actual_snapshot.window_ids, oracle.expected_window_ids,
        "{label}: an unrelated window identity changed"
    );
}

async fn assert_explicit_kill_surface_matrix(surface: KillSurface, mut pid_offset: u32) {
    // tmux 3.7b matrix, measured 2026-07-27 before this test: selection is
    // last-window when usable, otherwise cyclic predecessor by original
    // index. Every stable identity below is captured before the mutation.
    for client_kind in [
        TargetClientKind::None,
        TargetClientKind::Pty,
        TargetClientKind::Control,
    ] {
        for window_size_mode in ["manual", "latest", "largest", "smallest"] {
            pid_offset += 1;
            let mut scenario = build_scenario(client_kind, 4, window_size_mode, pid_offset).await;
            for application in ["first", "repeat"] {
                let oracle = capture_oracle_removal(&scenario.handler, &scenario.target).await;
                invoke_surface(&scenario.handler, &scenario.target, &oracle, surface).await;
                assert_oracle_transition(
                    &mut scenario,
                    oracle,
                    &format!(
                        "surface={surface:?}, client={client_kind:?}, \
                             window-size={window_size_mode}, application={application}"
                    ),
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn indexed_kill_pane_publishes_oracle_fallback_before_close_matrix() {
    assert_explicit_kill_surface_matrix(KillSurface::Indexed, 0).await;
}

#[tokio::test]
async fn stable_id_pane_kill_publishes_oracle_fallback_before_close_matrix() {
    assert_explicit_kill_surface_matrix(KillSurface::StableId, 100).await;
}

#[tokio::test]
async fn explicit_kill_selection_neighbours_remain_silent_when_identity_cannot_change() {
    for surface in [KillSurface::Indexed, KillSurface::StableId] {
        let mut inactive = build_scenario(TargetClientKind::None, 3, "manual", 700).await;
        let (target, pane_id, killed_window_id, active_window_id) = {
            let state = inactive.handler.state.lock().await;
            let session = state
                .sessions
                .session(&inactive.target)
                .expect("inactive target session exists");
            let window = session.window_at(1).expect("inactive window exists");
            let pane = window.active_pane().expect("inactive pane exists");
            (
                PaneTarget::with_window(inactive.target.clone(), 1, pane.index()),
                pane.id(),
                window.id().to_string(),
                session.window().id().to_string(),
            )
        };
        let oracle = OracleRemoval {
            target,
            pane_id,
            killed_window_id: killed_window_id.clone(),
            expected_window_id: active_window_id,
            expected_window_ids: BTreeSet::new(),
            consumer: SelectionConsumer {
                session_id: String::new(),
                active_window_id: String::new(),
                window_ids: BTreeSet::new(),
            },
        };
        invoke_surface(&inactive.handler, &inactive.target, &oracle, surface).await;
        assert_eq!(
            relevant_notifications(&mut inactive.observer_events),
            vec![format!("%unlinked-window-close {killed_window_id}")],
            "{surface:?} emitted a false inactive selection"
        );

        let mut final_session = build_scenario(TargetClientKind::Control, 1, "smallest", 710).await;
        let (target, pane_id, window_id) = {
            let state = final_session.handler.state.lock().await;
            let session = state
                .sessions
                .session(&final_session.target)
                .expect("final target session exists");
            let window = session.window();
            let pane = window.active_pane().expect("final pane exists");
            (
                PaneTarget::with_window(final_session.target.clone(), 0, pane.index()),
                pane.id(),
                window.id().to_string(),
            )
        };
        let response = match surface {
            KillSurface::Indexed => {
                final_session
                    .handler
                    .handle(Request::KillPane(KillPaneRequest {
                        target,
                        kill_all_except: false,
                    }))
                    .await
            }
            KillSurface::StableId => {
                final_session
                    .handler
                    .handle(Request::PaneKill(PaneKillRequest {
                        target: PaneTargetRef::by_id(final_session.target.clone(), pane_id),
                        kill_all_except: false,
                    }))
                    .await
            }
        };
        assert!(matches!(response, Response::KillPane(_)), "{response:?}");
        assert_eq!(
            relevant_notifications(&mut final_session.observer_events),
            vec![
                format!("%unlinked-window-close {window_id}"),
                "%sessions-changed".to_owned(),
            ],
            "{surface:?} announced a replacement for a destroyed session"
        );
    }
}

async fn create_linked_family(handler: &RequestHandler) -> (SessionName, SessionName) {
    let alpha = session_name("linked-alpha");
    let beta = session_name("linked-beta");
    for session in [&alpha, &beta] {
        SessionSpec::create(handler, session).await;
        handler.create_window(session).await;
        handler.create_window(session).await;
    }
    TestRequest::send_ok(
        handler,
        LinkWindowRequest {
            kill_destination: true,
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 0),
            ))
        },
    )
    .await;
    (alpha, beta)
}

async fn expected_selection(
    handler: &RequestHandler,
    session_name: &SessionName,
    oracle_index: u32,
) -> (String, String) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("oracle session exists before mutation");
    let expected_window_id = session
        .window_at(oracle_index)
        .expect("tmux-selected slot exists before mutation")
        .id()
        .to_string();
    (session.id().to_string(), expected_window_id)
}

#[tokio::test]
async fn linked_and_grouped_last_pane_kill_interleave_each_selection_before_its_close() {
    for renumber in [false, true] {
        let handler = RequestHandler::new();
        let observer = session_name(&format!("linked-observer-{renumber}"));
        SessionSpec::create(&handler, &observer).await;
        let (alpha, beta) = create_linked_family(&handler).await;
        let renumber_windows = if renumber { "on" } else { "off" };
        for session in [&alpha, &beta] {
            handler
                .set_option(
                    ScopeSelector::Session(session.clone()),
                    OptionName::RenumberWindows,
                    renumber_windows,
                )
                .await;
        }
        // tmux 3.7b selects index 2 for both active aliases in this
        // no-history, three-window setup.
        let (alpha_session_id, alpha_expected_id) = expected_selection(&handler, &alpha, 2).await;
        let (beta_session_id, beta_expected_id) = expected_selection(&handler, &beta, 2).await;
        let (shared_id, pane_index) = {
            let state = handler.state.lock().await;
            let window = state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(0))
                .expect("shared window exists");
            (
                window.id().to_string(),
                window.active_pane().expect("shared pane exists").index(),
            )
        };
        let (_, mut events) = handler
            .register_control_for_test(83_000 + renumber as u32, Some(&observer))
            .await;
        let _ = relevant_notifications(&mut events);
        TestRequest::send_ok(
            &handler,
            KillPaneRequest {
                target: PaneTarget::with_window(alpha, 0, pane_index),
                kill_all_except: false,
            },
        )
        .await;
        assert_eq!(
            relevant_notifications(&mut events),
            vec![
                format!("%session-window-changed {alpha_session_id} {alpha_expected_id}"),
                format!("%unlinked-window-close {shared_id}"),
                format!("%session-window-changed {beta_session_id} {beta_expected_id}"),
                format!("%unlinked-window-close {shared_id}"),
            ],
            "linked renumber={renumber}"
        );
    }

    let handler = RequestHandler::new();
    let observer = session_name("grouped-observer");
    let owner = session_name("grouped-owner");
    SessionSpec::create(&handler, &observer).await;
    handler
        .set_option(ScopeSelector::Global, OptionName::BaseIndex, "2")
        .await;
    SessionSpec::create(&handler, &owner).await;
    for index in [0, 1] {
        handler
            .create_window(NewWindowRequest {
                target_window_index: Some(index),
                ..Fixture::fixture(&owner)
            })
            .await;
    }
    let peer = SessionSpec::create(&handler, Grouped("grouped-peer", &owner)).await;
    TestRequest::send_ok(
        &handler,
        SelectWindowRequest {
            target: WindowTarget::with_window(peer.clone(), 2),
        },
    )
    .await;
    // Fresh tmux 3.7b causal matrix: a group peer created while the
    // no-history owner is on index 2 starts on its own index 0. Its single
    // selection of index 2 therefore records 0 as the local last window.
    // Removing index 2 sends the owner to 1 and the peer back to 0.
    let (owner_session_id, owner_expected_id) = expected_selection(&handler, &owner, 1).await;
    let (peer_session_id, peer_expected_id) = expected_selection(&handler, &peer, 0).await;
    let (killed_window_id, pane_index) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(2))
            .expect("grouped window exists");
        (
            window.id().to_string(),
            window.active_pane().expect("grouped pane exists").index(),
        )
    };
    let (_, mut events) = handler
        .register_control_for_test(83_100, Some(&observer))
        .await;
    let _ = relevant_notifications(&mut events);
    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(owner, 2, pane_index),
            kill_all_except: false,
        },
    )
    .await;
    assert_eq!(
        relevant_notifications(&mut events),
        vec![
            format!("%session-window-changed {owner_session_id} {owner_expected_id}"),
            format!("%unlinked-window-close {killed_window_id}"),
            format!("%session-window-changed {peer_session_id} {peer_expected_id}"),
            format!("%unlinked-window-close {killed_window_id}"),
        ]
    );
}

#[tokio::test]
async fn surviving_window_kill_keeps_pane_selection_without_session_duplicate() {
    let mut scenario = build_scenario(TargetClientKind::None, 1, "manual", 800).await;
    TestRequest::send_ok(
        &scenario.handler,
        SplitWindowRequest::fixture(&scenario.target),
    )
    .await;
    let _ = relevant_notifications(&mut scenario.observer_events);
    let (window_id, target, expected_pane_id) = {
        let state = scenario.handler.state.lock().await;
        let session = state
            .sessions
            .session(&scenario.target)
            .expect("target session exists");
        let window = session.window();
        let killed_pane = window.active_pane().expect("active pane exists");
        let expected = window
            .panes()
            .iter()
            .find(|pane| pane.id() != killed_pane.id())
            .expect("fallback pane exists");
        (
            window.id().to_string(),
            PaneTarget::with_window(scenario.target.clone(), 0, killed_pane.index()),
            expected.id().to_string(),
        )
    };
    let response = TestRequest::send_ok(
        &scenario.handler,
        KillPaneRequest {
            target,
            kill_all_except: false,
        },
    )
    .await;
    assert!(!response.window_destroyed, "{response:?}");
    assert_eq!(
        relevant_notifications(&mut scenario.observer_events),
        vec![format!(
            "%window-pane-changed {window_id} {expected_pane_id}"
        )]
    );
}
