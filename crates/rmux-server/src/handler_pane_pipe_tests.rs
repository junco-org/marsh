use std::fs;
use std::path::Path;
use std::time::Duration;

use super::RequestHandler;
use rmux_proto::{
    DisplayMessageRequest, KillPaneRequest, NewSessionRequest, PaneTarget, PipePaneRequest,
    Request, Response, SendKeysRequest, SessionName, Target, TerminalSize,
};
use tokio::time::sleep;

const PANE_PIPE_TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn session_name(value: &str) -> SessionName {
    SessionName::new(value).expect("valid session name")
}

/// A pipe command that logs the first line it is given and then exits.
///
/// Exiting is the point. A pipe command's writes are staged in its job's own snapshot of the seed
/// and reach the seed only when that command reaches an approved boundary; a logger that ends on
/// its own first line gives the test a real boundary to wait for instead of one it would have to
/// guess at.
///
/// `name` is relative for the same reason. A pipe job runs against its snapshot, so `name` is
/// where the log is staged and `<seed>/name` is where it appears once the gate approves. Handing
/// the command an absolute `<seed>/name` instead names a path outside the tree it is running
/// against, and the log is then written nowhere at all — not staged, not published.
#[cfg(unix)]
fn first_line_to_file_command(name: &str) -> String {
    format!("head -n 1 > {}", crate::test_shell::sh_quote(name))
}

#[cfg(windows)]
fn first_line_to_file_command(name: &str) -> String {
    crate::test_shell::powershell_encoded_command(&format!(
        "$line=[Console]::In.ReadLine(); [System.IO.File]::WriteAllText((Join-Path \
         (Get-Location).Path {}), $line)",
        crate::test_shell::powershell_quote(name)
    ))
}

/// A pipe command that logs everything it is given and never ends on its own.
///
/// Never ending is the point. A logger that finishes by itself reaches its own approved boundary
/// and publishes, which says nothing about teardown; this one is still running when its pane is
/// killed, so the only thing deciding the log's fate is how that teardown ends the job.
///
/// `name` is relative for the reason given on [`first_line_to_file_command`].
#[cfg(unix)]
fn all_input_to_file_command(name: &str) -> String {
    format!("cat > {}", crate::test_shell::sh_quote(name))
}

#[cfg(windows)]
fn all_input_to_file_command(name: &str) -> String {
    crate::test_shell::powershell_encoded_command(&format!(
        "$text=[Console]::In.ReadToEnd(); [System.IO.File]::WriteAllText((Join-Path \
         (Get-Location).Path {}), $text)",
        crate::test_shell::powershell_quote(name)
    ))
}

fn pipe_discard_command() -> String {
    crate::test_shell::stdin_discard_command()
}

#[cfg(unix)]
fn pane_print_command(text: &str) -> String {
    format!("printf '{}\\n'", text.replace('\'', r"'\''"))
}

#[cfg(windows)]
fn pane_print_command(text: &str) -> String {
    format!("echo {text}")
}

async fn create_session(handler: &RequestHandler, name: &str) {
    let response = handler
        .handle(Request::NewSession(NewSessionRequest {
            session_name: session_name(name),
            detached: true,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            environment: None,
        }))
        .await;
    assert!(matches!(response, Response::NewSession(_)));
    handler
        .wait_for_pane_startup_to_finish_for_test(&PaneTarget::new(session_name(name), 0))
        .await;
}

async fn display_pane_format(
    handler: &RequestHandler,
    target: PaneTarget,
    message: &str,
) -> String {
    let response = handler
        .handle(Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Pane(target)),
            print: true,
            message: Some(message.to_owned()),
            empty_target_context: false,
        }))
        .await;
    let Response::DisplayMessage(response) = response else {
        panic!("expected display-message response");
    };
    let output = response
        .command_output()
        .expect("display-message -p returns output");
    String::from_utf8_lossy(output.stdout())
        .trim_end()
        .to_owned()
}

async fn wait_for_pane_process(handler: &RequestHandler, target: PaneTarget) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let last = display_pane_format(handler, target.clone(), "#{pane_current_command}").await;
        if !last.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for pane process; last command={last:?}"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn pipe_pane(
    handler: &RequestHandler,
    target: PaneTarget,
    once: bool,
    command: Option<String>,
) {
    let response = handler
        .handle(Request::PipePane(PipePaneRequest {
            target,
            stdin: false,
            stdout: true,
            once,
            command,
        }))
        .await;
    assert!(
        matches!(response, Response::PipePane(_)),
        "pipe-pane should succeed, got {response:?}"
    );
}

async fn send_pane_line(handler: &RequestHandler, target: PaneTarget, text: &str) {
    let response = handler
        .handle(Request::SendKeys(SendKeysRequest {
            target,
            keys: vec![pane_print_command(text), "Enter".to_owned()],
        }))
        .await;
    assert!(matches!(response, Response::SendKeys(_)));
}

async fn wait_for_file_contains(path: &Path, expected: &str) {
    let deadline = tokio::time::Instant::now() + PANE_PIPE_TEST_TIMEOUT;
    loop {
        match fs::read_to_string(path) {
            Ok(contents) if contents.contains(expected) => return,
            Ok(_) | Err(_) if tokio::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(25)).await;
            }
            Ok(contents) => panic!(
                "timed out waiting for {} to contain {:?}, got {:?}",
                path.display(),
                expected,
                contents
            ),
            Err(error) => panic!(
                "timed out waiting for {} to exist containing {:?}: {error}",
                path.display(),
                expected
            ),
        }
    }
}

/// Waits until some job has *staged* `name` with `expected` written into it.
///
/// The immediate children of `snapshot_parent` are this session's per-job snapshots, so a pipe
/// command's log is at `<snapshot_parent>/<uid>/<name>` for as long as the job is running and
/// nowhere else. Waiting for it is what stops a later absence in the seed from being vacuous:
/// without this, a log that was never written yet and a log that was written and then discarded
/// look identical from the seed.
async fn wait_for_staged_file_contains(snapshot_parent: &Path, name: &str, expected: &str) {
    let deadline = tokio::time::Instant::now() + PANE_PIPE_TEST_TIMEOUT;
    loop {
        if let Ok(entries) = fs::read_dir(snapshot_parent) {
            for entry in entries.flatten() {
                let staged = entry.path().join(name);
                if fs::read_to_string(&staged).is_ok_and(|contents| contents.contains(expected)) {
                    return;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a staged {name} containing {expected:?} under {}",
            snapshot_parent.display()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

/// `pipe-pane -o` closes the running pipe and does not open the replacement it was given.
///
/// Both logs are named against the seed. The first one is what makes the absence below mean
/// something — it reaches `<seed>/once-first` once its logger ends on its own first line, which is
/// an approved boundary — and the second is the actual subject: a command that never ran has
/// nothing to publish, which is only a meaningful absence where a command that *had* run would
/// have left something.
#[tokio::test]
async fn pipe_pane_once_closes_existing_pipe_without_reopening() {
    let handler = RequestHandler::new();
    let Ok(io) = crate::managed_workload::handler_facade(&handler) else {
        return;
    };
    let seed = io.executor_info().seed.expect("test engine has a seed");
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let first_output = seed.join("once-first");
    let second_output = seed.join("once-second");
    create_session(&handler, "alpha").await;
    wait_for_pane_process(&handler, target.clone()).await;

    pipe_pane(
        &handler,
        target.clone(),
        false,
        Some(first_line_to_file_command("once-first")),
    )
    .await;
    send_pane_line(&handler, target.clone(), "pipe-one").await;
    wait_for_file_contains(&first_output, "pipe-one").await;

    pipe_pane(
        &handler,
        target.clone(),
        true,
        Some(first_line_to_file_command("once-second")),
    )
    .await;
    assert_eq!(
        display_pane_format(&handler, target.clone(), "#{pane_pipe}").await,
        "0",
        "pipe-pane -o must close the existing pipe without opening a replacement"
    );

    send_pane_line(&handler, target, "pipe-two").await;
    assert!(
        !second_output.exists(),
        "the replacement pipe's command must never have run"
    );

    let _ = fs::remove_file(first_output);
    let _ = fs::remove_file(second_output);
}

#[tokio::test]
async fn pane_pipe_format_reports_active_pipe_state() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    create_session(&handler, "alpha").await;

    assert_eq!(
        display_pane_format(&handler, target.clone(), "#{pane_pipe}").await,
        "0"
    );
    pipe_pane(
        &handler,
        target.clone(),
        false,
        Some(pipe_discard_command()),
    )
    .await;
    assert_eq!(
        display_pane_format(&handler, target.clone(), "#{pane_pipe}").await,
        "1"
    );
    pipe_pane(&handler, target.clone(), false, None).await;
    assert_eq!(
        display_pane_format(&handler, target, "#{pane_pipe}").await,
        "0"
    );
}

/// A killed pane's pipe log is discarded rather than published.
///
/// The close a user asks for is a boundary the gate may approve. A teardown is not that one:
/// nothing asked for this log, and the pane it was being kept for is gone. The distinction has to
/// be made where the job is ended, because ending a logger by closing its input is
/// indistinguishable from a clean close from the command's side — `cat` sees a real end of file,
/// exits zero, and a zero exit through the normal boundary is publishable. That is how a partial
/// log reached the seed with nobody waiting for it.
///
/// The log is waited for in the pipe job's snapshot first, and only then is the pane killed. A
/// pipe command's writes are staged there and reach the seed only on an approved boundary, so
/// without that wait the seed would be empty afterwards whether the log had been discarded or
/// simply never written — which is no assertion at all.
#[tokio::test]
async fn killing_a_pane_discards_its_pipe_log_instead_of_publishing_it() {
    let handler = RequestHandler::new();
    let Ok(io) = crate::managed_workload::handler_facade(&handler) else {
        return;
    };
    let info = io.executor_info();
    let seed = info.seed.expect("test engine has a seed");
    let snapshot_parent = info
        .snapshot_parent
        .expect("test engine stages jobs under a snapshot directory");
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let output = seed.join("killed-pipe-log");
    create_session(&handler, "alpha").await;
    wait_for_pane_process(&handler, target.clone()).await;

    pipe_pane(
        &handler,
        target.clone(),
        false,
        Some(all_input_to_file_command("killed-pipe-log")),
    )
    .await;
    assert_eq!(
        display_pane_format(&handler, target.clone(), "#{pane_pipe}").await,
        "1",
        "the pipe must be carrying the pane's output before its pane is killed"
    );
    send_pane_line(&handler, target.clone(), "pipe-killed").await;
    wait_for_staged_file_contains(&snapshot_parent, "killed-pipe-log", "pipe-killed").await;
    assert!(
        !output.exists(),
        "a running pipe's log is staged, not published, so it must not be in the seed yet"
    );

    let killed = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: target.clone(),
            kill_all_except: false,
        }))
        .await;
    assert!(
        matches!(killed, Response::KillPane(_)),
        "kill-pane should succeed, got {killed:?}"
    );

    // Bounded, and calibrated against the publishing path rather than guessed: a close that
    // reaches the gate publishes within the pipe's own termination grace, so a log still absent
    // well past that was discarded rather than merely slow. Checked throughout, not only at the
    // end, because a log that appears and is then removed still reached the seed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        assert!(
            !output.exists(),
            "a killed pane's pipe log must never reach the seed, found {}",
            output.display()
        );
        sleep(Duration::from_millis(25)).await;
    }

    let _ = fs::remove_file(output);
}
