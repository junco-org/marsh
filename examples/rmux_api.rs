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
//! `marsh::rmux::types`, the standard library or `tokio`, and nothing else at all. And it is a
//! *proof*: it opens a real seed, publishes through the gate, has a second principal refused,
//! restarts the whole system over the same seed, and shows that both the refusal and the earlier
//! release survived the restart.
//!
//! The host leases no seed. Its `initial_dir` argument is only the default directory for requests
//! that name none: a seed is discovered from the directory each *shell* starts in, and that
//! seed — not this process — owns the lease, the recovered log and the committed capability
//! history. Which is why every query below is asked with a canonical seed key taken from a shell
//! that actually opened it: before the first shell, `seeds()` is empty and `history()` answers
//! `None`, and no constructor here can change that.
//!
//! What it observes, in order: a staged write reaches the seed; a second principal's overwrite
//! exits zero and is denied; a `git add` is a runtime Stage the seed's own history records; a
//! pipe execution carries byte-exact separate streams; retained handles cannot keep a shut-down
//! daemon alive; reopening the host and starting a shell on that seed adopts its durable grants
//! before admitting work, so a reused job name inherits nothing; and a `kill-server` over the
//! real wire ends the daemon and reclaims its snapshots.

use std::path::PathBuf;
use std::time::Duration;

use marsh::rmux::types::{
    Action, CommandOptions, DaemonConfig, Outcome, ProcessCommand, RunError, ShellEnvironment,
    ShellId, SpawnOptions, TerminalGeometry, protocol,
};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, IoError, IoPhase, OutputLimit, OverflowPolicy, RmuxFrontend,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Every wait in this program is bounded by the same window: a hang is a failure, not a slow
    // success, and an unbounded await would turn one into the other.
    let limit = Duration::from_secs(30);

    let mut arguments = std::env::args_os().skip(1);
    let (seed, socket) = match (arguments.next(), arguments.next(), arguments.next()) {
        (Some(seed), Some(socket), None) => (
            std::path::PathBuf::from(seed),
            std::path::PathBuf::from(socket),
        ),
        _ => return Err("usage: rmux_api <seed> <socket>".into()),
    };

    // ---- Phase one: a live daemon whose default directory is that seed. ----------------------
    // No validator, no executor and no lease are constructed here: `seed` is only where a request
    // that names no directory of its own starts.
    let frontend = RmuxFrontend::open(
        DaemonConfig::new(socket.clone()),
        &seed,
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;

    // Kept alive across the shutdown below, which is the point of step 7.
    let retained = frontend.io();
    let mut writer = None;
    let mut other = None;

    let phase = async {
        // 1. Nothing is leased yet. Construction discovered no seed, so there is none to enumerate
        //    and none whose history could be asked for.
        assert!(
            frontend.seeds().is_empty(),
            "a host that has opened no shell owns no seed"
        );

        // 2. Two shells, both started in that directory, so both discover the same seed. Opening
        //    one creates it and runs nothing: a shell is a principal with a sandbox, and a line
        //    is a separate submission into it.
        let writer_job = frontend
            .open_shell(
                &seed,
                Some(ShellId::from("writer")),
                SpawnOptions::default(),
            )
            .await?;
        let other_job = frontend
            .open_shell(&seed, Some(ShellId::from("other")), SpawnOptions::default())
            .await?;
        writer = Some(writer_job.clone());
        other = Some(other_job.clone());

        // The canonical seed, as the *shell* resolved it. A relative or symlinked argument names
        // the same subvolume; this is the spelling every path and query below is made against,
        // and it belongs to the job rather than to the host.
        let canonical = writer_job.sandbox().seed.clone();
        assert_eq!(
            other_job.sandbox().seed,
            canonical,
            "two shells in one directory publish into one seed"
        );
        assert!(
            frontend
                .seeds()
                .iter()
                .any(|info| info.seed == canonical && !info.recovery_required),
            "and the host now enumerates exactly the seed they opened"
        );

        // 3. One line, two files. Running is a *completion*: `Ok` is the gate's approval and
        //    nothing less, so the exit code below is an extra fact rather than the verdict.
        let published = tokio::time::timeout(
            limit,
            writer_job.run_command(
                "printf owner > owned; printf staged > released",
                CommandOptions::default(),
            ),
        )
        .await??;
        assert_eq!(published.exit_code, Some(0), "the process said zero");
        assert!(published.is_published(), "and the gate agreed");
        assert_eq!(std::fs::read(canonical.join("owned"))?, b"owner");
        assert_eq!(std::fs::read(canonical.join("released"))?, b"staged");

        // 4. A second principal overwrites the first's file. The process succeeds; the
        //    publication does not, and the seed keeps the original bytes. The refusal is the
        //    *error* here, because a line that changed nothing is not a result a caller can
        //    accidentally read as one — and it still carries the whole completion, so the
        //    program's own zero exit and the policy's explanations are both still there.
        let refused = tokio::time::timeout(
            limit,
            other_job.run_command("printf intruder > owned", CommandOptions::default()),
        )
        .await?;
        let denied = match refused {
            Err(IoError::Run(RunError::Policy(denied))) => denied,
            other => panic!("a zero exit is not an approval: {other:?}"),
        };
        let completion = denied.completion();
        assert_eq!(completion.exit_code, Some(0), "the process still said zero");
        assert!(
            matches!(
                completion.outcome.as_ref(),
                Ok(Outcome::Denied { denials, .. }) if !denials.is_empty()
            ),
            "and the refusal names what it refused"
        );
        assert_eq!(std::fs::read(canonical.join("owned"))?, b"owner");

        // 5. `git add` is a runtime Stage request, not repository bookkeeping: it releases
        //    `released`'s unstaged ownership. `owned` is deliberately left owned, which is what
        //    step 8 restarts into. The grant is recorded against *the seed*, not against this
        //    process: `history` is keyed by the canonical seed the shell discovered, and a host
        //    that never opened it would answer `None`.
        let staged = tokio::time::timeout(
            limit,
            writer_job.run_command("git add -- released", CommandOptions::default()),
        )
        .await??;
        assert_eq!(staged.exit_code, Some(0));
        assert!(staged.is_published());
        let events = frontend
            .history(&canonical)
            .ok_or("the seed these shells opened has no history")?;
        assert!(
            events.iter().any(|event| event.action == Action::Stage),
            "the grant is committed to the history this seed is judged against"
        );
        assert!(
            frontend.history(&canonical.join("no-such-seed")).is_none(),
            "and a seed nothing opened has no history to report"
        );

        // 6. A pipe execution: two real streams, byte-exact, never merged.
        let execution = frontend
            .execute(ExecutionSpec {
                initial_dir: PathBuf::new(),
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
        assert_eq!(
            captured.stderr, b"stderr",
            "and stderr is never merged into it"
        );
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
            retained
                .open_shell(&seed, None, SpawnOptions::default())
                .await,
            Err(IoError::Closed)
        ),
        "a retained handle refuses work rather than reaching a released engine"
    );

    // 8. Reopen the same socket with the same default directory. The new host knows nothing.
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
        ShellEnvironment::default(),
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await?;

    let phase = async {
        // A reopened host has discovered nothing: the seed's durable history is read when a shell
        // opens that seed, not when a listener binds a socket. Claiming otherwise here would be
        // claiming a lease this process does not hold.
        assert!(reopened.seeds().is_empty());
        assert!(
            reopened.history(&canonical).is_none(),
            "no shell has opened this seed yet, so there is nothing to report about it"
        );

        // The same *name*, a different snapshot uid. Rights belong to the uid, so this job
        // inherits nothing from the `writer` that earned them.
        let job = reopened
            .open_shell(
                &seed,
                Some(ShellId::from("writer")),
                SpawnOptions::default(),
            )
            .await?;
        assert_eq!(job.sandbox().seed, canonical, "the same seed, rediscovered");

        // Read before a single line is admitted: opening the seed replayed its log and installed
        // its durable history, so this is the seed's property and not this process's.
        let events = reopened
            .history(&canonical)
            .ok_or("opening a shell on the seed did not open the seed")?;
        assert!(
            events.iter().any(|event| event.action == Action::Stage),
            "reopening adopts the grants the previous run published"
        );

        let refused = tokio::time::timeout(
            limit,
            job.run_command("printf intruder > owned", CommandOptions::default()),
        )
        .await?;
        let denied = match refused {
            Err(IoError::Run(RunError::Policy(denied))) => denied,
            other => panic!(
                "an unstaged path stays owned by a principal that no longer exists: {other:?}"
            ),
        };
        let completion = denied.completion();
        assert_eq!(completion.exit_code, Some(0));
        assert!(
            matches!(
                completion.outcome.as_ref(),
                Ok(Outcome::Denied { denials, .. }) if !denials.is_empty()
            ),
            "and the refusal names the unstaged path it refused"
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
        let republished = tokio::time::timeout(
            limit,
            job.run_command("printf after-reopen > released", CommandOptions::default()),
        )
        .await??;
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
