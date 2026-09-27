use super::attach_support::ActiveAttachIdentity;
use super::{with_expected_attach_and_session_identity, RequestHandler};
use crate::test_fixtures::Fixture;
use rmux_proto::{
    KillPaneRequest, KillSessionRequest, LinkWindowRequest, MoveWindowRequest, MoveWindowTarget,
    NewWindowRequest, PaneTarget, Request, RespawnPaneRequest, RespawnWindowRequest, Response,
    SessionId, SessionName, SwapWindowRequest, UnlinkWindowRequest, WindowTarget,
};

async fn replace_session(handler: &RequestHandler, session_name: &SessionName) -> SessionId {
    let stale_id = handler.session_id_for_test(session_name).await;
    handler
        .handle_ok(KillSessionRequest {
            target: session_name.clone(),
            kill_all_except_target: false,
            clear_alerts: false,
            kill_group: false,
        })
        .await;
    let recreated = handler.create_session(session_name).await;
    assert_eq!(&recreated, session_name);
    assert_ne!(handler.session_id_for_test(session_name).await, stale_id);
    stale_id
}

async fn run_as_stale_attached_session(
    handler: &RequestHandler,
    session_name: SessionName,
    stale_id: SessionId,
    request: Request,
) -> Response {
    with_expected_attach_and_session_identity(
        ActiveAttachIdentity::new(990_001, 1, stale_id),
        session_name,
        stale_id,
        handler.handle(request),
    )
    .await
}

#[tokio::test]
async fn stale_attached_session_cannot_respawn_replacement_window() {
    let handler = RequestHandler::new();
    let session_name = handler.create_session("identity-respawn-window").await;
    let stale_id = replace_session(&handler, &session_name).await;

    let response = run_as_stale_attached_session(
        &handler,
        session_name.clone(),
        stale_id,
        Request::RespawnWindow(Box::new(RespawnWindowRequest {
            target: WindowTarget::with_window(session_name.clone(), 0),
            kill: true,
            environment: None,
            command: None,
            start_directory: None,
        })),
    )
    .await;

    assert!(matches!(response, Response::Error(_)), "{response:?}");
    assert_ne!(handler.session_id_for_test(&session_name).await, stale_id);
}

#[tokio::test]
async fn stale_attached_session_cannot_respawn_or_kill_replacement_pane() {
    let handler = RequestHandler::new();
    let session_name = handler.create_session("identity-pane-mutations").await;
    let stale_id = replace_session(&handler, &session_name).await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);

    let respawn = run_as_stale_attached_session(
        &handler,
        session_name.clone(),
        stale_id,
        Request::RespawnPane(Box::new(RespawnPaneRequest::fixture(&target))),
    )
    .await;
    assert!(matches!(respawn, Response::Error(_)), "{respawn:?}");

    let killed = run_as_stale_attached_session(
        &handler,
        session_name.clone(),
        stale_id,
        Request::KillPane(KillPaneRequest {
            target,
            kill_all_except: false,
        }),
    )
    .await;
    assert!(matches!(killed, Response::Error(_)), "{killed:?}");
    assert_ne!(handler.session_id_for_test(&session_name).await, stale_id);
}

#[tokio::test]
async fn stale_attached_session_cannot_unlink_recreated_window_slot() {
    let handler = RequestHandler::new();
    let session_name = handler.create_session("identity-unlink-window").await;
    let window = NewWindowRequest {
        target_window_index: Some(1),
        ..Fixture::fixture(&session_name)
    };
    handler.create_window(&window).await;
    let stale_id = replace_session(&handler, &session_name).await;
    handler.create_window(window).await;

    let response = run_as_stale_attached_session(
        &handler,
        session_name.clone(),
        stale_id,
        Request::UnlinkWindow(UnlinkWindowRequest {
            target: WindowTarget::with_window(session_name.clone(), 1),
            kill_if_last: true,
        }),
    )
    .await;

    assert!(matches!(response, Response::Error(_)), "{response:?}");
    let state = handler.state.lock().await;
    assert!(
        state
            .sessions
            .session(&session_name)
            .and_then(|session| session.window_at(1))
            .is_some(),
        "replacement window must survive the stale unlink"
    );
}

#[tokio::test]
async fn stale_attached_session_cannot_drive_cross_session_window_mutations() {
    let handler = RequestHandler::new();
    let attached = handler.create_session("identity-window-source").await;
    let other = handler.create_session("identity-window-other").await;
    let stale_id = replace_session(&handler, &attached).await;

    let move_response = run_as_stale_attached_session(
        &handler,
        attached.clone(),
        stale_id,
        Request::MoveWindow(MoveWindowRequest {
            source: Some(WindowTarget::with_window(attached.clone(), 0)),
            target: MoveWindowTarget::Window(WindowTarget::with_window(other.clone(), 1)),
            renumber: false,
            kill_destination: false,
            detached: true,
            after: false,
            before: false,
        }),
    )
    .await;
    assert!(
        matches!(move_response, Response::Error(_)),
        "{move_response:?}"
    );

    let swap_response = run_as_stale_attached_session(
        &handler,
        attached.clone(),
        stale_id,
        Request::SwapWindow(SwapWindowRequest {
            source: WindowTarget::with_window(attached.clone(), 0),
            target: WindowTarget::with_window(other.clone(), 0),
            detached: true,
        }),
    )
    .await;
    assert!(
        matches!(swap_response, Response::Error(_)),
        "{swap_response:?}"
    );

    let link_response = run_as_stale_attached_session(
        &handler,
        attached.clone(),
        stale_id,
        Request::LinkWindow(LinkWindowRequest {
            kill_destination: true,
            ..Fixture::fixture((
                WindowTarget::with_window(other, 0),
                WindowTarget::with_window(attached.clone(), 0),
            ))
        }),
    )
    .await;
    assert!(
        matches!(link_response, Response::Error(_)),
        "{link_response:?}"
    );
    assert_ne!(handler.session_id_for_test(&attached).await, stale_id);
}

#[tokio::test]
async fn attached_queue_can_still_mutate_explicit_other_sessions() {
    let handler = RequestHandler::new();
    let attached = handler.create_session("identity-explicit-attached").await;
    let source = handler.create_session("identity-explicit-source").await;
    let destination = handler
        .create_session("identity-explicit-destination")
        .await;
    let attached_id = handler.session_id_for_test(&attached).await;

    let response = with_expected_attach_and_session_identity(
        ActiveAttachIdentity::new(990_002, 1, attached_id),
        attached,
        attached_id,
        handler.handle(Request::LinkWindow(LinkWindowRequest::fixture((
            WindowTarget::with_window(source, 0),
            WindowTarget::with_window(destination.clone(), 1),
        )))),
    )
    .await;

    assert!(matches!(response, Response::LinkWindow(_)), "{response:?}");
    assert!(handler
        .state
        .lock()
        .await
        .sessions
        .session(&destination)
        .and_then(|session| session.window_at(1))
        .is_some());
}
