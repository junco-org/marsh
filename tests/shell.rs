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
    PublishMeta, Shell, Signal, SnapshotUid,
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

    /// A path beside the seed rather than inside it.
    ///
    /// Everything a concurrency test uses to control a command — a barrier the command waits on, a
    /// counter it appends to once per evaluation — has to live here. Inside the snapshot it would
    /// be part of the footprint under test, and a barrier that was itself a publication would be
    /// measuring the mechanism with itself.
    fn outside(&self, name: &str) -> PathBuf {
        self.seed
            .parent()
            .expect("the seed has a parent")
            .join(name)
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

/// The environment every native `git` of this file runs with, fixture setup and managed shell
/// alike: no host configuration, a fixed identity and clock, a stable locale, and no automatic
/// maintenance — a detached `git maintenance` would still be rewriting `.git/` while the next
/// snapshot of it is taken.
const NATIVE_GIT_ENV: [(&str, &str); 16] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "gc.auto"),
    ("GIT_CONFIG_VALUE_0", "0"),
    ("GIT_CONFIG_KEY_1", "maintenance.auto"),
    ("GIT_CONFIG_VALUE_1", "false"),
    ("LC_ALL", "C"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1112911993 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1112911993 +0000"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// Exports [`NATIVE_GIT_ENV`] into the shell, which is where a managed `git` reads it from.
async fn export_git_identity(shell: &Shell) {
    let mut guard = shell.shell_ref().lock().await;
    for (name, value) in NATIVE_GIT_ENV {
        let mut variable = ShellVariable::new(value);
        variable.export();
        guard
            .env_mut()
            .set_global(name, variable)
            .expect("set a git environment variable");
    }
    drop(guard);
}

/// Runs the system `git` in `dir` outside every managed shell, for fixture setup and inspection,
/// and returns its standard output. Fails the test when git does.
fn native_git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(NATIVE_GIT_ENV)
        .output()
        .expect("run the system git");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git printed UTF-8")
}

/// A repository at `dir` on branch `main`, with one commit holding `files`.
fn committed_repository(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).expect("the repository directory");
    native_git(dir, &["init", "-q", "-b", "main"]);
    commit_files(dir, files, "initial");
}

/// Writes `files` into the repository at `dir` and commits them as `subject`.
fn commit_files(dir: &Path, files: &[(&str, &str)], subject: &str) {
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("parents");
        std::fs::write(path, contents).expect("a committed file");
    }
    native_git(dir, &["add", "-A"]);
    native_git(dir, &["commit", "-q", "--allow-empty", "-m", subject]);
}

/// Runs a setup line that must succeed and publish, returning what it was granted.
async fn checked(shell: &Shell, line: &str) -> Vec<Event> {
    let (code, outcome) = run(shell, line).await;
    assert_eq!(code, 0, "`{line}` exits 0");
    match outcome {
        Outcome::Published { granted, .. } => granted,
        other => panic!("`{line}` was not published: {other:?}"),
    }
}

/// A shell over `fixture` with the fixture git environment, standing in `repo`.
async fn repository_shell(fixture: &Fixture) -> Shell {
    let shell = fixture.shell().await;
    export_git_identity(&shell).await;
    checked(&shell, "cd repo").await;
    shell
}

#[tokio::test]
#[serial]
async fn git_command_status() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[
            ("staged.txt", "old\n"),
            ("tracked.txt", "old\n"),
            ("sub/inner.txt", "inner\n"),
            (".gitignore", "ignored.txt\n"),
        ],
    );
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'new\\n' > staged.txt; git add -- staged.txt").await;
    checked(
        &shell,
        "printf 'changed\\n' > tracked.txt; printf 'u\\n' > untracked.txt; printf 'i\\n' > ignored.txt",
    )
    .await;

    let human = fixture.outside("status.txt");
    let porcelain = fixture.outside("porcelain.txt");
    let child = fixture.outside("child.txt");
    let unset = "unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_AUTHOR_DATE GIT_COMMITTER_NAME \
                 GIT_COMMITTER_EMAIL GIT_COMMITTER_DATE";
    for line in [
        format!("{unset}; git status > {}", human.display()),
        format!("git status --porcelain=v1 > {}", porcelain.display()),
        format!(
            "cd sub && git status --porcelain=v1 > {}; cd ..",
            child.display()
        ),
    ] {
        assert_eq!(
            checked(&shell, &line).await,
            Vec::new(),
            "`{line}` inspects without requesting anything"
        );
    }

    let human = std::fs::read_to_string(human).expect("the human status");
    for expected in [
        "On branch main",
        "Changes to be committed:",
        "modified:   staged.txt",
        "Changes not staged for commit:",
        "modified:   tracked.txt",
        "Untracked files:",
        "untracked.txt",
    ] {
        assert!(human.contains(expected), "{expected:?} in {human}");
    }
    assert!(!human.contains("ignored.txt"), "ignored paths are not listed: {human}");
    let rows = "M  staged.txt\n M tracked.txt\n?? untracked.txt\n";
    assert_eq!(std::fs::read_to_string(porcelain).expect("porcelain"), rows);
    assert_eq!(
        std::fs::read_to_string(child).expect("porcelain from a child directory"),
        rows,
        "porcelain v1 paths are repository-relative from a subdirectory"
    );
}

/// What native git prints in `dir`, less its final line feed.
fn git_out(dir: &Path, args: &[&str]) -> String {
    native_git(dir, args).trim_end().to_string()
}

/// Whether native git exits 0 in `dir`.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(NATIVE_GIT_ENV)
        .output()
        .expect("run the system git")
        .status
        .success()
}

/// A file's contents.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The requests `shell`'s principal makes, one per `(action, seed-relative path)`.
fn events(shell: &Shell, requests: &[(Action, &str)]) -> Vec<Event> {
    requests
        .iter()
        .map(|(action, path)| event(shell, action.clone(), path))
        .collect()
}

/// Runs a line that must exit with `code` and publish, returning what it was granted.
async fn exits(shell: &Shell, line: &str, code: u8) -> Vec<Event> {
    let (exit, outcome) = run(shell, line).await;
    assert_eq!(exit, code, "`{line}` exits {code}");
    published(outcome).1
}

#[tokio::test]
#[serial]
async fn git_command_clone() {
    let fixture = Fixture::new();
    let origin = fixture.outside("origin");
    committed_repository(&origin, &[("p", "cloned\n")]);
    let vacant = fixture.outside("vacant.git");
    native_git(
        &fixture.seed,
        &["init", "-q", "--bare", vacant.to_str().expect("UTF-8")],
    );
    std::fs::create_dir_all(fixture.seed.join("repo")).expect("a directory with no repository");
    let shell = repository_shell(&fixture).await;

    let line = format!("git clone -q --no-local {} cloned", origin.display());
    assert_eq!(
        checked(&shell, &line).await,
        events(&shell, &[(Action::Checkout, "repo/cloned/p")])
    );
    let cloned = fixture.seed.join("repo/cloned");
    assert_eq!(
        git_out(&cloned, &["rev-parse", "HEAD"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(read(&cloned.join("p")), "cloned\n");
    assert_eq!(git_out(&cloned, &["ls-files"]), "p");
    assert!(git_ok(&cloned, &["diff", "--quiet", "HEAD"]), "index and worktree are HEAD's");
    assert_eq!(
        git_out(&cloned, &["config", "remote.origin.url"]),
        origin.display().to_string()
    );

    // An empty origin clones to an unborn repository: nothing but directories and metadata.
    let line = format!("git clone -q {} vacant 2>/dev/null", vacant.display());
    assert_eq!(checked(&shell, &line).await, Vec::new());
    let empty = fixture.seed.join("repo/vacant");
    assert_eq!(git_out(&empty, &["rev-parse", "--git-dir"]), ".git");
    assert!(git_ok(&empty, &["status"]));
}

#[tokio::test]
#[serial]
async fn git_command_init() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    std::fs::create_dir_all(&repo).expect("an empty directory");
    {
        let shell = repository_shell(&fixture).await;
        assert_eq!(checked(&shell, "git init -q -b main").await, Vec::new());
    }
    assert_eq!(git_out(&repo, &["rev-parse", "--git-dir"]), ".git");
    assert_eq!(git_out(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert!(!git_ok(&repo, &["rev-parse", "-q", "--verify", "HEAD"]), "an unborn branch");
    assert!(repo.join(".git/objects").is_dir() && repo.join(".git/refs/heads").is_dir());
    let names: Vec<_> = std::fs::read_dir(&repo)
        .expect("list the repository")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(names, [".git"], "no working file was invented");

    // A new session over the published seed: the repository works from there.
    let shell = repository_shell(&fixture).await;
    checked(&shell, "git status > /dev/null").await;
    assert_eq!(
        checked(
            &shell,
            "printf 'first\\n' > a.txt; git add -- a.txt; git commit -q -m first"
        )
        .await,
        events(
            &shell,
            &[
                (Action::Edit, "repo/a.txt"),
                (Action::Stage, "repo/a.txt"),
                (Action::commit("first"), "repo/a.txt"),
            ]
        )
    );
    assert_eq!(git_out(&repo, &["log", "--format=%s"]), "first");
}

#[tokio::test]
#[serial]
async fn git_command_add() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[("p", "v1\n"), ("gone", "gone\n"), (".gitignore", "ignored.txt\n")],
    );
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'v2\\n' > p; printf 'i\\n' > ignored.txt").await;

    assert_eq!(
        checked(&shell, "git add -- p").await,
        events(&shell, &[(Action::Stage, "repo/p")])
    );
    assert_eq!(git_out(&repo, &["show", ":p"]), "v2");
    assert_eq!(read(&repo.join("p")), "v2\n");
    assert_eq!(git_out(&repo, &["ls-files", "ignored.txt"]), "");

    // A deletion is staged like any other change.
    assert_eq!(
        checked(&shell, "rm gone; git add -- gone").await,
        events(&shell, &[(Action::Edit, "repo/gone"), (Action::Stage, "repo/gone")])
    );
    assert!(!git_ok(&repo, &["cat-file", "-e", ":gone"]));

    // A file git cannot read is git's refusal, and nothing is staged.
    let root = std::os::unix::fs::MetadataExt::uid(
        &std::fs::metadata("/proc/self").expect("this process"),
    ) == 0;
    if !root {
        let code = fixture.outside("unreadable-code");
        checked(
            &shell,
            &format!(
                "printf s > secret; chmod 000 secret; git add -- secret 2>/dev/null; \
                 echo $? > {}; chmod 600 secret",
                code.display()
            ),
        )
        .await;
        assert_eq!(read(&code), "128\n");
        assert!(!git_ok(&repo, &["cat-file", "-e", ":secret"]));
    }
}

#[tokio::test]
#[serial]
async fn git_command_mv() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "moved\n"), ("dst/keep", "k\n")]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git mv -- p dst/q").await,
        events(
            &shell,
            &[
                (Action::Edit, "repo/dst/q"),
                (Action::Stage, "repo/dst/q"),
                (Action::Delete, "repo/p"),
            ]
        )
    );
    assert!(!repo.join("p").exists());
    assert!(!git_ok(&repo, &["cat-file", "-e", ":p"]));
    assert_eq!(read(&repo.join("dst/q")), "moved\n");
    assert_eq!(git_out(&repo, &["show", ":dst/q"]), "moved");
}

#[tokio::test]
#[serial]
async fn git_command_restore() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "old\n")]);
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'new\\n' > p; git add -- p").await;

    assert_eq!(
        checked(&shell, "git restore --staged -- p").await,
        events(&shell, &[(Action::Unstage, "repo/p")])
    );
    assert_eq!(git_out(&repo, &["show", ":p"]), "old");
    assert_eq!(read(&repo.join("p")), "new\n");

    assert_eq!(
        checked(&shell, "git restore -- p").await,
        events(&shell, &[(Action::Checkout, "repo/p")])
    );
    assert_eq!(read(&repo.join("p")), "old\n", "the worktree came back from the index");
    assert_eq!(git_out(&repo, &["show", ":p"]), "old");
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
}

#[tokio::test]
#[serial]
async fn git_command_rm() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[("p", "p\n"), ("m", "m\n"), ("s", "s\n"), ("sm", "sm\n")],
    );
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git rm -q -- p").await,
        events(&shell, &[(Action::Delete, "repo/p")])
    );
    assert!(!repo.join("p").exists());
    assert!(!git_ok(&repo, &["cat-file", "-e", ":p"]));
    assert!(git_ok(&repo, &["cat-file", "-e", "HEAD:p"]));

    // Git's own safety boundary: a modified, a staged, and a staged-then-modified path.
    checked(
        &shell,
        "printf 'x\\n' > m; printf 'x\\n' > s; git add -- s; printf 'y\\n' > sm; git add -- sm; \
         printf 'z\\n' > sm",
    )
    .await;
    for path in ["m", "s", "sm"] {
        let before = (read(&repo.join(path)), git_out(&repo, &["show", &format!(":{path}")]));
        assert_eq!(
            exits(&shell, &format!("git rm -q -- {path} 2>/dev/null"), 1).await,
            Vec::new(),
            "{path}"
        );
        let after = (read(&repo.join(path)), git_out(&repo, &["show", &format!(":{path}")]));
        assert_eq!(before, after, "{path} is untouched");
    }
}

#[tokio::test]
#[serial]
async fn git_command_bisect() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "0\n")]);
    for step in 1..=4 {
        commit_files(&repo, &[("p", &format!("{step}\n"))], &format!("step {step}"));
    }
    let tip = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(checked(&shell, "git bisect start").await, Vec::new());
    assert_eq!(checked(&shell, "git bisect bad HEAD").await, Vec::new());
    assert_eq!(
        checked(&shell, "git bisect good HEAD~4 > /dev/null").await,
        events(&shell, &[(Action::Checkout, "repo/p")])
    );
    let chosen = read(&repo.join("p"));
    assert!(["1\n", "2\n", "3\n"].contains(&chosen.as_str()), "{chosen:?}");
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/bisect/bad"]),
        tip,
        "the bisection's bound is recorded"
    );

    assert_eq!(
        checked(&shell, "git bisect reset 2>/dev/null").await,
        events(&shell, &[(Action::Checkout, "repo/p")])
    );
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), tip);
    assert_eq!(read(&repo.join("p")), "4\n");
}

#[tokio::test]
#[serial]
async fn git_command_diff() {
    let fixture = Fixture::new();
    committed_repository(&fixture.seed.join("repo"), &[("p", "old\n")]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'new\\n' > p").await;

    let capture = fixture.outside("diff.txt");
    assert_eq!(
        checked(&shell, &format!("git diff -- p > {}", capture.display())).await,
        Vec::new()
    );
    let patch = read(&capture);
    assert!(patch.contains("\n-old\n") && patch.contains("\n+new\n"), "{patch}");
}

#[tokio::test]
#[serial]
async fn git_command_grep() {
    let fixture = Fixture::new();
    committed_repository(&fixture.seed.join("repo"), &[("p", "alpha\nbeta\n")]);
    let shell = repository_shell(&fixture).await;

    let capture = fixture.outside("grep.txt");
    assert_eq!(
        checked(&shell, &format!("git grep -n beta -- p > {}", capture.display())).await,
        Vec::new()
    );
    assert_eq!(read(&capture), "p:2:beta\n");
    assert_eq!(
        exits(&shell, "git grep -n gamma -- p", 1).await,
        Vec::new(),
        "no match is git's own exit 1, not a refusal"
    );
}

#[tokio::test]
#[serial]
async fn git_command_log() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("u", "u\n")]);
    commit_files(&repo, &[("p", "one\n")], "p one");
    commit_files(&repo, &[("u", "u2\n")], "unrelated");
    commit_files(&repo, &[("p", "two\n")], "p two");
    let shell = repository_shell(&fixture).await;

    let capture = fixture.outside("log.txt");
    assert_eq!(
        checked(&shell, &format!("git log --oneline -- p > {}", capture.display())).await,
        Vec::new()
    );
    let subjects: Vec<String> = read(&capture)
        .lines()
        .map(|line| line.split_once(' ').expect("hash and subject").1.to_string())
        .collect();
    assert_eq!(subjects, ["p two", "p one"]);
}

#[tokio::test]
#[serial]
async fn git_command_show() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "old\n")]);
    commit_files(&repo, &[("p", "new\n")], "update");
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'dirty\\n' > p").await;

    let capture = fixture.outside("show.txt");
    assert_eq!(
        checked(&shell, &format!("git show HEAD:p > {}", capture.display())).await,
        Vec::new()
    );
    assert_eq!(read(&capture), "new\n", "the committed bytes, not the worktree's");
}

#[tokio::test]
#[serial]
async fn git_command_backfill() {
    let fixture = Fixture::new();
    let origin = fixture.outside("origin");
    committed_repository(&origin, &[("a", "a1\n")]);
    commit_files(&origin, &[("a", "a2\n"), ("b", "b1\n")], "second");
    native_git(&origin, &["config", "uploadpack.allowFilter", "true"]);
    native_git(&origin, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let repo = fixture.seed.join("repo");
    native_git(
        &fixture.seed,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", origin.display()),
            "repo",
        ],
    );
    native_git(&repo, &["config", "protocol.file.allow", "always"]);
    let missing = || -> Vec<String> {
        native_git(
            &repo,
            &["--no-lazy-fetch", "rev-list", "--objects", "--all", "--missing=print"],
        )
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .map(str::to_string)
        .collect()
    };
    let absent = missing();
    assert!(!absent.is_empty(), "a real partial clone has promised blobs");
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git backfill --min-batch-size=1 2>/dev/null").await,
        Vec::new()
    );
    assert_eq!(missing(), Vec::<String>::new(), "every promised blob arrived");
    for object in &absent {
        assert!(git_ok(&repo, &["--no-lazy-fetch", "cat-file", "-e", object]), "{object}");
    }
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    let names: Vec<_> = std::fs::read_dir(&repo)
        .expect("list the repository")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(names, [".git"], "nothing was checked out");
}

#[tokio::test]
#[serial]
async fn git_command_branch() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "p\n")]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(checked(&shell, "git branch feature").await, Vec::new());
    assert_eq!(
        git_out(&repo, &["rev-parse", "feature"]),
        git_out(&repo, &["rev-parse", "HEAD"])
    );
    let capture = fixture.outside("branches.txt");
    checked(&shell, &format!("git branch --list > {}", capture.display())).await;
    assert!(read(&capture).contains("feature"));
    assert_eq!(checked(&shell, "git branch -q -d feature").await, Vec::new());
    assert!(!git_ok(&repo, &["rev-parse", "-q", "--verify", "refs/heads/feature"]));
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]), "no content moved");
}

#[tokio::test]
#[serial]
async fn git_command_commit() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[("p", "v1\n"), ("q", "q1\n"), ("r", "r1\n"), ("d", "d\n")],
    );
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'v2\\n' > p; git add -- p").await;

    assert_eq!(
        checked(&shell, "git commit -q -m saved").await,
        events(&shell, &[(Action::commit("saved"), "repo/p")])
    );
    assert_eq!(git_out(&repo, &["log", "-1", "--format=%s"]), "saved");
    assert_eq!(git_out(&repo, &["show", "HEAD:p"]), "v2");
    assert!(git_ok(&repo, &["diff", "--cached", "--quiet"]), "the index is clean");
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%an <%ae> %at %cn <%ce> %ct"]),
        "Test <test@example.com> 1112911993 Test <test@example.com> 1112911993"
    );

    // A partial commit takes the named path's worktree state, stages it first, and leaves the
    // other staged entry staged.
    checked(&shell, "printf 'r2\\n' > r; git add -- r; printf 'q2\\n' > q").await;
    assert_eq!(
        checked(&shell, "git commit -q -m partial -- q").await,
        events(
            &shell,
            &[(Action::Stage, "repo/q"), (Action::commit("partial"), "repo/q")]
        )
    );
    assert_eq!(git_out(&repo, &["show", "HEAD:q"]), "q2");
    assert_eq!(git_out(&repo, &["show", "HEAD:r"]), "r1");
    assert_eq!(git_out(&repo, &["show", ":r"]), "r2");

    // A deletion is committed like any other change.
    checked(&shell, "rm d; git add -- d").await;
    assert_eq!(
        checked(&shell, "git commit -q -m gone -- d").await,
        events(&shell, &[(Action::commit("gone"), "repo/d")])
    );
    assert!(!git_ok(&repo, &["cat-file", "-e", "HEAD:d"]));
}

#[tokio::test]
#[serial]
async fn git_command_history() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("u", "u1\n")]);
    commit_files(&repo, &[("p", "p1\n")], "add p");
    commit_files(&repo, &[("u", "u2\n")], "change u");
    let target = git_out(&repo, &["rev-parse", "HEAD~1"]);
    let child = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'p2\\n' > p; git add -- p").await;

    assert_eq!(
        checked(&shell, "git history fixup HEAD~1").await,
        events(&shell, &[(Action::commit("change u"), "repo/p")])
    );
    assert_ne!(git_out(&repo, &["rev-parse", "HEAD~1"]), target);
    assert_ne!(git_out(&repo, &["rev-parse", "HEAD"]), child);
    assert_eq!(git_out(&repo, &["log", "-1", "--format=%s", "HEAD~1"]), "add p");
    assert_eq!(git_out(&repo, &["show", "HEAD~1:p"]), "p2");
    assert_eq!(git_out(&repo, &["show", "HEAD:u"]), "u2");
    assert_eq!(read(&repo.join("p")), "p2\n");
}

#[tokio::test]
#[serial]
async fn git_command_merge() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "p\n")]);
    native_git(&repo, &["switch", "-q", "-c", "feature"]);
    commit_files(&repo, &[("q", "q\n")], "add q");
    native_git(&repo, &["switch", "-q", "main"]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git merge -q --ff-only feature").await,
        events(&shell, &[(Action::Checkout, "repo/q")])
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        git_out(&repo, &["rev-parse", "feature"])
    );
    assert_eq!(read(&repo.join("q")), "q\n");
    assert_eq!(git_out(&repo, &["show", ":q"]), "q");
}

#[tokio::test]
#[serial]
async fn git_command_rebase() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("base", "b\n")]);
    native_git(&repo, &["switch", "-q", "-c", "topic"]);
    commit_files(&repo, &[("t", "t\n")], "topic change");
    native_git(&repo, &["switch", "-q", "main"]);
    commit_files(&repo, &[("m", "m\n")], "main change");
    native_git(&repo, &["switch", "-q", "topic"]);
    let old = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    let granted = checked(&shell, "git rebase -q main").await;
    assert!(
        granted.contains(&event(&shell, Action::Checkout, "repo/m"))
            && granted.iter().all(|event| event.action == Action::Checkout),
        "{granted:?}"
    );
    assert_ne!(git_out(&repo, &["rev-parse", "HEAD"]), old);
    assert!(git_ok(&repo, &["merge-base", "--is-ancestor", "main", "HEAD"]));
    assert_eq!((read(&repo.join("t")), read(&repo.join("m"))), ("t\n".into(), "m\n".into()));
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]), "the index is clean");
}

#[tokio::test]
#[serial]
async fn git_command_reset() {
    let fixture = Fixture::new();
    for name in ["repo", "soft", "mixed"] {
        let dir = fixture.seed.join(name);
        committed_repository(&dir, &[("p", "one\n"), ("r", "r\n")]);
        commit_files(&dir, &[("p", "two\n"), ("q", "q\n")], "second");
    }
    let repo = fixture.seed.join("repo");
    let first = git_out(&repo, &["rev-parse", "HEAD~1"]);
    let shell = repository_shell(&fixture).await;
    checked(
        &shell,
        "printf 'staged\\n' > p; git add -- p; printf 'unstaged\\n' > r",
    )
    .await;

    assert_eq!(
        checked(&shell, "git reset -q --hard HEAD~1").await,
        events(
            &shell,
            &[
                (Action::Checkout, "repo/p"),
                (Action::Checkout, "repo/q"),
                (Action::Checkout, "repo/r"),
            ]
        )
    );
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), first);
    assert_eq!((read(&repo.join("p")), read(&repo.join("r"))), ("one\n".into(), "r\n".into()));
    assert!(!repo.join("q").exists());
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));

    // --soft moves HEAD alone, and claims no resource.
    let soft = fixture.seed.join("soft");
    let soft_first = git_out(&soft, &["rev-parse", "HEAD~1"]);
    assert_eq!(checked(&shell, "cd ../soft; git reset -q --soft HEAD~1").await, Vec::new());
    assert_eq!(git_out(&soft, &["rev-parse", "HEAD"]), soft_first);
    assert_eq!(git_out(&soft, &["show", ":p"]), "two");
    assert_eq!(read(&soft.join("p")), "two\n");

    // The default moves HEAD and the index, and leaves the worktree modified.
    let mixed = fixture.seed.join("mixed");
    let mixed_first = git_out(&mixed, &["rev-parse", "HEAD~1"]);
    assert_eq!(
        checked(&shell, "cd ../mixed; git reset -q HEAD~1").await,
        events(&shell, &[(Action::Edit, "mixed/p"), (Action::Edit, "mixed/q")])
    );
    assert_eq!(git_out(&mixed, &["rev-parse", "HEAD"]), mixed_first);
    assert_eq!(git_out(&mixed, &["show", ":p"]), "one");
    assert_eq!(read(&mixed.join("p")), "two\n");
    assert_eq!(read(&mixed.join("q")), "q\n");
}

#[tokio::test]
#[serial]
async fn git_command_switch() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "main\n")]);
    native_git(&repo, &["switch", "-q", "-c", "feature"]);
    commit_files(&repo, &[("p", "feature\n")], "feature p");
    native_git(&repo, &["switch", "-q", "main"]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git switch -q feature").await,
        events(&shell, &[(Action::Checkout, "repo/p")])
    );
    assert_eq!(git_out(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/feature");
    assert_eq!(read(&repo.join("p")), "feature\n");
    assert_eq!(git_out(&repo, &["show", ":p"]), "feature");

    // A switch that would overwrite a dirty file is git's refusal, and changes nothing.
    checked(&shell, "printf 'dirty\\n' > p").await;
    assert_eq!(exits(&shell, "git switch -q main 2>/dev/null", 1).await, Vec::new());
    assert_eq!(git_out(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/feature");
    assert_eq!(read(&repo.join("p")), "dirty\n");
}

#[tokio::test]
#[serial]
async fn git_command_tag() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "p\n")]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(checked(&shell, "git tag v1").await, Vec::new());
    assert_eq!(
        git_out(&repo, &["rev-parse", "v1"]),
        git_out(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(checked(&shell, "git tag -d v1 > /dev/null").await, Vec::new());
    assert!(!git_ok(&repo, &["rev-parse", "-q", "--verify", "refs/tags/v1"]));
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]), "no content moved");
}

/// A remote outside the seed, a client clone of it at `seed/repo`, and one more commit on the
/// remote that the client does not have yet.
fn advanced_remote(fixture: &Fixture) -> (PathBuf, PathBuf) {
    let origin = fixture.outside("origin");
    committed_repository(&origin, &[("p", "p\n")]);
    native_git(
        &fixture.seed,
        &["clone", "-q", origin.to_str().expect("UTF-8"), "repo"],
    );
    commit_files(&origin, &[("new", "n\n")], "advance");
    (origin, fixture.seed.join("repo"))
}

#[tokio::test]
#[serial]
async fn git_command_fetch() {
    let fixture = Fixture::new();
    let (origin, repo) = advanced_remote(&fixture);
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    assert_eq!(checked(&shell, "git fetch -q origin").await, Vec::new());
    assert_eq!(
        git_out(&repo, &["rev-parse", "origin/main"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!repo.join("new").exists());
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));
}

#[tokio::test]
#[serial]
async fn git_command_pull() {
    let fixture = Fixture::new();
    let (origin, repo) = advanced_remote(&fixture);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "git pull -q --ff-only origin main").await,
        events(&shell, &[(Action::Checkout, "repo/new")])
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(read(&repo.join("new")), "n\n");
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));
}

#[tokio::test]
#[serial]
async fn git_command_push() {
    let fixture = Fixture::new();
    let origin = fixture.outside("origin.git");
    native_git(
        &fixture.seed,
        &["init", "-q", "--bare", origin.to_str().expect("UTF-8")],
    );
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "p\n")]);
    native_git(&repo, &["remote", "add", "origin", origin.to_str().expect("UTF-8")]);
    native_git(&repo, &["push", "-q", "origin", "main"]);
    let shell = repository_shell(&fixture).await;
    checked(
        &shell,
        "printf 'n\\n' > n; git add -- n; git commit -q -m pushed",
    )
    .await;

    assert_eq!(checked(&shell, "git push -q origin main").await, Vec::new());
    let pushed = git_out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(git_out(&origin, &["rev-parse", "main"]), pushed);
    assert!(git_ok(&origin, &["cat-file", "-e", &format!("{pushed}:n")]));
}

#[tokio::test]
#[serial]
async fn git_command_stage() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "v1\n")]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'v2\\n' > p").await;

    assert_eq!(
        checked(&shell, "git stage -- p").await,
        events(&shell, &[(Action::Stage, "repo/p")])
    );
    assert_eq!(git_out(&repo, &["show", ":p"]), "v2");
}

#[tokio::test]
#[serial]
async fn git_command_checkout() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[("p", "committed\n"), ("target", "t\n"), ("run.sh", "#!/bin/sh\n")],
    );
    std::os::unix::fs::symlink("target", repo.join("link")).expect("a symlink");
    std::fs::set_permissions(
        repo.join("run.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    native_git(&repo, &["add", "-A"]);
    native_git(&repo, &["commit", "-q", "-m", "kinds"]);
    let shell = repository_shell(&fixture).await;

    checked(&shell, "printf 'dirty\\n' > p").await;
    assert_eq!(
        checked(&shell, "git checkout HEAD -- p").await,
        events(&shell, &[(Action::Checkout, "repo/p")])
    );
    assert_eq!(read(&repo.join("p")), "committed\n");

    // A restored symlink is a link to the same target, and an executable stays executable.
    checked(&shell, "rm link; ln -s p link; chmod -x run.sh").await;
    assert_eq!(
        checked(&shell, "git checkout HEAD -- link run.sh").await,
        events(
            &shell,
            &[(Action::Checkout, "repo/link"), (Action::Checkout, "repo/run.sh")]
        )
    );
    assert_eq!(
        std::fs::read_link(repo.join("link")).expect("a symlink"),
        Path::new("target")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(repo.join("run.sh")).expect("run.sh").permissions(),
    );
    assert_ne!(mode & 0o111, 0, "{mode:o}");
}

#[tokio::test]
#[serial]
async fn git_command_stash() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "clean\n"), ("d", "d\n")]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'dirty\\n' > p; printf 'new\\n' > n; rm d").await;

    assert_eq!(
        checked(&shell, "git stash push -q -u").await,
        events(
            &shell,
            &[
                (Action::Stash, "repo/d"),
                (Action::Stash, "repo/n"),
                (Action::Stash, "repo/p"),
            ]
        )
    );
    assert_eq!((read(&repo.join("p")), read(&repo.join("d"))), ("clean\n".into(), "d\n".into()));
    assert!(!repo.join("n").exists());
    assert_eq!(git_out(&repo, &["stash", "list"]).lines().count(), 1);

    let capture = fixture.outside("stash.txt");
    assert_eq!(
        checked(
            &shell,
            &format!("git stash show -p --include-untracked > {}", capture.display())
        )
        .await,
        Vec::new()
    );
    let shown = read(&capture);
    assert!(shown.contains("+dirty") && shown.contains("+new"), "{shown}");

    assert_eq!(
        checked(&shell, "git stash pop -q").await,
        events(
            &shell,
            &[(Action::Edit, "repo/d"), (Action::Edit, "repo/n"), (Action::Edit, "repo/p")]
        )
    );
    assert_eq!((read(&repo.join("p")), read(&repo.join("n"))), ("dirty\n".into(), "new\n".into()));
    assert!(!repo.join("d").exists());
    assert_eq!(git_out(&repo, &["stash", "list"]), "");
}

#[tokio::test]
#[serial]
async fn git_command_clean() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "tracked\n")]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 's\\n' > scratch.txt; printf 'k\\n' > keep.txt").await;

    let capture = fixture.outside("clean.txt");
    assert_eq!(
        checked(&shell, &format!("git clean -n > {}", capture.display())).await,
        Vec::new()
    );
    assert!(read(&capture).contains("Would remove scratch.txt"));
    assert!(repo.join("scratch.txt").exists() && repo.join("keep.txt").exists());

    assert_eq!(
        checked(&shell, "git clean -f -q -- scratch.txt").await,
        events(&shell, &[(Action::Clean, "repo/scratch.txt")])
    );
    assert!(!repo.join("scratch.txt").exists());
    assert_eq!(read(&repo.join("keep.txt")), "k\n");
    assert_eq!(read(&repo.join("p")), "tracked\n", "tracked files are never cleaned");
}

/// A shell of its own over `seed`, acting as `principal`, sharing `validator`'s history.
async fn shell_as(
    seed: &MarshExecutor,
    validator: &Arc<Mutex<PolicyValidator>>,
    principal: &str,
) -> Shell {
    Shell::build(
        seed.snapshot(Principal::from(principal))
            .unwrap_or_else(|error| panic!("{principal}'s snapshot: {error}")),
        validator.clone(),
    )
    .await
    .unwrap_or_else(|error| panic!("build {principal}: {error}"))
}

/// A control file outside every seed that a held command waits for.
///
/// Explicit, because a test that held a command with `sleep 2` would be asserting about a
/// duration rather than about an order: the barrier is opened when the other half of the scenario
/// has provably finished, and never a moment earlier.
struct Gate(PathBuf);

impl Gate {
    fn new(fixture: &Fixture, name: &str) -> Self {
        Self(fixture.outside(name))
    }

    /// The shell fragment that blocks until the gate is opened.
    fn wait(&self) -> String {
        format!(
            "while [ ! -e {} ]; do sleep 0.02; done",
            self.0.display()
        )
    }

    fn open(&self) {
        std::fs::write(&self.0, b"").expect("open the gate");
    }
}

/// A file outside every seed that a line appends to once per evaluation.
///
/// This is how a test tells "the line ran again" from "the line took longer": nothing inside the
/// snapshot can answer that, because a replay throws the snapshot away.
struct Attempts(PathBuf);

impl Attempts {
    fn new(fixture: &Fixture, name: &str) -> Self {
        Self(fixture.outside(name))
    }

    /// The shell fragment that records one evaluation.
    fn record(&self) -> String {
        format!("printf 'x\\n' >> {}", self.0.display())
    }

    fn count(&self) -> usize {
        std::fs::read_to_string(&self.0).map_or(0, |text| text.lines().count())
    }
}

/// Waits for `condition`, failing the test rather than hanging when it never holds.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The capabilities a denied line was refused.
fn denied(outcome: &Outcome) -> &[Denial] {
    match outcome {
        Outcome::Denied { denials, .. } => denials,
        other => panic!("expected a denied line, got {other:?}"),
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

/// Every seed-relative destination the log's file and symlink `MOVE` records name.
///
/// A directory's `MOVE` is a change of shape, not of content, and is left out.
fn moved(records: &[WalRecord<PublishMeta>]) -> Vec<PathBuf> {
    records
        .iter()
        .filter_map(|record| match record {
            WalRecord::Move {
                to,
                directory_mode: None,
                ..
            } => Some(to.clone()),
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

/// A newline-terminated record that fails to parse or typecheck resets the whole WAL: the
/// history and ownership it carried are gone, publication restarts at sequence 1, and the seed
/// keeps whatever it already had. An actual I/O failure reading the log is a different thing
/// entirely, and must still refuse to open a session.
#[tokio::test]
#[serial]
async fn wal_parse_errors_reset_history_but_io_errors_still_fail() {
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

    let mut raw = std::fs::read(fixture.log()).expect("read the valid log");
    raw.extend_from_slice(b"{ this is not a record }\n");
    std::fs::write(fixture.log(), raw).expect("append a record that fails to parse");

    let shell = fixture.shell().await;
    assert_eq!(
        std::fs::read(fixture.seed.join("owned")).expect("the seed's file"),
        b"first",
        "the seed keeps what was already published"
    );
    let (code, outcome) = run(&shell, "printf second > owned").await;
    assert_eq!(code, 0);
    assert_eq!(
        published(outcome).0,
        Publication { seq: 1, ops: 1 },
        "the reset log's history and ownership are gone, so publication restarts at sequence 1"
    );
    assert_eq!(
        std::fs::read(fixture.seed.join("owned")).expect("the seed's file"),
        b"second",
        "the new shell could publish over the formerly owned path"
    );
    drop(shell);

    std::fs::remove_file(fixture.log()).expect("remove the WAL");
    std::fs::create_dir(fixture.log()).expect("put a directory where the WAL was");
    let error = fixture
        .open()
        .expect_err("a log that cannot be read yields no session");
    assert!(
        matches!(error, MarshError::Wal(marsh_wal::Error::Io(_))),
        "an I/O failure reading the log is a refusal, not a parse error to reset: got {error:?}"
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
            directory_mode: None,
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

/// The reported conflict, exactly as two terminals produce it: one shell writes an unstaged file
/// and the next shell's append to the *same* file is refused.
///
/// Both snapshots exist before either line runs, which is what makes this a concurrency question
/// rather than a sequence of edits by one owner. The refusal is a capability decision — the second
/// shell may not edit a resource the first holds unstaged — and it is final: there is nothing to
/// rerun, because rerunning would be refused for the same reason.
#[tokio::test]
#[serial]
async fn an_unstaged_file_another_shell_owns_cannot_be_edited() {
    let fixture = Fixture::new();
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let first = shell_as(&seed, &validator, "first").await;
    let second = shell_as(&seed, &validator, "second").await;

    let (code, outcome) = run(&first, "echo foo > test.txt").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0, Publication { seq: 1, ops: 1 });

    let (code, outcome) = run(&second, "echo foo2 >> test.txt").await;
    assert_eq!(code, 0, "the program itself succeeded");
    let denials = denied(&outcome);
    assert_eq!(
        denials.iter().map(|denial| &denial.event).collect::<Vec<_>>(),
        vec![&event(&second, Action::Edit, "test.txt")],
        "the refusal names this shell's edit of the file the other one owns"
    );

    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("test.txt")).expect("the seed's file"),
        "foo\n",
        "the seed carries the owner's bytes and nothing appended to them"
    );
    assert_eq!(
        validator.lock().expect("the history").history(),
        [Event::new(Principal::from("first"), Action::Edit, ["test.txt"])],
        "a denial grants nothing"
    );
    assert_eq!(
        fixture.wal().iter().filter(|record| matches!(record, WalRecord::Begin { .. })).count(),
        1,
        "and publishes nothing"
    );
}

/// Two shells truncating the same file: the loser is denied, not asked to try again.
///
/// A truncating write depends on nothing that was there before, so there is no read to
/// resynchronize and nothing a second evaluation would decide differently. What is left is the
/// ownership question, and the capability policy is what answers it.
#[tokio::test]
#[serial]
async fn a_concurrent_unstaged_edit_is_denied() {
    let fixture = Fixture::new();
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let a = shell_as(&seed, &validator, "a").await;
    let b = shell_as(&seed, &validator, "b").await;

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
        .await
        .expect("conclude b's line");
    drop(guard);
    assert_eq!(
        denied(&outcome)
            .iter()
            .map(|denial| &denial.event)
            .collect::<Vec<_>>(),
        vec![&event(&b, Action::Edit, "p.txt")],
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

/// A line that *read* a file another shell republishes while it ran is evaluated again, against
/// the bytes that are now current.
///
/// The proof is the attempt counter reaching two while the line is still held at its second
/// barrier: the replay is a reaction to the observed read, not something that happens once a line
/// finishes and is found to be out of date.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_read_of_a_republished_file_is_evaluated_again() {
    let fixture = Fixture::new();
    std::fs::write(fixture.seed.join("foo.txt"), b"old\n").expect("seed foo.txt");
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(shell_as(&seed, &validator, "reader").await);
    let writer = shell_as(&seed, &validator, "writer").await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let before = Gate::new(&fixture, "before-read");
    let after = Gate::new(&fixture, "after-read");
    let line = format!(
        "{}; {}; /bin/cat foo.txt > observed.txt; {}",
        attempts.record(),
        before.wait(),
        after.wait()
    );

    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        let line = line.clone();
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;

    let (code, outcome) = run(&writer, "printf 'new\n' > foo.txt").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0.seq, 1);

    // Only the first barrier: the reader must react to the read it then makes, while it is still
    // held at the second one.
    before.open();
    until("the reader's second evaluation", || attempts.count() >= 2).await;
    assert!(
        !fixture.seed.join("observed.txt").exists(),
        "the abandoned evaluation published nothing"
    );

    after.open();
    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    let (publication, granted) = published(outcome);
    assert_eq!(publication.ops, 1, "only its own file: {publication:?}");
    assert_eq!(
        granted,
        vec![event(&reader, Action::Edit, "observed.txt")],
        "the replay requested what it wrote, and the abandoned attempt requested nothing"
    );
    assert_eq!(
        attempts.count(),
        2,
        "one invalidation, one replay — not a loop"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("observed.txt")).expect("the observation"),
        "new\n",
        "the line saw the bytes that are current, not the ones it started against"
    );
}

/// The same dependency, read by the shell itself rather than by a program it started.
///
/// A redirection and a `read` builtin never reach the spawner, so this is the case a
/// command-line-shaped rule would miss entirely: what says the file was read is the syscall.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn an_in_process_read_is_a_dependency_too() {
    let fixture = Fixture::new();
    std::fs::write(fixture.seed.join("foo.txt"), b"old\n").expect("seed foo.txt");
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(shell_as(&seed, &validator, "reader").await);
    let writer = shell_as(&seed, &validator, "writer").await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let before = Gate::new(&fixture, "before-read");
    let after = Gate::new(&fixture, "after-read");
    let line = format!(
        "{}; {}; read -r seen < foo.txt; printf '%s\\n' \"$seen\" > observed.txt; {}",
        attempts.record(),
        before.wait(),
        after.wait()
    );

    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;

    run(&writer, "printf 'new\n' > foo.txt").await;
    before.open();
    until("the reader's second evaluation", || attempts.count() >= 2).await;
    after.open();

    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    published(outcome);
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("observed.txt")).expect("the observation"),
        "new\n"
    );
}

/// A publication of a file this line never touched changes nothing about it.
///
/// Concurrency is the point of the design: two shells doing unrelated work must both make
/// progress, and a whole-seed version number would make every publication invalidate every other
/// shell.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_publication_of_another_file_does_not_replay_a_reader() {
    let fixture = Fixture::new();
    std::fs::write(fixture.seed.join("foo.txt"), b"old\n").expect("seed foo.txt");
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(shell_as(&seed, &validator, "reader").await);
    let writer = shell_as(&seed, &validator, "writer").await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let before = Gate::new(&fixture, "before-read");
    let after = Gate::new(&fixture, "after-read");
    let line = format!(
        "{}; {}; /bin/cat foo.txt > observed.txt; {}",
        attempts.record(),
        before.wait(),
        after.wait()
    );

    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;

    // A different file entirely, published while the reader is held.
    let (code, outcome) = run(&writer, "printf 'bar\n' > bar.txt").await;
    assert_eq!(code, 0);
    assert_eq!(published(outcome).0.ops, 1);

    before.open();
    after.open();
    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    published(outcome);
    assert_eq!(
        attempts.count(),
        1,
        "an unrelated publication is not a dependency"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("observed.txt")).expect("the observation"),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("bar.txt")).expect("the writer's file"),
        "bar\n"
    );
}

/// Staging a file releases it, and the older shell's append to it is then evaluated against the
/// staged bytes rather than refused.
///
/// The replay here is driven by the append's own read of `foo.txt`; the publication it then makes
/// has to carry both lines, which is what proves it started again from the new content instead of
/// merging over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_released_file_is_appended_to_after_resynchronizing() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);
    std::fs::write(fixture.seed.join("foo.txt"), b"first\n").expect("seed foo.txt");
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(shell_as(&seed, &validator, "reader").await);
    let writer = shell_as(&seed, &validator, "writer").await;
    export_git_identity(&reader).await;
    export_git_identity(&writer).await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let before = Gate::new(&fixture, "before-append");
    let line = format!(
        "{}; {}; printf 'second\\n' >> foo.txt",
        attempts.record(),
        before.wait()
    );

    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;

    // The writer edits and then stages, which is what releases the resource.
    run(&writer, "printf 'staged\n' > foo.txt").await;
    let (code, outcome) = run(&writer, "git add -- foo.txt").await;
    assert_eq!(code, 0);
    published(outcome);

    before.open();
    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    published(outcome);
    assert!(
        attempts.count() >= 2,
        "the append read a file the writer had republished: {}",
        attempts.count()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("foo.txt")).expect("the seed's file"),
        "staged\nsecond\n",
        "the append started again from the staged bytes"
    );
}

/// Two shells staging *different* files: both entries survive, because the dependency that forces
/// the replay is the index each one actually read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_staging_of_different_files_keeps_both_entries() {
    let fixture = Fixture::new();
    init_repository(&fixture.seed);
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let held_shell = Arc::new(shell_as(&seed, &validator, "held").await);
    let other = shell_as(&seed, &validator, "other").await;
    export_git_identity(&held_shell).await;
    export_git_identity(&other).await;

    let attempts = Attempts::new(&fixture, "held-attempts");
    let before = Gate::new(&fixture, "before-add");
    let line = format!(
        "{}; printf 'held\\n' > held.txt; {}; git add -- held.txt",
        attempts.record(),
        before.wait()
    );

    let held = tokio::spawn({
        let shell = Arc::clone(&held_shell);
        async move { shell.run(&line).await }
    });
    until("the held shell's first evaluation", || attempts.count() >= 1).await;

    run(&other, "printf 'other\n' > other.txt").await;
    let (code, outcome) = run(&other, "git add -- other.txt").await;
    assert_eq!(code, 0);
    published(outcome);

    before.open();
    let (_, outcome) = held.await.expect("join the held shell").expect("its line");
    published(outcome);

    let repository = git2::Repository::open(&fixture.seed).expect("open the seed's repository");
    let index = repository.index().expect("the index");
    let staged: Vec<String> = index
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    assert!(
        staged.contains(&"held.txt".to_string()) && staged.contains(&"other.txt".to_string()),
        "both staged entries survive: {staged:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("held.txt")).expect("held.txt"),
        "held\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.seed.join("other.txt")).expect("other.txt"),
        "other\n"
    );
}

/// A shell over `seed` for `principal` with the fixture git environment, standing in `repo`.
async fn repository_shell_as(
    seed: &MarshExecutor,
    validator: &Arc<Mutex<PolicyValidator>>,
    principal: &str,
) -> Shell {
    let shell = shell_as(seed, validator, principal).await;
    export_git_identity(&shell).await;
    checked(&shell, "cd repo").await;
    shell
}

/// A git read that another principal's publication made stale is evaluated again against the
/// current bytes — its local edit and its in-snapshot output redirection with it — and observing
/// a resource claims nothing: the next principal may still delete what was read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn git_policy_a_fresh_grep_claims_nothing() {
    let fixture = Fixture::new();
    committed_repository(
        &fixture.seed.join("repo"),
        &[("remote.txt", "needle old\n"), ("other.txt", "needle other\n")],
    );
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(repository_shell_as(&seed, &validator, "reader").await);
    let writer = repository_shell_as(&seed, &validator, "writer").await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let gate = Gate::new(&fixture, "before-grep");
    let line = format!(
        "{}; printf 'local\\n' > local.txt; {}; git grep -h needle -- remote.txt > found.txt",
        attempts.record(),
        gate.wait()
    );
    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;
    checked(&writer, "printf 'needle new\\n' > remote.txt").await;
    gate.open();

    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    assert_eq!(
        published(outcome).1,
        events(
            &reader,
            &[(Action::Edit, "repo/local.txt"), (Action::Edit, "repo/found.txt")]
        ),
        "the grep itself requested nothing"
    );
    assert!(attempts.count() >= 2, "the stale read was evaluated again");
    let repo = fixture.seed.join("repo");
    assert_eq!(read(&repo.join("found.txt")), "needle new\n");
    assert_eq!(read(&repo.join("local.txt")), "local\n");
    assert!(
        validator
            .lock()
            .expect("the history")
            .history()
            .iter()
            .all(|event| !event.action.is_read()),
        "no read claim entered the history"
    );

    checked(&reader, "git grep -q needle -- other.txt").await;
    assert_eq!(
        checked(&writer, "git rm -q -- other.txt").await,
        events(&writer, &[(Action::Delete, "repo/other.txt")]),
        "a path somebody only read is still anybody's to delete"
    );
}

/// A read whose line also edited a path another principal has since edited is evaluated again,
/// and the replay's edit is then refused: nothing of the line — its edit, its git state — reaches
/// the seed or the history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn git_policy_a_read_after_a_conflicting_edit_is_denied() {
    let fixture = Fixture::new();
    committed_repository(&fixture.seed.join("repo"), &[("p", "base\n")]);
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let reader = Arc::new(repository_shell_as(&seed, &validator, "reader").await);
    let writer = repository_shell_as(&seed, &validator, "writer").await;

    let attempts = Attempts::new(&fixture, "reader-attempts");
    let gate = Gate::new(&fixture, "before-grep");
    let line = format!(
        "{}; printf 'reader\\n' > p; {}; git grep -q reader -- p",
        attempts.record(),
        gate.wait()
    );
    let held = tokio::spawn({
        let reader = Arc::clone(&reader);
        async move { reader.run(&line).await }
    });
    until("the reader's first evaluation", || attempts.count() >= 1).await;
    checked(&writer, "printf 'writer\\n' > p").await;
    let history = validator.lock().expect("the history").history().to_vec();
    gate.open();

    let (_, outcome) = held.await.expect("join the reader").expect("the reader's line");
    assert_eq!(
        denied(&outcome)
            .iter()
            .map(|denial| denial.event.clone())
            .collect::<Vec<_>>(),
        events(&reader, &[(Action::Edit, "repo/p")])
    );
    assert_eq!(read(&fixture.seed.join("repo/p")), "writer\n");
    assert_eq!(validator.lock().expect("the history").history(), history.as_slice());
}

/// A hard reset in one principal's tree is a checkout of every path it restores: refused over
/// another principal's unstaged edit — seed, repository and history untouched — and granted to
/// the owner, after which the path is anybody's again.
#[tokio::test]
#[serial]
async fn git_policy_a_hard_reset_cannot_discard_another_principals_edit() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "base\n")]);
    let validator = validator();
    let seed = fixture.open().expect("attach to the seed");
    let owner = repository_shell_as(&seed, &validator, "owner").await;
    let other = repository_shell_as(&seed, &validator, "other").await;
    checked(&owner, "printf 'owned\\n' > p").await;
    let index = git_out(&repo, &["ls-files", "--stage"]);
    let history = validator.lock().expect("the history").history().to_vec();

    let (code, outcome) = run(&other, "git reset -q --hard").await;
    assert_eq!(code, 0, "git itself succeeded in the other principal's tree");
    assert_eq!(
        denied(&outcome)
            .iter()
            .map(|denial| denial.event.clone())
            .collect::<Vec<_>>(),
        events(&other, &[(Action::Checkout, "repo/p")])
    );
    assert_eq!(read(&repo.join("p")), "owned\n");
    assert_eq!(git_out(&repo, &["ls-files", "--stage"]), index);
    assert_eq!(validator.lock().expect("the history").history(), history.as_slice());

    assert_eq!(
        checked(&owner, "git reset -q --hard").await,
        events(&owner, &[(Action::Checkout, "repo/p")]),
        "the owner's reset restores its own edit"
    );
    assert_eq!(read(&repo.join("p")), "base\n");
    assert_eq!(
        checked(&other, "printf 'next\\n' > p").await,
        events(&other, &[(Action::Edit, "repo/p")])
    );
}

/// Metadata a git command creates is never an edit, and never hides one: the files a line wrote
/// before `git init --bare` turned their directory into a repository are still requested.
#[tokio::test]
#[serial]
async fn git_policy_metadata_hides_no_ordinary_write() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.seed.join("repo")).expect("a directory");
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(
            &shell,
            "mkdir meta; printf 'x\\n' > meta/notes; git init -q --bare meta"
        )
        .await,
        events(&shell, &[(Action::Edit, "repo/meta/notes")])
    );
    assert_eq!(
        git_out(&fixture.seed.join("repo/meta"), &["rev-parse", "--is-bare-repository"]),
        "true"
    );
    assert_eq!(
        checked(
            &shell,
            "git init -q --separate-git-dir=separate work; printf 'w\\n' > work/w"
        )
        .await,
        events(&shell, &[(Action::Edit, "repo/work/w")])
    );
}

/// Every request is a real transition of a real path: an implicit `add -A` over an index larger
/// than a pipe asks only for what changed, repeated transitions of one path survive in order,
/// two commits in one line keep their own messages, a write undone within its line asks for
/// nothing, and a write after a checkout is an edit after it.
#[tokio::test]
#[serial]
async fn git_policy_requests_are_exact_footprints() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    let files: Vec<(String, String)> = (0..3000)
        .map(|n| (format!("f/{n:04}"), format!("{n}\n")))
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect();
    committed_repository(&repo, &borrowed);
    commit_files(&repo, &[("p", "p\n"), ("q", "q\n")], "two more");
    assert!(git_out(&repo, &["ls-files", "--stage", "-z"]).len() > 64 * 1024);
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        checked(&shell, "printf 'x\\n' > f/0007; git add -A").await,
        events(&shell, &[(Action::Edit, "repo/f/0007"), (Action::Stage, "repo/f/0007")])
    );
    assert_eq!(
        checked(
            &shell,
            "printf a > p; git add -- p; printf b > p; git add -- p"
        )
        .await,
        events(
            &shell,
            &[
                (Action::Edit, "repo/p"),
                (Action::Stage, "repo/p"),
                (Action::Edit, "repo/p"),
                (Action::Stage, "repo/p"),
            ]
        )
    );
    assert_eq!(
        checked(
            &shell,
            "git commit -q -m one; printf 'q2\\n' > q; git add -- q; git commit -q -m two"
        )
        .await,
        events(
            &shell,
            &[
                (Action::commit("one"), "repo/f/0007"),
                (Action::commit("one"), "repo/p"),
                (Action::Edit, "repo/q"),
                (Action::Stage, "repo/q"),
                (Action::commit("two"), "repo/q"),
            ]
        )
    );
    assert_eq!(checked(&shell, "printf z > z; rm z").await, Vec::new());
    checked(&shell, "printf 'dirty\\n' > p").await;
    assert_eq!(
        checked(&shell, "git checkout HEAD -- p; printf 'later\\n' > p").await,
        events(&shell, &[(Action::Checkout, "repo/p"), (Action::Edit, "repo/p")])
    );
}

/// A merge that stops on a conflict exits 1 and still changed the tree: its conflict markers and
/// unmerged index are published, as the edit they are.
#[tokio::test]
#[serial]
async fn git_policy_a_conflicted_merge_is_an_edit() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "base\n")]);
    native_git(&repo, &["switch", "-q", "-c", "feature"]);
    commit_files(&repo, &[("p", "feature\n")], "feature p");
    native_git(&repo, &["switch", "-q", "main"]);
    commit_files(&repo, &[("p", "main\n")], "main p");
    let shell = repository_shell(&fixture).await;

    assert_eq!(
        exits(&shell, "git merge feature > /dev/null", 1).await,
        events(&shell, &[(Action::Edit, "repo/p")])
    );
    assert!(read(&repo.join("p")).contains("<<<<<<<"));
    assert!(!git_out(&repo, &["ls-files", "-u"]).is_empty(), "the index is unmerged");
}

/// Git runs through the shell's recorded spawner — the command and its probes are spawn records
/// like any other — and two inspections can share a pipeline, however much one feeds the other:
/// the consumer reads every byte of an output far larger than a pipe holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn git_policy_git_runs_are_recorded_and_inspections_pipe() {
    let fixture = Fixture::new();
    let lines = (0..20_000).fold(String::new(), |mut lines, n| {
        lines.push_str("needle ");
        lines.push_str(&n.to_string());
        lines.push('\n');
        lines
    });
    assert!(lines.len() > 64 * 1024);
    committed_repository(&fixture.seed.join("repo"), &[("p", &lines), ("q", &lines)]);
    let shell = repository_shell(&fixture).await;
    let mark = shell.executor().spawn_records().len();

    assert_eq!(
        checked(
            &shell,
            "git grep -h needle -- p | git diff --no-index --exit-code - q"
        )
        .await,
        Vec::new(),
        "the consumer saw exactly what the producer printed"
    );

    checked(&shell, "printf 'n\\n' > n; git add -- n").await;
    let gits: Vec<Vec<String>> = shell.executor().spawn_records()[mark..]
        .iter()
        .filter_map(|record| match record {
            SpawnRecord::Spawned { request, .. } if request.program.ends_with("git") => {
                Some(request.args.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        gits.iter().any(|args| args.first().map(String::as_str) == Some("add")),
        "the command itself: {gits:?}"
    );
    assert!(
        gits.iter().any(|args| args.iter().any(|arg| arg == "ls-files")),
        "and the probes around it: {gits:?}"
    );
}

/// Two state-changing gits at once in one tree cannot be told apart, so the second is refused
/// without running and the line fails whole; a git still running at the boundary is ended and
/// fails its line the same way. Neither leaves anything in the seed or the history, and the
/// shell goes on working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn git_policy_unattributable_gits_fail_their_line() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "p\n")]);
    let started = fixture.outside("hook-started");
    let gate = Gate::new(&fixture, "hook-gate");
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch {}\n{}\n", started.display(), gate.wait()),
    )
    .expect("a hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod the hook");
    let validator = validator();
    let shell = Shell::build(fixture.open().expect("attach"), Arc::clone(&validator))
        .await
        .expect("build the shell");
    export_git_identity(&shell).await;
    checked(&shell, "cd repo").await;
    checked(&shell, "printf 'p2\\n' > p; git add -- p").await;
    let history = validator.lock().expect("the history").history().to_vec();
    let head = git_out(&repo, &["rev-parse", "HEAD"]);

    let tag_code = fixture.outside("tag-code");
    let overlapping = format!(
        "git commit -q -m held & while [ ! -e {} ]; do sleep 0.02; done; git tag v1 2>/dev/null; \
         echo $? > {}; touch {}; wait",
        started.display(),
        tag_code.display(),
        gate.0.display()
    );
    let Err(error) = shell.run(&overlapping).await else {
        panic!("an unattributable line has no verdict");
    };
    assert!(error.to_string().contains("overlapping"), "{error}");
    assert_eq!(read(&tag_code), "128\n", "the second git never ran");
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!git_ok(&repo, &["rev-parse", "-q", "--verify", "refs/tags/v1"]));
    assert_eq!(validator.lock().expect("the history").history(), history.as_slice());

    std::fs::remove_file(&started).expect("reset the hook's marker");
    std::fs::remove_file(&gate.0).expect("close the gate");
    let Err(error) = shell
        .run(&format!(
            "git commit -q -m background & while [ ! -e {} ]; do sleep 0.02; done",
            started.display()
        ))
        .await
    else {
        panic!("a git still running at the boundary fails its line");
    };
    assert!(error.to_string().contains("still running"), "{error}");
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match shell.run("printf 'ok\\n' > ok.txt").await {
            Ok((_, outcome)) => {
                assert_eq!(published(outcome).1, events(&shell, &[(Action::Edit, "repo/ok.txt")]));
                break;
            }
            Err(error) => {
                assert!(Instant::now() < deadline, "the shell never recovered: {error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// A published line leaves nothing behind to publish again: a following line that changes
/// nothing — an inspection included — writes no transaction and requests nothing.
#[tokio::test]
#[serial]
async fn git_policy_a_published_line_is_not_republished() {
    let fixture = Fixture::new();
    committed_repository(&fixture.seed.join("repo"), &[("p", "p\n")]);
    let shell = repository_shell(&fixture).await;
    checked(&shell, "printf 'x\\n' > x").await;
    let records = fixture.wal().len();
    for line in [
        "true",
        "git status > /dev/null",
        "git grep -q p -- p",
        "git --version > /dev/null",
        "git > /dev/null; true",
        "true",
    ] {
        let (code, outcome) = run(&shell, line).await;
        assert_eq!(code, 0, "{line}");
        let (publication, granted) = published(outcome);
        assert_eq!((publication.ops, granted), (0, Vec::new()), "{line}");
    }
    assert_eq!(fixture.wal().len(), records, "no transaction was logged");
}

/// The host's git configuration reaches a managed git no more than a stock one.
#[tokio::test]
#[serial]
async fn git_policy_host_configuration_is_isolated() {
    let fixture = Fixture::new();
    committed_repository(&fixture.seed.join("repo"), &[("p", "p\n")]);
    let hostile = fixture.outside("hostile.gitconfig");
    std::fs::write(&hostile, "[alias]\n\tst = status\n[core]\n\tautocrlf = true\n")
        .expect("a hostile configuration");
    let shell = repository_shell(&fixture).await;
    let exports = format!(
        "export GIT_CONFIG_GLOBAL={0} GIT_CONFIG_SYSTEM={0} GIT_CONFIG_NOSYSTEM=",
        hostile.display()
    );
    checked(&shell, &exports).await;
    assert_eq!(exits(&shell, "git st 2>/dev/null", 1).await, Vec::new());
    assert_eq!(
        exits(&shell, "git config --get core.autocrlf", 1).await,
        Vec::new()
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
