use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

mod common;

use common::{send_request, session_name, start_server, wait_for_socket_removal, TestHarness};
use rmux_proto::{
    CapturePaneRequest, DeleteBufferRequest, DisplayMessageRequest, HasSessionRequest,
    IfShellRequest, KillServerRequest, ListBuffersRequest, ListPanesRequest, ListSessionsRequest,
    LoadBufferRequest, NewSessionRequest, NewWindowRequest, PaneTarget, PasteBufferRequest,
    RenameSessionRequest, Request, Response, RunShellRequest, SaveBufferRequest, SendKeysRequest,
    SetBufferRequest, ShowBufferRequest, SplitDirection, SplitWindowRequest, SplitWindowTarget,
    Target, TerminalSize, WaitForMode, WaitForRequest,
};
use tokio::time::sleep;

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
    assert!(matches!(
        send_request(
            socket_path,
            &Request::NewSession(NewSessionRequest {
                session_name: session_name("alpha"),
                detached: true,
                size: Some(TerminalSize {
                    cols: 120,
                    rows: 40
                }),
                environment: None,
            }),
        )
        .await?,
        Response::NewSession(_)
    ));
    Ok(())
}

async fn exercise_buffer_file_requests(
    socket_path: &Path,
    save_path: &Path,
    load_path: &Path,
    paste_command: &str,
) -> Result<(), Box<dyn Error>> {
    assert!(matches!(
        send_request(
            socket_path,
            &Request::SetBuffer(Box::new(SetBufferRequest {
                name: Some("empty".to_owned()),
                content: Vec::new(),
                append: false,
                new_name: None,
                set_clipboard: false,
                target_client: None,
            })),
        )
        .await?,
        Response::SetBuffer(_)
    ));
    let empty = send_request(
        socket_path,
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("empty".to_owned()),
        }),
    )
    .await?;
    assert!(matches!(empty, Response::Error(_)));

    assert!(matches!(
        send_request(
            socket_path,
            &Request::SetBuffer(Box::new(SetBufferRequest {
                name: Some("delete-me".to_owned()),
                content: b"x".to_vec(),
                append: false,
                new_name: None,
                set_clipboard: false,
                target_client: None,
            })),
        )
        .await?,
        Response::SetBuffer(_)
    ));

    assert!(matches!(
        send_request(
            socket_path,
            &Request::SetBuffer(Box::new(SetBufferRequest {
                name: Some("pastecmd".to_owned()),
                content: paste_command.as_bytes().to_vec(),
                append: false,
                new_name: None,
                set_clipboard: false,
                target_client: None,
            })),
        )
        .await?,
        Response::SetBuffer(_)
    ));

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

    assert!(matches!(
        send_request(
            socket_path,
            &Request::SaveBuffer(SaveBufferRequest {
                path: save_path.to_string_lossy().into_owned(),
                cwd: None,
                name: Some("pastecmd".to_owned()),
                append: false,
            }),
        )
        .await?,
        Response::SaveBuffer(_)
    ));
    assert_eq!(fs::read_to_string(save_path)?, paste_command);

    fs::write(load_path, "loaded-over-socket")?;
    assert!(matches!(
        send_request(
            socket_path,
            &Request::LoadBuffer(Box::new(LoadBufferRequest {
                path: load_path.to_string_lossy().into_owned(),
                cwd: None,
                name: Some("loaded".to_owned()),
                set_clipboard: false,
                target_client: None,
            })),
        )
        .await?,
        Response::LoadBuffer(_)
    ));
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

async fn exercise_paste_capture_and_delete_requests(
    socket_path: &Path,
    pane: &PaneTarget,
    paste_marker: &str,
) -> Result<(), Box<dyn Error>> {
    assert!(matches!(
        send_request(
            socket_path,
            &Request::PasteBuffer(Box::new(PasteBufferRequest {
                name: Some("pastecmd".to_owned()),
                target: pane.clone(),
                delete_after: false,
                separator: None,
                linefeed: false,
                raw: false,
                bracketed: false,
            })),
        )
        .await?,
        Response::PasteBuffer(_)
    ));
    assert!(matches!(
        send_request(
            socket_path,
            &Request::SendKeys(SendKeysRequest {
                target: pane.clone(),
                keys: vec!["Enter".to_owned()],
            }),
        )
        .await?,
        Response::SendKeys(_)
    ));
    let capture = wait_for_capture(socket_path, pane.clone(), paste_marker).await?;
    assert!(capture.contains(paste_marker));

    let captured = send_request(
        socket_path,
        &Request::CapturePane(Box::new(CapturePaneRequest {
            target: pane.clone(),
            start: None,
            end: None,
            print: false,
            buffer_name: Some("captured".to_owned()),
            alternate: false,
            escape_ansi: false,
            escape_sequences: false,
            include_format: false,
            hyperlinks: false,
            line_numbers: false,
            join_wrapped: false,
            use_mode_screen: false,
            preserve_trailing_spaces: false,
            do_not_trim_spaces: false,
            pending_input: false,
            quiet: false,
            start_is_absolute: false,
            end_is_absolute: false,
        })),
    )
    .await?;
    assert!(matches!(captured, Response::CapturePane(_)));
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

    assert!(matches!(
        send_request(
            socket_path,
            &Request::DeleteBuffer(DeleteBufferRequest {
                name: Some("delete-me".to_owned()),
            }),
        )
        .await?,
        Response::DeleteBuffer(_)
    ));
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
    let display = send_request(
        socket_path,
        &Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Pane(pane.clone())),
            print: true,
            message: Some("#{session_name}:#{pane_index}:#{missing}".to_owned()),
            empty_target_context: false,
        }),
    )
    .await?;
    assert_eq!(
        display
            .command_output()
            .expect("display-message -p returns command output")
            .stdout(),
        b"alpha:0:\n"
    );

    let shell = send_request(
        socket_path,
        &Request::RunShell(Box::new(RunShellRequest {
            command: "printf server-run-shell-output".to_owned(),
            arguments: Vec::new(),
            background: false,
            as_commands: false,
            show_stderr: true,
            delay_seconds: None,
            start_directory: None,
            target: None,
            source_depth: None,
        })),
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

    let if_shell = send_request(
        socket_path,
        &Request::IfShell(Box::new(IfShellRequest {
            condition: "#{pane_active}".to_owned(),
            format_mode: true,
            then_command: "set-buffer -b branch chosen".to_owned(),
            else_command: Some("set-buffer -b branch skipped".to_owned()),
            target: Some(Target::Pane(pane.clone())),
            caller_cwd: None,
            background: false,
        })),
    )
    .await?;
    assert!(matches!(if_shell, Response::IfShell(_)));
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

    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::NewSession(NewSessionRequest {
                session_name: alpha.clone(),
                detached: true,
                size: Some(TerminalSize {
                    cols: 120,
                    rows: 40
                }),
                environment: None,
            }),
        )
        .await?,
        Response::NewSession(_)
    ));
    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::SplitWindow(SplitWindowRequest {
                target: SplitWindowTarget::Session(alpha.clone()),
                direction: SplitDirection::Vertical,
                before: false,
                environment: None,
            }),
        )
        .await?,
        Response::SplitWindow(_)
    ));
    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::NewWindow(Box::new(NewWindowRequest {
                target: alpha.clone(),
                name: Some("logs".to_owned()),
                detached: true,
                start_directory: None,
                environment: None,
                command: None,
                process_command: None,
                target_window_index: None,
                insert_at_target: false,
            })),
        )
        .await?,
        Response::NewWindow(_)
    ));

    let listed_before = send_request(
        harness.socket_path(),
        &Request::ListPanes(Box::new(ListPanesRequest {
            target: alpha.clone(),
            format: Some("#{session_name}:#{window_index}:#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })),
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

    let renamed = send_request(
        harness.socket_path(),
        &Request::RenameSession(RenameSessionRequest {
            target: alpha.clone(),
            new_name: gamma.clone(),
        }),
    )
    .await?;
    assert!(matches!(renamed, Response::RenameSession(_)));

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

    let sessions = send_request(
        harness.socket_path(),
        &Request::ListSessions(ListSessionsRequest {
            format: Some(
                "#{session_name}:#{session_windows}:#{session_width}x#{session_height}".to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
        }),
    )
    .await?;
    assert_eq!(
        sessions
            .command_output()
            .expect("list-sessions returns command output")
            .stdout(),
        b"gamma:2:x\n"
    );

    let panes_after = send_request(
        harness.socket_path(),
        &Request::ListPanes(Box::new(ListPanesRequest {
            target: gamma.clone(),
            format: Some("#{session_name}:#{window_index}:#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })),
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
    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::WaitFor(WaitForRequest {
                channel: "ready".to_owned(),
                mode: WaitForMode::Signal,
            }),
        )
        .await?,
        Response::WaitFor(_)
    ));
    assert!(matches!(ready_waiter.await?, Response::WaitFor(_)));

    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::WaitFor(WaitForRequest {
                channel: "check".to_owned(),
                mode: WaitForMode::Lock,
            }),
        )
        .await?,
        Response::WaitFor(_)
    ));
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
    assert!(matches!(
        send_request(
            harness.socket_path(),
            &Request::WaitFor(WaitForRequest {
                channel: "check".to_owned(),
                mode: WaitForMode::Unlock,
            }),
        )
        .await?,
        Response::WaitFor(_)
    ));
    assert!(matches!(lock_waiter.await?, Response::WaitFor(_)));

    handle.shutdown().await?;
    Ok(())
}

async fn wait_for_capture(
    socket_path: &Path,
    target: PaneTarget,
    marker: &str,
) -> Result<String, Box<dyn Error>> {
    for _ in 0..100 {
        let response = send_request(
            socket_path,
            &Request::CapturePane(Box::new(CapturePaneRequest {
                target: target.clone(),
                start: None,
                end: None,
                print: true,
                buffer_name: None,
                alternate: false,
                escape_ansi: false,
                escape_sequences: false,
                include_format: false,
                hyperlinks: false,
                line_numbers: false,
                join_wrapped: false,
                use_mode_screen: false,
                preserve_trailing_spaces: false,
                do_not_trim_spaces: false,
                pending_input: false,
                quiet: false,
                start_is_absolute: false,
                end_is_absolute: false,
            })),
        )
        .await?;
        let output = std::str::from_utf8(
            response
                .command_output()
                .expect("capture-pane -p returns command output")
                .stdout(),
        )?
        .to_owned();
        if output.contains(marker) {
            return Ok(output);
        }

        sleep(Duration::from_millis(20)).await;
    }

    Err(format!("capture-pane -p never surfaced marker {marker}").into())
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
