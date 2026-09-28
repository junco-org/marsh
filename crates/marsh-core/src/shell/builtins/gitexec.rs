//! The system git, run through the shell's own external-command path, and what it did.
//!
//! [`run`] forwards a command line to the `git` found on the shell's `PATH` exactly as typed —
//! same arguments, working directory, exported environment and descriptors — through brush's
//! [`SimpleCommand`], so the process is started by the shell's spawner, recorded like any other,
//! and traced like any other. Three things are added around it, all command-local: configuration
//! outside the repository is switched off, repository discovery stops at the snapshot, and a
//! repository or destination outside the snapshot is refused before anything is started.
//!
//! In a snapshot-attached shell the run is also *observed*. An inspection — whose classification
//! the policy says only reads — is only checked afterwards for having changed repository state.
//! Anything else runs alone in its snapshot and is bracketed by probes: read-only native git
//! queries of the index, `HEAD` and status before and after it. The difference, together with the
//! traced writes inside the run's window, is recorded as the git actions it performed on each
//! path ([`GitEffectRecord`]). Those records, not the command line, are what the line's boundary
//! later maps onto capability requests. A fact the attribution needs that cannot be read latches a
//! failure instead, and the line is never published.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use brush_core::commands::{CommandArg, ShellForCommand, SimpleCommand};
use brush_core::env::EnvironmentScope;
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::processes::{ChildProcess, ProcessWaitResult};
use brush_core::results::ExecutionWaitResult;
use brush_core::{
    ExecutionContext, ExecutionParameters, ExecutionResult, ProcessGroupPolicy, Shell,
    ShellExtensions, ShellVariable,
};

use super::gitcmd::{self, GitAction, GitInvocation};
use crate::shell::execution::brush_error;
use crate::shell::policy::capability_of;
use crate::shell::snapshot::{GitCohortKind, GitEffectRecord, Snapshot};

/// Exit code of a command the shell cannot find.
const NOT_FOUND: u8 = 127;
/// Exit code git uses for `fatal:` conditions, and this builtin for its own refusals.
const FATAL: u8 = 128;

/// Configuration outside the repository, which no git run here reads: a host `core.autocrlf` or
/// alias would make a command's effect depend on who ran it rather than on the seed.
const ISOLATION: [(&str, &str); 3] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
];

/// Options every probe adds after the command line's own global options: no pager, no lock taken
/// for a stat refresh, no object fetched on demand, no filesystem monitor started.
const PROBE_OPTIONS: [&str; 5] = [
    "--no-pager",
    "--no-optional-locks",
    "--no-lazy-fetch",
    "-c",
    "core.fsmonitor=false",
];

/// Runs `argv` through the system git, observing it when `snapshot` is the tree it runs in.
///
/// # Errors
///
/// Fails when the shell cannot compose or wait for the command, or cannot write a diagnostic.
/// Git's own failures are its exit status, never an error here.
#[allow(
    clippy::too_many_lines,
    reason = "one invocation's containment, admission, run and attribution are a single ordered \
              sequence; splitting it would scatter the guard obligations it carries"
)]
pub(crate) async fn run<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    argv: Vec<String>,
    snapshot: Arc<Snapshot>,
) -> Result<ExecutionResult, brush_core::Error> {
    let Some(git) = context
        .shell
        .find_first_executable_in_path("git")
        .and_then(|path| path.to_str().map(str::to_string))
    else {
        writeln!(context.stderr(), "git: command not found")?;
        return Ok(ExecutionResult::new(NOT_FOUND));
    };
    let invocation = gitcmd::parse(&argv);
    let kind = match invocation.action.clone().map(capability_of) {
        Some(action) if action.is_read() && !action.is_write() => GitCohortKind::Inspect,
        _ => GitCohortKind::Exclusive,
    };
    let bound = snapshot.path().to_path_buf();

    // One clone per invocation carries every overlay, so nothing set here can leak into the
    // caller's shell, whatever becomes of this future.
    let mut shell = context.shell.clone();
    isolate(&mut shell, &bound, kind)?;
    let cwd = effective_cwd(shell.working_dir(), invocation.global_args);
    let mut runner = Runner {
        shell: &mut shell,
        params: &context.params,
        git: &git,
        globals: invocation.global_args,
        target: None,
    };

    let admitted = match runner.contain(&invocation, &cwd, &bound).await {
        Ok(layout) => snapshot.begin_git(kind).map(|guard| (layout, guard)),
        Err(refusal) => Err(format!("fatal: {refusal}")),
    };
    let (layout, guard) = match admitted {
        Ok(admitted) => admitted,
        Err(refusal) => {
            writeln!(context.stderr(), "{refusal}")?;
            return Ok(ExecutionResult::new(FATAL));
        }
    };

    let before = match kind {
        GitCohortKind::Inspect => None,
        GitCohortKind::Exclusive => match runner.observe(&invocation, layout, true).await {
            Ok(before) => Some(before),
            Err(failure) => {
                guard.fail(failure);
                None
            }
        },
    };
    let (result, stopped) = runner.execute(&argv, context.process_group_id).await?;
    if stopped {
        guard.fail("git: a managed git was stopped before it completed".to_string());
    }

    let trace_snapshot = Arc::clone(&snapshot);
    let run = snapshot.active().map_err(brush_error)?;
    let drain = {
        let internal = run.tracing.internal_scope()?;
        let _guard = internal.enter();
        run.runtime
            .spawn_blocking(move || trace_snapshot.drain_trace())
    };
    let window = match drain.await {
        Ok(Ok(())) => snapshot.writes_for(guard.invocation),
        failure => {
            guard.fail(format!("git: evidence drain failed: {failure:?}"));
            return Ok(result);
        }
    };
    if !result.is_success() && !window.is_empty() {
        guard.fail("failed git invocation changed protected state".into());
        return Ok(result);
    }
    let invocation_id = guard.invocation;
    match (kind, before) {
        (GitCohortKind::Inspect, _) => {
            if window.iter().any(|path| changes_repository_state(path)) {
                guard.fail("git: inspection changed repository state".to_string());
            }
            guard.complete();
        }
        (GitCohortKind::Exclusive, None) => {}
        (GitCohortKind::Exclusive, Some(before)) => {
            match runner
                .attribute(&invocation, &before, &window, &bound)
                .await
            {
                Ok((requests, metadata)) => guard.record(GitEffectRecord {
                    invocation: invocation_id,
                    started_order: 0,
                    finished_order: 0,
                    requests,
                    metadata,
                }),
                Err(failure) => guard.fail(failure),
            }
        }
    }
    Ok(result)
}

/// Adds the invocation's environment overlays to its own shell, in a command scope above
/// everything the caller set.
///
/// Discovery stops at the snapshot's parent, so a repository at the snapshot root is still found
/// from below it and nothing above it ever is. Automatic maintenance runs in the foreground, so
/// what it rewrites is inside the run's window rather than done by a detached process nobody
/// observes, after the boundary that judged the line. An inspection takes no optional lock, so it
/// cannot rewrite the index while only reading it.
fn isolate<SE: ShellExtensions>(
    shell: &mut Shell<SE>,
    bound: &Path,
    kind: GitCohortKind,
) -> Result<(), brush_core::Error> {
    let mut overlays: Vec<(String, String)> = ISOLATION
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    if let Some(ceiling) = bound.parent() {
        let ceiling = ceiling.to_string_lossy();
        let value = match shell.env_str("GIT_CEILING_DIRECTORIES") {
            Some(existing) if !existing.is_empty() => format!("{ceiling}:{existing}"),
            _ => ceiling.into_owned(),
        };
        overlays.push(("GIT_CEILING_DIRECTORIES".to_string(), value));
        // Appended after whatever configuration the caller passed the same way.
        let base: usize = shell
            .env_str("GIT_CONFIG_COUNT")
            .and_then(|count| count.parse().ok())
            .unwrap_or(0);
        for (offset, key) in ["maintenance.autoDetach", "gc.autoDetach"]
            .into_iter()
            .enumerate()
        {
            overlays.push((format!("GIT_CONFIG_KEY_{}", base + offset), key.to_string()));
            overlays.push((
                format!("GIT_CONFIG_VALUE_{}", base + offset),
                "false".to_string(),
            ));
        }
        overlays.push(("GIT_CONFIG_COUNT".to_string(), (base + 2).to_string()));
    }
    if kind == GitCohortKind::Inspect {
        overlays.push(("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()));
    }
    let environment = shell.env_mut();
    environment.push_scope(EnvironmentScope::Command);
    for (name, value) in overlays {
        let mut variable = ShellVariable::new(value);
        variable.export();
        environment.add(name, variable, EnvironmentScope::Command)?;
    }
    Ok(())
}

/// The directory git runs in once the command line's `-C` options have been applied, in order.
fn effective_cwd<S: AsRef<str>>(start: &Path, globals: &[S]) -> PathBuf {
    gitcmd::global_options(globals).fold(start.to_path_buf(), |cwd, option| match option {
        ("-C", Some(directory)) => gitcmd::resolve(&cwd, directory),
        _ => cwd,
    })
}

/// The value of a global option that takes a path, from either spelling; the last one wins.
fn global_path<'a, S: AsRef<str>>(globals: &'a [S], name: &str) -> Option<&'a str> {
    gitcmd::global_options(globals).fold(None, |found, (word, value)| {
        if word == name {
            value.or(found)
        } else {
            word.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
                .or(found)
        }
    })
}

/// `path` with its longest existing ancestor resolved through the filesystem, so a symlink out
/// of the snapshot cannot pass a prefix check that only compared spellings.
fn physical(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest: Vec<&OsStr> = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let mut out = resolved;
            out.extend(rest.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Whether a write at this snapshot-relative path changed what a repository says is staged or
/// committed: its index, `HEAD`, or a ref.
fn changes_repository_state(path: &Path) -> bool {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component == Component::Normal(OsStr::new(".git")) {
            return matches!(
                components.next(),
                Some(Component::Normal(name))
                    if [OsStr::new("index"), OsStr::new("HEAD"), OsStr::new("packed-refs"), OsStr::new("refs")]
                        .contains(&name)
            );
        }
    }
    false
}

/// Where a repository keeps its state, as git itself resolves it.
struct Layout {
    /// `--git-dir`.
    git_dir: PathBuf,
    /// `--git-common-dir`.
    common_dir: PathBuf,
    /// `--git-path index`: the index file, which `GIT_INDEX_FILE` may move.
    index: PathBuf,
    /// `--git-path objects`: the object store, which `GIT_OBJECT_DIRECTORY` may move.
    objects: PathBuf,
    /// `--show-toplevel`, absent for a bare repository.
    worktree: Option<PathBuf>,
    /// `--show-cdup`: the way up from where the probes run to [`Self::worktree`], as a pathspec
    /// that names all of it. `ls-files` lists only what lies below its working directory.
    to_top: String,
}

/// One reading of a repository's state, around an invocation.
#[derive(Default)]
struct Observation {
    /// The repository, when there was one to read.
    layout: Option<Layout>,
    /// Index entries by repository-relative path: `<mode> <object> <stage>`, one per stage.
    index: BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
    /// The commit `HEAD` names, when the attribution needs it and the branch is born.
    head: Option<Vec<u8>>,
    /// Paths whose index entry differs from `HEAD`, when the attribution needs it.
    staged: BTreeSet<Vec<u8>>,
}

/// What a probe printed, and how it ended.
struct Probe {
    /// Its exit status.
    status: u8,
    /// Everything it wrote to standard output.
    stdout: Vec<u8>,
    /// Everything it wrote to standard error.
    stderr: Vec<u8>,
}

impl Probe {
    /// Standard output, when the probe succeeded; the probe's own complaint otherwise.
    fn output(self, what: &str) -> Result<Vec<u8>, String> {
        if self.status == 0 {
            Ok(self.stdout)
        } else {
            Err(format!(
                "git: could not read {what}: {}",
                String::from_utf8_lossy(&self.stderr).trim_end()
            ))
        }
    }
}

/// One invocation's native runs: the command itself and its probes, through one shell clone.
struct Runner<'a, SE: ShellExtensions> {
    /// The invocation's own shell, carrying its overlays.
    shell: &'a mut Shell<SE>,
    /// The builtin's parameters: its descriptors and process-group policy.
    params: &'a ExecutionParameters,
    /// The resolved `git` executable.
    git: &'a str,
    /// The command line's global options, which every probe repeats.
    globals: &'a [String],
    /// The repository a `git init` or `git clone` creates, which probes are pointed at instead of
    /// the one the working directory is in.
    target: Option<String>,
}

impl<SE: ShellExtensions> Runner<'_, SE> {
    /// Runs the command line itself with the builtin's own descriptors.
    ///
    /// A process group the pipeline already established is joined; a process that would
    /// otherwise land in the shell's own group gets one of its own, so a boundary can end it
    /// without touching anything else. A process that stopped rather than exited is ended and
    /// reaped here: the observation around it cannot wait for a job that may never resume.
    ///
    /// Returns the result and whether the process had to be ended.
    async fn execute(
        &mut self,
        argv: &[String],
        group: Option<i32>,
    ) -> Result<(ExecutionResult, bool), brush_core::Error> {
        let mut params = self.params.clone();
        if group.is_none()
            && matches!(
                params.process_group_policy,
                ProcessGroupPolicy::SameProcessGroup
            )
        {
            params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
        }
        self.spawn(params, argv.iter().skip(1).cloned(), group)
            .await
    }

    /// Runs `git` with `args` through the shell's own external-command path — never a function
    /// of that name — and waits for it; a process that stopped instead is ended and reaped.
    ///
    /// Returns the result and whether the process had to be ended.
    async fn spawn(
        &mut self,
        params: ExecutionParameters,
        args: impl IntoIterator<Item = String> + Send,
        group: Option<i32>,
    ) -> Result<(ExecutionResult, bool), brush_core::Error> {
        let argv = std::iter::once(self.git.to_string())
            .chain(args)
            .map(CommandArg::String)
            .collect();
        let mut command = SimpleCommand::new(
            ShellForCommand::ParentShell(self.shell),
            params,
            self.git.to_string(),
            argv,
        );
        command.use_functions = false;
        command.argv0 = Some("git".to_string());
        command.process_group_id = group;
        match command.execute().await?.wait().await? {
            ExecutionWaitResult::Completed(result) => Ok((result, false)),
            ExecutionWaitResult::Stopped(child) => Ok((reap(child).await?, true)),
        }
    }

    /// Runs one read-only query with the command line's global options and [`PROBE_OPTIONS`],
    /// capturing what it prints.
    ///
    /// Both output pipes are drained while the probe runs, so an index larger than a pipe buffer
    /// cannot stall it. The probe goes through the same spawner as the command, and is recorded
    /// and traced like it: it is part of what this line ran.
    async fn probe(&mut self, args: &[&str]) -> Result<Probe, String> {
        let (out, out_writer) = std::io::pipe().map_err(unprobed)?;
        let (err, err_writer) = std::io::pipe().map_err(unprobed)?;
        let null = std::fs::File::open("/dev/null").map_err(unprobed)?;
        let mut params = self.params.clone();
        params.set_fd(OpenFiles::STDIN_FD, OpenFile::File(null));
        params.set_fd(OpenFiles::STDOUT_FD, out_writer.into());
        params.set_fd(OpenFiles::STDERR_FD, err_writer.into());
        params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
        let managed = super::current_context()
            .ok_or_else(|| "git probe has no managed context".to_string())?;
        let stdout = managed
            .spawn_blocking(move || read_all(out))
            .map_err(unprobed)?;
        let stderr = managed
            .spawn_blocking(move || read_all(err))
            .map_err(unprobed)?;

        let target = self
            .target
            .iter()
            .flat_map(|target| ["-C".to_string(), target.clone()]);
        let options = PROBE_OPTIONS
            .iter()
            .chain(args)
            .map(|word| (*word).to_string());
        let argv: Vec<String> = self
            .globals
            .iter()
            .cloned()
            .chain(target)
            .chain(options)
            .collect();
        let status = self.spawn(params, argv, None).await;
        let stdout = stdout.await.map_err(unprobed)?;
        let stderr = stderr.await.map_err(unprobed)?;
        let (status, _) = status.map_err(unprobed)?;
        Ok(Probe {
            status: status.exit_code.into(),
            stdout: stdout.map_err(unprobed)?,
            stderr: stderr.map_err(unprobed)?,
        })
    }

    /// Refuses an invocation that would use a repository, or create one, outside `bound`, and
    /// returns the repository it runs in when there is one.
    ///
    /// Checked before anything is started, because a process that has created a directory
    /// outside the snapshot cannot be taken back. `git init` and `git clone` are checked by their
    /// destinations, parsed from the command line exactly as git parses them; every other
    /// subcommand by the repository git itself discovers from the effective working directory.
    /// Informational forms need no repository and write none, so nothing refuses them; the
    /// repository they stand in is still returned, because it is what an observation of them
    /// compares with afterwards. Sources — a clone's origin, an alternate — may be anywhere: only
    /// what git writes has to be here.
    async fn contain(
        &mut self,
        invocation: &GitInvocation<'_, String>,
        cwd: &Path,
        bound: &Path,
    ) -> Result<Option<Layout>, String> {
        let outside = |path: &Path| !physical(path).starts_with(bound);
        let Some(subcommand) = invocation.subcommand.filter(|word| !word.starts_with('-')) else {
            return self.layout().await;
        };
        if matches!(subcommand, "init" | "clone") {
            let target = destination(subcommand, invocation.command_args, cwd)?;
            let environment = [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_COMMON_DIR",
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
            ]
            .into_iter()
            .filter_map(|name| {
                self.shell
                    .env_str(name)
                    .filter(|value| !value.is_empty())
                    .map(|value| gitcmd::resolve(cwd, &value))
            });
            let options = ["--git-dir", "--work-tree"].into_iter().filter_map(|name| {
                global_path(self.globals, name).map(|value| gitcmd::resolve(cwd, value))
            });
            let mut writes: Vec<PathBuf> = environment.chain(options).collect();
            let redirected = !writes.is_empty();
            writes.extend(target.separate.iter().cloned());
            writes.push(target.path.clone());
            if let Some(escape) = writes.iter().find(|path| outside(path)) {
                return Err(format!(
                    "git {subcommand} would write {}, which is outside this snapshot ({})",
                    escape.display(),
                    bound.display()
                ));
            }
            if !redirected {
                self.target = Some(
                    target
                        .path
                        .to_str()
                        .ok_or_else(|| {
                            format!(
                                "git {subcommand}: destination {} is not UTF-8",
                                target.path.display()
                            )
                        })?
                        .to_string(),
                );
            }
            let layout = self.layout().await?;
            return Ok(layout.filter(|layout| {
                let root = if target.bare {
                    Some(&layout.git_dir)
                } else {
                    layout.worktree.as_ref()
                };
                redirected || root.is_some_and(|root| physical(root) == physical(&target.path))
            }));
        }
        let Some(layout) = self.layout().await? else {
            // No repository: git reports that itself, and has nothing of the seed to touch.
            return Ok(None);
        };
        let used = [
            Some(&layout.git_dir),
            Some(&layout.common_dir),
            Some(&layout.index),
            Some(&layout.objects),
            layout.worktree.as_ref(),
        ];
        if let Some(escape) = used.into_iter().flatten().find(|path| outside(path)) {
            return Err(format!(
                "the repository at {} is outside this snapshot ({}); git here may only use a \
                 repository inside it",
                escape.display(),
                bound.display()
            ));
        }
        Ok(Some(layout))
    }

    /// The repository the probes run in, or `None` when there is none.
    ///
    /// The four paths come from one query, each on its own line; a path with a line feed in it
    /// would make that ambiguous, so it is refused rather than guessed at.
    async fn layout(&mut self) -> Result<Option<Layout>, String> {
        let dirs = self
            .probe(&[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
                "--git-path",
                "index",
                "--git-path",
                "objects",
            ])
            .await?;
        if dirs.status != 0 {
            return Ok(None);
        }
        let lines: Vec<&[u8]> = dirs
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&dirs.stdout)
            .split(|byte| *byte == b'\n')
            .collect();
        let [git_dir, common_dir, index, objects] = lines.as_slice() else {
            return Err("git: the repository's paths could not be read unambiguously".to_string());
        };
        let path = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
        let (git_dir, common_dir, index, objects) =
            (path(git_dir), path(common_dir), path(index), path(objects));
        // The way up is a run of `../` and cannot hold a line feed, so it goes first and the
        // top-level directory is everything after it.
        let top = self
            .probe(&["rev-parse", "--show-cdup", "--show-toplevel"])
            .await?;
        let (to_top, worktree) = match top.stdout.strip_suffix(b"\n") {
            Some(lines) if top.status == 0 => {
                let end = lines
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .ok_or("git: the repository's top level could not be read")?;
                let up = lines.get(..end).unwrap_or_default();
                let top = lines.get(end + 1..).unwrap_or_default();
                (
                    format!("./{}", String::from_utf8_lossy(up)),
                    Some(path(top)),
                )
            }
            _ => (String::new(), None),
        };
        Ok(Some(Layout {
            git_dir,
            common_dir,
            index,
            objects,
            worktree,
            to_top,
        }))
    }

    /// Reads the repository state the attribution of `invocation` needs.
    ///
    /// `layout` is the repository [`Self::contain`] already found for the reading before the
    /// run; the reading after it looks again, because the run may have created or moved one.
    async fn observe(
        &mut self,
        invocation: &GitInvocation<'_, String>,
        layout: Option<Layout>,
        before: bool,
    ) -> Result<Observation, String> {
        let layout = if before { layout } else { self.layout().await? };
        let Some(present) = &layout else {
            return Ok(Observation::default());
        };
        let mut observation = Observation::default();
        if present.worktree.is_some() {
            let everything = [
                "ls-files",
                "--stage",
                "-z",
                "--full-name",
                "--",
                &present.to_top,
            ];
            observation.index = parse_index(&self.probe(&everything).await?.output("the index")?);
        }
        let action = &invocation.action;
        if matches!(action, Some(GitAction::Commit { .. })) || unknown(invocation) {
            observation.head = self.head().await?;
        }
        if before && matches!(action, Some(GitAction::Unstage)) {
            observation.staged = if self.head().await?.is_some() {
                let staged = self
                    .probe(&["diff-index", "--cached", "--name-only", "-z", "HEAD", "--"])
                    .await?;
                records(&staged.output("the staged paths")?)
                    .map(<[u8]>::to_vec)
                    .collect()
            } else {
                // An unborn branch: everything in the index is staged.
                observation.index.keys().cloned().collect()
            };
        }
        observation.layout = layout;
        Ok(observation)
    }

    /// The commit `HEAD` names, or `None` on an unborn branch.
    async fn head(&mut self) -> Result<Option<Vec<u8>>, String> {
        let head = self
            .probe(&["rev-parse", "-q", "--verify", "HEAD^{commit}"])
            .await?;
        Ok((head.status == 0).then(|| head.stdout.trim_ascii_end().to_vec()))
    }

    /// What the run did, as git actions on snapshot-relative paths, and the repository metadata
    /// its window's writes belong to.
    ///
    /// # Errors
    ///
    /// Fails with a diagnostic when a fact the attribution needs cannot be read, or when the run
    /// changed staged or committed state that its classification cannot name.
    async fn attribute(
        &mut self,
        invocation: &GitInvocation<'_, String>,
        before: &Observation,
        window: &[PathBuf],
        bound: &Path,
    ) -> Result<(Vec<(GitAction, PathBuf)>, Vec<PathBuf>), String> {
        let after = self.observe(invocation, None, false).await?;
        let relative = |path: &Path| path.strip_prefix(bound).ok().map(Path::to_path_buf);
        let metadata: Vec<PathBuf> = [&before.layout, &after.layout]
            .into_iter()
            .flatten()
            .flat_map(|layout| [&layout.git_dir, &layout.common_dir])
            .filter_map(|path| relative(&physical(path)))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let Some(top) = [&after.layout, &before.layout]
            .into_iter()
            .flatten()
            .find_map(|layout| layout.worktree.as_deref())
            .and_then(|top| relative(&physical(top)))
        else {
            // No worktree before or after: nothing a resource names changed.
            return Ok((Vec::new(), metadata));
        };
        let written: BTreeSet<Vec<u8>> = window
            .iter()
            .filter(|path| !metadata.iter().any(|root| path.starts_with(root)))
            .filter_map(|path| path.strip_prefix(&top).ok())
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.as_os_str().as_bytes().to_vec())
            .collect();
        let changed: BTreeSet<&Vec<u8>> = before
            .index
            .keys()
            .chain(after.index.keys())
            .filter(|path| before.index.get(*path) != after.index.get(*path))
            .collect();
        let requests = self
            .transitions(invocation, before, &after, &changed, &written)
            .await?
            .into_iter()
            .map(|(action, path)| (action, top.join(OsStr::from_bytes(&path))))
            .collect();
        Ok((requests, metadata))
    }

    /// The transitions a run made, by repository-relative path, read off the index entries it
    /// `changed` and the worktree paths it `written` according to what it was classified as.
    ///
    /// # Errors
    ///
    /// Fails when a fact the attribution needs cannot be read, or when a run the grammar could
    /// not decide — or one that should only have touched metadata — changed staged or committed
    /// state.
    async fn transitions(
        &mut self,
        invocation: &GitInvocation<'_, String>,
        before: &Observation,
        after: &Observation,
        changed: &BTreeSet<&Vec<u8>>,
        written: &BTreeSet<Vec<u8>>,
    ) -> Result<Vec<(GitAction, Vec<u8>)>, String> {
        let mut requests: Vec<(GitAction, Vec<u8>)> = Vec::new();
        let mut request = |action: GitAction, path: &[u8]| requests.push((action, path.to_vec()));
        match &invocation.action {
            Some(GitAction::Stage) => {
                for path in changed {
                    if written.contains(*path) {
                        request(GitAction::Edit, path);
                    }
                    request(GitAction::Stage, path);
                }
            }
            Some(GitAction::Unstage) => {
                for path in changed {
                    let action = if before.staged.contains(*path) {
                        GitAction::Unstage
                    } else {
                        // Not staged before: the committed base moved under unchanged content,
                        // which leaves the resource modified rather than unstaged.
                        GitAction::Edit
                    };
                    request(action, path);
                }
            }
            Some(GitAction::Delete) => {
                for path in changed {
                    if !after.index.contains_key(*path) {
                        request(GitAction::Delete, path);
                    } else {
                        if !before.index.contains_key(*path) {
                            request(GitAction::Edit, path);
                        }
                        request(GitAction::Stage, path);
                    }
                }
            }
            Some(GitAction::Commit { .. }) => {
                return self.commits(before, after, changed, written).await;
            }
            Some(GitAction::Checkout) => return self.settle(&after.index, changed, written).await,
            Some(GitAction::Edit) if !unknown(invocation) => {
                return self.settle(&after.index, changed, written).await;
            }
            Some(GitAction::Stash) => {
                let mut stashed: BTreeSet<&[u8]> =
                    changed.iter().map(|path| path.as_slice()).collect();
                stashed.extend(written.iter().map(Vec::as_slice));
                for path in stashed {
                    request(GitAction::Stash, path);
                }
            }
            Some(GitAction::Clean) => {
                for path in written {
                    request(GitAction::Clean, path);
                }
            }
            Some(GitAction::Edit) | None => {
                let moved_head = unknown(invocation) && before.head != after.head;
                if !changed.is_empty() || moved_head {
                    return Err("git: cannot attribute repository state changes".to_string());
                }
            }
            Some(GitAction::Read | GitAction::Diff | GitAction::History) => {}
        }
        Ok(requests)
    }

    /// What a commit-making run did: each path the new `HEAD` changed is committed with that
    /// commit's real message — edited first when the run itself rewrote it, staged first when the
    /// committed entry is not the one the index held — and anything else it staged is staged.
    /// A run that made no commit commits nothing.
    async fn commits(
        &mut self,
        before: &Observation,
        after: &Observation,
        changed: &BTreeSet<&Vec<u8>>,
        written: &BTreeSet<Vec<u8>>,
    ) -> Result<Vec<(GitAction, Vec<u8>)>, String> {
        let committed = match (&before.head, &after.head) {
            (_, None) => Vec::new(),
            (Some(old), Some(new)) if old == new => Vec::new(),
            (old, Some(new)) => self.committed(old.as_deref(), new).await?,
        };
        let mut requests: Vec<(GitAction, Vec<u8>)> = Vec::new();
        if let (Some(new), false) = (&after.head, committed.is_empty()) {
            let message = self.message(new).await?;
            for (path, entry) in &committed {
                if written.contains(path) {
                    requests.push((GitAction::Edit, path.clone()));
                }
                let staged = before
                    .index
                    .get(path)
                    .and_then(|entries| entries.iter().find(|entry| entry.ends_with(b" 0")))
                    .and_then(|entry| entry.get(..entry.len() - 2));
                if staged != entry.as_deref() {
                    requests.push((GitAction::Stage, path.clone()));
                }
                requests.push((
                    GitAction::Commit {
                        message: Some(message.clone()),
                    },
                    path.clone(),
                ));
            }
        }
        for path in changed {
            if !committed.iter().any(|(committed, _)| committed == *path) {
                requests.push((GitAction::Stage, (*path).clone()));
            }
        }
        Ok(requests)
    }

    /// The per-path outcome of a run that puts repository content into the worktree: a
    /// checkout, a switch, a merge, a rebase, a restoring reset, a clone, a popped stash.
    ///
    /// Every path the run wrote, whose index entry it changed, or that it left conflicted is
    /// judged by where it ended up. Clean — worktree, index and `HEAD` agree — is a checkout;
    /// staged — the index moved and the worktree with it — is an edit and a stage; anything else,
    /// a conflict above all, is an edit that settles nothing.
    async fn settle(
        &mut self,
        after: &BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
        changed: &BTreeSet<&Vec<u8>>,
        written: &BTreeSet<Vec<u8>>,
    ) -> Result<Vec<(GitAction, Vec<u8>)>, String> {
        let conflicted = after
            .iter()
            .filter(|(_, entries)| entries.iter().any(|entry| !entry.ends_with(b" 0")))
            .map(|(path, _)| path.as_slice());
        let affected: BTreeSet<&[u8]> = changed
            .iter()
            .map(|path| path.as_slice())
            .chain(written.iter().map(Vec::as_slice))
            .chain(conflicted)
            .collect();
        if affected.is_empty() {
            return Ok(Vec::new());
        }
        let status = parse_status(
            &self
                .probe(&[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                    "--no-renames",
                ])
                .await?
                .output("the worktree status")?,
        );
        let mut requests = Vec::new();
        for path in affected {
            match status.get(path) {
                None => requests.push((GitAction::Checkout, path.to_vec())),
                Some(Status::Staged) => {
                    requests.push((GitAction::Edit, path.to_vec()));
                    requests.push((GitAction::Stage, path.to_vec()));
                }
                Some(Status::Dirty) => requests.push((GitAction::Edit, path.to_vec())),
            }
        }
        Ok(requests)
    }

    /// The entries a commit changed relative to `old`, by repository-relative path; an entry is
    /// `<mode> <object>`, or `None` for a path the commit removed.
    async fn committed(
        &mut self,
        old: Option<&[u8]>,
        new: &[u8],
    ) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>, String> {
        let new = object_name(new)?;
        let Some(old) = old else {
            let listing = self
                .probe(&["ls-tree", "-r", "-z", "--full-tree", new])
                .await?
                .output("the committed tree")?;
            return Ok(records(&listing)
                .filter_map(|record| {
                    let tab = record.iter().position(|byte| *byte == b'\t')?;
                    let fields: Vec<&[u8]> = record[..tab].split(|byte| *byte == b' ').collect();
                    let [mode, _, object] = fields.as_slice() else {
                        return None;
                    };
                    Some((
                        record[tab + 1..].to_vec(),
                        Some([*mode, b" ", *object].concat()),
                    ))
                })
                .collect());
        };
        let old = object_name(old)?;
        let raw = self
            .probe(&["diff-tree", "-r", "-z", "--no-renames", old, new])
            .await?
            .output("the committed changes")?;
        let mut fields = records(&raw);
        let mut committed = Vec::new();
        while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
            let parts: Vec<&[u8]> = meta.split(|byte| *byte == b' ').collect();
            let [_, mode, _, object, _] = parts.as_slice() else {
                return Err("git: the committed changes could not be read".to_string());
            };
            let removed = object.iter().all(|byte| *byte == b'0');
            committed.push((
                path.to_vec(),
                (!removed).then(|| [*mode, b" ", *object].concat()),
            ));
        }
        Ok(committed)
    }

    /// The message of `commit` as the object stores it, less exactly one terminating line feed.
    async fn message(&mut self, commit: &[u8]) -> Result<String, String> {
        let commit = object_name(commit)?;
        let object = self
            .probe(&["cat-file", "commit", commit])
            .await?
            .output("the commit")?;
        let body = object
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .map_or(&[][..], |at| &object[at + 2..]);
        let body = body.strip_suffix(b"\n").unwrap_or(body);
        Ok(String::from_utf8_lossy(body).into_owned())
    }
}

/// Whether `invocation` is a form the grammar could not decide — classified as an edit — rather
/// than one whose edit is a known outcome, like a popped stash.
fn unknown<S: AsRef<str>>(invocation: &GitInvocation<'_, S>) -> bool {
    matches!(invocation.action, Some(GitAction::Edit)) && invocation.subcommand != Some("stash")
}

/// Ends a stopped process and waits for it to exit.
async fn reap(mut child: ChildProcess) -> Result<ExecutionResult, brush_core::Error> {
    if let Some(pid) = child.pid() {
        // The owned ChildProcess has not been reaped, so this PID still identifies that child.
        // SAFETY: pidfd_open takes integer arguments and returns an owned descriptor.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let descriptor =
            i32::try_from(descriptor).map_err(|error| std::io::Error::other(error.to_string()))?;
        // SAFETY: pidfd_open returned a fresh descriptor, uniquely owned here.
        let descriptor = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) };
        // SAFETY: the pidfd pins the child; null siginfo requests an ordinary SIGKILL.
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                descriptor.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    loop {
        if let ProcessWaitResult::Completed(output) = child.wait().await? {
            return Ok(ExecutionResult::from(output));
        }
    }
}

/// Everything a pipe carries until its writers close it.
fn read_all(mut pipe: std::io::PipeReader) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Why a probe could not run, as the attribution reports it.
fn unprobed(error: impl std::fmt::Display) -> String {
    format!("git: a probe could not run: {error}")
}

/// The non-empty NUL-terminated records of `bytes`.
fn records(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

/// A commit object name read back from git, which is hexadecimal and so UTF-8.
fn object_name(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|_| "git: HEAD is not a hexadecimal object name".to_string())
}

/// `git ls-files --stage -z`: `<mode> <object> <stage>\t<path>\0` per entry.
fn parse_index(bytes: &[u8]) -> BTreeMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut index: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for record in records(bytes) {
        if let Some(tab) = record.iter().position(|byte| *byte == b'\t') {
            index
                .entry(record[tab + 1..].to_vec())
                .or_default()
                .push(record[..tab].to_vec());
        }
    }
    index
}

/// Where a path the status lists stands. A path it does not list is clean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// The index differs from `HEAD` and the worktree agrees with the index.
    Staged,
    /// The worktree differs from the index, or the path is conflicted, untracked or ignored.
    Dirty,
}

/// `git status --porcelain=v2 -z --no-renames`, by repository-relative path.
///
/// Ordinary entries (`1`) carry eight fields before the path and conflicted ones (`u`) ten;
/// untracked (`?`) and ignored (`!`) entries carry the path alone. The path is everything after
/// the fixed fields, bytes and all.
fn parse_status(bytes: &[u8]) -> BTreeMap<Vec<u8>, Status> {
    records(bytes)
        .filter_map(status_entry)
        .map(|(path, status)| (path.to_vec(), status))
        .collect()
}

/// One porcelain-v2 record's path and standing, or `None` for a header or an unknown record.
fn status_entry(record: &[u8]) -> Option<(&[u8], Status)> {
    let fields = |count: usize| record.splitn(count + 1, |byte| *byte == b' ');
    match record.first()? {
        b'1' => {
            let mut parts = fields(8);
            let status = match parts.nth(1)? {
                [index, b'.'] if *index != b'.' => Status::Staged,
                _ => Status::Dirty,
            };
            Some((parts.nth(6)?, status))
        }
        b'u' => Some((fields(10).nth(10)?, Status::Dirty)),
        b'?' | b'!' => Some((record.get(2..)?, Status::Dirty)),
        _ => None,
    }
}

/// Where `git init` or `git clone` puts the repository it creates.
struct Destination {
    /// The directory it creates: the worktree, or the repository itself when bare.
    path: PathBuf,
    /// A `--separate-git-dir`, which is written too.
    separate: Option<PathBuf>,
    /// Whether the destination is a bare repository.
    bare: bool,
}

/// The destination of `git init` or `git clone` run with `args` from `cwd`.
///
/// Operands are found with git's own option table, so a value that looks like a path belongs to
/// the option before it. An omitted clone destination is named from the source the way git names
/// it ([`clone_basename`]).
fn destination<S: AsRef<str>>(
    subcommand: &str,
    args: &[S],
    cwd: &Path,
) -> Result<Destination, String> {
    let clone = subcommand == "clone";
    let scan = gitcmd::scan(args, if clone { &gitcmd::CLONE } else { &gitcmd::INIT });
    let bare = scan.has(None, "bare") || scan.has(None, "mirror");
    let separate = scan
        .value(None, "separate-git-dir")
        .map(|value| gitcmd::resolve(cwd, value));
    let path = if clone {
        let Some(source) = scan.positionals.first() else {
            return Err("git clone names no repository to clone".to_string());
        };
        if let Some(explicit) = scan.positionals.get(1) {
            gitcmd::resolve(cwd, explicit)
        } else {
            let name = clone_basename(source, is_bundle(source, cwd), bare).ok_or_else(|| {
                "git clone: no directory name could be guessed from the source".to_string()
            })?;
            gitcmd::resolve(cwd, &name)
        }
    } else {
        scan.positionals.first().map_or_else(
            || cwd.to_path_buf(),
            |explicit| gitcmd::resolve(cwd, explicit),
        )
    };
    Ok(Destination {
        path,
        separate,
        bare,
    })
}

/// Whether a clone source names a bundle file, as `builtin/clone.c`'s `get_repo_path` decides:
/// a local path whose repository spellings are not directories and whose bundle spellings are a
/// regular file.
fn is_bundle(source: &str, cwd: &Path) -> bool {
    if source.contains("://") {
        return false;
    }
    let path = gitcmd::resolve(cwd, source);
    let spelled = |suffix: &str| PathBuf::from(format!("{}{suffix}", path.display()));
    if ["/.git", "", ".git/.git", ".git"]
        .iter()
        .any(|suffix| spelled(suffix).is_dir())
    {
        return false;
    }
    [".bundle", ""]
        .iter()
        .any(|suffix| spelled(suffix).is_file())
}

/// The directory `git clone` creates for `repo` when none is given: `dir.c`'s `git_url_basename`.
///
/// The scheme and any authentication are skipped; trailing slashes, whitespace and a `/.git` are
/// stripped; a trailing port is dropped from a bare host; the last `/`- or `:`-separated component
/// is kept, less a `.git` (or, for a bundle, `.bundle`) suffix; a bare clone gets `.git` back; and
/// runs of control characters and whitespace become single spaces, trimmed at both ends.
fn clone_basename(repo: &str, is_bundle: bool, is_bare: bool) -> Option<String> {
    let bytes = repo.as_bytes();
    let space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let mut start = repo.find("://").map_or(0, |at| at + 3);
    let mut end = bytes.len();
    let mut at = start;
    while at < end && bytes[at] != b'/' {
        if bytes[at] == b'@' {
            start = at + 1;
        }
        at += 1;
    }
    while start < end && (bytes[end - 1] == b'/' || space(bytes[end - 1])) {
        end -= 1;
    }
    if end - start > 5 && bytes[end - 5] == b'/' && &bytes[end - 4..end] == b".git" {
        end -= 5;
        while start < end && bytes[end - 1] == b'/' {
            end -= 1;
        }
    }
    let host = &bytes[start..end];
    if !host.contains(&b'/') && host.contains(&b':') {
        let mut port = end;
        while start < port && bytes[port - 1].is_ascii_digit() {
            port -= 1;
        }
        if start < port && bytes[port - 1] == b':' {
            end = port - 1;
        }
    }
    let mut component = end;
    while start < component && bytes[component - 1] != b'/' && bytes[component - 1] != b':' {
        component -= 1;
    }
    let suffix: &[u8] = if is_bundle { b".bundle" } else { b".git" };
    let mut len = end - component;
    if bytes[component..end].ends_with(suffix) {
        len -= suffix.len();
    }
    if len == 0 || (len == 1 && bytes[component] == b'/') {
        return None;
    }
    let base = repo.get(component..component + len)?;
    let dir = if is_bare {
        format!("{base}.git")
    } else {
        base.to_string()
    };
    let mut out = String::with_capacity(dir.len());
    let mut previous_space = true;
    for character in dir.chars() {
        let character = if u32::from(character) < 0x20 {
            ' '
        } else {
            character
        };
        if character == ' ' {
            if previous_space {
                continue;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
        out.push(character);
    }
    if previous_space {
        out.pop();
    }
    Some(out)
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// The guess is git's, byte for byte, across the spellings a source can take.
    #[test]
    fn a_clone_destination_is_named_as_git_names_it() {
        for (repo, bundle, bare, expected) in [
            (
                "https://user@host.example/path/repo.git",
                false,
                false,
                Some("repo"),
            ),
            (
                "https://host.example/path/repo/.git/",
                false,
                false,
                Some("repo"),
            ),
            ("host.example:path/repo.git", false, false, Some("repo")),
            ("host.example:2222", false, false, Some("host.example")),
            ("/foo/bar:2222.git", false, false, Some("2222")),
            ("file:///srv/origin", false, false, Some("origin")),
            ("/srv/origin", false, true, Some("origin.git")),
            ("../backup.bundle", true, false, Some("backup")),
            (
                "/srv/with\ttab  and spaces ",
                false,
                false,
                Some("with tab and spaces"),
            ),
            ("/", false, false, None),
        ] {
            assert_eq!(
                clone_basename(repo, bundle, bare).as_deref(),
                expected,
                "{repo:?}"
            );
        }
    }

    /// The fixed fields end before the path, so a path with spaces in it is read whole.
    #[test]
    fn status_records_keep_their_whole_path() {
        let status = parse_status(
            b"1 M. N... 100644 100644 100644 a b staged file\0\
              1 .M N... 100644 100644 100644 a a dirty\0\
              u UU N... 100644 100644 100644 100644 a b c conflict\0\
              ? new file\0\
              ! ignored\0",
        );
        assert_eq!(status.get(&b"staged file"[..]), Some(&Status::Staged));
        assert_eq!(status.get(&b"dirty"[..]), Some(&Status::Dirty));
        assert_eq!(status.get(&b"conflict"[..]), Some(&Status::Dirty));
        assert_eq!(status.get(&b"new file"[..]), Some(&Status::Dirty));
        assert_eq!(status.get(&b"ignored"[..]), Some(&Status::Dirty));
        assert_eq!(status.len(), 5);
    }

    /// Only state files count: an object fetched on demand is not a change of what is staged.
    #[test]
    fn only_index_head_and_refs_are_repository_state() {
        for (path, state) in [
            ("repo/.git/index", true),
            ("repo/.git/HEAD", true),
            ("repo/.git/refs/heads/main", true),
            ("repo/.git/packed-refs", true),
            ("repo/.git/objects/ab/cdef", false),
            ("repo/index", false),
        ] {
            assert_eq!(changes_repository_state(Path::new(path)), state, "{path}");
        }
    }
}
