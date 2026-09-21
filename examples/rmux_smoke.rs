#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "a smoke driver's failure mode is an immediate, loud abort naming the observation \
              that did not hold"
)]
//! A throwaway public-API smoke driver: `cargo run -p marsh --example rmux_smoke`.
//!
//! Plan line 587. Where `rmux_api.rs` is the documented library sample, this one is the wider
//! behavioural driver: it leases a real btrfs seed, opens a frontend over it, and drives the
//! running system through nothing but that frontend and the handles it hands out. There is no raw
//! mux here and no accessor for one; every observation below is one an embedding application can
//! make for itself.
//!
//! What it proves, in order:
//!
//! 1. A native job named `api` is created, submitted into, selected and stopped through `io`, and
//!    its verdict is inspected as a *typed* completion — the exit code and the publication outcome
//!    read separately, because a zero exit is not an approval.
//! 2. The **same** daemon is reachable from outside: while that job is live, the real `rmux -S
//!    <socket>` executable captures the pane presenting it. One daemon, one seed, two front doors.
//! 3. A second principal's overwrite of the first's file exits zero and is *denied*, and the seed
//!    keeps the original bytes.
//! 4. A real pipe execution carries binary stdout and stderr independently and byte for byte, and
//!    its standard input really ends — the program produces nothing at all until it has read to
//!    end of file, so any output is proof the half-close arrived.
//!
//! The fixture is a real btrfs subvolume under `$HOME`. If one cannot be created the driver falls
//! back to [`CopyTree`](marsh_btrfs::fake::CopyTree) through the same public host API and says so
//! on stdout; that changes the fixture, never the shipped behaviour. Nothing here adds a fake mode
//! to the product.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marsh::rmux::types::{
    CommandCompletion, CommandOptions, DaemonConfig, EnsureSession, JobIo, Outcome, ProcessCommand,
    SessionName, ShellEnvironment, ShellId, SpawnOptions, Subvolumes, TerminalGeometry,
};
use marsh::rmux::{
    CollectOptions, ExecutionSpec, OutputLimit, OverflowPolicy, RmuxFrontend, ShellHandle,
};

/// How long any single observation may take before the driver declares it unmet.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Any failure at all: this is a driver, and the first thing that does not hold ends it.
type Failure = Box<dyn std::error::Error>;

/// The seed this run publishes into, and how it was made.
struct Fixture {
    /// The scratch root. The socket lives here, *outside* the seed.
    scratch: PathBuf,
    /// The seed's root.
    seed: PathBuf,
    /// A real btrfs subvolume, or the copy-tree fallback.
    real_btrfs: bool,
}

impl Fixture {
    /// Creates a real btrfs seed under `$HOME`, or falls back to a copy tree.
    ///
    /// Only *creating the subvolume* decides which backend this returns. A seed that was made and
    /// then could not be opened is a real failure, not a reason to quietly swap in a fixture and
    /// report success against something the product never ships.
    fn new() -> Result<(Self, Arc<dyn Subvolumes>), Failure> {
        let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
        let scratch = home.join(format!("marsh-rmux-smoke.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let seed = scratch.join("seed");

        let btrfs = marsh_btrfs::LibBtrfs;
        match marsh_btrfs::Subvolumes::create_subvolume(&btrfs, &seed) {
            Ok(()) => {
                let fixture = Self {
                    scratch,
                    seed,
                    real_btrfs: true,
                };
                Ok((fixture, Arc::new(marsh_btrfs::LibBtrfs)))
            }
            Err(error) => {
                println!("  ! real btrfs seed unavailable ({error}); falling back to CopyTree");
                marsh_btrfs::delete_subvolume(&seed);
                let _ = std::fs::remove_dir_all(&seed);
                std::fs::create_dir_all(&seed)?;
                let fs = Arc::new(marsh_btrfs::fake::CopyTree::new());
                fs.register(&seed);
                let fixture = Self {
                    scratch,
                    seed,
                    real_btrfs: false,
                };
                Ok((fixture, fs))
            }
        }
    }

    /// A path inside the seed, so the driver can read back what was published.
    fn seed(&self, path: &str) -> PathBuf {
        self.seed.join(path)
    }

    /// The daemon's socket, outside the seed so it is never part of what is published.
    fn socket(&self) -> PathBuf {
        self.scratch.join("rmux.sock")
    }

    /// Reclaims every subvolume this run made, then the scratch root.
    ///
    /// Job snapshots are subvolumes too, and `remove_dir_all` cannot delete one, so they go first.
    fn cleanup(self) {
        if self.real_btrfs {
            let snaps = self.scratch.join(".marsh").join("seed").join("snap");
            if let Ok(entries) = std::fs::read_dir(&snaps) {
                for entry in entries.flatten() {
                    marsh_btrfs::delete_subvolume(&entry.path());
                }
            }
            marsh_btrfs::delete_subvolume(&self.seed);
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
        Ok(Outcome::Stale { stale, .. }) => format!("Stale({} paths)", stale.len()),
        Ok(Outcome::Discarded) => "Discarded".to_owned(),
        Ok(Outcome::Detached) => "Detached".to_owned(),
        Err(error) => format!("error: {error}"),
    };
    format!("exit={:?} outcome={outcome}", completion.exit_code)
}

/// Submits one line into an open job and waits for its verdict.
async fn submit(
    io: &RmuxFrontend,
    job: &ShellHandle,
    line: &str,
) -> Result<Arc<CommandCompletion>, Failure> {
    let command = io.start_in(job, line, CommandOptions::default()).await?;
    Ok(tokio::time::timeout(TIMEOUT, command.wait()).await??)
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

    let job = io
        .spawn(
            "",
            Some(ShellId::from("api")),
            None,
            SpawnOptions {
                io: JobIo::Terminal {
                    geometry: Some(TerminalGeometry { rows: 24, cols: 80 }),
                },
                environment: None,
            },
        )
        .await?;
    let jobs = io.jobs();
    let names: Vec<&str> = jobs.iter().map(|view| view.id.as_str()).collect();
    println!("[2] io.spawn(\"api\") -> jobs {names:?}");
    assert_eq!(
        names.iter().filter(|name| **name == "api").count(),
        1,
        "`api` appears exactly once: {names:?}"
    );

    // The marker is assembled from two pieces on purpose: seeing the echoed command line on the
    // pane is not the same as seeing the command's output.
    let published = submit(io, &job, "printf owner > owned; printf '%s%s\\n' RMUX_ API").await?;
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
    let pane = io.pane_for(job).await?;
    let reference = pane.target();
    let target = format!(
        "{}:{}.{}",
        reference.session_name.as_str(),
        reference.window_index,
        reference.pane_index
    );

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
async fn prove_denial(io: &RmuxFrontend, fixture: &Fixture) -> Result<(), Failure> {
    let intruder = io
        .spawn(
            "",
            Some(ShellId::from("other")),
            None,
            SpawnOptions::default(),
        )
        .await?;
    let denied = submit(io, &intruder, "printf other > owned; exit 0").await?;
    println!("[7] `other` overwriting `api`'s file -> {}", describe(&denied));
    assert_eq!(denied.exit_code, Some(0), "the process still said zero");
    assert!(
        matches!(denied.outcome.as_ref(), Ok(Outcome::Denied { .. })),
        "a zero exit is not an approval: {}",
        describe(&denied)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed("owned"))?,
        "owner",
        "the seed still holds the first principal's bytes"
    );
    io.stop(&intruder, false).await?;
    Ok(())
}

/// One real pipe execution: two independent binary streams and a genuine end of file.
async fn prove_pipe(io: &RmuxFrontend) -> Result<(), Failure> {
    // The program writes nothing until `read()` returns, so any output at all proves stdin ended.
    let execution = io
        .execute(ExecutionSpec {
            directory: String::new(),
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
        captured.stdout,
        b"a\0b\r\n\xff",
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

    prove_denial(io, fixture).await?;
    prove_pipe(io).await?;

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
    println!("[9] io.stop(\"api\") -> remaining jobs {remaining:?}");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Failure> {
    let binary = rmux_binary()?;
    println!("rmux executable: {}", binary.display());

    let (fixture, filesystem) = Fixture::new()?;
    let socket = fixture.socket();
    println!(
        "fixture: seed={} ({}), socket={}\n",
        fixture.seed.display(),
        if fixture.real_btrfs {
            "real btrfs subvolume"
        } else {
            "CopyTree fallback"
        },
        socket.display()
    );

    let host = RmuxFrontend::open_with(
        DaemonConfig::new(socket.clone()),
        &fixture.seed,
        Arc::new(Mutex::new(marsh::PolicyValidator::new())),
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
