use std::fs;
use std::path::Path;
use std::time::Duration;

use super::RequestHandler;
use rmux_proto::{KillPaneRequest, PaneTarget, PipePaneRequest, SendKeysRequest};

const PANE_PIPE_TEST_TIMEOUT: Duration = Duration::from_secs(15);

use crate::test_fixtures::wait_until;
use crate::test_names::session_name;

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
fn first_line_to_file_command(name: &str) -> String {
    format!("head -n 1 > {}", crate::test_shell::sh_quote(name))
}

fn pipe_discard_command() -> String {
    crate::test_shell::stdin_discard_command()
}

fn pane_print_command(text: &str) -> String {
    format!("printf '{}\\n'", text.replace('\'', r"'\''"))
}

async fn display_pane_format(
    handler: &RequestHandler,
    target: PaneTarget,
    message: &str,
) -> String {
    String::from_utf8_lossy(&handler.display_print(target, message).await)
        .trim_end()
        .to_owned()
}

async fn wait_for_pane_process(handler: &RequestHandler, target: PaneTarget) {
    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || {
            let last =
                display_pane_format(handler, target.clone(), "#{pane_current_command}").await;
            if last.is_empty() {
                Err(last)
            } else {
                Ok(())
            }
        },
    )
    .await
    .unwrap_or_else(|last| panic!("timed out waiting for pane process; last command={last:?}"));
}

async fn pipe_pane(
    handler: &RequestHandler,
    target: PaneTarget,
    once: bool,
    command: Option<String>,
) {
    handler
        .handle_ok(PipePaneRequest {
            target,
            stdin: false,
            stdout: true,
            once,
            command,
        })
        .await;
}

async fn send_pane_line(handler: &RequestHandler, target: PaneTarget, text: &str) {
    handler
        .handle_ok(SendKeysRequest {
            target,
            keys: vec![pane_print_command(text), "Enter".to_owned()],
        })
        .await;
}

async fn wait_for_file_contains(path: &Path, expected: &str) {
    wait_until(
        PANE_PIPE_TEST_TIMEOUT,
        Duration::from_millis(25),
        async || match fs::read_to_string(path) {
            Ok(contents) if contents.contains(expected) => Ok(()),
            last => Err(last),
        },
    )
    .await
    .unwrap_or_else(|last| {
        panic!(
            "timed out waiting for {} to contain {expected:?}, got {last:?}",
            path.display()
        )
    });
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
    let seed = io.default_dir().to_path_buf();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let first_output = seed.join("once-first");
    let second_output = seed.join("once-second");
    handler.create_started_session(&alpha).await;
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
    handler.create_started_session(&alpha).await;

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
/// The logger acknowledges a real write outside the source, then its public command receipt
/// proves termination. No snapshot paths or quiet-period inference are needed.
#[tokio::test]
async fn killing_a_pane_discards_its_pipe_log_instead_of_publishing_it() {
    let handler = RequestHandler::new();
    let Ok(io) = crate::managed_workload::handler_facade(&handler) else {
        return;
    };
    let seed = io.default_dir().to_path_buf();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let output = seed.join("killed-pipe-log");
    let ready = tempfile::NamedTempFile::new().expect("logger acknowledgement");
    let script = format!("while IFS= read -r line; do printf '%s\\n' \"$line\" >> killed-pipe-log; case \"$line\" in *pipe-killed*) printf ready > {};; esac; done", crate::test_shell::sh_quote(&ready.path().to_string_lossy()));
    let logger = format!("/bin/sh -c {}", crate::test_shell::sh_quote(&script));
    handler.create_started_session(&alpha).await;
    wait_for_pane_process(&handler, target.clone()).await;

    pipe_pane(&handler, target.clone(), false, Some(logger)).await;
    assert_eq!(
        display_pane_format(&handler, target.clone(), "#{pane_pipe}").await,
        "1",
        "the pipe must be carrying the pane's output before its pane is killed"
    );
    send_pane_line(&handler, target.clone(), "pipe-killed").await;
    wait_for_file_contains(ready.path(), "ready").await;
    let receipt = io
        .snapshot()
        .state
        .commands
        .into_iter()
        .find(|command| command.text().contains("killed-pipe-log"))
        .expect("the logger still owns an admitted command");
    assert!(
        !output.exists(),
        "a running pipe's log is staged, not published, so it must not be in the seed yet"
    );

    handler
        .handle_ok(KillPaneRequest {
            target: target.clone(),
            kill_all_except: false,
        })
        .await;

    let completion = tokio::time::timeout(PANE_PIPE_TEST_TIMEOUT, receipt.wait())
        .await
        .expect("logger termination is bounded")
        .expect("logger produces a verdict");
    assert!(!completion.is_published(), "forced logger must not publish");
    assert!(!output.exists(), "the staged pipe log was discarded");

    let _ = fs::remove_file(output);
}
