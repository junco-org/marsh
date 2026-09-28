//! Fixtures and helpers the root integration test crates share.
//!
//! Every file under `tests/` is a crate of its own that declares `mod common;` and uses only the
//! part of this module its tests need, so an item one crate leaves unused is not dead code.
#![allow(
    dead_code,
    reason = "each integration test crate uses its own subset of these helpers"
)]

pub mod rmux;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marsh::shellmux::{CommandCompletion, CommandOptions, RunError};
use marsh::{
    ExecutionResult, OpenFile, SandboxPolicy, Shell, ShellBuilder, ShellError, ShellErrorKind,
    ShellVariable,
};
use marsh_btrfs::fake::CopyTree;
use tempfile::TempDir;

/// How long a wait may take before a test declares the claim it is waiting for unmet.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// A handle one managed line can be handed to, so every crate bounds and judges a line alike.
///
/// Implementors are `Sync` and their attempts `Send`, like any handle a runtime task drives, so
/// the generic helpers' futures are `Send` too.
pub trait Runner: Sync {
    /// What a published line produced.
    type Output;
    /// Why a line did not run or did not publish.
    type Error: std::fmt::Display;
    /// Runs `line` once, without a deadline.
    fn attempt(&self, line: &str)
    -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
    /// The Shell verdict behind `error`, when the line reached one.
    fn verdict(error: &Self::Error) -> Option<&ShellError>;
}

impl Runner for Shell {
    type Output = ExecutionResult;
    type Error = ShellError;
    async fn attempt(&self, line: &str) -> Result<ExecutionResult, ShellError> {
        self.run(line).await
    }
    fn verdict(error: &ShellError) -> Option<&ShellError> {
        Some(error)
    }
}

impl Runner for marsh::shellmux::Shell {
    type Output = Arc<CommandCompletion>;
    type Error = RunError;
    async fn attempt(&self, line: &str) -> Result<Arc<CommandCompletion>, RunError> {
        self.run_command(line, CommandOptions::default()).await
    }
    fn verdict(error: &RunError) -> Option<&ShellError> {
        error.completion()?.result.as_ref().as_ref().err()
    }
}

/// Runs `line` to a published result within [`TIMEOUT`], failing the test otherwise.
pub async fn run<R: Runner>(shell: &R, line: &str) -> R::Output {
    tokio::time::timeout(TIMEOUT, shell.attempt(line))
        .await
        .expect("bounded command")
        .unwrap_or_else(|error| panic!("{line}: {error}"))
}

/// Runs `line`, requiring a policy denial that still retains the native status, and returns it.
pub async fn denied<R: Runner>(shell: &R, line: &str) -> R::Error {
    let error = tokio::time::timeout(TIMEOUT, shell.attempt(line))
        .await
        .expect("bounded command")
        .err()
        .expect("denial");
    let verdict = R::verdict(&error).expect("a Shell verdict");
    assert!(
        matches!(verdict.kind(), ShellErrorKind::Denied { .. }),
        "{verdict}"
    );
    assert!(
        verdict.execution_result().is_some(),
        "native status is retained"
    );
    error
}

/// Runs `line` to a published result and returns its exit code.
pub async fn status(shell: &Shell, line: &str) -> u8 {
    run(shell, line).await.exit_code.into()
}

/// A canonicalized `source` directory under a fresh scratch root, which is kept alive by the
/// returned guard. Canonical, because the git builtin compares it against the repository root.
pub fn scratch() -> (TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("scratch directory");
    let source = root
        .path()
        .canonicalize()
        .expect("canonical scratch")
        .join("source");
    std::fs::create_dir(&source).expect("source directory");
    (root, source)
}

/// A [`scratch`] source holding one file, registered with a test filesystem its shells share.
pub struct Seed {
    /// The scratch root; dropping it deletes the tree.
    pub root: TempDir,
    /// The canonical source directory every shell starts in.
    pub source: PathBuf,
    /// The filesystem the source is registered with.
    pub fs: Arc<CopyTree>,
}

impl Seed {
    /// Creates the source with `contents` at `file` and registers it.
    pub fn new(file: &str, contents: &str) -> Self {
        let (root, source) = scratch();
        let file = source.join(file);
        std::fs::create_dir_all(file.parent().expect("a seeded file has a parent"))
            .expect("seed directory");
        std::fs::write(file, contents).expect("seed file");
        let fs = Arc::new(CopyTree::new());
        fs.register(&source);
        Self { root, source, fs }
    }

    /// Normal Shell construction over this source, with the default routing policy.
    pub fn builder(&self) -> ShellBuilder {
        marsh_core::test_support::shell_builder(self.fs.clone()).working_dir(self.source.clone())
    }

    /// Shell construction over this source whose every command takes the managed route.
    pub fn managed_builder(&self) -> ShellBuilder {
        self.builder().sandbox_policy(SandboxPolicy::allow())
    }

    /// A managed shell over this source.
    pub async fn shell(&self) -> Shell {
        self.managed_builder().build().await.expect("build shell")
    }

    /// A path beside the source, outside every snapshot of it.
    pub fn outside(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// The bytes at `name` in the source.
    pub fn bytes(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.source.join(name)).expect("read seed file")
    }

    /// Commits the whole source as the first commit on `main`, through the system git.
    pub fn git(&self) {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "-A"],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        ] {
            let status = std::process::Command::new("/bin/git")
                .current_dir(&self.source)
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?}");
        }
    }
}

/// A shell whose stdin a test writes and whose merged stdout and stderr it reads.
pub struct Controlled {
    pub shell: Arc<Shell>,
    control: std::io::PipeWriter,
    output: Option<std::io::BufReader<std::io::PipeReader>>,
}

/// Builds `builder`'s shell with its standard descriptors on this test's pipes.
pub async fn controlled(builder: ShellBuilder) -> Controlled {
    let (input, control) = std::io::pipe().unwrap();
    let (output, writer) = std::io::pipe().unwrap();
    let stdout = OpenFile::from(std::fs::File::from(OwnedFd::from(writer)));
    let fds = HashMap::from([
        (0, OpenFile::from(std::fs::File::from(OwnedFd::from(input)))),
        (1, stdout.clone()),
        (2, stdout),
    ]);
    Controlled {
        shell: Arc::new(builder.fds(fds).build().await.unwrap()),
        control,
        output: Some(std::io::BufReader::new(output)),
    }
}

impl Controlled {
    /// Reads output up to and including a `READY` line, returning everything read.
    pub async fn ready(&mut self) -> Vec<u8> {
        let mut output = self.output.take().unwrap();
        let (output, observed) = tokio::time::timeout(
            TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let mut observed = Vec::new();
                loop {
                    let mut line = Vec::new();
                    assert_ne!(
                        output.read_until(b'\n', &mut line).unwrap(),
                        0,
                        "producer exited before READY"
                    );
                    observed.extend_from_slice(&line);
                    if line == b"READY\n" {
                        return (output, observed);
                    }
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        self.output = Some(output);
        observed
    }

    /// Writes one line to the shell's stdin.
    pub fn release(&mut self) {
        self.control.write_all(b"continue\n").unwrap();
    }
}

/// Runs `line` on its own task, so the test can act while it is running.
pub fn launch(
    shell: &Arc<Shell>,
    line: String,
) -> tokio::task::JoinHandle<Result<ExecutionResult, ShellError>> {
    let shell = Arc::clone(shell);
    tokio::spawn(async move { shell.run(&line).await })
}

/// The verdict of a [`launch`]ed line, which must arrive within [`TIMEOUT`].
pub async fn join(
    task: tokio::task::JoinHandle<Result<ExecutionResult, ShellError>>,
) -> Result<ExecutionResult, ShellError> {
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("owned operation completes")
        .expect("operation task")
}

/// The git identity variables a reproducible commit pins, for the author and the committer alike.
const IDENTITY: [(&str, &str); 3] = [
    ("NAME", "Test"),
    ("EMAIL", "test@example.com"),
    ("DATE", "1112911993 +0000"),
];

/// Every git identity variable's name, with the value a reproducible commit pins it to.
fn identities() -> impl Iterator<Item = (String, &'static str)> {
    ["AUTHOR", "COMMITTER"].into_iter().flat_map(|who| {
        IDENTITY
            .into_iter()
            .map(move |(suffix, value)| (format!("GIT_{who}_{suffix}"), value))
    })
}

/// Builds a normal managed shell in `dir`, registered as the root of its own test filesystem.
///
/// A commit is only reproducible when both identities and the timestamp are pinned, and git reads
/// them from the environment the shell exports; without `pinned` the shell has none of them.
pub async fn git_shell(dir: &Path, pinned: bool) -> Shell {
    let filesystem = Arc::new(CopyTree::new());
    filesystem.register(dir);
    let shell = marsh_core::test_support::shell_builder(filesystem)
        .working_dir(dir.to_path_buf())
        .sandbox_policy(SandboxPolicy::allow())
        .build()
        .await
        .expect("build shell");
    if pinned {
        for (name, value) in identities() {
            export(&shell, &name, value).await;
        }
    } else {
        unset_identity(&shell).await;
    }
    shell
}

/// Unsets every git identity variable in the shell's environment.
pub async fn unset_identity(shell: &Shell) {
    for (name, _) in identities() {
        shell
            .run(&format!("unset {name}"))
            .await
            .expect("unset identity");
    }
}

/// Exports one variable into the shell's environment.
pub async fn export(shell: &Shell, name: &str, value: &str) {
    let mut variable = ShellVariable::new(value);
    variable.export();
    shell.set_var(name, variable).await.expect("set variable");
}

/// A repository whose root commit is empty, built through libgit2 rather than through the builtin.
///
/// `git commit -- <path>` is a *partial* commit, which the CLI refuses on an unborn branch
/// ("fatal: could not resolve 'HEAD'") and so does the builtin. The fixture therefore has to give
/// HEAD something to be.
pub fn init_repository(work: &Path) {
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
