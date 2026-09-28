//! Existing Git semantic contracts, with policy details kept below the public facade.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::policy::{Action, Event, Resource};
use super::super::{Shell, ShellVariable};
use super::{Fixture, close, read, session};
use serial_test::serial;
use std::path::{Path, PathBuf};

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
async fn export_git_identity(shell: &Shell) {
    for (name, value) in NATIVE_GIT_ENV {
        let mut variable = ShellVariable::new(value);
        variable.export();
        shell.set_var(name, variable).await.unwrap();
    }
}
fn native_git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(NATIVE_GIT_ENV)
        .output()
        .expect("native git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
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
fn committed_repository(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    native_git(dir, &["init", "-q", "-b", "main"]);
    commit_files(dir, files, "initial");
}
fn commit_files(dir: &Path, files: &[(&str, &str)], subject: &str) {
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    native_git(dir, &["add", "-A"]);
    native_git(dir, &["commit", "-q", "--allow-empty", "-m", subject]);
}
/// The names directly inside `dir`.
fn names(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .expect("list the repository")
        .map(|entry| entry.expect("an entry").file_name())
        .collect()
}
fn event(shell: &Shell, action: Action, path: &str) -> Event {
    Event::new(
        shell.principal().clone(),
        action,
        Resource::from(path.split('/').map(str::to_owned).collect::<Vec<_>>()),
    )
}
/// These migrated cases check Git/file transitions. Content-read claims and directory claims have
/// their own consumer regressions; neither is silently omitted from the actual authority history.
async fn run(shell: &Shell, line: &str) -> (u8, Vec<Event>) {
    let session = session(shell).await;
    let before = session.validator.read().history.len();
    let result = shell
        .run(line)
        .await
        .unwrap_or_else(|error| panic!("{line}: {error}"));
    let authority = session.validator.read();
    let granted = authority.history[before..]
        .iter()
        .filter(|event| {
            event.action.is_write()
                && !session
                    .persistence
                    .seed
                    .join(event.resource.segments().iter().collect::<PathBuf>())
                    .is_dir()
        })
        .cloned()
        .collect();
    drop(authority);
    (result.exit_code.into(), granted)
}
async fn checked(shell: &Shell, line: &str) -> Vec<Event> {
    let (code, events) = run(shell, line).await;
    assert_eq!(code, 0, "{line}");
    events
}
/// Runs `line`, which must exit 0 and be granted exactly `requests`, each an action on a
/// seed-relative path.
async fn grants(shell: &Shell, line: &str, requests: &[(Action, &str)]) {
    let expected: Vec<_> = requests
        .iter()
        .map(|(action, path)| event(shell, action.clone(), path))
        .collect();
    assert_eq!(checked(shell, line).await, expected, "{line}");
}
/// Runs a line that must exit with `code` and publish, returning what it was granted.
async fn exits(shell: &Shell, line: &str, code: u8) -> Vec<Event> {
    let (exit, events) = run(shell, line).await;
    assert_eq!(exit, code, "`{line}` exits {code}");
    events
}
async fn refused_git(shell: &Shell, line: &str, code: u8) {
    let error = shell
        .run(line)
        .await
        .err()
        .expect("failed Git mutation is refused");
    assert!(
        matches!(error.kind(), super::super::ShellErrorKind::Unsupported),
        "{error}"
    );
    assert_eq!(
        u8::from(error.execution_result().expect("native status").exit_code),
        code
    );
}
async fn repository_shell(fixture: &Fixture) -> Shell {
    let shell = fixture.shell().await;
    export_git_identity(&shell).await;
    checked(&shell, "cd repo").await;
    shell
}
/// A fresh fixture whose `seed/repo` commits `files`, and a shell working in that repository.
async fn repository(files: &[(&str, &str)]) -> (Fixture, PathBuf, Shell) {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, files);
    let shell = repository_shell(&fixture).await;
    (fixture, repo, shell)
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
async fn git_command_status() {
    let (fixture, _, shell) = repository(&[
        ("staged.txt", "old\n"),
        ("tracked.txt", "old\n"),
        ("sub/inner.txt", "inner\n"),
        (".gitignore", "ignored.txt\n"),
    ])
    .await;
    checked(
        &shell,
        "printf 'new\\n' > staged.txt; git add -- staged.txt",
    )
    .await;
    checked(&shell, "printf 'changed\\n' > tracked.txt; printf 'u\\n' > untracked.txt; printf 'i\\n' > ignored.txt").await;

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

    let human = read(&human);
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
    assert!(
        !human.contains("ignored.txt"),
        "ignored paths are not listed: {human}"
    );
    let rows = "M  staged.txt\n M tracked.txt\n?? untracked.txt\n";
    assert_eq!(read(&porcelain), rows);
    assert_eq!(
        read(&child),
        rows,
        "porcelain v1 paths are repository-relative from a subdirectory"
    );
    close(shell).await;
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

    grants(
        &shell,
        &format!("git clone -q --no-local {} cloned", origin.display()),
        &[(Action::Checkout, "repo/cloned/p")],
    )
    .await;
    let cloned = fixture.seed.join("repo/cloned");
    assert_eq!(
        git_out(&cloned, &["rev-parse", "HEAD"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(read(&cloned.join("p")), "cloned\n");
    assert_eq!(git_out(&cloned, &["ls-files"]), "p");
    assert!(
        git_ok(&cloned, &["diff", "--quiet", "HEAD"]),
        "index and worktree are HEAD's"
    );
    assert_eq!(
        git_out(&cloned, &["config", "remote.origin.url"]),
        origin.display().to_string()
    );

    // An empty origin clones to an unborn repository: nothing but directories and metadata.
    grants(
        &shell,
        &format!("git clone -q {} vacant 2>/dev/null", vacant.display()),
        &[],
    )
    .await;
    let empty = fixture.seed.join("repo/vacant");
    assert_eq!(git_out(&empty, &["rev-parse", "--git-dir"]), ".git");
    assert!(git_ok(&empty, &["status"]));
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_init() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    std::fs::create_dir_all(&repo).expect("an empty directory");
    {
        let shell = repository_shell(&fixture).await;
        grants(&shell, "git init -q -b main", &[]).await;
    }
    assert_eq!(git_out(&repo, &["rev-parse", "--git-dir"]), ".git");
    assert_eq!(git_out(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert!(
        !git_ok(&repo, &["rev-parse", "-q", "--verify", "HEAD"]),
        "an unborn branch"
    );
    assert!(repo.join(".git/objects").is_dir() && repo.join(".git/refs/heads").is_dir());
    assert_eq!(names(&repo), [".git"], "no working file was invented");

    // A new session over the published seed: the repository works from there.
    let shell = repository_shell(&fixture).await;
    checked(&shell, "git status > /dev/null").await;
    grants(
        &shell,
        "printf 'first\\n' > a.txt; git add -- a.txt; git commit -q -m first",
        &[
            (Action::Edit, "repo/a.txt"),
            (Action::Stage, "repo/a.txt"),
            (Action::commit("first"), "repo/a.txt"),
        ],
    )
    .await;
    assert_eq!(git_out(&repo, &["log", "--format=%s"]), "first");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_add() {
    let (fixture, repo, shell) = repository(&[
        ("p", "v1\n"),
        ("gone", "gone\n"),
        (".gitignore", "ignored.txt\n"),
    ])
    .await;
    checked(&shell, "printf 'v2\\n' > p; printf 'i\\n' > ignored.txt").await;

    grants(&shell, "git add -- p", &[(Action::Stage, "repo/p")]).await;
    assert_eq!(git_out(&repo, &["show", ":p"]), "v2");
    assert_eq!(read(&repo.join("p")), "v2\n");
    assert_eq!(git_out(&repo, &["ls-files", "ignored.txt"]), "");

    // A deletion is staged like any other change.
    grants(
        &shell,
        "rm gone; git add -- gone",
        &[(Action::Edit, "repo/gone"), (Action::Stage, "repo/gone")],
    )
    .await;
    assert!(!git_ok(&repo, &["cat-file", "-e", ":gone"]));

    // A file git cannot read is git's refusal, and nothing is staged.
    let root = std::os::unix::fs::MetadataExt::uid(
        &std::fs::metadata("/proc/self").expect("this process"),
    ) == 0;
    if !root {
        let code = fixture.outside("unreadable-code");
        let line = format!(
            "printf s > secret; chmod 000 secret; git add -- secret 2>/dev/null; echo $? > {}; chmod 600 secret",
            code.display()
        );
        refused_git(&shell, &line, 0).await;
        assert_eq!(read(&code), "128\n");
        assert!(!git_ok(&repo, &["cat-file", "-e", ":secret"]));
        assert!(
            !repo.join("secret").exists(),
            "the unrelated edit in the refused line is also discarded"
        );
    }
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_mv() {
    let (_fixture, repo, shell) = repository(&[("p", "moved\n"), ("dst/keep", "k\n")]).await;

    grants(
        &shell,
        "git mv -- p dst/q",
        &[
            (Action::Edit, "repo/dst/q"),
            (Action::Stage, "repo/dst/q"),
            (Action::Delete, "repo/p"),
        ],
    )
    .await;
    assert!(!repo.join("p").exists());
    assert!(!git_ok(&repo, &["cat-file", "-e", ":p"]));
    assert_eq!(read(&repo.join("dst/q")), "moved\n");
    assert_eq!(git_out(&repo, &["show", ":dst/q"]), "moved");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_restore() {
    let (_fixture, repo, shell) = repository(&[("p", "old\n")]).await;
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    checked(&shell, "printf 'new\\n' > p; git add -- p").await;

    grants(
        &shell,
        "git restore --staged -- p",
        &[(Action::Unstage, "repo/p")],
    )
    .await;
    assert_eq!(git_out(&repo, &["show", ":p"]), "old");
    assert_eq!(read(&repo.join("p")), "new\n");

    grants(&shell, "git restore -- p", &[(Action::Checkout, "repo/p")]).await;
    assert_eq!(
        read(&repo.join("p")),
        "old\n",
        "the worktree came back from the index"
    );
    assert_eq!(git_out(&repo, &["show", ":p"]), "old");
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_rm() {
    let (_fixture, repo, shell) =
        repository(&[("p", "p\n"), ("m", "m\n"), ("s", "s\n"), ("sm", "sm\n")]).await;

    grants(&shell, "git rm -q -- p", &[(Action::Delete, "repo/p")]).await;
    assert!(!repo.join("p").exists());
    assert!(!git_ok(&repo, &["cat-file", "-e", ":p"]));
    assert!(git_ok(&repo, &["cat-file", "-e", "HEAD:p"]));

    // Git's own safety boundary: a modified, a staged, and a staged-then-modified path.
    checked(&shell, "printf 'x\\n' > m; printf 'x\\n' > s; git add -- s; printf 'y\\n' > sm; git add -- sm; printf 'z\\n' > sm").await;
    for path in ["m", "s", "sm"] {
        let before = (
            read(&repo.join(path)),
            git_out(&repo, &["show", &format!(":{path}")]),
        );
        refused_git(&shell, &format!("git rm -q -- {path} 2>/dev/null"), 1).await;
        let after = (
            read(&repo.join(path)),
            git_out(&repo, &["show", &format!(":{path}")]),
        );
        assert_eq!(before, after, "{path} is untouched");
    }
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_bisect() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(&repo, &[("p", "0\n")]);
    for step in 1..=4 {
        commit_files(
            &repo,
            &[("p", &format!("{step}\n"))],
            &format!("step {step}"),
        );
    }
    let tip = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    grants(&shell, "git bisect start", &[]).await;
    grants(&shell, "git bisect bad HEAD", &[]).await;
    grants(
        &shell,
        "git bisect good HEAD~4 > /dev/null",
        &[(Action::Checkout, "repo/p")],
    )
    .await;
    let chosen = read(&repo.join("p"));
    assert!(
        ["1\n", "2\n", "3\n"].contains(&chosen.as_str()),
        "{chosen:?}"
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/bisect/bad"]),
        tip,
        "the bisection's bound is recorded"
    );

    grants(
        &shell,
        "git bisect reset 2>/dev/null",
        &[(Action::Checkout, "repo/p")],
    )
    .await;
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), tip);
    assert_eq!(read(&repo.join("p")), "4\n");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_diff() {
    let (fixture, _, shell) = repository(&[("p", "old\n")]).await;
    checked(&shell, "printf 'new\\n' > p").await;

    let capture = fixture.outside("diff.txt");
    grants(
        &shell,
        &format!("git diff -- p > {}", capture.display()),
        &[],
    )
    .await;
    let patch = read(&capture);
    assert!(
        patch.contains("\n-old\n") && patch.contains("\n+new\n"),
        "{patch}"
    );
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_grep() {
    let (fixture, _, shell) = repository(&[("p", "alpha\nbeta\n")]).await;

    let capture = fixture.outside("grep.txt");
    grants(
        &shell,
        &format!("git grep -n beta -- p > {}", capture.display()),
        &[],
    )
    .await;
    assert_eq!(read(&capture), "p:2:beta\n");
    assert_eq!(
        exits(&shell, "git grep -n gamma -- p", 1).await,
        Vec::new(),
        "no match is git's own exit 1, not a refusal"
    );
    close(shell).await;
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
    grants(
        &shell,
        &format!("git log --oneline -- p > {}", capture.display()),
        &[],
    )
    .await;
    let subjects: Vec<String> = read(&capture)
        .lines()
        .map(|line| {
            line.split_once(' ')
                .expect("hash and subject")
                .1
                .to_string()
        })
        .collect();
    assert_eq!(subjects, ["p two", "p one"]);
    close(shell).await;
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
    grants(
        &shell,
        &format!("git show HEAD:p > {}", capture.display()),
        &[],
    )
    .await;
    assert_eq!(
        read(&capture),
        "new\n",
        "the committed bytes, not the worktree's"
    );
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_backfill() {
    let fixture = Fixture::new();
    let origin = fixture.outside("origin");
    committed_repository(&origin, &[("a", "a1\n")]);
    commit_files(&origin, &[("a", "a2\n"), ("b", "b1\n")], "second");
    native_git(&origin, &["config", "uploadpack.allowFilter", "true"]);
    native_git(
        &origin,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
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
            &[
                "--no-lazy-fetch",
                "rev-list",
                "--objects",
                "--all",
                "--missing=print",
            ],
        )
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .map(str::to_string)
        .collect()
    };
    let absent = missing();
    assert!(
        !absent.is_empty(),
        "a real partial clone has promised blobs"
    );
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    grants(&shell, "git backfill --min-batch-size=1 2>/dev/null", &[]).await;
    assert_eq!(
        missing(),
        Vec::<String>::new(),
        "every promised blob arrived"
    );
    for object in &absent {
        assert!(
            git_ok(&repo, &["--no-lazy-fetch", "cat-file", "-e", object]),
            "{object}"
        );
    }
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(names(&repo), [".git"], "nothing was checked out");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_branch() {
    let (fixture, repo, shell) = repository(&[("p", "p\n")]).await;

    grants(&shell, "git branch feature", &[]).await;
    assert_eq!(
        git_out(&repo, &["rev-parse", "feature"]),
        git_out(&repo, &["rev-parse", "HEAD"])
    );
    let capture = fixture.outside("branches.txt");
    checked(
        &shell,
        &format!("git branch --list > {}", capture.display()),
    )
    .await;
    assert!(read(&capture).contains("feature"));
    grants(&shell, "git branch -q -d feature", &[]).await;
    assert!(!git_ok(
        &repo,
        &["rev-parse", "-q", "--verify", "refs/heads/feature"]
    ));
    assert!(
        git_ok(&repo, &["diff", "--quiet", "HEAD"]),
        "no content moved"
    );
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_commit() {
    let (_fixture, repo, shell) =
        repository(&[("p", "v1\n"), ("q", "q1\n"), ("r", "r1\n"), ("d", "d\n")]).await;
    checked(&shell, "printf 'v2\\n' > p; git add -- p").await;

    grants(
        &shell,
        "git commit -q -m saved",
        &[(Action::commit("saved"), "repo/p")],
    )
    .await;
    assert_eq!(git_out(&repo, &["log", "-1", "--format=%s"]), "saved");
    assert_eq!(git_out(&repo, &["show", "HEAD:p"]), "v2");
    assert!(
        git_ok(&repo, &["diff", "--cached", "--quiet"]),
        "the index is clean"
    );
    assert_eq!(
        git_out(
            &repo,
            &["log", "-1", "--format=%an <%ae> %at %cn <%ce> %ct"]
        ),
        "Test <test@example.com> 1112911993 Test <test@example.com> 1112911993"
    );

    // A partial commit takes the named path's worktree state, stages it first, and leaves the
    // other staged entry staged.
    checked(
        &shell,
        "printf 'r2\\n' > r; git add -- r; printf 'q2\\n' > q",
    )
    .await;
    grants(
        &shell,
        "git commit -q -m partial -- q",
        &[
            (Action::Stage, "repo/q"),
            (Action::commit("partial"), "repo/q"),
        ],
    )
    .await;
    assert_eq!(git_out(&repo, &["show", "HEAD:q"]), "q2");
    assert_eq!(git_out(&repo, &["show", "HEAD:r"]), "r1");
    assert_eq!(git_out(&repo, &["show", ":r"]), "r2");

    // A deletion is committed like any other change.
    checked(&shell, "rm d; git add -- d").await;
    grants(
        &shell,
        "git commit -q -m gone -- d",
        &[(Action::commit("gone"), "repo/d")],
    )
    .await;
    assert!(!git_ok(&repo, &["cat-file", "-e", "HEAD:d"]));
    close(shell).await;
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

    grants(
        &shell,
        "git history fixup HEAD~1",
        &[(Action::commit("change u"), "repo/p")],
    )
    .await;
    assert_ne!(git_out(&repo, &["rev-parse", "HEAD~1"]), target);
    assert_ne!(git_out(&repo, &["rev-parse", "HEAD"]), child);
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "add p"
    );
    assert_eq!(git_out(&repo, &["show", "HEAD~1:p"]), "p2");
    assert_eq!(git_out(&repo, &["show", "HEAD:u"]), "u2");
    assert_eq!(read(&repo.join("p")), "p2\n");
    close(shell).await;
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

    grants(
        &shell,
        "git merge -q --ff-only feature",
        &[(Action::Checkout, "repo/q")],
    )
    .await;
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        git_out(&repo, &["rev-parse", "feature"])
    );
    assert_eq!(read(&repo.join("q")), "q\n");
    assert_eq!(git_out(&repo, &["show", ":q"]), "q");
    close(shell).await;
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
    assert!(git_ok(
        &repo,
        &["merge-base", "--is-ancestor", "main", "HEAD"]
    ));
    assert_eq!(
        (read(&repo.join("t")), read(&repo.join("m"))),
        ("t\n".into(), "m\n".into())
    );
    assert!(
        git_ok(&repo, &["diff", "--quiet", "HEAD"]),
        "the index is clean"
    );
    close(shell).await;
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

    grants(
        &shell,
        "git reset -q --hard HEAD~1",
        &[
            (Action::Checkout, "repo/p"),
            (Action::Checkout, "repo/q"),
            (Action::Checkout, "repo/r"),
        ],
    )
    .await;
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        (read(&repo.join("p")), read(&repo.join("r"))),
        ("one\n".into(), "r\n".into())
    );
    assert!(!repo.join("q").exists());
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));

    // --soft moves HEAD alone, and claims no resource.
    let soft = fixture.seed.join("soft");
    let soft_first = git_out(&soft, &["rev-parse", "HEAD~1"]);
    grants(&shell, "cd ../soft; git reset -q --soft HEAD~1", &[]).await;
    assert_eq!(git_out(&soft, &["rev-parse", "HEAD"]), soft_first);
    assert_eq!(git_out(&soft, &["show", ":p"]), "two");
    assert_eq!(read(&soft.join("p")), "two\n");

    // The default moves HEAD and the index, and leaves the worktree modified.
    let mixed = fixture.seed.join("mixed");
    let mixed_first = git_out(&mixed, &["rev-parse", "HEAD~1"]);
    grants(
        &shell,
        "cd ../mixed; git reset -q HEAD~1",
        &[(Action::Edit, "mixed/p"), (Action::Edit, "mixed/q")],
    )
    .await;
    assert_eq!(git_out(&mixed, &["rev-parse", "HEAD"]), mixed_first);
    assert_eq!(git_out(&mixed, &["show", ":p"]), "one");
    assert_eq!(read(&mixed.join("p")), "two\n");
    assert_eq!(read(&mixed.join("q")), "q\n");
    close(shell).await;
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

    grants(
        &shell,
        "git switch -q feature",
        &[(Action::Checkout, "repo/p")],
    )
    .await;
    assert_eq!(
        git_out(&repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(read(&repo.join("p")), "feature\n");
    assert_eq!(git_out(&repo, &["show", ":p"]), "feature");

    // A switch that would overwrite a dirty file is git's refusal, and changes nothing.
    checked(&shell, "printf 'dirty\\n' > p").await;
    refused_git(&shell, "git switch -q main 2>/dev/null", 1).await;
    assert_eq!(
        git_out(&repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(read(&repo.join("p")), "dirty\n");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_tag() {
    let (_fixture, repo, shell) = repository(&[("p", "p\n")]).await;

    grants(&shell, "git tag v1", &[]).await;
    assert_eq!(
        git_out(&repo, &["rev-parse", "v1"]),
        git_out(&repo, &["rev-parse", "HEAD"])
    );
    grants(&shell, "git tag -d v1 > /dev/null", &[]).await;
    assert!(!git_ok(
        &repo,
        &["rev-parse", "-q", "--verify", "refs/tags/v1"]
    ));
    assert!(
        git_ok(&repo, &["diff", "--quiet", "HEAD"]),
        "no content moved"
    );
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_fetch() {
    let fixture = Fixture::new();
    let (origin, repo) = advanced_remote(&fixture);
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    let shell = repository_shell(&fixture).await;

    grants(&shell, "git fetch -q origin", &[]).await;
    assert_eq!(
        git_out(&repo, &["rev-parse", "origin/main"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!repo.join("new").exists());
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_pull() {
    let fixture = Fixture::new();
    let (origin, repo) = advanced_remote(&fixture);
    let shell = repository_shell(&fixture).await;

    grants(
        &shell,
        "git pull -q --ff-only origin main",
        &[(Action::Checkout, "repo/new")],
    )
    .await;
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        git_out(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(read(&repo.join("new")), "n\n");
    assert!(git_ok(&repo, &["diff", "--quiet", "HEAD"]));
    close(shell).await;
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
    native_git(
        &repo,
        &["remote", "add", "origin", origin.to_str().expect("UTF-8")],
    );
    native_git(&repo, &["push", "-q", "origin", "main"]);
    let shell = repository_shell(&fixture).await;
    checked(
        &shell,
        "printf 'n\\n' > n; git add -- n; git commit -q -m pushed",
    )
    .await;

    grants(&shell, "git push -q origin main", &[]).await;
    let pushed = git_out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(git_out(&origin, &["rev-parse", "main"]), pushed);
    assert!(git_ok(&origin, &["cat-file", "-e", &format!("{pushed}:n")]));
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_stage() {
    let (_fixture, repo, shell) = repository(&[("p", "v1\n")]).await;
    checked(&shell, "printf 'v2\\n' > p").await;

    grants(&shell, "git stage -- p", &[(Action::Stage, "repo/p")]).await;
    assert_eq!(git_out(&repo, &["show", ":p"]), "v2");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_checkout() {
    let fixture = Fixture::new();
    let repo = fixture.seed.join("repo");
    committed_repository(
        &repo,
        &[
            ("p", "committed\n"),
            ("target", "t\n"),
            ("run.sh", "#!/bin/sh\n"),
        ],
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
    grants(
        &shell,
        "git checkout HEAD -- p",
        &[(Action::Checkout, "repo/p")],
    )
    .await;
    assert_eq!(read(&repo.join("p")), "committed\n");

    // A restored symlink is a link to the same target, and an executable stays executable.
    checked(&shell, "rm link; ln -s p link; chmod -x run.sh").await;
    grants(
        &shell,
        "git checkout HEAD -- link run.sh",
        &[
            (Action::Checkout, "repo/link"),
            (Action::Checkout, "repo/run.sh"),
        ],
    )
    .await;
    assert_eq!(
        std::fs::read_link(repo.join("link")).expect("a symlink"),
        Path::new("target")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(repo.join("run.sh"))
            .expect("run.sh")
            .permissions(),
    );
    assert_ne!(mode & 0o111, 0, "{mode:o}");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_stash() {
    let (fixture, repo, shell) = repository(&[("p", "clean\n"), ("d", "d\n")]).await;
    checked(&shell, "printf 'dirty\\n' > p; printf 'new\\n' > n; rm d").await;

    grants(
        &shell,
        "git stash push -q -u",
        &[
            (Action::Stash, "repo/d"),
            (Action::Stash, "repo/n"),
            (Action::Stash, "repo/p"),
        ],
    )
    .await;
    assert_eq!(
        (read(&repo.join("p")), read(&repo.join("d"))),
        ("clean\n".into(), "d\n".into())
    );
    assert!(!repo.join("n").exists());
    assert_eq!(git_out(&repo, &["stash", "list"]).lines().count(), 1);

    let capture = fixture.outside("stash.txt");
    grants(
        &shell,
        &format!(
            "git stash show -p --include-untracked > {}",
            capture.display()
        ),
        &[],
    )
    .await;
    let shown = read(&capture);
    assert!(
        shown.contains("+dirty") && shown.contains("+new"),
        "{shown}"
    );

    grants(
        &shell,
        "git stash pop -q",
        &[
            (Action::Edit, "repo/d"),
            (Action::Edit, "repo/n"),
            (Action::Edit, "repo/p"),
        ],
    )
    .await;
    assert_eq!(
        (read(&repo.join("p")), read(&repo.join("n"))),
        ("dirty\n".into(), "new\n".into())
    );
    assert!(!repo.join("d").exists());
    assert_eq!(git_out(&repo, &["stash", "list"]), "");
    close(shell).await;
}

#[tokio::test]
#[serial]
async fn git_command_clean() {
    let (fixture, repo, shell) = repository(&[("p", "tracked\n")]).await;
    checked(
        &shell,
        "printf 's\\n' > scratch.txt; printf 'k\\n' > keep.txt",
    )
    .await;

    let capture = fixture.outside("clean.txt");
    grants(
        &shell,
        &format!("git clean -n > {}", capture.display()),
        &[],
    )
    .await;
    assert!(read(&capture).contains("Would remove scratch.txt"));
    assert!(repo.join("scratch.txt").exists() && repo.join("keep.txt").exists());

    grants(
        &shell,
        "git clean -f -q -- scratch.txt",
        &[(Action::Clean, "repo/scratch.txt")],
    )
    .await;
    assert!(!repo.join("scratch.txt").exists());
    assert_eq!(read(&repo.join("keep.txt")), "k\n");
    assert_eq!(
        read(&repo.join("p")),
        "tracked\n",
        "tracked files are never cleaned"
    );
    close(shell).await;
}
