#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! The `rmux` executable's formerly-local workloads, run against a real daemon.
//!
//! `rmux -c <command>` used to `exec` a host `$SHELL` in the client process: no snapshot, no
//! policy gate, no relationship to the multiplexer at all. It is now a one-shot pane on the
//! daemon the invocation resolves, in a session the invocation owns and destroys. These tests
//! drive the built `rmux` binary — not an in-process helper — against a live [`RmuxFrontend`]
//! host, so what they exercise is the same code path a user gets.
//!
//! What they pin is the three things the cutover can silently lose:
//!
//! 1. the workload actually reaches the seed through the gate, and its session does not outlive
//!    the invocation;
//! 2. a *denied* publication fails the command even though the program exited zero — the whole
//!    reason this stopped being a local `exec`;
//! 3. stdin is relayed, EOF is delivered as terminal EOF, and output is not truncated by the
//!    exit arriving before the last bytes do.
//!
//! The fixture is [`tests/rmux.rs`](./rmux.rs)'s: a fake btrfs ([`marsh_btrfs::fake::CopyTree`])
//! under a temporary directory, an isolated validator, and one host per test. Every test is
//! `#[serial]` because builtin instrumentation is process-global and each host takes a seed
//! lease.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brush_core::env::ShellEnvironment;
use marsh::rmux::{CollectOptions, ExecutionSpec, RmuxFrontend, ShellIo};
use marsh::shellmux::TerminalGeometry;
use marsh::PolicyValidator;
use marsh_btrfs::fake::CopyTree;
use rmux_server::DaemonConfig;
use serial_test::serial;
use tempfile::TempDir;

/// How long a managed CLI invocation may take before a test declares it hung.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A seed, a bound host, and the socket the CLI talks to it over.
struct CliHost {
    /// The running daemon.
    rmux: Option<RmuxFrontend>,
    /// The facade, for seeding state the CLI then has to interact with.
    io: ShellIo,
    /// The seed's root, so a test can read back what was published.
    seed: PathBuf,
    /// The daemon socket, passed to every CLI invocation as `-S`.
    socket: PathBuf,
    /// Kept alive: dropping it deletes the tree the host publishes into.
    _scratch: TempDir,
}

impl CliHost {
    /// Binds a host over a fresh seed.
    async fn new() -> Self {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let seed = scratch.path().join("seed");
        std::fs::create_dir_all(&seed).expect("create the seed root");

        let fs = Arc::new(CopyTree::new());
        fs.register(&seed);

        // Outside the seed, so the socket is never part of what a workload can publish.
        let socket = scratch.path().join("rmux.sock");
        let rmux = RmuxFrontend::open_with(
            DaemonConfig::new(socket.clone()),
            &seed,
            Arc::new(Mutex::new(PolicyValidator::new())),
            ShellEnvironment::new(),
            TerminalGeometry { rows: 24, cols: 80 },
            fs,
        )
        .await
        .expect("open an rmux frontend");

        let io = rmux.io();
        Self {
            rmux: Some(rmux),
            io,
            seed,
            socket,
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

    /// Runs the built `rmux` binary against this host's socket, with `stdin` piped in.
    ///
    /// Blocking work goes through `spawn_blocking` because the daemon this subprocess talks to
    /// is running on the very runtime the test is on: waiting for the child inline would hold
    /// the only thread the daemon needs to answer it.
    async fn run_cli(&self, args: &[&str], stdin: &'static [u8]) -> CliOutcome {
        let socket = self.socket.clone();
        let seed = self.seed.clone();
        let args: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
        let joined = tokio::time::timeout(
            TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let mut child = Command::new(env!("CARGO_BIN_EXE_rmux"))
                    .arg("-S")
                    .arg(&socket)
                    .args(&args)
                    // The invocation's cwd is what the managed pane starts in.
                    .current_dir(&seed)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn the rmux binary");
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(stdin)
                    .expect("write the workload's stdin");
                let output = child.wait_with_output().expect("collect the invocation");
                CliOutcome {
                    code: output.status.code().unwrap_or(-1),
                    stdout: output.stdout,
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                }
            }),
        )
        .await
        .expect("the managed invocation finishes rather than hanging");
        joined.expect("the invocation task did not panic")
    }

    /// Job identities the daemon currently has, which is where an owned session's pane shows up.
    async fn job_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.io.jobs().iter().map(|job| job.id.to_string()).collect();
        ids.sort();
        ids
    }
}

/// What one CLI invocation produced.
struct CliOutcome {
    /// The process exit code.
    code: i32,
    /// Raw stdout bytes, undecoded.
    stdout: Vec<u8>,
    /// Decoded stderr, for diagnostics in assertion messages.
    stderr: String,
}

/// Publishes a file through the facade, so a later CLI invocation meets an owned seed path.
async fn publish(io: &ShellIo, cmd: &str) {
    let execution = io
        .execute(ExecutionSpec {
            directory: String::new(),
            id: None,
            process: rmux_proto::ProcessCommand::Shell(cmd.to_owned()),
            environment: None,
        })
        .await
        .expect("admit the setup workload");
    let captured = tokio::time::timeout(TIMEOUT, execution.collect(CollectOptions::default()))
        .await
        .expect("the setup workload finishes")
        .expect("the setup workload collects");
    assert!(
        captured.completion.is_published(),
        "test setup must actually reach the seed: {:?}",
        captured.completion.outcome
    );
}

/// `rmux -c` runs on the shared daemon and leaves nothing of its own behind.
///
/// The two halves matter equally. A command whose write is granted must be in the seed
/// afterwards — that is what "goes through the gate" means rather than "was allowed to run".
/// And the session it ran in must be gone, because an app-owned session that outlived its
/// invocation would accumulate one per `rmux -c` for the life of the daemon.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_managed_command_publishes_and_leaves_no_session_behind() {
    let host = CliHost::new().await;
    let before = host.job_ids().await;

    let outcome = host
        .run_cli(&["-c", "printf gated > cli-file"], b"")
        .await;

    assert_eq!(
        outcome.code, 0,
        "a granted command reports success: {}",
        outcome.stderr
    );
    assert_eq!(
        std::fs::read_to_string(host.seed("cli-file")).expect("the published file"),
        "gated",
        "the bytes in the seed are the bytes the command wrote"
    );
    assert_eq!(
        host.job_ids().await,
        before,
        "the owned `marsh-io-` session is disposed of before the invocation returns"
    );

    host.shutdown().await;
}

/// Exiting zero is not being published, and `rmux -c` has to say so.
///
/// This is the single behaviour the cutover exists for. Under the old local `exec` the command
/// below would have overwritten the file and reported success; the seed would be wrong and the
/// caller would never know. The managed route reports the *gated* status, so a refused write is
/// a failed command even though the program itself was perfectly happy.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_denied_write_fails_the_command_that_exited_zero() {
    let host = CliHost::new().await;
    publish(&host.io, "printf owner > owned").await;

    let outcome = host.run_cli(&["-c", "printf other > owned"], b"").await;

    assert_ne!(
        outcome.code, 0,
        "a denied publication is a failed command even though `printf` exited 0; stderr: {}",
        outcome.stderr
    );
    assert_eq!(
        std::fs::read_to_string(host.seed("owned")).expect("the owned file"),
        "owner",
        "the seed keeps the first principal's bytes"
    );

    host.shutdown().await;
}

/// Redirected stdin is relayed, EOF arrives as terminal EOF, and the last bytes are not lost.
///
/// Three failure modes in one scenario, because they share a single ordering. If stdin were not
/// relayed the file would be empty; if EOF were never delivered `cat` would never return and the
/// invocation would hit the timeout; and if the exit observation were allowed to truncate the
/// stream, the marker printed immediately before exit would be missing from stdout. The marker
/// is deliberately built from two pieces so that seeing the echoed command is not mistaken for
/// seeing its output.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn relayed_stdin_reaches_the_workload_and_final_output_survives_exit() {
    let host = CliHost::new().await;

    let outcome = host
        .run_cli(
            &["-c", "cat > typed; printf '%s%s\\n' RELAY_ DONE"],
            b"piped-input\n",
        )
        .await;

    assert_eq!(
        outcome.code, 0,
        "the relayed command completes: {}",
        outcome.stderr
    );
    assert_eq!(
        std::fs::read_to_string(host.seed("typed")).expect("the published file"),
        "piped-input\n",
        "stdin bytes reached the pane and EOF let `cat` finish"
    );
    assert!(
        String::from_utf8_lossy(&outcome.stdout).contains("RELAY_DONE"),
        "output written immediately before exit is drained, not truncated by the exit: {:?}",
        String::from_utf8_lossy(&outcome.stdout)
    );

    host.shutdown().await;
}
