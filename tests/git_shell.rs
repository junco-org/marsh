#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! The whole crate through a real, stock brush shell.
//!
//! What no unit test can show is that a `brush_core::Shell` built from the map this crate
//! produces actually dispatches `git` to the builtin, and that the builtin then behaves as the
//! system git does — global options, help and version, a command that needs no identity — with
//! nothing of the host's git configuration reaching it. That is what this file is for, and it is
//! why it builds the shell out of stock `brush-builtins` rather than a fixture.

use std::path::{Path, PathBuf};

use brush_builtins::BuiltinSet;
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, ShellVariable, SourceInfo};
use marsh::builtins::git_builtins;

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

    // A commit is only reproducible when both identities and the timestamp are pinned; git reads
    // them from the environment the shell exports.
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

/// What `line` printed to its standard output, captured beside the repository rather than in it.
async fn output(shell: &mut Shell, capture: &Path, line: &str) -> (u8, String) {
    let code = run(shell, &format!("{line} > {}", capture.display())).await;
    let text = std::fs::read_to_string(capture).expect("read the capture");
    (code, text)
}

#[tokio::test]
async fn a_stock_shell_runs_the_git_builtin() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let root = scratch.path().canonicalize().expect("canonical scratch");
    let work = root.join("work");
    std::fs::create_dir_all(work.join("sub/deep")).expect("the work tree");
    init_repository(&work);
    let capture = root.join("capture.txt");

    let mut shell = shell(work.clone()).await;

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

    assert_eq!(run(&mut shell, "git commit -q -m init -- p").await, 0);
    let repository = git2::Repository::open(&work).expect("open");
    let head = repository
        .head()
        .expect("head")
        .peel_to_commit()
        .expect("commit");
    assert_eq!(head.message().expect("a UTF-8 message"), "init\n");
    drop(head);
    drop(repository);

    // Every subcommand is git's own, and so is every global option in front of it.
    std::fs::write(work.join("untracked"), b"u\n").expect("an untracked file");
    assert_eq!(
        output(&mut shell, &capture, "git status --porcelain=v1").await,
        (0, "?? untracked\n".to_string())
    );
    assert_eq!(
        output(&mut shell, &capture, "git -C sub -C deep rev-parse --show-prefix").await,
        (0, "sub/deep/\n".to_string()),
        "-C applies in order, each relative to the last"
    );
    let (code, abbreviated) = output(
        &mut shell,
        &capture,
        "git --no-pager -c core.abbrev=12 log -1 --format=%h",
    )
    .await;
    assert_eq!((code, abbreviated.trim_end().len()), (0, 12), "{abbreviated:?}");
    let (code, version) = output(&mut shell, &capture, "git --version").await;
    assert!(code == 0 && version.starts_with("git version "), "{version:?}");
    let (code, help) = output(&mut shell, &capture, "git --help").await;
    assert!(code == 0 && help.contains("usage: git"), "{help:?}");
    assert_eq!(
        run(&mut shell, "git >/dev/null").await,
        1,
        "a bare git prints its help and fails, as git does"
    );

    // Nothing that needs no identity asks for one.
    for who in ["AUTHOR", "COMMITTER"] {
        for what in ["NAME", "EMAIL", "DATE"] {
            shell
                .env_mut()
                .unset(&format!("GIT_{who}_{what}"))
                .expect("unset an identity");
        }
    }
    assert_eq!(run(&mut shell, "git status > /dev/null").await, 0);
    assert_eq!(run(&mut shell, "git add -- untracked").await, 0);
}

/// A git run here depends on the repository and the command line, never on who runs it: the
/// host's global and system configuration — an alias, a line-ending conversion — are ignored even
/// when the environment points straight at them, while repository and command-line configuration
/// still apply.
#[tokio::test]
async fn host_git_configuration_does_not_reach_the_builtin() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let root = scratch.path().canonicalize().expect("canonical scratch");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("the work tree");
    init_repository(&work);
    let hostile = root.join("hostile.gitconfig");
    std::fs::write(&hostile, "[alias]\n\tst = status\n[core]\n\tautocrlf = true\n")
        .expect("a hostile configuration");
    std::fs::write(root.join(".gitconfig"), std::fs::read(&hostile).expect("read"))
        .expect("a hostile home configuration");
    let capture = root.join("capture.txt");

    let mut shell = shell(work.clone()).await;
    for (name, value) in [
        ("HOME", root.display().to_string()),
        ("XDG_CONFIG_HOME", root.display().to_string()),
        ("GIT_CONFIG_GLOBAL", hostile.display().to_string()),
        ("GIT_CONFIG_SYSTEM", hostile.display().to_string()),
    ] {
        let mut variable = ShellVariable::new(value);
        variable.export();
        shell
            .env_mut()
            .set_global(name, variable)
            .expect("export a variable");
    }

    assert_eq!(run(&mut shell, "git st 2>/dev/null").await, 1, "no host alias");
    assert_eq!(
        output(&mut shell, &capture, "git config --get core.autocrlf").await,
        (1, String::new()),
        "no host core.autocrlf"
    );
    assert_eq!(
        run(&mut shell, "GIT_CONFIG_GLOBAL=/dev/stdin git st 2>/dev/null").await,
        1,
        "not even from a command-local assignment"
    );
    assert_eq!(run(&mut shell, "git config alias.lst status").await, 0);
    assert_eq!(run(&mut shell, "git lst > /dev/null").await, 0, "repository config");
    assert_eq!(
        run(&mut shell, "git -c alias.cst=status cst > /dev/null").await,
        0,
        "command-line config"
    );
}
