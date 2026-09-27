use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

mod common;

use common::{
    create_session, send, send_ok, send_request, session_name, start_server, wait_for_capture,
    wait_for_socket_removal, Fixture, TestHarness,
};
use rmux_proto::{
    CapturePaneRequest, DeleteBufferRequest, DisplayMessageRequest, HasSessionRequest,
    IfShellRequest, KillServerRequest, ListBuffersRequest, ListPanesRequest, ListSessionsRequest,
    LoadBufferRequest, NewWindowRequest, PaneTarget, PasteBufferRequest, RenameSessionRequest,
    Request, Response, RunShellRequest, SaveBufferRequest, SendKeysRequest, SetBufferRequest,
    ShowBufferRequest, SplitWindowRequest, Target, TerminalSize, WaitForMode, WaitForRequest,
};
use tokio::time::sleep;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn buffer_capture_and_scripting_requests_round_trip_over_real_socket() -> Result<(), Box<dyn Error>>
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()?;
    runtime.block_on(buffer_capture_and_scripting_requests_round_trip())
}

async fn buffer_capture_and_scripting_requests_round_trip() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("request-buffer-capture-scripting");
    let handle = start_server(&harness).await?;
    let pane = PaneTarget::with_window(session_name("alpha"), 0, 0);
    let save_path = harness
        .socket_path()
        .parent()
        .expect("socket path should have a parent")
        .join("saved-buffer.txt");
    let load_path = harness
        .socket_path()
        .parent()
        .expect("socket path should have a parent")
        .join("loaded-buffer.txt");
    let paste_marker = "server_request_pasted_marker";
    let paste_command = format!("printf {paste_marker}");

    Box::pin(create_detached_test_session(harness.socket_path())).await?;
    Box::pin(exercise_buffer_file_requests(
        harness.socket_path(),
        &save_path,
        &load_path,
        &paste_command,
    ))
    .await?;
    Box::pin(exercise_paste_capture_and_delete_requests(
        harness.socket_path(),
        &pane,
        paste_marker,
    ))
    .await?;
    Box::pin(exercise_scripting_requests(harness.socket_path(), &pane)).await?;

    handle.shutdown().await?;
    Ok(())
}

async fn create_detached_test_session(socket_path: &Path) -> Result<(), Box<dyn Error>> {
    create_session(
        socket_path,
        (
            "alpha",
            TerminalSize {
                cols: 120,
                rows: 40,
            },
        ),
    )
    .await?;
    Ok(())
}

async fn exercise_buffer_file_requests(
    socket_path: &Path,
    save_path: &Path,
    load_path: &Path,
    paste_command: &str,
) -> Result<(), Box<dyn Error>> {
    send_ok(socket_path, named_buffer("empty", b"")).await?;
    let empty = send_request(
        socket_path,
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("empty".to_owned()),
        }),
    )
    .await?;
    assert!(matches!(empty, Response::Error(_)));

    send_ok(socket_path, named_buffer("delete-me", b"x")).await?;
    send_ok(
        socket_path,
        named_buffer("pastecmd", paste_command.as_bytes()),
    )
    .await?;

    let listed = send_request(
        socket_path,
        &Request::ListBuffers(ListBuffersRequest::default()),
    )
    .await?;
    let listed_stdout = std::str::from_utf8(
        listed
            .command_output()
            .expect("list-buffers returns command output")
            .stdout(),
    )?;
    assert!(!listed_stdout.contains("empty:"));
    assert!(listed_stdout.contains("delete-me:"));
    assert!(listed_stdout.contains("pastecmd:"));

    send_ok(
        socket_path,
        SaveBufferRequest::fixture((save_path.to_string_lossy(), "pastecmd")),
    )
    .await?;
    assert_eq!(fs::read_to_string(save_path)?, paste_command);

    fs::write(load_path, "loaded-over-socket")?;
    send_ok(
        socket_path,
        LoadBufferRequest::fixture((load_path.to_string_lossy(), "loaded")),
    )
    .await?;
    let loaded = send_request(
        socket_path,
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("loaded".to_owned()),
        }),
    )
    .await?;
    assert_eq!(
        loaded
            .command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"loaded-over-socket"
    );
    Ok(())
}

/// `set-buffer -b name content`.
fn named_buffer(name: &str, content: impl Into<Vec<u8>>) -> SetBufferRequest {
    SetBufferRequest {
        name: Some(name.to_owned()),
        ..Fixture::fixture(content)
    }
}

async fn exercise_paste_capture_and_delete_requests(
    socket_path: &Path,
    pane: &PaneTarget,
    paste_marker: &str,
) -> Result<(), Box<dyn Error>> {
    send_ok(
        socket_path,
        PasteBufferRequest {
            name: Some("pastecmd".to_owned()),
            ..Fixture::fixture(pane)
        },
    )
    .await?;
    send_ok(socket_path, SendKeysRequest::fixture((pane, ["Enter"]))).await?;
    let capture = wait_for_capture(socket_path, pane, paste_marker, CAPTURE_TIMEOUT).await?;
    assert!(capture.contains(paste_marker));

    send_ok(
        socket_path,
        CapturePaneRequest {
            print: false,
            buffer_name: Some("captured".to_owned()),
            ..Fixture::fixture(pane)
        },
    )
    .await?;
    let show_captured = send_request(
        socket_path,
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("captured".to_owned()),
        }),
    )
    .await?;
    assert!(std::str::from_utf8(
        show_captured
            .command_output()
            .expect("show-buffer returns output")
            .stdout(),
    )?
    .contains(paste_marker));

    send_ok(
        socket_path,
        DeleteBufferRequest {
            name: Some("delete-me".to_owned()),
        },
    )
    .await?;
    let listed_after_delete = send_request(
        socket_path,
        &Request::ListBuffers(ListBuffersRequest::default()),
    )
    .await?;
    assert!(!std::str::from_utf8(
        listed_after_delete
            .command_output()
            .expect("list-buffers returns command output")
            .stdout(),
    )?
    .contains("delete-me:"));
    Ok(())
}

async fn exercise_scripting_requests(
    socket_path: &Path,
    pane: &PaneTarget,
) -> Result<(), Box<dyn Error>> {
    let display = send(
        socket_path,
        DisplayMessageRequest {
            target: Some(Target::Pane(pane.clone())),
            ..Fixture::fixture("#{session_name}:#{pane_index}:#{missing}")
        },
    )
    .await?;
    assert_eq!(
        display
            .command_output()
            .expect("display-message -p returns command output")
            .stdout(),
        b"alpha:0:\n"
    );

    let shell = send(
        socket_path,
        RunShellRequest {
            show_stderr: true,
            ..Fixture::fixture("printf server-run-shell-output")
        },
    )
    .await?;
    match shell {
        Response::RunShell(response) => {
            assert_eq!(response.exit_status(), Some(0));
            let output = response
                .command_output()
                .expect("run-shell -E returns foreground command output");
            assert_eq!(
                output.stdout(),
                b"server-run-shell-output\n",
                "foreground run-shell -E should return stdout like tmux 3.7"
            );
        }
        other => panic!("expected run-shell response, got {other:?}"),
    }

    send_ok(
        socket_path,
        IfShellRequest {
            else_command: Some("set-buffer -b branch skipped".to_owned()),
            target: Some(Target::Pane(pane.clone())),
            ..Fixture::fixture(("#{pane_active}", "set-buffer -b branch chosen"))
        },
    )
    .await?;
    let branch = send_request(
        socket_path,
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("branch".to_owned()),
        }),
    )
    .await?;
    assert_eq!(
        branch
            .command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"chosen"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_listing_and_wait_for_requests_round_trip_over_real_socket(
) -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("request-rename-list-wait");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");
    let gamma = session_name("gamma");

    create_session(
        harness.socket_path(),
        (
            &alpha,
            TerminalSize {
                cols: 120,
                rows: 40,
            },
        ),
    )
    .await?;
    send_ok(harness.socket_path(), SplitWindowRequest::fixture(&alpha)).await?;
    send_ok(
        harness.socket_path(),
        NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await?;

    let listed_before = send(
        harness.socket_path(),
        ListPanesRequest::fixture((&alpha, "#{session_name}:#{window_index}:#{pane_index}")),
    )
    .await?;
    let before_lines = nonempty_lines(std::str::from_utf8(
        listed_before
            .command_output()
            .expect("list-panes returns command output")
            .stdout(),
    )?);
    assert_eq!(before_lines.len(), 3);
    assert!(before_lines.iter().all(|line| line.starts_with("alpha:")));

    send_ok(
        harness.socket_path(),
        RenameSessionRequest {
            target: alpha.clone(),
            new_name: gamma.clone(),
        },
    )
    .await?;

    assert_eq!(
        send_request(
            harness.socket_path(),
            &Request::HasSession(HasSessionRequest {
                target: alpha.clone(),
            }),
        )
        .await?,
        Response::HasSession(rmux_proto::HasSessionResponse { exists: false })
    );
    assert_eq!(
        send_request(
            harness.socket_path(),
            &Request::HasSession(HasSessionRequest {
                target: gamma.clone(),
            }),
        )
        .await?,
        Response::HasSession(rmux_proto::HasSessionResponse { exists: true })
    );

    let sessions = send(
        harness.socket_path(),
        ListSessionsRequest::fixture(
            "#{session_name}:#{session_windows}:#{session_width}x#{session_height}",
        ),
    )
    .await?;
    assert_eq!(
        sessions
            .command_output()
            .expect("list-sessions returns command output")
            .stdout(),
        b"gamma:2:x\n"
    );

    let panes_after = send(
        harness.socket_path(),
        ListPanesRequest::fixture((&gamma, "#{session_name}:#{window_index}:#{pane_index}")),
    )
    .await?;
    let after_lines = nonempty_lines(std::str::from_utf8(
        panes_after
            .command_output()
            .expect("list-panes returns command output")
            .stdout(),
    )?);
    assert_eq!(after_lines.len(), 3);
    assert!(after_lines.iter().all(|line| line.starts_with("gamma:")));
    assert!(after_lines.contains(&"gamma:1:0"));

    let ready_socket = harness.socket_path().to_path_buf();
    let ready_waiter = tokio::spawn(async move {
        let request = Request::WaitFor(WaitForRequest {
            channel: "ready".to_owned(),
            mode: WaitForMode::Wait,
        });
        send_request(&ready_socket, &request)
            .await
            .expect("ready waiter request should complete")
    });
    sleep(Duration::from_millis(50)).await;
    assert!(
        !ready_waiter.is_finished(),
        "plain wait-for should block until signalled"
    );
    send_ok(
        harness.socket_path(),
        WaitForRequest {
            channel: "ready".to_owned(),
            mode: WaitForMode::Signal,
        },
    )
    .await?;
    assert!(matches!(ready_waiter.await?, Response::WaitFor(_)));

    send_ok(
        harness.socket_path(),
        WaitForRequest {
            channel: "check".to_owned(),
            mode: WaitForMode::Lock,
        },
    )
    .await?;
    let gate_socket = harness.socket_path().to_path_buf();
    let lock_waiter = tokio::spawn(async move {
        let request = Request::WaitFor(WaitForRequest {
            channel: "check".to_owned(),
            mode: WaitForMode::Lock,
        });
        send_request(&gate_socket, &request)
            .await
            .expect("lock waiter request should complete")
    });
    sleep(Duration::from_millis(50)).await;
    assert!(
        !lock_waiter.is_finished(),
        "wait-for -L should block while the lock is held"
    );
    send_ok(
        harness.socket_path(),
        WaitForRequest {
            channel: "check".to_owned(),
            mode: WaitForMode::Unlock,
        },
    )
    .await?;
    assert!(matches!(lock_waiter.await?, Response::WaitFor(_)));

    handle.shutdown().await?;
    Ok(())
}

fn nonempty_lines(output: &str) -> Vec<&str> {
    output.lines().filter(|line| !line.is_empty()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_server_request_shuts_down_server_and_cleans_socket() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("request-kill-server");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    match send_request(
        harness.socket_path(),
        &Request::KillServer(KillServerRequest),
    )
    .await
    {
        Ok(Response::KillServer(_)) => {}
        Ok(other) => panic!("unexpected kill-server response: {other:?}"),
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("connection closed")
                    || message.contains("UnexpectedEof")
                    || message.contains("reset")
                    || message.contains("broken pipe"),
                "unexpected kill-server transport error: {message}"
            );
        }
    }

    drop(handle);
    wait_for_socket_removal(&socket_path).await?;
    Ok(())
}
