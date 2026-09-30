use super::pane_group_transfer_tests::{create_grouped_session, create_session};
use super::RequestHandler;
use rmux_core::WindowId;
use rmux_proto::{
    BindKeyRequest, HookName, JoinPaneRequest, LinkWindowRequest, MovePaneRequest,
    NewWindowRequest, OptionName, OptionScopeSelector, PaneTarget, Request, Response,
    ScopeSelector, SendKeysExtRequest, SendKeysResponse, SessionName, SetHookRequest, WindowTarget,
};

use crate::test_fixtures::{Fixture, TestRequest};

const SURVIVOR_OPTION: &str = "@w13-m10-survivor";

#[derive(Clone, Copy)]
enum TransferCommand {
    Join,
    Move,
}

impl TransferCommand {
    const fn label(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Move => "move",
        }
    }

    fn request(self, source: PaneTarget, target: PaneTarget) -> Request {
        let key = (source, target);
        match self {
            Self::Join => Request::JoinPane(JoinPaneRequest::fixture(key)),
            Self::Move => Request::MovePane(MovePaneRequest::fixture(key)),
        }
    }
}

async fn create_window(
    handler: &RequestHandler,
    session_name: &SessionName,
    window_index: u32,
    name: &str,
) {
    handler
        .create_window(NewWindowRequest {
            name: Some(name.to_owned()),
            target_window_index: Some(window_index),
            ..Fixture::fixture(session_name)
        })
        .await;
}

async fn set_renumber(handler: &RequestHandler, session_name: &SessionName) {
    handler
        .set_option(
            ScopeSelector::Session(session_name.clone()),
            OptionName::RenumberWindows,
            "on",
        )
        .await;
}

async fn mark_survivor(handler: &RequestHandler, target: &WindowTarget, marker: &str) -> WindowId {
    handler
        .set_option_by_name(
            OptionScopeSelector::Window(target.clone()),
            SURVIVOR_OPTION,
            marker,
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Window(target.clone()),
            OptionName::AutomaticRename,
            "off",
        )
        .await;
    TestRequest::send_ok(
        handler,
        SetHookRequest::fixture((
            ScopeSelector::Window(target.clone()),
            HookName::WindowLayoutChanged,
            format!("display-message {marker}").as_str(),
        )),
    )
    .await;

    handler.window_id_for_test(target).await
}

async fn assert_renumbered_survivor(
    handler: &RequestHandler,
    session_name: &SessionName,
    expected_id: WindowId,
    marker: &str,
) {
    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(session_name)
        .expect("source session survives");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![0, 1],
        "{session_name} must be contiguous after the source window disappears"
    );
    let survivor = session.window_at(1).expect("survivor is reindexed to one");
    assert_eq!(survivor.id(), expected_id);
    assert_eq!(survivor.name(), Some("SURVIVOR"));
    assert!(!survivor.automatic_rename());

    let target = WindowTarget::with_window(session_name.clone(), 1);
    assert_eq!(
        state
            .options
            .explicit_value_by_name(
                &OptionScopeSelector::Window(target.clone()),
                SURVIVOR_OPTION,
            )
            .expect("valid user option")
            .1
            .as_deref(),
        Some(marker)
    );
    assert_eq!(
        state
            .hooks
            .window_bindings_view(&target, Some(HookName::WindowLayoutChanged))
            .iter()
            .map(|binding| binding.command())
            .collect::<Vec<_>>(),
        vec![format!("display-message {marker}")]
    );
}

fn transfer_target(response: Response, command: TransferCommand) -> PaneTarget {
    match (command, response) {
        (TransferCommand::Join, Response::JoinPane(response)) => response.target,
        (TransferCommand::Move, Response::MovePane(response)) => response.target,
        (_, response) => panic!("{}-pane failed: {response:?}", command.label()),
    }
}

async fn prepare_renumber_source(
    handler: &RequestHandler,
    label: &str,
    marker: &str,
) -> (SessionName, WindowId) {
    let session = create_session(handler, label).await;
    create_window(handler, &session, 1, "SOURCE").await;
    create_window(handler, &session, 2, "SURVIVOR").await;
    set_renumber(handler, &session).await;
    let _ = mark_survivor(
        handler,
        &WindowTarget::with_window(session.clone(), 1),
        "discarded-source",
    )
    .await;
    let survivor_id = mark_survivor(
        handler,
        &WindowTarget::with_window(session.clone(), 2),
        marker,
    )
    .await;
    (session, survivor_id)
}

async fn run_same_session_case(command: TransferCommand) {
    let handler = RequestHandler::new();
    let (session, survivor_id) = prepare_renumber_source(
        &handler,
        &format!("w13-m10-{}-same", command.label()),
        "same-survivor",
    )
    .await;

    let response = handler
        .handle(command.request(
            PaneTarget::with_window(session.clone(), 1, 0),
            PaneTarget::with_window(session.clone(), 2, 0),
        ))
        .await;
    assert_eq!(
        transfer_target(response, command),
        PaneTarget::with_window(session.clone(), 1, 1),
        "response must follow the target window from index 2 to index 1"
    );
    assert_renumbered_survivor(&handler, &session, survivor_id, "same-survivor").await;
}

async fn run_cross_session_case(command: TransferCommand) {
    let handler = RequestHandler::new();
    let source = create_session(
        &handler,
        &format!("w13-m10-{}-cross-source", command.label()),
    )
    .await;
    let destination = create_session(
        &handler,
        &format!("w13-m10-{}-cross-destination", command.label()),
    )
    .await;
    create_window(&handler, &source, 1, "SOURCE").await;
    create_window(&handler, &source, 2, "SURVIVOR").await;
    set_renumber(&handler, &source).await;
    let _ = mark_survivor(
        &handler,
        &WindowTarget::with_window(source.clone(), 1),
        "discarded-source",
    )
    .await;
    let survivor_id = mark_survivor(
        &handler,
        &WindowTarget::with_window(source.clone(), 2),
        "cross-survivor",
    )
    .await;

    let response = handler
        .handle(command.request(
            PaneTarget::with_window(source.clone(), 1, 0),
            PaneTarget::with_window(destination, 0, 0),
        ))
        .await;
    let _ = transfer_target(response, command);
    assert_renumbered_survivor(&handler, &source, survivor_id, "cross-survivor").await;
}

async fn run_grouped_case(command: TransferCommand) {
    let handler = RequestHandler::new();
    let owner = create_session(
        &handler,
        &format!("w13-m10-{}-group-owner", command.label()),
    )
    .await;
    create_window(&handler, &owner, 1, "SOURCE").await;
    create_window(&handler, &owner, 2, "SURVIVOR").await;
    let peer = create_grouped_session(
        &handler,
        &format!("w13-m10-{}-group-peer", command.label()),
        &owner,
    )
    .await;
    set_renumber(&handler, &owner).await;
    set_renumber(&handler, &peer).await;
    let _ = mark_survivor(
        &handler,
        &WindowTarget::with_window(owner.clone(), 1),
        "discarded-source",
    )
    .await;
    let owner_survivor = mark_survivor(
        &handler,
        &WindowTarget::with_window(owner.clone(), 2),
        "group-survivor",
    )
    .await;

    let response = handler
        .handle(command.request(
            PaneTarget::with_window(owner.clone(), 1, 0),
            PaneTarget::with_window(owner.clone(), 0, 0),
        ))
        .await;
    let _ = transfer_target(response, command);
    assert_renumbered_survivor(&handler, &owner, owner_survivor, "group-survivor").await;
    assert_renumbered_survivor(&handler, &peer, owner_survivor, "group-survivor").await;
}

async fn run_linked_case(command: TransferCommand) {
    let handler = RequestHandler::new();
    let source = create_session(
        &handler,
        &format!("w13-m10-{}-linked-source", command.label()),
    )
    .await;
    let alias = create_session(
        &handler,
        &format!("w13-m10-{}-linked-alias", command.label()),
    )
    .await;
    let destination = create_session(
        &handler,
        &format!("w13-m10-{}-linked-destination", command.label()),
    )
    .await;
    create_window(&handler, &source, 1, "SOURCE").await;
    create_window(&handler, &source, 2, "SURVIVOR").await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest::fixture((
            WindowTarget::with_window(source.clone(), 1),
            WindowTarget::with_window(alias.clone(), 1),
        )),
    )
    .await;
    create_window(&handler, &alias, 2, "SURVIVOR").await;
    set_renumber(&handler, &source).await;
    set_renumber(&handler, &alias).await;
    let _ = mark_survivor(
        &handler,
        &WindowTarget::with_window(source.clone(), 1),
        "discarded-source",
    )
    .await;
    let source_survivor = mark_survivor(
        &handler,
        &WindowTarget::with_window(source.clone(), 2),
        "source-survivor",
    )
    .await;
    let alias_survivor = mark_survivor(
        &handler,
        &WindowTarget::with_window(alias.clone(), 2),
        "alias-survivor",
    )
    .await;

    let response = handler
        .handle(command.request(
            PaneTarget::with_window(source.clone(), 1, 0),
            PaneTarget::with_window(destination, 0, 0),
        ))
        .await;
    let _ = transfer_target(response, command);
    assert_renumbered_survivor(&handler, &source, source_survivor, "source-survivor").await;
    assert_renumbered_survivor(&handler, &alias, alias_survivor, "alias-survivor").await;
}

// tmux 3.7b measured on 2026-07-26: a join/move which consumes the
// source window applies renumber-windows to surviving source-session slots.
#[tokio::test]
async fn join_and_move_renumber_destroyed_same_session_source() {
    for command in [TransferCommand::Join, TransferCommand::Move] {
        run_same_session_case(command).await;
    }
}

#[tokio::test]
async fn join_and_move_renumber_destroyed_cross_session_source() {
    for command in [TransferCommand::Join, TransferCommand::Move] {
        run_cross_session_case(command).await;
    }
}

#[tokio::test]
async fn join_and_move_renumber_destroyed_grouped_source_family() {
    for command in [TransferCommand::Join, TransferCommand::Move] {
        run_grouped_case(command).await;
    }
}

#[tokio::test]
async fn join_and_move_renumber_destroyed_linked_source_family() {
    for command in [TransferCommand::Join, TransferCommand::Move] {
        run_linked_case(command).await;
    }
}

#[tokio::test]
async fn bind_key_join_and_move_renumber_destroyed_source() {
    for command in [TransferCommand::Join, TransferCommand::Move] {
        let handler = RequestHandler::new();
        let marker = format!("binding-{}-survivor", command.label());
        let (session, survivor_id) = prepare_renumber_source(
            &handler,
            &format!("w13-m10-{}-binding", command.label()),
            &marker,
        )
        .await;
        let _control_rx = handler.attach_client(std::process::id(), &session).await;
        TestRequest::send_ok(
            &handler,
            BindKeyRequest::fixture((
                "prefix",
                "x",
                [
                    format!("{}-pane", command.label()),
                    "-d".to_owned(),
                    "-s".to_owned(),
                    format!("{session}:1.0"),
                    "-t".to_owned(),
                    format!("{session}:0.0"),
                ],
            )),
        )
        .await;

        let response = handler
            .handle(Request::SendKeysExt(SendKeysExtRequest::fixture((
                PaneTarget::with_window(session.clone(), 0, 0),
                ["C-b", "x"],
            ))))
            .await;
        assert_eq!(
            response,
            Response::SendKeys(SendKeysResponse { key_count: 2 })
        );
        assert_renumbered_survivor(&handler, &session, survivor_id, &marker).await;
    }
}
