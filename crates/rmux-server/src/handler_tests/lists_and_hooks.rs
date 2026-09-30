use super::*;
use crate::test_fixtures::{Fixture, SessionSpec, Sizeless, TestRequest};

#[tokio::test]
async fn list_sessions_returns_empty_output_when_no_sessions_exist() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::ListSessions(ListSessionsRequest {
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
        }))
        .await;

    let output = response
        .command_output()
        .expect("list-sessions returns command output");
    assert!(output.stdout().is_empty());
}

#[tokio::test]
async fn list_sessions_sorts_sessions_by_name() {
    let handler = RequestHandler::new();
    for name in ["charlie", "alpha", "bravo"] {
        SessionSpec::create(&handler, Sizeless(name)).await;
    }

    let response = handler
        .handle(Request::ListSessions(ListSessionsRequest {
            format: Some("#{session_name}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        }))
        .await;

    let output = response
        .command_output()
        .expect("list-sessions returns command output");
    assert_eq!(
        std::str::from_utf8(output.stdout()).expect("utf-8"),
        "alpha\nbravo\ncharlie\n"
    );
}

#[tokio::test]
async fn list_sessions_format_uses_each_sessions_active_pane_context() {
    let handler = RequestHandler::new();
    // Under this handler's own seed, not the process temp directory. `working_directory` is a
    // NAMED start directory, and a pane now opens over a snapshot of the one tree this daemon
    // leases: a sibling of that tree is a place the daemon genuinely cannot start a pane in.
    let root = crate::pane_terminals::seed_scratch_dir(&handler, "list-sessions-context");
    let alpha_dir = canonical_context_path(root.child("alpha").path());
    let beta_dir = canonical_context_path(root.child("beta").path());

    for (name, path) in [("alpha", &alpha_dir), ("beta", &beta_dir)] {
        SessionSpec::create(
            &handler,
            NewSessionExtRequest {
                working_directory: Some(path.to_string_lossy().into_owned()),
                size: None,
                ..Fixture::fixture(name)
            },
        )
        .await;
    }

    let response = handler
        .handle(Request::ListSessions(ListSessionsRequest {
            format: Some("#{session_name}|#{session_path}|#{pane_current_path}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        }))
        .await;

    let output = response
        .command_output()
        .expect("list-sessions returns command output");
    let stdout = std::str::from_utf8(output.stdout()).expect("utf-8");
    assert_eq!(
        stdout,
        format!(
            "alpha|{}|{}\nbeta|{}|{}\n",
            rendered_context_path(&alpha_dir),
            rendered_context_path(&alpha_dir),
            rendered_context_path(&beta_dir),
            rendered_context_path(&beta_dir)
        )
    );
}

#[tokio::test]
async fn session_path_stays_at_session_cwd_when_pane_cwds_differ() {
    let handler = RequestHandler::new();
    // Same reason as above: both directories have to be inside the seed this handler leased.
    let root = crate::pane_terminals::seed_scratch_dir(&handler, "session-path-context");
    let session_dir = canonical_context_path(root.child("session").path());
    let split_dir = canonical_context_path(root.child("split").path());
    let session = session_name("session-path-context");

    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            working_directory: Some(session_dir.to_string_lossy().into_owned()),
            size: None,
            ..Fixture::fixture(&session)
        },
    )
    .await;

    TestRequest::send_ok(
        &handler,
        rmux_proto::SplitWindowExtRequest {
            start_directory: Some(split_dir.clone()),
            detached: true,
            ..Fixture::fixture(&session)
        },
    )
    .await;

    let response = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: session,
            format: Some("#{pane_index}|#{session_path}|#{pane_current_path}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })))
        .await;
    let output = response
        .command_output()
        .expect("list-panes returns command output");
    let stdout = std::str::from_utf8(output.stdout()).expect("utf-8");
    assert_eq!(
        stdout,
        format!(
            "0|{session_path}|{session_path}\n1|{session_path}|{split_path}\n",
            session_path = rendered_context_path(&session_dir),
            split_path = rendered_context_path(&split_dir),
        )
    );
}

fn canonical_context_path(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).expect("canonicalize context directory")
}

fn rendered_context_path(path: &std::path::Path) -> String {
    path.display().to_string()
}

#[tokio::test]
async fn list_panes_returns_error_for_missing_session() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: session_name("missing"),
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })))
        .await;

    assert_eq!(
        response,
        Response::Error(ErrorResponse {
            error: RmuxError::SessionNotFound("missing".to_owned()),
        })
    );
}

fn format_value<'a>(formats: &'a [(String, String)], name: &str) -> Option<&'a str> {
    formats
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str())
}

#[test]
fn after_hook_formats_preserve_repeated_flag_values() {
    let parsed =
        parse_command_string("new-window -d -e FOO=1 -e BAR=2 -t alpha").expect("command parses");
    let command = parsed.commands().first().expect("one command");

    let formats = after_hook_format_values(HookName::AfterNewWindow, Some(command));

    assert_eq!(format_value(&formats, "hook"), Some("after-new-window"));
    assert_eq!(
        format_value(&formats, "hook_arguments"),
        Some("-d -e FOO=1 -e BAR=2 -t alpha")
    );
    assert_eq!(format_value(&formats, "hook_flag_d"), Some("1"));
    assert_eq!(format_value(&formats, "hook_flag_e"), Some("BAR=2"));
    assert_eq!(format_value(&formats, "hook_flag_e_0"), Some("FOO=1"));
    assert_eq!(format_value(&formats, "hook_flag_e_1"), Some("BAR=2"));
    assert_eq!(format_value(&formats, "hook_flag_t"), Some("alpha"));
    assert_eq!(format_value(&formats, "hook_flag_t_0"), Some("alpha"));
}
