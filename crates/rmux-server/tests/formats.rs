use std::error::Error;
mod common;

use common::{create_session, send, send_ok, session_name, start_server, Fixture, TestHarness};
use rmux_proto::{
    DisplayMessageRequest, ListWindowsRequest, NewSessionRequest, NewWindowRequest, Target,
    TerminalSize,
};

const SESSION_SIZE: TerminalSize = TerminalSize {
    cols: 120,
    rows: 40,
};

const FORMAT_FIELD_SEPARATOR: char = '\x1f';

fn default_shell_window_name() -> String {
    "bash".to_owned()
}

fn assert_window_format_line(
    line: &str,
    window_index: &str,
    window_name: &str,
    raw_flags: &str,
    active: &str,
    last: &str,
    conditional: &str,
) {
    let fields: Vec<&str> = line.split(FORMAT_FIELD_SEPARATOR).collect();
    assert_eq!(fields.len(), 14, "unexpected format fields: {fields:?}");
    assert_eq!(fields[0], "alpha");
    assert_eq!(fields[1], "2");
    assert_eq!(fields[2], "0");
    assert_eq!(fields[3], "x");
    assert_eq!(fields[4], window_index);
    assert_eq!(fields[5], window_name);
    assert_eq!(fields[6], raw_flags);
    assert_eq!(fields[7], active);
    assert_eq!(fields[8], last);
    assert_eq!(fields[9], format!("@{window_index}"));
    assert_eq!(fields[10], "");
    assert_eq!(fields[11], format!("{window_index}{window_name}alpha"));
    assert!(
        !fields[12].contains("#{"),
        "pane title should be resolved, not leaked as a format token"
    );
    assert_eq!(fields[13], conditional);
}

fn is_unix_pty_path(path: &str) -> bool {
    path.starts_with("/dev/pts/") || path.starts_with("/dev/ttys")
}

#[tokio::test(flavor = "multi_thread")]
async fn list_windows_uses_shared_formatter_through_real_socket() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("formats-list-windows");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(
        harness.socket_path(),
        NewSessionRequest {
            size: Some(SESSION_SIZE),
            environment: Some(vec![
                "SHELL=/bin/bash".to_owned(),
                "TERM_PROGRAM=tmux".to_owned(),
            ]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await?;
    send_ok(harness.socket_path(), attached_logs_window(&alpha)).await?;

    let listed = send(
        harness.socket_path(),
        ListWindowsRequest::fixture((
            alpha,
            "#{session_name}\x1f#{session_windows}\x1f#{session_attached}\x1f#{session_width}x#{session_height}\x1f#{window_index}\x1f#{window_name}\x1f#{window_raw_flags}\x1f#{window_active}\x1f#{window_last_flag}\x1f#{window_id}\x1f#{missing}\x1f#I#W#S\x1f#{=21:pane_title}\x1f#{?window_active,yes,no}",
        )),
    )
    .await?;

    let output = listed
        .command_output()
        .expect("list-windows returns command output");
    let stdout = std::str::from_utf8(output.stdout()).expect("list-windows output is utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected list-windows output: {stdout:?}");
    assert_window_format_line(
        lines[0].trim_end(),
        "0",
        &default_shell_window_name(),
        "-",
        "0",
        "1",
        "no",
    );
    assert_window_format_line(lines[1].trim_end(), "1", "logs", "*", "1", "0", "yes");

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_conditionals_expand_inner_templates_through_real_socket(
) -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("formats-nested-conditionals");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(harness.socket_path(), (&alpha, SESSION_SIZE)).await?;
    send_ok(harness.socket_path(), attached_logs_window(&alpha)).await?;

    let listed = send(
        harness.socket_path(),
        ListWindowsRequest::fixture((
            alpha,
            "#{?window_active,#{window_name},#{?window_last_flag,last,#{session_name}}}",
        )),
    )
    .await?;

    let output = listed
        .command_output()
        .expect("list-windows returns command output");
    assert_eq!(
        std::str::from_utf8(output.stdout()).expect("list-windows output is utf-8"),
        "last\nlogs\n"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn display_message_session_target_includes_active_pane_runtime_context(
) -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("formats-display-session-pane-context");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(harness.socket_path(), &alpha).await?;

    let displayed = send(
        harness.socket_path(),
        DisplayMessageRequest {
            target: Some(Target::Session(alpha)),
            ..Fixture::fixture(
                "#{session_name}|#{window_index}|#{pane_index}|#{pane_current_path}|#{pane_pid}|#{pane_tty}|#{socket_path}",
            )
        },
    )
    .await?;

    let output = displayed
        .command_output()
        .expect("display-message -p returns command output");
    let stdout = std::str::from_utf8(output.stdout()).expect("display-message output is utf-8");
    let fields: Vec<&str> = stdout.trim_end().split('|').collect();
    assert_eq!(fields.len(), 7);
    assert_eq!(fields[0], "alpha");
    assert_eq!(fields[1], "0");
    assert_eq!(fields[2], "0");
    assert!(!fields[3].is_empty(), "pane_current_path must be populated");
    // A detached `new-session` pane is an idle embedded shell: it has no OS child, so it names no
    // foreground process group. Empty is the honest answer, and it must stay empty rather than
    // leak the daemon's own pid, which a caller could signal.
    assert!(
        fields[4].is_empty(),
        "idle pane_pid must be empty, got {:?}",
        fields[4]
    );
    assert!(is_unix_pty_path(fields[5]), "pane_tty must be a pty");
    assert_eq!(fields[6], harness.socket_path().to_string_lossy());

    handle.shutdown().await?;
    Ok(())
}

/// A `new-window -n logs` in `session` that becomes its current window.
fn attached_logs_window(session: &rmux_proto::SessionName) -> NewWindowRequest {
    NewWindowRequest {
        name: Some("logs".to_owned()),
        detached: false,
        ..Fixture::fixture(session)
    }
}
