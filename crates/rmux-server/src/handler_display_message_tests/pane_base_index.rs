use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};
use rmux_core::formats::{DEFAULT_LIST_PANES_ALL_FORMAT, DEFAULT_LIST_PANES_SESSION_FORMAT};
use rmux_proto::{CommandOutput, ListPanesRequest};

fn stdout_string(output: &CommandOutput) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is utf-8")
}

fn default_list_pane_labels(output: &CommandOutput) -> Vec<&str> {
    std::str::from_utf8(output.stdout())
        .expect("list-panes output is utf-8")
        .lines()
        .map(|line| {
            line.split_once(": [")
                .expect("default list-panes line has a geometry suffix")
                .0
        })
        .collect()
}

#[tokio::test]
async fn pane_index_formats_use_window_local_pane_base_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, (&alpha, TerminalSize { cols: 20, rows: 6 })).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::PaneBaseIndex,
            "10",
        )
        .await;

    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: alpha.clone(),
            target_window_index: Some(0),
            format: Some("#{pane_index}:#{pane-base-index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(stdout_string(&list.output), "10:10\n11:10\n");

    for (format, expected) in [
        (None, vec!["10", "11"]),
        (
            Some(DEFAULT_LIST_PANES_SESSION_FORMAT.to_owned()),
            vec!["0.10", "0.11"],
        ),
        (
            Some(DEFAULT_LIST_PANES_ALL_FORMAT.to_owned()),
            vec!["alpha:0.10", "alpha:0.11"],
        ),
    ] {
        let list = TestRequest::send_ok(
            &handler,
            ListPanesRequest {
                target: alpha.clone(),
                target_window_index: Some(0),
                format,
                filter: None,
                sort_order: None,
                reversed: false,
            },
        )
        .await;
        assert_eq!(default_list_pane_labels(&list.output), expected);
    }

    let output = handler
        .display_print(PaneTarget::with_window(alpha, 0, 1), "#{pane_index}:#P")
        .await;
    assert_eq!(output, b"11:11\n");
}

#[tokio::test]
async fn default_list_panes_uses_global_pane_base_index_without_window_override() {
    let handler = RequestHandler::new();
    let beta = session_name("beta");
    handler
        .set_option(ScopeSelector::Global, OptionName::PaneBaseIndex, "7")
        .await;
    SessionSpec::create(&handler, (&beta, TerminalSize { cols: 20, rows: 6 })).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&beta)).await;

    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: beta.clone(),
            target_window_index: Some(0),
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(default_list_pane_labels(&list.output), ["7", "8"]);

    let list = TestRequest::send_ok(
        &handler,
        ListPanesRequest {
            target: beta,
            target_window_index: Some(0),
            format: Some("#{pane_index}:#{pane-base-index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        },
    )
    .await;
    assert_eq!(stdout_string(&list.output), "7:7\n8:7\n");
}

#[tokio::test]
async fn target_resolution_uses_visible_pane_base_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, (&alpha, TerminalSize { cols: 20, rows: 6 })).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    handler
        .set_option(
            ScopeSelector::Window(WindowTarget::with_window(alpha.clone(), 0)),
            OptionName::PaneBaseIndex,
            "10",
        )
        .await;

    let resolved = handler
        .handle(Request::ResolveTarget(rmux_proto::ResolveTargetRequest {
            target: Some("alpha:0.11".to_owned()),
            target_type: rmux_proto::ResolveTargetType::Pane,
            window_index: false,
            prefer_unattached: false,
        }))
        .await;
    let Response::ResolveTarget(resolved) = resolved else {
        panic!("visible pane target should resolve, got {resolved:?}");
    };
    assert_eq!(
        resolved.target,
        Target::Pane(PaneTarget::with_window(alpha, 0, 1))
    );
}
