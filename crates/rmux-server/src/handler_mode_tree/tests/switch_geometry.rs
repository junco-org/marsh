//! `choose-tree`'s default action is `switch-client -Zt`, so it owes both
//! halves of a switch the same geometry notifications any other `switch-client`
//! owes.

use super::super::mode_tree_order::{pane_item_id, session_item_id};
use super::*;

use crate::test_fixtures::{collect_control_notifications_through, settle_control_notifications};

/// Frozen tmux 3.7b oracle, measured 2026-07-25
/// (`.rmux-audit/oracle/scenario_switch_destination.py`): a 101x41 client
/// switching onto a session whose only other client is a 60x20 control client
/// stores the 101x41 terminal size while the default one-line status leaves a
/// 101x40 content layout, and that control client receives
///     %client-session-changed <client> $1 target
///     %layout-change @1 aefe,101x40,0,0,1 aefe,101x40,0,0,1 *
/// in that order. `choose-tree` reaches `switch-client -Zt` through its own
/// action path, so it is asserted separately from the command form.
#[tokio::test]
async fn choose_tree_switch_notifies_the_destination_session_layout_change_like_tmux37() {
    let handler = RequestHandler::new();
    let source = SessionName::new("choose-tree-switch-source").expect("valid session");
    let target = SessionName::new("choose-tree-switch-target").expect("valid session");
    handler.create_session(&source).await;
    handler.create_session(&target).await;
    for session in [&source, &target] {
        handler
            .set_option(
                ScopeSelector::Session(session.clone()),
                OptionName::WindowSize,
                "largest",
            )
            .await;
    }

    let attach_pid = std::process::id().saturating_add(211);
    let control_pid = attach_pid.saturating_add(1);
    let switching_size = TerminalSize {
        cols: 101,
        rows: 41,
    };
    let destination_size = TerminalSize { cols: 60, rows: 20 };

    let _control_rx = handler.attach_client(attach_pid, &source).await;
    handler
        .declare_client_size_for_test(attach_pid, switching_size)
        .await;

    let (_, mut control_events) = handler
        .register_control_for_test(control_pid, Some(&target))
        .await;
    handler
        .handle_ok(rmux_proto::RefreshClientRequest {
            control_size: Some(format!(
                "{}x{}",
                destination_size.cols, destination_size.rows
            )),
            ..Fixture::fixture(Some(control_pid.to_string()))
        })
        .await;
    assert_eq!(
        handler.active_window_size_for_test(&target).await,
        destination_size
    );

    let target_session_id = handler.session_id_for_test(&target).await;
    let target_window_id = handler.active_window_id_for_test(&target).await;
    let layout_prefix = format!("%layout-change @{target_window_id} ");

    open_mode_tree(&handler, attach_pid, &["choose-tree"]).await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(session_item_id(target_session_id));
    })
    .await;
    settle_control_notifications(&mut control_events).await;

    handler
        .accept_mode_tree_selection(attach_pid)
        .await
        .expect("choose-tree switch-client -Zt succeeds");

    assert_eq!(
        handler.active_window_size_for_test(&target).await,
        TerminalSize {
            cols: switching_size.cols,
            rows: switching_size.rows - 1,
        },
        "choose-tree's switch must grow the destination window"
    );
    let lines = collect_control_notifications_through(&mut control_events, &layout_prefix).await;
    let layout_index = lines
        .iter()
        .position(|line| line.starts_with(&layout_prefix))
        .expect("the destination control client must be told its window grew");
    assert_eq!(
        lines[layout_index]
            .split_whitespace()
            .nth(2)
            .and_then(|layout| layout.split(',').nth(1)),
        Some("101x40"),
        "{:?}",
        lines[layout_index]
    );
    let session_changed_index = lines
        .iter()
        .position(|line| line.starts_with("%client-session-changed "))
        .expect("the destination control client must be told the client arrived");
    assert!(
        session_changed_index < layout_index,
        "tmux 3.7b reports the client move before the layout it causes: {lines:?}"
    );
}

/// `choose-tree` reaches the same switch helper as `switch-client`, so a client
/// that never declared a size must carry its outer terminal anchor — not the
/// status-subtracted content rows it was registered against — onto the
/// destination session.
#[tokio::test]
async fn choose_tree_switch_carries_a_sizeless_client_outer_terminal_anchor() {
    let handler = RequestHandler::new();
    let source = SessionName::new("choose-tree-sizeless-source").expect("valid session");
    let target = SessionName::new("choose-tree-sizeless-target").expect("valid session");
    handler.create_session(&source).await;
    handler.create_session(&target).await;
    for session in [&source, &target] {
        handler
            .set_option(
                ScopeSelector::Session(session.clone()),
                OptionName::Status,
                "2",
            )
            .await;
    }

    let declared_pid = std::process::id().saturating_add(311);
    let sizeless_pid = declared_pid.saturating_add(1);
    let _declared_rx = handler.attach_client(declared_pid, &source).await;
    handler
        .handle_attached_resize(declared_pid, TerminalSize { cols: 80, rows: 24 })
        .await
        .expect("declared terminal geometry seeds status-aware content size");
    assert_eq!(
        handler.active_window_size_for_test(&source).await,
        TerminalSize { cols: 80, rows: 22 }
    );

    let _sizeless_rx = handler.attach_client(sizeless_pid, &source).await;

    let target_session_id = handler.session_id_for_test(&target).await;
    open_mode_tree(&handler, sizeless_pid, &["choose-tree"]).await;
    with_mode_tree(&handler, sizeless_pid, |mode| {
        mode.selected_id = Some(session_item_id(target_session_id));
    })
    .await;

    handler
        .accept_mode_tree_selection(sizeless_pid)
        .await
        .expect("choose-tree switch-client -Zt succeeds");

    assert_eq!(
        session_terminal_size(&handler, &target).await,
        TerminalSize { cols: 80, rows: 24 },
        "choose-tree must carry the outer terminal anchor to the destination"
    );
    assert_eq!(
        handler.active_window_size_for_test(&target).await,
        TerminalSize { cols: 80, rows: 22 },
        "the destination subtracts its own status rows exactly once"
    );
}

#[tokio::test]
async fn choose_tree_pane_switch_keeps_the_control_selection_model_current() {
    let handler = RequestHandler::new();
    let source = SessionName::new("choose-tree-selection-source").expect("valid session");
    let target = SessionName::new("choose-tree-selection-target").expect("valid session");
    handler.create_session(&source).await;
    handler.create_session(&target).await;
    let window = handler.create_window(&target).await;
    assert_eq!(window.window_index(), 1);
    handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(target.clone(), 1, 0))
        })
        .await;

    let (
        pane_item,
        session_id,
        initial_window_id,
        target_window_id,
        initial_pane_id,
        target_pane_id,
    ) = {
        let mut state = handler.state.lock().await;
        state.ensure_live_window_link_occurrences();
        let session = state
            .sessions
            .session_mut(&target)
            .expect("target session exists");
        session
            .select_pane_in_window(1, 0)
            .expect("inactive target pane selected for setup");
        session
            .select_window(0)
            .expect("source target window selected for setup");
        let session_id = session.id();
        let initial_window_id = session.window().id();
        let target_window = session.window_at(1).expect("target window exists");
        let target_window_id = target_window.id();
        let initial_pane_id = target_window
            .active_pane()
            .expect("target window has an active pane")
            .id();
        let target_pane_id = target_window
            .pane(1)
            .expect("inactive target pane exists")
            .id();
        let occurrence_id = state
            .window_link_occurrence_id(&target, 1)
            .expect("target occurrence has a stable identity");
        (
            pane_item_id(
                session_id,
                1,
                target_window_id,
                occurrence_id,
                target_pane_id,
            ),
            session_id,
            initial_window_id,
            target_window_id,
            initial_pane_id,
            target_pane_id,
        )
    };

    let attach_pid = std::process::id().saturating_add(212);
    let control_pid = attach_pid.saturating_add(1);
    let _attach_rx = handler.attach_client(attach_pid, source).await;
    let (_, mut control_events) = handler
        .register_control_for_test(control_pid, Some(&target))
        .await;

    open_mode_tree(&handler, attach_pid, &["choose-tree"]).await;
    with_mode_tree(&handler, attach_pid, |mode| {
        mode.selected_id = Some(pane_item);
    })
    .await;
    settle_control_notifications(&mut control_events).await;

    handler
        .accept_mode_tree_selection(attach_pid)
        .await
        .expect("choose-tree pane switch succeeds");
    let lines =
        collect_control_notifications_through(&mut control_events, "%client-session-changed ")
            .await;
    let transitions = lines
        .iter()
        .filter_map(|line| {
            let event = line.split_whitespace().next()?;
            matches!(
                event,
                "%window-pane-changed" | "%session-window-changed" | "%client-session-changed"
            )
            .then_some(event)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transitions,
        vec![
            "%window-pane-changed",
            "%session-window-changed",
            "%client-session-changed",
        ]
    );

    let session_id = session_id.to_string();
    let target_window_id = target_window_id.to_string();
    let target_pane_id = target_pane_id.to_string();
    let mut predicted_window_id = initial_window_id.to_string();
    let mut predicted_pane_id = initial_pane_id.to_string();
    for line in &lines {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        match fields.first().copied() {
            Some("%session-window-changed")
                if fields.get(1).copied() == Some(session_id.as_str()) =>
            {
                predicted_window_id = fields[2].to_owned();
            }
            Some("%window-pane-changed")
                if fields.get(1).copied() == Some(target_window_id.as_str()) =>
            {
                predicted_pane_id = fields[2].to_owned();
            }
            _ => {}
        }
    }
    assert_eq!(predicted_window_id, target_window_id);
    assert_eq!(predicted_pane_id, target_pane_id);
}

async fn session_terminal_size(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> TerminalSize {
    handler
        .state
        .lock()
        .await
        .sessions
        .session(session_name)
        .expect("session remains present")
        .terminal_size()
}
