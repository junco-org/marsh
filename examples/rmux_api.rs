#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::panic_in_result_fn
)]
//! Ordinary rmux construction and execution: `rmux_api <fresh-git-seed> <unused-socket>`.
//!
//! The seed is a disposable btrfs subvolume with an empty Git root commit. No caller constructs
//! policy history, executors, snapshots or recovery handles. Both daemon generations use the same
//! normal constructor, and durable ownership is proved by a later command's denial.

use marsh::rmux::types::{
    CommandOptions, DaemonConfig, ProcessCommand, RunError, ShellEnvironment, ShellErrorKind,
    ShellId, SpawnOptions, TerminalGeometry, protocol,
};
use marsh::rmux::{CollectOptions, ExecutionSpec, IoError, IoPhase, RmuxFrontend};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (seed, socket) = match (arguments.next(), arguments.next(), arguments.next()) {
        (Some(seed), Some(socket), None) => (PathBuf::from(seed), PathBuf::from(socket)),
        _ => return Err("usage: rmux_api <fresh-git-seed> <unused-socket>".into()),
    };
    let limit = Duration::from_secs(30);
    let zero = seed.join("api-zero");
    if zero.try_exists()? {
        return Err("the example requires a fresh disposable seed".into());
    }
    std::fs::write(&zero, b"same")?;
    let frontend = RmuxFrontend::open(
        DaemonConfig::new(socket.clone()),
        &seed,
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;
    let retained = frontend.io();
    let first = prove_first_generation(&frontend, &seed, limit).await;
    let stopped = tokio::time::timeout(limit, frontend.shutdown()).await;
    first?;
    stopped??;
    assert_eq!(retained.snapshot().phase, IoPhase::Closed);
    assert!(matches!(
        retained
            .open_shell(&seed, None, SpawnOptions::default())
            .await,
        Err(IoError::Closed)
    ));

    let reopened = RmuxFrontend::open(
        DaemonConfig::new(socket.clone()),
        &seed,
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;
    let second = prove_second_generation(&reopened, &seed, limit).await;
    if let Err(error) = second {
        reopened.shutdown().await?;
        return Err(error);
    }
    let mut connection = reopened.open_protocol().await?;
    let response = tokio::time::timeout(
        limit,
        tokio::task::spawn_blocking(move || {
            connection.roundtrip(&protocol::Request::KillServer(protocol::KillServerRequest))
        }),
    )
    .await???;
    assert!(matches!(response, protocol::Response::KillServer(_)));
    tokio::time::timeout(limit, reopened.wait()).await??;
    assert!(!socket.exists());
    println!(
        "rmux_api: shared authority, independent streams, durable denial and automatic recovery verified"
    );
    Ok(())
}

/// First generation: two shells share one seed, a blind edit is refused, and streams stay apart.
async fn prove_first_generation(
    frontend: &RmuxFrontend,
    seed: &Path,
    limit: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let writer = frontend
        .open_shell(seed, Some(ShellId::from("writer")), SpawnOptions::default())
        .await?;
    let other = frontend
        .open_shell(seed, Some(ShellId::from("other")), SpawnOptions::default())
        .await?;
    assert_eq!(writer.sandbox().seed, other.sandbox().seed);
    let completion = tokio::time::timeout(
        limit,
        writer.run_command(
            "printf owner > owned; printf released > released; release -- released; printf same > api-zero",
            CommandOptions::default(),
        ),
    )
    .await??;
    assert_eq!(completion.exit_code(), Some(0));
    assert!(completion.is_published());
    let refused = tokio::time::timeout(
        limit,
        other.run_command("printf blind > owned", CommandOptions::default()),
    )
    .await?;
    let Err(IoError::Run(RunError::Execution { completion })) = refused else {
        return Err("blind edit was not refused".into());
    };
    assert_eq!(completion.exit_code(), Some(0));
    assert!(matches!(
        completion.result.as_ref().as_ref().err().unwrap().kind(),
        ShellErrorKind::Denied { .. }
    ));
    assert_eq!(std::fs::read(seed.join("owned"))?, b"owner");
    let execution = frontend
        .execute(ExecutionSpec {
            initial_dir: seed.to_path_buf(),
            id: None,
            process: ProcessCommand::Shell("printf stdout; printf stderr >&2".into()),
            environment: None,
        })
        .await?;
    let captured =
        tokio::time::timeout(limit, execution.collect(CollectOptions::default())).await??;
    assert_eq!(captured.stdout, b"stdout");
    assert_eq!(captured.stderr, b"stderr");
    assert!(captured.completion.is_published());
    Ok(())
}

/// Second generation: ownership recorded before the restart still denies, and released work runs.
async fn prove_second_generation(
    reopened: &RmuxFrontend,
    seed: &Path,
    limit: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let writer = reopened
        .open_shell(seed, Some(ShellId::from("writer")), SpawnOptions::default())
        .await?;
    for path in ["owned", "api-zero"] {
        let refused = tokio::time::timeout(
            limit,
            writer.run_command(&format!("printf blind > {path}"), CommandOptions::default()),
        )
        .await?;
        let Err(IoError::Run(RunError::Execution { completion })) = refused else {
            return Err("durable ownership was lost".into());
        };
        assert!(matches!(
            completion.result.as_ref().as_ref().err().unwrap().kind(),
            ShellErrorKind::Denied { .. }
        ));
    }
    let completion = tokio::time::timeout(
        limit,
        writer.run_command(
            "/bin/cat released >/dev/null; printf after-reopen > released",
            CommandOptions::default(),
        ),
    )
    .await??;
    assert!(completion.is_published());
    assert_eq!(std::fs::read(seed.join("released"))?, b"after-reopen");
    Ok(())
}
