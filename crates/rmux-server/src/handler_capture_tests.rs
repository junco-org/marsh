use std::time::Duration;

use super::RequestHandler;
use crate::test_fixtures::{unique_temp_path, wait_until, Fixture, SessionSpec, TestRequest};
use rmux_core::{GridRenderOptions, ScreenCaptureRange};
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::ListBuffersRequest;
use rmux_proto::{
    CapturePaneRequest, CapturePaneTargetActionRequest, LoadBufferRequest, PaneTarget, Request,
    Response, SaveBufferRequest, SendKeysRequest, SetBufferRequest, ShowBufferRequest,
    TerminalSize,
};

use crate::test_names::session_name;

fn capture_stdout(response: Response) -> Vec<u8> {
    let Response::CapturePane(response) = response else {
        panic!("expected capture-pane response, got {response:?}");
    };
    response
        .command_output()
        .expect("capture-pane -p returns command output")
        .stdout()
        .to_vec()
}

fn load_buffer_request(
    path: &std::path::Path,
    cwd: Option<std::path::PathBuf>,
    name: &str,
) -> LoadBufferRequest {
    LoadBufferRequest {
        path: path.display().to_string(),
        cwd,
        name: Some(name.to_owned()),
        set_clipboard: false,
        target_client: None,
    }
}

fn save_buffer_request(
    path: &std::path::Path,
    cwd: Option<std::path::PathBuf>,
    name: &str,
) -> SaveBufferRequest {
    SaveBufferRequest {
        path: path.display().to_string(),
        cwd,
        name: Some(name.to_owned()),
        append: false,
    }
}

#[tokio::test]
async fn target_action_capture_resolves_raw_target_server_side() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, ("alpha", TerminalSize { cols: 20, rows: 4 })).await;
    let target = PaneTarget::with_window(session_name("alpha"), 0, 0);
    handler
        .replace_transcript_for_test(
            &target,
            TerminalSize { cols: 20, rows: 4 },
            b"target-capture",
        )
        .await;

    let response = handler
        .handle(Request::CapturePaneTargetAction(Box::new(
            CapturePaneTargetActionRequest {
                target: Some("alpha:0.0".to_owned()),
                start: Some(0),
                end: Some(0),
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
            },
        )))
        .await;
    let Response::CapturePane(response) = response else {
        panic!("expected capture-pane response, got {response:?}");
    };
    let output = response
        .command_output()
        .expect("capture-pane -p returns command output");
    assert_eq!(output.stdout(), b"target-capture\n");
}

#[tokio::test]
async fn direct_and_target_action_capture_join_stop_at_active_alternate_boundary() {
    let handler = RequestHandler::new();
    SessionSpec::create(&handler, ("alpha", TerminalSize { cols: 8, rows: 2 })).await;
    let target = PaneTarget::with_window(session_name("alpha"), 0, 0);
    handler
        .replace_transcript_for_test(
            &target,
            TerminalSize { cols: 8, rows: 2 },
            b"abcdefghijkl\r\n\x1b[?1049h\x1b[HVIM",
        )
        .await;

    let direct = CapturePaneRequest {
        join_wrapped: true,
        start_is_absolute: true,
        ..Fixture::fixture(target)
    };
    let direct = handler.handle(Request::CapturePane(Box::new(direct))).await;
    assert_eq!(capture_stdout(direct), b"abcdefgh\nVIM\n\n");

    let target_action = handler
        .handle(Request::CapturePaneTargetAction(Box::new(
            CapturePaneTargetActionRequest {
                target: Some("alpha:0.0".to_owned()),
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
                join_wrapped: true,
                use_mode_screen: false,
                preserve_trailing_spaces: false,
                do_not_trim_spaces: false,
                pending_input: false,
                quiet: false,
                start_is_absolute: true,
                end_is_absolute: false,
            },
        )))
        .await;
    assert_eq!(capture_stdout(target_action), b"abcdefgh\nVIM\n\n");

    let queued = handler
        .parse_control_commands("capture-pane -pJ -S - -t alpha:0.0")
        .await
        .expect("queued capture-pane parses");
    let queued = handler
        .execute_parsed_commands_for_test(std::process::id(), queued)
        .await
        .expect("queued capture-pane executes");
    assert_eq!(queued.stdout(), b"abcdefgh\nVIM\n\n");
}

#[tokio::test]
async fn named_capture_buffer_keeps_dch_field_boundary_like_tmux_3_7b() {
    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 12, rows: 8 };
    SessionSpec::create(&handler, ("mutations", size)).await;
    let target = PaneTarget::with_window(session_name("mutations"), 0, 0);
    handler
        .replace_transcript_for_test(
            &target,
            size,
            b"ABCDEFGHIJKLmnopqrstuvwx012345678\r\nNXT\r\nEND\
              \x1b[r\x1b[2;1H\x1b[99P",
        )
        .await;

    let response = TestRequest::send_ok(
        &handler,
        CapturePaneRequest {
            print: false,
            buffer_name: Some("mutation-consumer".to_owned()),
            join_wrapped: true,
            ..Fixture::fixture(target)
        },
    )
    .await;
    assert_eq!(response.buffer_name.as_deref(), Some("mutation-consumer"));

    let shown = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("mutation-consumer".to_owned()),
        }))
        .await;
    let bytes = shown
        .command_output()
        .expect("show-buffer returns captured bytes")
        .stdout()
        .to_vec();
    let nonempty = bytes
        .split(|byte| *byte == b'\n')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();

    assert_eq!(
        nonempty,
        [
            b"ABCDEFGHIJKL".as_slice(),
            b"012345678".as_slice(),
            b"NXT".as_slice(),
            b"END".as_slice(),
        ]
    );
}

async fn send_marker(handler: &RequestHandler, target: PaneTarget, marker: &str) {
    TestRequest::send_ok(
        handler,
        SendKeysRequest {
            target,
            keys: vec![marker_print_command(marker), "Enter".to_owned()],
        },
    )
    .await;
}

fn marker_print_command(marker: &str) -> String {
    format!("printf '{marker}\\n'")
}

async fn wait_for_capture(handler: &RequestHandler, target: PaneTarget, marker: &str) -> Vec<u8> {
    wait_until(
        Duration::from_secs(10),
        Duration::from_millis(20),
        async || {
            let stdout = TestRequest::send_ok(handler, CapturePaneRequest::fixture(&target))
                .await
                .command_output()
                .expect("capture-pane -p returns command output")
                .stdout()
                .to_vec();
            if String::from_utf8_lossy(&stdout).contains(marker) {
                Ok(stdout)
            } else {
                Err(stdout)
            }
        },
    )
    .await
    .unwrap_or_else(|last_stdout| {
        panic!(
            "capture output never contained marker {marker}; last stdout: {:?}",
            String::from_utf8_lossy(&last_stdout)
        )
    })
}

#[tokio::test]
async fn capture_pane_prints_transcript_without_creating_buffer() {
    let handler = RequestHandler::new();
    let target = PaneTarget::with_window(session_name("alpha"), 0, 0);
    let marker = "handler_capture_print_marker";

    SessionSpec::create(&handler, "alpha").await;
    send_marker(&handler, target.clone(), marker).await;

    let output = wait_for_capture(&handler, target, marker).await;
    assert!(String::from_utf8_lossy(&output).contains(marker));

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest { name: None }))
        .await;
    assert!(matches!(show, Response::Error(_)));
}

#[tokio::test]
async fn capture_pane_writes_named_buffer() {
    let handler = RequestHandler::new();
    let target = PaneTarget::with_window(session_name("alpha"), 0, 0);
    let marker = "handler_capture_buffer_marker";

    SessionSpec::create(&handler, "alpha").await;
    send_marker(&handler, target.clone(), marker).await;
    wait_for_capture(&handler, target.clone(), marker).await;

    let response = TestRequest::send_ok(
        &handler,
        CapturePaneRequest {
            print: false,
            buffer_name: Some("capture-buffer".to_owned()),
            ..Fixture::fixture(target)
        },
    )
    .await;
    assert_eq!(response.buffer_name.as_deref(), Some("capture-buffer"));
    assert!(response.command_output().is_none());

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("capture-buffer".to_owned()),
        }))
        .await;
    let output = show.command_output().expect("show-buffer returns output");
    assert!(String::from_utf8_lossy(output.stdout()).contains(marker));
}

#[tokio::test]
async fn capture_pane_do_not_trim_uses_tmux_cell_capacity() {
    let handler = RequestHandler::new();
    let target = PaneTarget::with_window(session_name("capacity"), 0, 0);
    let size = TerminalSize { cols: 20, rows: 6 };

    SessionSpec::create(&handler, ("capacity", size)).await;
    handler
        .replace_transcript_for_test(&target, size, b"a\r\nabcde\r\nabcdefghij\r\n")
        .await;

    let request = CapturePaneRequest {
        do_not_trim_spaces: true,
        ..Fixture::fixture(target)
    };
    let response = handler
        .handle(Request::CapturePane(Box::new(request)))
        .await;
    let output = response
        .command_output()
        .expect("capture-pane -Np returns command output");
    let output = String::from_utf8(output.stdout().to_vec()).expect("capture output is utf-8");

    assert_eq!(output, "a    \nabcde     \nabcdefghij          \n\n\n\n");
}

#[tokio::test]
async fn alternate_screen_off_keeps_program_output_on_main_screen() {
    let handler = RequestHandler::new();
    let target = PaneTarget::with_window(session_name("altscreen"), 0, 0);
    SessionSpec::create(&handler, ("altscreen", TerminalSize { cols: 20, rows: 5 })).await;

    handler
        .set_option_by_name(OptionScopeSelector::WindowGlobal, "alternate-screen", "off")
        .await;

    let transcript = {
        let state = handler.state.lock().await;
        state
            .transcript_handle(&target)
            .expect("session transcript must exist")
    };
    let output = {
        let mut transcript = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned");
        transcript.append_bytes(b"\x1b[2J\x1b[H\x1b[?1049hALTLINE\r\n\x1b[?1049lMAINLINE\r\n");
        assert!(!transcript.is_alternate());
        transcript.capture_main(ScreenCaptureRange::default(), GridRenderOptions::default())
    };
    let output = String::from_utf8(output).expect("capture output is utf8");
    assert!(output.contains("ALTLINE"), "{output:?}");
    assert!(output.contains("MAINLINE"), "{output:?}");
}

#[tokio::test]
async fn load_buffer_reads_server_file() {
    let handler = RequestHandler::new();
    let path = unique_temp_path("load-success");
    std::fs::write(&path, b"loaded data").expect("write input");

    let response = handler
        .handle(Request::LoadBuffer(Box::new(load_buffer_request(
            &path, None, "loaded",
        ))))
        .await;
    match response {
        Response::LoadBuffer(response) => assert_eq!(response.buffer_name, "loaded"),
        other => panic!("expected load-buffer response, got {other:?}"),
    }

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("loaded".to_owned()),
        }))
        .await;
    assert_eq!(
        show.command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"loaded data"
    );

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn load_buffer_waiting_on_fifo_does_not_block_other_requests() {
    let handler = RequestHandler::new();
    let path = unique_temp_path("load-fifo");
    let output = std::process::Command::new("mkfifo")
        .arg(&path)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        std::fs::write(writer_path, b"fifo data").expect("write fifo");
    });
    let load_handler = handler.clone();
    let load_path = path.clone();
    let load = tokio::spawn(async move {
        load_handler
            .handle(Request::LoadBuffer(Box::new(load_buffer_request(
                &load_path, None, "fifo",
            ))))
            .await
    });

    let concurrent_response = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::task::yield_now().await;
        handler
            .handle(Request::ListBuffers(ListBuffersRequest::default()))
            .await
    })
    .await
    .expect("a blocked FIFO read must not stall unrelated daemon requests");
    assert!(matches!(concurrent_response, Response::ListBuffers(_)));

    let load_response = load.await.expect("load-buffer task should finish");
    assert!(matches!(load_response, Response::LoadBuffer(_)));
    writer.join().expect("FIFO writer should finish");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn load_buffer_failure_does_not_mutate_existing_buffer() {
    let handler = RequestHandler::new();
    let missing_path = unique_temp_path("load-missing");

    TestRequest::send_ok(&handler, SetBufferRequest::fixture(("stable", b"original"))).await;

    let response = handler
        .handle(Request::LoadBuffer(Box::new(load_buffer_request(
            &missing_path,
            None,
            "stable",
        ))))
        .await;
    assert!(matches!(response, Response::Error(_)));

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("stable".to_owned()),
        }))
        .await;
    assert_eq!(
        show.command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"original"
    );
}

#[tokio::test]
async fn load_buffer_resolves_relative_path_against_request_cwd() {
    let handler = RequestHandler::new();
    let root = unique_temp_path("load-relative-root");
    let nested_dir = root.join("nested");
    std::fs::create_dir_all(&nested_dir).expect("create nested dir");
    std::fs::write(nested_dir.join("input.txt"), b"relative data").expect("write input");

    let response = handler
        .handle(Request::LoadBuffer(Box::new(load_buffer_request(
            &std::path::Path::new("nested").join("input.txt"),
            Some(root.clone()),
            "loaded",
        ))))
        .await;
    match response {
        Response::LoadBuffer(response) => assert_eq!(response.buffer_name, "loaded"),
        other => panic!("expected load-buffer response, got {other:?}"),
    }

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("loaded".to_owned()),
        }))
        .await;
    assert_eq!(
        show.command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"relative data"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn save_buffer_writes_server_file() {
    let handler = RequestHandler::new();
    let path = unique_temp_path("save-success");

    TestRequest::send_ok(&handler, SetBufferRequest::fixture(("saved", b"save me"))).await;

    let response = handler
        .handle(Request::SaveBuffer(save_buffer_request(
            &path, None, "saved",
        )))
        .await;
    match response {
        Response::SaveBuffer(response) => assert_eq!(response.buffer_name, "saved"),
        other => panic!("expected save-buffer response, got {other:?}"),
    }
    assert_eq!(std::fs::read(&path).expect("read saved file"), b"save me");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn save_buffer_waiting_on_fifo_does_not_block_other_requests() {
    for append in [false, true] {
        let handler = RequestHandler::new();
        let path = unique_temp_path(if append {
            "save-append-fifo"
        } else {
            "save-overwrite-fifo"
        });
        let output = std::process::Command::new("mkfifo")
            .arg(&path)
            .output()
            .expect("run mkfifo");
        assert!(
            output.status.success(),
            "mkfifo failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        TestRequest::send_ok(&handler, SetBufferRequest::fixture(("saved", b"fifo data"))).await;

        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            std::fs::read(reader_path).expect("read fifo")
        });
        let save_handler = handler.clone();
        let save_path = path.clone();
        let save = tokio::spawn(async move {
            let mut request = save_buffer_request(&save_path, None, "saved");
            request.append = append;
            save_handler.handle(Request::SaveBuffer(request)).await
        });

        let concurrent_response = tokio::time::timeout(Duration::from_millis(500), async {
            tokio::task::yield_now().await;
            handler
                .handle(Request::ListBuffers(ListBuffersRequest::default()))
                .await
        })
        .await
        .expect("a blocked FIFO write must not stall unrelated daemon requests");
        assert!(matches!(concurrent_response, Response::ListBuffers(_)));

        let save_response = save.await.expect("save-buffer task should finish");
        assert!(matches!(save_response, Response::SaveBuffer(_)));
        assert_eq!(
            reader.join().expect("FIFO reader should finish"),
            b"fifo data"
        );
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn save_buffer_resolves_relative_path_against_request_cwd() {
    let handler = RequestHandler::new();
    let root = unique_temp_path("save-relative-root");
    let nested_dir = root.join("nested");
    std::fs::create_dir_all(&nested_dir).expect("create nested dir");

    TestRequest::send_ok(
        &handler,
        SetBufferRequest::fixture(("saved", b"relative save")),
    )
    .await;

    let response = handler
        .handle(Request::SaveBuffer(save_buffer_request(
            &std::path::Path::new("nested").join("output.txt"),
            Some(root.clone()),
            "saved",
        )))
        .await;
    match response {
        Response::SaveBuffer(response) => assert_eq!(response.buffer_name, "saved"),
        other => panic!("expected save-buffer response, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(nested_dir.join("output.txt")).expect("read saved file"),
        b"relative save"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn save_buffer_failure_does_not_mutate_existing_buffer() {
    let handler = RequestHandler::new();
    let path = unique_temp_path("missing-parent").join("out.txt");

    TestRequest::send_ok(&handler, SetBufferRequest::fixture(("stable", b"original"))).await;

    let response = handler
        .handle(Request::SaveBuffer(save_buffer_request(
            &path, None, "stable",
        )))
        .await;
    assert!(matches!(response, Response::Error(_)));

    let show = handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some("stable".to_owned()),
        }))
        .await;
    assert_eq!(
        show.command_output()
            .expect("show-buffer returns output")
            .stdout(),
        b"original"
    );
}
