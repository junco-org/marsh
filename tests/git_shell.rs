#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! Managed Git behavior through the ordinary persistent Shell interface and native tracing.

mod common;

use std::path::Path;

use common::{export, git_shell, init_repository, scratch, status, unset_identity};
use marsh::Shell;

/// What `line` printed to its standard output, captured beside the repository rather than in it.
async fn output(shell: &Shell, capture: &Path, line: &str) -> (u8, String) {
    let code = status(shell, &format!("{line} > {}", capture.display())).await;
    let text = std::fs::read_to_string(capture).expect("read the capture");
    (code, text)
}

#[tokio::test]
async fn a_stock_shell_runs_the_git_builtin() {
    let (_guard, work) = scratch();
    std::fs::create_dir_all(work.join("sub/deep")).expect("the work tree");
    init_repository(&work);
    let capture = work.with_file_name("capture.txt");

    let shell = git_shell(&work, true).await;

    assert_eq!(status(&shell, "printf x > p").await, 0);
    assert_eq!(status(&shell, "git add -- p").await, 0);
    assert!(
        git2::Repository::open(&work)
            .expect("open")
            .index()
            .expect("index")
            .get_path(Path::new("p"), 0)
            .is_some(),
        "the staged path is in the index"
    );

    assert_eq!(status(&shell, "git commit -q -m init -- p").await, 0);
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
    status(&shell, "printf 'u\\n' > untracked").await;
    assert_eq!(
        output(&shell, &capture, "git status --porcelain=v1").await,
        (0, "?? untracked\n".to_string())
    );
    assert_eq!(
        output(
            &shell,
            &capture,
            "git -C sub -C deep rev-parse --show-prefix"
        )
        .await,
        (0, "sub/deep/\n".to_string()),
        "-C applies in order, each relative to the last"
    );
    let (code, abbreviated) = output(
        &shell,
        &capture,
        "git --no-pager -c core.abbrev=12 log -1 --format=%h",
    )
    .await;
    assert_eq!(
        (code, abbreviated.trim_end().len()),
        (0, 12),
        "{abbreviated:?}"
    );
    let (code, version) = output(&shell, &capture, "git --version").await;
    assert!(
        code == 0 && version.starts_with("git version "),
        "{version:?}"
    );
    let (code, help) = output(&shell, &capture, "git --help").await;
    assert!(code == 0 && help.contains("usage: git"), "{help:?}");
    assert_eq!(
        status(&shell, "git >/dev/null").await,
        1,
        "a bare git prints its help and fails, as git does"
    );

    // Nothing that needs no identity asks for one.
    unset_identity(&shell).await;
    assert_eq!(status(&shell, "git status > /dev/null").await, 0);
    assert_eq!(status(&shell, "git add -- untracked").await, 0);
    shell.close(false).await.expect("close shell");
}

/// A git run here depends on the repository and the command line, never on who runs it: the
/// host's global and system configuration — an alias, a line-ending conversion — are ignored even
/// when the environment points straight at them, while repository and command-line configuration
/// still apply.
#[tokio::test]
async fn host_git_configuration_does_not_reach_the_builtin() {
    let (_guard, work) = scratch();
    let root = work.parent().expect("the scratch root");
    init_repository(&work);
    let hostile = root.join("hostile.gitconfig");
    std::fs::write(
        &hostile,
        "[alias]\n\tst = status\n[core]\n\tautocrlf = true\n",
    )
    .expect("a hostile configuration");
    std::fs::write(
        root.join(".gitconfig"),
        std::fs::read(&hostile).expect("read"),
    )
    .expect("a hostile home configuration");
    let capture = root.join("capture.txt");

    let shell = git_shell(&work, true).await;
    for (name, value) in [
        ("HOME", root.display().to_string()),
        ("XDG_CONFIG_HOME", root.display().to_string()),
        ("GIT_CONFIG_GLOBAL", hostile.display().to_string()),
        ("GIT_CONFIG_SYSTEM", hostile.display().to_string()),
    ] {
        export(&shell, name, &value).await;
    }

    assert_eq!(
        status(&shell, "git st 2>/dev/null").await,
        1,
        "no host alias"
    );
    assert_eq!(
        output(&shell, &capture, "git config --get core.autocrlf").await,
        (1, String::new()),
        "no host core.autocrlf"
    );
    assert_eq!(
        status(&shell, "GIT_CONFIG_GLOBAL=/dev/stdin git st 2>/dev/null").await,
        1,
        "not even from a command-local assignment"
    );
    assert_eq!(status(&shell, "git config alias.lst status").await, 0);
    assert_eq!(
        status(&shell, "git lst > /dev/null").await,
        0,
        "repository config"
    );
    assert_eq!(
        status(&shell, "git -c alias.cst=status cst > /dev/null").await,
        0,
        "command-line config"
    );
    shell.close(false).await.expect("close shell");
}
