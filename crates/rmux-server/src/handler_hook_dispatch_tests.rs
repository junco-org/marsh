use super::shutdown_support::SHUTDOWN_RETRY_DELAY;
use super::RequestHandler;
use crate::daemon::ShutdownHandle;
use rmux_proto::{
    HookName, LinkWindowRequest, NewWindowRequest, Request, ResizeWindowRequest, Response,
    ScopeSelector, SetHookMutationRequest, ShowOptionsRequest, WindowTarget,
};

use crate::test_fixtures::{Fixture, Grouped};
use crate::test_names::session_name;

async fn buffer_text(handler: &RequestHandler, name: &str) -> Option<String> {
    let state = handler.state.lock().await;
    state
        .buffers
        .show(Some(name))
        .ok()
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
}

#[tokio::test]
async fn appended_after_new_window_hooks_run_once_in_order() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;

    handler
        .set_global_hook(HookName::AfterNewWindow, "set-buffer -a -b hook first")
        .await;
    handler
        .handle_ok(SetHookMutationRequest {
            append: true,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::AfterNewWindow,
                "set-buffer -a -b hook second",
            ))
        })
        .await;

    handler
        .create_window(NewWindowRequest {
            detached: false,
            ..Fixture::fixture("alpha")
        })
        .await;

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("hook"))
        .expect("hook buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "firstsecond");
}

#[tokio::test]
async fn after_new_window_runs_distinct_kill_window_lifecycle_hooks_in_tmux_order() {
    let handler = RequestHandler::new();
    let alpha = session_name("nested-lifecycle-alpha");
    let beta = session_name("nested-lifecycle-beta");
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    handler
        .set_global_hook(
            HookName::WindowUnlinked,
            "set-buffer -a -b nested-lifecycle W",
        )
        .await;
    handler
        .set_global_hook(
            HookName::SessionClosed,
            "set-buffer -a -b nested-lifecycle S",
        )
        .await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "kill-window -t nested-lifecycle-alpha:0",
        )
        .await;

    let _ = handler.create_window(&beta).await;

    assert_eq!(
        buffer_text(&handler, "nested-lifecycle").await.as_deref(),
        Some("WS")
    );
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&alpha)
        .is_none());
}

#[tokio::test]
async fn same_after_hook_does_not_reenter_when_its_command_creates_a_window() {
    let handler = RequestHandler::new();
    let alpha = session_name("same-after-hook");
    handler.create_session(&alpha).await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "set-buffer -a -b same-after-hook A",
        )
        .await;
    handler
        .handle_ok(SetHookMutationRequest {
            append: true,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::AfterNewWindow,
                "if-shell -F '#{==:#{session_windows},2}' 'new-window -d -t same-after-hook'",
            ))
        })
        .await;

    let _ = handler.create_window(&alpha).await;

    assert_eq!(
        buffer_text(&handler, "same-after-hook").await.as_deref(),
        Some("A")
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .sessions
            .session(&alpha)
            .expect("session survives")
            .windows()
            .len(),
        3
    );
}

#[tokio::test]
async fn lifecycle_hook_commands_cannot_enqueue_a_second_lifecycle_generation() {
    let handler = RequestHandler::new();
    let alpha = session_name("bounded-lifecycle-alpha");
    let beta = session_name("bounded-lifecycle-beta");
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    handler
        .set_global_hook(
            HookName::WindowUnlinked,
            "set-buffer -a -b bounded-lifecycle W",
        )
        .await;
    handler
        .handle_ok(SetHookMutationRequest {
            append: true,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::WindowUnlinked,
                "kill-window -t bounded-lifecycle-beta:1",
            ))
        })
        .await;
    handler
        .set_global_hook(
            HookName::SessionClosed,
            "set-buffer -a -b bounded-lifecycle S",
        )
        .await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "kill-window -t bounded-lifecycle-alpha:0",
        )
        .await;

    let _ = handler.create_window(&beta).await;

    assert_eq!(
        buffer_text(&handler, "bounded-lifecycle").await.as_deref(),
        Some("WS")
    );
    let state = handler.state.lock().await;
    let beta_session = state.sessions.session(&beta).expect("beta survives");
    assert_eq!(beta_session.windows().len(), 1);
    assert!(beta_session.window_at(0).is_some());
}

#[tokio::test]
async fn explicitly_run_hook_allows_one_same_lifecycle_generation() {
    let handler = RequestHandler::new();
    let alpha = session_name("same-lifecycle-alpha");
    let beta = session_name("same-lifecycle-beta");
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    handler
        .set_global_hook(
            HookName::WindowUnlinked,
            "set-buffer -a -b same-lifecycle X",
        )
        .await;
    handler
        .handle_ok(SetHookMutationRequest {
            append: true,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::WindowUnlinked,
                "kill-window -t same-lifecycle-alpha:0",
            ))
        })
        .await;
    handler
        .set_global_hook(HookName::SessionClosed, "set-buffer -a -b same-lifecycle S")
        .await;

    handler
        .handle_ok(SetHookMutationRequest {
            command: None,
            run_immediately: true,
            ..Fixture::fixture((ScopeSelector::Global, HookName::WindowUnlinked, ""))
        })
        .await;

    assert_eq!(
        buffer_text(&handler, "same-lifecycle").await.as_deref(),
        Some("XXS")
    );
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&alpha)
        .is_none());
}

#[tokio::test]
async fn command_error_hook_does_not_reenter_from_a_hook_command_failure() {
    let handler = RequestHandler::new();
    let alpha = session_name("nested-command-error");
    handler.create_session(&alpha).await;
    handler
        .set_global_hook(
            HookName::CommandError,
            "set-buffer -a -b nested-command-error E",
        )
        .await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "set-buffer -a -b nested-command-error A",
        )
        .await;
    handler
        .handle_ok(SetHookMutationRequest {
            append: true,
            ..Fixture::fixture((
                ScopeSelector::Global,
                HookName::AfterNewWindow,
                "kill-window -t missing-nested-hook:0",
            ))
        })
        .await;

    let _ = handler.create_window(&alpha).await;

    assert_eq!(
        buffer_text(&handler, "nested-command-error")
            .await
            .as_deref(),
        Some("A")
    );
}

#[tokio::test]
async fn session_scoped_after_hook_can_run_a_distinct_session_lifecycle_hook() {
    let handler = RequestHandler::new();
    let beta = session_name("session-scoped-nested-hook");
    handler.create_session(&beta).await;
    handler
        .handle_ok(SetHookMutationRequest::fixture((
            ScopeSelector::Session(beta.clone()),
            HookName::WindowUnlinked,
            "set-buffer -a -b session-scoped-nested-hook W",
        )))
        .await;
    handler
        .handle_ok(SetHookMutationRequest::fixture((
            ScopeSelector::Session(beta.clone()),
            HookName::AfterNewWindow,
            "kill-window -t session-scoped-nested-hook:1",
        )))
        .await;

    let _ = handler.create_window(&beta).await;

    assert_eq!(
        buffer_text(&handler, "session-scoped-nested-hook")
            .await
            .as_deref(),
        Some("W")
    );
    let state = handler.state.lock().await;
    let session = state.sessions.session(&beta).expect("session survives");
    assert_eq!(session.windows().len(), 1);
    assert!(session.window_at(0).is_some());
}

#[tokio::test]
async fn nested_linked_last_window_hooks_keep_tmux_family_order() {
    let handler = RequestHandler::new();
    let alpha = session_name("nested-linked-alpha");
    let beta = session_name("nested-linked-beta");
    let trigger = session_name("nested-linked-trigger");
    handler.create_session(&alpha).await;
    handler.create_session(&beta).await;
    handler.create_session(&trigger).await;
    handler
        .handle_ok(LinkWindowRequest::fixture((
            WindowTarget::with_window(alpha.clone(), 0),
            WindowTarget::with_window(beta.clone(), 1),
        )))
        .await;
    handler
        .set_global_hook(HookName::WindowUnlinked, "set-buffer -a -b nested-linked W")
        .await;
    handler
        .set_global_hook(HookName::SessionClosed, "set-buffer -a -b nested-linked S")
        .await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "kill-window -t nested-linked-alpha:0",
        )
        .await;

    let _ = handler.create_window(&trigger).await;

    assert_eq!(
        buffer_text(&handler, "nested-linked").await.as_deref(),
        Some("WSW")
    );
}

#[tokio::test]
async fn nested_grouped_last_window_hooks_keep_tmux_family_order() {
    let handler = RequestHandler::new();
    let alpha = session_name("nested-grouped-alpha");
    let trigger = session_name("nested-grouped-trigger");
    handler.create_session(&alpha).await;
    let beta = handler
        .create_session(Grouped("nested-grouped-beta", &alpha))
        .await;
    handler.create_session(&trigger).await;
    handler
        .set_global_hook(
            HookName::WindowUnlinked,
            "set-buffer -a -b nested-grouped W",
        )
        .await;
    handler
        .set_global_hook(HookName::SessionClosed, "set-buffer -a -b nested-grouped S")
        .await;
    handler
        .set_global_hook(
            HookName::AfterNewWindow,
            "kill-window -t nested-grouped-alpha:0",
        )
        .await;

    let _ = handler.create_window(&trigger).await;

    assert_eq!(
        buffer_text(&handler, "nested-grouped").await.as_deref(),
        Some("WSSW")
    );
    let state = handler.state.lock().await;
    assert!(state.sessions.session(&alpha).is_none());
    assert!(state.sessions.session(&beta).is_none());
}

#[tokio::test]
async fn nested_last_session_kill_preserves_exit_empty_shutdown() {
    let handler = RequestHandler::new();
    let (shutdown_handle, mut shutdown_rx) = ShutdownHandle::new();
    handler.install_shutdown_handle(shutdown_handle);
    let alpha = session_name("nested-shutdown-alpha");
    handler.create_session(&alpha).await;
    handler
        .set_global_hook(
            HookName::WindowUnlinked,
            "set-buffer -a -b nested-shutdown W",
        )
        .await;
    handler
        .set_global_hook(
            HookName::SessionClosed,
            "set-buffer -a -b nested-shutdown S",
        )
        .await;
    handler
        .set_global_hook(
            HookName::AfterShowOptions,
            "kill-window -t nested-shutdown-alpha:0",
        )
        .await;

    // The session above created a real managed pane, so this handler has an engine and that
    // pane's job is live work by the idle check's own measure. Losing the session is therefore
    // not proof that the job's delivery has retired; `wait_quiet` below is that proof. The
    // accessor answers with an unleased handle: a native-client lease taken here would itself
    // keep the daemon from ever being quiet.
    let io = handler
        .shell_io()
        .expect("the session installed its managed engine");
    // Deferral is what this regression is about, so it is made deliberate instead of left to
    // scheduler luck: while this guard lives, exit-empty must stay queued and keep retrying.
    let request = handler.begin_detached_request();

    let response = handler
        .handle(Request::ShowOptions(ShowOptionsRequest {
            scope: rmux_proto::OptionScopeSelector::SessionGlobal,
            name: None,
            value_only: false,
            include_inherited: true,
            quiet: false,
            include_hooks: false,
        }))
        .await;

    assert!(matches!(response, Response::ShowOptions(_)));
    assert_eq!(
        buffer_text(&handler, "nested-shutdown").await.as_deref(),
        Some("WS")
    );
    assert!(handler.state.lock().await.sessions.is_empty());

    // Still on the real clock, because this waits on real pseudoterminal and filesystem
    // teardown. The bound is a deadlock guard around an actual retirement notification, not a
    // settle delay: `jobs().is_empty()` and `wait_closed` both resolve before the daemon has
    // finished delivering and retiring the instance.
    tokio::time::timeout(std::time::Duration::from_secs(30), io.wait_quiet())
        .await
        .expect("the killed pane's managed job must finish delivering and retire");

    assert!(
        matches!(
            shutdown_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "an in-flight detached request must defer exit-empty shutdown, never fire it early"
    );

    // Native work is done, so the only thing left to wait for is the handler's own deferred
    // retry, which sleeps on the runtime this test owns. Pausing now advances that timer without
    // skipping past anything real.
    tokio::time::pause();
    drop(request);
    tokio::time::timeout(SHUTDOWN_RETRY_DELAY * 3, shutdown_rx)
        .await
        .expect("the nested last-session kill must request exit-empty shutdown")
        .expect("shutdown receiver should complete cleanly");
    tokio::time::resume();
}

#[tokio::test]
async fn window_resized_hook_runs_after_resize_window() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;
    handler
        .set_global_hook(HookName::WindowResized, "set-buffer -b resized yes")
        .await;

    handler
        .handle_ok(ResizeWindowRequest {
            target: WindowTarget::with_window(session_name("alpha"), 0),
            width: Some(90),
            height: Some(24),
            adjustment: None,
        })
        .await;

    let state = handler.state.lock().await;
    let (_, content) = state
        .buffers
        .show(Some("resized"))
        .expect("window-resized hook buffer exists");
    assert_eq!(String::from_utf8_lossy(content), "yes");
}
