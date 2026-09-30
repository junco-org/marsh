# rmux-marsh

A tmux-style terminal multiplexer with persistent, in-process
[Brush](https://github.com/reubeno/brush) shells. A per-command sandbox policy routes each accepted
command either through four private managed stages:

```text
immutable command baseline → observed execution → capability authorization → durable WAL merge
```

or directly against its source. By default a shell sandboxes exactly while another live shell in
the same process shares its source; a shell alone on its source runs commands natively.

Shells over the same canonical source share one authority. A managed command executes once:
conflicting newer data returns `Stale`, never an automatic replay. Supported managed changes reach
the source only when the complete [junco-policy](https://github.com/junco-org/junco-policy) batch
is granted.

The interfaces are `marsh::Shell`, the `rmux` executable, and `marsh::rmux::RmuxFrontend`.
None requires a caller to allocate snapshots, manage a validator, replay a log or finalize a line.
This controls publication, **not OS confinement**. Terminal output, network effects, external
FIFO/device operations, writes outside the private work view and every effect of a direct command
are not rolled back.

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

Observation is in-process. Marsh imports unmodified crates.io `lurk-cli = "=0.3.14"`; lurk is
neither vendored nor patched, and no companion, installed `strace` or `lurk` executable is used.

Marsh's observation wrapper reuses lurk's public syscall types, argument tables and filters.
It supplies the stopped-task callbacks and filesystem identities absent from the released tracer
API. Its records contain an upstream `SyscallInfo` plus raw path bytes, descriptor identities and
entry order; no renderer output is reparsed. Linux lets a process trace its descendants but never
its own threads, so each command a managed run spawns parks in a handshake between its launch
setup and `exec`, where a dedicated tracer thread of the host seizes it; that thread then follows
and reaps the command's whole process tree. The interpreter's own filesystem accesses
(redirections, tests, globs, PATH lookups, sourced scripts) are reported by brush-core as host
records in the same vocabulary. A launch the tracer cannot seize never runs, and failed
observation fails closed. No trace spool, sysctl changes or fallback tracer is involved.

`io_uring` operations bypass per-operation syscalls, so at every `io_uring_enter` stop the tracer
reads the opcodes pending in that ring's submission queue. A ring is accepted only without a
kernel submission thread (`IORING_SETUP_SQPOLL`) and while every submission has no filesystem
effect (poll, timeout, cancel, `EPOLL_CTL`); that is exactly libuv's event-loop batching, so Node
children work. Unknown rings, unreadable queues, any other opcode and `io_uring_register` are
refused. Other threads of the tracee can rewrite the queue between that read and the kernel's
consumption; like every argument read, this is observation, not confinement.

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

### Sandbox routing

`ShellBuilder::sandbox_policy(SandboxPolicy)` (and `MuxProfile::sandbox_policy` for every shell of
a mux) selects the route of each accepted call. A policy is one recursive Rust value:

```rust,no_run
use marsh::{CommandContext, SandboxPolicy, ShellCommand};

fn scoped(ctx: &CommandContext<'_>) -> bool {
    ctx.tool_as::<ShellCommand>()
        .is_some_and(|shell| shell.command.starts_with("make"))
}

let policy = SandboxPolicy::or(SandboxPolicy::SharedSource, SandboxPolicy::Base(scoped));
```

`SandboxPolicy::allow()` always sandboxes, `forbid()` never does, `SharedSource` (the default) is
true iff another live shell — any `marsh::Shell` in this process, idle or not, identified by its
principal — has the same source root, and `and`/`or` short-circuit left to right. True selects the
managed route; false runs the call in the same persistent interpreter against the source, with
no snapshot, tracing, authorization or WAL. These permit or forbid *sandboxing*, not execution.

Every routed call is a `MarshTool`: `Any + Send + Sync + Into<Action>` (junco-policy's `Action`)
plus a `description` that traces and published WAL metadata record. A shell span is a
`ShellCommand` carrying the accepted top-level text (the submitted string byte for byte; a script
path; a function name; empty for startup, prompt and end-of-input spans; the exact buffer of an
interactive completion request), classified conservatively as `Action::Edit` whatever that text
does. A `CommandContext` carries the original call (`tool_as::<T>()` downcasts it to its concrete
type, `None` for any other), its `Action`, converted once from a borrow before its type is erased,
the current shell's `shellmux::Sandbox` record, every live shell's record, and the source's shared
`PolicyValidator`, whose `decide` queries committed history without adopting anything. `Base`
predicates must be synchronous, read-only and must not call back into a shell; a panicking
predicate fails the call.

A shell's source root is the canonical Git work-tree root containing its initial directory, or that
directory itself outside a work tree, fixed for the shell's lifetime. Commands queue on the
shallower of that root and the Btrfs seed: overlapping managed commands run concurrently, while a
direct command or recovery excludes every overlapping command. Waiting commands re-evaluate the
policy as shells join or leave and as authority changes. A command issued from inside another live
command returns `Busy` rather than wait behind a conflict. Durable state is recovered before the
first routing decision on a source; a source that requires recovery refuses both routes. A true
route never falls back to direct when managed storage is unavailable.

Route changes keep variables, functions, the logical working directory and `cd -`. Descriptors
bound inside the view being left are revoked rather than reopened elsewhere, and caller-supplied
parameters carrying a descriptor into a private view are refused. On the direct route, builtin
context I/O and `git` act on the source natively.

### Native builtins

Use `marsh::builtins::{builtin, simple_builtin, decl_builtin, raw_arg_builtin}` with Brush's existing
command traits. Registrations are opaque and local to each shell; no global hook installer replaces
another mux's implementation.

`marsh::builtins::current_context()` provides logical `working_dir`, `open`, `metadata`,
`create_dir_all`, `remove_file`, `read_dir` and `glob`, plus cancellation and tracked
`spawn_blocking`; `physical_path`/`logical_path` map a path into and out of the run's view for
engines that read it directly. Retained contexts/iterators refuse I/O after their run ends. Trusted
native plugins must register spawned work through this context; arbitrary unregistered Rust threads
are not a safe extension mechanism.

### Tool calls

`Shell::run_tool(tool, operation)` runs an embedder's call as one accepted call routed like a
command: the policy sees the concrete `tool` (`tool_as::<T>()`) and the `Action` converted from a
borrow of it before its type is erased. `operation` receives a `BuiltinContext` on a registered,
trace-scoped blocking thread and returns a value the caller gets back once the call finished — on
the managed route, only once its effects were published. A process cannot trace its own threads, so
the view's changes are evidence only when made through the context's I/O methods; any other change
refuses publication. On the direct route the operation acts on the source. A refused, failed or
interrupted call drops the operation's result. `ShellMux::run_tool(directory, tool, operation)` runs
one such call in a transient shell built with the mux's builder and profile policy; the shell has
no streams, is not a job, and closes when the call ends.

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
* Managed Git preserves causal Stage/Commit/Checkout semantics. On the managed route,
  direct/descendant Git and repository metadata mutations outside a successful managed invocation
  are refused. `git add` releases an unstaged stake; a reused display name never inherits an older
  shell instance's authority.
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
brush-interactive = { path = "/absolute/path/to/marsh/crates/brush-interactive" }
rmux-server = { path = "/absolute/path/to/marsh/crates/rmux-server" }
```

Cargo ignores patches in dependency manifests. Only brush-core 0.5.0, brush-interactive 0.4.0 and
rmux-server 0.10.0 need the local patches above; lurk-cli 0.3.14 comes directly from crates.io.

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
cargo test -p brush-interactive --lib --features basic
cargo test -p marsh-core --lib
cargo test -p rmux-server --lib pane_repl::
cargo test -p marsh --test sandbox_policy --test shell --test shellmux --test builtins --test git_shell --test rmux --test rmux_cli
env -u RMUX -u TMUX cargo run -p marsh --example rmux_smoke
```

The smoke first execs itself with `--native-trace-only` and a temporary PATH containing only Git,
before the parent attaches any tracer. It checks that shell construction attaches the tracer
before any command or storage and that closing every shell detaches it, then normal Shell
sharing/recovery, Read enforcement, explicit read+edit and zero-op grant durability. The parent
keeps an idle standalone shell on each seed, so every pane routes through the managed stages, then
exercises the real rmux executable, independent sources (the sibling seed's first pane command
resetting an incompatible WAL), native file I/O, stale-without-replay and natural pane exit.

`marsh-core/testing` and `rmux-server/testing` provide explicit CopyTree/configured-builder
factories for deterministic tests. No production constructor accepts a backend. The smoke reports
whether it used real btrfs or CopyTree; only real btrfs proves readonly subvolume ioctls.

The private publication regression captures explicitly Internal-scoped calls through the
test-only `marsh-instrument/testing` feature. It correlates the completed WAL frame with
native intent sync, staged-payload fsync, namespace fsync and END sync in that order. This is
syscall-ordering evidence, not a power-cut/storage-fault proof.

## License

MIT. See [LICENSE](LICENSE).
