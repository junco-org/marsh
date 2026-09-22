#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! `marsh::Shell` through a real brush shell.
//!
//! Every assertion here is about what an *observer of the seed* sees: the file that appeared, the
//! transaction the log carries, the records the run dumped, the capabilities the gate granted or
//! refused. The snapshot is an implementation detail of how that happened, and is only inspected
//! where it is the thing under test.
//!
//! Most tests drive a fake btrfs (`marsh_btrfs::fake::CopyTree`) so they run anywhere;
//! [`real_btrfs_end_to_end`] repeats the core of the story against actual subvolumes and skips
//! itself where the filesystem cannot support it.
//!
//! Every shell gets its own `PolicyValidator`: the process-wide one would carry history from one
//! test into the next.
//!
//! Every test is `#[serial]`: builtin instrumentation is process-global, so two live shells would
//! both report into whichever hook was installed last.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use brush_core::traps::TrapSignal;
use brush_core::{
    ExecutionControlFlow, ProfileLoadBehavior, RcLoadBehavior, ShellVariable, SourceInfo,
};
use marsh::policy::{Action, Event, Principal};
use marsh::{
    Denial, MarshError, MarshExecutor, MarshShellExtensions, Outcome, PolicyValidator, Publication,
    PublishMeta, Shell, Signal, SnapshotUid, StalePath,
};
use marsh_btrfs::fake::CopyTree;
use marsh_btrfs::{LibBtrfs, Subvolumes};
use marsh_instrument::{BuiltinRecord, SpawnRecord, parse_records};
use marsh_wal::{JsonLog, WalRecord};
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

    /// A gated shell over this fixture's seed, with a history of its own.
    async fn shell(&self) -> Shell {
        Shell::build(self.open().expect("attach to the seed"), validator())
            .await
            .expect("build the shell")
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

/// A history no other test has written to.
fn validator() -> Arc<Mutex<PolicyValidator>> {
    Arc::new(Mutex::new(PolicyValidator::default()))
}

/// Runs one line through the gate, returning its exit code and how the gate ended it.
async fn run(shell: &Shell, line: &str) -> (u8, Outcome) {
    let (result, outcome) = shell.run(line).await.expect("run the line");
    (result.exit_code.into(), outcome)
}

/// What a published line wrote, and what it was granted to write it.
fn published(outcome: Outcome) -> (Publication, Vec<Event>) {
    match outcome {
        Outcome::Published {
            publication,
            granted,
        } => (publication, granted),
        other => panic!("expected a published line, got {other:?}"),
    }
}

/// The request `shell`'s principal makes of the seed-relative `path`.
fn event(shell: &Shell, action: Action, path: &str) -> Event {
    Event::new(
        shell.principal().clone(),
        action,
        path.split('/').collect::<Vec<_>>(),
    )
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
async fn export_git_identity(shell: &Shell) {
    let mut guard = shell.shell_ref().lock().await;
    for who in ["AUTHOR", "COMMITTER"] {
        for (suffix, value) in [
            ("NAME", "Test"),
            ("EMAIL", "test@example.com"),
            ("DATE", "1112911993 +0000"),
        ] {
            let mut variable = ShellVariable::new(value);
            variable.export();
            guard
                .env_mut()
                .set_global(format!("GIT_{who}_{suffix}"), variable)
                .expect("set a git identity variable");
        }
    }
    drop(guard);
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

/// The ids of the builtin invocations the executor recorded.
fn builtin_begins(executor: &MarshExecutor) -> Vec<u64> {
    executor
        .builtin_records()
        .into_iter()
        .filter_map(|record| match record {
            BuiltinRecord::Begin { id, .. } => Some(id),
            BuiltinRecord::End { .. } => None,
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn a_completed_line_is_published_and_recorded() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let uid = shell
        .executor()
        .uid()
        .expect("an attached executor has a uid")
        .to_string();

    assert_eq!(
        run(&shell, "printf hi > b.txt").await,
        (
            0,
            Outcome::Published {
                publication: Publication { seq: 1, ops: 1 },
                granted: vec![event(&shell, Action::Edit, "b.txt")],
            }
        )
    );

    assert_eq!(
        std::fs::read(fixture.seed.join("b.txt")).expect("the seed received the file"),
        b"hi"
    );

    let records = fixture.wal();
    let (meta, logged_uid) = begin(&records, 1);
    assert_eq!(meta.cmd, "printf hi > b.txt", "the line is what is logged");
    assert_eq!(logged_uid, uid);
    assert!(meta.spawns.is_empty(), "printf is a builtin, not a spawn");
    assert_eq!(meta.builtins, builtin_begins(shell.executor()));
    assert_eq!(moved(&records), vec![PathBuf::from("b.txt")]);
    assert!(
        matches!(records.last(), Some(WalRecord::End { seq: 1 })),
        "the transaction is closed: {records:?}"
    );
    assert!(
        shell.executor().spawn_records().is_empty(),
        "no external command ran: {:?}",
        shell.executor().spawn_records()
    );

    let run_dir = fixture.state.join("meta/runs").join(&uid);
    let spawns = std::fs::read_to_string(run_dir.join("spawns.json")).expect("the spawn dump");
    let builtins =
        std::fs::read_to_string(run_dir.join("builtins.json")).expect("the builtin dump");
    assert!(
        parse_records::<SpawnRecord>(&spawns).is_ok(),
        "the spawn dump parses: {spawns}"
    );
    assert_eq!(
        parse_records::<BuiltinRecord>(&builtins).expect("the builtin dump parses"),
        shell.executor().builtin_records()
    );
}

#[tokio::test]
#[serial]
async fn an_external_command_is_recorded_with_its_pid() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let snapshot = shell
        .executor()
        .snapshot_root()
        .expect("a snapshot")
        .to_path_buf();

    assert_eq!(run(&shell, "touch c.txt").await.0, 0);
    assert!(
        fixture.seed.join("c.txt").exists(),
        "an external command's effects are published when its line completes"
    );

    let spawns = shell.executor().spawn_records();
    let [
        SpawnRecord::Spawned {
            id, request, pid, ..
        },
    ] = spawns.as_slice()
    else {
        panic!("expected exactly one started process, got {spawns:?}");
    };
    assert_eq!(
        request.program.file_name().expect("a program name"),
        "touch"
    );
    assert_eq!(request.args, ["c.txt".to_string()]);
    assert_eq!(
        request.cwd, snapshot,
        "the shell spawns inside the snapshot"
    );
    assert!(pid.is_some(), "a started process reports its pid");

    let records = fixture.wal();
    let (meta, _) = begin(&records, 1);
    assert_eq!(meta.spawns, vec![*id]);
}

#[tokio::test]
#[serial]
async fn a_pipeline_is_published_once_when_its_line_completes() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    let (code, outcome) = run(&shell, "printf 'x\\n' | cat > d.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome),
        (
            Publication { seq: 1, ops: 1 },
            vec![event(&shell, Action::Edit, "d.txt")]
        )
    );
    assert_eq!(
        std::fs::read(fixture.seed.join("d.txt")).expect("the line published its pipeline"),
        b"x\n"
    );

    let programs: Vec<PathBuf> = shell
        .executor()
        .spawn_records()
        .iter()
        .map(|record| record.request().program.clone())
        .collect();
    assert!(
        programs
            .iter()
            .any(|program| program.file_name().is_some_and(|name| name == "cat")),
        "the pipeline's external stage reached the spawner: {programs:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_spawn_that_fails_is_recorded_with_its_error() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let snapshot = shell
        .executor()
        .snapshot_root()
        .expect("a snapshot")
        .to_path_buf();
    // No execute bit: `execve` refuses the file for every uid, root included.
    std::fs::write(snapshot.join("noexec.sh"), b"#!/bin/sh\ntrue\n").expect("write the script");

    assert_eq!(
        run(&shell, "./noexec.sh").await.0,
        126,
        "a spawn failure that is not `NotFound` is reported as failed-to-execute"
    );

    let spawns = shell.executor().spawn_records();
    let [SpawnRecord::Failed { request, error, .. }] = spawns.as_slice() else {
        panic!("expected exactly one refused spawn, got {spawns:?}");
    };
    assert!(
        request.program.ends_with("noexec.sh"),
        "the refused program is named: {}",
        request.program.display()
    );
    assert!(
        error.to_lowercase().contains("permission denied"),
        "the spawn error is recorded as reported: {error}"
    );
}

#[tokio::test]
#[serial]
async fn the_git_builtin_commits_inside_the_snapshot() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let snapshot = shell
        .executor()
        .snapshot_root()
        .expect("a snapshot")
        .to_path_buf();
    // `git commit -- <path>` is a partial commit, which git refuses on an unborn branch; the
    // fixture therefore gives HEAD a root commit through libgit2 rather than the builtin.
    init_repository(&snapshot);
    export_git_identity(&shell).await;

    // Staging a path nobody edited is illegal, so the line that edits it comes first.
    let (code, outcome) = run(&shell, "printf changed > src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![event(&shell, Action::Edit, "src/a.txt")]
    );

    let (code, outcome) = run(&shell, "git add -- src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![event(&shell, Action::Stage, "src/a.txt")]
    );

    let (code, outcome) = run(&shell, "git commit -m init -- src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![Event::new(
            shell.principal().clone(),
            Action::commit("init"),
            ["src", "a.txt"]
        )]
    );

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

    let builtins: Vec<String> = shell
        .executor()
        .builtin_records()
        .into_iter()
        .filter_map(|record| match record {
            BuiltinRecord::Begin { builtin, .. } => Some(builtin),
            BuiltinRecord::End { .. } => None,
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
async fn the_snapshot_root_is_exported_and_bounds_the_git_builtin() {
    let fixture = Fixture::new();
    // A repository in the seed itself: an unbounded search from a working directory inside the
    // seed would find it. The snapshot copies it, so no publication of this test touches it.
    init_repository(&fixture.seed);
    let shell = fixture.shell().await;
    let snapshot = shell
        .executor()
        .snapshot_root()
        .expect("a snapshot")
        .to_path_buf();
    export_git_identity(&shell).await;

    assert_eq!(
        run(&shell, "printf %s \"$MARSH_SNAPSHOT_ROOT\" > f.txt")
            .await
            .0,
        0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("f.txt")).expect("the published file"),
        snapshot.display().to_string(),
        "the shell was told where the snapshot root is"
    );

    let outside = format!(
        "cd {} && git add -- src/a.txt 2>/dev/null",
        fixture.seed.display()
    );
    let (code, outcome) = run(&shell, &outside).await;
    assert_eq!(
        code, 128,
        "the repository search stops at the boundary instead of climbing into the seed"
    );
    assert_eq!(
        outcome,
        Outcome::Published {
            publication: Publication { seq: 1, ops: 0 },
            granted: Vec::new(),
        },
        "a refused git command requests nothing and changes nothing"
    );
    let repository = git2::Repository::open(&fixture.seed).expect("the seed's repository");
    assert!(
        repository
            .index()
            .expect("index")
            .get_path(Path::new("src/a.txt"), 0)
            .is_none(),
        "nothing was staged anywhere"
    );
}

#[tokio::test]
#[serial]
async fn a_stage_of_an_untouched_path_is_denied_and_discarded() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);
    let shell = fixture.shell().await;
    let snapshot = shell
        .executor()
        .snapshot_root()
        .expect("a snapshot")
        .to_path_buf();
    export_git_identity(&shell).await;

    let (code, outcome) = run(&shell, "git add -- src/a.txt").await;
    assert_eq!(
        code, 0,
        "the builtin succeeded inside the snapshot; the gate is what refuses the line"
    );
    let requested = vec![event(&shell, Action::Stage, "src/a.txt")];
    assert_eq!(
        outcome,
        Outcome::Denied {
            denials: vec![Denial {
                event: requested[0].clone(),
                failed_precondition:
                    "stage requires an unstaged resource owned by the acting principal".to_string(),
                allowed_fixes: vec![format!(
                    "{} edit src/a.txt before staging",
                    shell.principal()
                )],
            }],
            requested,
        }
    );

    let snapshot_repository = git2::Repository::open(&snapshot).expect("the retaken snapshot");
    assert!(
        snapshot_repository
            .index()
            .expect("index")
            .get_path(Path::new("src/a.txt"), 0)
            .is_none(),
        "the snapshot was retaken from the seed, so the staging is gone"
    );
    drop(snapshot_repository);
    let seed_repository = git2::Repository::open(&fixture.seed).expect("the seed's repository");
    assert!(
        seed_repository
            .index()
            .expect("index")
            .get_path(Path::new("src/a.txt"), 0)
            .is_none(),
        "and it never reached the seed"
    );
    drop(seed_repository);
    assert!(
        shell
            .validator()
            .lock()
            .expect("the history")
            .history()
            .is_empty(),
        "a denied line commits nothing to the history"
    );

    let (code, outcome) = run(&shell, "printf x > z.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).0,
        Publication { seq: 1, ops: 1 },
        "the first transaction: the denied line logged none"
    );
}

#[tokio::test]
#[serial]
async fn a_line_orders_its_edit_before_its_stage() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);
    let shell = fixture.shell().await;
    export_git_identity(&shell).await;

    let (code, outcome) = run(&shell, "printf x > src/a.txt; git add -- src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![
            event(&shell, Action::Edit, "src/a.txt"),
            event(&shell, Action::Stage, "src/a.txt"),
        ],
        "the stage is legal only because the edit of the same line precedes it"
    );
}

#[tokio::test]
#[serial]
async fn gits_own_writes_are_not_edits() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);
    let shell = fixture.shell().await;
    export_git_identity(&shell).await;

    for line in [
        "printf v1 > src/a.txt",
        "git add -- src/a.txt",
        "git commit -m v1 -- src/a.txt",
        "printf v2 > src/a.txt",
    ] {
        let (code, outcome) = run(&shell, line).await;
        assert_eq!(code, 0, "`{line}` succeeded");
        published(outcome);
    }

    let (code, outcome) = run(&shell, "git checkout HEAD -- src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![event(&shell, Action::Checkout, "src/a.txt")],
        "the restore of src/a.txt is git's own write, not a user edit"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("src/a.txt")).expect("the published file"),
        "v1",
        "and it reached the seed"
    );
}

#[tokio::test]
#[serial]
async fn the_shared_validator_carries_history_across_shells() {
    let fixture = Fixture::new();
    let validator = validator();

    let first = Shell::build(
        fixture.open().expect("attach to the seed"),
        validator.clone(),
    )
    .await
    .expect("build the first shell");
    let (code, outcome) = run(&first, "printf x > src/a.txt").await;
    assert_eq!(code, 0);
    published(outcome);
    let author = first.principal().clone();
    drop(first);

    let second = Shell::build(fixture.open().expect("attach again"), validator.clone())
        .await
        .expect("build the second shell");
    let (code, outcome) = run(&second, "printf y > src/a.txt").await;
    assert_eq!(code, 0);
    let Outcome::Denied { denials, .. } = outcome else {
        panic!("expected the second principal to be refused, got {outcome:?}");
    };
    let [denial] = denials.as_slice() else {
        panic!("expected exactly one denial, got {denials:?}");
    };
    assert_eq!(
        denial.failed_precondition,
        format!(
            "src/a.txt is unstaged by {author}; {} may read, diff, or history it but may not edit it",
            second.principal()
        )
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("src/a.txt")).expect("the seed's file"),
        "x",
        "the refused edit was discarded"
    );
}

/// Ownership is a property of the seed, not of the process that happens to be serving it.
///
/// A principal edits a path and leaves it unstaged, which is what makes the path that principal's.
/// The session is then closed and reopened over the same seed with nothing written in between — a
/// daemon restart, an upgrade, a reboot — and a second principal asks for the same path. While the
/// history lived only in memory this was granted every time: the new process began owning nothing,
/// so the first line after any restart won against work nobody had settled.
#[tokio::test]
#[serial]
async fn ownership_survives_a_reopen() {
    let fixture = Fixture::new();

    let owner = fixture.shell().await;
    let (code, outcome) = run(&owner, "printf owner > owned").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 1, ops: 1 });
    // The stake belongs to the principal that took it, so its own next line is not refused by it.
    let (code, outcome) = run(&owner, "printf owner-again > owned").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 2, ops: 1 });
    drop(owner);

    // A different process over the same seed: its own session, its own empty history.
    let intruder = Shell::build(fixture.open().expect("reopen the seed"), validator())
        .await
        .expect("build the second shell");
    let (code, outcome) = run(&intruder, "printf other > owned").await;
    assert_eq!(
        code, 0,
        "the line runs inside the snapshot; only its publication is refused"
    );
    assert!(
        matches!(outcome, Outcome::Denied { .. }),
        "the reopened session must still refuse the overwrite, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("owned")).expect("the seed's file"),
        "owner-again",
        "the refused edit was discarded"
    );
    assert!(
        !fixture
            .wal()
            .iter()
            .any(|record| matches!(record, WalRecord::Begin { seq, .. } if *seq > 2)),
        "a refused line opens no transaction: {:?}",
        fixture.wal()
    );

    // Recovering a stake must not invent one: what nobody claimed is still free.
    let (code, outcome) = run(&intruder, "printf fresh > untouched").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 3, ops: 1 });
}

/// A name comes back and a snapshot id does not, so the stake is recorded against the id.
///
/// Two sessions, each running a job called `1` — a reused pane index, a restarted daemon numbering
/// its jobs from one again. The second `1` is a different actor, and a gate that compared names
/// would hand it everything the first one left unsettled: worse than forgetting, because it grants
/// where it should refuse.
#[tokio::test]
#[serial]
async fn a_reused_job_name_does_not_inherit_the_stake() {
    let fixture = Fixture::new();

    let seed_level = fixture.open().expect("attach to the seed");
    let first = Shell::build(
        seed_level
            .snapshot(Principal::from("1"))
            .expect("a job named 1"),
        validator(),
    )
    .await
    .expect("build the first job");
    assert_eq!(first.principal().as_str(), "1");
    let first_uid = first.executor().uid().expect("a uid").to_string();
    let (code, outcome) = run(&first, "printf owner > owned").await;
    assert_eq!(code, 0);
    published(outcome);
    drop(first);
    drop(seed_level);

    let seed_level = fixture.open().expect("reopen the seed");
    let second = Shell::build(
        seed_level
            .snapshot(Principal::from("1"))
            .expect("a job named 1 again"),
        validator(),
    )
    .await
    .expect("build the second job");
    assert_eq!(second.principal().as_str(), "1", "the same name");
    assert_ne!(
        second.executor().uid().expect("a uid"),
        first_uid,
        "a different snapshot"
    );

    let (code, outcome) = run(&second, "printf other > owned").await;
    assert_eq!(code, 0);
    assert!(
        matches!(outcome, Outcome::Denied { .. }),
        "the new job 1 is not the old job 1, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("owned")).expect("the seed's file"),
        "owner"
    );
}

/// A job may be called anything, including the id of the snapshot that owns the path it wants.
///
/// The policy compares principals as strings, so a name chosen to spell a recovered owner's id
/// would otherwise be that owner as far as the gate could tell — a bypass anyone who can read the
/// log and name a job could take.
#[tokio::test]
#[serial]
async fn a_job_named_after_a_durable_owner_does_not_become_it() {
    let fixture = Fixture::new();

    let owner = fixture.shell().await;
    let (code, outcome) = run(&owner, "printf owner > owned").await;
    assert_eq!(code, 0);
    published(outcome);
    let owner_uid = owner.executor().uid().expect("a uid").to_string();
    drop(owner);

    let seed_level = fixture.open().expect("reopen the seed");
    let impostor = Shell::build(
        seed_level
            .snapshot(Principal::from(owner_uid.as_str()))
            .expect("a job named after the owner"),
        validator(),
    )
    .await
    .expect("build the impostor");
    let (code, outcome) = run(&impostor, "printf other > owned").await;
    assert_eq!(code, 0);
    assert!(
        matches!(outcome, Outcome::Denied { .. }),
        "spelling the owner's id is not being the owner, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("owned")).expect("the seed's file"),
        "owner"
    );
}

/// A stage releases a path, and a release has to survive a reopen as surely as a stake does.
///
/// This is the half that fails *closed*. A replay that recovered the edit but lost the stage would
/// refuse work the live gate allows, for a reason the user cannot see and cannot undo — the stage
/// writes only `.git/index`, so nothing in the transaction's own operations says it happened.
#[tokio::test]
#[serial]
async fn a_stage_taken_before_a_reopen_still_releases_the_path_after_it() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);

    let owner = fixture.shell().await;
    export_git_identity(&owner).await;
    let (code, outcome) = run(&owner, "printf changed > src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![event(&owner, Action::Edit, "src/a.txt")]
    );
    let (code, outcome) = run(&owner, "git add -- src/a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).1,
        vec![event(&owner, Action::Stage, "src/a.txt")],
        "the stage is what hands the path back"
    );
    drop(owner);

    let next = Shell::build(fixture.open().expect("reopen the seed"), validator())
        .await
        .expect("build the second shell");
    let (code, outcome) = run(&next, "printf later > src/a.txt").await;
    assert_eq!(code, 0);
    published(outcome);
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("src/a.txt")).expect("the seed's file"),
        "later",
        "a staged path belongs to nobody, before the reopen and after it"
    );
}

/// A seed whose log says nothing owes nobody anything. A log that cannot be *read* says nothing
/// either, and the two must never be confused: the first is an unowned seed, the second is a seed
/// no session may be opened over at all.
#[tokio::test]
#[serial]
async fn an_absent_log_is_unowned_and_an_unreadable_one_is_refused() {
    let fixture = Fixture::new();
    assert!(!fixture.log().exists(), "a fresh seed has no log");

    let shell = fixture.shell().await;
    let (code, outcome) = run(&shell, "printf first > owned").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).0,
        Publication { seq: 1, ops: 1 },
        "an empty history refuses nothing, because nothing was ever claimed"
    );
    drop(shell);

    std::fs::write(fixture.log(), b"{ this is not a record }\n").expect("corrupt the log");
    let error = fixture
        .open()
        .expect_err("a log that cannot be read yields no session");
    assert!(
        matches!(error, MarshError::Wal(_)),
        "an unreadable log is a refusal, not an open gate: got {error:?}"
    );
}

#[tokio::test]
#[serial]
async fn dropping_the_shell_gates_and_publishes_what_it_left() {
    let fixture = Fixture::new();
    let validator = validator();
    let shell = Shell::build(
        fixture.open().expect("attach to the seed"),
        validator.clone(),
    )
    .await
    .expect("build the shell");
    let principal = shell.principal().clone();

    // Deliberately behind the gate's back: nothing is published until the shell drops.
    let mut guard = shell.shell_ref().lock().await;
    let params = guard.default_exec_params();
    guard
        .run_string("printf late > l.txt", &SourceInfo::from("test"), &params)
        .await
        .expect("run the line");
    drop(guard);
    assert!(
        !fixture.seed.join("l.txt").exists(),
        "a line run behind the gate's back publishes nothing by itself"
    );

    drop(shell);

    assert_eq!(
        std::fs::read(fixture.seed.join("l.txt")).expect("the drop published it"),
        b"late"
    );
    let leftover: Vec<PathBuf> = std::fs::read_dir(fixture.state.join("snap"))
        .expect("read snap/")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(
        leftover.is_empty(),
        "the drop reclaimed its snapshot: {leftover:?}"
    );
    let records = fixture.wal();
    let (meta, _) = begin(&records, 1);
    assert_eq!(meta.cmd, "", "no command line named the final publication");
    assert_eq!(
        validator.lock().expect("the history").history(),
        [Event::new(principal, Action::Edit, ["l.txt"])],
        "the drop is a boundary like any other: what it published, it requested"
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
                spawns: Vec::new(),
                builtins: Vec::new(),
                principal: SnapshotUid::default(),
                durable_principal: None,
                granted: Vec::new(),
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

    assert_eq!(
        std::fs::read(fixture.seed.join("a.txt")).expect("the replay reached the seed"),
        b"seed\n"
    );

    let shell = Shell::build(executor, validator())
        .await
        .expect("build the shell");
    let uid = shell.executor().uid().expect("a uid").to_string();
    let remaining: Vec<String> = std::fs::read_dir(&snap)
        .expect("read snap/")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        remaining,
        vec![uid],
        "the seed-level open swept the previous session's snapshot; only this shell's remains"
    );

    let (code, outcome) = run(&shell, "printf next > g.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).0,
        Publication { seq: 4, ops: 1 },
        "publication continues from the recovered sequence number"
    );
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
            MarshError::Btrfs(marsh_btrfs::Error::SessionBusy(path)) if *path == fixture.seed
        ),
        "got {error:?}"
    );
}

#[tokio::test]
#[serial]
async fn two_principals_publish_concurrently_over_one_seed() {
    let fixture = Fixture::new();
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let a = Shell::build(
        seed.snapshot(Principal::from("a")).expect("a's snapshot"),
        validator.clone(),
    )
    .await
    .expect("build a");
    let b = Shell::build(
        seed.snapshot(Principal::from("b")).expect("b's snapshot"),
        validator.clone(),
    )
    .await
    .expect("build b");

    assert_eq!(a.principal(), &Principal::from("a"));
    assert_eq!(b.principal(), &Principal::from("b"));
    let a_root = a
        .executor()
        .snapshot_root()
        .expect("a's root")
        .to_path_buf();
    let b_root = b
        .executor()
        .snapshot_root()
        .expect("b's root")
        .to_path_buf();
    assert_ne!(a_root, b_root, "each shell runs in a snapshot of its own");
    assert!(a_root.is_dir() && b_root.is_dir());

    let (code, outcome) = run(&a, "printf x > a.txt").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 1, ops: 1 });

    let (code, outcome) = run(&b, "printf y > b.txt").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).0,
        Publication { seq: 2, ops: 1 },
        "b refreshed onto a's publication, so its diff is its own file alone"
    );

    assert_eq!(
        std::fs::read(fixture.seed.join("a.txt")).expect("a's file"),
        b"x"
    );
    assert_eq!(
        std::fs::read(fixture.seed.join("b.txt")).expect("b's file"),
        b"y"
    );

    drop(a);
    drop(b);
    let leftover: Vec<PathBuf> = std::fs::read_dir(fixture.state.join("snap"))
        .expect("read snap/")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(
        leftover.is_empty(),
        "each shell reclaimed its own snapshot: {leftover:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_line_that_lost_the_race_is_stale() {
    let fixture = Fixture::new();
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let a = Shell::build(
        seed.snapshot(Principal::from("a")).expect("a's snapshot"),
        validator.clone(),
    )
    .await
    .expect("build a");
    let b = Shell::build(
        seed.snapshot(Principal::from("b")).expect("b's snapshot"),
        validator.clone(),
    )
    .await
    .expect("build b");

    // Behind the gate's back, so b's line is staged but unconcluded when a's boundary lands.
    let mut guard = b.shell_ref().lock().await;
    let params = guard.default_exec_params();
    guard
        .run_string("printf y > p.txt", &SourceInfo::from("test"), &params)
        .await
        .expect("run b's line");
    drop(guard);

    let (code, outcome) = run(&a, "printf x > p.txt").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 1, ops: 1 });

    let mut guard = b.shell_ref().lock().await;
    let outcome = b
        .conclude(&mut guard, "printf y > p.txt")
        .expect("conclude b's line");
    drop(guard);
    let Outcome::Stale { stale, .. } = &outcome else {
        panic!("expected a stale line, got {outcome:?}");
    };
    assert_eq!(
        stale,
        &vec![StalePath {
            path: PathBuf::from("p.txt"),
            merged_seq: 1,
        }]
    );

    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("p.txt")).expect("the seed's file"),
        "x",
        "the winner's content stands"
    );
    assert_eq!(
        std::fs::read_to_string(
            b.executor()
                .snapshot_root()
                .expect("b's root")
                .join("p.txt")
        )
        .expect("b's file"),
        "x",
        "b's snapshot was retaken from the seed, so its line is gone"
    );
    assert_eq!(
        validator.lock().expect("the history").history(),
        [Event::new(Principal::from("a"), Action::Edit, ["p.txt"])],
        "only the winner's edit was ever granted"
    );
}

#[tokio::test]
#[serial]
async fn a_detached_shell_passes_through() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let work = scratch.path().canonicalize().expect("canonical work tree");
    let executor = MarshExecutor::default();
    let validator = validator();

    let built = brush_core::Shell::builder_with_extensions::<MarshShellExtensions>()
        .external_command_spawner(executor.clone())
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
        .expect("build the shell");
    let shell =
        Shell::attach(executor, validator.clone(), built).expect("a detached attach is a no-op");
    assert_eq!(shell.shell_ref().lock().await.working_dir(), work);
    assert!(shell.executor().snapshot_root().is_none());

    for line in [
        "f() { printf inner; }; f > h.txt",
        "printf a | cat > i.txt",
        "false",
    ] {
        assert_eq!(run(&shell, line).await.1, Outcome::Detached);
    }

    // The same script through a stock shell, for parity.
    let mut stock = brush_core::Shell::builder()
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
    let stock_params = stock.default_exec_params();
    let mut stock_exits = Vec::new();
    for line in [
        "f() { printf inner; }; f > j.txt",
        "printf a | cat > k.txt",
        "false",
    ] {
        stock_exits.push(
            stock
                .run_string(line, &SourceInfo::from("stock"), &stock_params)
                .await
                .expect("run the line")
                .exit_code,
        );
    }
    assert_eq!(
        u8::from(stock_exits[2]),
        1,
        "the stock shell reports `false`"
    );

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

    assert!(shell.executor().spawn_records().is_empty());
    assert!(shell.executor().builtin_records().is_empty());
    assert_eq!(
        run(&shell, "printf x > never.txt").await.1,
        Outcome::Detached,
        "a detached shell is a stock shell: it runs the line and gates nothing"
    );
    assert!(work.join("never.txt").exists());
    assert!(
        validator.lock().expect("the history").history().is_empty(),
        "a detached shell requests nothing"
    );
}

/// `exec` has to stay inside this process: an `execve` would throw away the records and whatever
/// the session had not published. It goes through the spawner like any other command, and the line
/// publishes exactly as a non-`exec` line does.
#[tokio::test]
#[serial]
async fn exec_runs_through_the_spawner_and_is_published() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    let (result, outcome) = shell.run("exec touch x.txt").await.expect("run the line");
    assert!(
        matches!(result.next_control_flow, ExecutionControlFlow::ExitShell),
        "`exec` ends the shell"
    );
    assert_eq!(u8::from(result.exit_code), 0);
    assert_eq!(
        published(outcome),
        (
            Publication { seq: 1, ops: 1 },
            vec![event(&shell, Action::Edit, "x.txt")]
        )
    );
    assert!(
        fixture.seed.join("x.txt").exists(),
        "what the exec'd program wrote is published"
    );

    let spawns = shell.executor().spawn_records();
    let [SpawnRecord::Spawned { request, .. }] = spawns.as_slice() else {
        panic!("expected exactly one started process, got {spawns:?}");
    };
    assert_eq!(
        request.program.file_name().expect("a program name"),
        "touch"
    );

    let builtins = shell.executor().builtin_records();
    let begins: Vec<&String> = builtins
        .iter()
        .filter_map(|record| match record {
            BuiltinRecord::Begin { builtin, .. } => Some(builtin),
            BuiltinRecord::End { .. } => None,
        })
        .collect();
    assert_eq!(begins, ["exec"], "the `exec` builtin is instrumented");
}

#[tokio::test]
#[serial]
async fn real_btrfs_end_to_end() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("marsh-tests")
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
    fs.create_subvolume(&seed)
        .expect("create the seed subvolume");

    let snap = root.join(".marsh/seed/snap");
    {
        let shell = Shell::new(&seed, validator())
            .await
            .expect("attach to a real subvolume");
        let snapshot = shell
            .executor()
            .snapshot_root()
            .expect("a snapshot")
            .to_path_buf();
        assert!(
            fs.is_subvolume(&snapshot),
            "the session's work tree is a real btrfs snapshot"
        );
        let (code, outcome) = run(&shell, "printf hi > f.txt").await;
        assert_eq!(code, 0);
        assert_eq!(
            published(outcome).1,
            vec![event(&shell, Action::Edit, "f.txt")]
        );
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
        "dropping the shell reclaims its snapshot: {leftover:?}"
    );

    fs.delete_subvolume(&seed);
    std::fs::remove_dir_all(&root).expect("clean the test root");
}

#[tokio::test]
#[serial]
async fn a_stopped_jobs_process_is_killed_by_signal_job() {
    // brush-core's job manager only ever holds a real, signalable process for a foreground
    // pipeline that stopped (e.g. Ctrl-Z): a backgrounded `cmd &` job runs inside an internal
    // tokio task with no process group of its own, so `Job::kill` can never reach it (bash's own
    // `kill %1` on such a job would fail the same way). This test stops `sleep 30` with a raw
    // SIGSTOP — bypassing `signal_job`, only to arrange the fixture — to get it into the job
    // manager as `%1`, then proves `signal_job` reaches the real process.
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    let (ran, stopped) = tokio::join!(shell.run("sleep 30"), async {
        let pid = loop {
            let pids: Vec<u32> = shell
                .executor()
                .spawn_records()
                .iter()
                .filter_map(|record| match record {
                    SpawnRecord::Spawned { pid, .. } => *pid,
                    SpawnRecord::Failed { .. } => None,
                })
                .collect();
            if let Some(pid) = pids.first().copied() {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let trap = TrapSignal::try_from("STOP").expect("SIGSTOP is a known signal");
        brush_core::sys::signal::kill_process(i32::try_from(pid).expect("pid fits in i32"), trap)
    });
    stopped.expect("stop the running process");
    let (result, _outcome) = ran.expect("run the line");
    assert_eq!(
        u8::from(result.exit_code),
        148,
        "128 + SIGTSTP: the pipeline reports itself stopped"
    );

    shell
        .signal_job("%1", Signal::Kill)
        .await
        .expect("kill the stopped job");

    let started = Instant::now();
    run(&shell, "wait").await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "wait returned only once the killed job actually died"
    );
}

#[tokio::test]
#[serial]
async fn signal_running_kills_the_line_in_flight() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    let started = Instant::now();
    let (ran, signalled) = tokio::join!(shell.run("sleep 30"), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        shell.signal_running(Signal::Kill)
    });
    let elapsed = started.elapsed();

    assert_eq!(
        signalled.expect("the platform knows SIGKILL"),
        1,
        "exactly the one running external process was signalled"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the line in flight died from the signal rather than sleeping out its 30s"
    );
    let (result, _outcome) = ran.expect("run the line");
    assert_eq!(
        u8::from(result.exit_code),
        137,
        "128 + SIGKILL, per brush-core's signal exit mapping"
    );
}

#[tokio::test]
#[serial]
async fn an_unknown_job_spec_is_reported() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    let error = shell
        .signal_job("%9", Signal::Terminate)
        .await
        .expect_err("no job exists to match %9");
    match error {
        MarshError::NoSuchJob(spec) => assert_eq!(spec, "%9"),
        other => panic!("expected NoSuchJob, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn every_signal_is_known_to_brush() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;

    for signal in [
        Signal::Interrupt,
        Signal::Terminate,
        Signal::Kill,
        Signal::Hangup,
        Signal::Continue,
    ] {
        assert_eq!(
            shell
                .signal_jobs(signal)
                .await
                .expect("a known signal name"),
            0,
            "no jobs are running, so nothing accepts {signal:?}"
        );
    }
}
