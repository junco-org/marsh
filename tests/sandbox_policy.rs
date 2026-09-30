//! Per-command sandbox routing, observed through host bytes, native statuses and error kinds.
//!
//! A managed command's source effects appear only when it completes; a direct command's appear
//! while it is still running. Every routing claim below is judged by holding a command at a pipe
//! barrier and looking at the source, or by a route that cannot succeed at all on storage that
//! was never registered.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module
)]

mod common;

use std::borrow::Cow;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Controlled, Seed, TIMEOUT, controlled, denied, join, launch, run, scratch};
use junco_policy::{Event, PolicyDecision, Resource};
use marsh::{
    Action, CommandContext, ExecutionResult, MarshTool, RcLoadBehavior, SandboxPolicy, Shell,
    ShellBuilder, ShellCommand, ShellError, ShellErrorKind, ShellVariable, SourceInfo,
};
use marsh_btrfs::fake::CopyTree;
use serial_test::serial;
use tempfile::TempDir;

/// How long a command that must stay queued is given to show it did not.
const QUEUED: Duration = Duration::from_millis(300);

/// Host bytes at `path`, or `None` when it does not exist.
fn host(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// A variable's scalar value.
fn string(variable: &ShellVariable) -> String {
    match variable.value() {
        brush_core::ShellValue::String(value) => value.clone(),
        other => panic!("not a scalar: {other:?}"),
    }
}

/// Runs `line` followed by a `READY` barrier on `shell`, and returns the host bytes of `watched`
/// at the barrier together with the command's verdict once it is released.
async fn held(
    shell: &mut Controlled,
    watched: &Path,
    line: &str,
) -> (Option<Vec<u8>>, Result<ExecutionResult, ShellError>) {
    let task = launch(
        &shell.shell,
        format!("{line}; printf 'READY\\n'; read release"),
    );
    shell.ready().await;
    let during = host(watched);
    assert!(!task.is_finished(), "the barrier holds the command");
    shell.release();
    (during, join(task).await)
}

/// Requires that `task` neither finishes nor writes `witness` within [`QUEUED`].
async fn stays_queued(
    task: &mut tokio::task::JoinHandle<Result<ExecutionResult, ShellError>>,
    witness: &Path,
) {
    assert!(
        tokio::time::timeout(QUEUED, &mut *task).await.is_err(),
        "the overlapping command waits"
    );
    assert!(!witness.exists(), "no queued user code ran");
}

/// A scratch source with one file, on a test backend that never registered it as storage.
struct Bare {
    root: TempDir,
    source: PathBuf,
    fs: Arc<CopyTree>,
}
impl Bare {
    fn new() -> Self {
        let (root, source) = scratch();
        std::fs::write(source.join("a.txt"), "seed\n").unwrap();
        Self {
            root,
            source,
            fs: Arc::new(CopyTree::new()),
        }
    }
    fn builder(&self) -> ShellBuilder {
        self.at(self.source.clone())
    }
    fn at(&self, dir: PathBuf) -> ShellBuilder {
        marsh_core::test_support::shell_builder(self.fs.clone()).working_dir(dir)
    }
}

/// Makes `dir` a Git work tree with one commit of whatever it holds.
fn git_init(dir: &Path) {
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "initial",
        ],
    ] {
        let status = std::process::Command::new("/bin/git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

fn infrastructure<T>(result: Result<T, ShellError>) {
    let error = result.err().expect("the managed route has no storage");
    assert!(
        matches!(error.kind(), ShellErrorKind::Infrastructure),
        "{error}"
    );
}

const EXACT: &str = "printf  scoped > marker; printf 'READY\\n'; read release # keep\n";

fn exact(ctx: &CommandContext<'_>) -> bool {
    ctx.tool_as::<ShellCommand>()
        .is_some_and(|shell| shell.command == EXACT)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn routing_sees_the_exact_accepted_command_text() {
    let seed = Seed::new("a.txt", "seed\n");
    let mut a = controlled(seed.builder().sandbox_policy(SandboxPolicy::Base(exact))).await;
    let marker = seed.source.join("marker");

    let task = launch(&a.shell, EXACT.into());
    a.ready().await;
    assert_eq!(host(&marker), None, "the exact text is managed");
    a.release();
    join(task).await.unwrap();
    assert_eq!(host(&marker).unwrap(), b"scoped");

    run(&*a.shell, "printf before > marker").await;
    assert_eq!(host(&marker).unwrap(), b"before");

    let task = launch(&a.shell, EXACT.replacen("printf  ", "printf ", 1));
    a.ready().await;
    assert_eq!(
        host(&marker).unwrap(),
        b"scoped",
        "one byte of difference runs directly"
    );
    a.release();
    join(task).await.unwrap();
    a.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_solitary_shell_runs_directly_without_storage() {
    let bare = Bare::new();
    let mut a = controlled(bare.builder()).await;
    let marker = bare.source.join("marker");
    let (during, result) = held(&mut a, &marker, "printf raw > marker").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"raw");

    let result = run(&*a.shell, "printf partial > marker; false").await;
    assert_eq!(u8::from(result.exit_code), 1);
    assert_eq!(host(&marker).unwrap(), b"partial");
    a.shell.close(false).await.unwrap();

    let entries: Vec<_> = std::fs::read_dir(bare.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["source"], "no persistent state beside the source");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn peers_switch_a_live_shell_between_routes_without_losing_state() {
    for symlink in [false, true] {
        let seed = Seed::new("src/a.txt", "seed\n");
        seed.git();
        let alias = seed.outside("alias");
        std::os::unix::fs::symlink(&seed.source, &alias).unwrap();
        let (peer_dir, relative) = if symlink {
            (alias.clone(), "marker")
        } else {
            (seed.source.join("src"), "../marker")
        };
        let marker = seed.source.join("marker");
        let mut a = controlled(seed.builder()).await;
        run(&*a.shell, "x=kept; f() { printf fn; }; cd src; cd ..").await;

        let (during, result) = held(&mut a, &marker, "printf raw > marker").await;
        result.unwrap();
        assert_eq!(during.unwrap(), b"raw", "alone, A runs directly");

        let b = seed
            .builder()
            .working_dir(peer_dir.clone())
            .build()
            .await
            .unwrap();
        let (during, result) = held(&mut a, &marker, "printf staged > marker").await;
        result.unwrap();
        assert_eq!(during.unwrap(), b"raw", "an idle peer makes A managed");
        assert_eq!(host(&marker).unwrap(), b"staged");
        assert_eq!(a.shell.working_dir().await, seed.source);
        denied(&b, &format!("printf B > {relative}")).await;
        assert_eq!(host(&marker).unwrap(), b"staged");

        b.close(false).await.unwrap();
        let (during, result) = held(&mut a, &marker, "printf restored > marker").await;
        result.unwrap();
        assert_eq!(
            during.unwrap(),
            b"restored",
            "the closed peer no longer counts"
        );

        let b = seed.builder().working_dir(peer_dir).build().await.unwrap();
        run(&b, &format!("v=$(cat {relative})")).await;
        assert_eq!(string(&b.env_var("v").await.unwrap()), "restored");
        b.close(false).await.unwrap();

        run(
            &*a.shell,
            "printf '%s:%s' \"$x\" \"$(f)\" > state; cd - >/dev/null",
        )
        .await;
        assert_eq!(host(&seed.source.join("state")).unwrap(), b"kept:fn");
        assert_eq!(a.shell.working_dir().await, seed.source.join("src"));
        a.shell.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn policy_roots_are_worktrees_or_initial_directories() {
    // Linked work trees of one repository, both inside one registered storage seed.
    let (root, store) = scratch();
    let main = store.join("main");
    std::fs::create_dir(&main).unwrap();
    std::fs::write(main.join("a.txt"), "seed\n").unwrap();
    git_init(&main);
    let status = std::process::Command::new("/bin/git")
        .current_dir(&main)
        .args(["worktree", "add", "-q", "-b", "linked", "../linked"])
        .status()
        .unwrap();
    assert!(status.success());
    let fs = Arc::new(CopyTree::new());
    fs.register(&store);
    let builder =
        |dir: &Path| marsh_core::test_support::shell_builder(fs.clone()).working_dir(dir.into());
    let mut a = controlled(builder(&main)).await;
    let mut w = controlled(builder(&store.join("linked"))).await;
    let (during, result) = held(&mut a, &main.join("marker"), "printf main > marker").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"main");
    let linked = store.join("linked/marker");
    let (during, result) = held(&mut w, &linked, "printf linked > marker").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"linked");
    a.shell.close(false).await.unwrap();
    w.shell.close(false).await.unwrap();

    // Two repositories inside one registered storage seed.
    for repository in ["one", "two"] {
        std::fs::create_dir(store.join(repository)).unwrap();
        git_init(&store.join(repository));
    }
    let mut one = controlled(builder(&store.join("one"))).await;
    let two = builder(&store.join("two")).build().await.unwrap();
    let marker = store.join("one/marker");
    let (during, result) = held(&mut one, &marker, "printf one > marker").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"one");
    one.shell.close(false).await.unwrap();
    two.close(false).await.unwrap();

    // Outside Git, initial directories are compared.
    let plain = Seed::new("sub/a.txt", "seed\n");
    let mut a = controlled(plain.builder()).await;
    let sub = plain
        .builder()
        .working_dir(plain.source.join("sub"))
        .build()
        .await
        .unwrap();
    let marker = plain.source.join("marker");
    let (during, result) = held(&mut a, &marker, "printf distinct > marker").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"distinct");
    let same = plain.builder().build().await.unwrap();
    let (during, result) = held(&mut a, &marker, "printf shared > marker").await;
    result.unwrap();
    assert_eq!(
        during.unwrap(),
        b"distinct",
        "the same initial directory is shared"
    );
    assert_eq!(host(&marker).unwrap(), b"shared");
    for shell in [sub, same] {
        shell.close(false).await.unwrap();
    }
    a.shell.close(false).await.unwrap();
    drop(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn an_unavailable_managed_route_fails_closed_and_leaves_no_phantom_peer() {
    let bare = Bare::new();
    git_init(&bare.source);
    let a = bare.builder().build().await.unwrap();
    let b = bare.builder().build().await.unwrap();
    infrastructure(a.run("printf managed > marker").await);
    assert!(!bare.source.join("marker").exists());
    b.close(false).await.unwrap();
    run(&a, "printf direct > marker").await;
    assert_eq!(host(&bare.source.join("marker")).unwrap(), b"direct");

    // A retained closed handle is not a peer.
    let closed = bare.builder().build().await.unwrap();
    closed.close(false).await.unwrap();
    run(&a, "printf retained > marker").await;
    assert_eq!(host(&bare.source.join("marker")).unwrap(), b"retained");

    // A shell whose startup failed is not a peer.
    let rc = bare.root.path().join("rc");
    std::fs::write(&rc, "printf startup > startup\n").unwrap();
    infrastructure(
        bare.builder()
            .sandbox_policy(SandboxPolicy::allow())
            .interactive(true)
            .rc(RcLoadBehavior::LoadCustom(rc))
            .build()
            .await,
    );
    assert!(!bare.source.join("startup").exists());
    run(&a, "printf after-startup > marker").await;
    assert_eq!(host(&bare.source.join("marker")).unwrap(), b"after-startup");

    // Nor is one whose initial directory has no source-relative name.
    let unnamed = bare
        .source
        .join(std::ffi::OsStr::from_bytes(b"not-\xffutf8"));
    std::fs::create_dir(&unnamed).unwrap();
    infrastructure(bare.at(unnamed).build().await);
    run(&a, "printf after-unnamed > marker").await;
    assert_eq!(host(&bare.source.join("marker")).unwrap(), b"after-unnamed");
    drop(closed);
    a.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn overlapping_commands_wait_for_direct_work() {
    let seed = Seed::new("a.txt", "seed\n");
    let other = Seed::new("a.txt", "seed\n");
    let marker = seed.source.join("marker");
    let witness = seed.outside("started");
    let mut a = controlled(seed.builder()).await;
    let writer = launch(
        &a.shell,
        "printf first > marker; printf 'READY\\n'; read release; printf final > marker".into(),
    );
    a.ready().await;
    assert_eq!(host(&marker).unwrap(), b"first");
    let b = Arc::new(seed.builder().build().await.unwrap());
    let mut reader = launch(
        &b,
        format!(
            "printf x > {}; v=$(cat marker); printf %s \"$v\" > copy",
            witness.display()
        ),
    );
    stays_queued(&mut reader, &witness).await;

    // An unrelated source is not held behind this one.
    let unrelated = other.builder().build().await.unwrap();
    run(&unrelated, "printf free > free").await;
    assert_eq!(other.bytes("free"), b"free");
    unrelated.close(false).await.unwrap();

    a.release();
    join(writer).await.unwrap();
    join(reader).await.unwrap();
    assert_eq!(
        seed.bytes("copy"),
        b"final",
        "the managed read saw the final bytes"
    );
    b.close(false).await.unwrap();
    a.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn read_tools_finish_before_overlapping_writers() {
    for (writer_policy, expected) in [
        (SandboxPolicy::forbid(), b"during\n".as_slice()),
        (SandboxPolicy::allow(), b"seed\n".as_slice()),
    ] {
        let seed = Seed::new("src/a.txt", "seed\n");
        let mut writer = controlled(seed.builder().sandbox_policy(writer_policy)).await;
        let writer_task = launch(
            &writer.shell,
            "printf 'during\n' > src/a.txt; printf 'READY\n'; read release; printf 'after\n' > src/a.txt".into(),
        );
        writer.ready().await;

        let witness = seed.outside("queued-writer-started");
        let queued_shell = Arc::new(
            seed.builder()
                .sandbox_policy(SandboxPolicy::forbid())
                .build()
                .await
                .unwrap(),
        );
        let mut queued = launch(&queued_shell, format!("printf queued > {}", witness.display()));
        stays_queued(&mut queued, &witness).await;

        let read_shell = seed
            .builder()
            .working_dir(seed.source.join("src"))
            .sandbox_policy(SandboxPolicy::Base(writes))
            .build()
            .await
            .unwrap();
        let observed = tokio::time::timeout(
            TIMEOUT,
            read_shell.run_tool(Inspect, |ctx| {
                let physical = ctx.physical_path(Path::new("a.txt")).unwrap();
                let bytes = std::fs::read(&physical).unwrap();
                (physical, bytes)
            }),
        )
        .await;
        let writer_held = !writer_task.is_finished();
        let queued_held = !queued.is_finished();
        let witness_absent = !witness.exists();

        writer.release();
        let writer_result = join(writer_task).await;
        let queued_result = join(queued).await;

        let (physical, bytes) = observed.expect("read finishes before bash exits").unwrap();
        assert_eq!(physical, seed.source.join("src/a.txt"));
        assert_eq!(bytes, expected);
        assert!(writer_held && queued_held, "both writers were held during the read");
        assert!(witness_absent, "the queued writer did not run");
        writer_result.unwrap();
        queued_result.unwrap();
        assert_eq!(seed.bytes("src/a.txt"), b"after\n");
        assert_eq!(host(&witness).unwrap(), b"queued");
        read_shell.close(false).await.unwrap();
        queued_shell.close(false).await.unwrap();
        writer.shell.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn closing_a_queued_shell_interrupts_its_wait() {
    let seed = Seed::new("a.txt", "seed\n");
    let witness = seed.outside("started");
    let mut a = controlled(seed.builder()).await;
    let writer = launch(
        &a.shell,
        "printf 'READY\\n'; read release; printf final > marker".into(),
    );
    a.ready().await;
    let b = Arc::new(seed.builder().build().await.unwrap());
    let mut queued = launch(&b, format!("printf x > {}", witness.display()));
    stays_queued(&mut queued, &witness).await;
    tokio::time::timeout(TIMEOUT, b.close(true))
        .await
        .unwrap()
        .unwrap();
    let error = join(queued).await.err().expect("cancelled while queued");
    assert!(
        matches!(
            error.kind(),
            ShellErrorKind::Interrupted | ShellErrorKind::Closed
        ),
        "{error}"
    );
    assert!(!witness.exists());
    a.release();
    join(writer).await.unwrap();
    assert_eq!(seed.bytes("marker"), b"final");
    a.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn repositories_sharing_a_storage_seed_share_admission() {
    let (_root, store) = scratch();
    for repository in ["one", "two"] {
        std::fs::create_dir(store.join(repository)).unwrap();
        git_init(&store.join(repository));
    }
    let fs = Arc::new(CopyTree::new());
    fs.register(&store);
    let builder =
        |dir: PathBuf| marsh_core::test_support::shell_builder(fs.clone()).working_dir(dir);
    let mut one = controlled(builder(store.join("one"))).await;
    let two = Arc::new(builder(store.join("two")).build().await.unwrap());
    let writer = launch(
        &one.shell,
        "printf 'READY\\n'; read release; printf final > marker".into(),
    );
    one.ready().await;
    let witness = store.join("two/marker");
    let mut queued = launch(&two, "printf two > marker".into());
    stays_queued(&mut queued, &witness).await;
    one.release();
    join(writer).await.unwrap();
    join(queued).await.unwrap();
    assert_eq!(host(&witness).unwrap(), b"two");
    assert_eq!(host(&store.join("one/marker")).unwrap(), b"final");
    one.shell.close(false).await.unwrap();
    two.close(false).await.unwrap();
}

/// Managed exactly when this shell's blind edit of `src/a.txt` would not be granted.
fn edit_denied(ctx: &CommandContext<'_>) -> bool {
    let event = Event::new(
        ctx.current.uid.clone(),
        Action::Edit,
        Resource::from(["src", "a.txt"]),
    );
    !matches!(ctx.validator.decide(&event), Ok(PolicyDecision::Grant))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn predicates_query_the_sources_shared_authority() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let policy = || SandboxPolicy::Base(edit_denied);
    let other = seed.source.join("src/b.txt");
    let mut a = controlled(seed.builder().sandbox_policy(policy())).await;
    let peer = seed.managed_builder().build().await.unwrap();
    let (during, result) = held(&mut a, &other, "printf raw > src/b.txt").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"raw", "empty history grants the edit");

    run(&peer, "/bin/cat src/a.txt >/dev/null").await;
    let (during, result) = held(&mut a, &other, "printf managed > src/b.txt").await;
    result.unwrap();
    assert_eq!(during.unwrap(), b"raw", "the peer's read makes A managed");
    assert_eq!(seed.bytes("src/b.txt"), b"managed");
    denied(&*a.shell, "printf B > src/a.txt").await;
    assert_eq!(seed.bytes("src/a.txt"), b"seed\n");
    a.shell.close(false).await.unwrap();
    peer.close(false).await.unwrap();

    let reopened = seed
        .builder()
        .sandbox_policy(policy())
        .build()
        .await
        .unwrap();
    denied(&reopened, "printf C > src/a.txt").await;
    assert_eq!(seed.bytes("src/a.txt"), b"seed\n");
    reopened.close(false).await.unwrap();

    let unrelated = Seed::new("src/a.txt", "seed\n");
    let mut u = controlled(unrelated.builder().sandbox_policy(policy())).await;
    let (during, result) = held(
        &mut u,
        &unrelated.source.join("src/a.txt"),
        "printf U > src/a.txt",
    )
    .await;
    result.unwrap();
    assert_eq!(
        during.unwrap(),
        b"U",
        "another source's history is independent"
    );
    u.shell.close(false).await.unwrap();
}

/// The shell a [`Nested`] builtin calls into, and what that call returned.
static NESTED: Mutex<Option<Arc<Shell>>> = Mutex::new(None);
static NESTED_OUTCOME: Mutex<Option<Result<u8, ShellErrorKind>>> = Mutex::new(None);

#[derive(clap::Parser)]
struct Nested;
impl marsh::builtins::Command for Nested {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let inner = NESTED.lock().unwrap().clone().expect("a nested shell");
        let outcome = inner
            .run("printf child > child")
            .await
            .map(|result| u8::from(result.exit_code))
            .map_err(|error| error.kind().clone());
        *NESTED_OUTCOME.lock().unwrap() = Some(outcome);
        Ok(ExecutionResult::success())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn nested_calls_return_busy_instead_of_waiting_on_their_caller() {
    for managed in [false, true] {
        let seed = Seed::new("a.txt", "seed\n");
        let policy = if managed {
            SandboxPolicy::allow()
        } else {
            SandboxPolicy::forbid()
        };
        let outer = seed
            .builder()
            .sandbox_policy(policy.clone())
            .builtin("nested", marsh::builtins::builtin::<Nested>())
            .build()
            .await
            .unwrap();
        let inner = Arc::new(seed.builder().sandbox_policy(policy).build().await.unwrap());
        *NESTED.lock().unwrap() = Some(Arc::clone(&inner));
        run(&outer, "nested; printf outer > outer").await;
        let outcome = NESTED_OUTCOME
            .lock()
            .unwrap()
            .take()
            .expect("nested call ran");
        assert_eq!(seed.bytes("outer"), b"outer");
        if managed {
            assert_eq!(outcome, Ok(0), "managed callers admit managed callees");
            assert_eq!(seed.bytes("child"), b"child");
        } else {
            assert_eq!(outcome, Err(ShellErrorKind::Busy));
            assert!(!seed.source.join("child").exists());
            // Nothing leaked: the callee is admitted normally afterwards.
            run(&*inner, "printf later > later").await;
            assert_eq!(seed.bytes("later"), b"later");
        }
        *NESTED.lock().unwrap() = None;
        outer.close(false).await.unwrap();
        inner.close(false).await.unwrap();
    }
}

/// Logical names a [`ContextIo`] builtin observed through its callback context.
static CONTEXT_LOG: Mutex<Vec<Vec<PathBuf>>> = Mutex::new(Vec::new());

#[derive(clap::Parser)]
struct ContextIo;
impl marsh::builtins::Command for ContextIo {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let context = marsh::builtins::current_context().expect("owning context");
        let write = |path: &Path, bytes: &[u8]| {
            context
                .open(
                    path,
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true),
                )
                .unwrap()
                .write_all(bytes)
                .unwrap();
        };
        context.create_dir_all(Path::new("ctx")).unwrap();
        write(Path::new("ctx/relative"), b"relative");
        write(&context.working_dir().join("ctx/absolute"), b"absolute");
        assert_eq!(
            context.metadata(Path::new("ctx/relative")).unwrap().len(),
            8
        );
        let worker = context.clone();
        let blocking = context
            .spawn_blocking(move || {
                worker
                    .open(
                        Path::new("ctx/blocking"),
                        std::fs::OpenOptions::new().write(true).create_new(true),
                    )
                    .unwrap()
                    .write_all(b"blocking")
                    .unwrap();
            })
            .unwrap();
        blocking.await.unwrap();
        let mut globbed: Vec<_> = context
            .glob("ctx/*", None)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        globbed.sort();
        let mut listed: Vec<_> = context
            .read_dir(Path::new("ctx"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        listed.sort();
        CONTEXT_LOG.lock().unwrap().extend([globbed, listed]);
        Ok(ExecutionResult::success())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn builtin_context_io_follows_the_route() {
    let seed = Seed::new("a.txt", "seed\n");
    let mut a = controlled(
        seed.builder()
            .builtin("context-io", marsh::builtins::builtin::<ContextIo>()),
    )
    .await;
    let names = ["absolute", "blocking", "relative"].map(|name| seed.source.join("ctx").join(name));
    for peer in [false, true] {
        CONTEXT_LOG.lock().unwrap().clear();
        let peer = if peer {
            Some(seed.builder().build().await.unwrap())
        } else {
            None
        };
        let (during, result) = held(&mut a, &names[1], "context-io").await;
        result.unwrap();
        if peer.is_some() {
            assert_eq!(
                during, None,
                "managed callback writes publish at completion"
            );
        } else {
            assert_eq!(
                during.unwrap(),
                b"blocking",
                "direct callback writes land at once"
            );
        }
        for (name, bytes) in names.iter().zip(["absolute", "blocking", "relative"]) {
            assert_eq!(host(name).unwrap(), bytes.as_bytes());
        }
        assert_eq!(
            *CONTEXT_LOG.lock().unwrap(),
            [names.to_vec(), names.to_vec()]
        );
        std::fs::remove_dir_all(seed.source.join("ctx")).unwrap();
        if let Some(peer) = peer {
            peer.close(false).await.unwrap();
        }
    }
    a.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn route_changes_revoke_view_descriptors_and_keep_directory_state() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let a = seed.builder().build().await.unwrap();
    let fd = seed.source.join("fd.txt");
    run(
        &a,
        "cd src; cd ..; export OLDPWD; readonly OLDPWD; exec 3>fd.txt",
    )
    .await;
    let refused = |result: Result<ExecutionResult, ShellError>| {
        if let Ok(result) = result {
            assert_ne!(u8::from(result.exit_code), 0, "a revoked descriptor fails");
        }
    };

    let peer = seed.builder().build().await.unwrap();
    refused(a.run("printf old >&3").await);
    assert_eq!(
        host(&fd).unwrap(),
        b"",
        "neither the source nor the view was written"
    );
    run(&a, "exec 3>fd.txt; printf managed >&3").await;
    assert_eq!(host(&fd).unwrap(), b"managed");

    peer.close(false).await.unwrap();
    refused(a.run("printf stale >&3").await);
    assert_eq!(host(&fd).unwrap(), b"managed");
    run(&a, "exec 3>fd.txt; printf direct >&3; exec 3>&-").await;
    assert_eq!(host(&fd).unwrap(), b"direct");

    let oldpwd = a.env_var("OLDPWD").await.unwrap();
    assert!(oldpwd.is_readonly() && oldpwd.is_exported());
    assert_eq!(PathBuf::from(string(&oldpwd)), seed.source.join("src"));
    assert_eq!(a.working_dir().await, seed.source);
    a.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn every_entry_point_is_routed() {
    let bare = Bare::new();
    git_init(&bare.source);
    let script = bare.root.path().join("script.sh");
    std::fs::write(&script, "printf script > script\n").unwrap();
    let sourced = bare.root.path().join("sourced.sh");
    std::fs::write(&sourced, "printf sourced > sourced\n").unwrap();
    let rc = bare.root.path().join("rc");
    std::fs::write(&rc, "printf startup > startup\n").unwrap();
    let startup = || {
        bare.builder()
            .interactive(true)
            .rc(RcLoadBehavior::LoadCustom(rc.clone()))
            .build()
    };
    let markers = ["line", "string", "script", "sourced", "function", "startup"];

    // Alone, every entry point runs directly: there is no storage to manage it with.
    startup().await.unwrap().close(false).await.unwrap();
    let a = bare.builder().build().await.unwrap();
    run(
        &a,
        "f() { printf function > function; }; printf line > line",
    )
    .await;
    let params = a.default_exec_params().await;
    a.run_string("printf string > string", &SourceInfo::default(), &params)
        .await
        .unwrap();
    a.run_script(&script, &[]).await.unwrap();
    a.source_script(&sourced, &[], &params).await.unwrap();
    assert_eq!(a.invoke_function("f", &[], &params).await.unwrap(), 0);
    for marker in markers {
        let path = bare.source.join(marker);
        assert_eq!(host(&path).unwrap(), marker.as_bytes());
        std::fs::remove_file(path).unwrap();
    }

    // With a peer, every entry point takes the managed route, which fails before user code.
    let peer = bare.builder().build().await.unwrap();
    infrastructure(a.run("printf line > line").await);
    infrastructure(
        a.run_string("printf string > string", &SourceInfo::default(), &params)
            .await,
    );
    infrastructure(a.run_script(&script, &[]).await);
    infrastructure(a.source_script(&sourced, &[], &params).await);
    infrastructure(a.invoke_function("f", &[], &params).await);
    infrastructure(startup().await);
    for marker in markers {
        assert!(!bare.source.join(marker).exists(), "{marker} ran");
    }
    peer.close(false).await.unwrap();
    run(&a, "printf line > line").await;
    assert_eq!(host(&bare.source.join("line")).unwrap(), b"line");
    a.close(false).await.unwrap();
}

/// Panics when the accepted command names the sentinel; otherwise the default verdict.
fn panicky(ctx: &CommandContext<'_>) -> bool {
    assert!(
        !ctx.tool_as::<ShellCommand>()
            .is_some_and(|shell| shell.command.contains("PANIC")),
        "predicate sentinel"
    );
    SandboxPolicy::SharedSource.eval(ctx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_panicking_predicate_fails_the_command_and_leaks_nothing() {
    let seed = Seed::new("a.txt", "seed\n");
    let marker = seed.source.join("marker");
    let a = seed
        .builder()
        .sandbox_policy(SandboxPolicy::Base(panicky))
        .build()
        .await
        .unwrap();
    for peer in [false, true] {
        let b = if peer {
            Some(seed.builder().build().await.unwrap())
        } else {
            None
        };
        let error = a
            .run("printf panicked > marker # PANIC")
            .await
            .err()
            .expect("a panicking predicate refuses the command");
        assert!(
            matches!(error.kind(), ShellErrorKind::Infrastructure),
            "{error}"
        );
        assert!(
            error.to_string().contains("sandbox policy panicked"),
            "{error}"
        );
        assert!(!marker.exists(), "no user code ran");
        run(&a, "printf after > after").await;
        assert_eq!(seed.bytes("after"), b"after", "the shell is not left busy");
        std::fs::remove_file(seed.source.join("after")).unwrap();
        if let Some(b) = b {
            run(&b, "printf peer > peer").await;
            assert_eq!(seed.bytes("peer"), b"peer", "no admission leaked");
            b.close(false).await.unwrap();
        }
    }
    a.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_waiting_command_is_rerouted_by_a_committed_read() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let a = Arc::new(
        seed.builder()
            .sandbox_policy(SandboxPolicy::Base(edit_denied))
            .build()
            .await
            .unwrap(),
    );
    let mut b = controlled(seed.managed_builder()).await;
    let reader = launch(
        &b.shell,
        "/bin/cat src/a.txt >/dev/null; printf 'READY\\n'; read release".into(),
    );
    b.ready().await;
    let mut blind = launch(&a, "printf blind > src/a.txt".into());
    assert!(
        tokio::time::timeout(QUEUED, &mut blind).await.is_err(),
        "a direct verdict waits behind active managed work"
    );
    assert_eq!(seed.bytes("src/a.txt"), b"seed\n");
    b.release();
    join(reader).await.unwrap();
    let error = join(blind).await.err().expect("rerouted and denied");
    assert!(
        matches!(error.kind(), ShellErrorKind::Denied { .. }),
        "{error}"
    );
    assert_eq!(seed.bytes("src/a.txt"), b"seed\n");
    a.close(false).await.unwrap();
    b.shell.close(false).await.unwrap();
}

/// The first [`gated`] evaluation announces itself here, then blocks until [`GATE`] fires.
static ENTERED: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
static GATE: Mutex<Option<std::sync::mpsc::Receiver<()>>> = Mutex::new(None);

/// The default verdict, computed before the first evaluation is held at the gate.
fn gated(ctx: &CommandContext<'_>) -> bool {
    let verdict = SandboxPolicy::SharedSource.eval(ctx);
    let gate = GATE.lock().unwrap().take();
    if let Some(gate) = gate {
        ENTERED.lock().unwrap().take().unwrap().send(()).unwrap();
        gate.recv().unwrap();
    }
    verdict
}

/// Announces the first tool verdict without blocking or capturing policy state.
fn announce_writes(ctx: &CommandContext<'_>) -> bool {
    if let Some(entered) = ENTERED.lock().unwrap().take() {
        entered.send(()).unwrap();
    }
    ctx.action.is_write()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_verdict_made_stale_by_a_new_peer_is_discarded() {
    let seed = Seed::new("a.txt", "seed\n");
    let marker = seed.source.join("marker");
    let (entered, announced) = std::sync::mpsc::channel();
    let (open, gate) = std::sync::mpsc::channel();
    *ENTERED.lock().unwrap() = Some(entered);
    *GATE.lock().unwrap() = Some(gate);
    let mut a = controlled(seed.builder().sandbox_policy(SandboxPolicy::Base(gated))).await;
    let task = launch(
        &a.shell,
        "printf staged > marker; printf 'READY\\n'; read release".into(),
    );
    tokio::time::timeout(
        TIMEOUT,
        tokio::task::spawn_blocking(move || announced.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let b = seed.builder().build().await.unwrap();
    open.send(()).unwrap();
    a.ready().await;
    assert_eq!(
        host(&marker),
        None,
        "the peer that joined makes the command managed"
    );
    a.release();
    join(task).await.unwrap();
    assert_eq!(host(&marker).unwrap(), b"staged");
    b.close(false).await.unwrap();
    a.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn read_tools_wake_after_a_pending_writer_is_classified() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let mut raw = controlled(seed.builder().sandbox_policy(SandboxPolicy::forbid())).await;
    let raw_task = launch(&raw.shell, "printf 'READY\n'; read release".into());
    raw.ready().await;

    let (entered, announced) = std::sync::mpsc::channel();
    let (open, gate) = std::sync::mpsc::channel();
    *ENTERED.lock().unwrap() = Some(entered);
    *GATE.lock().unwrap() = Some(gate);
    let witness = seed.outside("classified-writer-started");
    let writer = Arc::new(
        seed.builder()
            .sandbox_policy(SandboxPolicy::and(
                SandboxPolicy::Base(gated),
                SandboxPolicy::forbid(),
            ))
            .build()
            .await
            .unwrap(),
    );
    let queued = launch(&writer, format!("printf queued > {}", witness.display()));
    tokio::time::timeout(
        TIMEOUT,
        tokio::task::spawn_blocking(move || announced.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();

    let (read_entered, read_announced) = std::sync::mpsc::channel();
    *ENTERED.lock().unwrap() = Some(read_entered);
    let read_shell = Arc::new(
        seed.builder()
            .sandbox_policy(SandboxPolicy::Base(announce_writes))
            .build()
            .await
            .unwrap(),
    );
    let mut read_task = {
        let shell = Arc::clone(&read_shell);
        tokio::spawn(async move {
            shell
                .run_tool(Inspect, |ctx| {
                    std::fs::read(ctx.physical_path(Path::new("src/a.txt")).unwrap()).unwrap()
                })
                .await
        })
    };
    tokio::time::timeout(
        TIMEOUT,
        tokio::task::spawn_blocking(move || read_announced.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let before_gate = tokio::time::timeout(QUEUED, &mut read_task).await;
    let reader_waited = before_gate.is_err();

    open.send(()).unwrap();
    let observed = match before_gate {
        Ok(result) => Ok(result),
        Err(_) => tokio::time::timeout(TIMEOUT, &mut read_task).await,
    };
    let raw_held = !raw_task.is_finished();
    let writer_queued = !queued.is_finished();
    let witness_absent = !witness.exists();
    raw.release();
    let raw_result = join(raw_task).await;
    let queued_result = join(queued).await;
    if observed.is_err() {
        let _ = tokio::time::timeout(TIMEOUT, read_task).await;
    }

    assert!(reader_waited, "an unresolved writer excludes the reader");
    assert_eq!(
        observed
            .expect("reader wakes while bash is held")
            .expect("reader task")
            .expect("read tool"),
        b"seed\n"
    );
    assert!(raw_held && writer_queued, "neither bash barrier opened");
    assert!(witness_absent, "the pending writer did not run");
    raw_result.unwrap();
    queued_result.unwrap();
    assert_eq!(host(&witness).unwrap(), b"queued");
    read_shell.close(false).await.unwrap();
    writer.close(false).await.unwrap();
    raw.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn cold_start_recovery_waits_for_overlapping_direct_work() {
    let (_root, store) = scratch();
    let repository = store.join("repository");
    std::fs::create_dir(&repository).unwrap();
    std::fs::write(repository.join("a.txt"), "seed\n").unwrap();
    git_init(&repository);
    let storage = Arc::new(CopyTree::new());
    storage.register(&store);
    let managed = || {
        marsh_core::test_support::shell_builder(storage.clone())
            .working_dir(repository.clone())
            .sandbox_policy(SandboxPolicy::allow())
    };
    let prior = managed().build().await.unwrap();
    run(&prior, "printf prior > prior").await;
    prior.close(false).await.unwrap();
    assert_eq!(host(&repository.join("prior")).unwrap(), b"prior");

    let mut raw = controlled(
        marsh_core::test_support::shell_builder(Arc::new(CopyTree::new()))
            .working_dir(repository.clone()),
    )
    .await;
    let marker = repository.join("marker");
    let writer = launch(
        &raw.shell,
        "printf first > marker; printf 'READY\\n'; read release; printf final > marker".into(),
    );
    raw.ready().await;
    assert_eq!(host(&marker).unwrap(), b"first");
    let witness = store.join("started");
    let recovering = Arc::new(managed().build().await.unwrap());
    let mut reader = launch(
        &recovering,
        format!(
            "printf x > {}; v=$(cat marker); printf %s \"$v\" > copy",
            witness.display()
        ),
    );
    stays_queued(&mut reader, &witness).await;
    let read_shell = Arc::new(
        managed()
            .sandbox_policy(SandboxPolicy::forbid())
            .build()
            .await
            .unwrap(),
    );
    let mut inspected = {
        let shell = Arc::clone(&read_shell);
        tokio::spawn(async move {
            shell
                .run_tool(Inspect, |ctx| {
                    std::fs::read(ctx.physical_path(Path::new("marker")).unwrap()).unwrap()
                })
                .await
        })
    };
    let before_release = tokio::time::timeout(QUEUED, &mut inspected).await;
    let waited_for_recovery = before_release.is_err();
    raw.release();
    join(writer).await.unwrap();
    join(reader).await.unwrap();
    let observed = match before_release {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(TIMEOUT, inspected)
            .await
            .expect("read completes after recovery"),
    };
    assert!(waited_for_recovery, "source reads cannot bypass pending recovery");
    assert_eq!(observed.expect("read task").expect("read tool"), b"final");
    assert_eq!(host(&repository.join("copy")).unwrap(), b"final");
    read_shell.close(false).await.unwrap();
    recovering.close(false).await.unwrap();
    raw.shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn supplied_descriptors_into_a_private_view_are_refused() {
    let seed = Seed::new("a.txt", "seed\n");
    let a = seed.builder().build().await.unwrap();
    let peer = seed.builder().build().await.unwrap();
    run(&a, "printf managed > managed").await;
    assert_eq!(seed.bytes("managed"), b"managed");
    let private = seed
        .outside(marsh_btrfs::STATE_DIR)
        .join("source/snap")
        .join(a.principal().as_str())
        .join("a.txt");
    let file = std::fs::File::open(&private).expect("A's private work view");
    let mut params = a.default_exec_params().await;
    params.set_fd(5, marsh::OpenFile::from(file));
    peer.close(false).await.unwrap();

    let error = a
        .run_string("printf x > marker", &SourceInfo::default(), &params)
        .await
        .err()
        .expect("a private-view descriptor is refused");
    assert!(
        matches!(error.kind(), ShellErrorKind::Unsupported),
        "{error}"
    );
    assert!(!seed.source.join("marker").exists());
    run(&a, "printf direct > marker").await;
    assert_eq!(seed.bytes("marker"), b"direct");
    a.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn the_direct_route_runs_native_git() {
    let seed = Seed::new("a.txt", "seed\n");
    seed.git();
    let mut a = controlled(seed.builder()).await;
    let task = launch(
        &a.shell,
        "saved=$PATH; PATH=/nonexistent; git status; s=$?; PATH=$saved; printf 'READY\\n'".into(),
    );
    let output = a.ready().await;
    join(task).await.unwrap();
    assert!(
        String::from_utf8_lossy(&output).contains("git: command not found"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(string(&a.shell.env_var("s").await.unwrap()), "127");
    let task = launch(
        &a.shell,
        "printf new > new; git add new; added=$?; git commit -qm direct-commit; committed=$?; printf 'READY\\n'".into(),
    );
    let output = a.ready().await;
    join(task).await.unwrap();
    let statuses = (
        string(&a.shell.env_var("added").await.unwrap()),
        string(&a.shell.env_var("committed").await.unwrap()),
    );
    assert_eq!(
        statuses,
        ("0".to_owned(), "0".to_owned()),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let log = std::process::Command::new("/bin/git")
        .current_dir(&seed.source)
        .args(["log", "-1", "--format=%s", "--name-only"])
        .output()
        .unwrap();
    assert!(log.status.success());
    assert_eq!(log.stdout, b"direct-commit\n\nnew\n");

    a.shell.close(false).await.unwrap();
}

/// A callback context kept past the end of the command that owned it.
static RETAINED: Mutex<Option<marsh::builtins::BuiltinContext>> = Mutex::new(None);
/// Whether each use of [`RETAINED`] inside a later command failed.
static RETAINED_USE: Mutex<Vec<bool>> = Mutex::new(Vec::new());

#[derive(clap::Parser)]
struct Retain;
impl marsh::builtins::Command for Retain {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        *RETAINED.lock().unwrap() = marsh::builtins::current_context();
        Ok(ExecutionResult::success())
    }
}

#[derive(clap::Parser)]
struct UseRetained;
impl marsh::builtins::Command for UseRetained {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let retained = RETAINED
            .lock()
            .unwrap()
            .clone()
            .expect("a retained context");
        let opened = retained.open(
            Path::new("leak"),
            std::fs::OpenOptions::new().write(true).create(true),
        );
        let inspected = retained.metadata(Path::new("a.txt"));
        RETAINED_USE
            .lock()
            .unwrap()
            .extend([opened.is_err(), inspected.is_err()]);
        Ok(ExecutionResult::success())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_retained_context_never_reaches_a_later_view() {
    let seed = Seed::new("a.txt", "seed\n");
    let a = seed
        .builder()
        .builtin("retain", marsh::builtins::builtin::<Retain>())
        .builtin("use-retained", marsh::builtins::builtin::<UseRetained>())
        .build()
        .await
        .unwrap();
    for peer in [false, true] {
        let peer = if peer {
            Some(seed.builder().build().await.unwrap())
        } else {
            None
        };
        RETAINED_USE.lock().unwrap().clear();
        run(&a, "retain").await;
        run(&a, "use-retained").await;
        assert_eq!(*RETAINED_USE.lock().unwrap(), [true, true]);
        assert!(!seed.source.join("leak").exists());
        let retained = RETAINED.lock().unwrap().take().unwrap();
        assert!(retained.metadata(Path::new("a.txt")).is_err());
        if let Some(peer) = peer {
            peer.close(false).await.unwrap();
        }
    }
    a.close(false).await.unwrap();
}

/// A tool call that edits.
struct Stamp;
impl From<&Stamp> for Action {
    fn from(_: &Stamp) -> Self {
        Self::Edit
    }
}
impl From<Stamp> for Action {
    fn from(tool: Stamp) -> Self {
        Self::from(&tool)
    }
}
impl MarshTool for Stamp {
    fn description(&self) -> Cow<'_, str> {
        Cow::Borrowed("stamp")
    }
}

/// A tool call that only reads.
struct Inspect;
impl From<&Inspect> for Action {
    fn from(_: &Inspect) -> Self {
        Self::Read
    }
}
impl From<Inspect> for Action {
    fn from(tool: Inspect) -> Self {
        Self::from(&tool)
    }
}
impl MarshTool for Inspect {
    fn description(&self) -> Cow<'_, str> {
        Cow::Borrowed("inspect")
    }
}

/// Managed exactly for calls that may write. A tool call is still its original value after
/// admission erased its type, beside the action converted from it.
fn writes(ctx: &CommandContext<'_>) -> bool {
    if ctx.tool_as::<ShellCommand>().is_none() {
        assert_eq!(
            ctx.tool_as::<Stamp>().is_some(),
            ctx.action == &Action::Edit
        );
        assert_eq!(
            ctx.tool_as::<Inspect>().is_some(),
            ctx.action == &Action::Read
        );
    }
    ctx.action.is_write()
}

/// Replaces `path` in `ctx`'s view with `bytes`, through the context's reported I/O.
fn stamp(ctx: &marsh::builtins::BuiltinContext, path: &str, bytes: &[u8]) {
    ctx.open(
        Path::new(path),
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
    )
    .unwrap()
    .write_all(bytes)
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn tool_calls_route_by_their_action_and_publish_their_effects() {
    let seed = Seed::new("a.txt", "seed\n");
    let shell = seed
        .builder()
        .sandbox_policy(SandboxPolicy::Base(writes))
        .build()
        .await
        .unwrap();
    let source = seed.source.join("b.txt");
    let (physical, logical) = shell
        .run_tool(Stamp, |ctx| {
            stamp(ctx, "b.txt", b"tool\n");
            ctx.remove_file(Path::new("a.txt")).unwrap();
            let physical = ctx.physical_path(Path::new("b.txt")).unwrap();
            let logical = ctx.logical_path(&physical).unwrap();
            (physical, logical)
        })
        .await
        .unwrap();
    assert_ne!(physical, source, "an edit runs in the private snapshot");
    assert_eq!(logical, source);
    assert_eq!(
        seed.bytes("b.txt"),
        b"tool\n",
        "published when the call returns"
    );
    assert!(!seed.source.join("a.txt").exists(), "so is its removal");

    let (physical, logical, bytes) = shell
        .run_tool(Inspect, |ctx| {
            let physical = ctx.physical_path(Path::new("b.txt")).unwrap();
            let logical = ctx.logical_path(&physical).unwrap();
            let bytes = std::fs::read(&physical).unwrap();
            (physical, logical, bytes)
        })
        .await
        .unwrap();
    assert_eq!(physical, source, "a read runs directly against the source");
    assert_eq!(logical, source);
    assert_eq!(bytes, b"tool\n");
    shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn seed_reads_preserve_unchanged_managed_descriptors() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let managed = seed.managed_builder().build().await.unwrap();
    let direct = seed
        .builder()
        .sandbox_policy(SandboxPolicy::forbid())
        .build()
        .await
        .unwrap();
    run(&managed, "exec 3<src/a.txt").await;
    let (physical, bytes) = direct
        .run_tool(Inspect, |ctx| {
            let physical = ctx.physical_path(Path::new("src/a.txt")).unwrap();
            let bytes = std::fs::read(&physical).unwrap();
            (physical, bytes)
        })
        .await
        .unwrap();
    assert_eq!(physical, seed.source.join("src/a.txt"));
    assert_eq!(bytes, b"seed\n");
    run(&managed, "read -r value <&3; exec 3<&-").await;
    assert_eq!(string(&managed.env_var("value").await.unwrap()), "seed");
    direct.close(false).await.unwrap();
    managed.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_direct_tool_call_acts_on_the_source() {
    let seed = Seed::new("a.txt", "seed\n");
    let shell = seed
        .builder()
        .sandbox_policy(SandboxPolicy::forbid())
        .build()
        .await
        .unwrap();
    let source = seed.source.join("b.txt");
    let (physical, logical) = shell
        .run_tool(Stamp, |ctx| {
            stamp(ctx, "b.txt", b"direct\n");
            let physical = ctx.physical_path(Path::new("b.txt")).unwrap();
            let logical = ctx.logical_path(&physical).unwrap();
            (physical, logical)
        })
        .await
        .unwrap();
    assert_eq!(physical, source);
    assert_eq!(logical, source);
    assert_eq!(seed.bytes("b.txt"), b"direct\n");
    shell.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_denied_tool_call_publishes_nothing_and_drops_its_result() {
    let seed = Seed::new("src/a.txt", "seed\n");
    let shell = seed.managed_builder().build().await.unwrap();
    let peer = seed.managed_builder().build().await.unwrap();
    run(&peer, "/bin/cat src/a.txt >/dev/null").await;
    let error = shell
        .run_tool(Stamp, |ctx| {
            stamp(ctx, "src/a.txt", b"T");
            "stamped"
        })
        .await
        .expect_err("the peer's read denies the edit");
    assert!(
        matches!(error.kind(), ShellErrorKind::Denied { .. }),
        "{error}"
    );
    assert_eq!(seed.bytes("src/a.txt"), b"seed\n");
    shell.close(false).await.unwrap();
    peer.close(false).await.unwrap();
}
