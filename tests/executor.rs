#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! `MarshExecutor` through a real brush shell.
//!
//! Every assertion here is about what an *observer of the seed* sees: the file that appeared, the
//! transaction the log carries, the records the run dumped. The snapshot is an implementation
//! detail of how that happened, and is only inspected where it is the thing under test.
//!
//! Most tests drive a fake btrfs (`brush_btrfs::fake::CopyTree`) so they run anywhere;
//! [`real_btrfs_end_to_end`] repeats the core of the story against actual subvolumes and skips
//! itself where the filesystem cannot support it.
//!
//! Every test is `#[serial]`: builtin instrumentation is process-global, so two live shells would
//! both report into whichever hook was installed last.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brush_btrfs::fake::CopyTree;
use brush_btrfs::{LibBtrfs, Subvolumes};
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, ShellVariable, SourceInfo};
use brush_extensions::{
    MarshError, MarshExecutor, MarshShellExtensions, PublishMeta, build_shell,
};
use brush_wal::{JsonLog, WalRecord};
use serial_test::serial;
use sha1::{Digest, Sha1};

/// The seed tree, the fake filesystem it is registered with, and the scratch root holding both.
struct Fixture {
    /// Kept alive so the scratch directory outlives the test.
    _scratch: tempfile::TempDir,
    /// The tree the executor publishes into.
    seed: PathBuf,
    /// `<scratch>/.marsh/seed`: where snapshots and the log live.
    state: PathBuf,
    /// The btrfs stand-in the executor takes snapshots through.
    fs: Arc<CopyTree>,
}

impl Fixture {
    /// A seed containing `src/a.txt`, registered as a subvolume of a fresh [`CopyTree`].
    fn new() -> Self {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(seed.join("src")).expect("seed tree");
        std::fs::write(seed.join("src/a.txt"), b"seed\n").expect("seed file");
        let fs = Arc::new(CopyTree::new());
        fs.register(&seed);
        Self {
            _scratch: scratch,
            state: root.join(".marsh/seed"),
            seed,
            fs,
        }
    }

    /// An executor attached to this fixture's seed.
    fn open(&self) -> Result<MarshExecutor, MarshError> {
        MarshExecutor::open_with(&self.seed, self.fs.clone())
    }

    /// The write-ahead log's path.
    fn log(&self) -> PathBuf {
        self.state.join("meta/wal.jsonl")
    }

    /// Every record the log carries.
    fn wal(&self) -> Vec<WalRecord<PublishMeta>> {
        JsonLog::<WalRecord<PublishMeta>>::read(&self.log()).expect("read the log")
    }
}

/// Runs one line, returning its exit code.
async fn run(shell: &mut Shell<MarshShellExtensions>, line: &str) -> u8 {
    let params = shell.default_exec_params();
    shell
        .run_string(line, &SourceInfo::from("test"), &params)
        .await
        .expect("run the line")
        .exit_code
        .into()
}

/// A repository with a root commit already in it.
fn init_repository(work: &Path) {
    let repository = git2::Repository::init(work).expect("init a repository");
    let who = git2::Signature::new(
        "Test",
        "test@example.com",
        &git2::Time::new(1_112_911_993, 0),
    )
    .expect("signature");
    let empty = repository
        .index()
        .expect("index")
        .write_tree()
        .expect("write the empty tree");
    let tree = repository.find_tree(empty).expect("find the empty tree");
    repository
        .commit(Some("HEAD"), &who, &who, "root\n", &tree, &[])
        .expect("root commit");
}

/// Exports the pinned git identities libgit2 reads out of the shell's environment.
fn export_git_identity(shell: &mut Shell<MarshShellExtensions>) {
    for who in ["AUTHOR", "COMMITTER"] {
        for (suffix, value) in [
            ("NAME", "Test"),
            ("EMAIL", "test@example.com"),
            ("DATE", "1112911993 +0000"),
        ] {
            let mut variable = ShellVariable::new(value);
            variable.export();
            shell
                .env_mut()
                .set_global(format!("GIT_{who}_{suffix}"), variable)
                .expect("set a git identity variable");
        }
    }
}

/// The `BEGIN` record of the transaction numbered `seq`.
fn begin(records: &[WalRecord<PublishMeta>], seq: u64) -> (&PublishMeta, &str) {
    records
        .iter()
        .find_map(|record| match record {
            WalRecord::Begin {
                seq: recorded,
                uid,
                meta,
                ..
            } if *recorded == seq => Some((meta, uid.as_str())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a BEGIN for seq {seq} in {records:?}"))
}

/// Every seed-relative destination the log's `MOVE` records name.
fn moved(records: &[WalRecord<PublishMeta>]) -> Vec<PathBuf> {
    records
        .iter()
        .filter_map(|record| match record {
            WalRecord::Move { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect()
}

/// The command records, as `(id, argv, cwd, kind)` for begins only.
fn begins(
    executor: &MarshExecutor,
) -> Vec<(u64, Vec<String>, PathBuf, brush_instrument::CommandKind)> {
    executor
        .command_records()
        .into_iter()
        .filter_map(|record| match record {
            brush_instrument::CommandRecord::Begin {
                id, argv, cwd, kind, ..
            } => Some((id, argv, cwd, kind)),
            _ => None,
        })
        .collect()
}

/// The id of the single command whose argv starts with `name`.
fn id_of(executor: &MarshExecutor, name: &str) -> u64 {
    let matching: Vec<u64> = begins(executor)
        .into_iter()
        .filter(|(_, argv, _, _)| argv.first().is_some_and(|word| word == name))
        .map(|(id, _, _, _)| id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {name} dispatch, got {matching:?}"
    );
    matching[0]
}

#[tokio::test]
#[serial]
async fn a_completed_command_is_published_and_recorded() {
    let fixture = Fixture::new();
    let executor = fixture.open().expect("attach to the seed");
    let uid = executor.uid().expect("an attached executor has a uid").to_string();
    let snapshot = executor
        .snapshot_root()
        .expect("an attached executor has a snapshot")
        .to_path_buf();
    let mut shell = build_shell(&executor).await.expect("build the shell");

    assert_eq!(run(&mut shell, "printf hi > b.txt").await, 0);

    assert_eq!(
        std::fs::read(fixture.seed.join("b.txt")).expect("the seed received the file"),
        b"hi"
    );

    let records = fixture.wal();
    let (meta, logged_uid) = begin(&records, 1);
    assert_eq!(meta.cmd, "printf hi");
    assert_eq!(logged_uid, uid);
    assert_eq!(meta.commands, vec![id_of(&executor, "printf")]);
    assert_eq!(moved(&records), vec![PathBuf::from("b.txt")]);
    assert!(
        matches!(records.last(), Some(WalRecord::End { seq: 1 })),
        "the transaction is closed: {records:?}"
    );

    let dispatched = begins(&executor);
    assert_eq!(
        dispatched,
        vec![(
            0,
            vec!["printf".to_string(), "hi".to_string()],
            snapshot,
            brush_instrument::CommandKind::Builtin
        )]
    );
    assert!(
        executor.command_records().iter().any(|record| matches!(
            record,
            brush_instrument::CommandRecord::End { exit: 0, .. }
        )),
        "the exit code was observed"
    );

    let run_dir = fixture.state.join("meta/runs").join(&uid);
    assert!(run_dir.join("commands.json").exists());
    assert!(run_dir.join("builtins.json").exists());
}

#[tokio::test]
#[serial]
async fn an_external_command_is_awaited_inline() {
    let fixture = Fixture::new();
    let executor = fixture.open().expect("attach to the seed");
    let mut shell = build_shell(&executor).await.expect("build the shell");

    assert_eq!(run(&mut shell, "touch c.txt").await, 0);

    assert!(
        fixture.seed.join("c.txt").exists(),
        "an external command's effects are published before the line returns"
    );
    let id = id_of(&executor, "touch");
    assert_eq!(
        begins(&executor)
            .into_iter()
            .find(|(recorded, _, _, _)| *recorded == id)
            .map(|(_, _, _, kind)| kind),
        Some(brush_instrument::CommandKind::External)
    );
    let terminators: Vec<brush_instrument::CommandRecord> = executor
        .command_records()
        .into_iter()
        .filter(|record| {
            record.id() == id
                && !matches!(record, brush_instrument::CommandRecord::Begin { .. })
        })
        .collect();
    assert!(
        matches!(
            terminators.as_slice(),
            [brush_instrument::CommandRecord::End { exit: 0, .. }]
        ),
        "the external command's exit code was observed, not deferred: {terminators:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_pipeline_stage_is_deferred_until_the_next_boundary() {
    let fixture = Fixture::new();
    let executor = fixture.open().expect("attach to the seed");
    let mut shell = build_shell(&executor).await.expect("build the shell");

    assert_eq!(run(&mut shell, "printf 'x\\n' | cat > d.txt").await, 0);

    let cat = id_of(&executor, "cat");
    assert!(
        executor.command_records().iter().any(|record| matches!(
            record,
            brush_instrument::CommandRecord::Spawned { id, .. } if *id == cat
        )),
        "a pipeline stage runs in an owned shell and is deferred"
    );
    assert!(
        !fixture.seed.join("d.txt").exists(),
        "nothing is published while the pipeline's stages are still owned by the interpreter"
    );

    assert_eq!(run(&mut shell, "true").await, 0);

    assert_eq!(
        std::fs::read(fixture.seed.join("d.txt")).expect("the next boundary published it"),
        b"x\n"
    );
    let records = fixture.wal();
    let (meta, _) = begin(&records, 1);
    let printf = id_of(&executor, "printf");
    assert!(
        meta.commands.contains(&printf) && meta.commands.contains(&cat),
        "the transaction names every command it covers: {:?}",
        meta.commands
    );
}

#[tokio::test]
#[serial]
async fn the_git_builtin_commits_inside_the_snapshot() {
    let fixture = Fixture::new();
    let executor = fixture.open().expect("attach to the seed");
    let snapshot = executor.snapshot_root().expect("a snapshot").to_path_buf();
    // `git commit -- <path>` is a partial commit, which git refuses on an unborn branch; the
    // fixture therefore gives HEAD a root commit through libgit2 rather than the builtin.
    init_repository(&snapshot);
    let mut shell = build_shell(&executor).await.expect("build the shell");
    export_git_identity(&mut shell);

    assert_eq!(run(&mut shell, "git add -- src/a.txt").await, 0);
    assert_eq!(run(&mut shell, "git commit -m init -- src/a.txt").await, 0);

    let repository = git2::Repository::open(&fixture.seed).expect("the seed received .git");
    let head = repository
        .head()
        .expect("HEAD")
        .peel_to_commit()
        .expect("a commit");
    assert_eq!(
        head.message().expect("a UTF-8 message").trim(),
        "init",
        "the commit the builtin made reached the seed through the log"
    );
    assert_eq!(
        head.parent_count(),
        1,
        "on top of the fixture's root commit"
    );
    drop(head);
    drop(repository);

    let builtins: Vec<String> = executor
        .builtin_records()
        .into_iter()
        .filter_map(|record| match record {
            brush_instrument::BuiltinRecord::Begin { builtin, .. } => Some(builtin),
            brush_instrument::BuiltinRecord::End { .. } => None,
        })
        .collect();
    assert_eq!(
        builtins.iter().filter(|name| *name == "git").count(),
        2,
        "both git invocations were reported as builtins: {builtins:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_seed_cwd_is_remapped_into_the_snapshot() {
    let fixture = Fixture::new();
    let executor = fixture.open().expect("attach to the seed");
    let snapshot = executor.snapshot_root().expect("a snapshot").to_path_buf();
    // Deliberately *not* `build_shell`: the point is a shell that starts inside the seed.
    let mut shell = Shell::builder_with_extensions::<MarshShellExtensions>()
        .command_executor(executor.clone())
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(fixture.seed.clone())
        .builtins(executor.builtins())
        .build()
        .await
        .expect("build the shell");

    assert_eq!(run(&mut shell, "true").await, 0, "the first command attaches");
    assert_eq!(run(&mut shell, "pwd > e.txt").await, 0);

    assert!(
        moved(&fixture.wal()).contains(&PathBuf::from("e.txt")),
        "the write went through the snapshot and was published"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("e.txt")).expect("read the published file"),
        format!("{}\n", snapshot.display())
    );

    assert_eq!(
        run(&mut shell, "printf %s \"$MARSH_SNAPSHOT_ROOT\" > f.txt").await,
        0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("f.txt")).expect("read the published file"),
        snapshot.display().to_string()
    );
}

#[tokio::test]
#[serial]
async fn recovery_replays_an_unfinished_transaction_then_sweeps() {
    let fixture = Fixture::new();
    let snap = fixture.state.join("snap");
    std::fs::create_dir_all(snap.join("job0")).expect("a previous session's snapshot");
    std::fs::create_dir_all(fixture.state.join("meta")).expect("meta");
    std::fs::write(snap.join("job0/a.txt"), b"seed\n").expect("its content");
    let mut log = JsonLog::<WalRecord<PublishMeta>>::open(&fixture.log()).expect("open the log");
    log.append(&[
        WalRecord::Begin {
            seq: 3,
            uid: "job0".to_string(),
            op_count: Some(1),
            meta: PublishMeta {
                cmd: String::new(),
                commands: Vec::new(),
            },
        },
        WalRecord::Move {
            from: PathBuf::from("a.txt"),
            to: PathBuf::from("a.txt"),
            sha1: hex::encode(Sha1::digest(b"seed\n")),
        },
    ])
    .expect("append an unfinished transaction");
    drop(log);

    let executor = fixture.open().expect("attach to the seed");
    let uid = executor.uid().expect("a uid").to_string();

    assert_eq!(
        std::fs::read(fixture.seed.join("a.txt")).expect("the replay reached the seed"),
        b"seed\n"
    );
    let remaining: Vec<String> = std::fs::read_dir(&snap)
        .expect("read snap/")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        remaining,
        vec![uid],
        "the previous session's snapshot was swept and only this one's remains"
    );

    let mut shell = build_shell(&executor).await.expect("build the shell");
    assert_eq!(run(&mut shell, "printf next > g.txt").await, 0);
    let (_, _) = begin(&fixture.wal(), 4);
}

#[tokio::test]
#[serial]
async fn a_second_session_is_refused() {
    let fixture = Fixture::new();
    let _first = fixture.open().expect("the first claim succeeds");
    let error = fixture.open().expect_err("the second claim is refused");
    assert!(
        matches!(
            &error,
            MarshError::Btrfs(brush_btrfs::Error::SessionBusy(path)) if *path == fixture.seed
        ),
        "got {error:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_detached_executor_passes_through() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let work = scratch.path().canonicalize().expect("canonical work tree");
    let executor = MarshExecutor::default();

    let mut shell = Shell::builder_with_extensions::<MarshShellExtensions>()
        .command_executor(executor.clone())
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(work.clone())
        .builtins(executor.builtins())
        .build()
        .await
        .expect("build the shell");

    assert_eq!(
        run(&mut shell, "f() { printf inner; }; f > h.txt").await,
        0
    );
    assert_eq!(run(&mut shell, "printf a | cat > i.txt").await, 0);
    assert_eq!(run(&mut shell, "false").await, 1);

    // The same script through a stock shell, for parity.
    let mut stock = Shell::builder()
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(work.clone())
        .builtins(brush_builtins::default_builtins(
            brush_builtins::BuiltinSet::BashMode,
        ))
        .build()
        .await
        .expect("build a stock shell");
    let params = stock.default_exec_params();
    for line in [
        "f() { printf inner; }; f > j.txt",
        "printf a | cat > k.txt",
        "false",
    ] {
        stock
            .run_string(line, &SourceInfo::from("stock"), &params)
            .await
            .expect("run the line");
    }

    assert_eq!(
        std::fs::read(work.join("h.txt")).expect("read"),
        std::fs::read(work.join("j.txt")).expect("read"),
        "a function's output is the same with a detached executor"
    );
    assert_eq!(
        std::fs::read(work.join("i.txt")).expect("read"),
        std::fs::read(work.join("k.txt")).expect("read"),
        "a pipeline's output is the same with a detached executor"
    );

    assert!(executor.command_records().is_empty());
    assert!(executor.builtin_records().is_empty());
    assert!(matches!(executor.publish(), Err(MarshError::Detached)));
}

#[tokio::test]
#[serial]
async fn real_btrfs_end_to_end() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/tmp/brush-extensions-tests")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&root).expect("create the test root");
    let fs = LibBtrfs;
    if fs.assert_btrfs(&root).is_err() || fs.assert_user_subvol_rm_allowed(&root).is_err() {
        eprintln!(
            "skipping real_btrfs_end_to_end: {} is not a btrfs mount with user_subvol_rm_allowed",
            root.display()
        );
        let _ = std::fs::remove_dir_all(&root);
        return;
    }

    let seed = root.join("seed");
    fs.create_subvolume(&seed).expect("create the seed subvolume");

    let snap = root.join(".marsh/seed/snap");
    {
        let executor = MarshExecutor::open(&seed).expect("attach to a real subvolume");
        let snapshot = executor.snapshot_root().expect("a snapshot").to_path_buf();
        assert!(
            fs.is_subvolume(&snapshot),
            "the session's work tree is a real btrfs snapshot"
        );
        let mut shell = build_shell(&executor).await.expect("build the shell");
        assert_eq!(run(&mut shell, "printf hi > f.txt").await, 0);
        assert_eq!(
            std::fs::read(seed.join("f.txt")).expect("the seed received the file"),
            b"hi"
        );
    }

    let leftover: Vec<PathBuf> = std::fs::read_dir(&snap)
        .expect("read snap/")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(
        leftover.is_empty(),
        "dropping the executor reclaims its snapshot: {leftover:?}"
    );

    fs.delete_subvolume(&seed);
    std::fs::remove_dir_all(&root).expect("clean the test root");
}
