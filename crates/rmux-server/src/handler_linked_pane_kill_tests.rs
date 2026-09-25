use super::RequestHandler;
use rmux_core::PaneId;
use rmux_proto::{
    ErrorResponse, KillPaneRequest, LinkWindowRequest, NewSessionRequest, NewWindowRequest,
    OptionName, PaneTarget, Request, Response, RmuxError, ScopeSelector, SessionName,
    SetOptionMode, SplitDirection, SplitWindowRequest, SplitWindowTarget, Target, TerminalSize,
    WindowTarget,
};

use crate::test_names::session_name;

async fn create_session(handler: &RequestHandler, value: &str) -> SessionName {
    let session = session_name(value);
    let response = handler
        .handle(Request::NewSession(NewSessionRequest {
            session_name: session.clone(),
            detached: true,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            environment: None,
        }))
        .await;
    assert!(matches!(response, Response::NewSession(_)), "{response:?}");
    session
}

async fn split(handler: &RequestHandler, session: &SessionName) {
    let response = handler
        .handle(Request::SplitWindow(SplitWindowRequest {
            target: SplitWindowTarget::Session(session.clone()),
            direction: SplitDirection::Vertical,
            before: false,
            environment: None,
        }))
        .await;
    assert!(matches!(response, Response::SplitWindow(_)), "{response:?}");
}

async fn create_window(handler: &RequestHandler, session: &SessionName, index: u32) {
    let response = handler
        .handle(Request::NewWindow(Box::new(NewWindowRequest {
            target: session.clone(),
            name: None,
            detached: true,
            environment: None,
            command: None,
            start_directory: None,
            target_window_index: Some(index),
            insert_at_target: false,
            process_command: None,
        })))
        .await;
    assert!(matches!(response, Response::NewWindow(_)), "{response:?}");
}

async fn link_window(handler: &RequestHandler, owner: &SessionName, alias: &SessionName) {
    let response = handler
        .handle(Request::LinkWindow(LinkWindowRequest {
            source: WindowTarget::with_window(owner.clone(), 0),
            target: WindowTarget::with_window(alias.clone(), 0),
            after: false,
            before: false,
            kill_destination: true,
            detached: true,
        }))
        .await;
    assert!(matches!(response, Response::LinkWindow(_)), "{response:?}");
}

fn pane_ids(state: &crate::pane_terminals::HandlerState, session: &SessionName) -> Vec<PaneId> {
    state
        .sessions
        .session(session)
        .expect("session exists")
        .window_at(0)
        .expect("window exists")
        .panes()
        .iter()
        .map(|pane| pane.id())
        .collect()
}

#[tokio::test]
async fn linked_pane_kill_from_alias_updates_owner_and_alias() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-kill-owner").await;
    split(&handler, &owner).await;
    let alias = create_session(&handler, "linked-kill-alias").await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let removed_pane_id = {
        let state = handler.state.lock().await;
        pane_ids(&state, &owner)[1]
    };
    let response = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 1),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    let owner_ids = pane_ids(&state, &owner);
    assert_eq!(owner_ids, pane_ids(&state, &alias));
    assert_eq!(owner_ids.len(), 1);
    assert_ne!(owner_ids[0], removed_pane_id);
    state
        .ensure_window_panes_exist(&alias, 0, &owner_ids)
        .expect("linked alias resolves the surviving owner runtime");
}

#[tokio::test]
async fn linked_pane_kill_all_except_from_owner_updates_every_alias() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-kill-all-owner").await;
    split(&handler, &owner).await;
    split(&handler, &owner).await;
    let alias = create_session(&handler, "linked-kill-all-alias").await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let kept_pane_id = {
        let state = handler.state.lock().await;
        pane_ids(&state, &owner)[1]
    };
    let response = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::with_window(owner.clone(), 0, 1),
            kill_all_except: true,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    assert_eq!(pane_ids(&state, &owner), vec![kept_pane_id]);
    assert_eq!(pane_ids(&state, &alias), vec![kept_pane_id]);
    state
        .ensure_window_panes_exist(&alias, 0, &[kept_pane_id])
        .expect("the kept pane runtime remains reachable through the alias");
}

#[tokio::test]
async fn linked_alias_kill_resize_rollback_restores_shared_runtime() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-rollback-owner").await;
    split(&handler, &owner).await;
    let alias = create_session(&handler, "linked-rollback-alias").await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let (pane_id, pane_instance) = {
        let mut state = handler.state.lock().await;
        let pane_id = pane_ids(&state, &owner)[1];
        // The pane's *job*, not a process id. A pane created without a command runs the
        // interpreter embedded in this daemon and has no OS child, so a pid no longer identifies
        // "the same runtime". The job's snapshot uid does, and it is allocated once per spawn —
        // a rollback that quietly restarted the shared pane would show a different one.
        let (_, shell) = state
            .pane_shell_if_alive(&owner, 0, 1)
            .expect("second pane has a live job");
        let pane_instance = shell.sandbox().uid.clone();
        assert!(state
            .toggle_marked_pane(&PaneTarget::with_window(owner.clone(), 0, 1))
            .expect("pane can be marked"));
        state.fail_next_resize_for_test();
        (pane_id, pane_instance)
    };

    let response = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 1),
            kill_all_except: false,
        }))
        .await;
    assert_eq!(
        response,
        Response::Error(ErrorResponse {
            error: RmuxError::Server("injected pane terminal resize failure".to_owned()),
        })
    );

    let state = handler.state.lock().await;
    for session in [&owner, &alias] {
        assert!(pane_ids(&state, session).contains(&pane_id));
    }
    state
        .ensure_window_panes_exist(&alias, 0, &[pane_id])
        .expect("rollback restores the owner runtime for the alias");
    state
        .pane_output_for_target(&alias, 0, 1)
        .expect("rollback restores pane output for the alias");
    assert!(state.pane_is_marked(&PaneTarget::with_window(owner.clone(), 0, 1)));
    let (_, restored) = state
        .pane_shell_if_alive(&alias, 0, 1)
        .expect("restored pane job remains inspectable through the alias");
    assert_eq!(
        restored.sandbox().uid,
        pane_instance,
        "rollback must preserve the shared pane job"
    );
}

#[tokio::test]
async fn linked_last_pane_kill_removes_shared_window_from_all_surviving_sessions() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-last-owner").await;
    create_window(&handler, &owner, 1).await;
    let alias = create_session(&handler, "linked-last-alias").await;
    create_window(&handler, &alias, 1).await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let response = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 0),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    for session_name in [&owner, &alias] {
        let session = state
            .sessions
            .session(session_name)
            .expect("session survives");
        assert!(session.window_at(0).is_none());
        assert!(session.window_at(1).is_some());
    }
}

#[tokio::test]
async fn linked_last_pane_kill_destroys_only_alias_with_no_surviving_window() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-last-owner-survivor").await;
    create_window(&handler, &owner, 1).await;
    let alias = create_session(&handler, "linked-last-only-alias").await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let response = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 0),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alias).is_none());
    let owner_session = state.sessions.session(&owner).expect("owner survives");
    assert!(owner_session.window_at(0).is_none());
    assert!(owner_session.window_at(1).is_some());
}

/// The renumber that follows a linked last-pane kill can collide with a stale
/// window override, which drives the real metadata-restore path in
/// `reindex_windows_from_base` and then the linked-kill snapshot restore. No
/// injection seam is involved: the collision is produced by ordinary options.
#[tokio::test]
async fn linked_last_pane_kill_metadata_collision_restores_aliases_and_runtime() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-metadata-owner").await;
    create_window(&handler, &owner, 2).await;
    create_window(&handler, &owner, 3).await;
    let alias = create_session(&handler, "linked-metadata-alias").await;
    create_window(&handler, &alias, 2).await;
    link_window(&handler, &owner, &alias).await;
    handler.wait_for_initial_panes_for_test().await;

    let mut state = handler.state.lock().await;
    for (option, value) in [
        (OptionName::BaseIndex, "0"),
        (OptionName::RenumberWindows, "on"),
    ] {
        state
            .options
            .set(
                ScopeSelector::Session(owner.clone()),
                option,
                value.to_owned(),
                SetOptionMode::Replace,
            )
            .expect("session option applies");
    }
    // Window index 1 deliberately has no live window: renumbering maps owner:3
    // onto owner:1 and collides with this stale override.
    for (window_index, value) in [(0, "off"), (1, "off"), (3, "on")] {
        state
            .options
            .set(
                ScopeSelector::Window(WindowTarget::with_window(owner.clone(), window_index)),
                OptionName::AutomaticRename,
                value.to_owned(),
                SetOptionMode::Replace,
            )
            .expect("window override applies");
    }
    state.mark_auto_named_window(&owner, 0);

    let pane_id = pane_ids(&state, &owner)[0];
    let pane_instance = state
        .pane_shell_if_alive(&owner, 0, 0)
        .expect("linked pane has a live job")
        .1
        .sandbox()
        .uid
        .clone();
    let stable_owner = crate::handler::StableTargetIdentity::capture(
        &mut state,
        Target::Pane(PaneTarget::with_window(owner.clone(), 0, 0)),
    )
    .expect("capture owner:0.0 identity");
    let stable_alias = crate::handler::StableTargetIdentity::capture(
        &mut state,
        Target::Pane(PaneTarget::with_window(alias.clone(), 0, 0)),
    )
    .expect("capture alias:0.0 identity");

    let error = state
        .kill_pane(PaneTarget::with_window(alias.clone(), 0, 0))
        .expect_err("stale window override collides with the renumber");
    let RmuxError::Server(reason) = &error else {
        panic!("expected a server-side collision error: {error:?}");
    };
    assert!(
        reason.starts_with("window options already exist for"),
        "{reason}"
    );

    assert_eq!(
        state
            .sessions
            .session(&owner)
            .expect("owner survives")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0, 2, 3]
    );
    assert_eq!(
        state
            .sessions
            .session(&alias)
            .expect("alias survives")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert!(stable_owner.is_current(&state));
    assert!(stable_alias.is_current(&state));
    assert_eq!(state.window_link_count(&owner, 0), 2);
    assert_eq!(state.window_link_count(&alias, 0), 2);
    for (window_index, value) in [(0, "off"), (1, "off"), (3, "on")] {
        assert_eq!(
            state.options.window_value(
                &WindowTarget::with_window(owner.clone(), window_index),
                OptionName::AutomaticRename
            ),
            Some(value),
            "rollback must restore the owner:{window_index} automatic-rename override"
        );
    }
    assert!(state.tracks_auto_named_window(&owner, 0));
    for session in [&owner, &alias] {
        assert!(pane_ids(&state, session).contains(&pane_id));
    }
    state
        .pane_output_for_target(&alias, 0, 0)
        .expect("rollback restores pane output for the alias");
    let restored = state
        .pane_shell_if_alive(&alias, 0, 0)
        .expect("restored pane job remains inspectable through the alias")
        .1;
    assert_eq!(
        restored.sandbox().uid,
        pane_instance,
        "rollback must preserve the shared pane job"
    );
}
