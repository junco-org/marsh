# rmux-marsh

A tmux-style terminal multiplexer with persistent, in-process
[Brush](https://github.com/reubeno/brush) shells. Each shell owns four private stages:

```text
immutable command baseline → observed execution → capability authorization → durable WAL merge
```

Shells over the same canonical source share one authority. A command executes once: conflicting
newer data returns `Stale`, never an automatic replay. Supported changes reach the source only
when the complete [junco-policy](https://github.com/junco-org/junco-policy) batch is granted.

The interfaces are `marsh::Shell`, the `rmux` executable, and `marsh::rmux::RmuxFrontend`.
None requires a caller to allocate snapshots, manage a validator, replay a log or finalize a line.
This controls publication, **not OS confinement**. Terminal output, network effects, external
FIFO/device operations and writes outside the private work view are not rolled back.

## Build and distribution

Requirements:

* Linux 5.3+ with procfs, usable `ptrace`/`PTRACE_GET_SYSCALL_INFO`, and pidfds.
* Rust 1.95 or newer, as specified by the root manifest.
* A C toolchain, Clang/libclang and libbtrfsutil. Debian/Ubuntu packages:
  `clang libclang-dev libbtrfsutil-dev`; Fedora: `clang-devel libbtrfsutil-devel`.
* Git and btrfs-progs for the setup commands below.
* A writable btrfs mount with `user_subvol_rm_allowed` for production source snapshots.
  Inspect it with `findmnt -T <path> -o FSTYPE,OPTIONS`.

Build or install **both** package binaries:

```sh
cargo build --release -p marsh --bins
# Alternatively:
cargo install --path . --bins
```

`marsh-trace` is the package-owned companion to `rmux` and embedded library applications. It
imports unmodified crates.io `lurk-cli = "=0.3.14"`; lurk is neither vendored nor patched. No
installed `strace` or `lurk` executable is used. Bundle `marsh-trace` beside the application's
executable. Cargo test/example executables also locate the sibling in their parent output
directory. There is no PATH search or caller-selected tracing backend.

Marsh's observation wrapper reuses lurk's public syscall types, argument tables and filters.
It supplies the stopped-task callbacks, all-thread attachment and filesystem identities absent
from the released tracer API. Its records contain an upstream `SyscallInfo` plus raw path bytes,
descriptor identities and entry order; no renderer output is reparsed. The host grants its exact
helper child `PR_SET_PTRACER` permission. The service's monitor thread creates and reaps the helper,
so Linux's thread-bound parent-death signal cannot tie it to a short-lived caller runtime.
Records use a bounded Unix socket queue; pinned process identities travel as pidfds. Kernel/LSM
refusal, incompatible helpers, queue overflow and lost observation fail closed. No trace spool,
sysctl changes or fallback tracer is involved.

## Start a disposable daemon

A source must be a dedicated subvolume, not its mount's root. Its parent must be writable because
private state lives at `<source-parent>/.marsh/<source-name>/`.

From the checkout root:

```sh
cargo build --release -p marsh --bins
RMUX="$PWD/target/release/rmux"
WORK="$(mktemp -d "$HOME/rmux-marsh.XXXXXX")"
btrfs subvolume create "$WORK/seed"
git -C "$WORK/seed" init -b main
git -C "$WORK/seed" -c user.name=Example -c user.email=example@example.invalid \
  commit --allow-empty -m seed
cd "$WORK/seed"
"$RMUX" -D -f /dev/null -S "$WORK/rmux.sock"
```

Use a suitable btrfs mount instead of `$HOME` if necessary. `-f /dev/null` isolates this example
from your rmux configuration. Keep the socket outside the source.

From another terminal, with the same absolute `RMUX` and `WORK`:

```sh
"$RMUX" -N -S "$WORK/rmux.sock" new-session -s demo
```

Inside the pane:

```sh
printf hello > greeting.txt
git add -- greeting.txt
```

`Ctrl-b d` detaches. Host commands can reattach or stop this disposable daemon:

```sh
"$RMUX" -N -S "$WORK/rmux.sock" attach-session -t demo
"$RMUX" -N -S "$WORK/rmux.sock" kill-server
```

`-N` is connect-only. Without it, a client may start a daemon when its endpoint is absent.
A daemon's initial directory is a request default, not a source lease: each shell discovers its
source from its own logical working directory. One daemon can therefore serve independent sources.
Within a process, canonical aliases share authority; another process is excluded by the source lease.

## Ordinary Shell API

```rust,no_run
use marsh::{Shell, ShellError, ShellErrorKind};

async fn example(source: &std::path::Path) -> Result<(), ShellError> {
    let a = Shell::new(source).await?;
    let b = Shell::new(source).await?;
    a.run("export KEPT=value; printf first > owned").await?;
    assert!(a.env_var("KEPT").await.is_some());

    let error = b.run("printf blind > owned").await.err().expect("denied");
    assert!(matches!(error.kind(), ShellErrorKind::Denied { .. }));
    assert_eq!(u8::from(error.execution_result().unwrap().exit_code), 0);

    a.close(false).await?;
    b.close(false).await?;
    Ok(())
}
```

`Shell::builder()` configures ordinary cwd, environment, variables, fds, builtin registrations,
options, arguments, rc/profile loading and interactive behavior. Startup scripts explicitly
requested by the caller go through the same boundary. Defaults inherit the environment and skip
rc/profile files.

`run`, `run_string`, `run_script`, `source_script` and `invoke_function` retain shell variables,
functions and cwd. State queries return owned ordinary Brush values; `env_var` avoids copying the
whole environment. Working directories and builtin file operations use logical source paths.

`Ok(ExecutionResult)` means supported effects were accepted, including a genuine nonzero exit.
`ShellError::kind()` distinguishes `Busy`, `Closed`, `Denied`, `Stale`, `Interrupted`, `Unsupported`
and `Infrastructure`. `execution_result()` preserves any native status/control flow obtained before
a later failure; underlying causes remain available through `Error::source()`.

Only one command is admitted at a time. Dropping its caller's future does not drop the boundary or
detach producers. `close(false)` waits for accepted work; `close(true)` requests cancellation and
joins/kills owned producers before discard. Natural `exit` closes the shell even if finalization
subsequently fails. Closed handles keep observations and identity, not the live interpreter or source
lease. `run_interactively(UIOptions)` owns prompts, line editing, completion, history and EXIT work;
the blocking editor requires a multi-thread runtime. Noninteractive methods also support a
current-thread runtime.

`Shell::principal()` returns junco-policy's `Principal`, used unchanged for live authorization,
durable grants and retained-handle identity. Display names never select ownership. There is no
separate Marsh principal type, compatibility namespace or caller-supplied principal.

### Native builtins

Use `marsh::builtins::{builtin, simple_builtin, decl_builtin, raw_arg_builtin}` with Brush's existing
command traits. Registrations are opaque and local to each shell; no global hook installer replaces
another mux's implementation.

`marsh::builtins::current_context()` provides logical `working_dir`, `open`, `metadata`,
`create_dir_all`, `read_dir` and `glob`, plus cancellation and tracked `spawn_blocking`. Retained
contexts/iterators refuse I/O after their run ends. Trusted native plugins must register spawned work
through this context; arbitrary unregistered Rust threads are not a safe extension mechanism.

## Publication and recovery

* A fresh immutable baseline is captured per command. The candidate diff is baseline→work, not
  work→a source that another shell may already have changed. Unexplained changes are refused.
* Actual reads acquire Read claims. A blind competing edit can be denied while an explicit read
  followed by an edit is permitted. Write intent still requires Edit when final bytes are unchanged;
  grant-only transactions are durable.
* Exact paths, lookup dependencies, directory membership and recursive replacements participate in
  freshness. Disjoint changes can both publish. A stale command's output, stdin and variables have
  already happened once; the line is not reoffered or reexecuted.
* Persistent descriptors survive unchanged work generations. Access through a descriptor or mapping
  from a necessarily retired generation fails rather than silently rebinding paths or offsets.
* Managed Git preserves causal Stage/Commit/Checkout semantics. Direct/descendant Git and repository
  metadata mutations outside a successful managed invocation are refused. `git add` releases an
  unstaged stake; a reused display name never inherits an older shell instance's authority.
* Directories, including empty directories and modes, have explicit operations. FIFO/socket/device
  publication, unsupported metadata effects and unrepresentable hard-link mutations are refused
  before intent. Directory removal is nonrecursive and replay never follows symlink ancestors.
* Quiescent work is frozen as readonly redo before intent. The WAL durably records the complete
  counted intent and grants, applies descriptor-relative operations with namespace fsyncs, and ends
  with durable `END`. Any possibly written intent failure blocks the source until reopen recovery.
  After `END`, cleanup failure cannot relabel committed bytes as unpublished.
* A complete WAL record that cannot be decoded — malformed JSON, an obsolete or incompatible
  record schema, or missing/obsolete ownership metadata — resets startup: the whole log, including
  valid prefixes, suffixes and pending intent, is atomically replaced by an empty durable log and
  nothing is replayed. Source bytes stay as they are; the discarded grants are gone. Failing to
  replace the log refuses startup with it unchanged. Records that decode but carry impossible
  framing, paths or sources still refuse startup unchanged. Only a genuinely torn final append is
  truncated. Recovery checks source kind, digest and mode before copying; an absent source is
  accepted only if the target proves the operation already applied.
* Reopen reconciles externally deleted source paths without forgetting other paths' grants. A
  deletion recorded by the WAL is not treated as an external disappearance. Missing/obsolete
  ownership metadata and old uncounted or untyped WAL records are never interpreted as empty
  ownership or silently migrated; they reset the log as above.

Private operator layout:

| Path below `.marsh/<source-name>/` | Contents |
| --- | --- |
| `snap/` | work views and retained per-command readonly baseline/redo subvolumes |
| `meta/wal.jsonl` | filesystem intent and durable grants |
| `meta/runs/<principal>/trace.log` | append-only JSON observations: upstream syscall info plus captured filesystem metadata |

Each command appends its owned observations once. Transaction staging is owned by its validated
WAL frame, not inferred from user filename suffixes. A legitimate filename ending `.tmp-wal`
is ordinary source content.

## RmuxFrontend library integration

A downstream Cargo workspace patches the vendored dependencies at its own root:

```toml
[dependencies]
marsh = { path = "/absolute/path/to/marsh", default-features = false }
tokio = { version = "1.52.3", features = ["macros", "rt-multi-thread", "time"] }

[patch.crates-io]
brush-core = { path = "/absolute/path/to/marsh/crates/brush-core" }
rmux-server = { path = "/absolute/path/to/marsh/crates/rmux-server" }
```

Cargo ignores patches in dependency manifests. Only Brush 0.5.0 and rmux-server 0.10.0 need the
local patches above; lurk-cli 0.3.14 comes directly from crates.io. Bundle the built `marsh-trace`
alongside the consuming executable.

[`examples/rmux_api.rs`](examples/rmux_api.rs) uses only the ordinary public interface. On a fresh
Git-initialized source and unused socket:

```sh
cargo run -p marsh --example rmux_api -- "$WORK/seed" "$WORK/rmux.sock"
```

`RmuxFrontend::open` binds the daemon; `io()` returns a cloneable native-client lease. `shutdown(self)`
closes its listener and shells even when handles remain; `wait(self)` waits for an external/idle stop.
`CommandCompletion.result` is the one `Arc<Result<ExecutionResult, ShellError>>`; `exit_code()` derives
the native status. `RunError::Execution` carries that full completion. No facade exposes a validator,
seed-history query, snapshot parent or recovery flag.

Pipe executions preserve independent stdout/stderr bytes and real stdin EOF. Terminal jobs retain
idle PTY leases, geometry and byte pumps. `observe()` reports an atomic snapshot plus ordered events;
falling behind is explicit. SDK and protocol connections address this same daemon.

## Verification

The `marsh-core` library suite includes the real readonly-redo/recovery regression by default. It
requires a writable btrfs `$HOME` mounted with `user_subvol_rm_allowed`; missing prerequisites fail
the test instead of skipping it.

```sh
cargo build -p marsh --bins
cargo test -p marsh-instrument
cargo test -p marsh-btrfs -p marsh-wal
cargo test -p brush-core --test external_command_spawner_tests
cargo test -p marsh-core --lib
cargo test -p rmux-server --lib pane_repl::
cargo test -p marsh --test shell --test shellmux --test builtins --test git_shell --test rmux --test rmux_cli
env -u RMUX -u TMUX cargo run -p marsh --example rmux_smoke
```

The smoke first execs itself with `--native-trace-only` and a temporary PATH containing only Git,
before the parent attaches any tracer. It checks normal Shell sharing/recovery, Read enforcement,
explicit read+edit and zero-op grant durability. The parent then exercises the real rmux executable,
independent sources (the sibling seed's pane starting over an incompatible WAL, which it resets),
native file I/O, stale-without-replay and natural pane exit.

`marsh-core/testing` and `rmux-server/testing` provide explicit CopyTree/configured-builder
factories for deterministic tests. No production constructor accepts a backend. The smoke reports
whether it used real btrfs or CopyTree; only real btrfs proves readonly subvolume ioctls.

The private publication regression captures explicitly Internal-scoped calls through the
test-only `marsh-instrument/testing` feature. It correlates the completed WAL frame with
native intent sync, staged-payload fsync, namespace fsync and END sync in that order. This is
syscall-ordering evidence, not a power-cut/storage-fault proof.

## License

MIT. See [LICENSE](LICENSE).
