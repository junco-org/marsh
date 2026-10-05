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
//!
//! A `git` some other process executes is observed by the same [`Runner`]: the tracer stops it
//! before its first instruction, its argv, cwd and environment become the runner's inputs
//! ([`external`]), and its end is attributed once its whole process tree has ended. It keeps its
//! own environment; only the probes reproduce it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Weak};

use brush_core::commands::{CommandArg, ShellForCommand, SimpleCommand, compose_std_command};
use brush_core::env::EnvironmentScope;
use brush_core::processes::{ChildProcess, ProcessWaitResult};
use brush_core::results::ExecutionWaitResult;
use brush_core::{
    ExecutionContext, ExecutionParameters, ExecutionResult, ProcessGroupPolicy, Shell,
    ShellExtensions, ShellVariable,
};
use marsh_instrument::{ChildEvent, InvocationId, TraceRun, Tracing};

use super::gitcmd::{self, GitAction};
use crate::shell::completion::Completion;
use crate::shell::execution::brush_error;
use crate::shell::policy::capability_of;
use crate::shell::snapshot::{GitCohortKind, GitEffectRecord, GitGuard, Snapshot};

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
    let runner = Runner::new(argv);
    let internal = |message: &str| {
        brush_core::Error::from(brush_core::ErrorKind::InternalError(message.into()))
    };

    // One clone per invocation carries every overlay, so nothing set here can leak into the
    // caller's shell, whatever becomes of this future.
    let mut shell = context.shell.clone();
    isolate(&mut shell, snapshot.path(), runner.kind())?;
    let template = compose_std_command(
        &ExecutionContext {
            shell: &mut shell,
            command_name: "git".to_string(),
            params: ExecutionParameters::default(),
            process_group_id: None,
        },
        &git,
        "git",
        &[] as &[&str],
        false,
    )?;
    let owner =
        super::current_context().ok_or_else(|| internal("git has no owning command context"))?;
    let invocation = owner
        .invocation()
        .ok_or_else(|| internal("git has no builtin invocation"))?;
    let (tracing, trace) = owner
        .run()
        .and_then(|run| run.trace())
        .map(|(tracing, trace)| (Arc::clone(tracing), trace))
        .map_err(brush_error)?;
    let prepared = {
        let snapshot = Arc::clone(&snapshot);
        owner
            .spawn_blocking(move || runner.prepare(template, &snapshot, tracing, trace, invocation))
            .map_err(brush_error)?
    };
    let runner = match prepared
        .await
        .map_err(|_| internal("git preparation was abandoned"))?
    {
        Ok(runner) => runner,
        Err(refusal) => {
            writeln!(context.stderr(), "{refusal}")?;
            return Ok(ExecutionResult::new(FATAL));
        }
    };

    let (result, stopped) = execute(
        &mut shell,
        &context.params,
        &git,
        runner.argv(),
        context.process_group_id,
    )
    .await?;
    let success = result.is_success();
    let finished = owner
        .spawn_blocking(move || runner.finish(success, stopped))
        .map_err(brush_error)?;
    match finished.await {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => snapshot.fail_evidence(failure),
        Err(_) => snapshot.fail_evidence("git invocation did not finish observation".into()),
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

/// Runs the command line itself with the builtin's own descriptors.
///
/// A process group the pipeline already established is joined; a process that would otherwise
/// land in the shell's own group gets one of its own, so a boundary can end it without touching
/// anything else. A process that stopped rather than exited is ended and reaped here: the
/// observation around it cannot wait for a job that may never resume.
///
/// Returns the result and whether the process had to be ended.
async fn execute<SE: ShellExtensions>(
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
    git: &str,
    argv: &[String],
    group: Option<i32>,
) -> Result<(ExecutionResult, bool), brush_core::Error> {
    let mut params = params.clone();
    if group.is_none()
        && matches!(
            params.process_group_policy,
            ProcessGroupPolicy::SameProcessGroup
        )
    {
        params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
    }
    spawn(shell, params, git, argv.iter().skip(1).cloned(), group).await
}

/// Runs `git` with `args` through the shell's own external-command path — never a function of
/// that name — and waits for it; a process that stopped instead is ended and reaped.
///
/// Returns the result and whether the process had to be ended.
async fn spawn<SE: ShellExtensions>(
    shell: &mut Shell<SE>,
    params: ExecutionParameters,
    git: &str,
    args: impl IntoIterator<Item = String> + Send,
    group: Option<i32>,
) -> Result<(ExecutionResult, bool), brush_core::Error> {
    let argv = std::iter::once(git.to_string())
        .chain(args)
        .map(CommandArg::String)
        .collect();
    let mut command = SimpleCommand::new(
        ShellForCommand::ParentShell(shell),
        params,
        git.to_string(),
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

/// The git actions a run performed on snapshot-relative paths, and the repository metadata
/// roots its writes belong to.
type Attribution = (Vec<(GitAction, PathBuf)>, Vec<PathBuf>);
/// A committed path and its new `<mode> <object>` entry, `None` when the commit removed it.
type CommittedEntry = (Vec<u8>, Option<Vec<u8>>);

/// Why an unprepared runner cannot observe anything.
const UNPREPARED: &str = "git: observation runner is not prepared";

/// An executed `git` as the builtin's runner takes it: its argument vector, and a template of
/// its program, working directory and exact environment for the probes to reproduce.
///
/// The conversion is lossless or refused. The classifier reads UTF-8 words; `git-*` dispatch
/// through `argv[0]` is not a builtin invocation; and an environment entry without a name, or
/// a name given twice, would leave the probes guessing which value git used.
///
/// # Errors
///
/// Returns the refusal's diagnostic.
pub(crate) fn external(
    command: marsh_instrument::ExecCommand,
) -> Result<(Vec<String>, Command), &'static str> {
    let argv = command
        .argv
        .into_iter()
        .map(std::ffi::OsString::into_string)
        .collect::<Result<Vec<String>, _>>()
        .map_err(|_| "git: exec arguments are not UTF-8")?;
    if argv
        .first()
        .is_none_or(|zero| Path::new(zero).file_name() != Some(OsStr::new("git")))
    {
        return Err("git: unsupported exec argv[0]");
    }
    let mut template = Command::new(&command.program);
    template.current_dir(&command.cwd).env_clear();
    let mut names = std::collections::HashSet::new();
    for entry in &command.environment {
        let entry = entry.as_bytes();
        let (name, value) = entry
            .iter()
            .position(|byte| *byte == b'=')
            .filter(|at| *at > 0)
            .map(|at| (&entry[..at], &entry[at + 1..]))
            .ok_or("git: external invocation environment is ambiguous")?;
        if !names.insert(name) {
            return Err("git: external invocation environment is ambiguous");
        }
        template.env(OsStr::from_bytes(name), OsStr::from_bytes(value));
    }
    Ok((argv, template))
}

/// One invocation's observation: its command line, classified once, and the native inputs its
/// probes share with the command — program, working directory and environment — together with
/// the reading before the run and the admission its attribution completes.
///
/// The builtin and an external `git` the tracer selected both prepare one before the command
/// runs and finish it after; nothing is observed through a runner that was never prepared.
pub(crate) struct Runner {
    /// The command line, `git` first.
    argv: Vec<String>,
    /// The classified action.
    action: Option<GitAction>,
    /// How many global options follow `argv[0]`.
    globals: usize,
    /// Whether the word after the global options is a subcommand.
    subcommand: bool,
    /// Whether the invocation only inspects or must run alone.
    kind: GitCohortKind,
    /// The repository a `git init` or `git clone` creates, which probes are pointed at instead of
    /// the one the working directory is in.
    target: Option<String>,
    /// The program, working directory and environment the command runs with, which every probe
    /// reproduces.
    template: Option<Command>,
    /// The tracer, run and invocation every probe is attributed to.
    trace: Option<(Arc<Tracing>, TraceRun, InvocationId)>,
    /// The snapshot the invocation runs in.
    snapshot: Weak<Snapshot>,
    /// The repository's state before an exclusive run.
    before: Option<Observation>,
    /// The admission that the attribution completes.
    guard: Option<Completion<GitGuard>>,
}

impl Runner {
    /// Classifies `argv`, which starts with `git`, once.
    pub(crate) fn new(argv: Vec<String>) -> Self {
        let parsed = gitcmd::parse(&argv);
        let globals = parsed.global_args.len();
        let subcommand = parsed.subcommand.is_some();
        let action = parsed.action;
        let kind = match action.clone().map(capability_of) {
            Some(action) if action.is_read() && !action.is_write() => GitCohortKind::Inspect,
            _ => GitCohortKind::Exclusive,
        };
        Self {
            argv,
            action,
            globals,
            subcommand,
            kind,
            target: None,
            template: None,
            trace: None,
            snapshot: Weak::new(),
            before: None,
            guard: None,
        }
    }

    /// Whether the invocation only inspects or must run alone.
    pub(crate) const fn kind(&self) -> GitCohortKind {
        self.kind
    }

    /// The command line, `git` first.
    pub(crate) fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Binds the command's native inputs, admits the invocation, refuses one that would use or
    /// create a repository outside the snapshot, and reads the state an exclusive run is judged
    /// against; returns the prepared runner.
    ///
    /// `command` supplies the program, working directory and environment every probe runs with;
    /// probes are attributed to `invocation` of `run`.
    ///
    /// # Errors
    ///
    /// Returns the diagnostic of a refusal; the command must not run. A reading that fails is
    /// latched on the admission instead, which then never publishes.
    pub(crate) fn prepare(
        mut self,
        command: Command,
        snapshot: &Arc<Snapshot>,
        tracing: Arc<Tracing>,
        run: TraceRun,
        invocation: InvocationId,
    ) -> Result<Self, String> {
        let start = command
            .get_current_dir()
            .ok_or("git: the invocation has no working directory")?
            .to_path_buf();
        self.template = Some(command);
        self.trace = Some((tracing, run, invocation));
        self.snapshot = Arc::downgrade(snapshot);
        let cwd = effective_cwd(&start, self.globals());
        let guard = snapshot.begin_git(run, invocation, self.kind)?;
        let layout = match self.contain(&cwd, snapshot.path()) {
            Ok(layout) => layout,
            Err(refusal) => {
                guard.complete();
                return Err(format!("fatal: {refusal}"));
            }
        };
        if self.kind == GitCohortKind::Exclusive {
            match self.observe(layout, true) {
                Ok(before) => self.before = Some(before),
                Err(failure) => guard.fail(failure),
            }
        }
        self.guard = Some(guard);
        Ok(self)
    }

    /// Attributes the finished run, which exited successfully when `success`, or had to be
    /// ended when `stopped`, and completes its admission.
    ///
    /// # Errors
    ///
    /// Fails only for a runner that was never prepared. Every failure of the attribution itself
    /// is latched on the admission, so the line is never published.
    pub(crate) fn finish(mut self, success: bool, stopped: bool) -> Result<(), String> {
        let guard = self.guard.take().ok_or(UNPREPARED)?;
        let (tracing, run, invocation) = self.trace.as_ref().ok_or(UNPREPARED)?;
        let snapshot = self.snapshot.upgrade().ok_or(UNPREPARED)?;
        if stopped {
            guard.fail("git: a managed git was stopped before it completed".to_string());
        }
        if let Err(failure) = tracing.drain(*run) {
            guard.fail(format!("git: evidence drain failed: {failure}"));
            return Ok(());
        }
        let window = snapshot.writes_for(*invocation);
        if !success && !window.is_empty() {
            guard.fail("failed git invocation changed protected state".into());
            return Ok(());
        }
        match (self.kind, self.before.take()) {
            (GitCohortKind::Inspect, _) => {
                if window.iter().any(|path| changes_repository_state(path)) {
                    guard.fail("git: inspection changed repository state".to_string());
                }
                guard.complete();
            }
            (GitCohortKind::Exclusive, None) => {}
            (GitCohortKind::Exclusive, Some(before)) => {
                match self.attribute(&before, &window, snapshot.path()) {
                    Ok((requests, metadata)) => guard.record(GitEffectRecord {
                        invocation: *invocation,
                        started_order: 0,
                        finished_order: 0,
                        requests,
                        metadata,
                    }),
                    Err(failure) => guard.fail(failure),
                }
            }
        }
        Ok(())
    }

    /// The command line's global options, which every probe repeats.
    fn globals(&self) -> &[String] {
        self.argv.get(1..=self.globals).unwrap_or_default()
    }

    /// The subcommand word, when the command line has one.
    fn subcommand(&self) -> Option<&str> {
        self.subcommand
            .then(|| self.argv.get(self.globals + 1).map(String::as_str))
            .flatten()
    }

    /// Runs one read-only query with the command line's global options and [`PROBE_OPTIONS`],
    /// capturing what it prints.
    ///
    /// The probe runs the command's own program in its working directory with exactly its
    /// environment, in a process group of its own, and is traced as part of the invocation: it
    /// is part of what this line ran. Both output pipes are drained while it runs, so an index
    /// larger than a pipe buffer cannot stall it; a probe that stops is ended.
    fn probe(&self, args: &[&str]) -> Result<Probe, String> {
        let (Some(template), Some((tracing, run, invocation))) = (&self.template, &self.trace)
        else {
            return Err(UNPREPARED.to_string());
        };
        let cwd = template
            .get_current_dir()
            .ok_or("git: the invocation has no working directory")?;
        let (out, out_writer) = std::io::pipe().map_err(unprobed)?;
        let (err, err_writer) = std::io::pipe().map_err(unprobed)?;
        let mut command = Command::new(template.get_program());
        command
            .arg0("git")
            .current_dir(cwd)
            .env_clear()
            .envs(
                template
                    .get_envs()
                    .filter_map(|(name, value)| value.map(|value| (name, value))),
            )
            .args(self.globals())
            .args(self.target.iter().flat_map(|target| ["-C", target.as_str()]))
            .args(PROBE_OPTIONS)
            .args(args)
            .stdin(Stdio::null())
            .stdout(out_writer)
            .stderr(err_writer)
            .process_group(0);
        let (sender, events) = std::sync::mpsc::channel();
        let scope = tracing.scope(*run, Some(*invocation)).map_err(unprobed)?;
        let child = {
            let _scope = scope.enter();
            tracing.spawn(
                command,
                Box::new(move |event| {
                    let _ = sender.send(event);
                }),
            )
        }
        .map_err(unprobed)?;
        let group = i32::try_from(child.pid).map_err(unprobed)?;
        let (status, stdout, stderr) = std::thread::scope(|threads| {
            let stdout = threads.spawn(|| read_all(out));
            let stderr = threads.spawn(|| read_all(err));
            let status = loop {
                match events.recv() {
                    Ok(ChildEvent::Exited(status)) => break Some(status),
                    // SAFETY: killpg takes integer arguments; the probe leads its own group,
                    // which its tracer has not reaped while it is stopped.
                    Ok(ChildEvent::Stopped) => unsafe {
                        libc::killpg(group, libc::SIGKILL);
                    },
                    Err(_) => break None,
                }
            };
            (status, stdout.join(), stderr.join())
        });
        child.wait_observed().map_err(unprobed)?;
        let status = status.ok_or_else(|| unprobed("its tracer reported no status"))?;
        let output = |joined: std::thread::Result<std::io::Result<Vec<u8>>>| {
            joined
                .map_err(|_| unprobed("an output reader panicked"))?
                .map_err(unprobed)
        };
        Ok(Probe {
            status: status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(FATAL),
            stdout: output(stdout)?,
            stderr: output(stderr)?,
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
    fn contain(&mut self, cwd: &Path, bound: &Path) -> Result<Option<Layout>, String> {
        let outside = |path: &Path| !physical(path).starts_with(bound);
        let Some(subcommand) = self.subcommand().filter(|word| !word.starts_with('-')) else {
            return self.layout();
        };
        if matches!(subcommand, "init" | "clone") {
            let target = destination(
                subcommand,
                self.argv.get(self.globals + 2..).unwrap_or_default(),
                cwd,
            )?;
            let template = self.template.as_ref().ok_or(UNPREPARED)?;
            let mut writes: Vec<PathBuf> = Vec::new();
            for name in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_COMMON_DIR",
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
            ] {
                let value = template
                    .get_envs()
                    .find(|(variable, _)| *variable == name)
                    .and_then(|(_, value)| value)
                    .filter(|value| !value.is_empty());
                if let Some(value) = value {
                    let value = value
                        .to_str()
                        .ok_or("git: repository environment path is not UTF-8")?;
                    writes.push(gitcmd::resolve(cwd, value));
                }
            }
            writes.extend(["--git-dir", "--work-tree"].into_iter().filter_map(|name| {
                global_path(self.globals(), name).map(|value| gitcmd::resolve(cwd, value))
            }));
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
            let layout = self.layout()?;
            return Ok(layout.filter(|layout| {
                let root = if target.bare {
                    Some(&layout.git_dir)
                } else {
                    layout.worktree.as_ref()
                };
                redirected || root.is_some_and(|root| physical(root) == physical(&target.path))
            }));
        }
        let Some(layout) = self.layout()? else {
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
    fn layout(&self) -> Result<Option<Layout>, String> {
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
            ])?;
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
            .probe(&["rev-parse", "--show-cdup", "--show-toplevel"])?;
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

    /// Reads the repository state the attribution of this invocation needs.
    ///
    /// `layout` is the repository [`Self::contain`] already found for the reading before the
    /// run; the reading after it looks again, because the run may have created or moved one.
    fn observe(&self, layout: Option<Layout>, before: bool) -> Result<Observation, String> {
        let layout = if before { layout } else { self.layout()? };
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
            observation.index = parse_index(&self.probe(&everything)?.output("the index")?);
        }
        let action = &self.action;
        if matches!(action, Some(GitAction::Commit { .. })) || self.unknown() {
            observation.head = self.head()?;
        }
        if before && matches!(action, Some(GitAction::Unstage)) {
            observation.staged = if self.head()?.is_some() {
                let staged = self
                    .probe(&["diff-index", "--cached", "--name-only", "-z", "HEAD", "--"])?;
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
    fn head(&self) -> Result<Option<Vec<u8>>, String> {
        let head = self
            .probe(&["rev-parse", "-q", "--verify", "HEAD^{commit}"])?;
        Ok((head.status == 0).then(|| head.stdout.trim_ascii_end().to_vec()))
    }

    /// What the run did, as git actions on snapshot-relative paths, and the repository metadata
    /// its window's writes belong to.
    ///
    /// # Errors
    ///
    /// Fails with a diagnostic when a fact the attribution needs cannot be read, or when the run
    /// changed staged or committed state that its classification cannot name.
    fn attribute(
        &self,
        before: &Observation,
        window: &[PathBuf],
        bound: &Path,
    ) -> Result<Attribution, String> {
        let after = self.observe(None, false)?;
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
            .transitions(before, &after, &changed, &written)?
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
    fn transitions(
        &self,
        before: &Observation,
        after: &Observation,
        changed: &BTreeSet<&Vec<u8>>,
        written: &BTreeSet<Vec<u8>>,
    ) -> Result<Vec<(GitAction, Vec<u8>)>, String> {
        let mut requests: Vec<(GitAction, Vec<u8>)> = Vec::new();
        let mut request = |action: GitAction, path: &[u8]| requests.push((action, path.to_vec()));
        match &self.action {
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
                return self.commits(before, after, changed, written);
            }
            Some(GitAction::Checkout) => return self.settle(&after.index, changed, written),
            Some(GitAction::Edit) if !self.unknown() => {
                return self.settle(&after.index, changed, written);
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
                let moved_head = self.unknown() && before.head != after.head;
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
    fn commits(
        &self,
        before: &Observation,
        after: &Observation,
        changed: &BTreeSet<&Vec<u8>>,
        written: &BTreeSet<Vec<u8>>,
    ) -> Result<Vec<(GitAction, Vec<u8>)>, String> {
        let committed = match (&before.head, &after.head) {
            (_, None) => Vec::new(),
            (Some(old), Some(new)) if old == new => Vec::new(),
            (old, Some(new)) => self.committed(old.as_deref(), new)?,
        };
        let mut requests: Vec<(GitAction, Vec<u8>)> = Vec::new();
        if let (Some(new), false) = (&after.head, committed.is_empty()) {
            let message = self.message(new)?;
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
    fn settle(
        &self,
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
                ])?
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
    fn committed(
        &self,
        old: Option<&[u8]>,
        new: &[u8],
    ) -> Result<Vec<CommittedEntry>, String> {
        let new = object_name(new)?;
        let Some(old) = old else {
            let listing = self
                .probe(&["ls-tree", "-r", "-z", "--full-tree", new])?
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
            .probe(&["diff-tree", "-r", "-z", "--no-renames", old, new])?
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
    fn message(&self, commit: &[u8]) -> Result<String, String> {
        let commit = object_name(commit)?;
        let object = self
            .probe(&["cat-file", "commit", commit])?
            .output("the commit")?;
        let body = object
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .map_or(&[][..], |at| &object[at + 2..]);
        let body = body.strip_suffix(b"\n").unwrap_or(body);
        Ok(String::from_utf8_lossy(body).into_owned())
    }

    /// Whether this is a form the grammar could not decide — classified as an edit — rather than
    /// one whose edit is a known outcome, like a popped stash.
    fn unknown(&self) -> bool {
        matches!(&self.action, Some(GitAction::Edit)) && self.subcommand() != Some("stash")
    }
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
