use super::RequestHandler;
use rmux_core::PaneId;
use rmux_proto::{
    ErrorResponse, KillPaneRequest, LinkWindowRequest, NewWindowRequest, OptionName, PaneTarget,
    Request, Response, RmuxError, ScopeSelector, SessionName, SetOptionMode, SplitWindowRequest,
    Target, WindowTarget,
};

use crate::test_fixtures::{Fixture, SessionSpec, TestRequest};

/// Links `owner:0` over `alias:0`, replacing the alias's own window 0.
async fn link_window(handler: &RequestHandler, owner: &SessionName, alias: &SessionName) {
    TestRequest::send_ok(
        handler,
        LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(owner.clone(), 0),
                WindowTarget::with_window(alias.clone(), 0),
            ))
        },
    )
    .await;
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
    let owner = SessionSpec::create(&handler, "linked-kill-owner").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&owner)).await;
    let alias = SessionSpec::create(&handler, "linked-kill-alias").await;
    link_window(&handler, &owner, &alias).await;

    let removed_pane_id = {
        let state = handler.state.lock().await;
        pane_ids(&state, &owner)[1]
    };
    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 1),
            kill_all_except: false,
        },
    )
    .await;

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
    let owner = SessionSpec::create(&handler, "linked-kill-all-owner").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&owner)).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&owner)).await;
    let alias = SessionSpec::create(&handler, "linked-kill-all-alias").await;
    link_window(&handler, &owner, &alias).await;

    let kept_pane_id = {
        let state = handler.state.lock().await;
        pane_ids(&state, &owner)[1]
    };
    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(owner.clone(), 0, 1),
            kill_all_except: true,
        },
    )
    .await;

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
    let owner = SessionSpec::create(&handler, "linked-rollback-owner").await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&owner)).await;
    let alias = SessionSpec::create(&handler, "linked-rollback-alias").await;
    link_window(&handler, &owner, &alias).await;

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
    let owner = SessionSpec::create(&handler, "linked-last-owner").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let alias = SessionSpec::create(&handler, "linked-last-alias").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&alias)
        })
        .await;
    link_window(&handler, &owner, &alias).await;

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 0),
            kill_all_except: false,
        },
    )
    .await;

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
    let owner = SessionSpec::create(&handler, "linked-last-owner-survivor").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    let alias = SessionSpec::create(&handler, "linked-last-only-alias").await;
    link_window(&handler, &owner, &alias).await;

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(alias.clone(), 0, 0),
            kill_all_except: false,
        },
    )
    .await;

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
    let owner = SessionSpec::create(&handler, "linked-metadata-owner").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(2),
            ..Fixture::fixture(&owner)
        })
        .await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(3),
            ..Fixture::fixture(&owner)
        })
        .await;
    let alias = SessionSpec::create(&handler, "linked-metadata-alias").await;
    handler
        .create_window(NewWindowRequest {
            target_window_index: Some(2),
            ..Fixture::fixture(&alias)
        })
        .await;
    link_window(&handler, &owner, &alias).await;

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
