use std::collections::{BTreeMap, HashMap, HashSet};

use super::*;
use crate::control::ControlServerEvent;
use crate::test_fixtures::TestRequest;

#[derive(Debug)]
struct StableSessionSelection {
    session_name: SessionName,
    session_id: String,
    window_id: String,
}

async fn run_swap_control(
    handler: &RequestHandler,
    requester_pid: u32,
    control_id: u64,
    command: &str,
) {
    let commands = handler
        .parse_control_commands(command)
        .await
        .expect("swap control command parses");
    let result = handler
        .execute_control_commands_identity(requester_pid, control_id, commands)
        .await;
    assert!(result.error.is_none(), "{command}: {:?}", result.error);
}

fn swap_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(event) = rx.try_recv() {
        let ControlServerEvent::Notification(line) = event else {
            continue;
        };
        if line.starts_with("%session-window-changed ") || line.starts_with("%layout-change ") {
            lines.push(line);
        }
    }
    lines
}

async fn create_indexed_windows(handler: &RequestHandler, name: &str, windows: u32) -> SessionName {
    let session_name = create_session(handler, name).await;
    for window_index in 1..windows {
        insert_window(handler, &session_name, window_index).await;
    }
    session_name
}

async fn select_window(handler: &RequestHandler, session_name: &SessionName, window_index: u32) {
    let mut state = handler.state.lock().await;
    state
        .sessions
        .session_mut(session_name)
        .expect("swap test session exists")
        .select_window(window_index)
        .expect("swap test window selection succeeds");
}

async fn session_window_ids(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> (String, BTreeMap<u32, String>) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("swap test session exists");
    (
        session.id().to_string(),
        session
            .windows()
            .iter()
            .map(|(index, window)| (*index, window.id().to_string()))
            .collect(),
    )
}

async fn active_window_id(handler: &RequestHandler, session_name: &SessionName) -> String {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session_name)
        .expect("swap test session exists")
        .window()
        .id()
        .to_string()
}

/// Each session's stable id mapped to its active window's stable id.
fn selection_model(selections: &[StableSessionSelection]) -> HashMap<String, String> {
    selections
        .iter()
        .map(|selection| (selection.session_id.clone(), selection.window_id.clone()))
        .collect()
}

async fn active_window_model(
    handler: &RequestHandler,
    session_names: &[SessionName],
) -> HashMap<String, String> {
    selection_model(&stable_selection_snapshot(handler, session_names).await)
}

async fn stable_selection_snapshot(
    handler: &RequestHandler,
    session_names: &[SessionName],
) -> Vec<StableSessionSelection> {
    let state = handler.state.lock().await;
    session_names
        .iter()
        .map(|session_name| {
            let session = state
                .sessions
                .session(session_name)
                .expect("snapshot session exists");
            StableSessionSelection {
                session_name: session_name.clone(),
                session_id: session.id().to_string(),
                window_id: session.window().id().to_string(),
            }
        })
        .collect()
}

fn semantic_notifications_from_snapshots(
    before: &[StableSessionSelection],
    after: &[StableSessionSelection],
    target_then_source_family: &[SessionName],
) -> Vec<String> {
    let mut seen = HashSet::new();
    target_then_source_family
        .iter()
        .filter_map(|session_name| {
            let before = before
                .iter()
                .find(|selection| &selection.session_name == session_name)
                .expect("ordered session exists in before snapshot");
            let after = after
                .iter()
                .find(|selection| &selection.session_name == session_name)
                .expect("ordered session exists in after snapshot");
            assert_eq!(
                before.session_id, after.session_id,
                "session identity remains stable"
            );
            if before.window_id == after.window_id || !seen.insert(after.session_id.clone()) {
                return None;
            }
            Some(format!(
                "%session-window-changed {} {}",
                after.session_id, after.window_id
            ))
        })
        .collect()
}

fn apply_session_window_events(model: &mut HashMap<String, String>, notifications: &[String]) {
    for line in notifications {
        if !line.starts_with("%session-window-changed ") {
            continue;
        }
        let mut fields = line.split_whitespace();
        assert_eq!(fields.next(), Some("%session-window-changed"));
        let session_id = fields.next().expect("event has session id");
        let window_id = fields.next().expect("event has window id");
        assert!(fields.next().is_none(), "unexpected event fields: {line}");
        model.insert(session_id.to_owned(), window_id.to_owned());
    }
}

#[tokio::test]
async fn swap_window_intra_session_publishes_only_real_identity_changes() {
    // tmux 3.7b was measured before this assertion. RMUX intentionally
    // normalizes the oracle's redundant and missing notifications to exactly
    // one event per real stable-identity transition.
    // (name, windows, active, source, target, detached, expected identity slot)
    let cases: [(&str, u32, u32, u32, u32, bool, u32); 10] = [
        ("inactive-d", 3, 2, 0, 1, true, 0),
        ("inactive-default", 3, 2, 0, 1, false, 2),
        ("inverse-d", 3, 2, 1, 0, true, 1),
        ("same-target-d", 3, 2, 0, 0, true, 2),
        ("source-active-d", 3, 0, 0, 1, true, 0),
        ("target-active-d", 3, 1, 0, 1, true, 0),
        ("source-active-default", 3, 0, 0, 1, false, 1),
        ("target-active-default", 3, 1, 0, 1, false, 0),
        ("two-source-active-d", 2, 0, 0, 1, true, 0),
        ("two-target-active-d", 2, 1, 0, 1, true, 0),
    ];

    for (offset, case) in cases.into_iter().enumerate() {
        let (name, windows, active, source, target, detached, expected_identity_slot) = case;
        let handler = RequestHandler::new();
        let session_name = create_indexed_windows(&handler, name, windows).await;
        select_window(&handler, &session_name, active).await;
        let (session_id, initial_ids) = session_window_ids(&handler, &session_name).await;
        let before = active_window_id(&handler, &session_name).await;
        let expected_after = initial_ids
            .get(&expected_identity_slot)
            .expect("expected identity slot exists")
            .clone();
        let (_control_id, mut rx) = handler
            .register_control_for_test(32_000 + offset as u32, Some(&session_name))
            .await;
        let _ = swap_notifications(&mut rx);

        let response = handler
            .handle(Request::SwapWindow(SwapWindowRequest {
                source: WindowTarget::with_window(session_name.clone(), source),
                target: WindowTarget::with_window(session_name.clone(), target),
                detached,
            }))
            .await;
        assert!(
            matches!(response, Response::SwapWindow(_)),
            "{name}: {response:?}"
        );

        let notifications = swap_notifications(&mut rx);
        let expected_notifications = if before == expected_after {
            Vec::new()
        } else {
            vec![format!(
                "%session-window-changed {session_id} {expected_after}"
            )]
        };
        assert_eq!(
            notifications, expected_notifications,
            "{name} must publish exactly its real identity transition"
        );
        assert_eq!(
            active_window_id(&handler, &session_name).await,
            expected_after,
            "{name} active identity"
        );
    }
}

#[tokio::test]
async fn swap_window_repetition_keeps_snapshot_event_model_exact_without_rescan() {
    let handler = RequestHandler::new();
    let alpha = create_indexed_windows(&handler, "repeat-alpha", 3).await;
    select_window(&handler, &alpha, 2).await;
    let (session_id, initial_ids) = session_window_ids(&handler, &alpha).await;
    let requester_pid = 32_100;
    let (control_id, mut rx) = handler
        .register_control_for_test(requester_pid, Some(&alpha))
        .await;
    let _ = swap_notifications(&mut rx);
    let mut model = active_window_model(&handler, std::slice::from_ref(&alpha)).await;

    for expected_slot in [0, 1] {
        run_swap_control(
            &handler,
            requester_pid,
            control_id,
            "swap-window -d -s repeat-alpha:0 -t repeat-alpha:1",
        )
        .await;
        let expected_window_id = initial_ids
            .get(&expected_slot)
            .expect("repetition identity exists");
        let notifications = swap_notifications(&mut rx);
        assert_eq!(
            notifications,
            vec![format!(
                "%session-window-changed {session_id} {expected_window_id}"
            )]
        );
        apply_session_window_events(&mut model, &notifications);
        assert_eq!(
            model.get(&session_id),
            Some(expected_window_id),
            "snapshot plus events predicts the repeated transition"
        );
        assert_eq!(
            active_window_id(&handler, &alpha).await,
            *expected_window_id,
            "final query validates but does not update the model"
        );
    }
}

#[tokio::test]
async fn cross_session_swap_orders_each_real_transition_target_then_source() {
    let handler = RequestHandler::new();
    let alpha = create_indexed_windows(&handler, "cross-alpha", 2).await;
    let beta = create_indexed_windows(&handler, "cross-beta", 2).await;
    select_window(&handler, &alpha, 1).await;
    select_window(&handler, &beta, 1).await;
    let (alpha_id, alpha_windows) = session_window_ids(&handler, &alpha).await;
    let (beta_id, beta_windows) = session_window_ids(&handler, &beta).await;
    let (_control_id, mut rx) = handler
        .register_control_for_test(32_200, Some(&alpha))
        .await;
    let _ = swap_notifications(&mut rx);
    let sessions = [alpha.clone(), beta.clone()];
    let mut model = active_window_model(&handler, &sessions).await;

    TestRequest::send_ok(
        &handler,
        SwapWindowRequest {
            source: WindowTarget::with_window(alpha.clone(), 0),
            target: WindowTarget::with_window(beta.clone(), 0),
            detached: true,
        },
    )
    .await;

    let source_window_id = alpha_windows.get(&0).expect("source identity");
    let target_window_id = beta_windows.get(&0).expect("target identity");
    let notifications = swap_notifications(&mut rx);
    assert_eq!(
        notifications,
        vec![
            format!("%session-window-changed {beta_id} {source_window_id}"),
            format!("%session-window-changed {alpha_id} {target_window_id}"),
        ],
        "tmux 3.7b orders cross-session selection target then source"
    );
    apply_session_window_events(&mut model, &notifications);
    assert_eq!(model, active_window_model(&handler, &sessions).await);
}

#[tokio::test]
async fn cross_session_swap_orders_complete_grouped_families_from_stable_snapshots() {
    let handler = RequestHandler::new();
    let alpha = create_indexed_windows(&handler, "family-alpha", 3).await;
    let gamma = create_grouped_session(&handler, "family-gamma", &alpha).await;
    let beta = create_indexed_windows(&handler, "family-beta", 3).await;
    let unchanged = create_indexed_windows(&handler, "family-unchanged", 1).await;
    for session_name in [&alpha, &gamma, &beta, &unchanged] {
        select_window(&handler, session_name, 0).await;
    }

    let (_alpha_id, alpha_windows) = session_window_ids(&handler, &alpha).await;
    let (_beta_id, beta_windows) = session_window_ids(&handler, &beta).await;
    let source_window_id = alpha_windows.get(&0).expect("source identity").clone();
    let target_window_id = beta_windows.get(&0).expect("target identity").clone();
    let session_names = [
        alpha.clone(),
        gamma.clone(),
        beta.clone(),
        unchanged.clone(),
    ];
    let requester_pid = 32_250;
    let (control_id, mut rx) = handler
        .register_control_for_test(requester_pid, Some(&alpha))
        .await;
    let _ = swap_notifications(&mut rx);

    let family_orders = [
        vec![
            beta.clone(),
            alpha.clone(),
            gamma.clone(),
            beta.clone(),
            unchanged.clone(),
        ],
        vec![
            alpha.clone(),
            gamma.clone(),
            beta.clone(),
            alpha.clone(),
            unchanged.clone(),
        ],
    ];
    for (operation, family_order) in family_orders.into_iter().enumerate() {
        let before = stable_selection_snapshot(&handler, &session_names).await;
        run_swap_control(
            &handler,
            requester_pid,
            control_id,
            &format!("swap-window -s {source_window_id} -t {target_window_id}"),
        )
        .await;
        let after = stable_selection_snapshot(&handler, &session_names).await;
        let expected = semantic_notifications_from_snapshots(&before, &after, &family_order);
        let notifications = swap_notifications(&mut rx);
        assert_eq!(
            notifications,
            expected,
            "operation {} publishes the whole target family before the source family",
            operation + 1
        );

        let mut model = selection_model(&before);
        apply_session_window_events(&mut model, &notifications);
        let final_snapshot = selection_model(&after);
        assert_eq!(
            model,
            final_snapshot,
            "snapshot plus ordered events reconstructs operation {}",
            operation + 1
        );
    }
}

#[tokio::test]
async fn grouped_and_linked_peers_do_not_receive_identity_stable_noise() {
    let handler = RequestHandler::new();
    let owner = create_indexed_windows(&handler, "group-owner", 3).await;
    let peer = create_grouped_session(&handler, "group-peer", &owner).await;
    select_window(&handler, &owner, 2).await;
    select_window(&handler, &peer, 2).await;
    let (owner_id, owner_windows) = session_window_ids(&handler, &owner).await;
    let (_peer_id, peer_windows) = session_window_ids(&handler, &peer).await;
    let peer_before = peer_windows.get(&2).expect("peer active identity").clone();
    let (_control_id, mut rx) = handler
        .register_control_for_test(32_300, Some(&owner))
        .await;
    let _ = swap_notifications(&mut rx);

    TestRequest::send_ok(
        &handler,
        SwapWindowRequest {
            source: WindowTarget::with_window(owner.clone(), 0),
            target: WindowTarget::with_window(owner.clone(), 1),
            detached: true,
        },
    )
    .await;
    assert_eq!(
        swap_notifications(&mut rx),
        vec![format!(
            "%session-window-changed {owner_id} {}",
            owner_windows.get(&0).expect("owner source identity")
        )]
    );
    assert_eq!(active_window_id(&handler, &peer).await, peer_before);

    let handler = RequestHandler::new();
    let owner = create_indexed_windows(&handler, "link-owner", 3).await;
    let peer = create_indexed_windows(&handler, "link-peer", 1).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(peer.clone(), 1),
        )),
    )
    .await;
    select_window(&handler, &owner, 2).await;
    select_window(&handler, &peer, 1).await;
    let (owner_id, owner_windows) = session_window_ids(&handler, &owner).await;
    let peer_before = active_window_id(&handler, &peer).await;
    let (_control_id, mut rx) = handler
        .register_control_for_test(32_301, Some(&owner))
        .await;
    let _ = swap_notifications(&mut rx);

    TestRequest::send_ok(
        &handler,
        SwapWindowRequest {
            source: WindowTarget::with_window(owner.clone(), 0),
            target: WindowTarget::with_window(owner.clone(), 1),
            detached: true,
        },
    )
    .await;
    assert_eq!(
        swap_notifications(&mut rx),
        vec![format!(
            "%session-window-changed {owner_id} {}",
            owner_windows.get(&0).expect("owner source identity")
        )]
    );
    assert_eq!(active_window_id(&handler, &peer).await, peer_before);
}
