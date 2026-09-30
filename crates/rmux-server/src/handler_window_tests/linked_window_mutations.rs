use super::linked_pane_selection::LinkedPaneFixture;
use super::*;

use crate::handler::prompt_support::PromptInputEvent;
use crate::test_fixtures::TestRequest;
use rmux_core::command_parser::CommandParser;
use rmux_proto::{
    PaneResizeRequest, ResizePaneAdjustment, ResizePaneRequest, SplitWindowExtRequest,
};

async fn linked_mutation_fixture(handler: &RequestHandler, label: &str) -> LinkedPaneFixture {
    handler
        .set_option(ScopeSelector::Global, OptionName::Status, "off")
        .await;

    let owner = create_session(handler, format!("{label}-owner")).await;
    let grouped_peer = create_grouped_session(handler, format!("{label}-grouped"), &owner).await;
    // Quiet, as in `linked_two_pane_fixture`: the default interactive shell's startup is
    // unbounded in time, and its activity races the pane mutations these tests commit.
    let split = TestRequest::send_ok(
        handler,
        SplitWindowExtRequest {
            command: Some(quiet_command()),
            ..Fixture::fixture(&owner)
        },
    )
    .await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&split.pane)
        .await;

    let fixture = LinkedPaneFixture::link(handler, owner, grouped_peer, label).await;
    assert_alias_windows_identical(handler, &fixture).await;
    fixture
}

async fn assert_alias_windows_identical(handler: &RequestHandler, fixture: &LinkedPaneFixture) {
    let state = handler.state.lock().await;
    let expected = state
        .sessions
        .session(&fixture.owner)
        .and_then(|session| session.window_at(0))
        .expect("owner window exists")
        .clone();
    for target in fixture.targets() {
        assert_eq!(
            state
                .sessions
                .session(target.session_name())
                .and_then(|session| session.window_at(target.window_index()))
                .expect("window alias exists"),
            &expected,
            "window model diverged for {target}"
        );
    }
}

async fn assert_alias_zoom(
    handler: &RequestHandler,
    fixture: &LinkedPaneFixture,
    zoomed: bool,
    active_pane: u32,
) {
    assert_alias_windows_identical(handler, fixture).await;
    let state = handler.state.lock().await;
    for target in fixture.targets() {
        let window = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .expect("window alias exists");
        assert_eq!(window.is_zoomed(), zoomed, "zoom diverged for {target}");
        assert_eq!(
            window.active_pane_index(),
            active_pane,
            "active pane diverged for {target}"
        );
    }
}

async fn assert_active_runtime_and_lifecycle_match(
    handler: &RequestHandler,
    fixture: &LinkedPaneFixture,
) {
    let (active_pane, expected_size, lifecycle_size) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&fixture.owner)
            .and_then(|session| session.window_at(0))
            .expect("owner window exists");
        let active_pane = window.active_pane_index();
        let pane = window.pane(active_pane).expect("active pane exists");
        (
            active_pane,
            TerminalSize::new(pane.geometry().cols(), pane.geometry().rows()),
            state
                .pane_lifecycle(pane.id())
                .expect("active pane lifecycle exists")
                .dimensions(),
        )
    };
    assert_eq!(lifecycle_size, expected_size);
    let active = PaneTarget::with_window(fixture.owner.clone(), 0, active_pane);
    assert_eq!(
        handler.pane_terminal_size_for_test(&active).await,
        expected_size,
        "shared PTY size must commit with the linked window model"
    );
}

#[tokio::test]
async fn cli_and_sdk_zoom_commit_linked_model_runtime_and_lifecycle_together() {
    let handler = RequestHandler::new();
    let fixture = linked_mutation_fixture(&handler, "linked-zoom").await;
    let resize_count_before = handler
        .state
        .lock()
        .await
        .window_runtime_resize_count_for_test();

    TestRequest::send_ok(
        &handler,
        ResizePaneRequest {
            target: PaneTarget::with_window(fixture.linked_peer.clone(), 1, 1),
            adjustment: ResizePaneAdjustment::Zoom,
        },
    )
    .await;
    assert_alias_zoom(&handler, &fixture, true, 1).await;
    assert_active_runtime_and_lifecycle_match(&handler, &fixture).await;

    let unzoomed = handler
        .handle(Request::PaneResize(PaneResizeRequest {
            target: PaneTargetRef::by_id(fixture.owner.clone(), fixture.pane_one_id),
            adjustment: ResizePaneAdjustment::Zoom,
        }))
        .await;
    assert!(matches!(unzoomed, Response::ResizePane(_)), "{unzoomed:?}");
    assert_alias_zoom(&handler, &fixture, false, 1).await;
    assert_active_runtime_and_lifecycle_match(&handler, &fixture).await;

    assert_eq!(
        handler
            .state
            .lock()
            .await
            .window_runtime_resize_count_for_test(),
        resize_count_before + 2,
        "CLI and stable-id SDK zooms must each resize the shared window runtime once"
    );
}

#[tokio::test]
async fn pane_selection_resize_failure_rolls_back_every_alias_and_the_shared_runtime() {
    let handler = RequestHandler::new();
    let fixture = linked_mutation_fixture(&handler, "linked-select-rollback").await;
    let pane_zero = PaneTarget::with_window(fixture.owner.clone(), 0, 0);
    let terminal_size_before = handler.pane_terminal_size_for_test(&pane_zero).await;
    let resize_count_before = {
        let mut state = handler.state.lock().await;
        let count = state.window_runtime_resize_count_for_test();
        state.fail_next_resize_for_test();
        count
    };

    let pane_one = PaneTarget::with_window(fixture.owner.clone(), 0, 1);
    let response = handler
        .handle(Request::SelectPane(Box::new(SelectPaneRequest::fixture(
            pane_one,
        ))))
        .await;
    assert_eq!(
        response,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Server(
                "injected pane terminal resize failure".to_owned(),
            ),
        })
    );
    assert_alias_zoom(&handler, &fixture, false, 0).await;
    assert_eq!(
        handler.pane_terminal_size_for_test(&pane_zero).await,
        terminal_size_before
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .window_runtime_resize_count_for_test(),
        resize_count_before + 2,
        "failed commit and runtime rollback must each issue one bounded resize"
    );
}

#[tokio::test]
async fn split_window_zoom_commits_the_new_zoomed_window_to_every_alias() {
    let handler = RequestHandler::new();
    let fixture = linked_mutation_fixture(&handler, "linked-split-zoom").await;

    let split = TestRequest::send_ok(
        &handler,
        SplitWindowExtRequest {
            command: Some(quiet_command()),
            preserve_zoom: true,
            ..Fixture::fixture(PaneTarget::with_window(fixture.linked_peer.clone(), 1, 1))
        },
    )
    .await;
    assert_alias_windows_identical(&handler, &fixture).await;
    let state = handler.state.lock().await;
    for target in fixture.targets() {
        let window = state
            .sessions
            .session(target.session_name())
            .and_then(|session| session.window_at(target.window_index()))
            .expect("window alias exists");
        assert!(window.is_zoomed(), "split zoom diverged for {target}");
        assert_eq!(window.active_pane_index(), split.pane.pane_index());
    }
    drop(state);
    let new_pane = PaneTarget::with_window(fixture.owner.clone(), 0, split.pane.pane_index());
    assert_eq!(
        handler.pane_terminal_size_for_test(&new_pane).await,
        TerminalSize::new(120, 40)
    );
}

#[tokio::test]
async fn mode_tree_zoom_and_dismissal_commit_every_linked_alias() {
    let handler = RequestHandler::new();
    let fixture = linked_mutation_fixture(&handler, "linked-mode-tree-zoom").await;
    let attach_pid = std::process::id().saturating_add(9_141);
    let _control_rx = handler.attach_client(attach_pid, &fixture.owner).await;

    let parsed = CommandParser::new()
        .parse_arguments(["choose-tree", "-Zw"])
        .expect("zoomed choose-tree parses");
    let command = RequestHandler::parse_mode_tree_queue_command(parsed.commands()[0].clone())
        .expect("zoomed choose-tree command is valid")
        .expect("choose-tree is recognized");
    handler
        .execute_queued_mode_tree(
            attach_pid,
            command,
            &crate::handler::scripting_support::QueueExecutionContext::without_caller_cwd(),
        )
        .await
        .expect("zoomed choose-tree opens");
    assert_alias_zoom(&handler, &fixture, true, 0).await;
    assert_active_runtime_and_lifecycle_match(&handler, &fixture).await;

    assert!(handler
        .handle_mode_tree_key_event(attach_pid, PromptInputEvent::Char('q'))
        .await
        .expect("q dismisses choose-tree"));
    assert_alias_zoom(&handler, &fixture, false, 0).await;
    assert_active_runtime_and_lifecycle_match(&handler, &fixture).await;
}

#[tokio::test]
async fn non_zoom_window_geometry_mutations_remain_transactional_across_aliases() {
    let handler = RequestHandler::new();
    let fixture = linked_mutation_fixture(&handler, "linked-layout").await;

    TestRequest::send_ok(
        &handler,
        ResizePaneRequest {
            target: PaneTarget::with_window(fixture.linked_peer.clone(), 1, 0),
            adjustment: ResizePaneAdjustment::AbsoluteHeight { rows: 12 },
        },
    )
    .await;
    assert_alias_windows_identical(&handler, &fixture).await;

    let layout = handler
        .handle(Request::NextLayout(rmux_proto::NextLayoutRequest {
            target: WindowTarget::with_window(fixture.grouped_peer.clone(), 0),
        }))
        .await;
    assert!(matches!(layout, Response::NextLayout(_)), "{layout:?}");
    assert_alias_windows_identical(&handler, &fixture).await;

    TestRequest::send_ok(
        &handler,
        RotateWindowRequest {
            target: WindowTarget::with_window(fixture.owner.clone(), 0),
            direction: RotateWindowDirection::Down,
            restore_zoom: false,
        },
    )
    .await;
    assert_alias_windows_identical(&handler, &fixture).await;
    assert_active_runtime_and_lifecycle_match(&handler, &fixture).await;
}
