use super::*;

#[tokio::test]
async fn resize_window_applies_explicit_dimensions() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    let resized = handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: Some(60),
            height: Some(20),
            adjustment: None,
        })
        .await;
    assert_eq!(resized.target, WindowTarget::with_window(alpha.clone(), 0));

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    let window = session.window_at(0).expect("window 0 should exist");
    assert_eq!(window.size().cols, 60);
    assert_eq!(window.size().rows, 20);
}

#[tokio::test]
async fn resize_window_applies_relative_adjustment() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    // Session created with cols=120, rows=40. Shrink by 10 cols.
    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: None,
            height: None,
            adjustment: Some(ResizeWindowAdjustment::Left(10)),
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    let window = session.window_at(0).expect("window 0 should exist");
    assert_eq!(window.size().cols, 110);
    assert_eq!(window.size().rows, 40);
}

#[tokio::test]
async fn resize_window_applies_adjustment_after_explicit_dimensions() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: Some(60),
            height: Some(20),
            adjustment: Some(ResizeWindowAdjustment::Down(5)),
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    let window = session.window_at(0).expect("window 0 should exist");
    assert_eq!(window.size().cols, 60);
    assert_eq!(window.size().rows, 25);
}

#[tokio::test]
async fn resize_window_largest_smallest_without_attached_clients_use_target_session_size() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_session(("alpha", TerminalSize::new(120, 40)))
        .await;
    let beta = handler
        .create_session(("beta", TerminalSize::new(80, 24)))
        .await;

    handler
        .handle_ok(LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        })
        .await;

    for (target, expected) in [
        (
            WindowTarget::with_window(alpha.clone(), 0),
            TerminalSize::new(120, 40),
        ),
        (
            WindowTarget::with_window(beta.clone(), 1),
            TerminalSize::new(80, 24),
        ),
    ] {
        for adjustment in [
            ResizeWindowAdjustment::LargestLinkedSession,
            ResizeWindowAdjustment::SmallestLinkedSession,
        ] {
            handler
                .handle_ok(ResizeWindowRequest {
                    target: target.clone(),
                    width: Some(70),
                    height: Some(20),
                    adjustment: None,
                })
                .await;
            handler
                .handle_ok(ResizeWindowRequest {
                    target: target.clone(),
                    width: None,
                    height: None,
                    adjustment: Some(adjustment),
                })
                .await;

            let state = handler.state.lock().await;
            let window = state
                .sessions
                .session(target.session_name())
                .and_then(|session| session.window_at(target.window_index()))
                .expect("window exists");
            assert_eq!(window.size(), expected);
        }
    }
}

#[tokio::test]
async fn resize_window_updates_linked_slots_and_refreshes_linked_sessions() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_session(("alpha", TerminalSize::new(80, 24)))
        .await;
    let beta = handler
        .create_session(("beta", TerminalSize::new(120, 40)))
        .await;

    handler
        .handle_ok(LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        })
        .await;
    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(beta.clone(), 1),
        })
        .await;

    let mut control_rx = handler.attach_client(42, &beta).await;
    drain_attach_controls(&mut control_rx).await;

    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: Some(70),
            height: Some(20),
            adjustment: None,
        })
        .await;

    {
        let state = handler.state.lock().await;
        for (session_name, window_index, expected) in [
            (&alpha, 0, TerminalSize::new(70, 20)),
            (&beta, 0, TerminalSize::new(120, 40)),
            (&beta, 1, TerminalSize::new(70, 20)),
        ] {
            let window = state
                .sessions
                .session(session_name)
                .and_then(|session| session.window_at(window_index))
                .expect("window exists");
            assert_eq!(window.size(), expected);
        }
    }

    let refresh = timeout(Duration::from_secs(2), control_rx.recv())
        .await
        .expect("linked session should receive a refresh")
        .expect("refresh channel should remain open");
    assert_refresh(refresh);
}

#[tokio::test]
async fn resize_window_propagates_linked_slots_to_their_session_group_peers() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_grouped_session(&handler, "beta", &alpha).await;
    let gamma = create_session(&handler, "gamma").await;
    let delta = create_grouped_session(&handler, "delta", &gamma).await;

    handler
        .handle_ok(LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(gamma.clone(), 1),
            ))
        })
        .await;
    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: Some(111),
            height: Some(33),
            adjustment: None,
        })
        .await;

    let state = handler.state.lock().await;
    for (session_name, window_index) in [(&alpha, 0), (&beta, 0), (&gamma, 1), (&delta, 1)] {
        let window = state
            .sessions
            .session(session_name)
            .and_then(|session| session.window_at(window_index))
            .expect("linked window should exist");
        assert_eq!(
            window.size(),
            TerminalSize::new(111, 33),
            "{session_name}:{window_index} should reflect linked resize"
        );
    }
}

#[tokio::test]
async fn resize_window_largest_smallest_with_attached_clients_still_use_client_sizes() {
    let handler = RequestHandler::new();
    let alpha = handler
        .create_session(("alpha", TerminalSize::new(120, 40)))
        .await;

    let _control_rx = handler.attach_client(42, &alpha).await;
    {
        let mut active_attach = handler.active_attach.lock().await;
        let active = active_attach
            .by_pid
            .get_mut(&42)
            .expect("registered attach must exist");
        active.set_declared_client_size(TerminalSize::new(100, 30));
    }

    for adjustment in [
        ResizeWindowAdjustment::LargestLinkedSession,
        ResizeWindowAdjustment::SmallestLinkedSession,
    ] {
        handler
            .handle_ok(ResizeWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), 0),
                width: Some(70),
                height: Some(20),
                adjustment: None,
            })
            .await;
        handler
            .handle_ok(ResizeWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), 0),
                width: None,
                height: None,
                adjustment: Some(adjustment),
            })
            .await;

        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("window exists");
        // The client owns an outer 100x30 terminal and `status` defaults to
        // `on`, so the window content is 100x29. tmux 3.7b measured with a real
        // 100x30 PTY client: `resize-window -A` and `-a` both land on 100x29.
        assert_eq!(window.size(), TerminalSize::new(100, 29));
    }
}

#[tokio::test]
async fn resize_window_clamps_relative_adjustments_to_a_minimum_size_of_one() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            width: Some(2),
            height: Some(3),
            adjustment: Some(ResizeWindowAdjustment::Left(10)),
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    let window = session.window_at(0).expect("window 0 should exist");
    assert_eq!(window.size().cols, 1);
    assert_eq!(window.size().rows, 3);
}

#[tokio::test]
async fn resize_window_keeps_multi_pane_geometry_at_the_tmux_viable_minimum() {
    let handler = RequestHandler::new();

    for (name, initial, direction, expected) in [
        (
            "vertical-minimum",
            TerminalSize::new(10, 3),
            SplitDirection::Vertical,
            TerminalSize::new(1, 3),
        ),
        (
            "horizontal-minimum",
            TerminalSize::new(3, 10),
            SplitDirection::Horizontal,
            TerminalSize::new(3, 1),
        ),
    ] {
        let session_name = handler.create_session((name, initial)).await;

        let split = handler
            .handle(Request::SplitWindow(SplitWindowRequest {
                direction,
                ..Fixture::fixture(&session_name)
            }))
            .await;
        assert!(
            matches!(split, Response::SplitWindow(_)),
            "split must succeed for {name}: {split:?}"
        );

        for _ in 0..2 {
            let response = handler
                .handle(Request::ResizeWindow(ResizeWindowRequest {
                    target: WindowTarget::with_window(session_name.clone(), 0),
                    width: Some(1),
                    height: Some(1),
                    adjustment: None,
                }))
                .await;
            assert!(
                matches!(response, Response::ResizeWindow(_)),
                "resize must succeed for {name}: {response:?}"
            );

            let state = handler.state.lock().await;
            let window = state
                .sessions
                .session(&session_name)
                .and_then(|session| session.window_at(0))
                .expect("window exists");
            assert_eq!(window.size(), expected, "session={name}");
            assert!(
                window.panes().iter().all(|pane| {
                    let geometry = pane.geometry();
                    geometry.cols() >= 1 && geometry.rows() >= 1
                }),
                "session={name} panes={:?}",
                window.panes()
            );
        }
    }
}

#[tokio::test]
async fn select_layout_expands_a_minimum_window_to_the_named_tree_minimum() {
    let handler = RequestHandler::new();
    let session_name = handler
        .create_session(("select-layout-minimum", TerminalSize::new(3, 10)))
        .await;

    handler
        .handle_ok(SplitWindowRequest::fixture(&session_name))
        .await;
    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(session_name.clone(), 0),
            width: Some(3),
            height: Some(1),
            adjustment: None,
        })
        .await;

    for _ in 0..2 {
        let selected = handler
            .handle(Request::SelectLayout(SelectLayoutRequest {
                target: SelectLayoutTarget::Window(WindowTarget::with_window(
                    session_name.clone(),
                    0,
                )),
                layout: LayoutName::EvenVertical,
            }))
            .await;
        assert!(
            matches!(
                selected,
                Response::SelectLayout(response) if response.layout == LayoutName::EvenVertical
            ),
            "select-layout must succeed: {selected:?}"
        );

        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(0))
            .expect("window exists");
        assert_eq!(window.size(), TerminalSize::new(3, 3));
        assert_eq!(
            window
                .panes()
                .iter()
                .map(|pane| pane.geometry())
                .collect::<Vec<_>>(),
            vec![
                rmux_core::PaneGeometry::new(0, 0, 3, 1),
                rmux_core::PaneGeometry::new(0, 2, 3, 1),
            ]
        );
    }
}

#[tokio::test]
async fn resize_window_rejects_nonexistent_window() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    let response = handler
        .handle(Request::ResizeWindow(ResizeWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 99),
            width: Some(40),
            height: Some(20),
            adjustment: None,
        }))
        .await;

    assert!(
        matches!(response, Response::Error(_)),
        "expected error for nonexistent window, got {response:?}"
    );
}

#[tokio::test]
async fn respawn_window_rejects_active_window_without_kill_flag() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    // Window 0 has a running pane — respawn without -k should fail.
    let response = handler
        .handle(Request::RespawnWindow(Box::new(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            kill: false,
            start_directory: None,
            environment: None,
            command: None,
        })))
        .await;

    assert!(
        matches!(&response, Response::Error(e) if e.error.to_string().contains("still active")),
        "expected still-active error, got {response:?}"
    );
}

#[tokio::test]
async fn respawn_window_succeeds_with_kill_flag() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    let respawned = handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;
    assert_eq!(
        respawned.target,
        WindowTarget::with_window(alpha.clone(), 0)
    );

    // After respawn, window should still exist with exactly one pane.
    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    let window = session
        .window_at(0)
        .expect("window 0 should exist after respawn");
    assert_eq!(window.panes().len(), 1);
}

#[tokio::test]
async fn respawn_window_reuses_shell_command_cwd_and_private_environment() {
    let handler = RequestHandler::new();
    // Start directories have to live in the seed this daemon leased: a pane runs in a snapshot of
    // it, so a directory outside is one the daemon genuinely cannot open a job over. The probe's
    // *output* file stays on the host, where the test can read it back — a job writing an
    // absolute path reaches the host directly, which is the documented trust boundary.
    let initial_cwd =
        crate::pane_terminals::seed_scratch_dir(&handler, "respawn-provenance-initial");
    let override_cwd =
        crate::pane_terminals::seed_scratch_dir(&handler, "respawn-provenance-override");
    let output = unique_temp_path("window-respawn-provenance-output");
    let initial_command = window_respawn_replay_command(&output, "initial-command");
    let initial_process_command = ProcessCommand::Shell(initial_command.clone());
    let initial_environment = "RMUX_RESPAWN=initial".to_owned();

    let alpha = handler
        .create_session(NewSessionExtRequest {
            working_directory: Some(initial_cwd.path().to_string_lossy().into_owned()),
            size: Some(WINDOW_TEST_SIZE),
            environment: Some(vec![initial_environment.clone()]),
            process_command: Some(initial_process_command.clone()),
            ..Fixture::fixture("respawn-window-provenance")
        })
        .await;

    let initial_line = (initial_cwd.relative(), "initial", "initial-command");
    wait_for_window_respawn_probe(&output, &[initial_line]).await;
    let pane_id = {
        let state = handler.state.lock().await;
        let pane_id = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.pane_id_in_window(0, 0))
            .expect("initial pane exists");
        let lifecycle = state.pane_lifecycle(pane_id).expect("initial lifecycle");
        assert_eq!(lifecycle.process_command(), Some(&initial_process_command));
        assert_eq!(
            lifecycle.respawn_environment(),
            std::slice::from_ref(&initial_environment)
        );
        pane_id
    };

    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler
        .handle_ok(RespawnWindowRequest {
            target: target.clone(),
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;
    wait_for_window_respawn_probe(&output, &[initial_line, initial_line]).await;

    let override_environment = "RMUX_RESPAWN=override".to_owned();
    let override_command = window_respawn_replay_command(&output, "override-command");
    let override_process_command = ProcessCommand::Shell(override_command.clone());
    handler
        .handle_ok(RespawnWindowRequest {
            target: target.clone(),
            kill: true,
            start_directory: Some(override_cwd.path().to_path_buf()),
            environment: Some(vec![override_environment.clone()]),
            command: Some(vec![override_command]),
        })
        .await;
    let override_line = (override_cwd.relative(), "override", "override-command");
    wait_for_window_respawn_probe(&output, &[initial_line, initial_line, override_line]).await;
    {
        let state = handler.state.lock().await;
        let lifecycle = state.pane_lifecycle(pane_id).expect("override lifecycle");
        assert_eq!(lifecycle.process_command(), Some(&override_process_command));
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&override_environment)
        );
        assert_eq!(
            lifecycle.respawn_environment(),
            std::slice::from_ref(&initial_environment)
        );
    }

    handler
        .handle_ok(RespawnWindowRequest {
            target,
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;
    let inherited_override_line = (override_cwd.relative(), "initial", "override-command");
    wait_for_window_respawn_probe(
        &output,
        &[
            initial_line,
            initial_line,
            override_line,
            inherited_override_line,
        ],
    )
    .await;

    drop(handler);
    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn respawn_window_keeps_the_original_resolved_shell_after_option_changes() {
    let handler = RequestHandler::new();
    let output = unique_temp_path("window-respawn-window-shell-provenance-output");
    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultShell, "/bin/sh")
        .await;
    let alpha = create_session(&handler, "respawn-window-shell-provenance").await;
    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultShell, "/bin/bash")
        .await;

    let target = WindowTarget::with_window(alpha.clone(), 0);
    let shell_command = window_respawn_shell_identity_command(&output, "shell");
    handler
        .handle_ok(RespawnWindowRequest {
            target: target.clone(),
            kill: true,
            start_directory: None,
            environment: None,
            command: Some(vec![shell_command]),
        })
        .await;
    let expected_line = "sh:/bin/sh:shell\n";
    wait_for_file_contents(&output, expected_line).await;

    handler
        .handle_ok(RespawnWindowRequest {
            target,
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;
    wait_for_file_contents(&output, &format!("{expected_line}{expected_line}")).await;

    let state = handler.state.lock().await;
    let pane_id = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.pane_id_in_window(0, 0))
        .expect("respawned window pane exists");
    assert_eq!(
        state
            .pane_lifecycle(pane_id)
            .expect("respawned window lifecycle")
            .respawn_shell(),
        &crate::terminal::PaneShell::External(std::path::PathBuf::from("/bin/sh"))
    );
    drop(state);
    drop(handler);
    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn failed_respawn_window_preserves_the_old_layout_terminal_and_lifecycle() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "respawn-window-rollback").await;
    let (session_before, pane_id, lifecycle_before) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let pane_id = session
            .pane_id_in_window(0, 0)
            .expect("initial pane exists");
        (
            session.clone(),
            pane_id,
            state
                .pane_lifecycle(pane_id)
                .expect("pane lifecycle exists")
                .clone(),
        )
    };

    let response = handler
        .handle(Request::RespawnWindow(Box::new(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            kill: true,
            start_directory: None,
            environment: Some(vec!["INVALID".to_owned()]),
            command: None,
        })))
        .await;
    assert!(matches!(response, Response::Error(_)), "{response:?}");

    let state = handler.state.lock().await;
    assert_eq!(state.sessions.session(&alpha), Some(&session_before));
    state
        .ensure_panes_exist(&alpha, &[pane_id])
        .expect("failed respawn must retain the old terminal");
    assert_eq!(state.pane_lifecycle(pane_id), Some(&lifecycle_before));
}

#[tokio::test]
async fn respawn_window_retains_surviving_pane_lifecycle_counters_and_redacts_env() {
    let handler = RequestHandler::new();
    let initial_secret = "RMUX_WINDOW_INITIAL=alpha-secret".to_owned();
    let split_secret = "RMUX_WINDOW_SPLIT=beta-secret".to_owned();
    let respawn_secret = "RMUX_WINDOW_RESPAWN=gamma-secret".to_owned();
    let respawn_command = crate::test_shell::stdin_discard_command();

    let alpha = handler
        .create_session(NewSessionExtRequest {
            size: Some(WINDOW_TEST_SIZE),
            environment: Some(vec![initial_secret.clone()]),
            ..Fixture::fixture("alpha")
        })
        .await;
    let split_target = handler
        .handle_ok(SplitWindowRequest {
            environment: Some(vec![split_secret.clone()]),
            ..Fixture::fixture(&alpha)
        })
        .await
        .pane;

    let (surviving_pane_id, split_pane_id, previous_generation, previous_revision, previous_output) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let surviving_pane = window.pane(0).expect("surviving pane exists");
        let split_pane = window
            .pane(split_target.pane_index())
            .expect("split pane exists");
        let lifecycle = state
            .pane_lifecycle(surviving_pane.id())
            .expect("surviving lifecycle exists");
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&initial_secret)
        );
        assert_eq!(
            state
                .pane_lifecycle(split_pane.id())
                .expect("split lifecycle exists")
                .private_environment(),
            std::slice::from_ref(&split_secret)
        );
        (
            surviving_pane.id(),
            split_pane.id(),
            lifecycle.generation,
            lifecycle.revision,
            lifecycle.output_sequence,
        )
    };

    let respawned = handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            kill: true,
            start_directory: None,
            environment: Some(vec![respawn_secret.clone()]),
            command: Some(vec![respawn_command.clone()]),
        })
        .await;
    assert_eq!(
        respawned.target,
        WindowTarget::with_window(alpha.clone(), 0)
    );

    let (generation, revision, output_sequence) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window.pane(0).expect("respawned pane exists");
        assert_eq!(window.panes().len(), 1);
        assert_eq!(pane.id(), surviving_pane_id);
        assert!(
            state.pane_lifecycle(split_pane_id).is_none(),
            "respawn-window must remove lifecycle state for panes it destroys"
        );

        let lifecycle = state
            .pane_lifecycle(surviving_pane_id)
            .expect("respawned lifecycle exists");
        assert_eq!(
            lifecycle.command(),
            Some(std::slice::from_ref(&respawn_command))
        );
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&respawn_secret)
        );
        assert!(!lifecycle.private_environment().contains(&initial_secret));
        assert!(!lifecycle.private_environment().contains(&split_secret));
        assert!(lifecycle.generation > previous_generation);
        assert!(lifecycle.revision > previous_revision);
        assert!(lifecycle.output_sequence > previous_output);
        (
            lifecycle.generation,
            lifecycle.revision,
            lifecycle.output_sequence,
        )
    };

    let listed = handler
        .handle_ok(ListPanesRequest {
            target: alpha.clone(),
            target_window_index: Some(0),
            format: Some(
                "#{pane_id}\t#{pane_lifecycle_generation}\t#{pane_lifecycle_revision}\t#{pane_output_sequence}\t#{pane_start_command}".to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await;
    let list_stdout = String::from_utf8(listed.output.stdout).expect("list-panes utf8");
    assert!(list_stdout.contains(&surviving_pane_id.to_string()));
    assert!(list_stdout.contains(&generation.to_string()));
    assert!(list_stdout.contains(&revision.to_string()));
    assert!(list_stdout.contains(&output_sequence.to_string()));
    assert!(!list_stdout.contains(&initial_secret));
    assert!(!list_stdout.contains(&split_secret));
    assert!(!list_stdout.contains(&respawn_secret));

    let windows = handler
        .handle_ok(ListWindowsRequest {
            target: alpha,
            format: Some(
                "#{window_id}\t#{pane_id}\t#{pane_lifecycle_generation}\t#{pane_output_sequence}"
                    .to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await;
    assert_eq!(windows.windows.len(), 1);
    let windows_stdout = String::from_utf8(windows.output.stdout).expect("list-windows utf8");
    assert!(!windows_stdout.contains(&initial_secret));
    assert!(!windows_stdout.contains(&split_secret));
    assert!(!windows_stdout.contains(&respawn_secret));
}

#[tokio::test]
async fn respawn_window_selects_target_window_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 1).await;

    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
        })
        .await;
    handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha should exist");
    assert_eq!(session.active_window_index(), 0);
}
