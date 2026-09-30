use super::*;

use crate::test_fixtures::TestRequest;
use rmux_proto::{
    LinkWindowResponse, OptionScopeSelector, PaneOptionSetRequest, SendKeysRequest,
    SetOptionByNameRequest,
};

async fn assert_send_keys_succeeds(handler: &RequestHandler, target: PaneTarget) {
    TestRequest::send_ok(
        handler,
        SendKeysRequest {
            target,
            keys: vec!["x".to_owned()],
        },
    )
    .await;
}

async fn assert_pane_output_observes(
    receiver: &mut crate::pane_io::PaneOutputReceiver,
    expected: &[u8],
) {
    timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await {
                rmux_core::events::OutputCursorItem::Event(event) if event.bytes() == expected => {
                    return;
                }
                rmux_core::events::OutputCursorItem::Event(_) => {}
                rmux_core::events::OutputCursorItem::Gap(gap) => {
                    panic!("pane output cursor fell behind before the expected event: {gap:?}");
                }
            }
        }
    })
    .await
    .expect("expected pane output was not observed");
}

#[tokio::test]
async fn grouped_unlink_k_preserves_each_session_local_fallback_identity() {
    for target_index in [2, 3] {
        for renumber in [false, true] {
            for peer_target_active in [false, true] {
                let handler = RequestHandler::new();
                let owner = session_name(&format!(
                    "unlink-local-owner-{target_index}-{renumber}-{peer_target_active}"
                ));
                let peer = session_name(&format!(
                    "unlink-local-peer-{target_index}-{renumber}-{peer_target_active}"
                ));
                handler
                    .set_option(
                        ScopeSelector::Global,
                        OptionName::BaseIndex,
                        &target_index.to_string(),
                    )
                    .await;
                create_session(&handler, &owner).await;
                for window_index in 0..target_index {
                    handler
                        .create_window(NewWindowRequest {
                            command: Some(quiet_command()),
                            target_window_index: Some(window_index),
                            ..Fixture::fixture(&owner)
                        })
                        .await;
                }
                create_grouped_session(&handler, &peer, &owner).await;
                if peer_target_active {
                    // tmux starts the peer on index 0, so this single command
                    // records 0 as its local last window. On the regression
                    // base RMUX has already copied the owner's target, making
                    // the same command a no-op with no fallback history.
                    TestRequest::send_ok(
                        &handler,
                        SelectWindowRequest {
                            target: WindowTarget::with_window(peer.clone(), target_index),
                        },
                    )
                    .await;
                }

                for session_name in [&owner, &peer] {
                    handler
                        .set_option(
                            ScopeSelector::Session(session_name.clone()),
                            OptionName::RenumberWindows,
                            if renumber { "on" } else { "off" },
                        )
                        .await;
                }

                let (owner_expected, peer_expected) = {
                    let state = handler.state.lock().await;
                    let owner_session = state.sessions.session(&owner).expect("owner exists");
                    let owner_expected = owner_session
                        .window_at(target_index - 1)
                        .expect("owner cyclic predecessor exists")
                        .id();
                    let peer_expected = state
                        .sessions
                        .session(&peer)
                        .and_then(|session| session.window_at(0))
                        .expect("peer local fallback exists")
                        .id();
                    (owner_expected, peer_expected)
                };

                TestRequest::send_ok(
                    &handler,
                    UnlinkWindowRequest {
                        target: WindowTarget::with_window(owner.clone(), target_index),
                        kill_if_last: true,
                    },
                )
                .await;

                let state = handler.state.lock().await;
                assert_eq!(
                    state
                        .sessions
                        .session(&owner)
                        .expect("owner survives")
                        .window()
                        .id(),
                    owner_expected,
                    "owner target={target_index}, renumber={renumber}, peer active={peer_target_active}"
                );
                assert_eq!(
                    state
                        .sessions
                        .session(&peer)
                        .expect("peer survives")
                        .window()
                        .id(),
                    peer_expected,
                    "peer target={target_index}, renumber={renumber}, active={peer_target_active}"
                );
            }
        }
    }
}

#[tokio::test]
async fn link_window_refreshes_attached_non_syntactic_group_peer_output_receiver() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "linked-refresh-owner").await;
    let peer = create_grouped_session(&handler, "linked-refresh-peer", &owner).await;
    let source = create_session(&handler, "linked-refresh-source").await;

    let source_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&source)
            .expect("source session exists")
            .window_at(0)
            .expect("source window exists")
            .active_pane()
            .expect("source active pane exists")
            .id()
    };

    let mut control_rx = handler.attach_client(42, &peer).await;
    drain_attach_controls(&mut control_rx).await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(source.clone(), 0),
                WindowTarget::with_window(owner, 0),
            ))
        },
    )
    .await;

    let control = timeout(Duration::from_secs(2), control_rx.recv())
        .await
        .expect("attached group peer must be refreshed after link-window")
        .expect("attached group peer control channel remains open");
    let AttachControl::Switch(target) = control else {
        panic!("expected attached group peer switch, got {control:?}");
    };
    let mut target = target.into_target();
    assert_eq!(target.session_name, peer);

    let output = {
        let state = handler.state.lock().await;
        let peer_pane_id = state
            .sessions
            .session(&peer)
            .expect("peer session exists")
            .window_at(0)
            .expect("peer window exists")
            .active_pane()
            .expect("peer active pane exists")
            .id();
        assert_eq!(peer_pane_id, source_pane_id);
        state
            .pane_output_for_target(&peer, 0, 0)
            .expect("linked peer output exists")
            .clone()
    };
    let expected = b"linked-peer-live-output".to_vec();
    output.send(expected.clone());
    assert_pane_output_observes(&mut target.pane_output, &expected).await;
}

#[tokio::test]
async fn scrollbar_options_resize_shared_runtime_and_refresh_linked_alias() {
    for kind in ["typed", "named", "sdk"] {
        let handler = RequestHandler::new();
        let suffix = kind;
        let owner = create_session(&handler, format!("scrollbar-option-owner-{suffix}")).await;
        let alias = create_session(&handler, format!("scrollbar-option-alias-{suffix}")).await;

        TestRequest::send_ok(
            &handler,
            LinkWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(owner.clone(), 0),
                    WindowTarget::with_window(alias.clone(), 0),
                ))
            },
        )
        .await;
        if kind == "sdk" {
            handler
                .set_option(
                    ScopeSelector::Window(WindowTarget::with_window(owner.clone(), 0)),
                    OptionName::PaneScrollbars,
                    "on",
                )
                .await;
        }

        let requester_pid = match kind {
            "typed" => 44,
            "named" => 45,
            "sdk" => 46,
            _ => unreachable!(),
        };
        let mut control_rx = handler.attach_client(requester_pid, &alias).await;
        drain_attach_controls(&mut control_rx).await;

        let request = match kind {
            "named" => Request::SetOptionByName(Box::new(SetOptionByNameRequest::fixture((
                OptionScopeSelector::Window(WindowTarget::with_window(owner.clone(), 0)),
                "pane-scrollbars",
                "on",
            )))),
            "sdk" => Request::PaneOptionSet(PaneOptionSetRequest {
                target: PaneTargetRef::slot(PaneTarget::with_window(owner.clone(), 0, 0)),
                name: "pane-scrollbars-style".to_owned(),
                value: Some("width=2,pad=1".to_owned()),
                mode: SetOptionMode::Replace,
                unset: false,
            }),
            "typed" => Request::SetOption(SetOptionRequest::fixture((
                ScopeSelector::Window(WindowTarget::with_window(owner.clone(), 0)),
                OptionName::PaneScrollbars,
                "on",
            ))),
            _ => unreachable!(),
        };
        let response = handler.handle(request).await;

        assert!(
            matches!(
                response,
                Response::SetOption(_) | Response::SetOptionByName(_) | Response::PaneOptionSet(_)
            ),
            "{suffix}: {response:?}"
        );
        let control = timeout(Duration::from_secs(2), control_rx.recv())
            .await
            .expect("linked alias must be refreshed after scrollbar geometry changes")
            .expect("linked alias control channel remains open");
        assert!(
            matches!(control, AttachControl::Refresh | AttachControl::Switch(_)),
            "{suffix}: unexpected linked alias control: {control:?}"
        );
        let state = handler.state.lock().await;
        let resolved = if kind == "sdk" {
            state
                .options
                .resolve_for_pane(&alias, 0, 0, OptionName::PaneScrollbarsStyle)
        } else {
            state
                .options
                .resolve_for_window(&alias, 0, OptionName::PaneScrollbars)
        };
        assert_eq!(
            resolved,
            Some(if kind == "sdk" { "width=2,pad=1" } else { "on" }),
            "{suffix}"
        );
        let owner_size = state
            .pane_terminal_size(&owner, 0, 0)
            .expect("owner pane size");
        let alias_size = state
            .pane_terminal_size(&alias, 0, 0)
            .expect("alias pane size");
        assert_eq!(
            (owner_size.cols, alias_size.cols),
            if kind == "sdk" {
                (117, 117)
            } else {
                (119, 119)
            },
            "{suffix}: both aliases expose the resized shared PTY"
        );
    }
}

#[tokio::test]
async fn link_window_k_rejects_same_window_identity_through_group_peer_atomically() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "link-self-owner").await;
    let peer = create_grouped_session(&handler, "link-self-peer", &owner).await;
    let external = create_session(&handler, "link-self-external").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )),
    )
    .await;

    let (before_sessions, before_targets, stable_window_id) = {
        let state = handler.state.lock().await;
        let before_sessions = [&owner, &peer, &external]
            .into_iter()
            .map(|session_name| {
                state
                    .sessions
                    .session(session_name)
                    .expect("session exists before rejected replacement")
                    .clone()
            })
            .collect::<Vec<_>>();
        let before_targets = state.window_linked_window_targets(&owner, 0);
        let stable_window_id = state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .expect("runtime owner window exists")
            .id();
        assert_eq!(
            state
                .sessions
                .session(&peer)
                .and_then(|session| session.window_at(0))
                .expect("group peer window exists")
                .id(),
            stable_window_id
        );
        assert_eq!(
            state
                .sessions
                .session(&external)
                .and_then(|session| session.window_at(1))
                .expect("external linked window exists")
                .id(),
            stable_window_id
        );
        (before_sessions, before_targets, stable_window_id)
    };

    for target in [
        PaneTarget::with_window(owner.clone(), 0, 0),
        PaneTarget::with_window(peer.clone(), 0, 0),
        PaneTarget::with_window(external.clone(), 1, 0),
    ] {
        assert_send_keys_succeeds(&handler, target).await;
    }

    for source in [
        WindowTarget::with_window(peer.clone(), 0),
        WindowTarget::with_window(external.clone(), 1),
    ] {
        let response = handler
            .handle(Request::LinkWindow(LinkWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((&source, WindowTarget::with_window(owner.clone(), 0)))
            }))
            .await;
        assert!(
            matches!(response, Response::Error(_)),
            "same-WindowId replacement from {source} must fail atomically, got {response:?}"
        );
    }

    {
        let state = handler.state.lock().await;
        let after_sessions = [&owner, &peer, &external]
            .into_iter()
            .map(|session_name| {
                state
                    .sessions
                    .session(session_name)
                    .expect("session survives rejected replacement")
                    .clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(after_sessions, before_sessions);
        assert_eq!(
            state.window_linked_window_targets(&owner, 0),
            before_targets
        );
        assert_eq!(state.window_link_count(&owner, 0), 2);
        for target in [
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(peer.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        ] {
            assert_eq!(
                state
                    .sessions
                    .session(target.session_name())
                    .and_then(|session| session.window_at(target.window_index()))
                    .expect("all aliases survive rejected replacement")
                    .id(),
                stable_window_id
            );
        }
    }

    for target in [
        PaneTarget::with_window(owner, 0, 0),
        PaneTarget::with_window(peer, 0, 0),
        PaneTarget::with_window(external, 1, 0),
    ] {
        assert_send_keys_succeeds(&handler, target).await;
    }
}

#[tokio::test]
async fn link_window_k_between_distinct_grouped_window_ids_remains_supported() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "link-distinct-owner").await;
    let peer = create_grouped_session(&handler, "link-distinct-peer", &owner).await;
    handler
        .create_window(NewWindowRequest {
            command: Some(quiet_command()),
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;

    let source_window_id = {
        let state = handler.state.lock().await;
        let destination_window_id = state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .expect("destination window exists")
            .id();
        let source_window_id = state
            .sessions
            .session(&peer)
            .and_then(|session| session.window_at(1))
            .expect("grouped source window exists")
            .id();
        assert_ne!(source_window_id, destination_window_id);
        source_window_id
    };

    // Distinct grouped WindowIds must remain replaceable.
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(peer.clone(), 1),
                WindowTarget::with_window(owner.clone(), 0),
            ))
        },
    )
    .await;

    {
        let state = handler.state.lock().await;
        for target in [
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(peer.clone(), 0),
            WindowTarget::with_window(owner.clone(), 1),
            WindowTarget::with_window(peer.clone(), 1),
        ] {
            assert_eq!(
                state
                    .sessions
                    .session(target.session_name())
                    .and_then(|session| session.window_at(target.window_index()))
                    .expect("linked grouped alias exists")
                    .id(),
                source_window_id
            );
            state
                .pane_profile_in_window(target.session_name(), target.window_index(), 0)
                .expect("linked grouped alias keeps runtime access");
        }
    }
    for target in [
        PaneTarget::with_window(owner.clone(), 0, 0),
        PaneTarget::with_window(peer.clone(), 0, 0),
        PaneTarget::with_window(owner, 1, 0),
        PaneTarget::with_window(peer, 1, 0),
    ] {
        assert_send_keys_succeeds(&handler, target).await;
    }
}

#[tokio::test]
async fn unlink_window_via_group_peer_refreshes_exact_family_and_removes_exact_timers() {
    let handler = RequestHandler::new();
    enable_global_monitor_silence(&handler).await;

    let owner = create_session(&handler, "unlink-refresh-owner").await;
    let peer = create_grouped_session(&handler, "unlink-refresh-peer", &owner).await;
    let external = create_session(&handler, "unlink-refresh-external").await;
    handler
        .create_window(NewWindowRequest {
            command: Some(quiet_command()),
            target_window_index: Some(1),
            ..Fixture::fixture(&owner)
        })
        .await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )),
    )
    .await;

    let mut control_rx = handler.attach_client(43, &owner).await;
    drain_attach_controls(&mut control_rx).await;

    let removed_targets = [
        WindowTarget::with_window(owner.clone(), 0),
        WindowTarget::with_window(peer.clone(), 0),
    ];
    let preserved_targets = [
        WindowTarget::with_window(owner.clone(), 1),
        WindowTarget::with_window(peer.clone(), 1),
        WindowTarget::with_window(external.clone(), 0),
        WindowTarget::with_window(external.clone(), 1),
    ];
    for target in &removed_targets {
        assert!(
            handler.silence_timer_snapshot_for_test(target).is_some(),
            "removed alias starts with a silence timer: {target}"
        );
    }
    let preserved_timer_snapshots = preserved_targets
        .iter()
        .map(|target| handler.silence_timer_snapshot_for_test(target))
        .collect::<Vec<_>>();

    let unlinked = TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(peer.clone(), 0),
            kill_if_last: false,
        },
    )
    .await;
    assert_eq!(unlinked.target, WindowTarget::with_window(peer.clone(), 1));

    let control = timeout(Duration::from_secs(2), control_rx.recv())
        .await
        .expect("non-syntactic owner attach must be refreshed after unlink-window")
        .expect("owner attach control channel remains open");
    let AttachControl::Switch(target) = control else {
        panic!("expected refreshed owner switch, got {control:?}");
    };
    let mut target = target.into_target();
    assert_eq!(target.session_name, owner);

    let output = {
        let state = handler.state.lock().await;
        assert!(state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .is_none());
        assert!(state
            .sessions
            .session(&peer)
            .and_then(|session| session.window_at(0))
            .is_none());
        assert!(
            state
                .sessions
                .session(&external)
                .and_then(|session| session.window_at(1))
                .is_some(),
            "external linked alias survives grouped peer unlink"
        );
        state
            .pane_output_for_target(&owner, 1, 0)
            .expect("owner survivor output exists")
            .clone()
    };
    let expected = b"unlink-peer-live-output".to_vec();
    output.send(expected.clone());
    assert_pane_output_observes(&mut target.pane_output, &expected).await;

    for target in &removed_targets {
        assert_eq!(
            handler.silence_timer_snapshot_for_test(target),
            None,
            "unlink-window removes the vanished alias timer: {target}"
        );
    }
    for (target, snapshot) in preserved_targets.iter().zip(preserved_timer_snapshots) {
        assert_eq!(
            handler.silence_timer_snapshot_for_test(target),
            snapshot,
            "unlink-window must not postpone surviving or unrelated timer {target}"
        );
    }
}

#[tokio::test]
async fn link_window_shares_runtime_tracks_linked_sessions_and_unlinks_cleanly() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;

    let linked = TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;
    assert_eq!(linked.target, WindowTarget::with_window(beta.clone(), 1));

    {
        let state = handler.state.lock().await;
        let alpha_window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("alpha window 0 should exist");
        let beta_window = state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(1))
            .expect("beta window 1 should exist");

        assert_eq!(alpha_window.id(), beta_window.id());
        assert_eq!(state.window_link_count(&alpha, 0), 2);
        assert_eq!(state.window_linked_session_count(&alpha, 0), 2);
        assert_eq!(
            state.window_linked_sessions_list(&alpha, 0),
            vec![alpha.clone(), beta.clone()]
        );
        assert!(
            state.pane_profile_in_window(&beta, 1, 0).is_ok(),
            "linked target should resolve pane runtime through the shared terminal owner"
        );
    }

    let linked_formats = handler
        .display_print(
            WindowTarget::with_window(alpha.clone(), 0),
            "#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        )
        .await;
    assert_eq!(String::from_utf8_lossy(&linked_formats), "1:2:alpha,beta\n");

    let renamed = TestRequest::send_ok(
        &handler,
        RenameWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 1),
            name: "logs".to_owned(),
        },
    )
    .await;
    assert_eq!(renamed.target, WindowTarget::with_window(beta.clone(), 1));

    {
        let state = handler.state.lock().await;
        let alpha_window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("alpha window 0 should exist after rename");
        let beta_window = state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(1))
            .expect("beta window 1 should exist after rename");

        assert_eq!(alpha_window.name(), Some("logs"));
        assert_eq!(beta_window.name(), Some("logs"));
    }

    let unlinked = TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 1),
            kill_if_last: false,
        },
    )
    .await;
    assert_eq!(unlinked.target, WindowTarget::with_window(beta.clone(), 0));

    let state = handler.state.lock().await;
    assert_eq!(state.window_link_count(&alpha, 0), 1);
    assert_eq!(state.window_linked_session_count(&alpha, 0), 1);
    assert_eq!(
        state.window_linked_sessions_list(&alpha, 0),
        vec![alpha.clone()]
    );
    assert!(
        state
            .sessions
            .session(&beta)
            .and_then(|session| session.window_at(1))
            .is_none(),
        "unlink-window should remove the target slot from beta"
    );
    assert!(
        state.pane_profile_in_window(&beta, 1, 0).is_err(),
        "unlinked target slot should no longer resolve pane runtime"
    );
}

#[tokio::test]
async fn linked_session_formats_include_session_group_peers() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    create_grouped_session(&handler, "beta", &alpha).await;
    let gamma = create_session(&handler, "gamma").await;
    create_grouped_session(&handler, "delta", &gamma).await;

    let linked = TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(gamma.clone(), 1),
            ))
        },
    )
    .await;
    assert_eq!(linked.target, WindowTarget::with_window(gamma, 1));

    let linked_formats = handler
        .display_print(
            WindowTarget::with_window(alpha, 0),
            "#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        )
        .await;

    assert_eq!(
        String::from_utf8_lossy(&linked_formats),
        "1:4:alpha,beta,gamma,delta\n"
    );
}

#[tokio::test]
async fn linked_windows_survive_runtime_owner_session_rename() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    let gamma = session_name("gamma");

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: alpha,
            new_name: gamma.clone(),
        },
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&gamma, 0), 2);
        assert_eq!(state.window_link_count(&beta, 1), 2);
        assert_eq!(
            state.window_linked_sessions_list(&beta, 1),
            vec![gamma.clone(), beta.clone()]
        );
        assert!(
            state.pane_profile_in_window(&beta, 1, 0).is_ok(),
            "linked target should still resolve through renamed runtime owner"
        );
    }

    // Linked list-panes must survive the owner rename.
    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: beta,
            target_window_index: Some(1),
            format: Some("#{session_name}:#{window_index}:#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(String::from_utf8_lossy(list.output.stdout()), "beta:1:0\n");
}

#[tokio::test]
async fn link_window_relative_same_destination_slot_makes_room_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;

    let source_pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .expect("alpha should exist")
            .pane_id_in_window(1, 0)
            .expect("source pane should exist")
    };

    let response = TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            after: true,
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 1),
                WindowTarget::with_window(alpha.clone(), 0),
            ))
        },
    )
    .await;

    assert_eq!(
        response,
        LinkWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 1),
        }
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(session.pane_id_in_window(1, 0), Some(source_pane_id));
    assert_eq!(session.pane_id_in_window(2, 0), Some(source_pane_id));
    assert_eq!(state.window_link_count(&alpha, 1), 2);
    assert_eq!(state.window_link_count(&alpha, 2), 2);
}

#[tokio::test]
async fn linked_windows_survive_runtime_owner_session_removal_after_rename() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    let gamma = session_name("gamma");

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: alpha,
            new_name: gamma.clone(),
        },
    )
    .await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&gamma)).await;

    {
        let state = handler.state.lock().await;
        assert!(
            state.sessions.session(&gamma).is_none(),
            "runtime owner session should be removed"
        );
        assert_eq!(state.window_link_count(&beta, 1), 1);
        assert_eq!(
            state.window_linked_sessions_list(&beta, 1),
            vec![beta.clone()]
        );
        assert!(
            state.pane_profile_in_window(&beta, 1, 0).is_ok(),
            "surviving linked target should adopt the removed owner's pane runtime"
        );
    }

    // Linked list-panes must survive the owner removal.
    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: beta,
            target_window_index: Some(1),
            format: Some("#{session_name}:#{window_index}:#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(String::from_utf8_lossy(list.output.stdout()), "beta:1:0\n");
}

#[tokio::test]
async fn unlink_window_runtime_owner_transfers_runtime_to_surviving_alias() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "unlink-runtime-owner").await;
    insert_window(&handler, &owner, 1).await;
    let external = create_session(&handler, "unlink-runtime-external").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(owner.clone(), 0),
            kill_if_last: false,
        },
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&external, 1), 1);
        state
            .pane_profile_in_window(&external, 1, 0)
            .expect("surviving external alias adopts the detached owner's runtime");
        state
            .pane_profile_in_window(&owner, 1, 0)
            .expect("the owner's unrelated window keeps its runtime");
    }
    assert_send_keys_succeeds(&handler, PaneTarget::with_window(external, 1, 0)).await;
}

#[tokio::test]
async fn link_window_k_runtime_owner_transfers_replaced_runtime_to_surviving_alias() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "link-k-runtime-owner").await;
    insert_window(&handler, &owner, 1).await;
    let external = create_session(&handler, "link-k-runtime-external").await;
    let replacement = create_session(&handler, "link-k-runtime-replacement").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(replacement.clone(), 0),
                WindowTarget::with_window(owner.clone(), 0),
            ))
        },
    )
    .await;

    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&external, 1), 1);
        state
            .pane_profile_in_window(&external, 1, 0)
            .expect("surviving alias adopts the replaced runtime");
        state
            .pane_profile_in_window(&owner, 0, 0)
            .expect("replacement target resolves its new linked runtime");
    }
    assert_send_keys_succeeds(&handler, PaneTarget::with_window(external, 1, 0)).await;
    assert_send_keys_succeeds(&handler, PaneTarget::with_window(owner, 0, 0)).await;
}

#[tokio::test]
async fn killing_grouped_runtime_owner_preserves_external_linked_alias() {
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "group-kill-runtime-owner").await;
    let peer = create_grouped_session(&handler, "group-kill-runtime-peer", &owner).await;
    let external = create_session(&handler, "group-kill-runtime-external").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(external.clone(), 1),
        )),
    )
    .await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture(&owner)).await;

    {
        let state = handler.state.lock().await;
        assert!(state.sessions.session(&owner).is_none());
        assert_eq!(state.window_link_count(&peer, 0), 2);
        assert_eq!(state.window_link_count(&external, 1), 2);
        assert_eq!(
            state.window_linked_sessions_list(&external, 1),
            vec![peer.clone(), external.clone()],
            "external alias metadata must be rekeyed from the removed owner to its peer"
        );
        state
            .pane_profile_in_window(&peer, 0, 0)
            .expect("group peer keeps the transferred runtime");
        state
            .pane_profile_in_window(&external, 1, 0)
            .expect("external alias follows the transferred group runtime");
    }
    assert_send_keys_succeeds(&handler, PaneTarget::with_window(peer, 0, 0)).await;
    assert_send_keys_succeeds(&handler, PaneTarget::with_window(external, 1, 0)).await;
}

#[tokio::test]
async fn link_window_shares_pane_base_index_with_linked_slots() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::PaneBaseIndex,
            "1",
        )
        .await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;

    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: beta.clone(),
            target_window_index: Some(1),
            format: Some("#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(list.output.stdout()),
        "1\n2\n",
        "linked windows should render the source pane-base-index"
    );

    let resolved = handler
        .handle(Request::ResolveTarget(ResolveTargetRequest {
            target: Some("beta:1.1".to_owned()),
            target_type: ResolveTargetType::Pane,
            window_index: false,
            prefer_unattached: false,
        }))
        .await;
    let Response::ResolveTarget(resolved) = resolved else {
        panic!("linked visible pane target should resolve, got {resolved:?}");
    };
    assert_eq!(
        resolved.target,
        Target::Pane(PaneTarget::with_window(beta, 1, 0))
    );
}

#[tokio::test]
async fn linked_window_id_resolution_prefers_current_session_slot() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(beta.clone(), 1),
        )),
    )
    .await;

    let window_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("linked source window exists")
            .id()
            .to_string()
    };

    let resolved = handler
        .handle(Request::ResolveTarget(ResolveTargetRequest {
            target: Some(window_id),
            target_type: ResolveTargetType::Window,
            window_index: false,
            prefer_unattached: false,
        }))
        .await;
    let Response::ResolveTarget(resolved) = resolved else {
        panic!("linked window id should resolve through preferred session, got {resolved:?}");
    };
    assert_eq!(
        resolved.target,
        Target::Window(WindowTarget::with_window(beta, 1))
    );
}

#[tokio::test]
async fn unlink_window_kill_if_last_deletes_an_unshared_window_slot() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;

    let unlinked = TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
            kill_if_last: true,
        },
    )
    .await;
    assert_eq!(
        unlinked.target,
        WindowTarget::with_window(alpha.clone(), 0),
        "expected unlink-window -k to remove the unshared slot"
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert!(
        session.window_at(1).is_none(),
        "unlink-window -k should delete the unshared destination window"
    );
    assert_eq!(session.active_window_index(), 0);
}

#[tokio::test]
async fn unlink_only_linked_window_destroys_the_empty_session() {
    // Frozen tmux 3.7b, measured on 2026-07-26: unlinking a session's only
    // window removes that session when the window survives through another
    // link.
    let handler = RequestHandler::new();
    let owner = create_session(&handler, "unlink-only-window-owner").await;
    let alias = create_session(&handler, "unlink-only-window-alias").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(alias.clone(), 9),
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        KillWindowRequest::fixture(WindowTarget::with_window(alias.clone(), 0)),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(alias.clone(), 9),
            kill_if_last: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert!(
        state.sessions.session(&alias).is_none(),
        "the empty alias session must be destroyed"
    );
    assert!(
        state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .is_some(),
        "the linked owner window must survive"
    );
}

#[tokio::test]
async fn unlink_only_linked_window_preserves_a_concurrently_added_window() {
    let handler = std::sync::Arc::new(RequestHandler::new());
    let owner = create_session(&handler, "unlink-race-owner").await;
    let alias = create_session(&handler, "unlink-race-alias").await;

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(owner.clone(), 0),
            WindowTarget::with_window(alias.clone(), 9),
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        KillWindowRequest::fixture(WindowTarget::with_window(alias.clone(), 0)),
    )
    .await;

    let pause = handler.install_kill_session_selection_identity_pause(alias.clone());
    let unlink_handler = std::sync::Arc::clone(&handler);
    let unlink_alias = alias.clone();
    let unlinking = tokio::spawn(async move {
        unlink_handler
            .handle(Request::UnlinkWindow(UnlinkWindowRequest {
                target: WindowTarget::with_window(unlink_alias, 9),
                kill_if_last: false,
            }))
            .await
    });
    timeout(Duration::from_secs(1), pause.reached.notified())
        .await
        .expect("conditional session removal reaches the identity pause");

    handler
        .create_window(NewWindowRequest {
            command: Some(quiet_command()),
            target_window_index: Some(10),
            ..Fixture::fixture(&alias)
        })
        .await;
    pause.release.notify_one();

    let unlinked = timeout(Duration::from_secs(2), unlinking)
        .await
        .expect("unlink-window must finish")
        .expect("unlink-window task joins");
    assert!(
        matches!(unlinked, Response::UnlinkWindow(_)),
        "{unlinked:?}"
    );

    let state = handler.state.lock().await;
    let alias_session = state
        .sessions
        .session(&alias)
        .expect("the concurrently extended session survives");
    assert!(alias_session.window_at(9).is_none());
    assert!(alias_session.window_at(10).is_some());
    assert!(
        state
            .sessions
            .session(&owner)
            .and_then(|session| session.window_at(0))
            .is_some(),
        "the linked owner window survives"
    );
}

#[tokio::test]
async fn unlink_window_kill_if_last_rekeys_renumbered_silence_timers_without_delay() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "unlink-renumber-timers").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;
    let unrelated = create_session(&handler, "unlink-renumber-unrelated").await;

    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::RenumberWindows,
            "on",
        )
        .await;
    enable_global_monitor_silence(&handler).await;

    let targets = [
        WindowTarget::with_window(alpha.clone(), 0),
        WindowTarget::with_window(alpha.clone(), 1),
        WindowTarget::with_window(alpha.clone(), 2),
    ];
    let snapshots = targets.clone().map(|target| {
        handler
            .silence_timer_snapshot_for_test(&target)
            .expect("each window starts with an armed silence timer")
    });
    let unrelated_target = WindowTarget::with_window(unrelated, 0);
    let unrelated_snapshot = handler
        .silence_timer_snapshot_for_test(&unrelated_target)
        .expect("unrelated session timer starts armed");
    let surviving_window_ids = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha exists");
        [
            session.window_at(1).expect("window one exists").id(),
            session.window_at(2).expect("window two exists").id(),
        ]
    };

    let unlinked = TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: targets[0].clone(),
            kill_if_last: true,
        },
    )
    .await;
    assert_eq!(
        unlinked.target,
        WindowTarget::with_window(alpha.clone(), 1),
        "expected unlink-window -k success with renumbering"
    );

    {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha survives");
        assert_eq!(
            session.window_at(0).expect("old window one moved").id(),
            surviving_window_ids[0]
        );
        assert_eq!(
            session.window_at(1).expect("old window two moved").id(),
            surviving_window_ids[1]
        );
        assert!(session.window_at(2).is_none());
    }
    assert_eq!(
        handler
            .silence_timer_snapshot_for_test(&targets[0])
            .expect("old window one timer moved to zero")
            .1,
        snapshots[1].1,
        "renumbering must preserve old window one's absolute silence deadline"
    );
    assert_eq!(
        handler
            .silence_timer_snapshot_for_test(&targets[1])
            .expect("old window two timer moved to one")
            .1,
        snapshots[2].1,
        "renumbering must preserve old window two's absolute silence deadline"
    );
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&targets[2]),
        None,
        "the stale pre-renumber timer key must be removed"
    );
    assert_eq!(
        handler.silence_timer_snapshot_for_test(&unrelated_target),
        Some(unrelated_snapshot),
        "unrelated session timer must remain untouched"
    );
}

#[tokio::test]
async fn unlink_window_restores_previous_last_window_flag_after_active_link_removal() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;
    insert_window(&handler, &alpha, 2).await;

    for window_index in [1, 0] {
        TestRequest::send_ok(
            &handler,
            SelectWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), window_index),
            },
        )
        .await;
    }

    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(alpha.clone(), 9),
            ))
        },
    )
    .await;
    {
        let state = handler.state.lock().await;
        assert_eq!(state.window_link_count(&alpha, 0), 2);
        assert_eq!(state.window_linked_session_count(&alpha, 0), 1);
        assert_eq!(
            state.window_linked_sessions_list(&alpha, 0),
            vec![alpha.clone()]
        );
    }
    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 9),
            kill_if_last: true,
        },
    )
    .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(session.active_window_index(), 0);
    assert_eq!(session.last_window_index(), Some(1));
}
