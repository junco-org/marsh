use super::*;
use rmux_proto::CapturePaneRequest;

async fn create_linked_respawn_family(
    handler: &RequestHandler,
) -> (SessionName, SessionName, SessionName) {
    let owner = create_session(handler, "respawn-linked-guard-owner").await;
    let alias1 = create_session(handler, "respawn-linked-guard-alias1").await;
    let alias2 = create_session(handler, "respawn-linked-guard-alias2").await;

    for alias in [&alias1, &alias2] {
        handler
            .handle_ok(LinkWindowRequest {
                kill_destination: true,
                ..Fixture::fixture((
                    WindowTarget::with_window(owner.clone(), 0),
                    WindowTarget::with_window(alias.clone(), 0),
                ))
            })
            .await;
    }

    (owner, alias1, alias2)
}

fn linked_targets(
    owner: &SessionName,
    alias1: &SessionName,
    alias2: &SessionName,
) -> [WindowTarget; 3] {
    [
        WindowTarget::with_window(owner.clone(), 0),
        WindowTarget::with_window(alias1.clone(), 0),
        WindowTarget::with_window(alias2.clone(), 0),
    ]
}

async fn capture_pane_print(handler: &RequestHandler, target: &PaneTarget) -> String {
    let output = handler
        .handle_ok(CapturePaneRequest::fixture(target))
        .await
        .output
        .expect("capture-pane -p should return command output");
    String::from_utf8(output.stdout).expect("capture-pane stdout is utf-8")
}

/// The process this linked window's pane is running, once it has one.
///
/// Waits rather than reads: a respawn answers as soon as the replacement job is open, and the
/// shell it execs claims the terminal's foreground process group a moment later. Reading once
/// would measure that gap instead of the restart this test is about.
async fn pane_pid(handler: &RequestHandler, target: &WindowTarget) -> u32 {
    handler
        .wait_for_pane_pid_for_test(&PaneTarget::with_window(
            target.session_name().clone(),
            target.window_index(),
            0,
        ))
        .await
}

async fn respawn_window(handler: &RequestHandler, target: WindowTarget, kill: bool) -> Response {
    handler
        .handle(Request::RespawnWindow(Box::new(RespawnWindowRequest {
            target,
            kill,
            start_directory: None,
            environment: None,
            command: Some(quiet_command()),
        })))
        .await
}

#[tokio::test]
async fn respawn_window_without_kill_rejects_active_linked_window_from_each_alias() {
    let handler = RequestHandler::new();
    let (owner, alias1, alias2) = create_linked_respawn_family(&handler).await;
    let owner_target = WindowTarget::with_window(owner.clone(), 0);
    let owner_pane = PaneTarget::with_window(owner.clone(), 0, 0);

    handler
        .state
        .lock()
        .await
        .append_bytes_to_pane_transcript_for_test(&owner, 0, 0, b"respawn-linked-old")
        .expect("linked marker transcript append succeeds");
    let initial_pid = pane_pid(&handler, &owner_target).await;
    let initial_capture = capture_pane_print(&handler, &owner_pane).await;
    assert!(
        initial_capture.contains("respawn-linked-old"),
        "expected linked marker in capture, got {initial_capture:?}"
    );

    for target in linked_targets(&owner, &alias1, &alias2) {
        let response = respawn_window(&handler, target.clone(), false).await;
        assert!(
            matches!(&response, Response::Error(error) if error.error.to_string().contains("still active")),
            "expected still-active error for {target}, got {response:?}"
        );
        assert_eq!(
            pane_pid(&handler, &owner_target).await,
            initial_pid,
            "respawn-window without -k must preserve the shared runtime from {target}"
        );
        assert_eq!(
            capture_pane_print(&handler, &owner_pane).await,
            initial_capture,
            "respawn-window without -k must preserve pane contents from {target}"
        );
    }
}

#[tokio::test]
async fn respawn_window_with_kill_restarts_active_linked_window_from_each_alias() {
    let handler = RequestHandler::new();
    let (owner, alias1, alias2) = create_linked_respawn_family(&handler).await;
    let owner_target = WindowTarget::with_window(owner.clone(), 0);

    for target in linked_targets(&owner, &alias1, &alias2) {
        let before_pid = pane_pid(&handler, &owner_target).await;
        let response = respawn_window(&handler, target.clone(), true).await;
        assert!(
            matches!(&response, Response::RespawnWindow(result) if result.target == target),
            "expected respawn-window -k success for {target}, got {response:?}"
        );

        let after_pid = pane_pid(&handler, &owner_target).await;
        assert_ne!(
            after_pid, before_pid,
            "respawn-window -k must restart the shared runtime from {target}"
        );
        for linked_target in linked_targets(&owner, &alias1, &alias2) {
            assert_eq!(
                pane_pid(&handler, &linked_target).await,
                after_pid,
                "linked target {linked_target} must resolve the restarted runtime from {target}"
            );
        }
    }
}
