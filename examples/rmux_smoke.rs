#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "a smoke driver's failure mode is an immediate, loud abort naming the observation \
              that did not hold"
)]
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
//! 3. A second principal's overwrite of the first's file exits zero and is *denied*: the native
//!    call answers `IoError::Run(RunError::Policy(..))`, the completion that error carries still
//!    reports exit zero, and the seed keeps the original bytes.
//! 4. A real pipe execution carries binary stdout and stderr independently and byte for byte, and
//!    its standard input really ends — the program produces nothing at all until it has read to
//!    end of file, so any output is proof the half-close arrived.
//! 5. **A directory selects a seed, and a host owns only a default.** The real CLI opens a pane
//!    with `new-session -c <sibling seed>/src`, a seed this daemon was never told about. That
//!    pane publishes into the sibling while the `api` job goes on publishing into the first, in
//!    the same host, at the same time; neither seed sees the other's bytes. Typing `sd b-sibling
//!    /src` at the sibling pane's prompt opens its new job on the *sibling* seed, because the
//!    REPL's leading `/` names the seed of the shell the line was typed in and not the daemon's
//!    default.
//! 6. **A typed `exit` ends exactly its own pane.** `exit 7` typed into a real prompt closes that
//!    job with status 7 through brush's own builtin, its pane and session leave the client's pane
//!    list, and the session beside it is still there in the same listing.
//!
//! The fixture is a pair of sibling btrfs subvolumes under `$HOME`. If the first cannot be
//! created the driver falls back to [`CopyTree`](marsh_btrfs::fake::CopyTree) through the same
//! public host API and says so on stdout — in which case this run is proof of the API and CLI
//! behaviour above, and not of kernel btrfs behaviour. That changes the fixture, never the
//! shipped behaviour: nothing here adds a fake mode to the product.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marsh::rmux::types::{
    CommandCompletion, CommandOptions, DaemonConfig, EnsureSession, JobIo, Outcome, ProcessCommand,
    RunError, SessionName, ShellEnvironment, ShellId, SpawnOptions, Subvolumes, TerminalGeometry,
};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, IoError, OutputLimit, OverflowPolicy, RmuxFrontend, ShellHandle,
};

/// How long any single observation may take before the driver declares it unmet.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Any failure at all: this is a driver, and the first thing that does not hold ends it.
type Failure = Box<dyn std::error::Error>;

/// The seeds this run publishes into, and how they were made.
struct Fixture {
    /// The scratch root. The socket lives here, *outside* either seed.
    scratch: PathBuf,
    /// The first seed's root, which is also the host's default directory.
    seed: PathBuf,
    /// A sibling seed, holding `src/`, that nothing but a pane's own `-c` argument names.
    other: PathBuf,
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
    fn new() -> Result<(Self, Arc<dyn Subvolumes>), Failure> {
        let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
        let scratch = home.join(format!("marsh-rmux-smoke.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let seed = scratch.join("seed");
        let other = scratch.join("other");

        let btrfs = marsh_btrfs::LibBtrfs;
        let (filesystem, real_btrfs): (Arc<dyn Subvolumes>, bool) =
            match marsh_btrfs::Subvolumes::create_subvolume(&btrfs, &seed) {
                Ok(()) => (Arc::new(marsh_btrfs::LibBtrfs), true),
                Err(error) => {
                    println!("  ! real btrfs seed unavailable ({error}); falling back to CopyTree");
                    marsh_btrfs::delete_subvolume(&seed);
                    let _ = std::fs::remove_dir_all(&seed);
                    let fake: Arc<dyn Subvolumes> = Arc::new(marsh_btrfs::fake::CopyTree::new());
                    fake.create_subvolume(&seed)?;
                    (fake, false)
                }
            };

        filesystem.create_subvolume(&other)?;
        std::fs::create_dir_all(other.join("src"))?;

        let fixture = Self {
            scratch,
            seed,
            other,
            real_btrfs,
        };
        Ok((fixture, filesystem))
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
        self.scratch.join("rmux.sock")
    }

    /// Reclaims every subvolume this run made, then the scratch root.
    ///
    /// Job snapshots are subvolumes too, and `remove_dir_all` cannot delete one, so they go
    /// first — for both seeds, since either may have hosted jobs.
    fn cleanup(self) {
        if self.real_btrfs {
            for seed in [&self.seed, &self.other] {
                let Some(name) = seed.file_name() else {
                    continue;
                };
                let snaps = self.scratch.join(".marsh").join(name).join("snap");
                if let Ok(entries) = std::fs::read_dir(&snaps) {
                    for entry in entries.flatten() {
                        marsh_btrfs::delete_subvolume(&entry.path());
                    }
                }
                marsh_btrfs::delete_subvolume(seed);
            }
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
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
            "{} does not exist; run `cargo build -p marsh --bin rmux` first",
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
fn rmux(binary: &Path, socket: &Path, args: &[&str]) -> Result<String, Failure> {
    let output = std::process::Command::new(binary)
        .arg("-N")
        .arg("-S")
        .arg(socket)
        .args(args)
        .env("TERM", "xterm-256color")
        .output()?;
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
    let outcome = match completion.outcome.as_ref() {
        Ok(Outcome::Published { granted, .. }) => format!("Published({} granted)", granted.len()),
        Ok(Outcome::Denied { denials, .. }) => format!("Denied({} refused)", denials.len()),
        Ok(Outcome::Discarded) => "Discarded".to_owned(),
        Ok(Outcome::Detached) => "Detached".to_owned(),
        Err(error) => format!("error: {error}"),
    };
    format!("exit={:?} outcome={outcome}", completion.exit_code)
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
    assert_eq!(
        names.iter().filter(|name| **name == "api").count(),
        1,
        "`api` appears exactly once: {names:?}"
    );

    // The other half of the two-step API: a name is resolved once, and everything after this runs
    // through the retrieved object. It is the generation that was just created rather than a
    // second shell that happens to answer to the same name, which is what the uid below says.
    let job = io.shell(&id)?;
    assert_eq!(
        job.sandbox().uid,
        created.sandbox().uid,
        "`io.shell` retrieves the shell that was created, not another one"
    );

    // The marker is assembled from two pieces on purpose: seeing the echoed command line on the
    // pane is not the same as seeing the command's output.
    let published = submit(&job, "printf owner > owned; printf '%s%s\\n' RMUX_ API").await?;
    println!("[3] `printf owner > owned` -> {}", describe(&published));
    assert_eq!(published.exit_code, Some(0), "the process said zero");
    assert!(
        published.is_published(),
        "and the gate agreed: {}",
        describe(&published)
    );
    assert_eq!(
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

    let sessions = rmux(binary, socket, &["list-sessions", "-F", "#{session_name}"])?;
    println!("[4] rmux -N -S … list-sessions -> {:?}", sessions.trim());
    assert!(
        sessions.contains("smoke"),
        "the external client sees this process's session: {sessions:?}"
    );

    let screen = until("the pane to show RMUX_API", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target]).ok()?;
        text.lines()
            .any(|line| line.trim_end() == "RMUX_API")
            .then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[5] rmux -N -S … capture-pane -t {target} -> {shown:?}");
    Ok(())
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
    let denied = match refused {
        Err(IoError::Run(RunError::Policy(denied))) => denied,
        other => return Err(format!("a zero exit is not an approval: {other:?}").into()),
    };
    let completion = denied.completion();
    println!(
        "[7] `other` overwriting `api`'s file -> {}",
        describe(completion)
    );
    assert_eq!(completion.exit_code, Some(0), "the process still said zero");
    assert!(
        matches!(
            completion.outcome.as_ref(),
            Ok(Outcome::Denied { denials, .. }) if !denials.is_empty()
        ),
        "and the refusal names what it refused: {}",
        describe(completion)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed("owned"))?,
        "owner",
        "the seed still holds the first principal's bytes"
    );
    // The line ended with `exit 0`, so this shell closed itself: an explicit stop here would be a
    // stale-handle error, and waiting for the closure is what proves the refusal did not keep it
    // open. A denied publication is still a finished command.
    tokio::time::timeout(TIMEOUT, intruder.wait_closed()).await??;
    assert!(
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
    assert_eq!(
        captured.stdout, b"a\0b\r\n\xff",
        "stdout is byte-identical: no line discipline, no CR insertion, no stop at the NUL"
    );
    assert_eq!(
        captured.stderr, b"E\0R\xfe",
        "stderr is its own stream and is never merged into stdout"
    );
    assert!(!captured.truncated);
    assert_eq!(captured.completion.exit_code, Some(0));
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

    rmux(
        binary,
        socket,
        &["new-session", "-d", "-s", "other-seed", "-c", start],
    )?;

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
    assert_eq!(
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
    assert_eq!(published.exit_code, Some(0));
    assert!(
        published.is_published(),
        "the sibling seed publishes on its own: {}",
        describe(&published)
    );
    assert_eq!(
        std::fs::read(second.join("src").join("marker"))?,
        b"second",
        "the bytes land in the seed the pane's directory chose"
    );
    assert!(
        !first.join("marker").exists() && !first.join("src").join("marker").exists(),
        "and nowhere in the host's default seed"
    );

    // The CLI's detached session presents that job at its first pane, so this is the target the
    // client names.
    let target = pane_target(io, &b).await?;
    assert_eq!(
        target, "other-seed:0.0",
        "the CLI's session presents B here"
    );
    // Matched anywhere on the screen rather than as a whole line: a CLI pane draws a prompt and
    // the output lands right after it. The marker is still unambiguous, because the echoed
    // command line spells it `second- pane` and only the program's own output spells it joined.
    let screen = until("the second seed's pane to show second-pane", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target]).ok()?;
        text.contains("second-pane").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[11] rmux -N -S … capture-pane -t {target} -> {shown:?}");

    // Both are live in the same host at the same instant: the API job still publishes into the
    // first seed while that pane holds the second.
    let again = submit(
        api,
        "printf first > first-again; printf '%s%s\\n' first-pane -again",
    )
    .await?;
    println!(
        "[12] first seed `printf first > first-again` -> {}",
        describe(&again)
    );
    assert!(
        again.is_published(),
        "the first seed is still this job's: {}",
        describe(&again)
    );
    assert_eq!(std::fs::read(first.join("first-again"))?, b"first");
    assert!(
        !second.join("src").join("first-again").exists(),
        "the two seeds never cross"
    );
    let target = pane_target(io, api).await?;
    let screen = until("the first seed's pane to show first-pane-again", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", &target]).ok()?;
        text.contains("first-pane-again").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = screen.lines().filter(|line| !line.is_empty()).collect();
    println!("[13] rmux -N -S … capture-pane -t {target} -> {shown:?}");

    prove_prompt_sibling(io, &b, &view.id, &second).await
}

/// A line typed at B's prompt, not a request made of the host.
///
/// Its leading `/` is the seed of the shell it was typed in — if it were the daemon's default,
/// this sibling would land on the first seed instead, and the assertion below is the whole
/// difference.
async fn prove_prompt_sibling(
    io: &RmuxFrontend,
    b: &ShellHandle,
    prompt: &ShellId,
    second: &Path,
) -> Result<(), Failure> {
    until("the second seed's prompt to go idle", || async {
        io.job(prompt)
            .filter(|view| view.running.is_none() && !view.starting)
            .map(|_| ())
    })
    .await?;
    io.write_input(b, b"sd b-sibling /src\r").await?;
    let sibling_id = ShellId::from("b-sibling");
    let sibling = until("`b-sibling` to open", || async { io.job(&sibling_id) }).await?;
    println!(
        "[14] `sd b-sibling /src` at B's prompt -> seed {} dir {:?}",
        sibling.sandbox.seed.display(),
        sibling.sandbox.dir.as_str()
    );
    assert_eq!(
        sibling.sandbox.seed, second,
        "the REPL's `/` names the originating shell's seed, never the daemon's default"
    );
    assert_eq!(sibling.sandbox.dir.as_str(), "src");

    let seeds: Vec<String> = io
        .seeds()
        .into_iter()
        .map(|info| info.seed.display().to_string())
        .collect();
    println!("[15] io.seeds() -> {seeds:?}");
    assert_eq!(seeds.len(), 2, "one host, two opened seeds: {seeds:?}");
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
    )?;

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
    rmux(
        binary,
        socket,
        &[
            "send-keys",
            "-t",
            "exit-smoke",
            "--wait-next-text",
            "EXIT_READY",
            "--timeout",
            "5s",
            "--",
            "printf '%s%s\\n' EXIT_ READY",
            "Enter",
        ],
    )?;

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
    assert_eq!(
        completion.exit_code,
        Some(7),
        "the builtin's argument is the status the job ended with"
    );
    assert!(
        completion.is_published(),
        "and the line was gated like any other: {}",
        describe(completion)
    );
    assert!(
        io.job(job.id()).is_none(),
        "the exited job is gone rather than idle"
    );

    let listed = until("`exit-smoke` to leave the pane list", || async {
        let text = rmux(
            binary,
            socket,
            &["list-panes", "-a", "-F", "#{session_name}"],
        )
        .ok()?;
        let removed = !text.lines().any(|line| line.trim() == "exit-smoke");
        let survivor = text.lines().any(|line| line.trim() == "smoke");
        (removed && survivor).then_some(text)
    })
    .await?;
    let shown: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    println!("[exit] rmux -N -S … list-panes -a -F '#{{session_name}}' -> {shown:?}");
    println!("[exit] status=7; target removed; survivor alive");
    Ok(())
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
/// never an instruction to rerun. The second half is the other verdict: a line that read a file
/// another shell then republished is evaluated again against the new bytes, with nothing but the
/// final result reported.
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
        )?;
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
    )?;
    let owned = fixture.seed("test.txt");
    until("the writer's publication to reach the seed", || async {
        std::fs::read_to_string(&owned).ok().filter(|text| text == "foo\n")
    })
    .await?;
    println!("[caps] `echo foo > test.txt` at caps-writer -> seed holds {:?}", "foo\n");

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
    )?;
    let reported = until("the reader's pane to report a verdict", || async {
        let text = rmux(binary, socket, &["capture-pane", "-p", "-t", "caps-reader"]).ok()?;
        text.contains("denied").then_some(text)
    })
    .await?;
    let shown: Vec<&str> = reported.lines().filter(|line| !line.is_empty()).collect();
    println!("[caps] rmux -N -S … capture-pane -t caps-reader -> {shown:?}");
    assert!(
        reported.contains("unstaged"),
        "the report names the owner's unstaged precondition: {shown:?}"
    );
    assert!(
        !reported.contains("stale") && !reported.contains("merged by seq"),
        "and never asks the user to rerun a line that would be refused again: {shown:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&owned)?,
        "foo\n",
        "the seed still holds the owner's bytes, byte for byte"
    );

    prove_read_dependency(fixture, &writer, &reader).await?;

    io.stop(&writer, false).await?;
    io.stop(&reader, false).await?;
    Ok(())
}

/// A held line that read a file another shell then republished is evaluated again.
///
/// Driven through two live persistent shells of the running daemon, not through a parser: what is
/// proved is that the *host* reacts to an observed read, and that only the last evaluation of the
/// line asks for anything.
async fn prove_read_dependency(
    fixture: &Fixture,
    writer: &ShellHandle,
    reader: &ShellHandle,
) -> Result<(), Failure> {
    submit(writer, "printf 'old\\n' > sync.txt").await?;
    let attempts = fixture
        .seed
        .parent()
        .ok_or("the seed has no parent")?
        .join("caps-attempts");
    let gate = fixture
        .seed
        .parent()
        .ok_or("the seed has no parent")?
        .join("caps-gate");
    let held = format!(
        "printf 'x\\n' >> {}; while [ ! -e {} ]; do sleep 0.05; done; /bin/cat sync.txt > \
         observed.txt",
        attempts.display(),
        gate.display()
    );
    let running = tokio::spawn({
        let reader = reader.clone();
        async move { reader.run_command(&held, CommandOptions::default()).await }
    });
    until("the reader's first evaluation", || async {
        std::fs::read_to_string(&attempts)
            .ok()
            .filter(|text| text.lines().count() >= 1)
    })
    .await?;

    submit(writer, "printf 'new\\n' > sync.txt").await?;
    std::fs::write(&gate, b"")?;
    let completion = tokio::time::timeout(TIMEOUT, running).await???;
    println!(
        "[caps] held read of sync.txt -> {}",
        describe(&completion)
    );
    assert!(
        completion.is_published(),
        "the replay published: {}",
        describe(&completion)
    );
    let evaluations = std::fs::read_to_string(&attempts)?.lines().count();
    println!(
        "[caps] observed.txt = {:?} after {evaluations} evaluation(s)",
        std::fs::read_to_string(fixture.seed("observed.txt"))?
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed("observed.txt"))?,
        "new\n",
        "the line read the bytes that are current, not the ones it started against"
    );
    assert_eq!(evaluations, 2, "one invalidation, one replay");
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

    let view = io.switch(&job).await?;
    let current = io.current_job().map(|view| view.id.as_str().to_owned());
    println!(
        "[6] io.switch(\"{}\") -> current = {current:?}",
        view.id.as_str()
    );
    assert_eq!(current.as_deref(), Some("api"), "the selection is `api`");

    // The frontend agreeing with itself is not the observation. The claim is that a *native*
    // switch reaches the presentation, so the real client is asked which pane and which window it
    // considers active, and both answers have to be this shell's.
    let target = pane_target(io, &job).await?;
    let active = until(
        "the client to report `api`'s pane and window active",
        || async {
            let text = rmux(
                binary,
                socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &target,
                    "#{pane_active} #{window_active}",
                ],
            )
            .ok()?;
            (text.trim() == "1 1").then_some(text)
        },
    )
    .await?;
    println!(
        "[6b] rmux -N -S … display-message -t {target} '#{{pane_active}} #{{window_active}}' \
         -> {:?}",
        active.trim()
    );

    prove_denial(io, fixture).await?;
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Failure> {
    let binary = rmux_binary()?;
    println!("rmux executable: {}", binary.display());

    let (fixture, filesystem) = Fixture::new()?;
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
    let host = RmuxFrontend::open_with(
        DaemonConfig::new(socket.clone()),
        &fixture.seed,
        ShellEnvironment::new(),
        TerminalGeometry { rows: 24, cols: 80 },
        filesystem,
    )
    .await?;

    // Everything below goes through the frontend's own operations. There is no `host.mux()`, and
    // this driver would not compile if it reached for one.
    let result = drive(&host, &fixture, &binary, &socket).await;

    host.shutdown().await?;
    fixture.cleanup();
    result?;
    println!("\nrmux_smoke: every observation held.");
    Ok(())
}
