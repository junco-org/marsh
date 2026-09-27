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
//! Every behavior runs through the ordinary managed Shell interface and the real native tracer.

mod common;

use std::path::Path;

use brush_core::ExecutionControlFlow;
use common::{git_shell, init_repository, run, scratch, status};
use marsh::Shell;

/// Runs one line with its stderr captured, returning `(exit code, stderr)`.
async fn run_capturing(shell: &Shell, line: &str) -> (u8, String) {
    let capture = tempfile::NamedTempFile::new().expect("capture");
    let code = status(shell, &format!("{line} 2> {}", capture.path().display())).await;
    let text = std::fs::read_to_string(capture.path()).expect("read diagnostic");
    (code, text)
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
    let shell = git_shell(&work, true).await;
    status(&shell, "printf 'a\\n' > a.txt; printf 'b\\n' > b.txt").await;

    assert_eq!(status(&shell, "git add 2>/dev/null").await, 0);
    assert!(staged(&work).is_empty(), "nothing specified, nothing added");
    assert_eq!(status(&shell, "git add -- '*.txt'").await, 0);
    assert_eq!(
        staged(&work),
        ["a.txt", "b.txt"],
        "the glob is git's pathspec"
    );

    status(&shell, "printf 'changed\\n' > a.txt").await;
    assert_eq!(status(&shell, "git restore -- a.txt").await, 0);
    assert_eq!(
        std::fs::read(work.join("a.txt")).expect("a.txt"),
        b"a\n",
        "the worktree came back from the index"
    );

    status(&shell, "printf 'u\\n' > untracked.txt").await;
    assert_eq!(
        status(&shell, "git clean -- untracked.txt 2>/dev/null").await,
        128,
        "git itself requires -f"
    );
    assert!(work.join("untracked.txt").exists());
    shell.close(false).await.expect("close shell");
}

#[tokio::test]
async fn a_working_directory_with_no_repository_above_it_is_fatal() {
    let (_guard, work) = scratch();
    let shell = git_shell(&work, true).await;

    let (code, reported) = run_capturing(&shell, "git add -- p").await;
    assert_eq!(code, 128, "{reported}");
    shell.close(false).await.expect("close shell");
}

/// Repository discovery is always bounded by the source selected during construction.
#[tokio::test]
async fn the_snapshot_root_stops_the_ancestor_search() {
    let (_guard, outer) = scratch();
    init_repository(&outer);
    let inner = outer.join("tree/deep");
    std::fs::create_dir_all(&inner).expect("dirs");
    std::fs::write(inner.join("p"), b"p\n").expect("p");
    let shell = git_shell(&inner, true).await;
    assert_eq!(status(&shell, "git add -- p 2>/dev/null").await, 128);
    assert!(staged(&outer).is_empty());
    shell.close(false).await.expect("close shell");
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
    let shell = git_shell(&tree, true).await;

    for line in [
        format!("git -C {} add -- p", outer.display()),
        format!(
            "git --git-dir={}/.git --work-tree={} add -- p",
            outer.display(),
            outer.display()
        ),
        format!("GIT_DIR={}/.git git status", outer.display()),
        format!("git clone {} ../escape", outer.display()),
        format!(
            "git clone --separate-git-dir ../meta {} inside",
            outer.display()
        ),
        "git init ../escape".to_string(),
        "git init --separate-git-dir=../meta inside".to_string(),
    ] {
        assert_eq!(
            status(&shell, &format!("{line} 2>/dev/null")).await,
            128,
            "{line}"
        );
    }
    assert!(staged(&outer).is_empty(), "nothing was staged outside");
    for created in ["escape", "meta", "tree/inside"] {
        assert!(!outer.join(created).exists(), "{created} was never created");
    }

    assert_eq!(
        status(&shell, "git init -q inside").await,
        0,
        "inside is allowed"
    );
    assert!(tree.join("inside/.git").is_dir());
    shell.close(false).await.expect("close shell");
}

/// A git identity is git's business: inspecting and staging need none, and a commit without one
/// fails in git itself — leaving `HEAD` and the index as they were — rather than in the builtin.
#[tokio::test]
async fn only_git_decides_what_needs_an_identity() {
    let (_guard, work) = scratch();
    init_repository(&work);
    std::fs::write(work.join("p"), b"p\n").expect("p");
    let shell = git_shell(&work, false).await;
    status(&shell, "unset EMAIL GIT_EDITOR; printf 'p\\n' > p").await;

    assert_eq!(status(&shell, "git status > /dev/null").await, 0);
    assert_eq!(status(&shell, "git add -- p").await, 0);
    let head = || {
        git2::Repository::open(&work)
            .expect("open")
            .head()
            .expect("HEAD")
            .target()
            .expect("a direct HEAD")
    };
    let before = head();
    let result = shell
        .run("git -c user.useConfigOnly=true commit -q -m no-identity 2>/dev/null")
        .await;
    let code = match &result {
        Ok(result) => u8::from(result.exit_code),
        Err(error) => u8::from(
            error
                .execution_result()
                .expect("native status retained")
                .exit_code,
        ),
    };
    assert_eq!(code, 128);
    assert_eq!(head(), before, "no commit was made");
    assert_eq!(staged(&work), ["p"], "the index is as the stage left it");
    shell.close(false).await.expect("close shell");
}

/// `exec` is the one builtin that must not replace the process: the session's records and its
/// unpublished effects live in this process. It runs the program and then ends the shell.
#[tokio::test]
async fn exec_runs_the_program_and_exits_the_shell() {
    let (_guard, work) = scratch();
    let shell = git_shell(&work, true).await;

    let result = run(&shell, "exec touch made.txt; printf after > not.txt").await;

    assert!(work.join("made.txt").exists(), "the program ran");
    assert!(!work.join("not.txt").exists(), "nothing after `exec` runs");
    assert!(
        matches!(result.next_control_flow, ExecutionControlFlow::ExitShell),
        "the shell is asked to exit"
    );
    assert_eq!(u8::from(result.exit_code), 0);
    assert!(shell.is_closed());
    shell.close(false).await.expect("close shell");
}

/// With no program, `exec` is a redirection statement: its file descriptors become the shell's and
/// outlive the line.
#[tokio::test]
async fn exec_without_a_program_applies_its_redirections() {
    let (_guard, work) = scratch();
    let shell = git_shell(&work, true).await;

    assert_eq!(status(&shell, "exec 3> fd.txt").await, 0);
    assert_eq!(status(&shell, "printf x >&3").await, 0);

    assert_eq!(
        std::fs::read_to_string(work.join("fd.txt")).expect("read"),
        "x"
    );
    shell.close(false).await.expect("close shell");
}

/// `-c` would run the program without the environment the session attributes its effects through,
/// so it is refused rather than approximated.
#[tokio::test]
async fn exec_refuses_an_empty_environment() {
    let (_guard, work) = scratch();
    let shell = git_shell(&work, true).await;

    let (code, _) = run_capturing(&shell, "exec -c true").await;
    assert_eq!(code, 2);
    shell.close(false).await.expect("close shell");
}

/// A program that is not on `PATH` is reported as git's — and bash's — 127, not as a panic or a
/// spawner error.
#[tokio::test]
async fn exec_reports_a_missing_program() {
    let (_guard, work) = scratch();
    let shell = git_shell(&work, true).await;

    let (code, _) = run_capturing(&shell, "exec definitely-not-a-program-xyz").await;
    assert_eq!(code, 127);
    shell.close(false).await.expect("close shell");
}
