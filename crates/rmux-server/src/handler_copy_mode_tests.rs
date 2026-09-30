use std::{fs, path::Path, time::Duration};

use std::os::unix::fs::PermissionsExt;

use super::super::RequestHandler;
use crate::outer_terminal::OuterTerminalContext;
use crate::pane_io::AttachControl;
use crate::test_fixtures::{
    quiet_command, unique_temp_path, wait_until, Fixture, SessionSpec, TestRequest,
};
use rmux_proto::{
    CapturePaneRequest, CopyModeRequest, ListPanesRequest, NewSessionExtRequest, OptionName,
    OptionScopeSelector, PaneTarget, Request, Response, ScopeSelector, SendKeysExtRequest,
    ShowBufferRequest, SwitchClientRequest, TerminalSize, WindowTarget,
};
use tokio::time::sleep;

async fn create_session(handler: &RequestHandler, name: &str, size: TerminalSize) -> PaneTarget {
    let session = SessionSpec::create(
        handler,
        NewSessionExtRequest {
            size: Some(size),
            command: Some(quiet_command()),
            ..Fixture::fixture(name)
        },
    )
    .await;
    PaneTarget::with_window(session, 0, 0)
}

async fn replace_transcript_contents(
    handler: &RequestHandler,
    target: &PaneTarget,
    size: TerminalSize,
    content: &[u8],
) {
    handler
        .wait_for_pane_startup_to_finish_for_test(target)
        .await;
    handler
        .replace_transcript_for_test(target, size, content)
        .await;
}

async fn wait_for_capture(
    handler: &RequestHandler,
    target: &PaneTarget,
    needle: &str,
    use_mode_screen: bool,
) -> String {
    for _ in 0..100 {
        let response = handler
            .handle(Request::CapturePane(Box::new(CapturePaneRequest {
                use_mode_screen,
                ..Fixture::fixture(target)
            })))
            .await;
        let output = response
            .command_output()
            .expect("capture-pane returns command output");
        let text = String::from_utf8_lossy(output.stdout()).into_owned();
        if text.contains(needle) {
            return text;
        }
        sleep(Duration::from_millis(20)).await;
    }

    panic!("capture output never contained {needle}");
}

#[tokio::test]
async fn direct_copy_mode_entry_enables_line_numbers() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "line-numbers-direct",
        TerminalSize { cols: 20, rows: 5 },
    )
    .await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    let transcript = {
        let state = handler.state.lock().await;
        state
            .transcript_handle(&target)
            .expect("session transcript must exist")
    };
    let enabled = transcript
        .lock()
        .expect("pane transcript mutex must not be poisoned")
        .copy_mode_render_snapshot()
        .expect("copy-mode snapshot")
        .line_numbers_enabled;

    assert!(enabled, "ordinary copy-mode entry enables the gutter");
}

async fn pane_terminal_size(handler: &RequestHandler, target: &PaneTarget) -> TerminalSize {
    handler
        .wait_for_pane_startup_to_finish_for_test(target)
        .await;
    handler.pane_terminal_size_for_test(target).await
}

/// `send-keys -X tokens…` against `target`: the tokens are one copy-mode command.
fn copy_mode_command<K>(target: &PaneTarget, tokens: K) -> SendKeysExtRequest
where
    K: IntoIterator,
    K::Item: Into<String>,
{
    SendKeysExtRequest {
        dispatch_key_table: false,
        copy_mode_command: true,
        ..Fixture::fixture((target, tokens))
    }
}

fn platform_copy_mode_arg(arg: &str) -> String {
    match arg {
        "cat >/dev/null" => crate::test_shell::stdin_discard_command(),
        _ => arg.to_owned(),
    }
}

async fn wait_for_file_containing(path: &Path, expected: &str) -> String {
    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(20),
        async || match fs::read_to_string(path) {
            Ok(contents) if contents.contains(expected) => Ok(contents),
            other => Err(other),
        },
    )
    .await
    .unwrap_or_else(|last| {
        let observation = last
            .map(|contents| format!("contents were {contents:?}"))
            .unwrap_or_else(|error| format!("read failed: {error}"));
        panic!(
            "file {} never contained {expected:?} within 5 seconds; {observation}",
            path.display()
        );
    })
}

fn stdin_to_file_command(path: &Path) -> String {
    format!("cat > {}", crate::test_shell::sh_quote_path(path))
}

fn stdin_to_relative_file_command(name: &str) -> String {
    format!("cat > {}", crate::test_shell::sh_quote(name))
}

fn file_url_path(path: &Path) -> String {
    path.to_string_lossy().replace(' ', "%20")
}

fn take_write(control: AttachControl) -> Option<Vec<u8>> {
    match control {
        AttachControl::Write(bytes) => Some(bytes),
        _ => None,
    }
}

async fn prepare_transfer_selection(handler: &RequestHandler, target: &PaneTarget) {
    TestRequest::send_ok(handler, copy_mode_command(target, ["select-line"])).await;
}

#[tokio::test]
async fn copy_mode_capture_uses_backing_screen_snapshot() {
    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 24, rows: 3 };
    let target = create_session(&handler, "alpha", size).await;
    replace_transcript_contents(
        &handler,
        &target,
        size,
        b"line1\r\nline2\r\nline3\r\nline4\r\nline5\r\n",
    )
    .await;

    let response = handler
        .handle(Request::CopyMode(CopyModeRequest {
            page_up: true,
            ..Fixture::fixture(&target)
        }))
        .await;
    assert_eq!(
        response,
        Response::CopyMode(rmux_proto::CopyModeResponse {
            target: target.clone(),
            active: true,
            view_mode: false,
        })
    );

    let mode_capture = wait_for_capture(&handler, &target, "line2", true).await;
    assert_eq!(mode_capture, "line1\nline2\nline3\n");
}

#[tokio::test]
async fn modal_scrollbar_resizes_pty_only_while_copy_mode_is_active() {
    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 20, rows: 8 };
    let target = create_session(&handler, "scrollbar-modal", size).await;
    assert_eq!(pane_terminal_size(&handler, &target).await, size);

    let scope = ScopeSelector::Window(WindowTarget::with_window(
        target.session_name().clone(),
        target.window_index(),
    ));
    handler
        .set_option(scope.clone(), OptionName::PaneScrollbars, "modal")
        .await;
    assert_eq!(pane_terminal_size(&handler, &target).await, size);

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    assert_eq!(
        pane_terminal_size(&handler, &target).await,
        TerminalSize { cols: 19, rows: 8 }
    );

    TestRequest::send_ok(
        &handler,
        CopyModeRequest {
            cancel_mode: true,
            ..Fixture::fixture(&target)
        },
    )
    .await;
    assert_eq!(pane_terminal_size(&handler, &target).await, size);

    for (value, expected_cols) in [("on", 19), ("off", 20)] {
        handler
            .set_option(scope.clone(), OptionName::PaneScrollbars, value)
            .await;
        assert_eq!(
            pane_terminal_size(&handler, &target).await,
            TerminalSize {
                cols: expected_cols,
                rows: 8,
            }
        );
    }
}

#[tokio::test]
async fn copy_mode_line_number_gutter_never_resizes_the_pty() {
    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 20, rows: 8 };
    let target = create_session(&handler, "line-number-pty", size).await;
    assert_eq!(pane_terminal_size(&handler, &target).await, size);

    let scope = ScopeSelector::Window(WindowTarget::with_window(
        target.session_name().clone(),
        target.window_index(),
    ));
    handler
        .set_option(scope, OptionName::CopyModeLineNumbers, "absolute")
        .await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    assert_eq!(
        pane_terminal_size(&handler, &target).await,
        size,
        "the line-number gutter is an internal copy-mode overlay"
    );
}

#[tokio::test]
async fn copy_mode_formats_report_live_state() {
    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 40, rows: 4 };
    let target = create_session(&handler, "beta", size).await;
    replace_transcript_contents(
        &handler,
        &target,
        size,
        b"alpha beta gamma\r\nneedle here\r\nomega\r\n",
    )
    .await;
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::CopyModeLineNumbers,
            "absolute",
        )
        .await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-word"])).await;

    let listed = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: target.session_name().clone(),
            format: Some(
                "#{pane_in_mode} #{pane_mode} #{search_present} #{selection_present} #{copy_cursor_word} #{copy_position}/#{copy_position_limit}".to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
        })))
        .await;
    let output = listed
        .command_output()
        .expect("list-panes returns command output");
    let text = String::from_utf8_lossy(output.stdout());
    assert_eq!(text.as_ref(), "1 copy-mode 1 1 needle 1/4\n");
}

#[tokio::test]
async fn copy_mode_command_table_dispatches_all_tmux_commands() {
    const COMMANDS: &[(&str, &[&str])] = &[
        ("append-selection", &[]),
        ("append-selection-and-cancel", &[]),
        ("back-to-indentation", &[]),
        ("begin-selection", &[]),
        ("bottom-line", &[]),
        ("cancel", &[]),
        ("clear-selection", &[]),
        ("copy-end-of-line", &[]),
        ("copy-end-of-line-and-cancel", &[]),
        ("copy-pipe-end-of-line", &["cat >/dev/null"]),
        ("copy-pipe-end-of-line-and-cancel", &["cat >/dev/null"]),
        ("copy-line", &[]),
        ("copy-line-and-cancel", &[]),
        ("copy-pipe-line", &["cat >/dev/null"]),
        ("copy-pipe-line-and-cancel", &["cat >/dev/null"]),
        ("copy-pipe-no-clear", &["cat >/dev/null"]),
        ("copy-pipe", &["cat >/dev/null"]),
        ("copy-pipe-and-cancel", &["cat >/dev/null"]),
        ("copy-selection-no-clear", &[]),
        ("copy-selection", &[]),
        ("copy-selection-and-cancel", &[]),
        ("cursor-down", &[]),
        ("cursor-down-and-cancel", &[]),
        ("cursor-left", &[]),
        ("cursor-right", &[]),
        ("cursor-up", &[]),
        ("cursor-centre-vertical", &[]),
        ("cursor-centre-horizontal", &[]),
        ("end-of-buffer", &[]),
        ("end-of-line", &[]),
        ("goto-line", &["1"]),
        ("halfpage-down", &[]),
        ("halfpage-down-and-cancel", &[]),
        ("halfpage-up", &[]),
        ("history-bottom", &[]),
        ("history-top", &[]),
        ("jump-again", &[]),
        ("jump-backward", &["a"]),
        ("jump-forward", &["a"]),
        ("jump-reverse", &[]),
        ("jump-to-backward", &["a"]),
        ("jump-to-forward", &["a"]),
        ("jump-to-mark", &[]),
        ("next-prompt", &[]),
        ("previous-prompt", &[]),
        ("middle-line", &[]),
        ("next-matching-bracket", &[]),
        ("next-paragraph", &[]),
        ("next-space", &[]),
        ("next-space-end", &[]),
        ("next-word", &[]),
        ("next-word-end", &[]),
        ("other-end", &[]),
        ("page-down", &[]),
        ("page-down-and-cancel", &[]),
        ("page-up", &[]),
        ("pipe-no-clear", &["cat >/dev/null"]),
        ("pipe", &["cat >/dev/null"]),
        ("pipe-and-cancel", &["cat >/dev/null"]),
        ("previous-matching-bracket", &[]),
        ("previous-paragraph", &[]),
        ("previous-space", &[]),
        ("previous-word", &[]),
        ("rectangle-on", &[]),
        ("rectangle-off", &[]),
        ("rectangle-toggle", &[]),
        ("refresh-from-pane", &[]),
        ("recentre-top-bottom", &[]),
        ("scroll-bottom", &[]),
        ("scroll-down", &[]),
        ("scroll-down-and-cancel", &[]),
        ("scroll-exit-on", &[]),
        ("scroll-exit-off", &[]),
        ("scroll-exit-toggle", &[]),
        ("scroll-middle", &[]),
        ("scroll-to-mouse", &[]),
        ("scroll-top", &[]),
        ("scroll-up", &[]),
        ("search-again", &[]),
        ("search-backward", &["alpha"]),
        ("search-backward-text", &["alpha"]),
        ("search-backward-incremental", &["-:alpha"]),
        ("search-forward", &["alpha"]),
        ("search-forward-text", &["alpha"]),
        ("search-forward-incremental", &["+:alpha"]),
        ("search-reverse", &[]),
        ("select-line", &[]),
        ("select-word", &[]),
        ("selection-mode", &["word"]),
        ("set-mark", &[]),
        ("start-of-buffer", &[]),
        ("start-of-line", &[]),
        ("stop-selection", &[]),
        ("toggle-position", &[]),
        ("top-line", &[]),
    ];

    let handler = RequestHandler::new();
    let target = create_session(&handler, "gamma", TerminalSize { cols: 48, rows: 6 }).await;

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 48, rows: 6 },
        b"(alpha) beta gamma\r\nword_two more words\r\nthird paragraph\r\n\r\nfourth line\r\nlast line\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "last line", false).await;

    for (command, args) in COMMANDS {
        TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;

        match *command {
            "append-selection"
            | "append-selection-and-cancel"
            | "copy-pipe-no-clear"
            | "copy-pipe"
            | "copy-pipe-and-cancel"
            | "copy-selection-no-clear"
            | "copy-selection"
            | "copy-selection-and-cancel"
            | "pipe-no-clear"
            | "pipe"
            | "pipe-and-cancel"
            | "stop-selection" => prepare_transfer_selection(&handler, &target).await,
            "other-end" => {
                prepare_transfer_selection(&handler, &target).await;
                let _ = handler
                    .handle(Request::SendKeysExt(copy_mode_command(
                        &target,
                        ["cursor-right"],
                    )))
                    .await;
            }
            "jump-again" | "jump-reverse" => {
                let _ = handler
                    .handle(Request::SendKeysExt(copy_mode_command(
                        &target,
                        ["jump-forward", "--", "a"],
                    )))
                    .await;
            }
            "jump-to-mark" => {
                let _ = handler
                    .handle(Request::SendKeysExt(copy_mode_command(
                        &target,
                        ["set-mark"],
                    )))
                    .await;
                let _ = handler
                    .handle(Request::SendKeysExt(copy_mode_command(
                        &target,
                        ["cursor-down"],
                    )))
                    .await;
            }
            "search-again" | "search-reverse" => {
                let _ = handler
                    .handle(Request::SendKeysExt(copy_mode_command(
                        &target,
                        ["search-backward", "--", "alpha"],
                    )))
                    .await;
            }
            _ => {}
        }

        let mut tokens = vec![(*command).to_owned()];
        if !args.is_empty() {
            tokens.push("--".to_owned());
            tokens.extend(args.iter().map(|arg| platform_copy_mode_arg(arg)));
        }
        let response = handler
            .handle(Request::SendKeysExt(copy_mode_command(&target, tokens)))
            .await;
        assert!(
            !matches!(response, Response::Error(_)),
            "{command} returned {response:?}"
        );
    }
}

#[tokio::test]
async fn copy_mode_copy_selection_and_cancel_writes_buffer() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "delta", TerminalSize { cols: 40, rows: 4 }).await;

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle value\r\nomega\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "needle", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-word"])).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["copy-selection-and-cancel"]),
    )
    .await;

    let buffer = handler
        .handle(Request::ShowBuffer(ShowBufferRequest { name: None }))
        .await;
    let output = buffer.command_output().expect("show-buffer returns output");
    assert!(String::from_utf8_lossy(output.stdout()).contains("needle"));
}

#[tokio::test]
async fn attached_copy_mode_switch_after_mutation_is_not_a_fatal_input_error() {
    let handler = std::sync::Arc::new(RequestHandler::new());
    let requester_pid = 71_404;
    let alpha = create_session(
        &handler,
        "copy-switch-alpha",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    let beta = create_session(
        &handler,
        "copy-switch-beta",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    let _control_rx = handler
        .attach_client(requester_pid, alpha.session_name())
        .await;
    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&alpha)).await;

    let pause = super::super::copy_mode_support::install_copy_mode_mutation_pause(requester_pid);
    let input_handler = std::sync::Arc::clone(&handler);
    let input = tokio::spawn(async move {
        input_handler
            .handle_attached_live_input_for_test(requester_pid, b"q")
            .await
    });
    pause.reached.notified().await;

    let switched = handler
        .dispatch(
            requester_pid,
            Request::SwitchClient(SwitchClientRequest {
                target: beta.session_name().clone(),
            }),
        )
        .await
        .response;
    assert!(
        matches!(switched, Response::SwitchClient(_)),
        "{switched:?}"
    );
    pause.release.notify_one();

    let result = input.await.expect("copy-mode input task joins");
    assert!(
        result.is_ok(),
        "post-mutation switch must not fail input: {result:?}"
    );
    let active_session = {
        let active_attach = handler.active_attach.lock().await;
        active_attach.current_session_candidate(requester_pid)
    };
    assert_eq!(active_session.as_ref(), Some(beta.session_name()));
}

#[tokio::test]
async fn copy_pipe_without_command_uses_copy_command_option() {
    let handler = RequestHandler::new();
    let target = create_session(&handler, "copy-command", TerminalSize { cols: 40, rows: 4 }).await;
    let output_path = unique_temp_path("copy-pipe-fallback");
    handler
        .set_option_by_name(
            OptionScopeSelector::ServerGlobal,
            "copy-command",
            &stdin_to_file_command(&output_path),
        )
        .await;

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle fallback\r\nomega\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "needle fallback", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["copy-pipe-and-cancel"]),
    )
    .await;

    let output = wait_for_file_containing(&output_path, "needle fallback").await;
    let _ = fs::remove_file(&output_path);
    assert!(output.contains("needle fallback"));
}

#[tokio::test]
async fn copy_pipe_hands_the_copy_command_non_ascii_selections_byte_exactly() {
    // Issue #177. RMUX must write the selection to the copy-command child as
    // raw UTF-8, byte for byte, exactly like tmux — it is the command, not
    // RMUX, that owns its input encoding. Every glyph in the needle below is
    // multi-byte in UTF-8 (box drawing, a Latin-1 accent, CJK), so any
    // code-page transcode on this path changes the bytes that reach the child.
    const NEEDLE: &str = "needle ╭─│╯ café 日本";

    let handler = RequestHandler::new();
    let size = TerminalSize { cols: 60, rows: 4 };
    let target = create_session(&handler, "copy-command-utf8", size).await;
    let output_path = unique_temp_path("copy-pipe-utf8");
    handler
        .set_option_by_name(
            OptionScopeSelector::ServerGlobal,
            "copy-command",
            &stdin_to_file_command(&output_path),
        )
        .await;

    replace_transcript_contents(
        &handler,
        &target,
        size,
        format!("alpha\r\n{NEEDLE}\r\nomega\r\n").as_bytes(),
    )
    .await;
    wait_for_capture(&handler, &target, NEEDLE, false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["copy-pipe-and-cancel"]),
    )
    .await;

    wait_for_file_containing(&output_path, NEEDLE).await;
    let piped = fs::read(&output_path).expect("copy-pipe output is readable");
    let _ = fs::remove_file(&output_path);

    assert!(
        piped
            .windows(NEEDLE.len())
            .any(|window| window == NEEDLE.as_bytes()),
        "copy-pipe delivered {piped:?}, which does not contain the selection's UTF-8 bytes"
    );
    assert!(
        String::from_utf8(piped).is_ok(),
        "copy-pipe delivered bytes that are not valid UTF-8"
    );
}

#[tokio::test]
async fn copy_pipe_explicit_command_overrides_copy_command_option() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "copy-command-explicit",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    let fallback_path = unique_temp_path("copy-pipe-fallback-unused");
    let explicit_path = unique_temp_path("copy-pipe-explicit");
    handler
        .set_option_by_name(
            OptionScopeSelector::ServerGlobal,
            "copy-command",
            &stdin_to_file_command(&fallback_path),
        )
        .await;

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle explicit\r\nomega\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "needle explicit", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    let explicit_command = stdin_to_file_command(&explicit_path);
    TestRequest::send_ok(
        &handler,
        copy_mode_command(
            &target,
            ["copy-pipe-and-cancel", "--", explicit_command.as_str()],
        ),
    )
    .await;

    let explicit_output = wait_for_file_containing(&explicit_path, "needle explicit").await;
    let fallback_output = fs::read_to_string(&fallback_path).ok();
    let _ = fs::remove_file(&explicit_path);
    let _ = fs::remove_file(&fallback_path);
    assert!(explicit_output.contains("needle explicit"));
    assert!(
        fallback_output.is_none(),
        "copy-command fallback should not run when copy-pipe has an explicit command"
    );
}

#[tokio::test]
async fn copy_mode_buffer_yank_emits_clipboard_when_set_clipboard_enabled() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "copy-mode-clipboard",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    let requester_pid = 42;
    handler
        .set_option_by_name(
            OptionScopeSelector::ServerGlobal,
            "set-clipboard",
            "external",
        )
        .await;

    let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
    handler
        .register_attach_with_terminal_context(
            requester_pid,
            target.session_name().clone(),
            control_tx,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color")]),
        )
        .await;

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle clipboard\r\nomega\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "needle clipboard", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    let yanked = handler
        .dispatch(
            requester_pid,
            Request::SendKeysExt(copy_mode_command(&target, ["copy-selection"])),
        )
        .await
        .response;
    assert!(matches!(yanked, Response::SendKeys(_)));

    let mut bytes = None;
    while let Ok(control) = control_rx.try_recv() {
        bytes = take_write(control).or(bytes);
        if bytes.is_some() {
            break;
        }
    }
    let bytes = bytes.expect("clipboard write");
    assert_eq!(bytes, b"\x1b]52;;bmVlZGxlIGNsaXBib2FyZA==\x07");
}

fn write_executable_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script permissions");
}

#[tokio::test]
async fn copy_pipe_uses_local_osc7_file_url_as_working_directory() {
    let handler = RequestHandler::new();
    let target = create_session(
        &handler,
        "copy-pipe-osc7-cwd",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    // Inside the seed, and with the space kept: the filter writes `copied.txt` *relative* to the
    // directory OSC7 announced, and a relative write only reaches the host when it publishes into
    // the seed. A directory outside the seed is not a place a job can be opened over at all — the
    // job would fall back to the seed root and the file would appear nowhere near here.
    let temp_dir = crate::pane_terminals::seed_scratch_dir(&handler, "copy pipe cwd")
        .path()
        .to_path_buf();
    let output_path = temp_dir.join("copied.txt");

    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle osc7 cwd\r\nomega\r\n",
    )
    .await;
    {
        let mut state = handler.state.lock().await;
        let osc7 = format!("\x1b]7;file://localhost{}\x07", file_url_path(&temp_dir));
        state
            .append_bytes_to_pane_transcript_for_test(
                target.session_name(),
                target.window_index(),
                target.pane_index(),
                osc7.as_bytes(),
            )
            .expect("OSC7 bytes append to pane transcript");
    }
    wait_for_capture(&handler, &target, "needle osc7 cwd", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    let relative_command = stdin_to_relative_file_command("copied.txt");
    TestRequest::send_ok(
        &handler,
        copy_mode_command(
            &target,
            ["copy-pipe-and-cancel", "--", relative_command.as_str()],
        ),
    )
    .await;

    let output = wait_for_file_containing(&output_path, "needle osc7 cwd").await;
    let _ = fs::remove_file(&output_path);
    assert!(output.contains("needle osc7 cwd"));
}

#[tokio::test]
async fn copy_pipe_uses_bin_sh_instead_of_default_shell_like_tmux() {
    let handler = RequestHandler::new();
    let root = unique_temp_path("copy-pipe-bin-sh");
    fs::create_dir_all(&root).expect("temp dir exists");
    let fake_shell = root.join("fake-shell.sh");
    let marker_path = root.join("default-shell-used.txt");
    let output_path = root.join("copy-pipe-output.txt");
    write_executable_script(
        &fake_shell,
        &format!(
            "#!/bin/sh\nprintf used > {}\nexit 42\n",
            crate::test_shell::sh_quote_path(&marker_path)
        ),
    );
    handler
        .set_option_by_name(
            OptionScopeSelector::ServerGlobal,
            "default-shell",
            &fake_shell.to_string_lossy(),
        )
        .await;

    let target = create_session(
        &handler,
        "copy-pipe-bin-sh",
        TerminalSize { cols: 40, rows: 4 },
    )
    .await;
    replace_transcript_contents(
        &handler,
        &target,
        TerminalSize { cols: 40, rows: 4 },
        b"alpha\r\nneedle bin sh\r\nomega\r\n",
    )
    .await;
    wait_for_capture(&handler, &target, "needle bin sh", false).await;

    TestRequest::send_ok(&handler, CopyModeRequest::fixture(&target)).await;
    TestRequest::send_ok(
        &handler,
        copy_mode_command(&target, ["search-backward", "--", "needle"]),
    )
    .await;
    TestRequest::send_ok(&handler, copy_mode_command(&target, ["select-line"])).await;
    let explicit_command = stdin_to_file_command(&output_path);
    TestRequest::send_ok(
        &handler,
        copy_mode_command(
            &target,
            ["copy-pipe-and-cancel", "--", explicit_command.as_str()],
        ),
    )
    .await;

    let output = wait_for_file_containing(&output_path, "needle bin sh").await;
    assert!(output.contains("needle bin sh"));
    assert!(
        !marker_path.exists(),
        "copy-pipe should not execute default-shell for tmux jobs"
    );
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(&fake_shell);
    let _ = fs::remove_dir_all(&root);
}
