#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! Everything the `git` builtin refuses before it ever opens a repository, and what `exec` does
//! in a shell that has to stay alive.
//!
//! These are the paths a unit test cannot reach: each one is decided from shell state — the
//! working directory, the environment, the snapshot boundary — so the only honest way to exercise
//! them is through a real `brush_core::Shell` that has the builtin registered. What each test pins
//! is the pair a script actually observes: the exit code and the line on stderr.

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
/// `omit` names an identity variable that is deliberately left unset, so a test can show what the
/// builtin does when the environment is incomplete.
async fn shell(working_dir: PathBuf, omit: Option<&str>) -> Shell {
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
            if omit == Some(name.as_str()) {
                continue;
            }
            export(&mut shell, &name, value);
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

/// A command line the grammar cannot express is reported as the grammar's own sentence, not as a
/// git failure: nothing was attempted, so nothing about the repository is being claimed.
#[tokio::test]
async fn an_unmappable_command_line_is_refused_with_its_reason() {
    let (_guard, work) = scratch();
    init_repository(&work);
    let mut shell = shell(work.clone(), None).await;

    for (line, reason) in [
        ("git add", "git add with an empty pathspec list"),
        (
            "git add -- 'src/*.txt'",
            "git pathspec pattern \"src/*.txt\" is not a resource",
        ),
        (
            "git restore -- p",
            "git restore without --staged is not a capability; use git checkout HEAD -- <path>",
        ),
        ("git clean -- p", "git clean requires -f"),
    ] {
        assert_eq!(
            run_capturing(&mut shell, &work, line).await,
            (2, format!("git: {reason}\n")),
            "{line}"
        );
    }
}

#[tokio::test]
async fn a_working_directory_with_no_repository_above_it_is_fatal() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), None).await;

    assert_eq!(
        run_capturing(&mut shell, &work, "git add -- p").await,
        (
            128,
            "fatal: not a git repository (or any of the parent directories): .git\n".to_string()
        )
    );
}

/// The builtin searches ancestors as git does, but never above the tree it was given: a repository
/// beside that tree is not the command's repository.
#[tokio::test]
async fn the_snapshot_root_stops_the_ancestor_search() {
    let (_guard, outer) = scratch();
    init_repository(&outer);
    let inner = outer.join("tree/deep");
    std::fs::create_dir_all(&inner).expect("dirs");

    let mut shell = shell(inner.clone(), None).await;
    let (code, reported) = run_capturing(&mut shell, &inner, "git add -- p").await;
    assert_eq!(
        (code, reported.as_str()),
        (
            128,
            "fatal: pathspec 'tree/deep/p' did not match any files\n"
        ),
        "without a boundary the outer repository is found, and the pathspec is named relative to \
         its worktree"
    );

    export(
        &mut shell,
        SNAPSHOT_ROOT_VAR,
        &outer.join("tree").display().to_string(),
    );
    assert_eq!(
        run_capturing(&mut shell, &inner, "git add -- p").await,
        (
            128,
            "fatal: not a git repository (or any of the parent directories): .git\n".to_string()
        )
    );
}

/// A pathspec has to name a resource *of this repository*. The worktree root, anything above it
/// and anything inside `.git/` name something else, and are refused before libgit2 sees them.
#[tokio::test]
async fn a_pathspec_that_is_not_a_repository_resource_is_refused() {
    let (_guard, work) = scratch();
    init_repository(&work);
    let mut shell = shell(work.clone(), None).await;

    for pathspec in ["../outside.txt", ".git/config", "."] {
        assert_eq!(
            run_capturing(&mut shell, &work, &format!("git add -- {pathspec}")).await,
            (
                2,
                format!("git: pathspec {pathspec:?} is outside the repository or inside .git/\n")
            ),
            "{pathspec}"
        );
    }
}

/// A commit whose timestamp or identity came from anywhere but the environment would not be
/// reproducible, so an incomplete environment is fatal rather than filled in.
#[tokio::test]
async fn an_incomplete_git_identity_is_fatal() {
    let (_guard, work) = scratch();
    init_repository(&work);
    let mut shell = shell(work.clone(), Some("GIT_COMMITTER_DATE")).await;

    assert_eq!(
        run_capturing(&mut shell, &work, "git add -- p").await,
        (128, "fatal: GIT_COMMITTER_DATE is not set\n".to_string())
    );
}

/// `exec` is the one builtin that must not replace the process: the session's records and its
/// unpublished effects live in this process. It runs the program and then ends the shell.
#[tokio::test]
async fn exec_runs_the_program_and_exits_the_shell() {
    let (_guard, work) = scratch();
    let mut shell = shell(work.clone(), None).await;
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
    let mut shell = shell(work.clone(), None).await;

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
    let mut shell = shell(work.clone(), None).await;

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
    let mut shell = shell(work.clone(), None).await;

    let (code, _) = run_capturing(&mut shell, &work, "exec definitely-not-a-program-xyz").await;
    assert_eq!(code, 127);
}
