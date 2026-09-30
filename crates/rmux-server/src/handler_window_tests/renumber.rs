use super::lifecycle::kill_window;
use super::*;
use crate::pane_io::PaneExitEvent;
use crate::test_fixtures::TestRequest;
use rmux_proto::{OptionScopeSelector, PaneKillRequest, SetHookRequest};

const RENUMBER_MARKER: &str = "@renumber-metadata";

struct RenumberMetadataFixture {
    session_name: SessionName,
    removed_pane: PaneTarget,
    removed_pane_id: rmux_core::PaneId,
    surviving_window_id: rmux_core::WindowId,
}

#[derive(Debug, Clone, Copy)]
enum OracleActiveWindowRemoval {
    IndexedKillPane,
    StablePaneKill,
    KillWindow,
    UnlinkWindowKill,
    NaturalExit,
}

async fn set_renumber_metadata(
    handler: &RequestHandler,
    target: WindowTarget,
    marker: &str,
    hook_command: &str,
) {
    handler
        .set_option_by_name(
            OptionScopeSelector::Window(target.clone()),
            RENUMBER_MARKER,
            marker,
        )
        .await;
    TestRequest::send_ok(
        handler,
        SetHookRequest::fixture((
            ScopeSelector::Window(target),
            HookName::WindowLayoutChanged,
            hook_command,
        )),
    )
    .await;
}

async fn enable_renumber_windows(handler: &RequestHandler, session: &SessionName) {
    handler
        .set_option(
            ScopeSelector::Session(session.clone()),
            OptionName::RenumberWindows,
            "on",
        )
        .await;
}

async fn renumber_metadata_fixture(
    handler: &RequestHandler,
    label: &str,
) -> RenumberMetadataFixture {
    let session_name = create_session(handler, label).await;
    insert_window(handler, &session_name, 1).await;
    insert_window(handler, &session_name, 2).await;
    enable_renumber_windows(handler, &session_name).await;

    set_renumber_metadata(
        handler,
        WindowTarget::with_window(session_name.clone(), 1),
        "discarded",
        "display-message discarded-hook",
    )
    .await;
    set_renumber_metadata(
        handler,
        WindowTarget::with_window(session_name.clone(), 2),
        "survivor",
        "display-message survivor-hook",
    )
    .await;
    TestRequest::send_ok(
        handler,
        RenameWindowRequest {
            target: WindowTarget::with_window(session_name.clone(), 2),
            name: "surviving-name".to_owned(),
        },
    )
    .await;

    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session exists");
    let removed_pane_id = session
        .window_at(1)
        .and_then(|window| window.pane(0))
        .map(rmux_core::Pane::id)
        .expect("removed pane exists");
    let surviving_window_id = session
        .window_at(2)
        .map(rmux_core::Window::id)
        .expect("surviving window exists");
    drop(state);

    RenumberMetadataFixture {
        removed_pane: PaneTarget::with_window(session_name.clone(), 1, 0),
        session_name,
        removed_pane_id,
        surviving_window_id,
    }
}

async fn assert_surviving_renumber_metadata(
    handler: &RequestHandler,
    fixture: &RenumberMetadataFixture,
) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&fixture.session_name)
        .expect("session survives");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    let survivor = session.window_at(1).expect("surviving window is reindexed");
    assert_eq!(survivor.id(), fixture.surviving_window_id);
    assert_eq!(survivor.name(), Some("surviving-name"));
    assert!(!survivor.automatic_rename());

    let target = WindowTarget::with_window(fixture.session_name.clone(), 1);
    assert_eq!(
        state
            .options
            .explicit_value_by_name(
                &OptionScopeSelector::Window(target.clone()),
                RENUMBER_MARKER,
            )
            .expect("valid user option")
            .1
            .as_deref(),
        Some("survivor")
    );
    assert_eq!(
        state
            .options
            .resolve_for_window(&fixture.session_name, 1, OptionName::AutomaticRename),
        Some("off")
    );
    assert_eq!(
        state
            .hooks
            .window_bindings_view(&target, Some(HookName::WindowLayoutChanged))
            .iter()
            .map(|binding| binding.command())
            .collect::<Vec<_>>(),
        vec!["display-message survivor-hook"]
    );
}

#[tokio::test]
async fn kill_window_renumbers_when_session_option_is_enabled() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;
    enable_renumber_windows(&handler, &alpha).await;

    assert_eq!(
        kill_window(&handler, &alpha, 1).await,
        WindowTarget::with_window(alpha.clone(), 0)
    );

    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&alpha)
        .expect("session should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(session.active_window_index(), 0);
}

#[tokio::test]
async fn kill_last_pane_renumbers_when_session_option_is_enabled() {
    // tmux 3.7b, measured on 2026-07-26: killing the only pane in window 1
    // closes that window and renumbers the surviving 0/2 slots to 0/1.
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "kill-pane-renumber").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;
    enable_renumber_windows(&handler, &alpha).await;

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(alpha.clone(), 1, 0),
            kill_all_except: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session survives");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn kill_last_pane_discards_removed_metadata_before_renumbering_survivor() {
    let handler = RequestHandler::new();
    let fixture = renumber_metadata_fixture(&handler, "kill-pane-renumber-metadata").await;

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: fixture.removed_pane.clone(),
            kill_all_except: false,
        },
    )
    .await;

    assert_surviving_renumber_metadata(&handler, &fixture).await;
}

#[tokio::test]
async fn pane_kill_by_id_preserves_surviving_metadata_when_window_is_renumbered() {
    let handler = RequestHandler::new();
    let fixture = renumber_metadata_fixture(&handler, "pane-id-renumber-metadata").await;

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(fixture.session_name.clone(), fixture.removed_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)), "{response:?}");

    assert_surviving_renumber_metadata(&handler, &fixture).await;
}

#[tokio::test]
async fn natural_last_pane_exit_preserves_surviving_metadata_when_window_is_renumbered() {
    let handler = RequestHandler::new();
    let fixture = renumber_metadata_fixture(&handler, "natural-renumber-metadata").await;
    {
        let mut state = handler.state.lock().await;
        state
            .mark_pane_dead_without_exit_details(&fixture.removed_pane)
            .expect("mark pane exited");
    }

    handler
        .handle_pane_exit_event(PaneExitEvent::eof_published(
            fixture.session_name.clone(),
            fixture.removed_pane_id,
            None,
        ))
        .await;

    assert_surviving_renumber_metadata(&handler, &fixture).await;
}

#[tokio::test]
async fn kill_last_linked_pane_renumbers_each_surviving_session() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "kill-linked-pane-renumber-owner").await;
    insert_window(&handler, &owner, 1).await;
    insert_window(&handler, &owner, 2).await;
    let alias = create_session(&handler, "kill-linked-pane-renumber-alias").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 1),
            WindowTarget::with_window(alias.clone(), 9),
        )),
    )
    .await;
    for session_name in [&owner, &alias] {
        enable_renumber_windows(&handler, session_name).await;
    }

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: PaneTarget::with_window(owner.clone(), 1, 0),
            kill_all_except: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&owner)
            .expect("owner survives")
            .windows()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0, 1]
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
        vec![0]
    );
}

#[tokio::test]
async fn active_low_window_removals_select_oracle_stable_fallback_after_renumber() {
    // The 2026-07-27 tmux 3.7b matrix selects original window index 2 after
    // removing active index 0 with no last-window. The expected stable ID is
    // captured before mutation for every public removal surface.
    for (case_index, removal) in [
        OracleActiveWindowRemoval::IndexedKillPane,
        OracleActiveWindowRemoval::StablePaneKill,
        OracleActiveWindowRemoval::KillWindow,
        OracleActiveWindowRemoval::UnlinkWindowKill,
        OracleActiveWindowRemoval::NaturalExit,
    ]
    .into_iter()
    .enumerate()
    {
        let handler = RequestHandler::new();
        let alpha = create_session(&handler, format!("oracle-fallback-{case_index}")).await;
        handler
            .wait_for_pane_startup_to_finish_for_test(&PaneTarget::with_window(alpha.clone(), 0, 0))
            .await;
        insert_window(&handler, &alpha, 1).await;
        insert_window(&handler, &alpha, 2).await;
        enable_renumber_windows(&handler, &alpha).await;

        let (removed_pane_id, expected_window_id) = {
            let state = handler.state.lock().await;
            let session = state.sessions.session(&alpha).expect("session exists");
            assert_eq!(session.active_window_index(), 0);
            assert_eq!(session.last_window_index(), None);
            (
                session
                    .window_at(0)
                    .and_then(|window| window.pane(0))
                    .map(rmux_core::Pane::id)
                    .expect("removed pane exists"),
                session
                    .window_at(2)
                    .map(rmux_core::Window::id)
                    .expect("oracle fallback exists before mutation"),
            )
        };

        let response = match removal {
            OracleActiveWindowRemoval::IndexedKillPane => Some(
                handler
                    .handle(Request::KillPane(KillPaneRequest {
                        target: PaneTarget::with_window(alpha.clone(), 0, 0),
                        kill_all_except: false,
                    }))
                    .await,
            ),
            OracleActiveWindowRemoval::StablePaneKill => Some(
                handler
                    .handle(Request::PaneKill(PaneKillRequest {
                        target: PaneTargetRef::by_id(alpha.clone(), removed_pane_id),
                        kill_all_except: false,
                    }))
                    .await,
            ),
            OracleActiveWindowRemoval::KillWindow => Some(
                handler
                    .handle(Request::KillWindow(KillWindowRequest::fixture(
                        WindowTarget::with_window(alpha.clone(), 0),
                    )))
                    .await,
            ),
            OracleActiveWindowRemoval::UnlinkWindowKill => Some(
                handler
                    .handle(Request::UnlinkWindow(UnlinkWindowRequest {
                        target: WindowTarget::with_window(alpha.clone(), 0),
                        kill_if_last: true,
                    }))
                    .await,
            ),
            OracleActiveWindowRemoval::NaturalExit => {
                {
                    let mut state = handler.state.lock().await;
                    state
                        .mark_pane_dead_without_exit_details(&PaneTarget::with_window(
                            alpha.clone(),
                            0,
                            0,
                        ))
                        .expect("mark pane exited");
                }
                handler
                    .handle_pane_exit_event(PaneExitEvent::eof_published(
                        alpha.clone(),
                        removed_pane_id,
                        None,
                    ))
                    .await;
                None
            }
        };
        if let Some(response) = response {
            assert!(
                !matches!(response, Response::Error(_)),
                "{removal:?} failed: {response:?}"
            );
        }

        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session survives");
        assert_eq!(
            session.windows().keys().copied().collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            session.window().id(),
            expected_window_id,
            "{removal:?} selected the wrong stable identity"
        );
        assert_eq!(session.active_window_index(), 1);
    }
}

#[tokio::test]
async fn linked_and_grouped_removal_preserves_each_oracle_window_identity() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "oracle-linked-owner").await;
    insert_window(&handler, &owner, 1).await;
    insert_window(&handler, &owner, 2).await;
    let linked_peer = create_session(&handler, "oracle-linked-peer").await;
    insert_window(&handler, &linked_peer, 1).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(linked_peer.clone(), 9),
        )),
    )
    .await;
    for session in [&owner, &linked_peer] {
        enable_renumber_windows(&handler, session).await;
    }
    let (removed_pane_id, expected_owner_id, linked_peer_active_id) = {
        let state = handler.state.lock().await;
        let owner_session = state.sessions.session(&owner).expect("owner exists");
        let peer_session = state
            .sessions
            .session(&linked_peer)
            .expect("linked peer exists");
        (
            owner_session
                .window_at(0)
                .and_then(|window| window.pane(0))
                .map(rmux_core::Pane::id)
                .expect("shared pane exists"),
            owner_session
                .window_at(2)
                .map(rmux_core::Window::id)
                .expect("owner oracle fallback exists"),
            peer_session.window().id(),
        )
    };
    let killed = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(owner.clone(), removed_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(killed, Response::KillPane(_)), "{killed:?}");
    {
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .sessions
                .session(&owner)
                .expect("owner survives")
                .window()
                .id(),
            expected_owner_id
        );
        assert_eq!(
            state
                .sessions
                .session(&linked_peer)
                .expect("linked peer survives")
                .window()
                .id(),
            linked_peer_active_id,
            "inactive linked target must not change the peer's active identity"
        );
    }

    let grouped_handler = RequestHandler::new();
    let grouped_owner = create_session(&grouped_handler, "oracle-group-owner").await;
    insert_window(&grouped_handler, &grouped_owner, 1).await;
    insert_window(&grouped_handler, &grouped_owner, 2).await;
    let grouped_peer =
        create_grouped_session(&grouped_handler, "oracle-group-peer", &grouped_owner).await;
    let (grouped_pane_id, expected_grouped_id) = {
        let state = grouped_handler.state.lock().await;
        let session = state
            .sessions
            .session(&grouped_owner)
            .expect("group owner exists");
        (
            session
                .window_at(0)
                .and_then(|window| window.pane(0))
                .map(rmux_core::Pane::id)
                .expect("grouped pane exists"),
            session
                .window_at(2)
                .map(rmux_core::Window::id)
                .expect("group oracle fallback exists"),
        )
    };
    let killed = grouped_handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(grouped_owner.clone(), grouped_pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(killed, Response::KillPane(_)), "{killed:?}");
    let state = grouped_handler.state.lock().await;
    for session_name in [&grouped_owner, &grouped_peer] {
        assert_eq!(
            state
                .sessions
                .session(session_name)
                .expect("group member survives")
                .window()
                .id(),
            expected_grouped_id,
            "{session_name} selected the wrong grouped fallback identity"
        );
    }
}
