use std::collections::{BTreeMap, HashSet};

use rmux_core::SessionStore;
use rmux_proto::{PaneTarget, SessionName, TerminalSize, WindowTarget};

use super::super::window_indices::window_ids_by_index;
use super::{HandlerState, PaneTransferGeometryContext, SessionCheckpoint};

const SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };

/// Session `alpha` (plus a collateral second window when asked), its grouped alias `beta`, and
/// an unrelated `source` session.
fn geometry_state(
    alpha: &SessionName,
    beta: &SessionName,
    source: &SessionName,
    collateral: bool,
) -> HandlerState {
    let mut state = HandlerState::default();
    state
        .sessions
        .create_session(alpha.clone(), SIZE)
        .expect("create alpha");
    if collateral {
        state
            .sessions
            .session_mut(alpha)
            .expect("alpha")
            .create_window(SIZE)
            .expect("create collateral window");
    }
    state
        .sessions
        .create_grouped_session_with_base_index(beta.clone(), SIZE, 0, alpha.clone())
        .expect("create beta alias");
    state
        .sessions
        .create_session(source.clone(), SIZE)
        .expect("create source");
    state
}

/// The windows whose resizes the last join/move recorded, in publication order.
fn applied_resize_targets(state: &mut HandlerState) -> Vec<WindowTarget> {
    state
        .take_applied_window_resizes()
        .into_iter()
        .map(|resize| resize.into_parts().0)
        .collect()
}

#[test]
fn join_or_move_geometry_uses_requested_alias_not_hashmap_first() {
    let alpha = session_name("geometry-alias-alpha");
    let beta = session_name("geometry-alias-beta");
    let source = session_name("geometry-alias-source");
    let resized = TerminalSize { cols: 79, rows: 24 };
    let mut state = geometry_state(&alpha, &beta, &source, false);

    let shared_window_id = state
        .sessions
        .session(&alpha)
        .expect("alpha")
        .window_at(0)
        .expect("alpha window")
        .id();
    let hashmap_first_alias = state
        .sessions
        .iter()
        .find_map(|(session_name, session)| {
            session
                .window_at(0)
                .is_some_and(|window| window.id() == shared_window_id)
                .then(|| session_name.clone())
        })
        .expect("shared alias");
    let requested_alias = if hashmap_first_alias == alpha {
        beta.clone()
    } else {
        alpha.clone()
    };
    let source_pane = PaneTarget::with_window(source, 0, 0);
    let requested_pane = PaneTarget::with_window(requested_alias.clone(), 0, 0);

    let context = PaneTransferGeometryContext::new(&source_pane, &requested_pane);
    state.mutate_join_or_move_and_record_window_geometry_changes(context, |state| {
        for session_name in [&alpha, &beta] {
            state
                .sessions
                .session_mut(session_name)
                .expect("shared session")
                .resize_window(0, resized)
                .expect("resize shared window");
        }
    });

    assert_eq!(
        applied_resize_targets(&mut state),
        vec![WindowTarget::with_window(requested_alias, 0)],
        "a join/move notification must retain the alias explicitly named by the operation"
    );
}

#[test]
fn join_or_move_geometry_orders_source_target_then_collateral() {
    let alpha = session_name("geometry-order-alpha");
    let beta = session_name("geometry-order-beta");
    let source = session_name("geometry-order-source");
    let mut state = geometry_state(&alpha, &beta, &source, true);

    let source_pane = PaneTarget::with_window(source.clone(), 0, 0);
    let target_pane = PaneTarget::with_window(beta.clone(), 0, 0);
    let context = PaneTransferGeometryContext::new(&source_pane, &target_pane);
    state.mutate_join_or_move_and_record_window_geometry_changes(context, |state| {
        state
            .sessions
            .session_mut(&source)
            .expect("source")
            .resize_window(0, TerminalSize { cols: 79, rows: 24 })
            .expect("resize source");
        for session_name in [&alpha, &beta] {
            let session = state
                .sessions
                .session_mut(session_name)
                .expect("shared session");
            session
                .resize_window(0, TerminalSize { cols: 78, rows: 24 })
                .expect("resize target");
            session
                .resize_window(1, TerminalSize { cols: 77, rows: 24 })
                .expect("resize collateral");
        }
    });

    assert_eq!(
        applied_resize_targets(&mut state),
        vec![
            WindowTarget::with_window(source, 0),
            WindowTarget::with_window(beta.clone(), 0),
            WindowTarget::with_window(beta, 1),
        ],
        "publication order and collateral rendering context must be operation-derived"
    );
}

/// Two sessions of two windows each, with one automatically named window apiece.
fn checkpoint_state(alpha: &SessionName, beta: &SessionName) -> HandlerState {
    let mut state = HandlerState::default();
    for name in [alpha, beta] {
        state
            .sessions
            .create_session(name.clone(), SIZE)
            .expect("create session");
        let (window_index, _) = state
            .sessions
            .session_mut(name)
            .expect("created session")
            .create_window(SIZE)
            .expect("create second window");
        state
            .auto_named_windows
            .insert((name.clone(), window_index));
    }
    state
}

/// A checkpoint of both sessions as they stand in `state`.
fn checkpoint_both<'a>(
    state: &HandlerState,
    alpha: &'a SessionName,
    beta: &'a SessionName,
) -> SessionCheckpoint<'a, 2> {
    SessionCheckpoint::capture(
        state,
        [alpha, beta].map(|name| {
            (
                name,
                state.sessions.session(name).cloned().expect("session"),
            )
        }),
    )
}

fn window_ids(state: &HandlerState, name: &SessionName) -> BTreeMap<u32, u32> {
    window_ids_by_index(state.sessions.session(name).expect("session"))
}

/// Reshapes both window tables and the automatic-name membership after a capture.
fn mutate_checkpointed_state(state: &mut HandlerState, alpha: &SessionName, beta: &SessionName) {
    let _ = state
        .sessions
        .session_mut(alpha)
        .expect("alpha")
        .remove_window(1)
        .expect("remove alpha window");
    let _ = state
        .sessions
        .session_mut(beta)
        .expect("beta")
        .create_window(SIZE)
        .expect("create beta window");
    state.auto_named_windows.clear();
    state.auto_named_windows.insert((beta.clone(), 7));
}

#[test]
fn session_checkpoint_restores_sessions_then_metadata() {
    let alpha = session_name("checkpoint-restore-alpha");
    let beta = session_name("checkpoint-restore-beta");
    let mut state = checkpoint_state(&alpha, &beta);
    let alpha_windows = window_ids(&state, &alpha);
    let beta_windows = window_ids(&state, &beta);
    let auto_named_before = state.auto_named_windows.clone();
    let checkpoint = checkpoint_both(&state, &alpha, &beta);

    mutate_checkpointed_state(&mut state, &alpha, &beta);
    assert_ne!(window_ids(&state, &alpha), alpha_windows);
    assert_ne!(window_ids(&state, &beta), beta_windows);
    assert_ne!(state.auto_named_windows, auto_named_before);

    checkpoint.restore(&mut state).expect("restore checkpoint");

    assert_eq!(window_ids(&state, &alpha), alpha_windows);
    assert_eq!(window_ids(&state, &beta), beta_windows);
    assert_eq!(
        state.auto_named_windows,
        HashSet::from([(alpha.clone(), 1), (beta.clone(), 1)]),
        "the captured automatic-name membership is restored after both sessions"
    );
}

#[test]
fn session_checkpoint_missing_later_session_keeps_metadata_unrestored() {
    let alpha = session_name("checkpoint-partial-alpha");
    let beta = session_name("checkpoint-partial-beta");
    let mut state = checkpoint_state(&alpha, &beta);
    let alpha_windows = window_ids(&state, &alpha);
    let checkpoint = checkpoint_both(&state, &alpha, &beta);

    mutate_checkpointed_state(&mut state, &alpha, &beta);
    let auto_named_mutated = state.auto_named_windows.clone();
    state.sessions = SessionStore::default();
    state
        .sessions
        .create_session(alpha.clone(), SIZE)
        .expect("recreate alpha");

    let _ = checkpoint
        .restore(&mut state)
        .expect_err("the missing second session stops the rollback");

    assert_eq!(
        window_ids(&state, &alpha),
        alpha_windows,
        "the first session was replaced before the failure"
    );
    assert!(state.sessions.session(&beta).is_none());
    assert_eq!(
        state.auto_named_windows, auto_named_mutated,
        "metadata restoration never runs after a failed session replacement"
    );
}

use crate::test_names::session_name;
