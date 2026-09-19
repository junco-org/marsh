//! The `git` builtin: one `git` registration that performs the supported command variants
//! in-process through libgit2. Registering the name is what makes the no-fork guarantee hold: a
//! builtin is found before any PATH search, so no `git` process is ever spawned, and every git
//! effect happens on the shell's own thread where it can be attributed. Command lines outside
//! [`GIT_VARIANTS`] are refused rather than forwarded — the deliberate price of that guarantee.
#![allow(
    clippy::unused_async_trait_impl,
    reason = "builtins implement a trait whose `execute` is async by contract"
)]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use brush_core::builtins::{self, Registration};
use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions};

use super::{gitcmd, gitexec};

/// Environment variable naming the tree a command belongs to.
///
/// Its value is the boundary a git builtin may not search for a repository past. A shell built
/// without it searches ancestors as git itself would.
pub const SNAPSHOT_ROOT_VAR: &str = "MARSH_SNAPSHOT_ROOT";

/// Exit code for a git command line that cannot be expressed as one of [`GIT_VARIANTS`].
const UNMAPPABLE: u8 = 2;
/// Exit code git uses for `fatal:` conditions.
const FATAL: u8 = 128;

/// The git subcommands the `git` builtin performs. Anything else is refused.
pub const GIT_VARIANTS: [&str; 10] = [
    "add", "stage", "rm", "commit", "restore", "checkout", "stash", "clean", "diff", "log",
];

/// The git registration: one `git` builtin, which dispatches on its own `argv[1]`.
///
/// Extend a builtin map with it — `map.extend(git_builtins())` — before handing the
/// map to `Shell::builder().builtins`.
#[must_use]
pub fn git_builtins<SE: ShellExtensions>() -> HashMap<String, Registration<SE>> {
    HashMap::from([("git".to_string(), builtins::builtin::<GitBuiltin, SE>())])
}

/// The `git` builtin. Argv is kept verbatim and parsed by [`gitcmd`]; clap is bypassed.
#[derive(clap::Parser)]
struct GitBuiltin {
    /// The command's own argument vector, `argv[0]` included.
    #[clap(allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}

impl builtins::Command for GitBuiltin {
    type Error = brush_core::Error;

    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        Ok(Self {
            args: args.into_iter().collect(),
        })
    }

    async fn execute<SE: ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // Dispatch before parsing: a subcommand this builtin does not perform has to be refused as
        // itself, not reported as a grammar failure.
        if let Some(subcommand) = self.args.get(1)
            && !GIT_VARIANTS.contains(&subcommand.as_str())
        {
            writeln!(
                context.stderr(),
                "git: {subcommand}: only these git commands are available as builtins: {}",
                GIT_VARIANTS.join(", ")
            )?;
            return Ok(ExecutionResult::general_error());
        }

        let invocation = match gitcmd::parse(&self.args) {
            Ok(invocation) => invocation,
            Err(reason) => {
                writeln!(context.stderr(), "git: {reason}")?;
                return Ok(ExecutionResult::new(UNMAPPABLE));
            }
        };

        let cwd = context.shell.working_dir().to_path_buf();
        let boundary = context
            .shell
            .env_str(SNAPSHOT_ROOT_VAR)
            .map_or_else(|| PathBuf::from("/"), |value| PathBuf::from(&*value));
        let Some(repo_root) = repo_root(&cwd, &boundary) else {
            writeln!(
                context.stderr(),
                "fatal: not a git repository (or any of the parent directories): .git"
            )?;
            return Ok(ExecutionResult::new(FATAL));
        };

        let mut resolved = Vec::with_capacity(invocation.pathspecs.len());
        for pathspec in &invocation.pathspecs {
            let absolute = gitcmd::resolve(&cwd, pathspec);
            let Some(relative) = repo_relative(&repo_root, &absolute) else {
                writeln!(
                    context.stderr(),
                    "git: pathspec {pathspec:?} is outside the repository or inside .git/"
                )?;
                return Ok(ExecutionResult::new(UNMAPPABLE));
            };
            resolved.push(relative);
        }

        let identities = ["AUTHOR", "COMMITTER"].map(|who| {
            gitexec::identity_from_env(
                |name| context.shell.env_str(name).map(|value| value.into_owned()),
                who,
            )
        });
        let [author, committer] = match identities {
            [Ok(author), Ok(committer)] => [author, committer],
            [Err(reason), _] | [_, Err(reason)] => {
                writeln!(context.stderr(), "fatal: {reason}")?;
                return Ok(ExecutionResult::new(FATAL));
            }
        };

        let invocation = gitcmd::GitInvocation {
            action: invocation.action,
            pathspecs: resolved,
        };
        let code = gitexec::run(
            &invocation,
            &repo_root,
            &author,
            &committer,
            &mut context.stdout(),
            &mut context.stderr(),
        );
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "git exit codes are 0..=128"
        )]
        Ok(ExecutionResult::new(code as u8))
    }
}

/// The nearest ancestor of `start` (inclusive) that contains a `.git` entry, never searching above
/// `boundary`.
///
/// The bound keeps a git command from climbing out of the tree it was given and opening a
/// repository beside it. That tree carries a repository only when the caller's does; it may hold
/// none, one, or many, at any depth.
#[must_use]
pub fn repo_root(start: &Path, boundary: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(directory) = current {
        if !directory.starts_with(boundary) {
            return None;
        }
        if directory.join(".git").exists() {
            return Some(directory.to_path_buf());
        }
        current = directory.parent();
    }
    None
}

/// The repository-relative, `/`-joined form of `absolute`, or `None` when it is not a resource of
/// this repository: outside the worktree, the worktree root itself, or inside `.git/`.
///
/// A `String` rather than a path because this is a libgit2 pathspec, which is a `/`-joined name in
/// the index, not a name the filesystem resolves.
fn repo_relative(repo_root: &Path, absolute: &Path) -> Option<String> {
    let segments = gitcmd::relative_segments(repo_root, absolute)?;
    if segments.is_empty() || segments[0] == ".git" {
        return None;
    }
    Some(segments.join("/"))
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_relative_names_resources_and_nothing_else() {
        let root = Path::new("/work");
        assert_eq!(
            repo_relative(root, Path::new("/work/src/a.txt")),
            Some("src/a.txt".to_string())
        );
        for outside in [
            "/work",
            "/work/.git",
            "/work/.git/index",
            "/tmp/escape",
            "/",
        ] {
            assert_eq!(
                repo_relative(root, Path::new(outside)),
                None,
                "{outside} is not a resource of the repository"
            );
        }
    }

    #[test]
    fn the_repository_root_is_found_from_a_subdirectory() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let root = directory.path();
        std::fs::create_dir_all(root.join("src/deep")).expect("dirs");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        assert_eq!(
            repo_root(&root.join("src/deep"), root).as_deref(),
            Some(root)
        );
        assert_eq!(
            repo_root(root, root).as_deref(),
            Some(root),
            "the root itself is its own repository root"
        );
    }

    /// The boundary is the tree the command was given. Without it, a git command in a tree whose
    /// origin has no repository would climb into whatever directory holds it and open one outside
    /// every such tree.
    #[test]
    fn the_search_stops_at_the_snapshot_root() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let root = directory.path();
        std::fs::create_dir_all(root.join("run/foo1")).expect("dirs");
        std::fs::create_dir_all(root.join(".git")).expect("an outer repository");
        assert_eq!(repo_root(&root.join("run/foo1"), &root.join("run")), None);
    }

    /// With no boundary to stop it, the search still has to end: a tree whose ancestors carry no
    /// repository yields none rather than looping or opening the filesystem root.
    #[test]
    fn an_exhausted_search_names_no_repository() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let start = directory.path().join("a/b");
        std::fs::create_dir_all(&start).expect("dirs");
        assert_eq!(repo_root(&start, Path::new("")), None);
    }
}
