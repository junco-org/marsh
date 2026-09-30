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
//! So is the prompt a commandless pane runs, typed at through the same binary's `send-keys`: a
//! finished line reaches that pane's shell exactly as typed, with no command grammar of the
//! prompt's own in between, and `exit` is the shell's builtin like any other word.
//!
//! The fixture is the [`Host`] [`tests/rmux.rs`](./rmux.rs) uses: a fake btrfs
//! ([`marsh_btrfs::fake::CopyTree`]) under a temporary directory and one host per test. Every test
//! is `#[serial]` because builtin instrumentation is process-global and because the shell a test
//! opens takes that seed's lease.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use brush_core::escape::{QuoteMode, quote_if_needed};
use common::rmux::{Host, frontend, pipes, saw};
use common::run;
use marsh::ShellErrorKind;
use marsh::rmux::IoEvent;
use marsh::shellmux::{CommandCompletion, CommandHandle, CommandOptions, ShellId};
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

/// A [`CopyTree`] whose *next* subvolume deletion, writable snapshot or read-only snapshot can be
/// stopped in the middle and resumed.
///
/// Engine release reclaims each job's snapshot before it gives the seed's lease back, and that
/// reclamation is the only observable point strictly *inside* teardown. Holding it open turns a
/// race — "does the socket outlive the lease?" — into a decidable question: while the gate is
/// closed the seed is provably still held, so whatever the endpoint looks like at that instant is
/// what a replacement would find.
///
/// The snapshot gates do the same for a command's own phases. A shell's first managed command
/// takes its writable view before any of its work can run, and a command that wrote anything
/// freezes that view read-only after its producers have finished and before its publication is
/// sealed. Holding either one is a command that is provably *not* executing, however long it
/// takes.
///
/// Every gate is one-shot and starts unarmed, so no ordinary operation is affected.
#[derive(Default)]
struct PausingCopyTree {
    /// The real behaviour; every operation but the gated ones is this backend's.
    inner: CopyTree,
    /// The armed deletion gate.
    delete_pause: Gate,
    /// The armed writable-snapshot gate.
    snapshot_pause: Gate,
    /// The armed read-only-snapshot gate.
    readonly_pause: Gate,
}

/// One armed gate: how the operation announces it arrived, and what it waits on.
type Gate = std::sync::Mutex<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
>;

/// Arms `gate`, returning the arrival notification and the release it waits for.
fn arm(
    gate: &Gate,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *gate.lock().expect("arm the gate") = Some((reached_tx, resume_rx));
    (reached_rx, resume_tx)
}

/// Holds the calling operation at `gate` when it is armed, until it is released.
fn pass(gate: &Gate) {
    // Out of the mutex before waiting: the gate is one-shot, and holding the lock across the wait
    // would stall every later operation of the same kind behind this one.
    let armed = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some((reached, resume)) = armed {
        let _ = reached.send(());
        // A release *or* a dropped sender ends the wait, so an unwinding test cannot strand this
        // blocking worker.
        let _ = resume.recv();
    }
}

impl PausingCopyTree {
    /// Arms the deletion gate.
    fn pause_next_delete(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        arm(&self.delete_pause)
    }

    /// Arms the writable-snapshot gate.
    fn pause_next_snapshot(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        arm(&self.snapshot_pause)
    }

    /// Arms the read-only-snapshot gate.
    fn pause_next_readonly(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        arm(&self.readonly_pause)
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
        pass(&self.snapshot_pause);
        self.inner.snapshot(src, dest)
    }

    fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<(), marsh_btrfs::Error> {
        pass(&self.readonly_pause);
        self.inner.snapshot_readonly(src, dest)
    }

    fn delete_subvolume(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        pass(&self.delete_pause);
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

/// Opens a commandless session, whose pane runs the prompt, and returns that pane's shell id.
async fn open_prompt(host: &Host, session: &str) -> String {
    let before = host.job_ids();
    host.succeeds(
        &["-N", "new-session", "-d", "-s", session],
        &format!("{session} is created"),
    )
    .await;
    added_job_id(host, &before).await
}

/// Types `keys` at `session`'s prompt and returns the verdict of the line it admits into
/// `job_id` as `expected_command`.
///
/// Subscribed before the keys are sent, so the acceptance cannot slip past between the two. The
/// text only picks the receipt out of the stream; what the line actually did is for the caller
/// to prove from what it left behind.
async fn submit_typed(
    host: &Host,
    session: &str,
    job_id: &str,
    keys: &[&str],
    expected_command: &str,
) -> Arc<CommandCompletion> {
    let mut events = host.io.observe().events;
    let mut args = vec!["-N", "send-keys", "-t", session, "--"];
    args.extend_from_slice(keys);
    host.succeeds(&args, &format!("{keys:?} is typed at {session}"))
        .await;
    let accepted = tokio::time::timeout(TIMEOUT, async {
        loop {
            let envelope = events
                .recv()
                .await
                .expect("the observer keeps up with the bus")
                .expect("the bus stays open while the host runs");
            if let IoEvent::CommandAccepted { command } = &envelope.event
                && command.shell().id.as_str() == job_id
                && command.text() == expected_command
            {
                return command.clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{session} never admitted {expected_command:?} into {job_id}"));
    tokio::time::timeout(TIMEOUT, accepted.wait())
        .await
        .unwrap_or_else(|_| panic!("{expected_command:?} never reached a verdict"))
        .expect("the admitted line concludes")
}

/// Requires `completion` to have exited zero and been published.
fn assert_published(completion: &CommandCompletion, line: &str) {
    assert!(
        completion.exit_code() == Some(0) && completion.is_published(),
        "{line:?} exits zero and publishes: {completion:?}"
    );
}

/// The contents a published line left at `file` in the first seed.
fn published(host: &Host, file: &str) -> String {
    std::fs::read_to_string(host.seed(file))
        .unwrap_or_else(|error| panic!("{file} was published: {error}"))
}

/// The daemon's job identities and every pane's placement and selection, compared whole.
///
/// Sorted, so a listing of the same panes in another order is not a change. Both active flags are
/// included, so a selection that moved without any pane being added still is one.
async fn topology(host: &Host) -> (Vec<String>, Vec<String>) {
    let listed = host
        .run_cli(
            &[
                "-N",
                "list-panes",
                "-a",
                "-F",
                "#{session_name}:#{window_index}:#{pane_index}:#{pane_active}:#{window_active}",
            ],
            b"",
        )
        .await;
    assert_eq!(listed.code, 0, "list-panes answers: {}", listed.stderr);
    let mut panes: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    panes.sort();
    (host.job_ids(), panes)
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

/// The words the prompt used to answer itself are ordinary command names.
///
/// Each is shadowed by a shell function, and function lookup precedes every builtin, so a marker
/// holding the function's own argument count and arguments proves the typed line reached the
/// shell whole: the prompt neither answered it, nor re-split or re-quoted its words, nor opened a
/// window for it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn prompt_command_names_reach_shell_functions() {
    const NAMES: [&str; 7] = ["jobs", "fg", "bg", "stop", "kill", "sd", "sda"];
    let host = Host::new().await;
    let job = open_prompt(&host, "raw-names").await;
    let layout = topology(&host).await;

    for name in NAMES {
        let declaration =
            format!("function {name} {{ printf '%s\\n' \"{name}:$#\" \"$@\" > called-{name}; }}");
        let defined = submit_typed(
            &host,
            "raw-names",
            &job,
            &[declaration.as_str(), "Enter"],
            &declaration,
        )
        .await;
        assert_published(&defined, &declaration);
    }
    let assignment = "payload='two words'";
    let assigned = submit_typed(&host, "raw-names", &job, &[assignment, "Enter"], assignment).await;
    assert_published(&assigned, assignment);

    let bare = submit_typed(&host, "raw-names", &job, &["jobs", "Enter"], "jobs").await;
    assert_published(&bare, "jobs");
    assert_eq!(published(&host, "called-jobs"), "jobs:0\n");
    for name in &NAMES[1..] {
        let line = format!("{name} \"$payload\"");
        let called = submit_typed(&host, "raw-names", &job, &[line.as_str(), "Enter"], &line).await;
        assert_published(&called, &line);
        assert_eq!(
            published(&host, &format!("called-{name}")),
            format!("{name}:1\ntwo words\n"),
            "`{name}` ran the function with the expanded, unsplit argument"
        );
    }

    assert_eq!(
        topology(&host).await,
        layout,
        "no line opened, closed or selected a job, pane or window"
    );
    host.shutdown().await;
}

/// `kill` is the shell's own and expands its words, and an open quote after `fg` continues.
///
/// The prompt used to rebuild `kill` from whitespace-split, single-quoted tokens, so `"$sig"`
/// never expanded and `>` was an argument rather than a redirection; and it answered `fg`
/// itself, so the first Enter below would have selected a job rather than waiting for the quote
/// to close. `kill -l` names a signal without sending one, so nothing here signals anything.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn prompt_expands_kill_and_continues_former_control_words() {
    let host = Host::new().await;
    let job = open_prompt(&host, "raw-syntax").await;
    let layout = topology(&host).await;

    let assignment = "sig=15";
    let assigned = submit_typed(
        &host,
        "raw-syntax",
        &job,
        &[assignment, "Enter"],
        assignment,
    )
    .await;
    assert_published(&assigned, assignment);
    let kill = "kill -l \"$sig\" > kill-listed";
    let listed = submit_typed(&host, "raw-syntax", &job, &[kill, "Enter"], kill).await;
    assert_published(&listed, kill);
    assert_eq!(
        published(&host, "kill-listed"),
        "TERM\n",
        "the builtin got the expanded signal number, and its output the redirection"
    );

    let declaration = "function fg { printf '%s' \"$1\" > multiline-fg; }";
    let defined = submit_typed(
        &host,
        "raw-syntax",
        &job,
        &[declaration, "Enter"],
        declaration,
    )
    .await;
    assert_published(&defined, declaration);

    let multiline = "fg \"two  \nwords\"";
    let continued = submit_typed(
        &host,
        "raw-syntax",
        &job,
        &["fg \"two  ", "Enter", "words\"", "Enter"],
        multiline,
    )
    .await;
    assert_published(&continued, multiline);
    assert_eq!(
        published(&host, "multiline-fg"),
        "two  \nwords",
        "the quoted argument kept its spaces and its line break"
    );

    assert_eq!(
        topology(&host).await,
        layout,
        "no line opened, closed or selected a job, pane or window"
    );
    host.shutdown().await;
}

/// Every spelling of a trailing `&` is the shell's own asynchronous list.
///
/// `&NAME` and `&"NAME"` used to name a new job in a window of its own. To the shell they are an
/// asynchronous command followed by the command `NAME`, and the asynchronous one is a task of the
/// line that started it: its output is published with that line's verdict, on the same shell.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn prompt_ampersands_use_native_shell_tasks() {
    let host = Host::new().await;
    let job = open_prompt(&host, "raw-async").await;
    let layout = topology(&host).await;

    let declaration = "function after_amp { printf '%s\\n' called >> amp-calls; }";
    let defined = submit_typed(
        &host,
        "raw-async",
        &job,
        &[declaration, "Enter"],
        declaration,
    )
    .await;
    assert_published(&defined, declaration);

    for (line, file, contents) in [
        ("printf plain > amp-plain &", "amp-plain", "plain"),
        ("printf bare > amp-bare &after_amp", "amp-bare", "bare"),
        (
            "printf quoted > amp-quoted &\"after_amp\"",
            "amp-quoted",
            "quoted",
        ),
    ] {
        let completion = submit_typed(&host, "raw-async", &job, &[line, "Enter"], line).await;
        assert_published(&completion, line);
        assert_eq!(
            published(&host, file),
            contents,
            "{line:?}'s asynchronous command"
        );
    }
    assert_eq!(
        published(&host, "amp-calls"),
        "called\ncalled\n",
        "the word after each named `&` ran as a command, once each"
    );

    assert_eq!(
        topology(&host).await,
        layout,
        "no line opened, closed or selected a job, pane or window"
    );
    host.shutdown().await;
}

/// A denied line's diagnostic is shown exactly once, whichever way its pane then closes.
///
/// Two renderers own the same verdict: the prompt draws it through its lease and records that it
/// did, and the job's retirement draws the verdict its job ended with. `report-idle`'s verdict
/// reaches the prompt, which is then closed by an end of input without a second command: that
/// graceful stop concludes no command, so the job ends carrying no verdict and a pane status of
/// zero, and the prompt's copy must be the only one. `report-exit`'s denied line closes its own
/// shell, so the job ends with that verdict and only retirement can render it. A lost or doubled
/// handoff is a count other than one in the dead pane's retained history.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn prompt_denial_is_reported_once_when_pane_closes() {
    let host = Host::new().await;
    // Published through the facade, so the typed lines below meet a path another shell owns.
    let setup = run(&host.io, "printf owner > report-owned").await;
    assert_published(&setup.completion, "printf owner > report-owned");
    // `-J` joins wrapped rows and `-S -` reaches back through the whole history, so a diagnostic
    // scrolled off the visible screen or wider than it is still one match.
    let capture = async |session: &str| {
        let captured = host
            .run_cli(
                &["-N", "capture-pane", "-p", "-J", "-S", "-", "-t", session],
                b"",
            )
            .await;
        assert_eq!(
            captured.code, 0,
            "{session} is capturable: {}",
            captured.stderr
        );
        String::from_utf8(captured.stdout).expect("the captured pane is UTF-8")
    };

    for (session, line, closes_itself) in [
        ("report-idle", "printf rejected > report-owned", false),
        (
            "report-exit",
            "printf rejected > report-owned; exit 0",
            true,
        ),
    ] {
        let job_id = open_prompt(&host, session).await;
        let shell = host
            .io
            .shell(&ShellId::from(job_id.as_str()))
            .expect("the prompt's shell is addressable");
        host.succeeds(
            &[
                "-N",
                "set-option",
                "-w",
                "-t",
                session,
                "remain-on-exit",
                "on",
            ],
            &format!("remain-on-exit on is set on {session}"),
        )
        .await;

        let denied = submit_typed(&host, session, &job_id, &[line, "Enter"], line).await;
        assert_eq!(denied.exit_code(), Some(0), "{line:?} exits zero natively");
        let Err(error) = denied.result.as_ref() else {
            panic!("{line:?} is refused rather than published: {denied:?}");
        };
        assert!(
            matches!(error.kind(), marsh::ShellErrorKind::Denied { .. }),
            "{line:?} is a policy denial: {error}"
        );
        assert_eq!(
            published(&host, "report-owned"),
            "owner",
            "the seed keeps the owning shell's bytes"
        );
        // The typed line never contains this, so an echoed command cannot satisfy it.
        let rendered = error.to_string();
        let header = format!(
            "{}: {}",
            denied.shell.id.reference(),
            rendered.lines().next().unwrap_or_default()
        );

        if !closes_itself {
            poll(async || {
                let screen = capture(session).await;
                match screen.matches(&header).count() {
                    0 => Err(format!("{session} never showed {header:?}: {screen:?}")),
                    1 => Ok(()),
                    _ => panic!("{session} showed {header:?} more than once: {screen:?}"),
                }
            })
            .await;
            assert!(
                host.job_ids().contains(&job_id),
                "{session}'s shell outlives the verdict its prompt showed"
            );
            // End of input on the empty prompt: no second command, so the denied line stays the
            // job's last completion when retirement looks at it.
            host.succeeds(
                &["-N", "send-keys", "-t", session, "--", "C-d"],
                &format!("end of input is typed at {session}"),
            )
            .await;
        }

        let end = tokio::time::timeout(TIMEOUT, shell.wait_closed())
            .await
            .unwrap_or_else(|_| panic!("{session}'s shell never closed"))
            .expect("the closure resolves");
        // A denied line with a zero native exit is a failed pane; a stop with no verdict is not.
        let (verdict, status) = if closes_itself {
            (Some(denied.id), "1:1")
        } else {
            (None, "1:0")
        };
        assert_eq!(
            end.completion.as_ref().map(|completion| completion.id),
            verdict,
            "{session}'s closure carries the verdict retirement may render"
        );
        let dead = poll(async || {
            let shown = host
                .run_cli(
                    &[
                        "-N",
                        "display-message",
                        "-p",
                        "-t",
                        session,
                        "#{pane_dead}:#{pane_dead_status}",
                    ],
                    b"",
                )
                .await;
            let text = String::from_utf8_lossy(&shown.stdout).trim_end().to_owned();
            if shown.code == 0 && text.starts_with("1:") && !host.job_ids().contains(&job_id) {
                return Ok(text);
            }
            Err(format!(
                "{session}'s pane never died with its job retired (exit {}): {text:?} / {}",
                shown.code, shown.stderr
            ))
        })
        .await;
        assert_eq!(
            dead, status,
            "{session}'s pane status is its closing verdict's gated status"
        );

        let screen = capture(session).await;
        assert_eq!(
            screen.matches(&header).count(),
            1,
            "{session}'s retained history shows the verdict exactly once: {screen:?}"
        );
        assert_eq!(
            published(&host, "report-owned"),
            "owner",
            "the seed keeps the owning shell's bytes"
        );
    }

    host.shutdown().await;
}

/// The pane every pipe-close test pipes.
const PIPE_TARGET: &str = "pipe-close:0.0";

/// How long a pipe-close test holds its command in a phase that is not execution.
///
/// Longer than both of the close's escalation windows together, so a close that timed setup or
/// publication as though it were a consumer ignoring end of file would already have forced it.
const HOLD: Duration = Duration::from_secs(1);

/// Opens the `pipe-close` session and waits for its pane's own command to print.
///
/// The pane's view is prepared by then, so the next snapshot a gate catches is the pipe's.
async fn open_piped_pane(host: &Host) {
    host.succeeds(
        &[
            "-N",
            "new-session",
            "-d",
            "-s",
            "pipe-close",
            "printf '%s%s\\n' PIPE_ READY; cat >/dev/null",
        ],
        "the piped pane's session is created",
    )
    .await;
    poll(async || {
        let pane = host
            .run_cli(&["-N", "capture-pane", "-p", "-t", "pipe-close"], b"")
            .await;
        if String::from_utf8_lossy(&pane.stdout).contains("PIPE_READY") {
            return Ok(());
        }
        Err(format!(
            "the piped pane never reached its marker (exit {}): {:?} / {}",
            pane.code,
            String::from_utf8_lossy(&pane.stdout),
            pane.stderr
        ))
    })
    .await;
}

/// `text` as one shell word.
fn sh_word(text: &str) -> String {
    quote_if_needed(text, QuoteMode::SingleQuote).into_owned()
}

/// `script` run by `/bin/sh`, as the one command string `pipe-pane` takes.
fn sh_command(script: &str) -> String {
    format!("/bin/sh -c {}", sh_word(script))
}

/// A pipe command that logs into the seed, acknowledges at `ready` outside it, then reads its
/// input to end of file.
fn cooperative_logger(ready: &Path) -> String {
    sh_command(&format!(
        "printf kept > pipe-close-log; printf ready > {}; cat >/dev/null",
        sh_word(&ready.to_string_lossy())
    ))
}

/// Opens `command` as the pane's pipe and returns the receipt of the command it admitted, picked
/// out by the seed file only its text names.
async fn open_pipe(host: &Host, command: &str, log: &str) -> CommandHandle {
    host.succeeds(
        &["-N", "pipe-pane", "-O", "-t", PIPE_TARGET, command],
        "the pipe opens",
    )
    .await;
    host.io
        .snapshot()
        .state
        .commands
        .into_iter()
        .find(|command| command.text().contains(log))
        .unwrap_or_else(|| panic!("the pipe command writing {log} is admitted"))
}

/// Closes the pane's pipe through an explicitly empty command.
async fn close_pipe(host: &Host) -> CliOutcome {
    host.run_cli(&["-N", "pipe-pane", "-t", PIPE_TARGET, ""], b"")
        .await
}

/// Drives `close` alongside `work`, failing if the close finishes first; `when` names the phase
/// the command was supposed to be held in.
async fn while_closing<T>(
    close: Pin<&mut impl Future<Output = CliOutcome>>,
    work: impl Future<Output = T>,
    when: &str,
) -> T {
    tokio::select! {
        biased;
        closed = close => panic!(
            "the close finished {when} (exit {}): {}",
            closed.code, closed.stderr
        ),
        value = work => value,
    }
}

/// Waits for an armed storage gate to be reached.
async fn gate_reached(reached: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(TIMEOUT, reached)
        .await
        .expect("the pipe command reaches the storage gate")
        .expect("the storage gate is not dropped before it is reached");
}

/// Closes the pane's pipe while its command is held at an armed storage gate, releasing the gate
/// only after [`HOLD`]; `phase` names what the gate holds.
///
/// The close is polled throughout and must not finish while the gate is held. The pane reports
/// its pipe gone as soon as the close is requested, because the registered pipe is removed before
/// its verdict is awaited — and nothing is in the seed yet, because nothing was approved.
async fn close_while_held(
    host: &Host,
    reached: tokio::sync::oneshot::Receiver<()>,
    resume: std::sync::mpsc::Sender<()>,
    phase: &str,
) -> CliOutcome {
    let held = format!("while {phase} was held");
    let close = close_pipe(host);
    tokio::pin!(close);
    while_closing(
        close.as_mut(),
        gate_reached(reached),
        &format!("before {phase} was reached"),
    )
    .await;
    assert!(
        !host.seed("pipe-close-log").exists(),
        "nothing is published {held}"
    );
    while_closing(close.as_mut(), await_pane_pipe(host, "0"), &held).await;
    while_closing(close.as_mut(), tokio::time::sleep(HOLD), &held).await;
    let _ = resume.send(());
    close.await
}

/// Waits for the pane to report `expected` as its `#{pane_pipe}` flag.
async fn await_pane_pipe(host: &Host, expected: &str) {
    poll(async || {
        let shown = host
            .run_cli(
                &["-N", "display-message", "-p", "-t", PIPE_TARGET, "#{pane_pipe}"],
                b"",
            )
            .await;
        let text = String::from_utf8_lossy(&shown.stdout).trim_end().to_owned();
        if shown.code == 0 && text == expected {
            return Ok(());
        }
        Err(format!(
            "the pane never reported pane_pipe={expected} (exit {}): {text:?} / {}",
            shown.code, shown.stderr
        ))
    })
    .await;
}

/// Waits for a pipe command's acknowledgement at `ready`.
async fn await_ready(ready: &Path) {
    poll(async || match std::fs::read(ready) {
        Ok(bytes) if bytes == b"ready" => Ok(()),
        other => Err(format!(
            "the pipe command never acknowledged at {}: {other:?}",
            ready.display()
        )),
    })
    .await;
}

/// The verdict `receipt` retains.
async fn verdict(receipt: &CommandHandle) -> Arc<CommandCompletion> {
    tokio::time::timeout(TIMEOUT, receipt.wait())
        .await
        .expect("the pipe command reaches a verdict")
        .expect("the pipe command concludes")
}

/// Requires `completion` to have been interrupted, which discards whatever it staged.
fn assert_interrupted(completion: &CommandCompletion) {
    assert!(
        !completion.is_published()
            && matches!(
                completion.result.as_ref(),
                Err(error) if matches!(error.kind(), ShellErrorKind::Interrupted)
            ),
        "the pipe command is interrupted and unpublished: {completion:?}"
    );
}

/// Closing a pipe whose command is still being prepared waits for it to run, not for a timer.
///
/// End of file bounds a command that is *executing*; one that has not started cannot have
/// ignored it. The writable view the pipe's shell takes before its first command runs is held
/// past both escalation windows, and the close still has to end in that command reading to its
/// end of file and publishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pipe_pane_close_waits_for_preparation() {
    let fs = Arc::new(PausingCopyTree::default());
    let host = Host::with(Arc::clone(&fs) as Arc<dyn Subvolumes>).await;
    open_piped_pane(&host).await;
    let ready = host.scratch.path().join("pipe-ready");

    // Armed once the pane's own view exists, so it catches the pipe shell's.
    let (reached, resume) = fs.pause_next_snapshot();
    let receipt = open_pipe(&host, &cooperative_logger(&ready), "pipe-close-log").await;

    let closed = close_while_held(&host, reached, resume, "the pipe's view").await;
    assert_eq!(
        closed.code, 0,
        "the close reports the pipe's approved verdict: {}",
        closed.stderr
    );
    assert_published(&*verdict(&receipt).await, receipt.text());
    assert_eq!(
        std::fs::read(host.seed("pipe-close-log")).ok().as_deref(),
        Some(b"kept".as_slice()),
        "the pipe command's log is published"
    );
    await_pane_pipe(&host, "0").await;
    host.shutdown().await;
}

/// Closing a pipe whose command is being published waits for the verdict, not for a timer.
///
/// The command has already written its log and read to end of file; what is held is the
/// read-only freeze of its view that precedes the seal. That work is the command's approval
/// being processed, not a consumer ignoring its input, and forcing it would discard a log the
/// command finished writing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pipe_pane_close_waits_for_publication() {
    let fs = Arc::new(PausingCopyTree::default());
    let host = Host::with(Arc::clone(&fs) as Arc<dyn Subvolumes>).await;
    open_piped_pane(&host).await;
    let ready = host.scratch.path().join("pipe-ready");
    let receipt = open_pipe(&host, &cooperative_logger(&ready), "pipe-close-log").await;
    await_ready(&ready).await;

    // The command's baseline was frozen before it could acknowledge, so the next read-only
    // snapshot is its publication's.
    let (reached, resume) = fs.pause_next_readonly();
    let closed = close_while_held(&host, reached, resume, "the pipe's publication").await;
    assert_eq!(
        closed.code, 0,
        "the close reports the pipe's approved verdict: {}",
        closed.stderr
    );
    assert_published(&*verdict(&receipt).await, receipt.text());
    assert_eq!(
        std::fs::read(host.seed("pipe-close-log")).ok().as_deref(),
        Some(b"kept".as_slice()),
        "the pipe command's log is published"
    );
    await_pane_pipe(&host, "0").await;
    host.shutdown().await;
}

/// A pipe command that ignores both end of file and `SIGTERM` is still forced, and discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn pipe_pane_close_forces_an_uncooperative_command() {
    let host = Host::with(Arc::new(PausingCopyTree::default())).await;
    open_piped_pane(&host).await;
    let ready = host.scratch.path().join("pipe-ready");
    let stubborn = sh_command(&format!(
        "trap '' TERM; printf private > stubborn-pipe-log; printf ready > {}; \
         while :; do /bin/sleep 60; done",
        sh_word(&ready.to_string_lossy())
    ));
    let receipt = open_pipe(&host, &stubborn, "stubborn-pipe-log").await;
    await_ready(&ready).await;

    let closed = close_pipe(&host).await;
    assert_ne!(
        closed.code, 0,
        "a forced close is reported as a failure: {}",
        String::from_utf8_lossy(&closed.stdout)
    );
    assert_interrupted(&*verdict(&receipt).await);
    assert!(
        !host.seed("stubborn-pipe-log").exists(),
        "nothing the forced command staged is published"
    );
    await_pane_pipe(&host, "0").await;
    host.shutdown().await;
}

/// An explicit forced stop still beats a publication that has not been sealed.
///
/// Closing a pipe no longer times finalization, but teardown keeps its right to discard it: a
/// force accepted while the publication's freeze is held must end in an interrupted command and
/// no log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn forced_stop_still_discards_pipe_publication() {
    let fs = Arc::new(PausingCopyTree::default());
    let host = Host::with(Arc::clone(&fs) as Arc<dyn Subvolumes>).await;
    open_piped_pane(&host).await;
    let ready = host.scratch.path().join("pipe-ready");
    let receipt = open_pipe(&host, &cooperative_logger(&ready), "pipe-close-log").await;
    // Resolved once: a replacement generation under the same name is not this command's shell.
    let logger = host
        .io
        .shell(&receipt.shell().id)
        .expect("the pipe command's shell is visible");
    await_ready(&ready).await;

    let (reached, resume) = fs.pause_next_readonly();
    let closed = {
        let close = close_pipe(&host);
        tokio::pin!(close);
        while_closing(
            close.as_mut(),
            gate_reached(reached),
            "before the pipe's publication was frozen",
        )
        .await;

        // Polled to its first suspension on this, its bound runtime: the cancellation is decided
        // inline before the stop waits for the held command to let go of its shell.
        let stop = host.io.stop(&logger, true);
        tokio::pin!(stop);
        let first = std::future::poll_fn(|context| Poll::Ready(stop.as_mut().poll(context))).await;
        let accepted = first.is_pending();
        let visible = host.io.job(&receipt.shell().id).is_some();
        // Released before anything that could unwind, so no storage worker is left parked.
        let _ = resume.send(());
        drop(resume);
        assert!(
            accepted,
            "the forced stop waits for the held publication: {first:?}"
        );
        assert!(
            !visible,
            "the forced stop was accepted while the publication was held"
        );

        stop.await.expect("the forced stop succeeds");
        close.await
    };
    assert_ne!(
        closed.code, 0,
        "a discarded close is reported as a failure: {}",
        String::from_utf8_lossy(&closed.stdout)
    );
    assert_interrupted(&*verdict(&receipt).await);
    assert!(
        !host.seed("pipe-close-log").exists(),
        "the discarded command's log is not published"
    );
    await_pane_pipe(&host, "0").await;
    host.shutdown().await;
}
