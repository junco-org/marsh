use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

#[tokio::test]
async fn attached_session_mutations_emit_refresh_switches() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, (&alpha, TerminalSize::new(120, 40))).await;
    let mut control_rx = handler.attach_client(requester_pid, &alpha).await;

    let split = TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            ..Fixture::fixture(&alpha)
        },
    )
    .await;
    assert_eq!(
        split,
        rmux_proto::SplitWindowResponse {
            pane: PaneTarget::new(alpha.clone(), 1),
        }
    );
    let split_frame = recv_render_frame(&mut control_rx, "split refresh").await;
    assert!(split_frame.contains('│'));

    let resized = handler
        .handle(Request::ResizePane(rmux_proto::ResizePaneRequest {
            target: PaneTarget::new(alpha.clone(), 0),
            adjustment: ResizePaneAdjustment::AbsoluteWidth { columns: 34 },
        }))
        .await;
    assert_eq!(
        resized,
        Response::ResizePane(rmux_proto::ResizePaneResponse {
            target: PaneTarget::new(alpha.clone(), 0),
            adjustment: ResizePaneAdjustment::AbsoluteWidth { columns: 34 },
        })
    );
    let resize_frame = recv_render_frame(&mut control_rx, "resize refresh").await;
    assert!(resize_frame.contains('│'));

    let selected_layout = handler
        .handle(Request::SelectLayout(SelectLayoutRequest {
            target: SelectLayoutTarget::Session(alpha.clone()),
            layout: LayoutName::MainVertical,
        }))
        .await;
    assert_eq!(
        selected_layout,
        Response::SelectLayout(rmux_proto::SelectLayoutResponse {
            layout: LayoutName::MainVertical,
        })
    );
    let layout_frame = recv_render_frame(&mut control_rx, "layout refresh").await;
    assert!(layout_frame.contains('│'));

    let selected_pane = TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::new(alpha, 1)),
    )
    .await;
    assert_eq!(
        selected_pane,
        rmux_proto::SelectPaneResponse {
            target: PaneTarget::new(session_name("alpha"), 1),
        }
    );
    let select_frame = recv_render_frame(&mut control_rx, "pane refresh").await;
    assert!(select_frame.contains('│'));
    assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn switch_client_updates_the_tracked_session_for_follow_up_refreshes() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: rmux_proto::SplitDirection::Horizontal,
            ..Fixture::fixture(&beta)
        },
    )
    .await;
    let mut control_rx = handler.attach_client(requester_pid, alpha).await;

    let switched = handler
        .handle(Request::SwitchClient(SwitchClientRequest {
            target: beta.clone(),
        }))
        .await;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );
    let switch_frame = recv_render_frame(&mut control_rx, "switch refresh").await;
    assert!(switch_frame.contains('│'));

    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::PaneActiveBorderStyle,
            "red",
        )
        .await;
    let global_frame = recv_render_frame(&mut control_rx, "global refresh").await;
    assert!(global_frame.contains("\u{1b}[31m"));

    let beta_window = WindowTarget::with_window(beta.clone(), 0);
    handler
        .set_option(
            ScopeSelector::Window(beta_window),
            OptionName::PaneActiveBorderStyle,
            "blue",
        )
        .await;
    let session_frame = recv_render_frame(&mut control_rx, "session refresh").await;
    assert!(session_frame.contains("\u{1b}[34m"));

    let alpha_window = WindowTarget::with_window(session_name("alpha"), 0);
    handler
        .set_option(
            ScopeSelector::Window(alpha_window),
            OptionName::PaneBorderStyle,
            "green",
        )
        .await;
    assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn choose_tree_renders_after_cli_switch_client() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut control_rx = create_attached_session(&handler, requester_pid, &alpha).await;

    SessionSpec::create(&handler, &beta).await;
    drain_attach_controls(&mut control_rx);

    let switched = handler
        .dispatch(
            requester_pid,
            Request::SwitchClient(SwitchClientRequest {
                target: beta.clone(),
            }),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse { session_name: beta })
    );
    let _ = recv_matching_attach_control(&mut control_rx, "switch to beta", |control| {
        matches!(control, AttachControl::Switch(_))
    })
    .await;
    drain_attach_controls(&mut control_rx);

    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02s")
        .await
        .expect("prefix s opens choose-tree after switch-client");

    let overlay = recv_overlay_frame(&mut control_rx, "choose-tree after switch-client").await;
    assert!(
        overlay.contains("sort:") && overlay.contains("alpha") && overlay.contains("beta"),
        "choose-tree should render sessions after CLI switch-client, got: {overlay:?}"
    );
}

#[tokio::test]
async fn terminal_feature_mutations_refresh_attached_targets_with_client_context() {
    let handler = RequestHandler::new();
    let requester_pid = 42;
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach_with_terminal_context(
            requester_pid,
            alpha,
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color")]),
        )
        .await;

    TestRequest::send_ok(
        &handler,
        SetOptionRequest {
            mode: SetOptionMode::Append,
            ..Fixture::fixture((
                ScopeSelector::Global,
                OptionName::TerminalFeatures,
                "xterm*:sync",
            ))
        },
    )
    .await;

    let target = recv_switch_target(&mut control_rx, "terminal feature refresh").await;
    assert!(target.outer_terminal.features_string().contains("sync"));
    assert!(target
        .outer_terminal
        .wrap_render_frame(&target.render_frame)
        .starts_with(b"\x1b[?2026h"));
}

#[tokio::test]
async fn allow_passthrough_mutations_refresh_attached_targets() {
    let handler = RequestHandler::new();
    let requester_pid = 43;
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach_with_terminal_context(
            requester_pid,
            alpha,
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-kitty")]),
        )
        .await;

    handler
        .set_option(ScopeSelector::Global, OptionName::AllowPassthrough, "on")
        .await;

    let target = recv_switch_target(&mut control_rx, "passthrough refresh").await;
    assert!(
        target.kitty_graphics_passthrough,
        "allow-passthrough changes must recompute the attach target gate"
    );
    assert!(
        !target.sixel_passthrough,
        "kitty-only terminals must not enable sixel passthrough"
    );
}

#[tokio::test]
async fn allow_passthrough_enables_sixel_for_sixel_terminals() {
    let handler = RequestHandler::new();
    let requester_pid = 43;
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach_with_terminal_context(
            requester_pid,
            alpha,
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "foot")]),
        )
        .await;

    handler
        .set_option(ScopeSelector::Global, OptionName::AllowPassthrough, "on")
        .await;

    let target = recv_switch_target(&mut control_rx, "passthrough refresh").await;
    assert!(
        target.sixel_passthrough,
        "allow-passthrough should enable sixel passthrough on sixel terminals"
    );
}

#[tokio::test]
async fn allow_passthrough_all_shares_the_active_pane_gate() {
    let handler = RequestHandler::new();
    let requester_pid = 43;
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach_with_terminal_context(
            requester_pid,
            alpha,
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-kitty")]),
        )
        .await;

    handler
        .set_option(ScopeSelector::Global, OptionName::AllowPassthrough, "all")
        .await;

    let target = recv_switch_target(&mut control_rx, "passthrough refresh").await;
    assert!(
        target.kitty_graphics_passthrough,
        "all is accepted and shares the active-pane passthrough path, since RMUX \
         renders the attached pane and has no unattached-pane passthrough to gate"
    );
}

#[tokio::test]
async fn kitty_passthrough_is_disabled_while_active_pane_is_in_copy_mode() {
    let handler = RequestHandler::new();
    let requester_pid = 43;
    let alpha = session_name("alpha");

    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach_with_terminal_context(
            requester_pid,
            alpha.clone(),
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-kitty")]),
        )
        .await;

    handler
        .set_option(ScopeSelector::Global, OptionName::AllowPassthrough, "on")
        .await;
    let target = recv_switch_target(&mut control_rx, "passthrough refresh").await;
    assert!(
        target.kitty_graphics_passthrough,
        "kitty passthrough should be available before modal pane modes"
    );

    TestRequest::send_ok(
        &handler,
        CopyModeRequest::fixture(PaneTarget::new(alpha, 0)),
    )
    .await;

    let target = recv_switch_target(&mut control_rx, "copy-mode refresh").await;
    assert!(
        !target.kitty_graphics_passthrough,
        "modal pane modes must suppress live kitty passthrough"
    );
}

#[tokio::test]
async fn different_requester_pids_can_control_the_sole_active_attach() {
    let handler = RequestHandler::new();
    let owner_pid = 101;
    let intruder_pid = 202;
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    let mut control_rx = handler.attach_client(owner_pid, &alpha).await;

    let switched = handler
        .dispatch(
            intruder_pid,
            Request::SwitchClient(SwitchClientRequest {
                target: beta.clone(),
            }),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );
    let _ = recv_matching_attach_control(&mut control_rx, "switch refresh", |control| {
        matches!(control, AttachControl::Switch(_))
    })
    .await;

    let detached = handler
        .dispatch(
            intruder_pid,
            Request::DetachClient(rmux_proto::DetachClientRequest),
        )
        .await
        .response;
    assert_eq!(
        detached,
        Response::DetachClient(rmux_proto::DetachClientResponse)
    );
    let _ = recv_matching_attach_control(&mut control_rx, "detach control", |control| {
        matches!(control, AttachControl::Detach)
    })
    .await;
}

#[tokio::test]
async fn rename_session_preserves_ambiguity_rules_for_switch_and_detach() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let gamma = session_name("gamma");

    for session in [&alpha, &beta] {
        SessionSpec::create(&handler, session).await;
    }

    let _first_rx = handler.attach_client(101, &alpha).await;
    let _second_rx = handler.attach_client(202, &alpha).await;

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: alpha,
            new_name: gamma,
        },
    )
    .await;

    let switched = handler
        .dispatch(
            303,
            Request::SwitchClient(SwitchClientRequest {
                target: beta.clone(),
            }),
        )
        .await
        .response;
    assert_eq!(
        switched,
        Response::Error(ErrorResponse {
            error: RmuxError::Server(
                "switch-client requires an unambiguous attached client".to_owned(),
            ),
        })
    );

    let detached = handler
        .dispatch(303, Request::DetachClient(DetachClientRequest))
        .await
        .response;
    assert_eq!(
        detached,
        Response::Error(ErrorResponse {
            error: RmuxError::Server(
                "detach-client requires an unambiguous attached client".to_owned(),
            ),
        })
    );
}
