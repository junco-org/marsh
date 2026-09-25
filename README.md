# rmux-marsh

A tmux-style terminal multiplexer whose panes are not host processes. Every pane, popup and
workload helper is a job on one shell multiplexer over one btrfs subvolume — the *seed*. A pane
runs an embedded [brush](https://github.com/reubeno/brush) interpreter; each submitted line is
staged in that job's own snapshot; and its effects reach the seed only once
[junco-policy](https://github.com/junco-org/junco-policy) has granted every capability the line
amounts to. A refused line leaves the seed untouched while the pane stays open, which is why a
process exit status and an approval are two separate results here.

Two ways in, and they are the same system: the `rmux` command, and `marsh::rmux::RmuxFrontend` in
a Rust program.

## Running rmux-marsh

### What the build and the seed need

* Linux. btrfs subvolume ioctls exist nowhere else.
* The Rust toolchain named by `rust-version` in the root manifest — currently **1.95** — or newer.
* A C build toolchain, **Clang/libclang**, and **libbtrfsutil** where the linker can find it.
  A Rust toolchain alone is not enough: `marsh-btrfs` pins `btrfsutil` 0.2.0, whose
  `btrfsutil-sys` 1.3.0 build script generates bindings with bindgen from its bundled header and
  links against `btrfsutil`. On Debian/Ubuntu that is `clang libclang-dev libbtrfsutil-dev`; on
  Fedora, `clang-devel libbtrfsutil-devel`.
* **Git** and **btrfs-progs** for the setup commands below.
* **`strace`**, and permission for the daemon to trace itself. Which files a line touched is
  observed through one tracer attached to the host, and a host that cannot attach one fails
  closed rather than publishing work it never saw. Under `kernel.yama.ptrace_scope=1` — the
  common default — nothing has to be configured: the daemon names its own tracer through
  `PR_SET_PTRACER`. Under `ptrace_scope=2` or `3`, or inside a sandbox that forbids `ptrace`
  outright, marsh will not start a shell. The tracer's output is spooled through private,
  already-unlinked files in `/tmp`, which must support `fallocate` hole punching (tmpfs, ext4,
  xfs and btrfs do); the kernel must be Linux 5.3 or newer, for pidfds.
* A writable **btrfs mount carrying `user_subvol_rm_allowed`**, so an unprivileged user can
  reclaim job snapshots. Check it with `findmnt -T <path> -o FSTYPE,OPTIONS`.

The seed must be a **dedicated subvolume**, not the mount's root subvolume: state lives beside it,
at `<seed-parent>/.marsh/<seed-name>/`, so the parent directory has to be writable and the seed has
to have a parent at all. A seed that is its own mount root is refused rather than putting state at
the filesystem root.

Seed discovery starts at the **daemon process's own working directory** and walks up to the first
enclosing subvolume. That is true of `rmux -D` and of the hidden auto-start entrypoint alike, which
is why every command below runs the daemon from inside the seed. The `--config-cwd` option is about
resolving relative paths in configuration files and has nothing to do with it.

### Setting up and starting a daemon

From the checkout root, using a fresh disposable example root:

```sh
cargo build --release -p marsh --bin rmux
RMUX="$PWD/target/release/rmux"
WORK="$(mktemp -d "$HOME/rmux-marsh.XXXXXX")"
btrfs subvolume create "$WORK/seed"
git -C "$WORK/seed" init -b main
git -C "$WORK/seed" -c user.name=Example -c user.email=example@example.invalid \
  commit --allow-empty -m seed
cd "$WORK/seed"
"$RMUX" -D -f /dev/null -S "$WORK/rmux.sock"
```

`$HOME` must be on the suitable btrfs mount for this to work as written; if it is not, put `WORK`
somewhere that is. If Cargo writes to a custom target directory, set `RMUX` to that directory's
`release/rmux` instead.

The example initializes Git **before** the managed shell starts only to give it a first commit.
The managed `git` builtin runs the system `git` — any subcommand, `init` and `clone` included —
through the shell's recorded spawner, inside the pane's snapshot: host Git configuration is
ignored, nothing outside the snapshot is written, and what each invocation did to each path is
requested from the policy before the line is published.
`-f /dev/null` isolates the example from your own rmux configuration. The socket is deliberately
outside the seed, so it is never part of what a workload could publish.

### Using it

In another terminal, with the same absolute `RMUX` and `WORK` values:

```sh
"$RMUX" -N -S "$WORK/rmux.sock" new-session -s demo
```

`-N` means connect-only: this client talks to the daemon that is already running and never starts
one of its own.

Now **inside the new pane** — these are not host-shell commands, they are lines submitted to the
managed shell:

```sh
printf hello > greeting.txt
git add -- greeting.txt
```

`Ctrl-b d` detaches. From the host shell again:

```sh
"$RMUX" -N -S "$WORK/rmux.sock" attach-session -t demo
"$RMUX" -N -S "$WORK/rmux.sock" kill-server
```

As an alternative to the foreground daemon, and only once any previous daemon over this seed has
stopped, running this from inside the seed starts a daemon automatically and attaches to it:

```sh
"$RMUX" -f /dev/null -S "$WORK/rmux.sock" new-session -s demo
```

One daemon leases one seed exclusively. A second daemon over the same seed is refused, whichever
way it was started.

### What is on disk, and what "published" means

State lives beside the seed:

| Path | What it is |
| --- | --- |
| `<seed-parent>/.marsh/<seed-name>/snap/` | one btrfs snapshot per live job |
| `<seed-parent>/.marsh/<seed-name>/meta/wal.jsonl` | the write-ahead log every publication goes through |
| `<seed-parent>/.marsh/<seed-name>/meta/runs/<uid>/` | one job's spawn, builtin and file-access record streams |

Each submitted shell line is staged in its job's snapshot. After the line ends, its filesystem
effects are diffed against the seed, its `git` requests are translated into capability events, and
the policy either grants **all** of them — the transaction is logged and applied to the seed — or
refuses, in which case the snapshot is retaken from the seed and the line's changes are gone.

Which files a line actually read and wrote is observed rather than guessed at: a `strace` attached
to the host reports every path-taking syscall of every shell and every process they start, and each
job's share is written to `meta/runs/<uid>/trace.log` beside its other record streams. Tracing is
required — a host that may not `ptrace` itself fails with the prerequisite rather than publishing
unobserved work.

That is what makes two panes over one seed independent rather than merely serialized:

* A line that **read** a file another pane publishes while it is still running is unwound and
  evaluated again against the new bytes. Its abandoned attempt requests nothing, and its own
  output, stdin consumption and writes outside the snapshot may repeat — a replay is the same
  line run again, not a rollback.
* A line that **wrote** a file another pane owns unstaged is refused, because running it a second
  time would be refused for the same reason. That is an ownership decision, and the capability
  policy is what makes it.
* Lines that touch disjoint files never wait for each other, whatever order they publish in.

This staging is *not* Git's index. `git add -- path` is a runtime **Stage** request: it releases
that path's unstaged ownership so another principal may edit it. `printf hello > greeting.txt`
above is an **Edit** that publishes and leaves `greeting.txt` owned by that pane's snapshot until
it is staged.

Two consequences worth planning for:

* A refused line leaves the seed unchanged **even when its process exited zero**. The exit status
  is the program's; the approval is the gate's.
* Published grants are durable and belong to the snapshot that earned them, never to a reusable
  job name. A path left unstaged when the daemon stops stays owned by a principal that no longer
  exists, and restarting does not hand the next holder that stake. Stage what you want released
  before shutting down. A library caller that controls a stable agent identity may instead open
  its shell with `SpawnOptions { durable: true, .. }`, whose grants that same name resumes after a
  reopen; rmux panes never do.

## Using `RmuxFrontend` as a library

### The consuming project

`marsh` is not published, so a consumer depends on a checkout by path. A minimal `Cargo.toml`:

```toml
[package]
name = "rmux-frontend-example"
version = "0.1.0"
edition = "2024"

[dependencies]
marsh = { path = "/absolute/path/to/marsh", default-features = false }
tokio = { version = "1.52.3", features = ["macros", "rt-multi-thread", "time"] }

[patch.crates-io]
brush-core = { path = "/absolute/path/to/marsh/crates/brush-core" }
rmux-server = { path = "/absolute/path/to/marsh/crates/rmux-server" }
```

Adjust both paths to your checkout. The `[patch.crates-io]` section must live in the **consuming
workspace's own root manifest**: Cargo ignores `[patch]` sections in dependency manifests, so
without it `marsh`'s `rmux-server = "=0.10.0"` and `brush-core = "0.5.0"` would resolve to
crates.io copies that do not carry the local server or the `ExternalCommandSpawner` seam. See the
Cargo Book on
[working with an unpublished minor version](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#working-with-an-unpublished-minor-version).
The patched versions are the in-tree ones: `brush-core` 0.5.0 and `rmux-server` 0.10.0, matching
what crates.io currently publishes for `brush-core`, so no version selection has to change.

No direct dependency on `brush-core`, `rmux-core`, `rmux-proto`, `rmux-sdk` or `marsh-core` is
needed. Every argument and result type the interface mentions is re-exported from
`marsh::rmux::types`.

### The program

This is [`examples/rmux_api.rs`](examples/rmux_api.rs) verbatim; drop it into `src/main.rs`.

```rust
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
```

Run it from the consumer project, after the CLI daemon from the first section has stopped:

```sh
cargo run -- "$WORK/seed" "$WORK/rmux.sock"
```

Or, in the checkout:

```sh
cargo run -p marsh --example rmux_api -- "$WORK/seed" "$WORK/rmux.sock"
```

Both consume the Git-initialized seed the first section prepared. The example's `owned` and
`released` must not already carry ownership from an earlier run, so use a **fresh seed** for each
run — repeat the `btrfs subvolume create` / `git init` / empty-commit sequence.

### The interface, and the lifecycle

**The validator is the caller's.** `Arc<Mutex<marsh::PolicyValidator>>` is marsh's existing
junco-policy `GitPolicy` adapter together with its committed history — not an internally chosen
global and not a new policy abstraction. A raw junco `GitPolicy` borrows a non-`Sync` arena, so
this adapter is the cross-thread contract. Use a fresh validator per independently governed seed;
reopening with an empty one still adopts that seed's durable grants before a single command is
accepted, which is exactly what step 8 above observes.

**Construction.** `RmuxFrontend::open` opens and replays the seed and binds one engine and one
server. `open_with` is the same thing over an explicitly supplied `Subvolumes` backend, which only
a test fixture normally needs. Normal consumers never construct an executor, a multiplexer, a
shell profile, a callback queue or an observation task.

**Ownership.** `RmuxFrontend` is the unique owner: of the seed's exclusive lease, of the
multiplexer, and of the listener task. Its operations are `ShellIo`'s, reached through `Deref`, so
`frontend.spawn(...)`, `frontend.execute(...)` and `frontend.observe()` are all direct calls.
`io()` explicitly returns a **cloneable native-client lease** for concurrent code — a task, a
thread, a struct field — and those handles neither create a second service nor own the server
task. Use `io()` rather than treating the owner as cloneable; it is deliberately neither `Clone`
nor `DerefMut`. `shutdown(self)` ends the listener and releases the seed however many handles are
still held; `wait(self)` drops the owner's lease and joins a server that something else stopped or
that went idle; owner `Drop` requests shutdown but cannot await it.

**Results.** `spawn` returns a job handle. `start_in` *returning* is admission, not completion:
inspect `CommandHandle::wait()` and its typed `Outcome`, not only `exit_code`. `execute` opens a
pipe job — separate byte-exact stdout and stderr, and a real end-of-file on input — while terminal
jobs have one merged stream instead. `observe` gives an atomic snapshot plus an event
subscription, where falling behind is an explicit error rather than silent loss. SDK handles and
`open_protocol` reach the same bound socket; protocol I/O after the connect is blocking and belongs
on a blocking worker, as the example shows.

**Durability.** Rights belong to snapshot uids, never to reused job names. An unstaged path stays
owned after the job that wrote it has vanished, and restarting does not reset that. Stage the work
you want released before closing its owner. Discarding cancels unpublished work; it does not undo
a grant already published into the seed. The example leaves `owned` unstaged on purpose, to show
the refusal after a restart, and stages `released`, to show that the release survives one too.

**What this is not.** It is a publication and policy boundary, not OS confinement. Network effects
and writes to absolute paths outside the seed are neither transactional nor sandboxed by this
interface.

## License

MIT. See [LICENSE](LICENSE).
