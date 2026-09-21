#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    reason = "a runnable consumer's failure mode is an immediate, loud abort naming the \
              observation that did not hold"
)]
#![allow(
    clippy::too_many_lines,
    reason = "this file is one documented `main` whose ordered observations are the proof; \
              splitting it into helpers would hide that order and it is quoted verbatim by \
              the README"
)]
//! An application driving rmux through nothing but `marsh::rmux`: `<seed> <socket>`.
//!
//! Run it as `cargo run -p marsh --example rmux_api -- "$WORK/seed" "$WORK/rmux.sock"`, where the
//! seed is a fresh btrfs subvolume holding a Git repository with an empty root commit — exactly
//! what the README's setup section prepares — and nothing is listening on the socket.
//!
//! Two things at once. It is the *library sample*: every type below comes from `marsh::rmux`,
//! `marsh::rmux::types`, the standard library or `tokio`, with the single deliberate exception of
//! `marsh::PolicyValidator`, which the caller owns because whose history a seed is judged against
//! is the caller's decision. And it is a *proof*: it opens a real seed, publishes through the
//! gate, has a second principal refused, restarts the whole system over the same seed with an
//! empty validator, and shows that both the refusal and the earlier release survived the restart.
//!
//! What it observes, in order: a staged write reaches the seed; a second principal's overwrite
//! exits zero and is denied; a `git add` is a runtime Stage the supplied validator sees; a pipe
//! execution carries byte-exact separate streams; retained handles cannot keep a shut-down daemon
//! alive; reopening adopts the seed's durable grants before admitting work, so a reused job name
//! inherits nothing; and a `kill-server` over the real wire ends the daemon and reclaims its
//! snapshots.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use marsh::rmux::types::{
    Action, protocol, CommandOptions, DaemonConfig, Outcome, ProcessCommand, ShellEnvironment,
    ShellId, SpawnOptions, TerminalGeometry,
};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, IoError, IoPhase, OutputLimit, OverflowPolicy, RmuxFrontend,
};
use marsh::PolicyValidator;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Every wait in this program is bounded by the same window: a hang is a failure, not a slow
    // success, and an unbounded await would turn one into the other.
    let limit = Duration::from_secs(30);

    let mut arguments = std::env::args_os().skip(1);
    let (seed, socket) = match (arguments.next(), arguments.next(), arguments.next()) {
        (Some(seed), Some(socket), None) => {
            (std::path::PathBuf::from(seed), std::path::PathBuf::from(socket))
        }
        _ => return Err("usage: rmux_api <seed> <socket>".into()),
    };

    // The caller's own validator. This is marsh's junco-policy adapter and its committed history,
    // not something the frontend chooses: passing it in is what makes step 5 below able to check
    // that the daemon really judged against *this* history.
    let validator = Arc::new(Mutex::new(PolicyValidator::new()));

    // ---- Phase one: a live daemon over a fresh seed. -----------------------------------------
    let frontend = RmuxFrontend::open(
        DaemonConfig::new(socket.clone()),
        &seed,
        Arc::clone(&validator),
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;

    // Kept alive across the shutdown below, which is the point of step 7.
    let retained = frontend.io();
    let mut writer = None;
    let mut other = None;

    let phase = async {
        // The canonical seed, as the daemon resolved it. A relative or symlinked argument names
        // the same subvolume; this is the spelling every path check below is made against.
        let canonical = frontend
            .executor_info()
            .seed
            .ok_or("the frontend leases no seed")?;

        let writer_job = frontend
            .spawn("", Some(ShellId::from("writer")), None, SpawnOptions::default())
            .await?;
        let other_job = frontend
            .spawn("", Some(ShellId::from("other")), None, SpawnOptions::default())
            .await?;
        writer = Some(writer_job.clone());
        other = Some(other_job.clone());

        // 3. One line, two files. A zero exit is not an approval, so both are required.
        let command = frontend
            .start_in(
                &writer_job,
                "printf owner > owned; printf staged > released",
                CommandOptions::default(),
            )
            .await?;
        let published = tokio::time::timeout(limit, command.wait()).await??;
        assert_eq!(published.exit_code, Some(0), "the process said zero");
        assert!(published.is_published(), "and the gate agreed");
        assert_eq!(std::fs::read(canonical.join("owned"))?, b"owner");
        assert_eq!(std::fs::read(canonical.join("released"))?, b"staged");

        // 4. A second principal overwrites the first's file. The process succeeds; the
        //    publication does not, and the seed keeps the original bytes.
        let command = frontend
            .start_in(&other_job, "printf intruder > owned", CommandOptions::default())
            .await?;
        let denied = tokio::time::timeout(limit, command.wait()).await??;
        assert_eq!(denied.exit_code, Some(0), "the process still said zero");
        assert!(
            matches!(denied.outcome.as_ref(), Ok(Outcome::Denied { .. })),
            "a zero exit is not an approval"
        );
        assert_eq!(std::fs::read(canonical.join("owned"))?, b"owner");

        // 5. `git add` is a runtime Stage request, not repository bookkeeping: it releases
        //    `released`'s unstaged ownership. `owned` is deliberately left owned, which is what
        //    step 8 restarts into. The supplied validator must have seen the grant — a
        //    constructor that ignored it would still publish, and this is what catches that.
        let command = frontend
            .start_in(&writer_job, "git add -- released", CommandOptions::default())
            .await?;
        let staged = tokio::time::timeout(limit, command.wait()).await??;
        assert_eq!(staged.exit_code, Some(0));
        assert!(staged.is_published());
        assert!(
            validator
                .lock()
                .expect("the caller's validator")
                .history()
                .iter()
                .any(|event| event.action == Action::Stage),
            "the daemon judged against the validator this caller supplied"
        );

        // 6. A pipe execution: two real streams, byte-exact, never merged.
        let execution = frontend
            .execute(ExecutionSpec {
                directory: String::new(),
                id: None,
                process: ProcessCommand::Shell("printf stdout; printf stderr >&2".to_owned()),
                environment: None,
            })
            .await?;
        let captured = tokio::time::timeout(
            limit,
            execution.collect(CollectOptions {
                limit: OutputLimit::Bytes(65_536),
                overflow: OverflowPolicy::Error,
            }),
        )
        .await??;
        assert_eq!(captured.stdout, b"stdout", "stdout is its own stream");
        assert_eq!(captured.stderr, b"stderr", "and stderr is never merged into it");
        assert_eq!(captured.completion.exit_code, Some(0));
        assert!(captured.completion.is_published());

        Ok::<std::path::PathBuf, Box<dyn std::error::Error>>(canonical)
    }
    .await;

    // Teardown happens whatever the phase made of itself: an explicit shutdown ends the listener
    // and releases the seed however many handles are still held.
    let released = tokio::time::timeout(limit, frontend.shutdown()).await;
    let canonical = match (phase, released) {
        (Ok(canonical), Ok(Ok(()))) => canonical,
        (Ok(_), Ok(Err(error))) => return Err(error.into()),
        (Ok(_), Err(elapsed)) => return Err(elapsed.into()),
        (Err(error), Ok(Ok(()))) => return Err(error),
        (Err(error), teardown) => {
            eprintln!("rmux_api: shutdown also failed: {teardown:?}");
            return Err(error);
        }
    };

    // 7. The daemon is gone, and the handles that outlived it hold nothing.
    assert!(!socket.exists(), "an explicit shutdown removes the socket");
    assert_eq!(retained.snapshot().phase, IoPhase::Closed);
    assert!(
        matches!(
            retained.spawn("", None, None, SpawnOptions::default()).await,
            Err(IoError::Closed)
        ),
        "a retained handle refuses work rather than reaching a released engine"
    );

    // 8. Reopen the same seed on the same socket, with an empty validator that knows nothing.
    let state = canonical
        .parent()
        .ok_or("the seed has no parent")?
        .join(".marsh")
        .join(canonical.file_name().ok_or("the seed has no name")?);
    let wal = state.join("meta").join("wal.jsonl");
    let before = std::fs::read(&wal)?;

    let reopened = RmuxFrontend::open(
        DaemonConfig::new(socket.clone()),
        &seed,
        Arc::new(Mutex::new(PolicyValidator::new())),
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;

    let phase = async {
        // Read before a single line is admitted: recovery installs the seed's durable history
        // during construction, so this is the seed's property and not this process's.
        assert!(
            reopened
                .history()
                .iter()
                .any(|event| event.action == Action::Stage),
            "reopening adopts the grants the previous run published"
        );

        // The same *name*, a different snapshot uid. Rights belong to the uid, so this job
        // inherits nothing from the `writer` that earned them.
        let job = reopened
            .spawn("", Some(ShellId::from("writer")), None, SpawnOptions::default())
            .await?;
        let command = reopened
            .start_in(&job, "printf intruder > owned", CommandOptions::default())
            .await?;
        let denied = tokio::time::timeout(limit, command.wait()).await??;
        assert_eq!(denied.exit_code, Some(0));
        assert!(
            matches!(denied.outcome.as_ref(), Ok(Outcome::Denied { .. })),
            "an unstaged path stays owned by a principal that no longer exists"
        );
        assert_eq!(std::fs::read(canonical.join("owned"))?, b"owner");
        assert_eq!(
            std::fs::read(&wal)?,
            before,
            "a replay and a refusal write nothing to the log"
        );

        // 9. `released` was staged before the restart, and the release survived it: this is the
        //    other half of durability, and the reason recovery is not "fail closed for every
        //    path".
        let command = reopened
            .start_in(&job, "printf after-reopen > released", CommandOptions::default())
            .await?;
        let republished = tokio::time::timeout(limit, command.wait()).await??;
        assert_eq!(republished.exit_code, Some(0));
        assert!(republished.is_published());
        assert_eq!(std::fs::read(canonical.join("released"))?, b"after-reopen");

        // 10. The full wire vocabulary, over the socket this frontend bound. Protocol I/O after
        //     the connect is blocking, so it belongs on a blocking worker.
        let connection = reopened.open_protocol().await?;
        let response = tokio::time::timeout(
            limit,
            tokio::task::spawn_blocking(move || {
                let mut connection = connection;
                connection.roundtrip(&protocol::Request::KillServer(protocol::KillServerRequest))
            }),
        )
        .await???;
        assert!(matches!(response, protocol::Response::KillServer(_)));
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    // A successful `kill-server` has already ended the listener, so `wait` is the ending that
    // belongs to it; anything else still needs an explicit stop.
    match phase {
        Ok(()) => tokio::time::timeout(limit, reopened.wait()).await??,
        Err(error) => {
            if let Err(teardown) = tokio::time::timeout(limit, reopened.shutdown()).await {
                eprintln!("rmux_api: shutdown also failed: {teardown}");
            }
            return Err(error);
        }
    }

    assert!(!socket.exists(), "a killed server removes its socket too");
    assert_eq!(
        std::fs::read_dir(state.join("snap"))?.count(),
        0,
        "every successful run's snapshot is reclaimed"
    );

    // Still in scope, and still holding nothing: two whole daemons have come and gone underneath
    // these values.
    drop((retained, writer, other));

    println!("rmux_api: staging, policy, and durable reopen verified.");
    Ok(())
}
