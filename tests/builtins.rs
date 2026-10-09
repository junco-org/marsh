#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! What `exec` does in a shell that has to stay alive.
//!
//! Every behavior runs through the ordinary managed Shell interface and the real native tracer.

mod common;

use brush_core::ExecutionControlFlow;
use common::{Seed, run, status};

/// `exec` is the one builtin that must not replace the process: the session's records and its
/// unpublished effects live in this process. It runs the program and then ends the shell.
#[tokio::test]
async fn exec_runs_the_program_and_exits_the_shell() {
    let seed = Seed::new("seed", "");
    let shell = seed.shell().await;

    let result = run(&shell, "exec touch made.txt; printf after > not.txt").await;

    assert!(seed.source.join("made.txt").exists(), "the program ran");
    assert!(
        !seed.source.join("not.txt").exists(),
        "nothing after `exec` runs"
    );
    assert!(
        matches!(result.next_control_flow, ExecutionControlFlow::ExitShell),
        "the shell is asked to exit"
    );
    assert_eq!(u8::from(result.exit_code), 0);
    assert!(shell.is_closed());
    shell.close(false).await.expect("close shell");
}

/// With no program, `exec` is a redirection statement: its file descriptors become the shell's and
/// outlive the line.
#[tokio::test]
async fn exec_without_a_program_applies_its_redirections() {
    let seed = Seed::new("seed", "");
    let shell = seed.shell().await;

    assert_eq!(status(&shell, "exec 3> fd.txt").await, 0);
    assert_eq!(status(&shell, "printf x >&3").await, 0);

    assert_eq!(seed.bytes("fd.txt"), b"x");
    shell.close(false).await.expect("close shell");
}

/// `-c` would run the program without the environment the session attributes its effects through,
/// so it is refused rather than approximated.
#[tokio::test]
async fn exec_refuses_an_empty_environment() {
    let seed = Seed::new("seed", "");
    let shell = seed.shell().await;

    assert_eq!(status(&shell, "exec -c true 2>/dev/null").await, 2);
    shell.close(false).await.expect("close shell");
}

/// A program that is not on `PATH` is reported as bash's 127, not as a panic or a spawner error.
#[tokio::test]
async fn exec_reports_a_missing_program() {
    let seed = Seed::new("seed", "");
    let shell = seed.shell().await;

    assert_eq!(
        status(&shell, "exec definitely-not-a-program-xyz 2>/dev/null").await,
        127
    );
    shell.close(false).await.expect("close shell");
}
