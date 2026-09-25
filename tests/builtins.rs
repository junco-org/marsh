#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! What the `git` builtin does before and around the system git it runs, and what `exec` does in
//! a shell that has to stay alive.
//!
//! These are the paths a unit test cannot reach: each one is decided from shell state — the
//! working directory, the environment, the snapshot boundary — so the only honest way to exercise
//! them is through a real `brush_core::Shell` that has the builtin registered. What each test pins
//! is what a script actually observes: the exit code, and the repository or tree it leaves.

use std::path::{Path, PathBuf};

use brush_builtins::BuiltinSet;
use brush_core::{
    ExecutionControlFlow, ProfileLoadBehavior, RcLoadBehavior, Shell, ShellVariable, SourceInfo,
};
use marsh::builtins::{SNAPSHOT_ROOT_VAR, exec_builtins, git_builtins};

/// The git identity variables a commit needs, all three of them, for both sides.
const IDENTITY: [(&str, &str); 3] = [
    ("NAME", "Test"),
    ("EMAIL", "test@example.com"),
    ("DATE", "1112911993 +0000"),
];

/// Builds the shell under test: stock builtins plus this module's `git` and `exec`.
///
/// `identity` says whether the pinned git identity is exported, so a test can show which git
/// commands need one and which do not.
async fn shell(working_dir: PathBuf, identity: bool) -> Shell {
    let builtins = {
        let mut builtins = brush_builtins::default_builtins(BuiltinSet::BashMode);
        builtins.extend(git_builtins());
        builtins.extend(exec_builtins());
        builtins
    };
    let mut shell = Shell::builder()
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(working_dir)
        .builtins(builtins)
        .build()
        .await
        .expect("build the shell");

    for who in ["AUTHOR", "COMMITTER"] {
        for (suffix, value) in IDENTITY {
            let name = format!("GIT_{who}_{suffix}");
            if identity {
                export(&mut shell, &name, value);
            } else {
                shell.env_mut().unset(&name).expect("unset an identity");
            }
        }
    }
    shell
}

/// Exports one variable into the shell's environment.
fn export(shell: &mut Shell, name: &str, value: &str) {
    let mut variable = ShellVariable::new(value);
    variable.export();
    shell
        .env_mut()
        .set_global(name.to_string(), variable)
        .expect("set an environment variable");
}

/// Runs one line, returning its exit code.
async fn run(shell: &mut Shell, line: &str) -> u8 {
    let params = shell.default_exec_params();
    shell
        .run_string(line, &SourceInfo::from("test"), &params)
        .await
        .expect("run the line")
        .exit_code
        .into()
}

/// Runs one line with its stderr captured, returning `(exit code, stderr)`.
async fn run_capturing(shell: &mut Shell, at: &Path, line: &str) -> (u8, String) {
    let capture = at.join("stderr.txt");
    let code = run(shell, &format!("{line} 2> {}", capture.display())).await;
    let text = std::fs::read_to_string(&capture).expect("read the captured stderr");
    std::fs::remove_file(&capture).expect("drop the capture");
    (code, text)
}

/// A repository whose root commit is empty, built through libgit2 rather than through the builtin.
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

/// A canonicalized scratch directory, because the builtin compares it against the repository root.
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("scratch directory");
    let path = directory.path().canonicalize().expect("canonical path");
    (directory, path)
}

/// The paths the index of the repository at `work` holds, read through libgit2.
fn staged(work: &Path) -> Vec<String> {
    let repository = git2::Repository::open(work).expect("open the repository");
    let index = repository.index().expect("index");
    index
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect()
}

/// Command lines the builtin used to refuse — no pathspec, a glob, a restore from the index, a
/// clean without `-f` — are git's to run, and each does what git does.
#[tokio::test]
async fn formerly_refused_forms_run_natively() {
    let (_guard, work) = scratch();
    init_repository(&work);
    std::fs::write(work.join("a.txt"), b"a\n").expect("a.txt");
    std::fs::write(work.join("b.txt"), b"b\n").expect("b.txt");
    let mut shell = shell(work.clone(), true).await;

    assert_eq!(run(&mut shell, "git add 2>/dev/null").await, 0);
    assert!(staged(&work).is_empty(), "nothing specified, nothing added");
    assert_eq!(run(&mut shell, "git add -- '*.txt'").await, 0);
    assert_eq!(staged(&work), ["a.txt", "b.txt"], "the glob is git's pathspec");

    std::fs::write(work.join("a.txt"), b"changed\n").expect("modify a.txt");
    assert_eq!(run(&mut shell, "git restore -- a.txt").await, 0);
    assert_eq!(
        std::fs::read(work.join("a.txt")).expect("a.txt"),
        b"a\n",
        "the worktree came back from the index"
    );

    std::fs::write(work.join("untracked.txt"), b"u\n").expect("an untracked file");
    assert_eq!(
        run(&mut shell, "git clean -- untracked.txt 2>/dev/null").await,
        128,
        "git itself requires -f"
    );
    assert!(work.join("untracked.txt").exists());
}

#[tokio::test]
async fn a_working_directory_with_no_repository_above_it_is_fatal() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), true).await;
    export(&mut shell, SNAPSHOT_ROOT_VAR, &work.display().to_string());

    let (code, reported) = run_capturing(&mut shell, &work, "git add -- p").await;
    assert_eq!(code, 128, "{reported}");
}

/// Git searches ancestors for a repository, but never above the tree the builtin was given: a
/// repository beside that tree is not the command's repository.
#[tokio::test]
async fn the_snapshot_root_stops_the_ancestor_search() {
    let (_guard, outer) = scratch();
    init_repository(&outer);
    let inner = outer.join("tree/deep");
    std::fs::create_dir_all(&inner).expect("dirs");
    std::fs::write(inner.join("p"), b"p\n").expect("p");

    let mut shell = shell(inner.clone(), true).await;
    assert_eq!(
        run(&mut shell, "git add -- p").await,
        0,
        "without a boundary the outer repository is found"
    );
    assert_eq!(staged(&outer), ["tree/deep/p"]);

    export(
        &mut shell,
        SNAPSHOT_ROOT_VAR,
        &outer.join("tree").display().to_string(),
    );
    std::fs::write(inner.join("q"), b"q\n").expect("q");
    assert_eq!(run(&mut shell, "git add -- q 2>/dev/null").await, 128);
    assert_eq!(staged(&outer), ["tree/deep/p"], "q was staged nowhere");
}

/// Naming a repository or a destination outside the tree is refused before git starts: an
/// explicit `-C` or `--git-dir`, a clone or init target, a separate git directory. Nothing is
/// created and nothing is staged outside.
#[tokio::test]
async fn a_repository_or_destination_outside_the_snapshot_is_refused() {
    let (_guard, outer) = scratch();
    init_repository(&outer);
    std::fs::write(outer.join("p"), b"p\n").expect("p");
    let tree = outer.join("tree");
    std::fs::create_dir_all(&tree).expect("the tree");
    let mut shell = shell(tree.clone(), true).await;
    export(&mut shell, SNAPSHOT_ROOT_VAR, &tree.display().to_string());

    for line in [
        format!("git -C {} add -- p", outer.display()),
        format!("git --git-dir={}/.git --work-tree={} add -- p", outer.display(), outer.display()),
        format!("GIT_DIR={}/.git git status", outer.display()),
        format!("git clone {} ../escape", outer.display()),
        format!("git clone --separate-git-dir ../meta {} inside", outer.display()),
        "git init ../escape".to_string(),
        "git init --separate-git-dir=../meta inside".to_string(),
    ] {
        assert_eq!(
            run(&mut shell, &format!("{line} 2>/dev/null")).await,
            128,
            "{line}"
        );
    }
    assert!(staged(&outer).is_empty(), "nothing was staged outside");
    for created in ["escape", "meta", "tree/inside"] {
        assert!(!outer.join(created).exists(), "{created} was never created");
    }

    assert_eq!(run(&mut shell, "git init -q inside").await, 0, "inside is allowed");
    assert!(tree.join("inside/.git").is_dir());
}

/// A git identity is git's business: inspecting and staging need none, and a commit without one
/// fails in git itself — leaving `HEAD` and the index as they were — rather than in the builtin.
#[tokio::test]
async fn only_git_decides_what_needs_an_identity() {
    let (_guard, work) = scratch();
    init_repository(&work);
    std::fs::write(work.join("p"), b"p\n").expect("p");
    let mut shell = shell(work.clone(), false).await;
    for name in ["EMAIL", "GIT_EDITOR"] {
        shell.env_mut().unset(name).expect("unset");
    }

    assert_eq!(run(&mut shell, "git status > /dev/null").await, 0);
    assert_eq!(run(&mut shell, "git add -- p").await, 0);
    let head = || {
        git2::Repository::open(&work)
            .expect("open")
            .head()
            .expect("HEAD")
            .target()
            .expect("a direct HEAD")
    };
    let before = head();
    assert_eq!(
        run(
            &mut shell,
            "git -c user.useConfigOnly=true commit -q -m no-identity 2>/dev/null"
        )
        .await,
        128
    );
    assert_eq!(head(), before, "no commit was made");
    assert_eq!(staged(&work), ["p"], "the index is as the stage left it");
}

/// `exec` is the one builtin that must not replace the process: the session's records and its
/// unpublished effects live in this process. It runs the program and then ends the shell.
#[tokio::test]
async fn exec_runs_the_program_and_exits_the_shell() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), true).await;
    let params = shell.default_exec_params();

    let result = shell
        .run_string(
            "exec touch made.txt; printf after > not.txt",
            &SourceInfo::from("test"),
            &params,
        )
        .await
        .expect("run the line");

    assert!(work.join("made.txt").exists(), "the program ran");
    assert!(!work.join("not.txt").exists(), "nothing after `exec` runs");
    assert!(
        matches!(result.next_control_flow, ExecutionControlFlow::ExitShell),
        "the shell is asked to exit"
    );
    assert_eq!(u8::from(result.exit_code), 0);
}

/// With no program, `exec` is a redirection statement: its file descriptors become the shell's and
/// outlive the line.
#[tokio::test]
async fn exec_without_a_program_applies_its_redirections() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), true).await;

    assert_eq!(run(&mut shell, "exec 3> fd.txt").await, 0);
    assert_eq!(run(&mut shell, "printf x >&3").await, 0);

    assert_eq!(
        std::fs::read_to_string(work.join("fd.txt")).expect("read"),
        "x"
    );
}

/// `-c` would run the program without the environment the session attributes its effects through,
/// so it is refused rather than approximated.
#[tokio::test]
async fn exec_refuses_an_empty_environment() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), true).await;

    let (code, stderr) = run_capturing(&mut shell, &work, "exec -c true").await;
    assert_eq!(code, 2);
    assert!(
        stderr.contains("exec: -c is not supported"),
        "got {stderr:?}"
    );
}

/// A program that is not on `PATH` is reported as git's — and bash's — 127, not as a panic or a
/// spawner error.
#[tokio::test]
async fn exec_reports_a_missing_program() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), true).await;

    let (code, _) = run_capturing(&mut shell, &work, "exec definitely-not-a-program-xyz").await;
    assert_eq!(code, 127);
}
