//! A throwaway public-API smoke driver: `cargo run -p marsh --example rmux_smoke`.
//!
//! Where `rmux_api.rs` is the documented library sample, this one is the wider behavioural
//! driver: it opens a frontend that leases nothing, lets its shells discover their own seeds, and
//! drives the running system through nothing but that frontend and the handles it hands out.
//! There is no raw mux here and no accessor for one; every observation below is one an embedding
//! application can make for itself.
//!
//! What it proves, in order:
//!
//! 1. A native shell named `api` is created through `io`, *retrieved again by name* with
//!    `io.shell`, run through, selected and stopped. Creation and execution are two separate
//!    calls, running a line resolves at its *completion*, and its verdict is inspected as a
//!    typed one — the exit code and the publication outcome read separately, because a zero exit
//!    is not an approval.
//! 2. The **same** daemon is reachable from outside: while that shell is live, the real `rmux -S
//!    <socket>` executable captures the pane presenting it, and after a native `io.switch` that
//!    client reports this shell's own pane *and* window active. One daemon, two front doors.
//! 3. A second shell's overwrite exits zero and is denied through `ShellErrorKind`, with its
//!    native status retained and the source bytes unchanged.
//! 4. A real pipe execution carries binary stdout and stderr independently and byte for byte, and
//!    its standard input really ends — the program produces nothing at all until it has read to
//!    end of file, so any output is proof the half-close arrived.
//! 5. **A directory selects a seed, and a host owns only a default.** The real CLI opens a pane
//!    with `new-session -c <sibling seed>/src`, a seed this daemon was never told about. That
//!    pane publishes into the sibling while the `api` job goes on publishing into the first, in
//!    the same host, at the same time; neither seed sees the other's bytes. The real CLI's
//!    `new-window -d -t other-seed -c <sibling seed>/src` then opens a detached window beside
//!    that pane whose shell is on the *sibling* seed, while the pane it opened beside stays the
//!    active one.
//! 6. **A typed line reaches the shell as typed, and a typed `exit` ends exactly its own pane.** A
//!    function named `fg`, once the prompt's own word, runs with its argument expanded unsplit.
//!    Then `exit 7` typed into the same prompt closes that job with status 7 through brush's own
//!    builtin, its pane and session leave the client's pane list, and the session beside it is
//!    still there in the same listing.
//!
//! Between the outside capture and the switch, while `api` is live, it also proves the parametric
//! extractions end to end: one protocol connection opens three raw and two surface pane streams
//! on `api`'s pane — the fourth answered from a ready raw cache — and every one of them delivers a
//! marker printed afterwards. The shared bounded-key, weighted-retention and occurrence-map
//! primitives give their exact answers through their public API, and a write-ahead log over plain
//! directories publishes, reconciles an outside deletion, clears a finished staging residue and
//! removes a link and an empty directory without following or recursing.
//!
//! The fixture is a pair of sibling btrfs subvolumes under `$HOME`. If the first cannot be
//! created the driver falls back to [`CopyTree`](marsh_btrfs::fake::CopyTree) through the explicit
//! test-support factory and says so on stdout — in which case this run proves API and CLI
//! behaviour above, and not of kernel btrfs behaviour. That changes the fixture, never the
//! shipped behaviour: nothing here adds a fake mode to the product.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marsh::rmux::types::protocol::{
    PaneOutputSubscriptionId, PaneRawRebase, PaneStreamCursorRequest, PaneStreamEvent,
    PaneStreamMode, PaneSurfaceFrame, PaneTarget, PaneTargetRef, Request, Response,
    SubscribePaneStreamRequest, SubscribePaneStreamResponse, TerminalSize,
    UnsubscribePaneStreamRequest,
};
use marsh::rmux::types::{
    ClientError, CommandCompletion, CommandOptions, Connection, DaemonConfig, EnsureSession, JobIo,
    ProcessCommand, RunError, SessionName, ShellEnvironment, ShellErrorKind, ShellId, SpawnOptions,
    TerminalGeometry,
};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, IoError, OutputLimit, OverflowPolicy, RmuxFrontend, ShellHandle,
};
use marsh_btrfs::Subvolumes;
use marsh_lib::{BoundedRetention, FifoSet, extend_occurrence_map, group_keys_by_value};
use marsh_wal::{CommitOp, JsonLog, Mode, Seq, SourceUid, Staging, WalRecord};
use rmux_core::{GridRenderOptions, ScreenCaptureRange, TerminalScreen};
use tokio::io::AsyncReadExt;

/// How long any single observation may take before the driver declares it unmet.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Any failure at all: this is a driver, and the first thing that does not hold ends it.
///
/// `Send` so that a failure held across the teardown that follows it keeps the future sendable.
type Failure = Box<dyn std::error::Error + Send + Sync>;

/// `assert!` for a proof that returns its failure: the same condition and message, as an error.
macro_rules! ensure {
    ($condition:expr $(,)?) => {
        if !$condition {
            return Err(concat!("assertion failed: ", stringify!($condition)).into());
        }
    };
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(format!($($message)+).into());
        }
    };
}

/// `assert_eq!` for a proof that returns its failure: the same comparison and message, as an error.
macro_rules! ensure_eq {
    ($left:expr, $right:expr $(,)?) => {{
        let (left, right) = (&$left, &$right);
        if *left != *right {
            return Err(
                format!("assertion `left == right` failed\n  left: {left:?}\n right: {right:?}").into(),
            );
        }
    }};
    ($left:expr, $right:expr, $($message:tt)+) => {{
        let (left, right) = (&$left, &$right);
        if *left != *right {
            let message = format!($($message)+);
            return Err(format!(
                "assertion `left == right` failed: {message}\n  left: {left:?}\n right: {right:?}"
            )
            .into());
        }
    }};
}

/// An observation's outcome together with the cleanup that followed it, neither hiding the other.
///
/// The observation's own failure comes first when both failed: it is the diagnosis, and the
/// cleanup failure is what it additionally left behind.
fn conclude<T>(result: Result<T, Failure>, cleanup: Result<(), Failure>) -> Result<T, Failure> {
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(original), Err(cleanup)) => {
            Err(format!("{original}; cleanup failed: {cleanup}").into())
        }
    }
}

/// The line the stream proof waits for. `printf` assembles it, so the echoed command line —
/// `parametric-%s` — can never satisfy the observation.
const STREAM_MARKER: &str = "parametric-streams";

/// The stream proof's five subscriptions, `(mode, include_snapshot)`, in request order.
///
/// The first raw request initializes a keyframe-only cache and the third finds that cache
/// insufficient for its snapshot, so it initializes a richer one. Only the fourth — keyframe-only
/// again, with no pane input since — is answered from the ready cache, its snapshot stripped: the
/// first two raw requests alone would never exercise the ready-payload path.
const STREAM_REQUESTS: [(PaneStreamMode, bool); 5] = [
    (PaneStreamMode::Raw, false),
    (PaneStreamMode::Surface, false),
    (PaneStreamMode::Raw, true),
    (PaneStreamMode::Raw, false),
    (PaneStreamMode::Surface, false),
];

/// The seeds this run publishes into, and how they were made.
struct Fixture {
    /// The private scratch root, owned until [`Self::cleanup`]. The socket lives here, *outside*
    /// either seed.
    scratch: tempfile::TempDir,
    /// The first seed's root, which is also the host's default directory.
    seed: PathBuf,
    /// A sibling seed, holding `src/`, that nothing but a pane's own `-c` argument names.
    other: PathBuf,
    /// The backend both seeds were made through, and every subvolume of this run is deleted by.
    filesystem: Arc<dyn Subvolumes>,
    /// Real btrfs subvolumes, or the copy-tree fallback.
    real_btrfs: bool,
}

impl Fixture {
    /// Creates two sibling seeds under `$HOME`, on real btrfs or on a copy tree.
    ///
    /// Only *creating the first subvolume* decides which backend this returns. A seed that was
    /// made and then could not be opened is a real failure, not a reason to quietly swap in a
    /// fixture and report success against something the product never ships. The sibling is then
    /// made through whichever backend won, so nothing below can tell the two apart: the driver's
    /// claim is that a *directory* selects a seed, and two seeds of different kinds would let a
    /// backend quirk stand in for that.
    ///
    /// The scratch root is owned before either seed is attempted, and a failure after that
    /// reclaims whatever was made before it is returned.
    fn new() -> Result<Self, Failure> {
        let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
        let scratch = tempfile::Builder::new()
            .prefix("marsh-rmux-smoke.")
            .tempdir_in(home)?;
        let seed = scratch.path().join("seed");
        let other = scratch.path().join("other");

        let (filesystem, unavailable): (Arc<dyn Subvolumes>, _) =
            match marsh_btrfs::LibBtrfs.create_subvolume(&seed) {
                Ok(()) => (Arc::new(marsh_btrfs::LibBtrfs), None),
                Err(error) => (Arc::new(marsh_btrfs::fake::CopyTree::new()), Some(error)),
            };
        let fixture = Self {
            scratch,
            seed,
            other,
            filesystem,
            real_btrfs: unavailable.is_none(),
        };
        match fixture.initialize(unavailable) {
            Ok(()) => Ok(fixture),
            Err(error) => {
                let cleanup = fixture.cleanup();
                conclude(Err(error), cleanup)
            }
        }
    }

    /// Makes the rest of both seeds through the selected backend: the copy-tree seed in place of
    /// a failed btrfs one, then the sibling and its `src/`.
    fn initialize(&self, unavailable: Option<marsh_btrfs::Error>) -> Result<(), Failure> {
        if let Some(error) = unavailable {
            println!("  ! real btrfs seed unavailable ({error}); falling back to CopyTree");
            // Whatever the failed create left is inside this run's own scratch root, and a seed
            // that cannot be cleared is a failure rather than something to build on top of.
            marsh_btrfs::LibBtrfs
                .delete_subvolume(&self.seed)
                .map_err(|error| {
                    format!(
                        "the failed btrfs seed {} could not be removed: {error}",
                        self.seed.display()
                    )
                })?;
            self.filesystem.create_subvolume(&self.seed)?;
        }
        self.filesystem.create_subvolume(&self.other)?;
        std::fs::create_dir_all(self.other.join("src"))?;
        // Both seeds are Git work trees, so each is one policy root for sandbox routing.
        git2::Repository::init(&self.seed)?;
        git2::Repository::init(&self.other)?;
        Ok(())
    }

    /// A path inside the first seed, so the driver can read back what was published.
    fn seed(&self, path: &str) -> PathBuf {
        self.seed.join(path)
    }

    /// A path inside the sibling seed.
    fn other(&self, path: &str) -> PathBuf {
        self.other.join(path)
    }

    /// The daemon's socket, outside both seeds so it is never part of what is published.
    fn socket(&self) -> PathBuf {
        self.scratch.path().join("rmux.sock")
    }

    /// Reclaims every subvolume this run made, then the scratch root.
    ///
    /// Job snapshots are subvolumes too, and `remove_dir_all` cannot delete one, so they go
    /// first — for both seeds, since either may have hosted jobs. Every owned path is attempted
    /// even after an earlier one failed, and every failure is reported.
    fn cleanup(self) -> Result<(), Failure> {
        let Self {
            scratch,
            seed,
            other,
            filesystem,
            ..
        } = self;
        let mut outcome = Ok(());
        for root in [&seed, &other] {
            let reclaimed = reclaim_seed(scratch.path(), root, filesystem.as_ref());
            outcome = conclude(outcome, reclaimed);
        }
        let root = scratch.keep();
        let removed = std::fs::remove_dir_all(&root).map_err(|error| {
            Failure::from(format!(
                "the scratch root {} remains: {error}",
                root.display()
            ))
        });
        conclude(outcome, removed)
    }
}

/// Deletes every job snapshot recorded for `seed` under `scratch`'s state directory, then `seed`
/// itself, all through `filesystem`; an absent snapshot directory is one already cleaned.
fn reclaim_seed(scratch: &Path, seed: &Path, filesystem: &dyn Subvolumes) -> Result<(), Failure> {
    let name = seed
        .file_name()
        .ok_or_else(|| format!("the seed {} has no name", seed.display()))?;
    let snaps = scratch.join(marsh_btrfs::STATE_DIR).join(name).join("snap");
    let unreadable = |error: std::io::Error| {
        Failure::from(std::io::Error::new(
            error.kind(),
            format!("reading {}: {error}", snaps.display()),
        ))
    };
    let delete = |path: &Path| {
        filesystem
            .delete_subvolume(path)
            .map_err(|error| Failure::from(format!("deleting {}: {error}", path.display())))
    };
    let mut outcome = Ok(());
    match std::fs::read_dir(&snaps) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => outcome = Err(unreadable(error)),
        Ok(entries) => {
            for entry in entries {
                let deleted = entry
                    .map_err(unreadable)
                    .and_then(|entry| delete(&entry.path()));
                outcome = conclude(outcome, deleted);
            }
        }
    }
    conclude(outcome, delete(seed))
}

/// The real `rmux` executable built beside this example.
///
/// `CARGO_BIN_EXE_*` is only set for tests and benchmarks, so the path is derived from this
/// example's own location: `target/<profile>/examples/rmux_smoke` → `target/<profile>/rmux`.
fn rmux_binary() -> Result<PathBuf, Failure> {
    let me = std::env::current_exe()?;
    let binary = me
        .parent()
        .and_then(Path::parent)
        .ok_or("the example is not inside a target directory")?
        .join("rmux");
    if !binary.is_file() {
        return Err(format!(
            "{} does not exist; run `cargo build -p marsh --bins` first",
            binary.display()
        )
        .into());
    }
    Ok(binary)
}

/// One `rmux -N -S <socket> …` client invocation, returning its standard output.
///
/// `-N` is the load-bearing flag: the client must not start a daemon of its own, so anything it
/// answers came from the daemon *this* process is hosting.
async fn rmux(binary: &Path, socket: &Path, args: &[&str]) -> Result<String, Failure> {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("-N")
        .arg("-S")
        .arg(socket)
        .args(args)
        .env("TERM", "xterm-256color");
    let output = command_output(&mut command, &format!("rmux {args:?}"), TIMEOUT).await?;
    if !output.status.success() {
        return Err(format!(
            "rmux {args:?} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `target`'s expansion of `format`, as the real client reports it, without surrounding space.
async fn display(
    binary: &Path,
    socket: &Path,
    target: &str,
    format: &str,
) -> Result<String, Failure> {
    let text = rmux(
        binary,
        socket,
        &["display-message", "-p", "-t", target, format],
    )
    .await?;
    Ok(text.trim().to_owned())
}

/// Types `keys` into `target` through the real client, returning once `marker` is next printed.
async fn send_keys_until(
    binary: &Path,
    socket: &Path,
    target: &str,
    marker: &str,
    keys: &[&str],
) -> Result<(), Failure> {
    let mut args = vec![
        "send-keys",
        "-t",
        target,
        "--wait-next-text",
        marker,
        "--timeout",
        "5s",
        "--",
    ];
    args.extend_from_slice(keys);
    rmux(binary, socket, &args).await?;
    Ok(())
}

/// Runs `command` to completion within `limit`, with no standard input, returning its status
/// and both output streams whatever the status: whether a nonzero exit is expected is the
/// caller's to decide.
///
/// The child is owned here on every path. A child still running when the deadline passes or
/// its output cannot be read is killed through its retained handle and then reaped, so it never
/// outlives this call; `kill_on_drop` only backs that up should this future itself be dropped.
async fn command_output(
    command: &mut tokio::process::Command,
    what: &str,
    limit: Duration,
) -> Result<Output, Failure> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| std::io::Error::new(error.kind(), format!("starting {what}: {error}")))?;
    let captured = capture(&mut child, what, limit).await;
    let terminated = if captured.is_ok() {
        Ok(())
    } else {
        terminate(&mut child, what).await
    };
    conclude(captured, terminated)
}

/// Awaits `child`'s exit while draining both of its pipes concurrently, all within `limit`.
async fn capture(
    child: &mut tokio::process::Child,
    what: &str,
    limit: Duration,
) -> Result<Output, Failure> {
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| format!("{what} has no stdout pipe"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| format!("{what} has no stderr pipe"))?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (status, _, _) = tokio::time::timeout(limit, async {
        tokio::try_join!(
            child.wait(),
            stdout_pipe.read_to_end(&mut stdout),
            stderr_pipe.read_to_end(&mut stderr),
        )
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{what} did not finish within {limit:?}"),
        )
    })?
    .map_err(|error| std::io::Error::new(error.kind(), format!("{what}: {error}")))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Kills `child` unless it has already exited, and reaps it either way.
///
/// The wait is attempted even when signalling failed: a child that exited in between is still
/// one to reap, and a failure of either step is reported.
async fn terminate(child: &mut tokio::process::Child, what: &str) -> Result<(), Failure> {
    let signalled = match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => child.start_kill(),
        Err(error) => Err(error),
    }
    .map_err(|error| Failure::from(format!("killing {what}: {error}")));
    let reaped = child
        .wait()
        .await
        .map(drop)
        .map_err(|error| Failure::from(format!("reaping {what}: {error}")));
    conclude(signalled, reaped)
}

/// Polls a fallible observation until it holds, naming `what` if it never does.
async fn until<T, F, Fut>(what: &str, mut attempt: F) -> Result<T, Failure>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = attempt().await {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How a completion reads to a caller that keeps its two answers apart.
fn describe(completion: &CommandCompletion) -> String {
    let verdict = match completion.result.as_ref() {
        Ok(_) => "accepted".to_owned(),
        Err(error) => error.to_string(),
    };
    format!("exit={:?} {verdict}", completion.exit_code())
}

/// Runs one line in an open shell and waits for its verdict.
///
/// `Ok` is the gate's approval and nothing less: a denial, a discard or a lost answer all arrive
/// as an error, which is what the approved paths below want — the first
/// thing that does not hold ends the driver. The one path that *expects* a refusal reads the
/// native error itself instead of coming through here.
async fn submit(job: &ShellHandle, line: &str) -> Result<Arc<CommandCompletion>, Failure> {
    Ok(tokio::time::timeout(TIMEOUT, job.run_command(line, CommandOptions::default())).await??)
}

/// The `session:window.pane` spelling of the surface presenting one shell.
///
/// Resolved through the facade rather than guessed: a client observation that timed out because
/// the target was misspelled would look exactly like a pane that never answered.
async fn pane_target(io: &RmuxFrontend, job: &ShellHandle) -> Result<String, Failure> {
    let pane = io.pane_for(job).await?;
    let reference = pane.target();
    Ok(format!(
        "{}:{}.{}",
        reference.session_name.as_str(),
        reference.window_index,
        reference.pane_index
    ))
}

/// Opens the session and the `api` job, and publishes the file the rest of the run is about.
async fn open_api(io: &RmuxFrontend, fixture: &Fixture) -> Result<ShellHandle, Failure> {
    let session = io
        .new_session(
            EnsureSession::named(SessionName::new("smoke")?)
                .detached(true)
                .create_only(),
        )
        .await?;
    println!("[1] io.new_session -> {}", session.name().as_str());

    let id = ShellId::from("api");
    // Creation, and nothing else: opening a shell runs no line at all, which is why the workload
    // below is a separate call rather than a fourth argument here.
    let created = io
        .open_shell(
            Path::new(""),
            Some(id.clone()),
            SpawnOptions {
                io: JobIo::Terminal {
                    geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
                },
                ..SpawnOptions::default()
            },
        )
        .await?;
    let jobs = io.jobs();
    let names: Vec<&str> = jobs.iter().map(|view| view.id.as_str()).collect();
    println!("[2] io.open_shell(\"api\") -> jobs {names:?}");
    ensure_eq!(
        names.iter().filter(|name| **name == "api").count(),
        1,
        "`api` appears exactly once: {names:?}"
    );

    // The other half of the two-step API: a name is resolved once, and everything after this runs
    // through the retrieved object. It is the generation that was just created rather than a
    // second shell that happens to answer to the same name, which is what the uid below says.
    let job = io.shell(&id)?;
    ensure_eq!(
        job.sandbox().uid,
        created.sandbox().uid,
        "`io.shell` retrieves the shell that was created, not another one"
    );

    // The marker is assembled from two pieces on purpose: seeing the echoed command line on the
    // pane is not the same as seeing the command's output.
    let published = submit(&job, "printf owner > owned; printf '%s%s\\n' RMUX_ API").await?;
    println!("[3] `printf owner > owned` -> {}", describe(&published));
    ensure_eq!(published.exit_code(), Some(0), "the process said zero");
    ensure!(
        published.is_published(),
        "and the gate agreed: {}",
        describe(&published)
    );
    ensure_eq!(
        std::fs::read_to_string(fixture.seed("owned"))?,
        "owner",
        "the bytes in the seed are the bytes the line wrote"
    );
    Ok(job)
}

/// Reaches the very same daemon from outside, over the real executable.
async fn capture_from_outside(
    io: &RmuxFrontend,
    job: &ShellHandle,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let target = pane_target(io, job).await?;

    let sessions = rmux(binary, socket, &["list-sessions", "-F", "#{session_name}"]).await?;
    println!("[4] rmux -N -S … list-sessions -> {:?}", sessions.trim());
    ensure!(
        sessions.contains("smoke"),
        "the external client sees this process's session: {sessions:?}"
    );

    let screen = until("the pane to show RMUX_API", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target])
            .await
            .ok()?;
        text.lines()
            .any(|line| line.trim_end() == "RMUX_API")
            .then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[5] rmux -N -S … capture-pane -t {target} -> {shown:?}");
    Ok(())
}

/// Three raw and two surface pane streams on one protocol connection, each delivering a marker
/// printed only once all five are admitted.
///
/// The connection's I/O blocks, so one blocking worker owns it for the whole scenario and reports
/// back through a oneshot. This side submits the marker only after that readiness, and joins the
/// worker on every path: a running blocking task cannot be aborted, so none is ever abandoned.
async fn prove_parametric_streams(io: &RmuxFrontend, job: &ShellHandle) -> Result<(), Failure> {
    let pane = io.pane_for(job).await?;
    let slot = pane.target();
    let target = PaneTargetRef::slot(PaneTarget::with_window(
        slot.session_name.clone(),
        slot.window_index,
        slot.pane_index,
    ));
    let connection = io.open_protocol().await?;
    let (ready, admitted) = tokio::sync::oneshot::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let mut connection = connection;
        drive_streams(&mut connection, &target, ready)
    });

    match tokio::time::timeout(TIMEOUT, admitted).await {
        Ok(Ok(())) => {}
        // The worker dropped its sender: it ended before admitting all five, and says why.
        Ok(Err(_)) => {
            worker.await??;
            return Err("the stream worker ended without admitting its subscriptions".into());
        }
        // The receiver is gone with the timed-out wait, so a worker still admitting learns that
        // nobody will submit the marker; its own deadlines bound it either way.
        Err(_) => {
            let outcome = match worker.await? {
                Ok(()) => "the worker finished regardless".to_owned(),
                Err(error) => format!("the worker reported: {error}"),
            };
            return Err(format!(
                "timed out after {TIMEOUT:?} waiting for five pane stream subscriptions; {outcome}"
            )
            .into());
        }
    }
    // The worker is joined whether or not the submission succeeded, and a failed submission is
    // the answer then: without the marker the worker can only time out.
    let refused = submit(job, "printf 'parametric-%s\\n' streams")
        .await
        .err()
        .map(|error| error.to_string());
    let joined = worker.await;
    if let Some(error) = refused {
        return Err(error.into());
    }
    joined??;
    println!("[parametric] streams: 3 raw + 2 surface subscribers delivered parametric-streams");
    Ok(())
}

/// The stream proof's worker: owns the connection through three phases, each with a fresh
/// deadline — admitting the five subscriptions, seeing the marker reach every one of them, and
/// closing them.
fn drive_streams(
    connection: &mut Connection,
    target: &PaneTargetRef,
    ready: tokio::sync::oneshot::Sender<()>,
) -> std::io::Result<()> {
    let mut probes = admit_streams(connection, target)?;
    // Nobody left to submit the marker means nothing would ever arrive: fail now, and let the
    // dropped connection release the subscriptions.
    ready.send(()).map_err(|()| {
        std::io::Error::other("the marker's submitter was gone once all five streams were admitted")
    })?;
    await_marker(connection, &mut probes)?;
    close_streams(connection, &probes)
}

/// Opens [`STREAM_REQUESTS`] in order, checking each initial event against its row and that all
/// five resolved the same pane.
fn admit_streams(
    connection: &mut Connection,
    target: &PaneTargetRef,
) -> std::io::Result<Vec<StreamProbe>> {
    let deadline = Instant::now() + TIMEOUT;
    let mut probes = Vec::with_capacity(STREAM_REQUESTS.len());
    let mut pane = None;
    for (number, (mode, include_snapshot)) in (1_usize..).zip(STREAM_REQUESTS) {
        let operation =
            format!("subscribing stream {number} ({mode:?}, include_snapshot={include_snapshot})");
        let request = Request::SubscribePaneStream(SubscribePaneStreamRequest {
            target: target.clone(),
            mode,
            include_snapshot,
        });
        let response = match stream_roundtrip(connection, deadline, &operation, &request)? {
            Response::SubscribePaneStream(response) => *response,
            other => return Err(unexpected(&operation, &other)),
        };
        let first = *pane.get_or_insert(response.pane_id);
        if response.pane_id != first {
            return Err(std::io::Error::other(format!(
                "{operation} resolved pane {}, not {first}",
                response.pane_id
            )));
        }
        probes.push(StreamProbe::admit(
            number,
            &operation,
            mode,
            include_snapshot,
            response,
        )?);
    }
    Ok(probes)
}

/// Polls every subscription each round until the marker has reached all five, pausing at most
/// 25 ms between rounds. The readiness handshake, not this interval, is what orders the marker
/// after admission.
fn await_marker(connection: &mut Connection, probes: &mut [StreamProbe]) -> std::io::Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let pending: Vec<usize> = probes
            .iter()
            .filter(|probe| !probe.delivered)
            .map(|probe| probe.number)
            .collect();
        for probe in probes.iter_mut() {
            let operation = format!(
                "polling stream {} while streams {pending:?} still lack {STREAM_MARKER}",
                probe.number
            );
            let request = Request::PaneStreamCursor(PaneStreamCursorRequest {
                subscription_id: probe.id,
                max_events: Some(32),
            });
            match stream_roundtrip(connection, deadline, &operation, &request)? {
                Response::PaneStreamCursor(response) => probe.absorb(response.events)?,
                other => return Err(unexpected(&operation, &other)),
            }
        }
        if probes.iter().all(|probe| probe.delivered) {
            return Ok(());
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(25)),
        );
    }
}

/// Closes every subscription, each of which must still have been live.
fn close_streams(connection: &mut Connection, probes: &[StreamProbe]) -> std::io::Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    for probe in probes {
        let operation = format!("closing stream {}", probe.number);
        let request = Request::UnsubscribePaneStream(UnsubscribePaneStreamRequest {
            subscription_id: probe.id,
        });
        match stream_roundtrip(connection, deadline, &operation, &request)? {
            Response::UnsubscribePaneStream(response) if response.removed => {}
            Response::UnsubscribePaneStream(_) => {
                return Err(std::io::Error::other(format!(
                    "{operation}: it was no longer live"
                )));
            }
            other => return Err(unexpected(&operation, &other)),
        }
    }
    Ok(())
}

/// One request on the stream proof's connection, refused before it is sent once its phase's
/// `deadline` has passed.
///
/// `rmux-client` keeps the connection's socket timeouts to itself, so they cannot be narrowed to
/// the time remaining; a request sent just before the deadline is bounded by those fixed ones.
fn stream_roundtrip(
    connection: &mut Connection,
    deadline: Instant,
    operation: &str,
    request: &Request,
) -> std::io::Result<Response> {
    if Instant::now() >= deadline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out {operation}"),
        ));
    }
    connection.roundtrip(request).map_err(|error| match error {
        ClientError::Io(error) => {
            std::io::Error::new(error.kind(), format!("{operation}: {error}"))
        }
        error => std::io::Error::other(format!("{operation}: {error}")),
    })
}

/// A response of the wrong kind to `operation`, naming the daemon's refusal when it was one.
fn unexpected(operation: &str, response: &Response) -> std::io::Error {
    match response {
        Response::Error(refusal) => {
            std::io::Error::other(format!("{operation}: refused: {}", refusal.error))
        }
        other => std::io::Error::other(format!(
            "{operation}: unexpected {} response",
            other.command_name()
        )),
    }
}

/// One subscription of the stream proof, and what it has shown so far.
struct StreamProbe {
    /// Its row in [`STREAM_REQUESTS`], counting from one.
    number: usize,
    /// The daemon's id for it.
    id: PaneOutputSubscriptionId,
    /// A raw stream's reconstructed terminal; `None` for a surface stream.
    screen: Option<TerminalScreen>,
    /// Whether the marker has reached it.
    delivered: bool,
}

impl StreamProbe {
    /// The subscription `response` opened for row `number`, whose initial event must be the one
    /// that row requires: a raw rebase carrying a snapshot exactly when one was asked for, or a
    /// surface reset.
    fn admit(
        number: usize,
        operation: &str,
        mode: PaneStreamMode,
        include_snapshot: bool,
        response: SubscribePaneStreamResponse,
    ) -> std::io::Result<Self> {
        let screen = match (mode, response.event) {
            (PaneStreamMode::Raw, PaneStreamEvent::RawRebase(rebase))
                if rebase.snapshot.is_some() == include_snapshot =>
            {
                Some(rebuild(&rebase))
            }
            (PaneStreamMode::Surface, PaneStreamEvent::SurfaceReset(_)) => None,
            (_, event) => {
                return Err(std::io::Error::other(format!(
                    "{operation} opened with a {}",
                    event_kind(&event)
                )));
            }
        };
        Ok(Self {
            number,
            id: response.subscription_id,
            screen,
            delivered: false,
        })
    }

    /// Applies one batch of this stream's events in order, then records whether the marker has
    /// reached it. A raw stream resets its reconstruction on every rebase.
    fn absorb(&mut self, events: Vec<PaneStreamEvent>) -> std::io::Result<()> {
        for event in events {
            match (&mut self.screen, event) {
                (Some(screen), PaneStreamEvent::RawRebase(rebase)) => *screen = rebuild(&rebase),
                (Some(screen), PaneStreamEvent::RawBytes(bytes)) => screen.feed(&bytes.bytes),
                (
                    None,
                    PaneStreamEvent::SurfaceReset(frame) | PaneStreamEvent::SurfacePatch(frame),
                ) => self.delivered |= frame_shows_marker(&frame),
                (_, PaneStreamEvent::Lifecycle(_)) => {}
                (_, PaneStreamEvent::End(reason)) => {
                    return Err(std::io::Error::other(format!(
                        "stream {} ended: {reason:?}",
                        self.number
                    )));
                }
                (_, event) => {
                    return Err(std::io::Error::other(format!(
                        "stream {} delivered an unexpected {}",
                        self.number,
                        event_kind(&event)
                    )));
                }
            }
        }
        if let Some(screen) = &self.screen {
            self.delivered |= screen_shows_marker(screen);
        }
        Ok(())
    }
}

/// A terminal rebuilt from `rebase`'s keyframe alone, at the keyframe's size.
fn rebuild(rebase: &PaneRawRebase) -> TerminalScreen {
    let mut screen = TerminalScreen::new(
        TerminalSize {
            cols: rebase.cols,
            rows: rebase.rows,
        },
        100,
    );
    screen.feed(&rebase.keyframe);
    screen
}

/// Whether a reconstructed raw stream shows [`STREAM_MARKER`].
fn screen_shows_marker(screen: &TerminalScreen) -> bool {
    screen
        .screen()
        .capture_transcript(ScreenCaptureRange::default(), GridRenderOptions::default())
        .windows(STREAM_MARKER.len())
        .any(|window| window == STREAM_MARKER.as_bytes())
}

/// Whether a surface frame's cells, concatenated, show [`STREAM_MARKER`].
fn frame_shows_marker(frame: &PaneSurfaceFrame) -> bool {
    frame
        .snapshot
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .contains(STREAM_MARKER)
}

/// A short name for `event`, for a diagnostic that must not dump a whole keyframe.
fn event_kind(event: &PaneStreamEvent) -> &'static str {
    match event {
        PaneStreamEvent::RawRebase(rebase) if rebase.snapshot.is_some() => {
            "raw rebase with a snapshot"
        }
        PaneStreamEvent::RawRebase(_) => "raw rebase without a snapshot",
        PaneStreamEvent::RawBytes(_) => "raw byte event",
        PaneStreamEvent::SurfaceReset(_) => "surface reset",
        PaneStreamEvent::SurfacePatch(_) => "surface patch",
        PaneStreamEvent::Lifecycle(_) => "lifecycle event",
        PaneStreamEvent::End(_) => "end of stream",
        _ => "event this driver does not know",
    }
}

/// A second principal overwrites the first's file, exits zero, and is refused.
///
/// Deliberately not through [`submit`]: the refusal *is* the answer here, so it is read as the
/// native typed error rather than boxed into this driver's generic failure. The completion that
/// error carries is the whole verdict — the program's own exit code, and what the policy refused.
async fn prove_denial(io: &RmuxFrontend, fixture: &Fixture) -> Result<(), Failure> {
    let intruder = io
        .open_shell(
            Path::new(""),
            Some(ShellId::from("other")),
            SpawnOptions::default(),
        )
        .await?;
    let refused = tokio::time::timeout(
        TIMEOUT,
        intruder.run_command("printf other > owned; exit 0", CommandOptions::default()),
    )
    .await?;
    let completion = match refused {
        Err(IoError::Run(RunError::Execution { completion })) => completion,
        other => return Err(format!("a zero exit is not an approval: {other:?}").into()),
    };
    println!(
        "[7] `other` overwriting `api`'s file -> {}",
        describe(&completion)
    );
    ensure_eq!(
        completion.exit_code(),
        Some(0),
        "the process still said zero"
    );
    ensure!(
        matches!(completion.result.as_ref(), Err(error) if matches!(error.kind(), ShellErrorKind::Denied { denials } if !denials.is_empty())),
        "{}",
        describe(&completion)
    );
    ensure_eq!(
        std::fs::read_to_string(fixture.seed("owned"))?,
        "owner",
        "the seed still holds the first principal's bytes"
    );
    // The line ended with `exit 0`, so this shell closed itself: an explicit stop here would be a
    // stale-handle error, and waiting for the closure is what proves the refusal did not keep it
    // open. A denied publication is still a finished command.
    tokio::time::timeout(TIMEOUT, intruder.wait_closed()).await??;
    ensure!(
        io.job(intruder.id()).is_none(),
        "`other` exited, so its job is gone rather than idle"
    );
    Ok(())
}

/// One real pipe execution: two independent binary streams and a genuine end of file.
async fn prove_pipe(io: &RmuxFrontend) -> Result<(), Failure> {
    // The program writes nothing until `read()` returns, so any output at all proves stdin ended.
    let execution = io
        .execute(ExecutionSpec {
            initial_dir: PathBuf::new(),
            id: None,
            process: ProcessCommand::Argv(vec![
                "python3".to_owned(),
                "-c".to_owned(),
                "import sys\n\
                 data = sys.stdin.buffer.read()\n\
                 sys.stdout.buffer.write(data)\n\
                 sys.stderr.buffer.write(b'E\\x00R\\xfe')\n"
                    .to_owned(),
            ]),
            environment: None,
        })
        .await?;
    execution.input().write_all(b"a\0b\r\n\xff").await?;
    let captured = tokio::time::timeout(
        TIMEOUT,
        execution.collect(CollectOptions {
            limit: OutputLimit::Bytes(64 * 1024),
            overflow: OverflowPolicy::Error,
        }),
    )
    .await??;
    println!(
        "[8] pipe execution -> stdout={:?} stderr={:?} truncated={} {}",
        captured.stdout,
        captured.stderr,
        captured.truncated,
        describe(&captured.completion)
    );
    ensure_eq!(
        captured.stdout,
        b"a\0b\r\n\xff",
        "stdout is byte-identical: no line discipline, no CR insertion, no stop at the NUL"
    );
    ensure_eq!(
        captured.stderr,
        b"E\0R\xfe",
        "stderr is its own stream and is never merged into stdout"
    );
    ensure!(!captured.truncated);
    ensure_eq!(captured.completion.exit_code(), Some(0));
    Ok(())
}

/// Two seeds at once in one host: the API job on the first, a real CLI pane on the second.
///
/// Nothing about this daemon names the sibling seed. Its default directory is the first one, so
/// the pane's own `-c` argument is the *only* thing that selects the second — which is the claim
/// this proves: a host owns a default directory, and a shell owns a seed.
async fn prove_second_seed(
    io: &RmuxFrontend,
    fixture: &Fixture,
    api: &ShellHandle,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    // Canonical spellings, because that is how a job names the seed it discovered.
    let first = std::fs::canonicalize(&fixture.seed)?;
    let second = std::fs::canonicalize(&fixture.other)?;
    let start = fixture.other("src");
    let start = start.to_str().ok_or("the fixture path is not UTF-8")?;

    let wal = write_incompatible_wal(fixture)?;

    rmux(
        binary,
        socket,
        &["new-session", "-d", "-s", "other-seed", "-c", start],
    )
    .await?;

    // Found by its *seed* rather than by its name: what is being observed is that the CLI's `-c`
    // reached per-shell seed discovery, and a job view is where that answer becomes visible.
    let view = until("a job on the second seed", || async {
        io.jobs()
            .into_iter()
            .find(|view| view.sandbox.seed == second)
    })
    .await?;
    println!(
        "[9] rmux -N -S … new-session -c {start} -> `{}` on seed {}",
        view.id.as_str(),
        view.sandbox.seed.display()
    );
    ensure_eq!(
        view.sandbox.dir.as_str(),
        "src",
        "the pane starts where it was told to, relative to the seed it found"
    );
    let b = io.shell(&view.id)?;

    // The marker is assembled from two pieces for the same reason the first pane's is: an echoed
    // command line is not the command's output.
    let published = submit(&b, "printf second > marker; printf '%s%s\\n' second- pane").await?;
    println!(
        "[10] second seed `printf second > marker` -> {}",
        describe(&published)
    );
    ensure_eq!(published.exit_code(), Some(0));
    ensure!(
        published.is_published(),
        "the sibling seed publishes on its own: {}",
        describe(&published)
    );
    // Storage opens lazily at the pane's first admitted span rather than during construction, so
    // the incompatible log is observed only once a command on that seed has completed; by then
    // the log holds that command's own frames and none of the incompatible ones.
    let log = std::fs::read(&wal)?;
    ensure!(
        !log.windows(b"old-schema".len())
            .any(|window| window == b"old-schema"),
        "the pane's first admission replaced the incompatible log"
    );
    ensure_eq!(
        std::fs::read(second.join("src").join("marker"))?,
        b"second",
        "the bytes land in the seed the pane's directory chose"
    );
    ensure!(
        !first.join("marker").exists() && !first.join("src").join("marker").exists(),
        "and nowhere in the host's default seed"
    );

    // The CLI's detached session presents that job at its first pane, so this is the target the
    // client names.
    let target = pane_target(io, &b).await?;
    ensure_eq!(
        target,
        "other-seed:0.0",
        "the CLI's session presents B here"
    );
    // Matched anywhere on the screen rather than as a whole line: a CLI pane draws a prompt and
    // the output lands right after it. The marker is still unambiguous, because the echoed
    // command line spells it `second- pane` and only the program's own output spells it joined.
    let screen = until("the second seed's pane to show second-pane", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target])
            .await
            .ok()?;
        text.contains("second-pane").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[11] rmux -N -S … capture-pane -t {target} -> {shown:?}");
    println!("[wal] incompatible startup WAL reset; pane publication succeeded");

    prove_first_seed_still_live(io, api, &first, &second, binary, socket).await?;
    prove_rmux_sibling(io, &b, &second, binary, socket).await
}

/// Writes the sibling seed's log as an old-schema record that recovery must discard, returning it.
///
/// The sibling seed's only live shell so far is an idle standalone peer that opens storage
/// lazily, so the pane's first admission is the first to read this log: a complete record
/// missing the `staging` every current `BEGIN` carries, which recovery must discard whole
/// rather than refuse the pane.
fn write_incompatible_wal(fixture: &Fixture) -> Result<PathBuf, Failure> {
    let wal = fixture
        .scratch
        .path()
        .join(marsh_btrfs::STATE_DIR)
        .join("other/meta")
        .join(marsh_wal::LOG_FILE);
    std::fs::create_dir_all(wal.parent().ok_or("the log has no parent")?)?;
    std::fs::write(
        &wal,
        concat!(
            r#"{"op":"BEGIN","seq":1,"uid":"old-schema","op_count":0,"cmd":"old-schema","principal":"previous-owner","granted":[]}"#,
            "\n",
            r#"{"op":"END","seq":1}"#,
            "\n",
        ),
    )?;
    Ok(wal)
}

/// The API job still publishes into the first seed while the pane holds the second.
///
/// Both are live in the same host at the same instant, and neither seed sees the other's bytes.
async fn prove_first_seed_still_live(
    io: &RmuxFrontend,
    api: &ShellHandle,
    first: &Path,
    second: &Path,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let again = submit(
        api,
        "printf first > first-again; printf '%s%s\\n' first-pane -again",
    )
    .await?;
    println!(
        "[12] first seed `printf first > first-again` -> {}",
        describe(&again)
    );
    ensure!(
        again.is_published(),
        "the first seed is still this job's: {}",
        describe(&again)
    );
    ensure_eq!(std::fs::read(first.join("first-again"))?, b"first");
    ensure!(
        !second.join("src").join("first-again").exists(),
        "the two seeds never cross"
    );
    let target = pane_target(io, api).await?;
    let screen = until("the first seed's pane to show first-pane-again", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target])
            .await
            .ok()?;
        text.contains("first-pane-again").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[13] rmux -N -S … capture-pane -t {target} -> {shown:?}");
    Ok(())
}

/// A window the real client opens beside B, on B's source, without selecting it.
///
/// `-c` names the sibling seed's source directory by its absolute logical path. Nothing about the
/// daemon names that seed, so a shell that lands on it rather than on the host's default is the
/// directory choosing the seed, and B keeping its active window is `-d` holding.
async fn prove_rmux_sibling(
    io: &RmuxFrontend,
    b: &ShellHandle,
    second: &Path,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let start = second.join("src");
    let start = start.to_str().ok_or("the fixture path is not UTF-8")?;
    let before: Vec<ShellId> = io.jobs().into_iter().map(|view| view.id).collect();
    let b_target = pane_target(io, b).await?;
    ensure_eq!(
        display(binary, socket, &b_target, "#{window_active}").await?,
        "1",
        "B's window is the active one before anything opens beside it"
    );

    rmux(
        binary,
        socket,
        &[
            "new-window",
            "-d",
            "-t",
            "other-seed",
            "-n",
            "b-sibling",
            "-c",
            start,
        ],
    )
    .await?;

    // Every job the command added, taken once one of them is an idle terminal shell on the
    // sibling seed: a second addition is a failure to report, never a candidate to choose between.
    let added = until("an idle terminal job on the second seed", || async {
        let added: Vec<_> = io
            .jobs()
            .into_iter()
            .filter(|view| !before.contains(&view.id))
            .collect();
        added
            .iter()
            .any(|view| {
                view.sandbox.seed == second
                    && matches!(view.io, JobIo::Terminal { .. })
                    && view.running.is_none()
                    && !view.starting
            })
            .then_some(added)
    })
    .await?;
    let [sibling] = added.as_slice() else {
        let ids: Vec<&str> = added.iter().map(|view| view.id.as_str()).collect();
        return Err(format!("`new-window` added exactly one job, not {ids:?}").into());
    };
    let target = pane_target(io, &io.shell(&sibling.id)?).await?;
    let surface = display(binary, socket, &target, "#{session_name} #{window_name}").await?;
    println!(
        "[14] rmux -N -S … new-window -d -t other-seed -n b-sibling -c {start} -> `{}` at \
         {target} ({surface}) on seed {} dir {:?}",
        sibling.id.as_str(),
        sibling.sandbox.seed.display(),
        sibling.sandbox.dir.as_str()
    );
    ensure_eq!(
        surface,
        "other-seed b-sibling",
        "the new shell is presented by the window the client named"
    );
    ensure_eq!(
        sibling.sandbox.seed,
        second,
        "`-c` selects the sibling seed, never the daemon's default"
    );
    ensure_eq!(sibling.sandbox.dir.as_str(), "src");
    ensure_eq!(
        display(binary, socket, &target, "#{window_active}").await?,
        "0",
        "the new window was opened detached"
    );
    ensure_eq!(
        display(binary, socket, &b_target, "#{window_active}").await?,
        "1",
        "and B's window is still the active one"
    );

    let sources: std::collections::BTreeSet<_> = io
        .jobs()
        .into_iter()
        .map(|view| view.sandbox.seed)
        .collect();
    ensure!(sources.contains(second));
    ensure_eq!(
        sources.len(),
        2,
        "live jobs retain their independent logical source directories"
    );
    println!("[15] live jobs cover two independent sources");
    Ok(())
}

/// A typed `exit 7` at a real prompt, proved through the real client.
///
/// Nothing here stops anything: the status is the shell's own `exit` builtin's, the closure is the
/// job's, and the pane and its session go with it. The survivor is checked in the same listing, so
/// "the target is gone" cannot be a daemon that died.
async fn prove_prompt_exit(
    io: &RmuxFrontend,
    fixture: &Fixture,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let before: Vec<String> = io
        .jobs()
        .into_iter()
        .map(|view| view.id.as_str().to_owned())
        .collect();
    let seed = fixture
        .seed
        .to_str()
        .ok_or("the fixture path is not UTF-8")?;
    // No workload argument: this pane runs the prompt, which is the thing a user types `exit` at.
    rmux(
        binary,
        socket,
        &["new-session", "-d", "-s", "exit-smoke", "-c", seed],
    )
    .await?;

    // Chosen by the surface it presents, never by creation order: another job opened beside this
    // one would answer the "a new job exists" question just as well and be the wrong shell.
    let job = until("the `exit-smoke` pane's own shell", || async {
        for view in io.jobs() {
            if before.iter().any(|id| id == view.id.as_str())
                || view.starting
                || view.running.is_some()
            {
                continue;
            }
            let Ok(job) = io.shell(&view.id) else {
                continue;
            };
            if matches!(pane_target(io, &job).await, Ok(target) if target == "exit-smoke:0.0") {
                return Some(job);
            }
        }
        None
    })
    .await?;
    println!(
        "[exit] rmux -N -S … new-session -d -s exit-smoke -> `{}` at exit-smoke:0.0",
        job.id().as_str()
    );

    // The echoed command line spells the marker `EXIT_ READY`, so only the line's own *output* can
    // satisfy this wait: reaching it proves the prompt runs what is typed at it.
    send_keys_until(
        binary,
        socket,
        "exit-smoke",
        "EXIT_READY",
        &["printf '%s%s\\n' EXIT_ READY", "Enter"],
    )
    .await?;

    // `fg` was a word the prompt used to answer itself. Now it is looked up by the shell, which
    // finds this function first and expands its argument unsplit. The marker is joined only by
    // running the function: the echoed declaration spells it `PREPARSER_ REMOVED`, and the echoed
    // call does not spell it at all.
    send_keys_until(
        binary,
        socket,
        "exit-smoke",
        "PREPARSER_REMOVED:two words",
        &[
            "function fg { printf '%s%s:%s\\n' PREPARSER_ REMOVED \"$1\"; }; payload='two words'",
            "Enter",
            "fg \"$payload\"",
            "Enter",
        ],
    )
    .await?;
    println!("[grammar] ordinary shell lookup and expansion reached the pane");

    io.write_input(&job, b"exit 7\r").await?;
    let end = tokio::time::timeout(TIMEOUT, job.wait_closed()).await??;
    let completion = end
        .completion
        .as_ref()
        .ok_or("the closure carries no completion at all")?;
    println!(
        "[exit] `exit 7` typed at that prompt -> {}",
        describe(completion)
    );
    ensure_eq!(
        completion.exit_code(),
        Some(7),
        "the builtin's argument is the status the job ended with"
    );
    ensure!(
        completion.is_published(),
        "and the line was gated like any other: {}",
        describe(completion)
    );
    ensure!(
        io.job(job.id()).is_none(),
        "the exited job is gone rather than idle"
    );

    let listed = session_gone(binary, socket, "exit-smoke", "smoke").await?;
    let shown: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    println!("[exit] rmux -N -S … list-panes -a -F '#{{session_name}}' -> {shown:?}");
    println!("[exit] status=7; target removed; survivor alive");
    Ok(())
}

/// The real client's pane listing once it names `survivor`'s session and no longer `gone`'s.
///
/// The survivor is checked in the same listing, so an absent target cannot be a dead daemon.
async fn session_gone(
    binary: &Path,
    socket: &Path,
    gone: &str,
    survivor: &str,
) -> Result<String, Failure> {
    until(&format!("`{gone}` to leave the pane list"), || async {
        let text = rmux(
            binary,
            socket,
            &["list-panes", "-a", "-F", "#{session_name}"],
        )
        .await
        .ok()?;
        let removed = !text.lines().any(|line| line.trim() == gone);
        let alive = text.lines().any(|line| line.trim() == survivor);
        (removed && alive).then_some(text)
    })
    .await
}

/// The shell a named session's first pane runs, resolved by the surface it presents.
///
/// Numbering jobs by creation order would name whichever shell happened to open beside this one,
/// so the pane target is what identifies it — exactly as the exit scenario does.
async fn pane_shell(
    io: &RmuxFrontend,
    session: &str,
    claimed: &[String],
) -> Result<ShellHandle, Failure> {
    let target = format!("{session}:0.0");
    let job = until(&format!("the `{session}` pane's own shell"), || async {
        for view in io.jobs() {
            if claimed.iter().any(|id| id == view.id.as_str()) || view.starting {
                continue;
            }
            let Ok(job) = io.shell(&view.id) else {
                continue;
            };
            if matches!(pane_target(io, &job).await, Ok(found) if found == target) {
                return Some(job);
            }
        }
        None
    })
    .await?;
    println!("[caps] {target} -> shell `{}`", job.id().as_str());
    Ok(job)
}

/// The two concurrency verdicts, driven through real panes and real persistent shells.
///
/// The first half is the reported conflict: one pane writes an unstaged file, another appends to
/// it, and the pane must show a *capability denial* naming the owner's unstaged precondition —
/// never an instruction to rerun. A conflicting read returns stale after one evaluation.
async fn prove_capability_sync(
    io: &RmuxFrontend,
    fixture: &Fixture,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let seed = fixture
        .seed
        .to_str()
        .ok_or("the fixture path is not UTF-8")?;
    let mut before: Vec<String> = io
        .jobs()
        .into_iter()
        .map(|view| view.id.as_str().to_owned())
        .collect();

    // Two prompt panes, opened before either types anything: the conflict is between two live
    // shells over one seed, not between two edits by one owner.
    for session in ["caps-writer", "caps-reader"] {
        rmux(
            binary,
            socket,
            &["new-session", "-d", "-s", session, "-c", seed],
        )
        .await?;
    }

    let writer = pane_shell(io, "caps-writer", &before).await?;
    before.push(writer.id().as_str().to_owned());
    let reader = pane_shell(io, "caps-reader", &before).await?;

    rmux(
        binary,
        socket,
        &[
            "send-keys",
            "-t",
            "caps-writer",
            "--",
            "echo foo > test.txt",
            "Enter",
        ],
    )
    .await?;
    let owned = fixture.seed("test.txt");
    until("the writer's publication to reach the seed", || async {
        std::fs::read_to_string(&owned)
            .ok()
            .filter(|text| text == "foo\n")
    })
    .await?;
    println!(
        "[caps] `echo foo > test.txt` at caps-writer -> seed holds {:?}",
        "foo\n"
    );

    rmux(
        binary,
        socket,
        &[
            "send-keys",
            "-t",
            "caps-reader",
            "--",
            "echo foo2 >> test.txt",
            "Enter",
        ],
    )
    .await?;
    let reported = until("the reader's pane to report a verdict", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", "caps-reader"])
            .await
            .ok()?;
        text.contains("denied").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = reported.lines().filter(|line| !line.is_empty()).collect();
    println!("[caps] rmux -N -S … capture-pane -t caps-reader -> {shown:?}");
    ensure_eq!(
        std::fs::read_to_string(&owned)?,
        "foo\n",
        "the seed still holds the owner's bytes, byte for byte"
    );

    prove_read_dependency(io, fixture, &writer, &reader).await?;

    io.stop(&writer, false).await?;
    io.stop(&reader, false).await?;
    Ok(())
}

/// A held command returns stale with its native status; its outside counter changes once.
///
/// The reader appends one `x` to a counter outside the seed, reads `sync.txt`, says it is ready
/// and blocks on its standard input; the writer then publishes new bytes, and only that input
/// resumes the reader. Should anything fail before the held command completes, the reader is
/// force-stopped and its task joined before the failure is returned.
async fn prove_read_dependency(
    io: &RmuxFrontend,
    fixture: &Fixture,
    writer: &ShellHandle,
    reader: &ShellHandle,
) -> Result<(), Failure> {
    submit(writer, "printf 'old\\n' > sync.txt").await?;
    let attempts = fixture.scratch.path().join("caps-attempts");
    let ready = fixture.scratch.path().join("caps-ready");
    let held = format!(
        "printf x >> {}; /bin/cat sync.txt >/dev/null; printf ready > {}; /bin/sh -c 'read value'; printf candidate > observed.txt",
        quoted_path(&attempts)?,
        quoted_path(&ready)?
    );
    let mut running = tokio::spawn({
        let reader = reader.clone();
        async move { reader.run_command(&held, CommandOptions::default()).await }
    });
    let finished = match release_held_read(io, writer, reader, &ready).await {
        Ok(()) => tokio::time::timeout(TIMEOUT, &mut running)
            .await
            .map_err(|_| {
                Failure::from(format!(
                    "the held command did not complete within {TIMEOUT:?}"
                ))
            }),
        Err(error) => Err(error),
    };
    let result = match finished {
        // Its task has ended, so there is nothing left to stop.
        Ok(joined) => joined?,
        Err(error) => {
            let stopped = io.stop(reader, true).await.map_err(Failure::from);
            let joined = if let Ok(joined) = tokio::time::timeout(TIMEOUT, &mut running).await {
                joined.map(drop).map_err(Failure::from)
            } else {
                running.abort();
                Err(format!("the held command outlived its forced stop by {TIMEOUT:?}").into())
            };
            return conclude(Err(error), conclude(stopped, joined));
        }
    };
    let completion = match result {
        Err(IoError::Run(RunError::Execution { completion })) => completion,
        other => return Err(format!("expected one stale completion, got {other:?}").into()),
    };
    ensure!(
        matches!(completion.result.as_ref(), Err(error) if matches!(error.kind(), ShellErrorKind::Stale { .. }))
    );
    ensure_eq!(completion.exit_code(), Some(0));
    ensure_eq!(std::fs::read(attempts)?, b"x");
    ensure!(!fixture.seed("observed.txt").exists());
    println!("[caps] stale read returned once, with no replay and no publication");
    Ok(())
}

/// Waits for the held reader's readiness, publishes the writer's new bytes, then resumes it.
async fn release_held_read(
    io: &RmuxFrontend,
    writer: &ShellHandle,
    reader: &ShellHandle,
    ready: &Path,
) -> Result<(), Failure> {
    until("the reader to finish its original read", || async {
        ready.exists().then_some(())
    })
    .await?;
    submit(writer, "printf 'new\\n' > sync.txt").await?;
    io.write_input(reader, b"continue\n").await?;
    Ok(())
}

/// `path` as one single-quoted shell word, whatever it contains.
fn quoted_path(path: &Path) -> Result<String, Failure> {
    let text = path.to_str().ok_or("the fixture path is not UTF-8")?;
    Ok(brush_core::escape::force_quote(
        text,
        brush_core::escape::QuoteMode::SingleQuote,
    ))
}

/// A normal [`marsh::Shell`] starting in `initial_dir`, through the fixture's own backend.
async fn fixture_shell(fixture: &Fixture, initial_dir: &Path) -> Result<marsh::Shell, Failure> {
    if fixture.real_btrfs {
        Ok(marsh::Shell::new(initial_dir).await?)
    } else {
        Ok(
            marsh_core::test_support::shell_builder(Arc::clone(&fixture.filesystem))
                .working_dir(initial_dir.to_path_buf())
                .build()
                .await?,
        )
    }
}

/// This process's kernel-reported tracer: zero when no native helper is attached.
fn tracer_pid() -> Result<i32, Failure> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("TracerPid:"))
        .ok_or("missing TracerPid: in /proc/self/status")?;
    Ok(value.trim().parse()?)
}

/// Executed only in a fresh process whose PATH contains git and no tracer executable.
///
/// Every shell the proof opens is retained until teardown, where each is closed even after
/// another failed to; the fixture is reclaimed after that, whatever the proof concluded.
async fn native_only() -> Result<(), Failure> {
    let fixture = Fixture::new()?;
    let mut shells = Vec::new();
    let proved = prove_native_shells(&fixture, &mut shells).await;
    let mut closed = Ok(());
    for shell in &shells {
        let outcome = shell.close(true).await.map_err(Failure::from);
        closed = conclude(closed, outcome);
    }
    drop(shells);
    let cleaned = fixture.cleanup();
    conclude(conclude(proved, closed), cleaned)?;
    println!("[shell] normal interface; shared authority; recovery automatic");
    println!("[lurk] native Rust tracing; no tracer executable; read claim enforced");
    Ok(())
}

/// Two live shells share one seed's authority, and a third opened after both closed recovers it.
///
/// Each shell is pushed onto `shells` as soon as it exists, so a later failure cannot orphan it.
async fn prove_native_shells(
    fixture: &Fixture,
    shells: &mut Vec<marsh::Shell>,
) -> Result<(), Failure> {
    std::fs::write(fixture.seed("read-claim"), b"before")?;
    std::fs::write(fixture.seed("zero-op"), b"same")?;
    let mut git = tokio::process::Command::new("git");
    git.args(["init", "-q"])
        .arg(&fixture.seed)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    let initialized = command_output(&mut git, "git init -q", TIMEOUT).await?;
    ensure!(
        initialized.status.success(),
        "git init -q failed with {}: {}",
        initialized.status,
        String::from_utf8_lossy(&initialized.stderr)
    );
    ensure_eq!(tracer_pid()?, 0);
    shells.push(fixture_shell(fixture, &fixture.seed).await?);
    // Construction itself attaches the tracer; durable storage still waits for a managed route.
    let helper = tracer_pid()?;
    ensure!(helper != 0, "a built shell has no native tracer attached");
    let state = fixture.scratch.path().join(marsh_btrfs::STATE_DIR);
    ensure!(
        !state.try_exists()?,
        "building a shell materialized {}",
        state.display()
    );
    println!("[tracing] shell build ready before commands");
    shells.push(fixture_shell(fixture, &fixture.seed).await?);
    ensure_eq!(tracer_pid()?, helper);
    let [a, b] = shells.as_slice() else {
        return Err("the two initial shells were not retained".into());
    };
    ensure!(a.principal() != b.principal());
    let result = a.run("export KEPT=value; printf first > owned").await?;
    ensure_eq!(u8::from(result.exit_code), 0);
    let kept = a
        .env_var("KEPT")
        .await
        .ok_or("`KEPT` is unset after the line that exported it")?;
    ensure!(matches!(kept.value(), brush_core::ShellValue::String(value) if value == "value"));
    for path in ["owned"] {
        let error = b
            .run(&format!("printf blind > {path}"))
            .await
            .err()
            .ok_or("blind write unexpectedly accepted")?;
        ensure!(matches!(error.kind(), ShellErrorKind::Denied { .. }));
        let refused = error
            .execution_result()
            .ok_or("the refusal carries no execution result")?;
        ensure_eq!(u8::from(refused.exit_code), 0);
    }
    a.run("/bin/cat read-claim >/dev/null").await?;
    let error = b
        .run("printf blind > read-claim")
        .await
        .err()
        .ok_or("read claim was lost")?;
    ensure!(matches!(error.kind(), ShellErrorKind::Denied { .. }));
    ensure_eq!(std::fs::read(fixture.seed("read-claim"))?, b"before");
    b.run("/bin/cat read-claim >/dev/null; printf accepted > read-claim")
        .await?;
    ensure_eq!(std::fs::read(fixture.seed("read-claim"))?, b"accepted");
    a.run("printf same > zero-op").await?;
    // Both close before the third opens: that ordering is what makes its authority recovered.
    a.close(false).await?;
    ensure_eq!(tracer_pid()?, helper);
    b.close(false).await?;
    // Closed handles stay in `shells`; releasing a shell must still release its tracer.
    until("the closed shells' native tracer to detach", || async {
        match tracer_pid() {
            Ok(0) => Some(Ok(())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }
    })
    .await??;
    let first = a.principal().clone();
    // An idle peer keeps the reopened shell on the managed route under the default policy.
    shells.push(fixture_shell(fixture, &fixture.seed).await?);
    let reattached = tracer_pid()?;
    ensure!(
        reattached != 0,
        "a reopened shell has no native tracer attached"
    );
    shells.push(fixture_shell(fixture, &fixture.seed).await?);
    ensure_eq!(tracer_pid()?, reattached);
    let reopened = shells.last().ok_or("the reopened shell was not retained")?;
    ensure!(*reopened.principal() != first);
    for path in ["owned", "read-claim", "zero-op"] {
        let error = reopened
            .run(&format!("printf blind > {path}"))
            .await
            .err()
            .ok_or("durable ownership was lost")?;
        ensure!(matches!(error.kind(), ShellErrorKind::Denied { .. }));
    }
    ensure_eq!(u8::from(reopened.run("false").await?.exit_code), 1);
    reopened.close(false).await?;
    ensure_eq!(std::fs::read(fixture.seed("owned"))?, b"first");
    ensure_eq!(std::fs::read(fixture.seed("zero-op"))?, b"same");
    Ok(())
}

/// Runs [`native_only`] in a child whose PATH holds nothing but a link to git, so no tracer
/// executable can be found there, and relays what it printed.
async fn prove_without_tracer_executables() -> Result<(), Failure> {
    let search = std::env::var_os("PATH").ok_or("PATH is unset")?;
    let git = std::env::split_paths(&search)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .ok_or("git is not available")?
        .canonicalize()?;
    let path = tempfile::tempdir()?;
    std::os::unix::fs::symlink(git, path.path().join("git"))?;
    // Only the child's environment changes; the multithreaded parent is never mutated.
    let mut child = tokio::process::Command::new(std::env::current_exe()?);
    child.arg("--native-trace-only").env("PATH", path.path());
    let output = command_output(&mut child, "the native-only smoke", TIMEOUT).await?;
    if !output.status.success() {
        return Err(format!(
            "native-only smoke failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let text = String::from_utf8(output.stdout)?;
    ensure!(text.contains("[tracing] shell build ready before commands"));
    ensure!(text.contains("[lurk] native Rust tracing; no tracer executable; read claim enforced"));
    print!("{text}");
    Ok(())
}

async fn prove_read_claim(io: &RmuxFrontend, fixture: &Fixture) -> Result<(), Failure> {
    let reader = io
        .open_shell(
            &fixture.seed,
            Some(ShellId::from("trace-reader")),
            SpawnOptions::default(),
        )
        .await?;
    let writer = io
        .open_shell(
            &fixture.seed,
            Some(ShellId::from("trace-writer")),
            SpawnOptions::default(),
        )
        .await?;
    submit(&reader, "__rmux_io read -- read-claim >/dev/null").await?;
    let refused = writer
        .run_command("printf blind > read-claim", CommandOptions::default())
        .await;
    let Err(IoError::Run(RunError::Execution { completion })) = refused else {
        return Err("rmux lost a native Read claim".into());
    };
    ensure_eq!(completion.exit_code(), Some(0));
    ensure!(
        matches!(completion.result.as_ref(), Err(error) if matches!(error.kind(), ShellErrorKind::Denied { .. }))
    );
    ensure_eq!(std::fs::read(fixture.seed("read-claim"))?, b"before");
    submit(
        &writer,
        "__rmux_io read -- read-claim >/dev/null; printf accepted > read-claim",
    )
    .await?;
    ensure_eq!(std::fs::read(fixture.seed("read-claim"))?, b"accepted");
    io.stop(&reader, false).await?;
    io.stop(&writer, false).await?;
    println!("[trace] read claim enforced; explicit read+edit accepted");
    Ok(())
}

/// The shared pure primitives, through their public `marsh_lib` API, each answer asserted exactly
/// before it is printed: FIFO membership of a bounded key set, eviction order and charge across a
/// multi-item weighted eviction, and positional occurrence pairing that ignores an identity only
/// the later layout has.
fn prove_parametric_collections() -> Result<(), Failure> {
    let members = |keys: &FifoSet<String>| -> Vec<&'static str> {
        ["a", "b", "c", "d"]
            .into_iter()
            .filter(|key| keys.contains(&(*key).to_owned()))
            .collect()
    };
    let mut keys = FifoSet::new(2);
    let inserted: Vec<bool> = ["a", "b", "a", "c"]
        .into_iter()
        .map(|key| keys.insert(key.to_owned()))
        .collect();
    ensure_eq!(
        inserted,
        [true, true, false, true],
        "a repeated key is not new, and keeps its age"
    );
    ensure_eq!(members(&keys), ["b", "c"]);
    ensure!(keys.remove(&"b".to_owned()));
    ensure!(
        keys.insert("b".to_owned()),
        "a removed key comes back as a new one"
    );
    ensure!(keys.insert("d".to_owned()));
    ensure_eq!(
        members(&keys),
        ["b", "d"],
        "the reinserted key is younger than `c`"
    );
    ensure_eq!(keys.len(), 2);
    println!("[parametric] bounded keys: a b a c -> [b, c]; -b +b +d -> [b, d]");

    let mut retention = BoundedRetention::new(2, 5, 2);
    let mut evicted = Vec::new();
    for (item, bytes) in [("a", 2), ("b", 2), ("c", 4)] {
        retention.push(item, bytes, |item| evicted.push(item));
    }
    ensure_eq!(
        evicted,
        ["a", "b"],
        "oldest first, one at a time, until `c` fits"
    );
    ensure_eq!(retention.iter().copied().collect::<Vec<_>>(), ["c"]);
    ensure_eq!(retention.retained_bytes(), 4);
    retention.push("d", 6, |item| evicted.push(item));
    ensure_eq!(
        evicted,
        ["a", "b", "c", "d"],
        "an item over the whole ceiling empties the retention and is not kept itself"
    );
    ensure!(retention.is_empty());
    ensure_eq!(retention.retained_bytes(), 0);
    println!(
        "[parametric] weighted retention (2 items, 5 bytes): (a,2) (b,2) (c,4) evicts [a, b] \
         and keeps [c] at 4; (d,6) evicts [c, d] and keeps nothing at 0"
    );

    let before = group_keys_by_value(BTreeMap::from([(1_u32, 7_u32), (4, 7), (9, 8)]));
    let after = group_keys_by_value(BTreeMap::from([(2_u32, 7_u32), (5, 7), (6, 9), (10, 8)]));
    let mut remapped = BTreeMap::new();
    extend_occurrence_map(&mut remapped, before, after, |id| {
        format!("window @{id} occurrence count changed")
    })
    .map_err(|mismatch| {
        format!("every identity occurs as often after as before, but {mismatch}")
    })?;
    ensure_eq!(remapped, BTreeMap::from([(1, 2), (4, 5), (9, 10)]));
    println!(
        "[parametric] occurrences: {{1:@7, 4:@7, 9:@8}} -> {{2:@7, 5:@7, 6:@9, 10:@8}} remaps \
         {remapped:?}"
    );
    Ok(())
}

/// Paths a transaction granted, each with its grant's label: this driver's own WAL metadata, not
/// a product schema.
///
/// One named field rather than a bare map, because the log flattens metadata into a `BEGIN`
/// record that refuses unknown fields, and a flattened map's keys would read back as exactly that.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Grants {
    /// Seed-relative path → grant label.
    grants: BTreeMap<String, String>,
}

impl Grants {
    /// Metadata granting each `(path, label)` pair.
    fn of<const N: usize>(entries: [(&str, &str); N]) -> Self {
        Self {
            grants: entries
                .into_iter()
                .map(|(path, label)| (path.to_owned(), label.to_owned()))
                .collect(),
        }
    }

    /// Recovery's projector: every granted path, as the seed-relative path it names.
    fn paths(&self, paths: &mut Vec<PathBuf>) {
        paths.extend(self.grants.keys().map(PathBuf::from));
    }

    /// Recovery's pruner: forgets the grant of every path deleted outside the log, and keeps the
    /// transaction only while a grant is left.
    fn prune(&mut self, missing: &BTreeSet<PathBuf>) -> bool {
        self.grants
            .retain(|path, _| !missing.contains(Path::new(path.as_str())));
        !self.grants.is_empty()
    }
}

/// Fails unless nothing at all — not even a dangling symlink — is at `path`.
fn absent(path: &Path) -> Result<(), Failure> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(format!("{} still exists", path.display()).into()),
    }
}

/// The WAL proof's scratch layout: an empty seed, a snapshot root holding each transaction's
/// source under its uid, and a log whose `meta/` directory nothing creates in advance.
struct WalLayout {
    /// The tree transactions publish into.
    seed: PathBuf,
    /// The snapshot root; a transaction's source is `snap/<uid>`.
    snap: PathBuf,
    /// `meta/wal.jsonl`.
    log: PathBuf,
}

/// The write-ahead log over plain directories, in a scratch root of its own.
///
/// Publishes a file, a second file, a symlink and a directory with explicit modes; then, with a
/// finished transaction's staging residue left behind and one published file deleted outside the
/// log, recovers with real metadata callbacks. Finally a second transaction removes the symlink
/// and the empty directory — the link itself, never what it points at.
fn prove_parametric_wal() -> Result<(), Failure> {
    let root = tempfile::TempDir::new()?;
    let layout = WalLayout {
        seed: root.path().join("seed"),
        snap: root.path().join("snap"),
        log: root.path().join("meta").join(marsh_wal::LOG_FILE),
    };
    std::fs::create_dir(&layout.seed)?;
    std::fs::create_dir(&layout.snap)?;

    let uid = SourceUid::new("parametric-wal")?;
    publish_parametric_source(&layout, &uid)?;
    reconcile_parametric_log(&layout, &uid)?;
    remove_parametric_entries(&layout)?;
    println!(
        "[parametric] wal: fingerprints, reconciliation, staging cleanup, and removal preserved"
    );
    Ok(())
}

/// Transaction 1: `gone` and `kept` files at 0644, `link -> kept` and an `empty` directory at
/// 0750, each path granted a label, published exactly as the source holds them.
fn publish_parametric_source(layout: &WalLayout, uid: &SourceUid) -> Result<(), Failure> {
    let WalLayout { seed, snap, log } = layout;
    let source = snap.join(uid);
    std::fs::create_dir(&source)?;
    for (name, bytes) in [("gone", b"gone\n"), ("kept", b"kept\n")] {
        std::fs::write(source.join(name), bytes)?;
        std::fs::set_permissions(source.join(name), Permissions::from_mode(0o644))?;
    }
    std::os::unix::fs::symlink("kept", source.join("link"))?;
    std::fs::create_dir(source.join("empty"))?;
    std::fs::set_permissions(source.join("empty"), Permissions::from_mode(0o750))?;

    let grants = Grants::of([
        ("gone", "grant-gone"),
        ("kept", "grant-kept"),
        ("link", "grant-link"),
    ]);
    let ops = marsh_wal::diff_trees(seed, &source)?;
    marsh_wal::prepare(seed, &source, log, uid, Seq::new(1), &grants, &ops)?.apply()?;
    ensure_eq!(std::fs::read(seed.join("gone"))?, b"gone\n");
    ensure_eq!(std::fs::read(seed.join("kept"))?, b"kept\n");
    ensure_eq!(std::fs::read_link(seed.join("link"))?, Path::new("kept"));
    ensure!(std::fs::symlink_metadata(seed.join("empty"))?.is_dir());
    for (name, mode) in [("gone", 0o644), ("kept", 0o644), ("empty", 0o750)] {
        ensure_eq!(
            Mode::of(&std::fs::symlink_metadata(seed.join(name))?),
            Mode::new(mode),
            "`{name}` is published with its source's mode"
        );
    }
    Ok(())
}

/// Transaction 1's staging residue left behind and `gone` deleted outside the log, recovered with
/// the real metadata callbacks: exactly that path's write and grant go, the residue is cleared,
/// and the rewritten log is one a second recovery leaves byte for byte.
fn reconcile_parametric_log(layout: &WalLayout, uid: &SourceUid) -> Result<(), Failure> {
    let WalLayout { seed, snap, log } = layout;
    let staging = Staging::of(Seq::new(1), uid);
    let residue = seed.join(staging.as_str());
    std::fs::create_dir(&residue)?;
    std::fs::set_permissions(&residue, Permissions::from_mode(0o700))?;
    std::fs::remove_file(seed.join("gone"))?;

    let recovered = marsh_wal::recover::<Grants>(seed, snap, log, Grants::paths, Grants::prune)?;
    let [transaction] = recovered.as_slice() else {
        return Err(
            format!("expected exactly one recovered transaction, got {recovered:?}").into(),
        );
    };
    let kept = Grants::of([("kept", "grant-kept"), ("link", "grant-link")]);
    ensure_eq!((transaction.seq, &transaction.uid), (Seq::new(1), uid));
    ensure_eq!(
        transaction.ops,
        [
            CommitOp::CreateDirectory {
                path: PathBuf::from("empty"),
                mode: Mode::new(0o750),
            },
            CommitOp::Write(PathBuf::from("kept")),
            CommitOp::Write(PathBuf::from("link")),
        ],
        "only the write of the path deleted outside the log is forgotten"
    );
    ensure_eq!(transaction.meta, kept, "and only its grant");

    let records = JsonLog::<WalRecord<Grants>>::read(log)?;
    let [
        WalRecord::Begin {
            seq,
            uid: logged,
            op_count,
            staging: logged_staging,
            meta,
        },
        operations @ ..,
        WalRecord::End { seq: ended },
    ] = records.as_slice()
    else {
        return Err(format!("the rewritten log is not one BEGIN … END frame: {records:?}").into());
    };
    ensure_eq!(
        (*seq, logged, *op_count, logged_staging, meta),
        (Seq::new(1), uid, 3, &staging, &kept),
        "the rewritten BEGIN counts what is left, under the same staging identity"
    );
    ensure_eq!(*ended, Seq::new(1));
    ensure_eq!(operations.len(), 3);
    ensure!(
        operations
            .iter()
            .all(|record| !matches!(record, WalRecord::Begin { .. } | WalRecord::End { .. })),
        "one frame with one matching END: {records:?}"
    );
    absent(&residue)?;
    ensure_eq!(std::fs::read(seed.join("kept"))?, b"kept\n");
    ensure_eq!(std::fs::read_link(seed.join("link"))?, Path::new("kept"));

    let compacted = std::fs::read(log)?;
    let again = marsh_wal::recover::<Grants>(seed, snap, log, Grants::paths, Grants::prune)?;
    let [repeated] = again.as_slice() else {
        return Err(format!("a second recovery returned {again:?}").into());
    };
    ensure_eq!(
        (repeated.seq, &repeated.uid, &repeated.ops, &repeated.meta),
        (
            transaction.seq,
            &transaction.uid,
            &transaction.ops,
            &transaction.meta
        ),
        "a second recovery returns the same transaction"
    );
    ensure_eq!(
        std::fs::read(log)?,
        compacted,
        "and leaves the log byte for byte"
    );
    Ok(())
}

/// Transaction 2, from an empty source and with no grants: removes `link` and the `empty`
/// directory, neither following the link nor recursing.
fn remove_parametric_entries(layout: &WalLayout) -> Result<(), Failure> {
    let WalLayout { seed, snap, log } = layout;
    let removal = SourceUid::new("parametric-remove")?;
    let source = snap.join(&removal);
    std::fs::create_dir(&source)?;
    marsh_wal::prepare(
        seed,
        &source,
        log,
        &removal,
        Seq::new(2),
        &Grants::of([]),
        &[
            CommitOp::Remove(PathBuf::from("link")),
            CommitOp::RemoveDirectory(PathBuf::from("empty")),
        ],
    )?
    .apply()?;
    absent(&seed.join("link"))?;
    absent(&seed.join("empty"))?;
    ensure_eq!(
        std::fs::read(seed.join("kept"))?,
        b"kept\n",
        "removing the link never touched what it pointed at"
    );
    Ok(())
}

/// Everything between binding the host and shutting it down.
async fn drive(
    io: &RmuxFrontend,
    fixture: &Fixture,
    binary: &Path,
    socket: &Path,
) -> Result<(), Failure> {
    let job = open_api(io, fixture).await?;
    capture_from_outside(io, &job, binary, socket).await?;
    prove_parametric_streams(io, &job).await?;
    prove_parametric_collections()?;
    prove_parametric_wal()?;

    let view = io.switch(&job).await?;
    let current = io.current_job().map(|view| view.id.as_str().to_owned());
    println!(
        "[6] io.switch(\"{}\") -> current = {current:?}",
        view.id.as_str()
    );
    ensure_eq!(current.as_deref(), Some("api"), "the selection is `api`");

    // The frontend agreeing with itself is not the observation. The claim is that a *native*
    // switch reaches the presentation, so the real client is asked which pane and which window it
    // considers active, and both answers have to be this shell's.
    let target = pane_target(io, &job).await?;
    let active = until(
        "the client to report `api`'s pane and window active",
        || async {
            let text = display(binary, socket, &target, "#{pane_active} #{window_active}")
                .await
                .ok()?;
            (text == "1 1").then_some(text)
        },
    )
    .await?;
    println!(
        "[6b] rmux -N -S … display-message -t {target} '#{{pane_active}} #{{window_active}}' \
         -> {active:?}"
    );

    prove_denial(io, fixture).await?;
    prove_read_claim(io, fixture).await?;
    prove_pipe(io).await?;
    prove_second_seed(io, fixture, &job, binary, socket).await?;
    prove_prompt_exit(io, fixture, binary, socket).await?;
    prove_capability_sync(io, fixture, binary, socket).await?;

    io.stop(&job, false).await?;
    let api = ShellId::from("api");
    let remaining = until("`api` to disappear", || async {
        io.job(&api).is_none().then(|| {
            io.jobs()
                .iter()
                .map(|view| view.id.as_str().to_owned())
                .collect::<Vec<_>>()
        })
    })
    .await?;
    println!("[16] io.stop(\"api\") -> remaining jobs {remaining:?}");
    Ok(())
}

/// Seeds the fixture, hosts a daemon over it and drives every observation through that host.
///
/// Once the host exists it is shut down whatever the drive concluded, and awaited to the end:
/// its seeds are only reclaimed after this returns, and a shutdown cut short would leave them
/// live.
async fn host_and_drive(fixture: &Fixture, binary: &Path) -> Result<(), Failure> {
    std::fs::write(fixture.seed("read-claim"), b"before")?;
    let socket = fixture.socket();
    println!(
        "fixture: seed={} sibling={} ({}), socket={}\n",
        fixture.seed.display(),
        fixture.other.display(),
        if fixture.real_btrfs {
            "real btrfs subvolumes"
        } else {
            "CopyTree fallback: API and CLI behaviour only, not kernel btrfs behaviour"
        },
        socket.display()
    );

    // No validator and no seed: the host is handed the directory a request that names none starts
    // in, and every seed below is discovered by the shell that asked for it.
    let geometry = TerminalGeometry { rows: 24, cols: 80 };
    let host = if fixture.real_btrfs {
        RmuxFrontend::open(
            DaemonConfig::new(socket.clone()),
            &fixture.seed,
            ShellEnvironment::new(),
            geometry,
        )
        .await?
    } else {
        rmux_server::test_support::open_frontend(
            DaemonConfig::new(socket.clone()),
            &fixture.seed,
            ShellEnvironment::new(),
            geometry,
            Arc::clone(&fixture.filesystem),
            marsh::SandboxPolicy::default(),
        )
        .await?
    };

    // Idle standalone peers on each seed: under the default SharedSource policy they make every
    // pane and workload shell take the managed route the proofs below observe.
    let mut peers = Vec::new();
    let driven = async {
        peers.push(fixture_shell(fixture, &fixture.seed).await?);
        peers.push(fixture_shell(fixture, &fixture.other).await?);
        // Everything below goes through the frontend's own operations. There is no `host.mux()`,
        // and this driver would not compile if it reached for one.
        drive(&host, fixture, binary, &socket).await
    }
    .await;
    let shutdown = host.shutdown().await.map_err(Failure::from);
    let mut closed = Ok(());
    for peer in &peers {
        let outcome = peer.close(true).await.map_err(Failure::from);
        closed = conclude(closed, outcome);
    }
    conclude(conclude(driven, shutdown), closed)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Failure> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--native-trace-only")
    {
        return native_only().await;
    }
    prove_without_tracer_executables().await?;
    let binary = rmux_binary()?;
    println!("rmux executable: {}", binary.display());

    let fixture = Fixture::new()?;
    let proved = host_and_drive(&fixture, &binary).await;
    let cleaned = fixture.cleanup();
    conclude(proved, cleaned)?;
    println!("\nrmux_smoke: every observation held.");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A nonzero exit is the caller's to judge: both streams still arrive byte for byte beside it.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn subprocess_capture_preserves_bytes_and_status() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'O\\000U'; printf 'E\\377R' >&2; exit 7"]);
        let output = command_output(&mut command, "the capture probe", TIMEOUT)
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"O\0U");
        assert_eq!(output.stderr, b"E\xffR");
    }

    /// A child still running at the deadline is not merely signalled or abandoned: it has been
    /// reaped, so its recorded pid is no longer a child of this process at all.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn subprocess_timeout_reaps_child() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "printf %s \"$$\" > \"$PID_FILE\"; exec /bin/sleep 60"])
            .env("PID_FILE", &pid_file);
        let error = command_output(&mut command, "the sleeping probe", Duration::from_secs(5))
            .await
            .unwrap_err();
        let kind = error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(std::io::ErrorKind::TimedOut), "{error}");

        let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let mut status = 0;
        // SAFETY: `waitpid` reads only its arguments and writes the status through a pointer to
        // a live local; `WNOHANG` keeps it from blocking.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            (waited, errno),
            (-1, Some(libc::ECHILD)),
            "pid {pid} is still a child of this process"
        );
    }

    /// A malformed snapshot directory under one seed is reported, and everything else the
    /// fixture owns is still reclaimed through its real backend.
    #[test]
    fn fixture_cleanup_continues_after_snapshot_error() {
        let scratch = tempfile::TempDir::new().unwrap();
        let root = scratch.path().to_path_buf();
        let filesystem: Arc<dyn Subvolumes> = Arc::new(marsh_btrfs::fake::CopyTree::new());
        let fixture = Fixture {
            seed: root.join("seed"),
            other: root.join("other"),
            scratch,
            filesystem: Arc::clone(&filesystem),
            real_btrfs: false,
        };
        filesystem.create_subvolume(&fixture.seed).unwrap();
        filesystem.create_subvolume(&fixture.other).unwrap();
        std::fs::write(fixture.other("kept"), b"kept").unwrap();
        let state = root.join(marsh_btrfs::STATE_DIR);
        let snaps = state.join("other").join("snap");
        std::fs::create_dir_all(&snaps).unwrap();
        let snapshot = snaps.join("job");
        filesystem.snapshot(&fixture.other, &snapshot).unwrap();
        let malformed = state.join("seed").join("snap");
        std::fs::create_dir_all(state.join("seed")).unwrap();
        std::fs::write(&malformed, b"not a directory").unwrap();

        let error = fixture.cleanup().unwrap_err();
        let kind = error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(std::io::ErrorKind::NotADirectory), "{error}");
        assert!(
            error.to_string().contains(&malformed.display().to_string()),
            "{error}"
        );
        for path in [&snapshot, &root.join("seed"), &root.join("other"), &root] {
            assert!(
                std::fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "{} remains",
                path.display()
            );
        }
    }
}
