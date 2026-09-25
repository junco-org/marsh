#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! The rmux host as an application drives it: one seed, one daemon, one facade, and the five
//! boundaries a caller must be able to tell apart.
//!
//! These tests exercise [`marsh::rmux::RmuxFrontend`] and the [`ShellIo`] it hands out — the
//! supported surface — rather than the multiplexer underneath it. What they pin is not that the
//! plumbing works but that the *distinctions* survive it: a process status is not an approval, a
//! stream
//! ending is not a command finishing, a command finishing is not a job closing, and a handle that
//! outlived its job cannot reach the job that took its name.
//!
//! Every fixture drives a fake btrfs ([`marsh_btrfs::fake::CopyTree`]), the same one the rest of
//! the suite uses, so this runs anywhere. Every test is `#[serial]` because builtin
//! instrumentation is process-global and because the shells a test opens take their seeds' leases
//! — a host on its own leases nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brush_core::env::ShellEnvironment;
use marsh::policy::{Action, Resource};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, IoError, OutputLimit, OverflowPolicy, RmuxFrontend, ShellIo,
};
use marsh::shellmux::{
    CommandOptions, JobIo, MuxError, OutputChannel, ShellId, SpawnOptions, TerminalGeometry,
};
use marsh::{MarshError, Outcome};
use marsh_btrfs::fake::CopyTree;
use rmux_server::DaemonConfig;
use serial_test::serial;
use tempfile::TempDir;

/// How long a wait may take before a test declares the claim it is waiting for unmet.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Two sibling seeds, the fake btrfs both are registered with, and a bound host over them.
///
/// The second seed is registered but never opened by the fixture itself: a host leases nothing
/// until a shell asks for a directory, so the tests that are about leases open their own shells
/// and can prove one host serves both trees.
struct Host {
    /// The running daemon.
    rmux: Option<RmuxFrontend>,
    /// The facade every test drives.
    io: ShellIo,
    /// The first seed's root, and the host's default directory.
    seed: PathBuf,
    /// A second registered seed beside the first.
    other: PathBuf,
    /// The filesystem both seeds live on, so a competing host sees the same trees.
    fs: Arc<CopyTree>,
    /// Kept alive: dropping it deletes the trees the host is publishing into.
    _scratch: TempDir,
}

impl Host {
    /// Builds a seed with `files` in it, a registered sibling seed beside it, and binds a host
    /// whose default directory is the first.
    async fn new(files: &[(&str, &str)]) -> Self {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let seed = scratch.path().join("seed");
        std::fs::create_dir_all(&seed).expect("create the seed root");
        for (path, contents) in files {
            let path = seed.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create a seed directory");
            }
            std::fs::write(path, contents).expect("seed a file");
        }
        // A sibling rather than a child: a nested tree would be reachable by walking up out of
        // the first seed, and the two would not be independent publication roots.
        let other = scratch.path().join("other");
        std::fs::create_dir_all(&other).expect("create the second seed root");

        let fs = Arc::new(CopyTree::new());
        fs.register(&seed);
        fs.register(&other);

        // A socket under the scratch root, so two tests never contend for one path.
        let socket = scratch.path().join("rmux.sock");
        let rmux = RmuxFrontend::open_with(
            DaemonConfig::new(socket),
            &seed,
            ShellEnvironment::new(),
            TerminalGeometry { rows: 24, cols: 80 },
            Arc::clone(&fs) as Arc<dyn marsh_btrfs::Subvolumes>,
        )
        .await
        .expect("open an rmux frontend");

        let io = rmux.io();
        Self {
            rmux: Some(rmux),
            io,
            seed,
            other,
            fs,
            _scratch: scratch,
        }
    }

    /// A path inside the seed.
    fn seed(&self, path: &str) -> PathBuf {
        self.seed.join(path)
    }

    /// Stops the daemon and waits for every snapshot to be reclaimed.
    async fn shutdown(mut self) {
        if let Some(rmux) = self.rmux.take() {
            rmux.shutdown().await.expect("shut the host down");
        }
    }
}

/// Runs one workload to completion and returns everything it produced.
async fn run(io: &ShellIo, cmd: &str) -> marsh::rmux::CapturedOutput {
    let execution = io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell(cmd.to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");
    tokio::time::timeout(TIMEOUT, execution.collect(CollectOptions::default()))
        .await
        .expect("the workload finishes")
        .expect("the workload collects")
}

/// Opens a job whose shell starts in `initial_dir`, publishes `line` through it, and returns it.
///
/// The tests about leases need a seed that is genuinely *held*, and constructing a host no longer
/// holds anything: the lease, the recovered log and the validator all arrive with the first shell
/// that names a directory on that seed. Publishing a line as well is what puts a committed event
/// into the seed's history, so a frozen copy of it afterwards is a copy of something.
async fn publish_in(
    io: &ShellIo,
    initial_dir: &Path,
    id: &str,
    line: &str,
) -> marsh::rmux::ShellHandle {
    let job = io
        .open_shell(
            initial_dir,
            Some(ShellId::from(id)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a job on the named directory's seed");
    let completion =
        tokio::time::timeout(TIMEOUT, job.run_command(line, CommandOptions::default()))
            .await
            .expect("the line finishes")
            .expect("the verdict resolves");
    assert!(
        completion.is_published(),
        "the line publishes into its own seed: {:?}",
        completion.outcome
    );
    job
}

/// Whether `error` is the btrfs lease refusal for `seed`, carried whole through the facade.
///
/// Matched structurally rather than by message: "another owner holds this seed" has to stay
/// distinguishable from "that directory is not a subvolume" and from a generic I/O failure.
fn is_session_busy(error: &IoError, seed: &Path) -> bool {
    matches!(
        error,
        IoError::Mux(mux) if matches!(
            &**mux,
            MuxError::Marsh(MarshError::Btrfs(marsh_btrfs::Error::SessionBusy(held)))
                if held == seed
        )
    )
}

/// The canonical key a seed's metadata and history are filed under.
fn canonical(seed: &Path) -> PathBuf {
    seed.canonicalize().expect("the seed exists")
}

/// A workload's output is *data*: stdout and stderr are independent streams, preserved byte for
/// byte, with no line discipline between the program and the reader.
///
/// This is the whole reason a helper does not get a pseudoterminal. A pty would merge the two,
/// rewrite every `\n` into `\r\n`, and leave a caller unable to tell a diagnostic from a result.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_workloads_two_streams_stay_separate_and_byte_exact() {
    let host = Host::new(&[]).await;

    let captured = run(&host.io, "printf 'out-no-newline'; printf 'err\\n' >&2").await;

    assert_eq!(
        captured.stdout, b"out-no-newline",
        "a final line with no newline survives, and no carriage return was inserted"
    );
    assert_eq!(captured.stderr, b"err\n");
    assert!(!captured.truncated);
    assert_eq!(captured.completion.exit_code, Some(0));

    host.shutdown().await;
}

/// The distinction the whole engine exists for: exiting zero is not being published.
///
/// A line that writes into the seed is approved and its file is there afterwards; the exit code
/// alone never proves that, and a caller that treated it as proof would be reporting success for
/// work that was thrown away.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_exit_code_and_an_approval_are_different_answers() {
    let host = Host::new(&[]).await;

    let captured = run(&host.io, "printf hello > greeting").await;

    assert_eq!(captured.completion.exit_code, Some(0));
    assert!(
        captured.completion.is_published(),
        "a write that was granted reaches the seed: {:?}",
        captured.completion.outcome
    );
    assert_eq!(
        std::fs::read_to_string(host.seed("greeting")).expect("the published file"),
        "hello",
        "the bytes in the seed are the bytes the command wrote"
    );

    // And the negative: a command that exits zero having staged nothing publishes nothing.
    let nothing = run(&host.io, "true").await;
    assert_eq!(nothing.completion.exit_code, Some(0));
    assert!(
        matches!(
            nothing.completion.outcome.as_ref(),
            Ok(Outcome::Published { .. })
        ),
        "an empty boundary is still a boundary: {:?}",
        nothing.completion.outcome
    );

    host.shutdown().await;
}

/// A failing program is a *result*, not an infrastructure error.
///
/// `execute` succeeds, the collection succeeds, and the non-zero status is in the completion. A
/// facade that turned this into an `Err` would make a caller unable to distinguish "the program
/// said no" from "the daemon could not run it".
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_failing_program_is_a_completion_not_an_error() {
    let host = Host::new(&[]).await;

    let captured = run(&host.io, "exit 3").await;

    assert_eq!(captured.completion.exit_code, Some(3));
    assert!(captured.stdout.is_empty());

    host.shutdown().await;
}

/// A pipe job's standard input really ends: closing the write end is an end-of-file the program
/// observes, not a keystroke.
///
/// A pseudoterminal has no half-close, which is why `execute` uses pipes. A program blocked on
/// `read` would otherwise never return.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn closing_a_pipe_jobs_input_is_a_real_end_of_file() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell("cat".to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");

    execution
        .input()
        .write_all(b"piped\n")
        .await
        .expect("the input accepts the write");

    // `collect` closes stdin before draining, which is the only reason `cat` ever exits.
    let captured = tokio::time::timeout(TIMEOUT, execution.collect(CollectOptions::default()))
        .await
        .expect("cat sees end of file and exits")
        .expect("the workload collects");

    assert_eq!(captured.stdout, b"piped\n");
    assert_eq!(captured.completion.exit_code, Some(0));

    host.shutdown().await;
}

/// An argv workload survives quoting: an empty argument, an embedded space and a glob character
/// all reach the program exactly as given.
///
/// Naive joining loses every one of these, and the failure is silent — the program simply
/// receives different arguments than the caller passed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_argv_workloads_arguments_are_not_reinterpreted() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Argv(vec![
                "printf".to_string(),
                "[%s]".to_string(),
                String::new(),
                "two words".to_string(),
                "*".to_string(),
            ]),
            environment: None,
        })
        .await
        .expect("admit the workload");
    let captured = tokio::time::timeout(TIMEOUT, execution.collect(CollectOptions::default()))
        .await
        .expect("the workload finishes")
        .expect("the workload collects");

    assert_eq!(
        String::from_utf8_lossy(&captured.stdout),
        "[][two words][*]",
        "an empty argument is still an argument, and a glob was never expanded"
    );

    host.shutdown().await;
}

/// A bounded collection that overflows under [`OverflowPolicy::Error`] reports the limit rather
/// than returning a silent prefix.
///
/// A caller that asked for complete output and got a truncated buffer with no indication would
/// act on data that is not what the program produced.
///
/// The allowance is *shared*: three bytes on standard output and three on standard error each fit
/// under a four-byte limit on their own, and only the combined allowance the two streams draw on
/// together makes this an overflow at all.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_bounded_collection_refuses_to_return_a_silent_prefix() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell("printf abc; printf XYZ >&2".to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");

    let outcome = tokio::time::timeout(
        TIMEOUT,
        execution.collect(CollectOptions {
            limit: OutputLimit::Bytes(4),
            overflow: OverflowPolicy::Error,
        }),
    )
    .await
    .expect("the collection settles");

    assert!(
        matches!(outcome, Err(IoError::OutputLimit { limit: 4 })),
        "two streams that each fit still overflow the allowance they share"
    );

    host.shutdown().await;
}

/// The same overflow under [`OverflowPolicy::Truncate`] keeps the prefix and *says* it is one.
///
/// Exactly four bytes are retained across *both* streams, and each stream keeps a prefix of its
/// own source rather than of the other's. Which stream wins the fourth byte is a race between two
/// independent pipes and is deliberately not asserted; that the total is exactly the allowance,
/// and that the verdict is still awaited and still approved, is.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_truncating_collection_marks_what_it_kept() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell("printf abc; printf XYZ >&2".to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");

    let captured = tokio::time::timeout(
        TIMEOUT,
        execution.collect(CollectOptions {
            limit: OutputLimit::Bytes(4),
            overflow: OverflowPolicy::Truncate,
        }),
    )
    .await
    .expect("the collection settles")
    .expect("a truncating collection still succeeds");

    assert!(
        captured.truncated,
        "the caller is told the bytes are a prefix"
    );
    assert_eq!(
        captured.stdout.len() + captured.stderr.len(),
        4,
        "one allowance, drawn on by both streams together"
    );
    assert!(
        b"abc".starts_with(&captured.stdout),
        "standard output kept a prefix of its own source: {:?}",
        String::from_utf8_lossy(&captured.stdout)
    );
    assert!(
        b"XYZ".starts_with(&captured.stderr),
        "standard error kept a prefix of its own source: {:?}",
        String::from_utf8_lossy(&captured.stderr)
    );
    assert_eq!(
        captured.completion.exit_code,
        Some(0),
        "the verdict is still awaited and still real"
    );
    assert!(
        captured.completion.is_published(),
        "retaining a prefix does not change what the gate decided"
    );

    host.shutdown().await;
}

/// A handle that outlived its job cannot reach the job that took its name.
///
/// A job name is a capability principal and may be reused. A retained handle names one
/// *generation*, so acting through a stale one fails rather than silently addressing a different
/// sandbox — which would misattribute both the work and the grant.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_stale_handle_cannot_reach_its_own_replacement() {
    let host = Host::new(&[]).await;

    let first = host
        .io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("worker")),
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open the first job");
    let first_uid = first.sandbox().uid.clone();

    host.io.stop(&first, true).await.expect("force the job");
    tokio::time::timeout(TIMEOUT, first.wait_closed())
        .await
        .expect("the job closes")
        .expect("the closure resolves");

    let second = host
        .io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("worker")),
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("reuse the name");
    assert_ne!(
        second.sandbox().uid,
        first_uid,
        "a name is reusable; a snapshot id never is"
    );

    let refused = host.io.write_input(&first, b"x").await;
    assert!(
        matches!(
            &refused,
            Err(IoError::Mux(error))
                if matches!(&**error, MuxError::StaleJob(_) | MuxError::JobClosing(_))
        ),
        "the stale handle must not reach the replacement: {refused:?}"
    );

    host.io.stop(&second, true).await.expect("clean up");
    host.shutdown().await;
}

/// A handle from one host is refused by another.
///
/// Two hosts means two seeds, two leases and two principals' histories. Acting on a foreign handle
/// would publish one host's work through the other's gate.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_foreign_handle_is_refused() {
    let first = Host::new(&[]).await;
    let second = Host::new(&[]).await;

    let job = first
        .io
        .open_shell(
            Path::new(""),
            None,
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open a job on the first host");

    let refused = second.io.write_input(&job, b"x").await;
    assert!(
        matches!(refused, Err(IoError::WrongHost)),
        "a handle names a host as well as a job: {refused:?}"
    );

    first.io.stop(&job, true).await.expect("clean up");
    first.shutdown().await;
    second.shutdown().await;
}

/// A subscription is atomic with its snapshot: a change concurrent with `observe` is in one or the
/// other, never neither.
///
/// An observer that read the state and subscribed separately could miss everything that happened
/// between the two calls, and would have no way to know it had.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_observation_never_falls_between_its_snapshot_and_its_stream() {
    let host = Host::new(&[]).await;

    let observation = host.io.observe();
    let before = observation.snapshot.state.jobs.len();

    let job = host
        .io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("watched")),
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open a job after subscribing");

    // Everything from `next_event_sequence` onwards is on the stream. The job is not in the
    // snapshot, so it must be in the events.
    let mut events = observation.events;
    let mut saw_opened = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(envelope))) =
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        assert!(
            envelope.sequence >= observation.snapshot.next_event_sequence,
            "the stream resumes exactly where the snapshot stopped"
        );
        if matches!(envelope.event, marsh::rmux::IoEvent::Opened { .. }) {
            saw_opened = true;
            break;
        }
    }
    assert!(
        saw_opened,
        "a job opened after the snapshot must appear on the stream"
    );
    assert_eq!(
        before,
        observation.snapshot.state.jobs.len(),
        "the snapshot itself never changes after it is taken"
    );

    host.io.stop(&job, true).await.expect("clean up");
    host.shutdown().await;
}

/// Every boundary is distinct, and they resolve in the documented order: the stream ends, then the
/// command's verdict lands, then the job closes.
///
/// Collapsing any two of these loses a real difference. A caller that treated end-of-output as
/// completion would read a verdict that does not exist yet; one that treated completion as closure
/// would act on a snapshot that is still being reclaimed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_four_boundaries_are_separate_and_ordered() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell("printf done".to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");

    let parts = execution.into_parts();
    let shell = parts.shell.clone();
    let command = parts.command.clone();
    let mut stdout = parts.stdout;
    parts.stdin.close().await.expect("end the input");

    // 1. the stream ends.
    let mut bytes = Vec::new();
    while let Ok(Some(item)) = stdout.recv().await {
        if let rmux_core::events::OutputCursorItem::Event(event) = item {
            bytes.extend_from_slice(event.bytes());
        }
    }
    assert_eq!(bytes, b"done");

    // 2. the verdict lands.
    let completion = tokio::time::timeout(TIMEOUT, command.wait())
        .await
        .expect("the command finishes")
        .expect("the verdict resolves");
    assert_eq!(completion.exit_code, Some(0));

    // 3. the job closes, which is a later boundary than the verdict.
    let end = tokio::time::timeout(TIMEOUT, shell.wait_closed())
        .await
        .expect("the job closes")
        .expect("the closure resolves");
    assert_eq!(end.shell.uid, completion.shell.uid);
    assert_eq!(
        end.completion.as_ref().map(|last| last.id),
        Some(completion.id),
        "the job's end carries the verdict of the last command that ran in it"
    );

    host.shutdown().await;
}

/// A terminal job is one merged stream; a pipe job is two independent ones. The shape is fixed
/// when the job is admitted and is never converted.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_jobs_stream_shape_is_fixed_when_it_is_admitted() {
    let host = Host::new(&[]).await;

    let terminal = host
        .io
        .open_shell(Path::new(""), None, SpawnOptions::default())
        .await
        .expect("open a terminal job");
    assert_eq!(terminal.output_channels(), &[OutputChannel::Terminal]);
    assert!(
        matches!(
            &host.io.close_input(&terminal).await,
            Err(IoError::Mux(error)) if matches!(&**error, MuxError::NotPiped(_))
        ),
        "a pseudoterminal has no half-close, and Ctrl-D is a keystroke rather than an end of file"
    );

    let piped = host
        .io
        .open_shell(
            Path::new(""),
            None,
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open a pipe job");
    assert_eq!(
        piped.output_channels(),
        &[OutputChannel::Stdout, OutputChannel::Stderr]
    );
    assert!(
        matches!(
            &host.io.resize(&piped, TerminalGeometry { rows: 40, cols: 100 }).await,
            Err(IoError::Mux(error)) if matches!(&**error, MuxError::NotTerminal(_))
        ),
        "a pipe job has no terminal, and inventing one would be a lie a caller could act on"
    );

    host.io.stop(&terminal, true).await.expect("clean up");
    host.io.stop(&piped, true).await.expect("clean up");
    host.shutdown().await;
}

/// A scheduled line's receipt exists before that line can have produced anything.
///
/// Creating a shell no longer takes a command, so "the spawn hands back a receipt" is not a
/// question any more. The boundary that survives it is the one every streaming caller depends
/// on: a run hands its receipt over at *admission*, before the interpreter can emit a byte or
/// reach a verdict. That ordering is the only reason feeding a program's standard input — and so
/// running a program that reads to end of file at all — is possible.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_scheduled_command_is_accepted_before_it_produces_anything() {
    let host = Host::new(&[]).await;

    let job = host
        .io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("scheduled")),
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open a job with nothing running in it");

    let idle = host.io.job(job.id()).expect("the new job is visible");
    assert!(
        idle.running.is_none() && !idle.closing,
        "creating a shell runs no command and closes nothing: {idle:?}"
    );

    // Subscribed before the line is scheduled, so the order of its observations is read off the
    // stream rather than reconstructed from it afterwards.
    let observation = host.io.observe();

    // `cat` cannot write a byte or exit until its input ends, and its input is not ended below
    // until the receipt is already in hand: this cannot pass by the run simply being over.
    let (accepted, receipt) = tokio::sync::oneshot::channel();
    let scheduled = job.clone();
    let running = tokio::spawn(async move {
        scheduled
            .run_command(
                "cat",
                CommandOptions {
                    on_accept: Some(accepted),
                    ..CommandOptions::default()
                },
            )
            .await
    });

    let command = tokio::time::timeout(TIMEOUT, receipt)
        .await
        .expect("the line is admitted")
        .expect("an admitted line hands over its receipt");
    assert_eq!(
        command.text(),
        "cat",
        "the receipt carries the line as submitted"
    );
    assert!(
        !command.is_finished(),
        "a program still reading its input has reached no verdict"
    );
    assert_eq!(
        host.io
            .job(job.id())
            .and_then(|view| view.running)
            .map(|flight| flight.id),
        Some(command.id()),
        "the command in flight is the one the receipt names"
    );

    // Only now can it produce anything at all.
    host.io
        .write_input(&job, b"produced\n")
        .await
        .expect("a running command's input is still writable");
    host.io.close_input(&job).await.expect("end the input");

    let completion = tokio::time::timeout(TIMEOUT, running)
        .await
        .expect("the run settles")
        .expect("the runner task finished")
        .expect("the line publishes");
    assert_eq!(completion.id, command.id(), "one line, one identity");
    assert_eq!(completion.exit_code, Some(0));

    // The stream says the same thing in order: neither a byte the command produced nor its
    // verdict comes before its acceptance.
    let mut events = observation.events;
    let mut accepted_first = false;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(envelope))) =
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        match &envelope.event {
            marsh::rmux::IoEvent::CommandAccepted { command: handle }
                if handle.id() == command.id() =>
            {
                accepted_first = true;
                break;
            }
            marsh::rmux::IoEvent::Output { .. } | marsh::rmux::IoEvent::Finished { .. } => {
                panic!(
                    "a command's bytes and its verdict both come after its acceptance: {:?}",
                    envelope.event
                )
            }
            _ => {}
        }
    }
    assert!(
        accepted_first,
        "an admitted line announces itself on the stream"
    );

    host.shutdown().await;
}

/// After shutdown the facade is still readable, still refuses work, and still knows **every** seed
/// it served.
///
/// A caller holding a clone must not get a panic or a lie: read-only metadata is answered from the
/// frozen copy, live state is empty, and every admission fails with [`IoError::Closed`]. One host
/// serves as many seeds as its shells name, so freezing one of them — the first, the last, or the
/// daemon's default directory — would silently lose the others' committed histories, which is the
/// only record a later caller has of what was granted.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_closed_facade_answers_rather_than_pretending() {
    let host = Host::new(&[]).await;
    let io = host.io.clone();
    let first = canonical(&host.seed);
    let second = canonical(&host.other);

    // Nothing is leased and nothing is knowable until a shell asks for a directory.
    assert!(
        io.seeds().is_empty(),
        "a host that has opened no shell has opened no seed"
    );
    assert_eq!(io.history(&first), None);

    let a = publish_in(&io, &host.seed, "a", "printf a > from-a").await;
    let b = publish_in(&io, &host.other, "b", "printf b > from-b").await;
    assert_eq!(a.sandbox().seed, first, "the first job publishes into A");
    assert_eq!(b.sandbox().seed, second, "the second job publishes into B");

    // `seeds()` enumerates in canonical-path order, which is what a caller can rely on rather
    // than the order the shells happened to open in.
    let live: Vec<PathBuf> = io.seeds().iter().map(|info| info.seed.clone()).collect();
    let mut both = vec![first.clone(), second.clone()];
    both.sort();
    assert_eq!(live, both, "one host, both seeds");

    host.shutdown().await;

    let frozen: Vec<PathBuf> = io.seeds().iter().map(|info| info.seed.clone()).collect();
    assert_eq!(
        frozen, live,
        "which seeds were served is still a legitimate question once the leases are gone"
    );
    assert!(
        io.seeds().iter().all(|info| !info.recovery_required
            && info.snapshot_parent.starts_with(
                info.seed
                    .parent()
                    .expect("a seed has a parent to put its state beside")
            )),
        "each frozen record keeps its own state parent: {:?}",
        io.seeds()
    );

    // The committed histories survive whole, and each seed keeps only its own principal's events.
    let edit_of = |seed: &Path, principal: &str, file: &str| {
        io.history(seed)
            .unwrap_or_else(|| panic!("{} was opened", seed.display()))
            .iter()
            .any(|event| {
                event.action == Action::Edit
                    && event.principal.as_str() == principal
                    && event.resource == Resource::from(vec![file])
            })
    };
    assert!(edit_of(&first, "a", "from-a"), "A's grant is frozen");
    assert!(edit_of(&second, "b", "from-b"), "B's grant is frozen");
    assert!(!edit_of(&first, "b", "from-b"), "B's grant is not in A");
    assert!(!edit_of(&second, "a", "from-a"), "A's grant is not in B");
    assert_eq!(
        io.history(Path::new("/nowhere")),
        None,
        "a seed this host never opened is still unknown afterwards"
    );

    assert!(io.jobs().is_empty());
    assert!(matches!(
        io.open_shell(Path::new(""), None, SpawnOptions::default())
            .await,
        Err(IoError::Closed)
    ));
    assert!(matches!(
        io.shell(&ShellId::from("anything")),
        Err(IoError::Closed)
    ));
}

/// One job submitting several lines keeps one principal and one snapshot across all of them.
///
/// A job is a sandbox, not a command: reusing it is how a caller accumulates approved state under
/// a single principal instead of inventing a new one per line.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_job_outlives_the_commands_run_in_it() {
    let host = Host::new(&[]).await;

    let job = host
        .io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("acc")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a terminal job");
    let uid = job.sandbox().uid.clone();

    for line in ["printf one >> log", "printf two >> log"] {
        let completion =
            tokio::time::timeout(TIMEOUT, job.run_command(line, CommandOptions::default()))
                .await
                .expect("the line finishes")
                .expect("the verdict resolves");
        assert!(
            completion.is_published(),
            "each line is published on its own: {:?}",
            completion.outcome
        );
        assert_eq!(completion.shell.uid, uid, "the sandbox did not change");
    }

    assert_eq!(
        std::fs::read_to_string(host.seed("log")).expect("the published file"),
        "onetwo",
        "both lines published into the same seed, in order"
    );

    host.io.stop(&job, false).await.expect("clean up");
    host.shutdown().await;
}

/// Opens a host over `first` as its default directory and publishes a line on **both** seeds.
///
/// The proof that a stopped daemon released what it held: a lease is exclusive, so a shell that
/// opens and publishes is a shell nobody else is still holding that seed against. Doing it for
/// both seeds is what makes a per-seed leak visible — releasing only the default directory's seed
/// would pass a single-seed check and strand every other one for the life of the process.
async fn reopen_and_publish_on_both(
    fs: Arc<dyn marsh_btrfs::Subvolumes>,
    socket: PathBuf,
    first: &Path,
    second: &Path,
) {
    let reopened = RmuxFrontend::open_with(
        DaemonConfig::new(socket),
        first,
        ShellEnvironment::new(),
        TerminalGeometry { rows: 24, cols: 80 },
        fs,
    )
    .await
    .expect("a host binds its socket whatever the seeds are doing");
    let io = reopened.io();

    publish_in(&io, first, "reopened-a", "printf again-a > reopened").await;
    publish_in(&io, second, "reopened-b", "printf again-b > reopened").await;
    assert_eq!(
        std::fs::read(first.join("reopened")).expect("the first seed's file"),
        b"again-a",
        "the first seed's lease was really released"
    );
    assert_eq!(
        std::fs::read(second.join("reopened")).expect("the second seed's file"),
        b"again-b",
        "the second seed's lease was really released"
    );

    reopened
        .shutdown()
        .await
        .expect("shut the reopened frontend down");
}

/// A `kill-server` releases every seed the host opened, even while an application still holds a
/// facade clone.
///
/// This is the leak that a "closed" handle would otherwise hide: the listener stops, the socket
/// goes away, the daemon looks gone — and a retained clone quietly keeps the multiplexer, and with
/// it each opened seed's *exclusive* lease, alive forever. The next process to open one of those
/// seeds would then fail for a daemon that is not running.
///
/// The proof is the lease itself, and it is now a *per-seed* proof. Binding a host leases nothing,
/// so a competing [`RmuxFrontend`] over the same trees constructs perfectly well — what it cannot
/// do is open a shell on a seed this host's shells already hold. Both seeds must be reachable
/// again once the first host is really gone.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn stopping_the_daemon_releases_the_seed_even_with_a_clone_outstanding() {
    let host = Host::new(&[]).await;
    let seed = host.seed.clone();
    let other = host.other.clone();
    let first = canonical(&seed);
    let second = canonical(&other);
    let scratch_root = seed
        .parent()
        .expect("the seed sits in a scratch root")
        .to_path_buf();
    let fs = Arc::clone(&host.fs) as Arc<dyn marsh_btrfs::Subvolumes>;
    let retained = host.io.clone();

    // Nothing is held until a shell asks. These two are what put both seeds under lease.
    publish_in(&host.io, &seed, "a", "printf a > held-a").await;
    publish_in(&host.io, &other, "b", "printf b > held-b").await;

    // A competing host over the same trees, on a socket of its own: a refusal on the bound socket
    // would prove nothing about a seed. Construction succeeds — a listener's lifetime no longer
    // depends on any seed's — and the refusal lands on the shell that actually wants the lease.
    let competing = RmuxFrontend::open_with(
        DaemonConfig::new(scratch_root.join("competing.sock")),
        &seed,
        ShellEnvironment::new(),
        TerminalGeometry { rows: 24, cols: 80 },
        Arc::clone(&fs),
    )
    .await
    .expect("a second host binds its own socket while another host holds the seeds");
    for (directory, canonical_seed, name) in [
        (&seed, &first, "intruder-a"),
        (&other, &second, "intruder-b"),
    ] {
        let refusal = competing
            .io()
            .open_shell(
                directory,
                Some(ShellId::from(name)),
                SpawnOptions::default(),
            )
            .await
            .expect_err("the running host holds that seed's exclusive lease");
        assert!(
            is_session_busy(&refusal, canonical_seed),
            "{} is busy, not broken: {refusal:?}",
            canonical_seed.display()
        );
    }
    assert!(
        competing.io().jobs().is_empty() && competing.io().seeds().is_empty(),
        "a refused spawn admits no job and opens no seed"
    );
    competing
        .shutdown()
        .await
        .expect("shut the competing host down");

    // The scratch tree is taken out of the fixture before the host is torn down. `Host::shutdown`
    // consumes the fixture, which would otherwise drop the `TempDir` and delete the very
    // directories this test is about to reopen — the reopen would then fail for a missing seed and
    // look exactly like a lease that was never released.
    let Host {
        rmux,
        io,
        _scratch: scratch,
        ..
    } = host;
    drop(io);
    rmux.expect("the host is running")
        .shutdown()
        .await
        .expect("shut the host down");

    // The clone is still alive and still answers — and no longer holds anything.
    let frozen: Vec<PathBuf> = retained
        .seeds()
        .iter()
        .map(|info| info.seed.clone())
        .collect();
    let mut both = vec![first.clone(), second.clone()];
    both.sort();
    assert_eq!(
        frozen, both,
        "a closed handle still answers which seeds were served"
    );
    assert!(retained.history(&first).is_some() && retained.history(&second).is_some());
    assert!(retained.jobs().is_empty());
    assert!(matches!(
        retained
            .open_shell(Path::new(""), None, SpawnOptions::default())
            .await,
        Err(IoError::Closed)
    ));

    reopen_and_publish_on_both(fs, scratch_root.join("reopened.sock"), &seed, &other).await;
    // Still in scope for the whole run, and still harmless: a retained handle never pinned a
    // seed, before or after the reopen.
    drop(retained);
    drop(scratch);
}

/// The same release happens for every opened seed when the daemon is stopped from outside rather
/// than by this host.
///
/// `wait` returns because something else ended the server — here a real `kill-server` request over
/// this daemon's own socket, which is the path a signal and an idle exit also take. None of those
/// goes through [`RmuxFrontend::shutdown`], so if the core were only released there, every one of
/// them would leak every lease the host's shells had taken.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_external_stop_releases_the_seed_too() {
    let host = Host::new(&[]).await;
    let seed = host.seed.clone();
    let other = host.other.clone();
    let first = canonical(&seed);
    let second = canonical(&other);
    let scratch_root = seed
        .parent()
        .expect("the seed sits in a scratch root")
        .to_path_buf();
    let fs = Arc::clone(&host.fs) as Arc<dyn marsh_btrfs::Subvolumes>;
    let retained = host.io.clone();

    // Two shells on two seeds: two leases, taken by the shells rather than by the host.
    publish_in(&host.io, &seed, "a", "printf a > held-a").await;
    publish_in(&host.io, &other, "b", "printf b > held-b").await;

    let Host {
        rmux,
        _scratch: scratch,
        ..
    } = host;
    let rmux = rmux.expect("the host is running");
    let socket = rmux.socket_path().to_path_buf();

    // A real client of this daemon, over the socket it actually bound.
    let connection = retained
        .open_protocol()
        .await
        .expect("connect to this daemon's socket");

    // `wait` is the ending an externally stopped daemon takes: nothing in this process asks the
    // owner to stop, so the listener's own exit is the only thing that can end it.
    let waiting = tokio::spawn(async move { rmux.wait().await });

    let response = tokio::time::timeout(
        TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let mut connection = connection;
            connection.roundtrip(&rmux_proto::Request::KillServer(
                rmux_proto::KillServerRequest,
            ))
        }),
    )
    .await
    .expect("the kill-server exchange finishes")
    .expect("the blocking worker finished")
    .expect("the daemon answered the request");
    assert!(matches!(response, rmux_proto::Response::KillServer(_)));

    // Every layer unwrapped: a timeout, a join failure and a daemon error are three different
    // ways for this to be a failure, and none of them may be quietly accepted.
    tokio::time::timeout(TIMEOUT, waiting)
        .await
        .expect("the daemon's listener exits")
        .expect("the waiting task finished")
        .expect("the daemon stopped cleanly");

    assert!(
        !socket.exists(),
        "an externally stopped daemon removes its socket"
    );

    let mut both = vec![first.clone(), second.clone()];
    both.sort();
    let frozen: Vec<PathBuf> = retained
        .seeds()
        .iter()
        .map(|info| info.seed.clone())
        .collect();
    assert_eq!(
        frozen, both,
        "an externally stopped daemon still leaves its clone the record of both seeds"
    );
    assert!(retained.history(&first).is_some() && retained.history(&second).is_some());
    assert!(retained.jobs().is_empty());

    reopen_and_publish_on_both(fs, scratch_root.join("reopened.sock"), &seed, &other).await;
    drop(retained);
    drop(scratch);
}

/// A workload that cannot be composed is refused having allocated nothing.
///
/// The ordering matters more than the error. An empty argv has no program to run, and rejecting it
/// *after* the job was created would leave an idle pipe job holding a real snapshot, with two
/// armed output receivers nobody will ever close — a one-shot job only closes when its command
/// ends, and this one would never get a command.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_uncomposable_workload_leaves_no_job_behind() {
    let host = Host::new(&[]).await;
    let before = host.io.jobs().len();

    let refused = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Argv(Vec::new()),
            environment: None,
        })
        .await;

    assert!(refused.is_err(), "an empty argv names no program");
    assert_eq!(
        host.io.jobs().len(),
        before,
        "a refused workload must not leave a snapshot behind"
    );

    host.shutdown().await;
}

/// Observing a stream is refused for a foreign handle and after teardown.
///
/// Both would otherwise hand back a reader that waits forever: a stream entry registered against a
/// host with no core is one nothing will ever write to or end, and a foreign handle would be
/// reading another host's bytes.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn observing_a_stream_checks_the_host_before_the_channel() {
    let first = Host::new(&[]).await;
    let second = Host::new(&[]).await;

    let job = first
        .io
        .open_shell(
            Path::new(""),
            None,
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        )
        .await
        .expect("open a job on the first host");

    assert!(
        matches!(
            second
                .io
                .output(&job, OutputChannel::Stdout, rmux_sdk::PaneOutputStart::Now),
            Err(IoError::WrongHost)
        ),
        "a handle names a host as well as a job"
    );

    let retained = first.io.clone();
    first.io.stop(&job, true).await.expect("clean up");
    first.shutdown().await;

    assert!(
        matches!(
            retained.output(&job, OutputChannel::Stdout, rmux_sdk::PaneOutputStart::Now),
            Err(IoError::Closed)
        ),
        "a closed host answers rather than handing back a reader that waits forever"
    );

    second.shutdown().await;
}

/// Observing a job that has already closed answers at once instead of waiting forever.
///
/// The host is still live here, so nothing refuses the call — which is exactly what makes this
/// the dangerous case. A stream's retained storage is reclaimed when it ends with no reader
/// attached, and without an ended-tombstone a later observer *recreates* that entry as an empty,
/// not-ended stream: `recv` would then block for the life of the process waiting for bytes that
/// can never arrive. The closed-host test does not cover this, because there the facade refuses
/// before any stream is registered.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn observing_a_closed_job_ends_rather_than_waits() {
    let host = Host::new(&[]).await;

    let execution = host
        .io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell("printf gone".to_string()),
            environment: None,
        })
        .await
        .expect("admit the workload");
    let shell = execution.shell().clone();

    let captured = tokio::time::timeout(TIMEOUT, execution.collect(CollectOptions::default()))
        .await
        .expect("the workload finishes")
        .expect("the workload collects");
    assert_eq!(captured.stdout, b"gone");

    // The job is gone; the host is not.
    tokio::time::timeout(TIMEOUT, shell.wait_closed())
        .await
        .expect("the job closes")
        .expect("the closure resolves");

    let mut stream = host
        .io
        .output(
            &shell,
            OutputChannel::Stdout,
            rmux_sdk::PaneOutputStart::Oldest,
        )
        .expect("observing a closed job is a legitimate question");

    // The bound is the assertion: without the tombstone this never returns.
    let next = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("a closed job's stream must end rather than block")
        .expect("the stream answers");
    assert!(
        next.is_none(),
        "a reclaimed stream reports end of file, not an empty stream that never ends"
    );

    host.shutdown().await;
}
