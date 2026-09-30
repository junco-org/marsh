use std::fs;
use std::time::Duration;

use super::RequestHandler;
use rmux_core::LifecycleEvent;
use rmux_proto::{
    ErrorResponse, HookLifecycle, HookName, KillPaneRequest, KillSessionRequest,
    KillSessionResponse, KillWindowRequest, LinkWindowRequest, MoveWindowRequest,
    NewSessionExtRequest, NewSessionRequest, NewWindowRequest, OptionName, PaneTarget,
    ProcessCommand, Request, RespawnPaneRequest, Response, RmuxError, ScopeSelector,
    SetEnvironmentRequest, SetHookRequest, SetOptionMode, SetOptionRequest, ShowEnvironmentRequest,
    ShowOptionsRequest, SplitWindowRequest, UnlinkWindowRequest, WindowTarget,
};

use crate::test_fixtures::{
    unique_temp_path, wait_until, Fixture, Grouped, SessionSpec, TestRequest,
};
use crate::test_names::session_name;
use crate::test_shell::{command_quote, sh_quote_path};

async fn buffer_text(handler: &RequestHandler, name: &str) -> Option<String> {
    let state = handler.state.lock().await;
    state
        .buffers
        .show(Some(name))
        .ok()
        .map(|(_, content)| String::from_utf8_lossy(content).into_owned())
}

#[tokio::test]
async fn global_environment_applies_to_initial_panes_created_after_mutation() {
    let handler = RequestHandler::new();
    let variable_name = "RMUX_TEST_GLOBAL";

    assert_eq!(
        handler
            .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
                scope: ScopeSelector::Global,
                name: variable_name.to_owned(),
                value: "screen".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            })))
            .await,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Global,
            name: variable_name.to_owned(),
        })
    );

    SessionSpec::create(&handler, "alpha").await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session_name("alpha"), 0)
        .expect("pane 0 profile exists");
    assert_eq!(pane_zero.environment_value(variable_name), Some("screen"));
}

#[tokio::test]
async fn default_terminal_applies_to_initial_panes_and_yields_to_explicit_term() {
    let handler = RequestHandler::new();

    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest {
                scope: ScopeSelector::Global,
                option: OptionName::DefaultTerminal,
                value: "tmux-256color".to_owned(),
                mode: SetOptionMode::Replace,
            }))
            .await,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::DefaultTerminal,
            mode: SetOptionMode::Replace,
        })
    );

    SessionSpec::create(&handler, "alpha").await;

    {
        let state = handler.state.lock().await;
        let pane_zero = state
            .pane_profile(&session_name("alpha"), 0)
            .expect("pane 0 profile exists");
        assert_eq!(pane_zero.environment_value("TERM"), Some("tmux-256color"));
    }

    assert_eq!(
        handler
            .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
                scope: ScopeSelector::Session(session_name("alpha")),
                name: "TERM".to_owned(),
                value: "screen-256color".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            })))
            .await,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            name: "TERM".to_owned(),
        })
    );

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(session_name("alpha"))).await;

    let state = handler.state.lock().await;
    let pane_one = state
        .pane_profile(&session_name("alpha"), 1)
        .expect("pane 1 profile exists");
    assert_eq!(pane_one.environment_value("TERM"), Some("tmux-256color"));
    drop(state);

    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            environment: Some(vec!["TERM=screen-256color".to_owned()]),
            ..Fixture::fixture(session_name("alpha"))
        },
    )
    .await;

    let state = handler.state.lock().await;
    let pane_two = state
        .pane_profile(&session_name("alpha"), 2)
        .expect("pane 2 profile exists");
    assert_eq!(pane_two.environment_value("TERM"), Some("screen-256color"));
}

#[tokio::test]
async fn environment_mutations_apply_only_to_future_panes_and_session_values_win() {
    let handler = RequestHandler::new();
    let variable_name = "RMUX_TEST_SESSION_VALUE";
    SessionSpec::create(&handler, "alpha").await;

    {
        let state = handler.state.lock().await;
        let pane_zero = state
            .pane_profile(&session_name("alpha"), 0)
            .expect("pane 0 profile exists");
        assert_eq!(pane_zero.environment_value(variable_name), None);
    }

    assert_eq!(
        handler
            .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
                scope: ScopeSelector::Global,
                name: variable_name.to_owned(),
                value: "screen".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            })))
            .await,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Global,
            name: variable_name.to_owned(),
        })
    );
    {
        let state = handler.state.lock().await;
        let pane_zero = state
            .pane_profile(&session_name("alpha"), 0)
            .expect("pane 0 profile exists");
        assert_eq!(pane_zero.environment_value(variable_name), None);
        assert_eq!(
            state
                .environment
                .resolve(Some(&session_name("alpha")), variable_name),
            Some("screen")
        );
    }

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(session_name("alpha"))).await;
    {
        let state = handler.state.lock().await;
        let pane_one = state
            .pane_profile(&session_name("alpha"), 1)
            .expect("pane 1 profile exists");
        assert_eq!(pane_one.environment_value(variable_name), Some("screen"));
    }

    assert_eq!(
        handler
            .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
                scope: ScopeSelector::Session(session_name("alpha")),
                name: variable_name.to_owned(),
                value: "tmux-256color".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            })))
            .await,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            name: variable_name.to_owned(),
        })
    );
    {
        let state = handler.state.lock().await;
        let pane_one = state
            .pane_profile(&session_name("alpha"), 1)
            .expect("pane 1 profile exists");
        assert_eq!(pane_one.environment_value(variable_name), Some("screen"));
        assert_eq!(
            state
                .environment
                .resolve(Some(&session_name("alpha")), variable_name),
            Some("tmux-256color")
        );
    }

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(session_name("alpha"))).await;

    let state = handler.state.lock().await;
    let pane_two = state
        .pane_profile(&session_name("alpha"), 2)
        .expect("pane 2 profile exists");
    assert_eq!(
        pane_two.environment_value(variable_name),
        Some("tmux-256color")
    );
}

#[tokio::test]
async fn set_hook_updates_the_store_and_one_shot_hooks_are_consumed_on_attach() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    assert_eq!(
        handler
            .handle(Request::SetHook(SetHookRequest {
                scope: ScopeSelector::Session(session_name("alpha")),
                hook: HookName::ClientAttached,
                command: "display-message attached".to_owned(),
                lifecycle: HookLifecycle::OneShot,
            }))
            .await,
        Response::SetHook(rmux_proto::SetHookResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            hook: HookName::ClientAttached,
            lifecycle: HookLifecycle::OneShot,
        })
    );

    {
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .hooks
                .session_command(&session_name("alpha"), HookName::ClientAttached),
            Some("display-message attached")
        );
    }

    let outcome = handler
        .dispatch(
            std::process::id(),
            Request::AttachSession(rmux_proto::AttachSessionRequest {
                target: session_name("alpha"),
            }),
        )
        .await;
    assert!(matches!(outcome.response, Response::AttachSession(_)));

    let attach = outcome.attach.expect("attach upgrade");
    let _attach_id = handler
        .register_attach(std::process::id(), session_name("alpha"), attach.control_tx)
        .await;
    let queued = {
        let mut state = handler.state.lock().await;
        super::prepare_lifecycle_event(
            &mut state,
            &LifecycleEvent::ClientAttached {
                session_name: session_name("alpha"),
                client_name: None,
            },
        )
    };
    handler.dispatch_lifecycle_hook(queued).await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .hooks
            .session_command(&session_name("alpha"), HookName::ClientAttached),
        None
    );
}

#[tokio::test]
async fn session_closed_hooks_fire_before_session_scope_is_removed() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;
    let lifecycle_events = handler
        .take_lifecycle_dispatch_receiver()
        .expect("test owns the lifecycle dispatch receiver");
    let (hook_shutdown, hook_shutdown_rx) = tokio::sync::oneshot::channel();
    let hook_handler = handler.clone();
    let hook_task = tokio::spawn(async move {
        hook_handler
            .consume_lifecycle_hooks(lifecycle_events, hook_shutdown_rx)
            .await;
    });

    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Session(session_name("alpha")),
        HookName::SessionClosed,
        "if-shell -F '#{==:#{hook_session_name},alpha}' 'set-buffer -b closed ok' 'set-buffer -b closed bad'",
    )))
        .await;

    assert_eq!(
        handler
            .handle(Request::KillSession(KillSessionRequest::fixture("alpha")))
            .await,
        Response::KillSession(KillSessionResponse { existed: true })
    );

    handler.wait_for_buffer("closed", "ok").await;
    let _ = hook_shutdown.send(());
    hook_task.await.expect("lifecycle hook task joins");
}

#[tokio::test]
async fn move_window_last_source_session_emits_lifecycle_hooks_in_tmux_3_7b_order() {
    let handler = RequestHandler::new();
    let source = session_name("move-lifecycle-source");
    let destination = session_name("move-lifecycle-destination");
    SessionSpec::create(&handler, &source).await;
    SessionSpec::create(&handler, &destination).await;
    let source_session_id = handler.session_id_for_test(&source).await.as_u32();
    let hook_commands = [
        (
            HookName::WindowLinked,
            "display-message global-window-linked",
        ),
        (
            HookName::WindowUnlinked,
            "display-message global-window-unlinked",
        ),
        (
            HookName::SessionClosed,
            "display-message global-session-closed",
        ),
    ];
    for (hook, command) in hook_commands {
        TestRequest::send_ok(
            &handler,
            SetHookRequest::fixture((ScopeSelector::Global, hook, command)),
        )
        .await;
    }
    let mut events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(destination.clone(), 1),
        )),
    )
    .await;

    let lifecycle = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| {
            matches!(
                event.hook_name,
                HookName::WindowLinked | HookName::WindowUnlinked | HookName::SessionClosed
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle
            .iter()
            .map(|event| event.hook_name)
            .collect::<Vec<_>>(),
        vec![
            HookName::WindowLinked,
            HookName::WindowUnlinked,
            HookName::SessionClosed,
        ],
        "tmux 3.7b emits window-linked, window-unlinked, then session-closed"
    );
    assert!(matches!(
        lifecycle.last().map(|event| &event.event),
        Some(LifecycleEvent::SessionClosed {
            session_name,
            session_id: Some(actual_session_id),
        }) if session_name == &source && *actual_session_id == source_session_id
    ));
    for (event, (_, command)) in lifecycle.iter().zip(hook_commands) {
        assert_eq!(
            event
                .hooks
                .iter()
                .filter(|dispatch| dispatch.command() == command)
                .count(),
            1,
            "global lifecycle hook must survive the source-session teardown"
        );
    }
}

#[tokio::test]
async fn move_window_last_source_group_emits_tmux_3_7b_lifecycle_batch_order() {
    let handler = RequestHandler::new();
    let owner = session_name("move-group-lifecycle-owner");
    let peer = session_name("move-group-lifecycle-peer");
    let destination = session_name("move-group-lifecycle-destination");
    SessionSpec::create(&handler, &owner).await;
    SessionSpec::create(&handler, Grouped(&peer, &owner)).await;
    SessionSpec::create(&handler, &destination).await;
    for (hook, command) in [
        (HookName::WindowLinked, "display-message group-linked"),
        (HookName::WindowUnlinked, "display-message group-unlinked"),
        (HookName::SessionClosed, "display-message group-closed"),
    ] {
        TestRequest::send_ok(
            &handler,
            SetHookRequest::fixture((ScopeSelector::Global, hook, command)),
        )
        .await;
    }
    let mut events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest::fixture((
            WindowTarget::with_window(peer.clone(), 0),
            WindowTarget::with_window(destination.clone(), 1),
        )),
    )
    .await;

    let lifecycle = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match &event.event {
            LifecycleEvent::WindowLinked { session_name, .. } => {
                Some((HookName::WindowLinked, session_name.clone(), event.hooks))
            }
            LifecycleEvent::WindowUnlinked { session_name, .. } => {
                Some((HookName::WindowUnlinked, session_name.clone(), event.hooks))
            }
            LifecycleEvent::SessionClosed { session_name, .. } => {
                Some((HookName::SessionClosed, session_name.clone(), event.hooks))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle
            .iter()
            .map(|(hook, session_name, _)| (*hook, session_name.clone()))
            .collect::<Vec<_>>(),
        vec![
            (HookName::WindowLinked, destination),
            (HookName::WindowUnlinked, peer.clone()),
            (HookName::SessionClosed, owner.clone()),
            (HookName::WindowUnlinked, owner.clone()),
            (HookName::SessionClosed, peer.clone()),
        ],
        "tmux 3.7b emits one linked event, then the two grouped teardown pairs in this order"
    );
    for (hook, _, dispatches) in lifecycle {
        let expected_command = match hook {
            HookName::WindowLinked => "display-message group-linked",
            HookName::WindowUnlinked => "display-message group-unlinked",
            HookName::SessionClosed => "display-message group-closed",
            _ => unreachable!("filtered lifecycle hook"),
        };
        assert_eq!(
            dispatches
                .iter()
                .filter(|dispatch| dispatch.command() == expected_command)
                .count(),
            1,
            "each grouped lifecycle event keeps its global hook dispatch"
        );
    }
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&owner).is_none());
    assert!(state.sessions.session(&peer).is_none());
}

#[tokio::test]
async fn move_window_last_source_session_preserves_local_closed_hook_product_divergence() {
    let handler = RequestHandler::new();
    let source = session_name("move-local-hook-source");
    let destination = session_name("move-local-hook-destination");
    SessionSpec::create(&handler, &source).await;
    SessionSpec::create(&handler, &destination).await;
    let global_command = "display-message global-session-closed-fallback";
    let local_command = "display-message local-session-closed";
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::SessionClosed,
            global_command,
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Session(source.clone()),
            HookName::SessionClosed,
            local_command,
        )),
    )
    .await;
    let mut events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 0),
            WindowTarget::with_window(destination, 1),
        )),
    )
    .await;

    let closed = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| {
            matches!(
                &event.event,
                LifecycleEvent::SessionClosed { session_name, .. } if session_name == &source
            )
        })
        .expect("move-window emits session-closed for the removed source session");
    assert_eq!(
        closed
            .hooks
            .iter()
            .filter(|dispatch| dispatch.command() == local_command)
            .count(),
        1,
        "RMUX preserves the source session hook before removing its scope"
    );
    assert_eq!(
        closed
            .hooks
            .iter()
            .filter(|dispatch| dispatch.command() == global_command)
            .count(),
        0,
        "the explicit session hook continues to override the global fallback"
    );
}

#[tokio::test]
async fn kill_pane_does_not_synthesize_pane_exited_hook_like_tmux() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(session_name("alpha"))).await;

    let pane_target = PaneTarget::with_window(session_name("alpha"), 0, 1);
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Pane(pane_target.clone()),
            HookName::PaneExited,
            "set-buffer -b exited bad",
        )),
    )
    .await;

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: pane_target,
            kill_all_except: false,
        },
    )
    .await;

    let state = handler.state.lock().await;
    assert!(
        state.buffers.show(Some("exited")).is_err(),
        "kill-pane must use after-kill-pane, not synthesize pane-exited"
    );
}

#[tokio::test]
async fn window_unlinked_hooks_keep_removed_window_name_and_id() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    let window = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture("alpha")
        })
        .await;
    let window_id = handler.window_id_for_test(&window).await.as_u32();

    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{hook_window_name}} #{{hook_window}},logs @{window_id}}}' 'set-buffer -b unlinked ok' 'set-buffer -b unlinked bad'"
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(&handler, KillWindowRequest::fixture(window)).await;

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("unlinked"))
        .expect("unlinked buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "ok");
}

#[tokio::test]
async fn kill_window_renumbered_unlinked_hook_keeps_removed_formats_and_active_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("kill-window-unlinked-target");
    SessionSpec::create(&handler, &alpha).await;
    for (window_index, name) in [(1, "removed"), (2, "renumbered")] {
        handler
            .create_window(NewWindowRequest {
                name: Some(name.to_owned()),
                target_window_index: Some(window_index),
                ..Fixture::fixture(&alpha)
            })
            .await;
    }

    let (active_window_id, removed_window_id, renumbered_window_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha exists");
        (
            session.window_at(0).expect("active window exists").id(),
            session.window_at(1).expect("removed window exists").id(),
            session.window_at(2).expect("renumbered window exists").id(),
        )
    };
    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::RenumberWindows,
            "on",
        )
        .await;
    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{window_id}} #{{hook_window}} #{{hook_window_name}},{active_window_id} {removed_window_id} removed}}' 'set-buffer -b kill-window-unlinked-target active-removed' 'set-buffer -b kill-window-unlinked-target wrong'"
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(
        &handler,
        KillWindowRequest::fixture(WindowTarget::with_window(alpha.clone(), 1)),
    )
    .await;

    assert_eq!(
        buffer_text(&handler, "kill-window-unlinked-target").await,
        Some("active-removed".to_owned()),
        "window-unlinked must target the active survivor while preserving the removed window formats"
    );
    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("alpha survives");
    assert_eq!(session.active_window_index(), 0);
    assert_eq!(
        session
            .window_at(1)
            .expect("old window 2 was renumbered")
            .id(),
        renumbered_window_id
    );
}

#[tokio::test]
async fn unlink_window_unlinked_hook_targets_the_surviving_window_alias() {
    let handler = RequestHandler::new();
    let alpha = session_name("unlink-window-unlinked-source");
    let keeper = session_name("unlink-window-unlinked-keeper");
    SessionSpec::create(&handler, &alpha).await;
    SessionSpec::create(&handler, &keeper).await;
    let linked = handler
        .create_window(NewWindowRequest {
            name: Some("linked".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let linked_window_id = handler.window_id_for_test(&linked).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((&linked, WindowTarget::with_window(keeper.clone(), 5))),
    )
    .await;
    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{window_id}},{linked_window_id}}}' 'set-buffer -b unlink-window-unlinked-target alias' 'set-buffer -b unlink-window-unlinked-target wrong'"
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: linked,
            kill_if_last: false,
        },
    )
    .await;

    assert_eq!(
        buffer_text(&handler, "unlink-window-unlinked-target").await,
        Some("alias".to_owned()),
        "window-unlinked must follow the stable window identity to its surviving alias"
    );
    let state = handler.state.lock().await;
    assert!(state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(1))
        .is_none());
    assert_eq!(
        state
            .sessions
            .session(&keeper)
            .and_then(|session| session.window_at(5))
            .expect("surviving alias remains linked")
            .id(),
        linked_window_id
    );
}

#[tokio::test]
async fn unlink_window_kill_if_last_renumbered_hook_keeps_removed_formats_and_active_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("unlink-kill-unlinked-target");
    SessionSpec::create(&handler, &alpha).await;
    for window_index in [1, 2] {
        handler
            .create_window(NewWindowRequest {
                name: Some(format!("window-{window_index}")),
                target_window_index: Some(window_index),
                ..Fixture::fixture(&alpha)
            })
            .await;
    }
    let (active_window_id, removed_window_id) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha exists");
        (
            session.window_at(0).expect("active window exists").id(),
            session.window_at(1).expect("removed window exists").id(),
        )
    };
    handler
        .set_option(
            ScopeSelector::Session(alpha.clone()),
            OptionName::RenumberWindows,
            "on",
        )
        .await;
    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{window_id}} #{{hook_window}} #{{hook_window_name}},{active_window_id} {removed_window_id} window-1}}' 'set-buffer -b unlink-kill-unlinked-target active-removed' 'set-buffer -b unlink-kill-unlinked-target wrong'"
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(
        &handler,
        UnlinkWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
            kill_if_last: true,
        },
    )
    .await;

    assert_eq!(
        buffer_text(&handler, "unlink-kill-unlinked-target").await,
        Some("active-removed".to_owned()),
        "destructive unlink must target the active survivor while preserving the removed window formats"
    );
}

#[tokio::test]
async fn relative_move_window_unlinked_hook_follows_the_moved_window_identity() {
    let handler = RequestHandler::new();
    let alpha = session_name("relative-move-unlinked");
    SessionSpec::create(&handler, &alpha).await;
    for (window_index, name) in [(1, "middle"), (2, "moved")] {
        handler
            .create_window(NewWindowRequest {
                name: Some(name.to_owned()),
                target_window_index: Some(window_index),
                ..Fixture::fixture(&alpha)
            })
            .await;
    }
    let moved_window_id = handler
        .window_id_for_test(&WindowTarget::with_window(alpha.clone(), 2))
        .await;
    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{hook_window}} #{{window_index}} #{{window_id}},{} 0 {}}}' 'set-buffer -b relative-move-unlinked ok' 'set-buffer -b relative-move-unlinked bad'",
            moved_window_id, moved_window_id
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(
        &handler,
        MoveWindowRequest {
            before: true,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 2),
                WindowTarget::with_window(alpha.clone(), 0),
            ))
        },
    )
    .await;

    handler
        .wait_for_buffer("relative-move-unlinked", "ok")
        .await;
    let state = handler.state.lock().await;
    assert_eq!(
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .map(rmux_core::Window::id),
        Some(moved_window_id)
    );
}

#[tokio::test]
async fn kill_session_emits_window_unlinked_for_removed_windows() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    let window = handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture("alpha")
        })
        .await;
    let window_id = handler.window_id_for_test(&window).await.as_u32();

    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowUnlinked,
        format!(
            "if-shell -F '#{{==:#{{hook_window_name}} #{{hook_window}},logs @{window_id}}}' 'set-buffer -b kill-session-unlinked ok' 'set-buffer -b kill-session-unlinked bad'"
        )
        .as_str(),
    )))
        .await;

    TestRequest::send_ok(&handler, KillSessionRequest::fixture("alpha")).await;

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("kill-session-unlinked"))
        .expect("unlinked buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "ok");
}

#[tokio::test]
async fn unlink_only_linked_window_runs_window_hook_before_session_cleanup() {
    let handler = RequestHandler::new();
    let owner = session_name("unlink-order-owner");
    let alias = session_name("unlink-order-alias");
    SessionSpec::create(&handler, &owner).await;
    SessionSpec::create(&handler, &alias).await;

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
    assert_eq!(
        handler
            .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
                scope: ScopeSelector::Global,
                name: "RMUX_UNLINK_HOOK_DEPENDENCY".to_owned(),
                value: "alive".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            })))
            .await,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Global,
            name: "RMUX_UNLINK_HOOK_DEPENDENCY".to_owned(),
        })
    );
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::WindowUnlinked,
            "if-shell -F '#{==:#{RMUX_UNLINK_HOOK_DEPENDENCY},alive}' \
         'set-buffer -a -b unlink-hook-order window-saw-alive,' \
         'set-buffer -a -b unlink-hook-order window-saw-missing,'",
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::SessionClosed,
            "run-shell -C 'set-buffer -a -b unlink-hook-order session-cleanup, ; \
         set-environment -gu RMUX_UNLINK_HOOK_DEPENDENCY'",
        )),
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

    let observed = wait_until(
        Duration::from_secs(5),
        Duration::from_millis(20),
        async || match buffer_text(&handler, "unlink-hook-order").await {
            Some(value) if value.matches(',').count() == 2 => Ok(value),
            actual => Err(actual),
        },
    )
    .await
    .unwrap_or_else(|actual| panic!("timed out waiting for both unlink hooks, got {actual:?}"));
    assert_eq!(
        observed, "window-saw-alive,session-cleanup,",
        "window-unlinked must consume shared hook state before session-closed tears it down"
    );

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alias).is_none());
    assert!(state
        .sessions
        .session(&owner)
        .and_then(|session| session.window_at(0))
        .is_some());
}

#[tokio::test]
async fn kill_session_emits_session_closed_before_window_unlinked() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;
    SessionSpec::create(&handler, "keeper").await;
    handler
        .create_window(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture("alpha")
        })
        .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::SessionClosed,
            "set-buffer -a -b kill-session-order session-closed,",
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::WindowUnlinked,
            "set-buffer -a -b kill-session-order unlinked,",
        )),
    )
    .await;

    assert_eq!(
        handler
            .handle(Request::KillSession(KillSessionRequest::fixture("alpha")))
            .await,
        Response::KillSession(KillSessionResponse { existed: true })
    );

    handler
        .wait_for_buffer("kill-session-order", "session-closed,unlinked,unlinked,")
        .await;
}

#[tokio::test]
async fn last_pane_shell_exit_emits_window_unlinked_before_session_closed() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;
    SessionSpec::create(&handler, "keeper").await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::WindowUnlinked,
            "set-buffer -a -b last-pane-order unlinked,",
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::SessionClosed,
            "set-buffer -a -b last-pane-order session-closed,",
        )),
    )
    .await;
    let mut lifecycle_events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            process_command: Some(ProcessCommand::Shell("exit 0".to_owned())),
            ..Fixture::fixture(PaneTarget::new(session_name("alpha"), 0))
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let actual = buffer_text(&handler, "last-pane-order").await;
        if actual.as_deref() == Some("unlinked,session-closed,") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for last-pane hooks, got {actual:?}"
        );
        match tokio::time::timeout(Duration::from_millis(200), lifecycle_events.recv()).await {
            Ok(Ok(event)) => handler.dispatch_lifecycle_hook(event).await,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                panic!("lifecycle event channel closed")
            }
            Err(_) => {}
        }
    }
}

#[tokio::test]
async fn kill_server_emits_session_closed_without_window_unlinked() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;
    SessionSpec::create(&handler, "beta").await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::SessionClosed,
            "set-buffer -a -b kill-server-hooks session-closed,",
        )),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::WindowUnlinked,
            "set-buffer -a -b kill-server-hooks unlinked,",
        )),
    )
    .await;

    assert_eq!(
        handler
            .handle(Request::KillServer(rmux_proto::KillServerRequest))
            .await,
        Response::KillServer(rmux_proto::KillServerResponse)
    );

    handler
        .wait_for_buffer("kill-server-hooks", "session-closed,session-closed,")
        .await;
}

#[tokio::test]
async fn self_unsetting_hook_payloads_are_normalized_to_one_shot_shell_commands() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    assert_eq!(
        handler
            .handle(Request::SetHook(SetHookRequest {
                scope: ScopeSelector::Session(session_name("alpha")),
                hook: HookName::ClientAttached,
                command: format!(
                    "run-shell {}; set-hook -u -t alpha client-attached",
                    command_quote("printf attached > /tmp/rmux-hook")
                ),
                lifecycle: HookLifecycle::Persistent,
            }))
            .await,
        Response::SetHook(rmux_proto::SetHookResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            hook: HookName::ClientAttached,
            lifecycle: HookLifecycle::OneShot,
        })
    );

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .hooks
            .session_command(&session_name("alpha"), HookName::ClientAttached),
        Some("run-shell \"printf attached > /tmp/rmux-hook\"")
    );
    assert_eq!(
        state
            .hooks
            .session_lifecycle(&session_name("alpha"), HookName::ClientAttached),
        Some(HookLifecycle::OneShot)
    );
}

#[tokio::test]
async fn session_scoped_mutations_require_live_sessions_and_are_cleared_on_kill() {
    let handler = RequestHandler::new();

    let missing_environment = handler
        .handle(Request::SetEnvironment(Box::new(SetEnvironmentRequest {
            scope: ScopeSelector::Session(session_name("missing")),
            name: "TERM".to_owned(),
            value: "screen".to_owned(),
            mode: None,
            hidden: false,
            format: false,
        })))
        .await;
    assert_eq!(
        missing_environment,
        Response::Error(ErrorResponse {
            error: RmuxError::SessionNotFound("missing".to_owned()),
        })
    );

    SessionSpec::create(&handler, "alpha").await;
    handler
        .set_option(
            ScopeSelector::Session(session_name("alpha")),
            OptionName::Status,
            "off",
        )
        .await;
    TestRequest::send_ok(
        &handler,
        SetEnvironmentRequest {
            scope: ScopeSelector::Session(session_name("alpha")),
            name: "TERM".to_owned(),
            value: "screen".to_owned(),
            mode: None,
            hidden: false,
            format: false,
        },
    )
    .await;
    assert_eq!(
        handler
            .handle(Request::KillSession(KillSessionRequest::fixture("alpha")))
            .await,
        Response::KillSession(KillSessionResponse { existed: true })
    );

    SessionSpec::create(&handler, "alpha").await;
    {
        let state = handler.state.lock().await;
        assert_eq!(
            state
                .options
                .resolve(Some(&session_name("alpha")), OptionName::Status),
            Some("on")
        );
        assert_eq!(
            state
                .environment
                .resolve(Some(&session_name("alpha")), "TERM"),
            None
        );
        let pane_zero = state
            .pane_profile(&session_name("alpha"), 0)
            .expect("pane 0 profile exists");
        assert_eq!(pane_zero.environment_value("TERM"), Some("tmux-256color"));
    }
}

#[tokio::test]
async fn after_show_options_runs_without_triggering_nested_notify_hooks() {
    let handler = RequestHandler::new();

    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::AfterShowOptions,
        "if-shell -F '#{==:#{hook},after-show-options}' 'set-buffer -b observed ok' 'set-buffer -b observed bad'",
    )))
        .await;
    assert!(matches!(
        handler
            .handle(Request::SetHook(SetHookRequest {
                scope: ScopeSelector::Global,
                hook: HookName::PasteBufferChanged,
                command: "set-buffer -b recursive fired".to_owned(),
                lifecycle: HookLifecycle::OneShot,
            }))
            .await,
        Response::Error(_)
    ));

    assert!(matches!(
        handler
            .handle(Request::ShowOptions(ShowOptionsRequest {
                scope: rmux_proto::OptionScopeSelector::SessionGlobal,
                name: None,
                value_only: false,
                include_inherited: true,
                quiet: false,
                include_hooks: false,
            }))
            .await,
        Response::ShowOptions(_)
    ));

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("observed"))
        .expect("observed buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "ok");
    assert_eq!(
        state.hooks.global_command(HookName::PasteBufferChanged),
        None
    );
}

#[tokio::test]
async fn split_window_runs_after_hook_once() {
    let handler = RequestHandler::new();
    let output_path = unique_temp_path("after-split-window");
    let shell_command = format!("printf x >> {}", sh_quote_path(&output_path));
    SessionSpec::create(&handler, "alpha").await;

    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::AfterSplitWindow,
            format!("run-shell {}", command_quote(&shell_command)).as_str(),
        )),
    )
    .await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(session_name("alpha"))).await;

    assert_eq!(
        fs::read_to_string(&output_path).expect("split hook output exists"),
        "x"
    );
    let _ = fs::remove_file(output_path);
}

#[tokio::test]
async fn window_linked_hooks_receive_session_and_window_format_context() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    TestRequest::send_ok(&handler, SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::WindowLinked,
        "if-shell -F '#{==:#{hook_session} #{hook_window},$0 @1}' 'set-buffer -b linked ok' 'set-buffer -b linked bad'",
    )))
        .await;

    let window = handler
        .create_window(NewWindowRequest {
            detached: false,
            ..Fixture::fixture("alpha")
        })
        .await;

    let state = handler.state.lock().await;
    assert!(
        state
            .sessions
            .session(&session_name("alpha"))
            .and_then(|session| session.window_at(window.window_index()))
            .is_some(),
        "new window exists"
    );
    let (_, content) = state
        .buffers
        .show(Some("linked"))
        .expect("linked buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "ok");
}

#[tokio::test]
async fn hook_commands_do_not_pre_expand_set_buffer_arguments() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    TestRequest::send_ok(
        &handler,
        SetHookRequest::fixture((
            ScopeSelector::Global,
            HookName::AfterNewWindow,
            "set-buffer -b hook '#{hook} #{hook_window_name}'",
        )),
    )
    .await;

    handler
        .create_window(NewWindowRequest {
            name: Some("hooked".to_owned()),
            detached: false,
            ..Fixture::fixture("alpha")
        })
        .await;

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("hook"))
        .expect("hook buffer exists");
    assert_eq!(
        String::from_utf8_lossy(content),
        "#{hook} #{hook_window_name}"
    );
}

#[tokio::test]
async fn spawned_pane_environment_contains_pane_id_with_percent_prefix() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session_name("alpha"), 0)
        .expect("pane 0 profile exists");
    let rmux_pane = pane_zero.environment_value("RMUX_PANE");
    assert!(
        rmux_pane.is_some(),
        "RMUX_PANE must be set in spawned pane environment"
    );
    let rmux_pane = rmux_pane.expect("RMUX_PANE is set");
    assert_eq!(pane_zero.environment_value("TMUX_PANE"), Some(rmux_pane));
    assert!(
        rmux_pane.starts_with('%'),
        "RMUX_PANE must start with %: got {rmux_pane}"
    );
    let id_part = &rmux_pane[1..];
    assert!(
        id_part.parse::<u32>().is_ok(),
        "RMUX_PANE must be %<id>: got {rmux_pane}"
    );
}

#[tokio::test]
async fn spawned_pane_environment_contains_mux_socket_pid_session_format() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, "alpha").await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session_name("alpha"), 0)
        .expect("pane 0 profile exists");
    let rmux_value = pane_zero.environment_value("RMUX").expect("RMUX is set");
    assert_eq!(pane_zero.environment_value("TMUX"), Some(rmux_value));
    let parts: Vec<_> = rmux_value.split(',').collect();
    assert_eq!(
        parts.len(),
        3,
        "RMUX must be <socket>,<pid>,<session_id>: got {rmux_value}"
    );
    assert!(
        parts[1].parse::<u32>().is_ok(),
        "RMUX pid must be numeric: got {}",
        parts[1]
    );
    assert!(
        parts[2].parse::<u32>().is_ok(),
        "RMUX session_id must be numeric: got {}",
        parts[2]
    );
}

#[tokio::test]
async fn environment_override_layering_session_then_override_then_rmux_pane() {
    let handler = RequestHandler::new();

    TestRequest::send_ok(
        &handler,
        SetEnvironmentRequest {
            scope: ScopeSelector::Global,
            name: "MY_VAR".to_owned(),
            value: "global".to_owned(),
            mode: None,
            hidden: false,
            format: false,
        },
    )
    .await;

    // Create session with -e overrides: MY_VAR should be overridden by -e.
    SessionSpec::create(
        &handler,
        NewSessionRequest {
            environment: Some(vec!["MY_VAR=override".to_owned()]),
            ..Fixture::fixture("alpha")
        },
    )
    .await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session_name("alpha"), 0)
        .expect("pane 0 profile exists");

    // -e overrides must beat session/global environment.
    assert_eq!(pane_zero.environment_value("MY_VAR"), Some("override"));
    // RMUX_PANE must still be present despite -e.
    assert!(pane_zero.environment_value("RMUX_PANE").is_some());
    drop(state);

    let shown = handler
        .handle(Request::ShowEnvironment(ShowEnvironmentRequest {
            scope: ScopeSelector::Session(session_name("alpha")),
            name: Some("MY_VAR".to_owned()),
            hidden: false,
            shell_format: false,
        }))
        .await;
    let output = shown
        .command_output()
        .expect("show-environment should expose new-session -e");
    assert_eq!(output.stdout(), b"MY_VAR=override\n");
}

#[tokio::test]
async fn new_session_ext_client_environment_respects_tmux_precedence() {
    let handler = RequestHandler::new();
    let session = session_name("client-env");

    for name in ["RMUX_GLOBAL_ENV_SENTINEL", "RMUX_OVERRIDE_ENV_SENTINEL"] {
        TestRequest::send_ok(
            &handler,
            SetEnvironmentRequest {
                scope: ScopeSelector::Global,
                name: name.to_owned(),
                value: "from-server".to_owned(),
                mode: None,
                hidden: false,
                format: false,
            },
        )
        .await;
    }

    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            environment: Some(vec![
                "RMUX_OVERRIDE_ENV_SENTINEL=from-explicit".to_owned(),
                "RMUX_CLIENT_ENV_SENTINEL=from-explicit".to_owned(),
            ]),
            client_environment: Some(vec![
                "PATH=/tmp/rmux-client-bin:/usr/bin".to_owned(),
                "SSH_AUTH_SOCK=/tmp/rmux-client-agent.sock".to_owned(),
                "RMUX_GLOBAL_ENV_SENTINEL=from-client".to_owned(),
                "RMUX_OVERRIDE_ENV_SENTINEL=from-client".to_owned(),
                "RMUX_CLIENT_ENV_SENTINEL=from-client".to_owned(),
                "RMUX_CLIENT_ONLY_ENV_SENTINEL=from-client".to_owned(),
            ]),
            ..Fixture::fixture(&session)
        },
    )
    .await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session, 0)
        .expect("pane 0 profile exists");
    assert_eq!(
        pane_zero.environment_value("PATH"),
        Some("/tmp/rmux-client-bin:/usr/bin")
    );
    assert_eq!(
        pane_zero.environment_value("SSH_AUTH_SOCK"),
        Some("/tmp/rmux-client-agent.sock")
    );
    assert_eq!(
        pane_zero.environment_value("RMUX_GLOBAL_ENV_SENTINEL"),
        Some("from-server")
    );
    assert_eq!(
        pane_zero.environment_value("RMUX_OVERRIDE_ENV_SENTINEL"),
        Some("from-explicit")
    );
    assert_eq!(
        pane_zero.environment_value("RMUX_CLIENT_ENV_SENTINEL"),
        Some("from-explicit")
    );
    assert_eq!(
        pane_zero.environment_value("RMUX_CLIENT_ONLY_ENV_SENTINEL"),
        Some("from-client")
    );
    drop(state);

    let shown = handler
        .handle(Request::ShowEnvironment(ShowEnvironmentRequest {
            scope: ScopeSelector::Session(session.clone()),
            name: Some("RMUX_CLIENT_ONLY_ENV_SENTINEL".to_owned()),
            hidden: false,
            shell_format: false,
        }))
        .await;
    assert!(matches!(shown, Response::Error(_)));
}

#[tokio::test]
async fn new_session_ext_skip_environment_update_keeps_client_spawn_environment() {
    let handler = RequestHandler::new();
    let session = session_name("skip-client-env");

    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            client_environment: Some(vec![
                "PATH=/tmp/rmux-client-bin:/usr/bin".to_owned(),
                "RMUX_CLIENT_ONLY_ENV_SENTINEL=from-client".to_owned(),
            ]),
            skip_environment_update: true,
            ..Fixture::fixture(&session)
        },
    )
    .await;

    let state = handler.state.lock().await;
    let pane_zero = state
        .pane_profile(&session, 0)
        .expect("pane 0 profile exists");
    assert_eq!(
        pane_zero.environment_value("RMUX_CLIENT_ONLY_ENV_SENTINEL"),
        Some("from-client")
    );
    drop(state);

    let shown = handler
        .handle(Request::ShowEnvironment(ShowEnvironmentRequest {
            scope: ScopeSelector::Session(session),
            name: Some("PATH".to_owned()),
            hidden: false,
            shell_format: false,
        }))
        .await;
    assert!(matches!(shown, Response::Error(_)));
}
