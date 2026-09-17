#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! The whole crate through a real, stock brush shell.
//!
//! Every unit test in this crate exercises a piece in isolation: the grammar parses, libgit2 does
//! what the CLI does, the repository search stops where it was told to. What none of them can show
//! is that a `brush_core::Shell` built from the map this crate produces actually dispatches `git`
//! to that builtin. That is what this file is for, and it is why it builds the shell out of stock
//! `brush-builtins` rather than a fixture.
//!
//! One test, because it is one process: git identities are read out of the shell's environment,
//! which this test sets once.

use std::path::{Path, PathBuf};

use brush_builtin::git_builtins;
use brush_builtins::BuiltinSet;
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, ShellVariable, SourceInfo};

/// Builds the shell under test: stock builtins plus this crate's `git`.
async fn shell(working_dir: PathBuf) -> Shell {
    let builtins = {
        let mut builtins = brush_builtins::default_builtins(BuiltinSet::BashMode);
        builtins.extend(git_builtins());
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

    // A commit is only reproducible when both identities and the timestamp are pinned; libgit2
    // reads them from the shell's environment, exactly as the CLI reads them from the process's.
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
    shell
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

/// A repository with a root commit already in it.
///
/// `git commit -- <path>` is a *partial* commit, which the CLI refuses on an unborn branch
/// ("fatal: could not resolve 'HEAD'") and so does this crate. The fixture therefore has to give
/// HEAD something to be, and it does so through libgit2 rather than through the builtin under
/// test.
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

#[tokio::test]
async fn a_stock_shell_runs_the_git_builtin() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let work = scratch.path().canonicalize().expect("canonical work tree");
    init_repository(&work);

    let mut shell = shell(work.clone()).await;

    // `git` is a builtin, so no git process is ever searched for or spawned.
    assert_eq!(run(&mut shell, "printf x > p").await, 0);
    assert_eq!(run(&mut shell, "git add -- p").await, 0);
    assert!(
        git2::Repository::open(&work)
            .expect("open")
            .index()
            .expect("index")
            .get_path(Path::new("p"), 0)
            .is_some(),
        "the staged path is in the index"
    );

    assert_eq!(run(&mut shell, "git commit -m init -- p").await, 0);
    let repository = git2::Repository::open(&work).expect("open");
    let head = repository
        .head()
        .expect("head")
        .peel_to_commit()
        .expect("commit");
    assert_eq!(
        head.message().expect("a UTF-8 message").trim(),
        "init",
        "the commit is the one the builtin made"
    );
    drop(head);
    drop(repository);

    // A subcommand outside GIT_VARIANTS is refused in-process. Falling through to a PATH search
    // would run a real git whose effects nothing records, which is what this refusal buys.
    assert_eq!(run(&mut shell, "git status 2> err.txt").await, 1);
    let refusal = std::fs::read_to_string(work.join("err.txt")).expect("read stderr capture");
    assert!(
        refusal.starts_with("git: status: only these git commands are available as builtins:"),
        "got {refusal:?}"
    );
}
