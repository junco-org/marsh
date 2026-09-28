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
//! The same binary's *startup* is pinned here too, because it needs exactly this fixture: an
//! ordinary application launch replaces whatever daemon owns the endpoint it selected, while a
//! control command, a `-c` workload and `-N` reuse it.
//!
//! The fixture is the [`Host`] [`tests/rmux.rs`](./rmux.rs) uses: a fake btrfs
//! ([`marsh_btrfs::fake::CopyTree`]) under a temporary directory and one host per test. Every test
//! is `#[serial]` because builtin instrumentation is process-global and because the shell a test
//! opens takes that seed's lease.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::rmux::{Host, frontend, pipes, saw};
use common::run;
use marsh::rmux::IoEvent;
use marsh::shellmux::{CommandOptions, ShellId};
use marsh_btrfs::Subvolumes;
use marsh_btrfs::fake::CopyTree;
use serial_test::serial;

/// How long a managed CLI invocation may take before a test declares it hung.
const TIMEOUT: Duration = Duration::from_mins(1);

/// How long a readiness or exit probe sleeps between attempts.
const POLL: Duration = Duration::from_millis(10);

/// The built `rmux` binary aimed at `socket`.
///
/// Whoever runs the suite may already be inside a multiplexer, and an inherited `$RMUX`/`$TMUX`
/// changes which client context the CLI admits. The endpoint under test is the `-S` one, never an
/// ambient one.
fn rmux(socket: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rmux"));
    command
        .arg("-S")
        .arg(socket)
        .env_remove("RMUX")
        .env_remove("TMUX");
    command
}

impl Host {
    /// Runs the built `rmux` binary against this host's socket, with `stdin` piped in.
    ///
    /// Blocking work goes through `spawn_blocking` because the daemon this subprocess talks to
    /// is running on the very runtime the test is on: waiting for the child inline would hold
    /// the only thread the daemon needs to answer it.
    async fn run_cli(&self, args: &[&str], stdin: &'static [u8]) -> CliOutcome {
        let mut command = rmux(&self.socket);
        command
            .args(args)
            // The invocation's cwd is what the managed pane starts in.
            .current_dir(&self.seed)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let joined = tokio::time::timeout(
            TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let mut child = command.spawn().expect("spawn the rmux binary");
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

    /// Runs the binary with no input, requiring it to succeed; `what` names the claim.
    async fn succeeds(&self, args: &[&str], what: &str) {
        let outcome = self.run_cli(args, b"").await;
        assert_eq!(outcome.code, 0, "{what}: {}", outcome.stderr);
    }

    /// Job identities the daemon currently has, which is where an owned session's pane shows up.
    fn job_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .io
            .jobs()
            .iter()
            .map(|job| job.id.to_string())
            .collect();
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

/// A foreground `rmux` process this test owns: a `-D` daemon, or a client run to its failure.
///
/// Ownership is the whole point: [`Host::run_cli`] waits for the child it spawned, so it
/// cannot hold a process that is supposed to outlive the invocation. `Drop` is failure cleanup
/// only — a passing run stops every daemon through the ordinary `kill-server` CLI and reaps it
/// here, so a panicking assertion is the only thing that can leave a process bound to the
/// scratch socket.
struct CliProcess {
    /// The spawned process, killed and reaped on drop.
    child: std::process::Child,
    /// Where its stderr was redirected, read back when it ends unexpectedly.
    stderr: PathBuf,
}

impl CliProcess {
    /// Spawns `command` with its stderr redirected to `stderr`.
    fn spawn(command: &mut Command, stderr: PathBuf) -> Self {
        let log = std::fs::File::create(&stderr).expect("create a stderr log");
        let child = command
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn the rmux binary");
        Self { child, stderr }
    }

    /// This process's exit status if it has already finished, reaping it when it has.
    fn finished(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("poll the foreground process")
    }

    /// Whatever the process wrote to stderr so far, for an assertion message.
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A [`CopyTree`] whose *next* subvolume deletion can be stopped in the middle and resumed.
///
/// Engine release reclaims each job's snapshot before it gives the seed's lease back, and that
/// reclamation is the only observable point strictly *inside* teardown. Holding it open turns a
/// race — "does the socket outlive the lease?" — into a decidable question: while the gate is
/// closed the seed is provably still held, so whatever the endpoint looks like at that instant is
/// what a replacement would find.
///
/// The gate is one-shot and starts unarmed, so no ordinary reclamation is affected.
#[derive(Default)]
struct PausingCopyTree {
    /// The real behaviour; every operation but one is this backend's.
    inner: CopyTree,
    /// The armed gate: how the deletion announces it arrived, and what it waits on.
    delete_pause: std::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}

impl PausingCopyTree {
    /// Arms the gate, returning the arrival notification and the release it waits for.
    fn pause_next_delete(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        *self.delete_pause.lock().expect("arm the deletion gate") = Some((reached_tx, resume_rx));
        (reached_rx, resume_tx)
    }
}

impl Subvolumes for PausingCopyTree {
    fn is_subvolume(&self, path: &Path) -> bool {
        self.inner.is_subvolume(path)
    }

    fn is_mount_root(&self, path: &Path) -> Result<bool, marsh_btrfs::Error> {
        self.inner.is_mount_root(path)
    }

    fn assert_btrfs(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.assert_btrfs(path)
    }

    fn assert_user_subvol_rm_allowed(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.assert_user_subvol_rm_allowed(path)
    }

    fn create_subvolume(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.create_subvolume(path)
    }

    fn snapshot(&self, src: &Path, dest: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.snapshot(src, dest)
    }

    fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.snapshot_readonly(src, dest)
    }

    fn delete_subvolume(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        // Out of the mutex before waiting: the gate is one-shot, and holding the lock across the
        // wait would stall every later reclamation behind this one.
        let armed = self
            .delete_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((reached, resume)) = armed {
            let _ = reached.send(());
            // A release *or* a dropped sender ends the wait, so an unwinding test cannot strand
            // this blocking worker.
            let _ = resume.recv();
        }
        self.inner.delete_subvolume(path)
    }
}

/// Re-runs `probe` every [`POLL`] until it answers, failing with its latest complaint once
/// [`TIMEOUT`] has passed.
async fn poll<T>(mut probe: impl AsyncFnMut() -> Result<T, String>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match probe().await {
            Ok(answer) => return answer,
            Err(complaint) => assert!(Instant::now() < deadline, "{complaint}"),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Polls the endpoint until `daemon` answers `list-sessions`, returning the sessions it reported.
///
/// `-N` is deliberate. An auto-start here would answer from a daemon the test never spawned, so
/// a replacement that failed to bind would read as a successful restart.
async fn await_sessions(host: &Host, daemon: &mut CliProcess, label: &str) -> serde_json::Value {
    poll(async || {
        if let Some(status) = daemon.finished() {
            panic!(
                "{label} exited ({status}) instead of serving {}: {}",
                host.socket.display(),
                daemon.stderr()
            );
        }
        let outcome = host.run_cli(&["-N", "list-sessions", "--json"], b"").await;
        if outcome.code == 0 {
            return Ok(serde_json::from_slice(&outcome.stdout).expect("list-sessions emits JSON"));
        }
        Err(format!(
            "{label} never answered on {}: {}",
            host.socket.display(),
            outcome.stderr
        ))
    })
    .await
}

/// Waits for an owned daemon to exit and returns its status.
async fn await_exit(daemon: &mut CliProcess, label: &str) -> ExitStatus {
    poll(async || {
        daemon
            .finished()
            .ok_or_else(|| format!("{label} never exited: {}", daemon.stderr()))
    })
    .await
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
    let host = Host::new().await;
    let before = host.job_ids();

    host.succeeds(
        &["-c", "printf gated > cli-file"],
        "a granted command reports success",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(host.seed("cli-file")).expect("the published file"),
        "gated",
        "the bytes in the seed are the bytes the command wrote"
    );
    assert_eq!(
        host.job_ids(),
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
    let host = Host::new().await;
    // Published through the facade, so the CLI invocation meets an owned seed path.
    let setup = run(&host.io, "printf owner > owned").await;
    assert!(
        setup.completion.is_published(),
        "test setup must actually reach the seed: {:?}",
        setup.completion
    );

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
    let host = Host::new().await;

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

/// An ordinary application startup replaces the daemon on its endpoint; a control command reuses
/// it.
///
/// Starting the application is how a freshly built executable becomes the daemon, so the launch
/// cannot be allowed to hand the socket back to whatever is already there — not when the old
/// daemon is idle, and not when it is busy. The seeded session below is genuinely live, with a
/// pane still blocked on `cat`, because "replace the daemon" that quietly degraded to "replace an
/// idle daemon" would pass every cheaper check.
///
/// The other half is the part that must *not* change. `list-sessions`, a `-c` workload and `-N`
/// all target sessions the caller expects to still be there afterwards; if startup replacement
/// leaked into them, using the multiplexer would destroy the work being multiplexed. A second
/// host proves the same scoping across endpoints: a restart on one socket is not a restart of
/// every daemon.
///
/// Two replacements run back to back from one build, so a restart gated on a version or a binary
/// mtime would leave the second child racing a socket the first still owns. The third starts on
/// an endpoint that was just killed, which is the cold case: nothing to stop is not a failure.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn application_startup_replaces_the_daemon_but_control_commands_do_not() {
    let mut host = Host::new().await;

    // The pane's first managed command starts the process-wide tracer unless a managed view
    // already holds it. That start must not race the capture-pane polling below, whose clients
    // this process keeps forking, so an idle managed shell holds the tracer across it.
    let warm = marsh_core::test_support::shell_builder(Arc::clone(&host.fs))
        .working_dir(host.seed.clone())
        .sandbox_policy(marsh::SandboxPolicy::allow())
        .build()
        .await
        .expect("build the tracer-holding shell");
    run(&warm, ":").await;

    // A real session with a pane parked on `cat`: the marker is printed by the pane, and the two
    // `printf` pieces keep an echo of the command itself from being mistaken for its output.
    host.succeeds(
        &[
            "new-session",
            "-d",
            "-s",
            "before-restart",
            "printf '%s%s\\n' ACTIVE_ READY; cat",
        ],
        "the live session is created",
    )
    .await;
    poll(async || {
        let pane = host
            .run_cli(&["capture-pane", "-p", "-t", "before-restart"], b"")
            .await;
        if String::from_utf8_lossy(&pane.stdout).contains("ACTIVE_READY") {
            return Ok(());
        }
        Err(format!(
            "the seeded pane never reached its marker (exit {}): {:?} / {}",
            pane.code,
            String::from_utf8_lossy(&pane.stdout),
            pane.stderr
        ))
    })
    .await;
    // The pane's own managed view holds the tracer from here on.
    warm.close(false)
        .await
        .expect("close the tracer-holding shell");

    for (label, args) in [
        ("an explicit control command", vec!["list-sessions"]),
        (
            "a managed workload",
            vec!["-c", "printf '%s%s\\n' KEEP_ ALIVE"],
        ),
    ] {
        host.succeeds(&args, &format!("{label} succeeds")).await;
        host.succeeds(
            &["has-session", "-t", "before-restart"],
            &format!("{label} must not replace the daemon"),
        )
        .await;
    }

    // `-N` is the explicit no-auto-start mode. Its own admission may refuse this piped stdin —
    // that wording is not this test's business — but either way it is not an application startup.
    let no_start = host.run_cli(&["-N"], b"").await;
    let alive = host
        .run_cli(&["has-session", "-t", "before-restart"], b"")
        .await;
    assert_eq!(
        alive.code, 0,
        "`-N` must not replace the daemon (it exited {}: {}): {}",
        no_start.code, no_start.stderr, alive.stderr
    );

    let other = Host::new().await;
    other
        .succeeds(
            &["new-session", "-d", "-s", "untouched"],
            "the other endpoint's sentinel session is created",
        )
        .await;

    // Without this a replacement could shut itself down for being empty, and a free socket would
    // no longer distinguish "took over" from "never bound".
    let config = host.scratch.path().join("restart.conf");
    std::fs::write(&config, "set-option -s exit-empty off\n").expect("write the restart config");

    let spawn_daemon = |label: &str| {
        CliProcess::spawn(
            rmux(&host.socket)
                .arg("-D")
                .arg("-f")
                .arg(&config)
                .current_dir(&host.seed)
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
            host.scratch.path().join(format!("{label}.stderr")),
        )
    };

    // `wait` is the ending an externally stopped daemon takes, and the only one the replacement
    // can cause: nothing in this process asks the fixture host to stop.
    let rmux = host.rmux.take().expect("the fixture host is running");
    let waiting = tokio::spawn(async move { rmux.wait().await });

    let mut first = spawn_daemon("first-replacement");
    tokio::time::timeout(TIMEOUT, waiting)
        .await
        .expect("the fixture daemon's listener exits once the replacement stops it")
        .expect("the waiting task finished")
        .expect("the fixture daemon stopped cleanly");
    assert_eq!(
        await_sessions(&host, &mut first, "the first replacement daemon").await,
        serde_json::json!([]),
        "the replacement owns the endpoint, and the live session went with the daemon that held it"
    );
    other
        .succeeds(
            &["has-session", "-t", "untouched"],
            "a restart is scoped to the endpoint it selected",
        )
        .await;

    let mut second = spawn_daemon("second-replacement");
    let replaced = await_exit(&mut first, "the first replacement daemon").await;
    assert!(
        replaced.success(),
        "an identical executable still replaces the daemon it finds, and stops it cleanly: \
         {replaced}"
    );
    assert_eq!(
        await_sessions(&host, &mut second, "the second replacement daemon").await,
        serde_json::json!([]),
        "the second replacement bound the endpoint the first one released"
    );

    host.succeeds(
        &["kill-server"],
        "the ordinary control command stops the daemon",
    )
    .await;
    let stopped = await_exit(&mut second, "the second replacement daemon").await;
    assert!(
        stopped.success(),
        "`kill-server` ends it cleanly: {stopped}"
    );
    assert!(!host.socket.exists(), "`kill-server` released the endpoint");

    // Cold start: with nothing to stop, the pre-start shutdown has to be a no-op rather than the
    // "no server running" failure an absent daemon is to every other command.
    let mut cold = spawn_daemon("cold-start");
    assert_eq!(
        await_sessions(&host, &mut cold, "the cold-start daemon").await,
        serde_json::json!([]),
        "an absent daemon does not turn a startup into a failure"
    );
    host.succeeds(&["kill-server"], "the cold-start daemon stops on request")
        .await;
    let stopped = await_exit(&mut cold, "the cold-start daemon").await;
    assert!(
        stopped.success(),
        "the cold-start daemon ends cleanly: {stopped}"
    );
    assert!(!host.socket.exists(), "the endpoint is released again");

    other
        .succeeds(
            &["has-session", "-t", "untouched"],
            "the other endpoint survived every restart",
        )
        .await;

    other.shutdown().await;
    host.shutdown().await;
}

/// A startup that dies before any daemon exists still says which operation died, and where.
///
/// A bare `rmux` launch stops whatever owns the endpoint it selected before binding it. Pointing
/// `-S` through a regular file makes that stop fail with `ENOTDIR`, which is a real transport
/// failure rather than the absent daemon a cold start tolerates. What is pinned is that the
/// message is no longer only the raw client error: it names `stop previous daemon` and the
/// selected socket while keeping the original OS cause, so the stage a launch died at is
/// readable from the diagnostic alone. Nothing is started, so this needs no host.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn startup_stop_failure_identifies_operation_and_socket() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let blocker = scratch.path().join("not-a-directory");
    std::fs::write(&blocker, b"").expect("create the file standing where a directory would be");
    let socket = blocker.join("rmux.sock");

    let mut startup = CliProcess::spawn(
        rmux(&socket)
            .current_dir(scratch.path())
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
        scratch.path().join("startup.stderr"),
    );

    let status = await_exit(&mut startup, "the failing startup").await;
    let diagnostic = startup.stderr();

    assert_eq!(
        status.code(),
        Some(1),
        "an unusable endpoint fails the launch: {diagnostic}"
    );
    assert_eq!(
        diagnostic.matches("startup failed during").count(),
        1,
        "the stage is named once, not re-wrapped by an outer handler: {diagnostic}"
    );
    assert!(
        diagnostic.contains("stop previous daemon"),
        "the diagnostic names the startup operation that failed: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&socket.display().to_string()),
        "the diagnostic names the endpoint this launch selected: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&std::io::Error::from_raw_os_error(libc::ENOTDIR).to_string()),
        "decoration keeps the original cause rather than replacing it: {diagnostic}"
    );
}

/// An attach that fails while driving the terminal names the attach stage, not just the errno.
///
/// This is the other side of the same diagnostic: a daemon that answers, a session that is
/// created, and a failure that only happens once the attach stream is writing to the client's
/// terminal. A private pty on stdin is what admits the attach at all; `/dev/full` on stdout is
/// what the stream then fails to write to, with `ENOSPC` — a cause no retry or timeout can be
/// mistaken for. Pinning both tests together is the point: two genuinely different startup
/// failures keep their own causes while naming different operations.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn attach_io_failure_identifies_operation_and_socket() {
    let host = Host::new().await;

    // The master stays owned by the test until the client is reaped: closing it early would
    // hang up the client's stdin and race the write failure this test is about.
    let (master, slave) = marsh::shellmux::pty::open_pty(24, 80).expect("open a private pty");
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full as the attach stream's output");
    let socket = host.socket.clone();

    // `-N` keeps this a client of the fixture host: an ordinary launch would replace that fake
    // btrfs host with a real daemon, and there would be no seed left to create a session in.
    let mut attaching = CliProcess::spawn(
        rmux(&socket)
            .args(["-N", "new-session", "-s", "diagnostic-attach"])
            .current_dir(&host.seed)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave))
            .stdout(Stdio::from(sink)),
        host.scratch.path().join("attach.stderr"),
    );

    let status = await_exit(&mut attaching, "the failing attach client").await;
    let diagnostic = attaching.stderr();
    drop(master);
    host.shutdown().await;

    assert_eq!(
        status.code(),
        Some(1),
        "an attach that cannot write to the terminal fails the command: {diagnostic}"
    );
    assert_eq!(
        diagnostic.matches("startup failed during").count(),
        1,
        "the stage is named once, not re-wrapped by an outer handler: {diagnostic}"
    );
    assert!(
        diagnostic.contains("run terminal attach"),
        "the diagnostic names the attach phase rather than the whole invocation: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&socket.display().to_string()),
        "the diagnostic names the endpoint this client attached through: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&std::io::Error::from_raw_os_error(libc::ENOSPC).to_string()),
        "decoration keeps the original cause rather than replacing it: {diagnostic}"
    );
}

/// `kill-server` releases the endpoint only after the engine has given the seed back.
///
/// This is the warm-restart race a replacement actually hits. A launch stops whatever owns the
/// endpoint it selected and then binds it, and the client it does that through treats the
/// socket's *pathname* disappearing as the stop having completed. If the outgoing daemon removed
/// that pathname while its multiplexer still held the seed's exclusive lease, the replacement
/// would be admitted to bind and its very first pane would fail to open — the seed "already has
/// an active session" belonging to a daemon that is already gone.
///
/// Pausing the outgoing daemon inside snapshot reclamation is what makes that decidable rather
/// than a scheduling coin flip: the seed is provably still leased at that instant, so the state
/// of the endpoint there is exactly what a replacement would find. The refusal is matched
/// structurally, so "that seed is busy" cannot be confused with any other spawn failure, and the
/// reacquisition is proven by a *published* file rather than by a socket that merely bound.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn kill_server_waits_for_seed_release_before_releasing_endpoint() {
    let fs = Arc::new(PausingCopyTree::default());
    let host = Host::with(Arc::clone(&fs) as Arc<dyn Subvolumes>).await;

    // An ordinary directory inside the seed, not a subvolume of its own: the lease belongs to the
    // containing seed, so a job started here and a replacement started here contend for one
    // lease — the parent-seed/child-repository shape a real launch has.
    let repo = host.seed("repo");
    std::fs::create_dir_all(&repo).expect("create the nested repository directory");

    // Binding a host leases nothing, and neither does opening a shell: the first managed command
    // of a shell on the seed is what takes it. A pipe job is hidden rather than adopted as a pane,
    // so no pane lifecycle work can reclaim its snapshot before engine release does.
    let mut events = host.io.observe().events;
    let held = host
        .io
        .open_shell(&repo, Some(ShellId::from("held")), pipes())
        .await
        .expect("open the job that takes the seed's lease");
    let observed_open = saw(
        &mut events,
        |envelope| matches!(&envelope.event, IoEvent::Opened { job } if job.id() == held.id()),
    )
    .await;
    assert!(observed_open, "the held job is open");
    // Still running when teardown starts, so its private view and the seed's lease are held
    // until engine release reclaims them.
    let running = tokio::spawn({
        let held = held.clone();
        async move {
            held.run_command("sleep 3600", CommandOptions::default())
                .await
        }
    });
    let seed_name = host.seed.file_name().expect("a named seed");
    let view = host
        .seed
        .parent()
        .expect("a seed parent")
        .join(marsh_btrfs::STATE_DIR)
        .join(seed_name)
        .join("snap")
        .join(held.sandbox().uid.as_str());
    poll(async || {
        if view.exists() {
            Ok(())
        } else {
            Err(format!(
                "the held command's view {} never appeared",
                view.display()
            ))
        }
    })
    .await;

    // The consumer that proves both halves: refused while the seed is held, served once it is
    // released. Construction leases nothing, so this succeeds now and decides nothing yet.
    let next = frontend(
        &host.scratch.path().join("next.sock"),
        &repo,
        Arc::clone(&host.fs),
        "a second host binds its own socket while the first holds the seed",
    )
    .await;

    // Armed only now, so the gate catches teardown's reclamation rather than the seed's opening
    // sweep.
    let (reached, resume) = fs.pause_next_delete();
    let mut killer = CliProcess::spawn(
        rmux(&host.socket)
            .args(["-N", "kill-server"])
            .current_dir(&host.seed)
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
        host.scratch.path().join("kill-server.stderr"),
    );

    tokio::time::timeout(TIMEOUT, reached)
        .await
        .expect("engine release reaches snapshot reclamation")
        .expect("the reclamation gate is not dropped before it is reached");

    // Recorded rather than asserted: an assertion that unwound here would leave the daemon's
    // blocking worker parked on a gate nobody opens.
    let socket_present = host.socket.exists();
    let endpoint_answers = std::os::unix::net::UnixStream::connect(&host.socket).is_ok();
    let killer_running = killer.finished().is_none();
    let refusal = next
        .io()
        .open_shell(&repo, Some(ShellId::from("intruder")), pipes())
        .await;
    let shared_during_cleanup = refusal.is_ok();

    // The gate opens before anything that could abort.
    let _ = resume.send(());
    drop(resume);

    let status = await_exit(&mut killer, "the kill-server invocation").await;
    let killer_stderr = killer.stderr();

    // Deliberately before the old frontend is awaited or shut down: what the CLI's own success
    // has to mean is that the seed is *already* reusable, not that it will be once this process
    // gets around to joining a daemon a real launch could never join.
    let reopened = next
        .io()
        .open_shell(&repo, Some(ShellId::from("reopened")), pipes())
        .await;
    let published = match &reopened {
        Ok(job) => tokio::time::timeout(
            TIMEOUT,
            job.run_command("printf acquired > lease-ready", CommandOptions::default()),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|completion| completion.is_published()),
        Err(_) => None,
    };
    let lease_ready = std::fs::read(host.seed("repo/lease-ready")).ok();

    next.shutdown()
        .await
        .expect("shut the replacement host down");
    host.shutdown().await;
    let held_verdict = tokio::time::timeout(TIMEOUT, running)
        .await
        .expect("teardown ends the held command")
        .expect("the held command's task did not panic");
    assert!(
        held_verdict.is_err(),
        "the held command was ended by teardown, not completed: {held_verdict:?}"
    );

    assert!(
        socket_present,
        "the endpoint must stay bound until the seed is released, or a replacement binds it and \
         then cannot open a pane on the seed the outgoing daemon still holds; `kill-server` \
         stderr: {killer_stderr}"
    );
    assert!(
        endpoint_answers,
        "the reserved endpoint is still the listening one, not an orphaned pathname"
    );
    assert!(
        killer_running,
        "`kill-server` reports the stop as complete only after the seed is back (exit {status})"
    );
    assert!(
        shared_during_cleanup,
        "same-process construction shares authority without waiting for another shell's cleanup: {refusal:?}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "the stop itself succeeds: {killer_stderr}"
    );
    assert!(
        reopened.is_ok(),
        "the released seed opens for the next generation: {reopened:?}"
    );
    assert_eq!(
        published,
        Some(true),
        "the replacement owns the seed well enough to publish through it"
    );
    assert_eq!(
        lease_ready.as_deref(),
        Some(b"acquired".as_slice()),
        "the published bytes are in the seed, so this is ownership rather than a bound socket"
    );
}

/// The one job id the daemon gained since `before`, waited for.
///
/// A detached `new-session` answers as soon as the session exists; the pane's shell is a job a
/// moment later, and only the id itself proves a later removal was that pane's rather than a
/// redrawn listing's.
async fn added_job_id(host: &Host, before: &[String]) -> String {
    poll(async || {
        let fresh: Vec<String> = host
            .job_ids()
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        if let [id] = fresh.as_slice() {
            return Ok(id.clone());
        }
        Err(format!("expected exactly one new job, got {fresh:?}"))
    })
    .await
}

/// A typed `exit` closes the pane it was typed in, with the status its own builtin computed.
///
/// The prompt used to answer `exit` itself and throw its argument away, so a typed `exit 7` closed
/// a pane with no status of its own. It is brush's builtin now: the line runs in that pane's shell
/// like any other, the job ends with 7, and the daemon turns that into an ordinary pane death —
/// which `remain-on-exit` still governs, in both of its settings. Nothing else is touched.
///
/// Driven through the real binary and the real prompt: `rmux -c`, a one-shot pipe execution or a
/// direct `io.stop` would each skip the path that was broken.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_typed_exit_uses_builtin_status_and_closes_only_its_pane() {
    let host = Host::new().await;

    // No workload argument anywhere here: these panes run the prompt, which is what a user types
    // into.
    for session in ["exit-survivor", "exit-status"] {
        host.succeeds(
            &["-N", "new-session", "-d", "-s", session],
            &format!("{session} is created"),
        )
        .await;
    }
    let before = host.job_ids();
    host.succeeds(
        &["-N", "new-session", "-d", "-s", "exit-bare"],
        "exit-bare is created",
    )
    .await;
    let bare_job = added_job_id(&host, &before).await;

    // Both policies explicitly, so neither result is the default's accident.
    for (session, value) in [("exit-status", "on"), ("exit-bare", "off")] {
        host.succeeds(
            &[
                "-N",
                "set-option",
                "-w",
                "-t",
                session,
                "remain-on-exit",
                value,
            ],
            &format!("remain-on-exit {value} is set on {session}"),
        )
        .await;
    }

    // The marker is assembled from two pieces: the echoed command line carries `EXIT_ READY` with
    // the space, so only the line's *output* can satisfy this wait. Reaching it proves the prompt
    // is running lines before anything types `exit` at it.
    for session in ["exit-status", "exit-bare"] {
        host.succeeds(
            &[
                "-N",
                "send-keys",
                "-t",
                session,
                "--wait-next-text",
                "EXIT_READY",
                "--timeout",
                "5s",
                "--",
                "printf '%s%s\\n' EXIT_ READY",
                "Enter",
            ],
            &format!("{session}'s prompt ran a line of its own"),
        )
        .await;
    }

    host.succeeds(
        &[
            "-N",
            "send-keys",
            "-t",
            "exit-status",
            "--",
            "exit 7",
            "Enter",
        ],
        "`exit 7` is delivered",
    )
    .await;

    let dead = poll(async || {
        let shown = host
            .run_cli(
                &[
                    "-N",
                    "display-message",
                    "-p",
                    "-t",
                    "exit-status",
                    "#{pane_dead}:#{pane_dead_status}",
                ],
                b"",
            )
            .await;
        let text = String::from_utf8_lossy(&shown.stdout).trim_end().to_owned();
        if shown.code == 0 && text.starts_with("1:") {
            return Ok(text);
        }
        Err(format!(
            "the typed `exit 7` never killed its pane (exit {}): {text:?} / {}",
            shown.code, shown.stderr
        ))
    })
    .await;
    assert_eq!(
        dead, "1:7",
        "the builtin's own status reaches the pane rather than being discarded by the prompt"
    );

    host.succeeds(
        &["-N", "send-keys", "-t", "exit-bare", "--", "exit", "Enter"],
        "the bare `exit` is delivered",
    )
    .await;

    poll(async || {
        let gone = host
            .run_cli(&["-N", "has-session", "-t", "exit-bare"], b"")
            .await;
        let jobs = host.job_ids();
        if gone.code == 1 && !jobs.contains(&bare_job) {
            return Ok(());
        }
        Err(format!(
            "the bare `exit` never took its pane down (has-session exited {}, jobs {jobs:?})",
            gone.code
        ))
    })
    .await;
    host.succeeds(
        &["-N", "has-session", "-t", "exit-survivor"],
        "one pane's exit is not another's",
    )
    .await;

    // The retained dead pane is still a pane: killing it retires the session it was kept in.
    host.succeeds(
        &["-N", "kill-pane", "-t", "exit-status"],
        "the retained dead pane is killable",
    )
    .await;
    let retired = host
        .run_cli(&["-N", "has-session", "-t", "exit-status"], b"")
        .await;
    assert_eq!(
        retired.code, 1,
        "killing its only pane took the session with it"
    );
    host.succeeds(
        &["-N", "has-session", "-t", "exit-survivor"],
        "the survivor is still reachable afterwards",
    )
    .await;

    host.shutdown().await;
}
