use super::inactive_winlink_resize::{
    assert_window_and_pty_size, register_sized_attach, LARGE_SIZE, SMALL_SIZE,
};
use super::{create_grouped_last_pane_family, RequestHandler};
use crate::pane_io::PaneExitEvent;
use crate::test_fixtures::Fixture;
use rmux_proto::{
    KillPaneRequest, LinkWindowRequest, NewWindowRequest, PaneKillRequest, PaneTarget,
    PaneTargetRef, Request, Response, SelectWindowRequest, TerminalSize, UnlinkWindowRequest,
    WindowTarget,
};

#[derive(Clone, Copy)]
enum AliasRemoval {
    PaneKill,
    UnlinkWindow,
}

#[derive(Clone, Copy)]
enum LinkedFamilyRemoval {
    KillPane,
    NaturalExit,
}

#[derive(Clone, Copy)]
struct ResizeScenario {
    policy: &'static str,
    source_size: TerminalSize,
    survivor_size: TerminalSize,
    expected_before: TerminalSize,
    expected_after: TerminalSize,
}

const REMOVE_SMALL_CLIENT: ResizeScenario = ResizeScenario {
    policy: "smallest",
    source_size: SMALL_SIZE,
    survivor_size: LARGE_SIZE,
    expected_before: SMALL_SIZE,
    expected_after: LARGE_SIZE,
};

const REMOVE_LARGE_CLIENT: ResizeScenario = ResizeScenario {
    policy: "largest",
    source_size: LARGE_SIZE,
    survivor_size: SMALL_SIZE,
    expected_before: LARGE_SIZE,
    expected_after: SMALL_SIZE,
};

async fn assert_attach_removed(handler: &RequestHandler, removed_pid: u32, retained_pid: u32) {
    let active_attach = handler.active_attach.lock().await;
    assert!(!active_attach.by_pid.contains_key(&removed_pid));
    assert!(active_attach.by_pid.contains_key(&retained_pid));
}

#[tokio::test]
async fn pane_id_non_owner_alias_kill_reconciles_smallest_after_small_client_removal() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-smallest-non-owner").await;
    handler.set_window_size_policy(&owner, 0, "smallest").await;

    let large_pid = 7301;
    let small_pid = 7302;
    let _large_rx = register_sized_attach(&handler, large_pid, &owner, LARGE_SIZE).await;
    let _small_rx = register_sized_attach(&handler, small_pid, &peer, SMALL_SIZE).await;
    assert_window_and_pty_size(&handler, &owner, 0, SMALL_SIZE).await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&peer).is_none());
        assert!(state.sessions.session(&owner).is_some());
    }
    assert_attach_removed(&handler, small_pid, large_pid).await;
    assert_window_and_pty_size(&handler, &owner, 0, LARGE_SIZE).await;
}

#[tokio::test]
async fn pane_id_runtime_owner_kill_transfers_and_reconciles_smallest_runtime() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-smallest-owner").await;
    handler.set_window_size_policy(&owner, 0, "smallest").await;

    let small_pid = 7311;
    let large_pid = 7312;
    let _small_rx = register_sized_attach(&handler, small_pid, &owner, SMALL_SIZE).await;
    let _large_rx = register_sized_attach(&handler, large_pid, &peer, LARGE_SIZE).await;
    assert_window_and_pty_size(&handler, &owner, 0, SMALL_SIZE).await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(owner.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&owner).is_none());
        assert!(state.sessions.session(&peer).is_some());
        assert_eq!(state.sessions.runtime_owner(&peer), Some(peer.clone()));
    }
    assert_attach_removed(&handler, small_pid, large_pid).await;
    assert_window_and_pty_size(&handler, &peer, 0, LARGE_SIZE).await;
}

#[tokio::test]
async fn pane_id_group_and_real_winlink_reconcile_smallest_after_alias_removal() {
    let handler = RequestHandler::new();
    let (_keeper, owner, peer, family_pane_id) =
        create_grouped_last_pane_family(&handler, "pane-id-smallest-linked").await;
    let linked_survivor = handler
        .create_session("pane-id-smallest-linked-survivor")
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(linked_survivor.clone(), 1),
        )))
        .await;
    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(linked_survivor.clone(), 1),
        })
        .await;
    handler.wait_for_initial_panes_for_test().await;

    let large_pid = 7321;
    let small_pid = 7322;
    let _large_rx = register_sized_attach(&handler, large_pid, &owner, LARGE_SIZE).await;
    let _small_rx = register_sized_attach(&handler, small_pid, &peer, SMALL_SIZE).await;
    handler.set_window_size_policy(&owner, 0, "smallest").await;
    handler.set_window_size_policy(&peer, 0, "smallest").await;
    handler
        .set_window_size_policy(&linked_survivor, 1, "smallest")
        .await;
    handler
        .reconcile_attached_session_size_and_emit(&owner)
        .await
        .expect("linked family smallest policy reconciles");
    assert_window_and_pty_size(&handler, &owner, 0, SMALL_SIZE).await;
    assert_window_and_pty_size(&handler, &linked_survivor, 1, SMALL_SIZE).await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(peer.clone(), family_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&peer).is_none());
        assert!(state.sessions.session(&owner).is_some());
        assert!(state.sessions.session(&linked_survivor).is_some());
    }
    assert_attach_removed(&handler, small_pid, large_pid).await;
    assert_window_and_pty_size(&handler, &owner, 0, LARGE_SIZE).await;
    assert_window_and_pty_size(&handler, &linked_survivor, 1, LARGE_SIZE).await;
}

async fn assert_multi_window_alias_removal_reconciles_real_winlink_survivor(
    label: &str,
    removal: AliasRemoval,
    scenario: ResizeScenario,
    first_pid: u32,
) {
    let handler = RequestHandler::new();
    let source = handler.create_session(format!("{label}-source")).await;
    let survivor = handler.create_session(format!("{label}-survivor")).await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(survivor.clone(), 0),
            WindowTarget::with_window(source.clone(), 1),
        )))
        .await;
    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(source.clone(), 1),
        })
        .await;
    handler.wait_for_initial_panes_for_test().await;

    let family_pane_id = {
        let state = handler.state.lock().await;
        let source_session = state.sessions.session(&source).expect("source exists");
        assert_eq!(source_session.active_window_index(), 1);
        let linked_sessions = state.window_linked_sessions_list(&source, 1);
        assert!(linked_sessions.contains(&source));
        assert!(linked_sessions.contains(&survivor));
        source_session
            .window_at(1)
            .and_then(|window| window.pane(0))
            .expect("linked source pane exists")
            .id()
    };
    handler
        .set_window_size_policy(&source, 1, scenario.policy)
        .await;
    handler
        .set_window_size_policy(&survivor, 0, scenario.policy)
        .await;

    let survivor_pid = first_pid;
    let source_pid = first_pid + 1;
    let _survivor_rx =
        register_sized_attach(&handler, survivor_pid, &survivor, scenario.survivor_size).await;
    let _source_rx =
        register_sized_attach(&handler, source_pid, &source, scenario.source_size).await;
    handler
        .reconcile_attached_session_size_and_emit(&survivor)
        .await
        .expect("linked family size policy reconciles");
    assert_window_and_pty_size(&handler, &survivor, 0, scenario.expected_before).await;

    let response = match removal {
        AliasRemoval::PaneKill => {
            handler
                .handle(Request::PaneKill(PaneKillRequest {
                    target: PaneTargetRef::by_id(source.clone(), family_pane_id),
                    kill_all_except: false,
                }))
                .await
        }
        AliasRemoval::UnlinkWindow => {
            handler
                .handle(Request::UnlinkWindow(UnlinkWindowRequest {
                    target: WindowTarget::with_window(source.clone(), 1),
                    kill_if_last: false,
                }))
                .await
        }
    };
    assert!(
        matches!(response, Response::KillPane(_) | Response::UnlinkWindow(_)),
        "{response:?}"
    );

    {
        let state = handler.state.lock().await;
        let source_session = state.sessions.session(&source).expect("source survives");
        assert!(source_session.window_at(0).is_some());
        assert!(source_session.window_at(1).is_none());
        assert!(state
            .sessions
            .session(&survivor)
            .and_then(|session| session.window_at(0))
            .is_some());
    }
    {
        let active_attach = handler.active_attach.lock().await;
        assert!(active_attach.by_pid.contains_key(&source_pid));
        assert!(active_attach.by_pid.contains_key(&survivor_pid));
    }
    assert_window_and_pty_size(&handler, &survivor, 0, scenario.expected_after).await;
}

#[tokio::test]
async fn pane_id_multi_window_alias_kill_reconciles_smallest_real_winlink_survivor() {
    assert_multi_window_alias_removal_reconciles_real_winlink_survivor(
        "pane-id-multi-window-smallest",
        AliasRemoval::PaneKill,
        REMOVE_SMALL_CLIENT,
        7331,
    )
    .await;
}

#[tokio::test]
async fn pane_id_multi_window_alias_kill_reconciles_largest_real_winlink_survivor() {
    assert_multi_window_alias_removal_reconciles_real_winlink_survivor(
        "pane-id-multi-window-largest",
        AliasRemoval::PaneKill,
        REMOVE_LARGE_CLIENT,
        7341,
    )
    .await;
}

#[tokio::test]
async fn unlink_window_reconciles_smallest_real_winlink_survivor() {
    assert_multi_window_alias_removal_reconciles_real_winlink_survivor(
        "unlink-window-smallest",
        AliasRemoval::UnlinkWindow,
        REMOVE_SMALL_CLIENT,
        7351,
    )
    .await;
}

#[tokio::test]
async fn unlink_window_reconciles_largest_real_winlink_survivor() {
    assert_multi_window_alias_removal_reconciles_real_winlink_survivor(
        "unlink-window-largest",
        AliasRemoval::UnlinkWindow,
        REMOVE_LARGE_CLIENT,
        7361,
    )
    .await;
}

async fn assert_linked_family_removal_reconciles_replacement_window(
    label: &str,
    removal: LinkedFamilyRemoval,
    scenario: ResizeScenario,
    first_pid: u32,
) {
    let handler = RequestHandler::new();
    let source = handler
        .create_session((format!("{label}-source"), scenario.expected_before))
        .await;
    let survivor = handler
        .create_session((format!("{label}-survivor"), scenario.expected_before))
        .await;
    let replacement_alias = handler
        .create_session((
            format!("{label}-replacement-alias"),
            scenario.expected_before,
        ))
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(survivor.clone(), 0),
            WindowTarget::with_window(replacement_alias.clone(), 1),
        )))
        .await;
    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(replacement_alias.clone(), 1),
        })
        .await;
    handler
        .create_window(NewWindowRequest {
            name: Some("shared".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&source)
        })
        .await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 1),
            WindowTarget::with_window(survivor.clone(), 1),
        )))
        .await;
    for session_name in [&source, &survivor] {
        handler
            .handle_ok(SelectWindowRequest {
                target: WindowTarget::with_window(session_name.clone(), 1),
            })
            .await;
        handler
            .set_window_size_policy(session_name, 1, scenario.policy)
            .await;
    }
    handler
        .set_window_size_policy(&survivor, 0, scenario.policy)
        .await;
    handler
        .set_window_size_policy(&replacement_alias, 1, scenario.policy)
        .await;
    handler.wait_for_initial_panes_for_test().await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&source)
            .and_then(|session| session.window_at(1))
            .and_then(|window| window.pane(0))
            .expect("shared pane exists")
            .id()
    };

    let survivor_pid = first_pid;
    let source_pid = first_pid + 1;
    let _survivor_rx =
        register_sized_attach(&handler, survivor_pid, &survivor, scenario.survivor_size).await;
    let _source_rx =
        register_sized_attach(&handler, source_pid, &source, scenario.source_size).await;
    handler
        .reconcile_attached_session_size_and_emit(&survivor)
        .await
        .expect("linked family size policy reconciles");
    assert_window_and_pty_size(&handler, &survivor, 1, scenario.expected_before).await;
    assert_window_and_pty_size(&handler, &replacement_alias, 1, scenario.expected_before).await;

    match removal {
        LinkedFamilyRemoval::KillPane => {
            handler
                .handle_ok(KillPaneRequest {
                    target: PaneTarget::with_window(source.clone(), 1, 0),
                    kill_all_except: false,
                })
                .await;
        }
        LinkedFamilyRemoval::NaturalExit => {
            {
                let mut state = handler.state.lock().await;
                state
                    .mark_pane_dead_without_exit_details(&PaneTarget::with_window(
                        source.clone(),
                        1,
                        0,
                    ))
                    .expect("mark linked pane naturally exited");
            }
            handler
                .handle_pane_exit_event(PaneExitEvent::eof_published(source.clone(), pane_id, None))
                .await;
        }
    }

    {
        let state = handler.state.lock().await;
        for session_name in [&source, &survivor] {
            let session = state
                .sessions
                .session(session_name)
                .expect("session survives linked family removal");
            assert!(session.window_at(0).is_some());
            assert!(
                session.window_at(1).is_none(),
                "shared window remains in {session_name}"
            );
        }
    }
    assert_window_and_pty_size(&handler, &survivor, 0, scenario.expected_after).await;
    assert_window_and_pty_size(&handler, &replacement_alias, 1, scenario.expected_after).await;
}

#[tokio::test]
async fn kill_pane_linked_family_reconciles_smallest_replacement_window() {
    assert_linked_family_removal_reconciles_replacement_window(
        "kill-pane-linked-family-smallest",
        LinkedFamilyRemoval::KillPane,
        REMOVE_SMALL_CLIENT,
        7371,
    )
    .await;
}

#[tokio::test]
async fn natural_linked_pane_exit_reconciles_largest_replacement_window() {
    assert_linked_family_removal_reconciles_replacement_window(
        "natural-linked-family-largest",
        LinkedFamilyRemoval::NaturalExit,
        REMOVE_LARGE_CLIENT,
        7381,
    )
    .await;
}
