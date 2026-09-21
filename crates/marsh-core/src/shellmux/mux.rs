//! The multiplexer: one seed, many jobs, one `marsh::Shell` per job.
//!
//! One [`ShellMux`] owns a seed-level [`MarshExecutor`] — the seed's exclusive lease and its
//! recovered write-ahead log — and one [`crate::Shell`] per job, each over a snapshot of its own
//! taken for that job's principal. A job's line is that shell's run: the snapshot is refreshed
//! from the seed when another principal has published, the line runs, and the gate translates,
//! checks and publishes or discards it. Everything atomic about a command is the shell's; the mux
//! never snapshots, diffs, checks or publishes.
//!
//! What the mux owns is the multiplexing: the streams every job runs attached to, the job table
//! and the names it draws from, the pumps that keep every job draining whether or not anyone is
//! looking at it, and the delivery of all of that to one frontend.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use brush_builtins::BuiltinSet;
use brush_core::openfiles::OpenFile;
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, ShellFd, ShellVariable};
use marsh_instrument::SpawnRecord;
use tokio::sync::Notify;

use crate::policy::Event;
use crate::shellmux::error::MuxError;
use crate::shellmux::frontend::{FrontendEvent, ShellFrontend, lock_frontend, notify};
use crate::shellmux::ids::{JobDir, ShellId, SnapshotUid};
use crate::shellmux::jobs::{Background, JobTable, validate_size};
use crate::shellmux::types::{ExecutorInfo, MuxProfile};
use crate::{MarshExecutor, MarshShellExtensions, PolicyValidator};

/// Fixed timestamp used for every commit a job's `git` builtin produces.
///
/// Commit hashes are a function of tree, parents, message, author and committer — including their
/// timestamps. Pinning the timestamp makes a published history reproducible: replaying the same
/// commands serially yields byte-identical commit objects, which is what lets a concurrent run be
/// compared against its serial ground truth by `rev-parse HEAD`.
///
/// The value is git's raw `<epoch> <±HHMM>` date form (the same instant as
/// `2005-04-07T22:13:13 +0000`), which is what the `git` builtin parses out of the environment.
const FIXED_GIT_DATE: &str = "1112911993 +0000";

/// One sandbox: a job's identity, the directory it works in, and the snapshot its lines run in.
///
/// A sandbox outlives the commands that run in it. Its snapshot is the job's own for the job's
/// whole life — [`crate::Shell::run`] retakes it from the seed whenever another principal has
/// published — and it is reclaimed only when the job closes.
///
/// The [`uid`](Self::uid) is the generation marker: a job name can be reused, a snapshot id never
/// is, so a pair of them is what a retained handle is checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    /// The job's identity, which *is* its principal.
    pub id: ShellId,
    /// Seed-relative directory its commands start in; the seed root when empty.
    pub dir: JobDir,
    /// Short id naming its snapshot under `snap/`.
    pub uid: SnapshotUid,
}

/// A capability-gated shell multiplexer over one btrfs seed.
///
/// Built from one seed-level [`MarshExecutor`], the [`PolicyValidator`] every job's lines are
/// judged against, one [`MuxProfile`] every shell it builds is built from, and the
/// [`ShellFrontend`] whose default geometry terminal jobs open at and whose callbacks every job's
/// bytes, results and table changes reach.
pub struct ShellMux {
    /// The one profile every shell of this mux is built from: seeded variables and the extra
    /// builtins registered before attachment.
    profile: MuxProfile,
    /// The committed capability history every job's lines are checked against.
    validator: Arc<Mutex<PolicyValidator>>,
    /// The open jobs, the name series they draw from, the selected job, the unfinished commands
    /// and the default geometry.
    ///
    /// Never held across a shell build, a line, a callback or a reclamation.
    pub(crate) jobs: Mutex<JobTable>,
    /// Announces that a launch finished publishing, so a waiting `switch` stops polling.
    pub(crate) launched: Notify,
    /// The long-lived tasks this mux owns, joined by shutdown.
    pub(crate) tasks: Mutex<Background>,
    /// The runtime this mux was built on, and the only one its tasks are created on.
    ///
    /// Captured once, at construction, rather than read back from `Handle::current()` wherever a
    /// task happens to be created. A job's launch, its lifecycle, its byte pumps, the task one
    /// line runs on and a builtin's native workers all outlive the call that started them, so
    /// creating them on the caller's runtime would tie a job's whole future to whoever happened
    /// to ask for it: a status thread with a throwaway executor, another runtime, or a detached
    /// queue. Dropping that caller's runtime would then cancel the job's pumps and drop the
    /// reactor its descriptors are registered with — a live job whose output silently stops.
    ///
    /// [`CommandContext`](crate::shellmux::CommandContext) already carries this handle for the
    /// same reason; this is the same guarantee for the tasks the mux creates itself.
    pub(crate) runtime: tokio::runtime::Handle,
    /// Serializes the physical application of terminal geometry.
    ///
    /// The table decides what the size *is*; this decides the order the ioctls happen in. Without
    /// it two concurrent resizes could apply in the opposite order to the one the table settled
    /// on, leaving the terminal disagreeing with every view of it.
    pub(crate) resize_lock: tokio::sync::Mutex<()>,
    /// The command identity series. Monotonic for this mux's whole life.
    pub(crate) command_counter: AtomicU64,
    /// The user interface every job's bytes, results and table changes are delivered to.
    ///
    /// The original allocation, stored behind the trait object it was coerced to: the mux itself
    /// is not generic, because a job pump that carried the frontend's concrete type would make
    /// every internal signature depend on it.
    ///
    /// Before the executor, so a frontend's retained handles are released before the session lease
    /// is.
    frontend: Arc<Mutex<dyn ShellFrontend>>,
    /// The seed-level executor: the seed's lease and its recovered log.
    ///
    /// Deliberately private and never handed out: it is both an ungated spawner and a publication
    /// capability. [`Self::executor_info`] answers the questions a caller legitimately has about
    /// it without granting either.
    ///
    /// Last, as the old crate had it: every job's snapshot holds its own clone of the session, so
    /// the lease outlives every job whatever the drop order turns out to be.
    executor: MarshExecutor,
}

impl std::fmt::Debug for ShellMux {
    /// Names what the mux is over and how much it holds — never the variables it seeds shells
    /// with. Those are the embedding application's, frequently carry credentials, and a debug
    /// print of a daemon is not where they belong.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellMux")
            .field("seed", &self.executor.seed())
            .field("jobs", &self.jobs().len())
            .finish()
    }
}

impl ShellMux {
    /// Opens the mux over `executor`'s seed, with `profile` as the one shell profile it builds
    /// every job from.
    ///
    /// `executor` already holds the seed's exclusive lease and has already recovered its log: a
    /// competing marsh has failed by the time this is called, and there is nothing left here to
    /// sweep. `validator` is the history every job's lines are judged against. `frontend` is read
    /// for its default geometry before any job exists, and is bound to the finished mux and told
    /// its first state before this returns.
    ///
    /// `profile` is frozen here. Its extra builtins are registered on every shell this mux builds,
    /// before attachment, so the process-wide instrumentation covers all of them and every
    /// attached shell holds the identical builtin set — panes, popups and hidden helper jobs
    /// alike. Registering a builtin on one kind of job and not another would break that
    /// installation for every shell in the process.
    ///
    /// Must be called from within a Tokio runtime: that runtime is captured here and is the one
    /// every job's byte pump, every launch and every line this mux ever creates runs on,
    /// whichever thread or runtime later asks for the work.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::InvalidTerminalSize`] when a dimension of the frontend's geometry is
    /// zero.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, because there would be no runtime to bind the
    /// mux's tasks to. Every caller is already inside one: a mux that reached its first
    /// [`Self::spawn`] without one could not register a descriptor with a reactor either.
    pub fn new<V: ShellFrontend>(
        executor: MarshExecutor,
        validator: Arc<Mutex<PolicyValidator>>,
        profile: MuxProfile,
        frontend: Arc<Mutex<V>>,
    ) -> Result<Arc<Self>, MuxError> {
        let frontend: Arc<Mutex<dyn ShellFrontend>> = frontend;
        let (rows, cols) = lock_frontend(&frontend).size();
        // Before anything else: a geometry no job could use must not open a session at all.
        validate_size(rows, cols)?;
        // After the geometry check, so a refused geometry still fails before anything is bound.
        let runtime = tokio::runtime::Handle::current();
        // The mux answers `history()` before it has built a single job's shell, and every job it
        // does build is judged against this validator. Installing the seed's durable history here
        // means neither answer nor verdict can depend on whether a job happened to start first.
        executor.rehydrate(&validator);
        let mux = Arc::new(Self {
            profile,
            validator,
            jobs: Mutex::new(JobTable::new(rows, cols)),
            launched: Notify::new(),
            tasks: Mutex::new(Background::new()),
            runtime,
            resize_lock: tokio::sync::Mutex::new(()),
            command_counter: AtomicU64::new(1),
            frontend: Arc::clone(&frontend),
            executor,
        });
        // One guard for both: a frontend must never be told the table changed by a mux it has not
        // been given a reference to yet.
        let mut bound = lock_frontend(&frontend);
        bound.bind(Arc::downgrade(&mux));
        let _ = bound.update(FrontendEvent::Changed);
        drop(bound);
        Ok(mux)
    }

    /// Delivers one observation to this mux's frontend.
    ///
    /// Never called with a job-table or background-task lock held: a frontend callback is allowed
    /// to read the mux back, and holding one of those across it would deadlock the first frontend
    /// that does.
    ///
    /// The receipt a frontend may return is meaningful only for output, which the pumps deliver
    /// themselves; every other event ignores it here.
    pub(crate) fn announce(&self, event: FrontendEvent<'_>) {
        let _ = notify(&self.frontend, event);
    }

    /// This mux's frontend, for the pumps that outlive the row they read.
    pub(crate) fn frontend(&self) -> Arc<Mutex<dyn ShellFrontend>> {
        Arc::clone(&self.frontend)
    }

    /// Releases the frontend's reference to this mux, for shutdown.
    pub(crate) fn detach(&self) {
        lock_frontend(&self.frontend).bind(Weak::new());
    }

    /// What this mux will say about the executor it was built over.
    ///
    /// Metadata, not capability: the executor itself stays inside, because handing one out would
    /// hand out an ungated spawner and the ability to publish without a gate.
    #[must_use]
    pub fn executor_info(&self) -> ExecutorInfo {
        ExecutorInfo {
            seed: self.executor.seed().map(Path::to_path_buf),
            // The directory every job's snapshot lives in, not a snapshot of the mux's own: a mux
            // is built over a seed-level executor, which has none. A caller resolving a path that
            // is already inside some job's snapshot needs the shared parent to strip.
            snapshot_parent: self.executor.snapshot_dir(),
            uid: self.executor.uid().map(ToString::to_string),
            principal: self.executor.principal().cloned().map(ShellId::from),
            recovery_required: self.executor.recovery_required(),
        }
    }

    /// Whether the session is waiting for a write-ahead log replay.
    pub(crate) fn executor_recovery_required(&self) -> bool {
        self.executor.recovery_required()
    }

    /// Whether every shell this mux builds carries the builtin `name`.
    ///
    /// The profile is frozen, so this is a property of the mux rather than of any one job: a host
    /// can check that its support builtins were registered before it opens a single pane.
    #[must_use]
    pub fn has_builtin(&self, name: &str) -> bool {
        self.profile.builtins.contains_key(name)
            || crate::shell::builtins::all::<MarshShellExtensions>().contains_key(name)
    }

    /// The seed-relative directory a job starts in when none was named: where marsh was launched.
    ///
    /// A job directory is a *label* in the `sd NAME DIR` grammar and in the console prompt — a
    /// `/`-joined string the user types — which is why this is a [`JobDir`] and not a path. The
    /// empty label is the seed root, which is what `cwd == seed` yields. A `cwd` outside the seed
    /// cannot happen, since the seed was discovered from it, and falls back to the seed root; a
    /// detached mux has no seed at all, and answers the same.
    #[must_use]
    pub fn default_dir(&self, cwd: &Path) -> JobDir {
        let Some(seed) = self.executor.seed() else {
            return JobDir::default();
        };
        cwd.strip_prefix(seed).map_or_else(
            |_| JobDir::default(),
            |relative| {
                JobDir::from(
                    relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                )
            },
        )
    }

    /// The committed capability history, in grant order.
    #[must_use]
    pub fn history(&self) -> Vec<Event> {
        self.validator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .history()
            .to_vec()
    }

    /// Creates a sandbox for `id`, rooted at the seed-relative `dir`, and takes its snapshot.
    ///
    /// `dir` must resolve inside the seed, component-wise; `..` that escapes it, and a path that
    /// does not exist in the seed, are both refused.
    ///
    /// The snapshot is taken here rather than at the first command because it *is* the job: one
    /// snapshot per principal, retaken from the seed by [`crate::Shell::run`] whenever another
    /// principal has published.
    ///
    /// # Errors
    ///
    /// Fails when `dir` escapes the seed or names nothing, or when the snapshot cannot be taken.
    pub(crate) fn new_sandbox(
        &self,
        id: &ShellId,
        dir: &str,
    ) -> Result<(Sandbox, MarshExecutor), MuxError> {
        let relative = seed_relative(dir).ok_or_else(|| MuxError::SandboxDir {
            path: PathBuf::from(dir),
            reason: "escapes the seed".to_string(),
        })?;
        if let Some(seed) = self.executor.seed()
            && !seed.join(&relative).is_dir()
        {
            return Err(MuxError::SandboxDir {
                path: PathBuf::from(dir),
                reason: "no such directory in the seed".to_string(),
            });
        }

        let executor = self.executor.snapshot(id.principal().clone())?;
        let uid = SnapshotUid::from(executor.uid().unwrap_or_default());
        Ok((
            Sandbox {
                id: id.clone(),
                dir: JobDir::from(relative),
                uid,
            },
            executor,
        ))
    }

    /// Builds one job's shell: seeded, gated, with no profile or rc, over `fds`.
    ///
    /// The shell is the job's own [`crate::Shell`] over its own snapshot, so every line it runs is
    /// staged, checked and published on its own. `executor` is the attached executor
    /// [`Self::new_sandbox`] produced; the same clone is what the brush shell spawns through, which
    /// is what makes its spawn records the job's.
    ///
    /// `environment`, when given, *replaces* the profile's seeded variables for this job. It does
    /// not merge with them and it does not resurrect a variable the caller unset. Marsh's own
    /// principal and git identity are reapplied afterwards either way, because they are the
    /// shell's identity rather than the caller's configuration.
    ///
    /// The profile's extra builtins are registered **before** [`crate::Shell::attach`], so the
    /// instrumentation that attach installs covers them exactly as it covers `git` and `exec`.
    ///
    /// For a terminal job, standard input is the job's pseudoterminal and
    /// `external_cmd_leads_session` is set, so each external command becomes a session leader
    /// owning that terminal: Ctrl-C at the pty reaches the command, and full-screen programs work.
    /// `monitor` is enabled so an external gets its own process group first, which is what makes
    /// `kill(-pgid, …)` a whole-command kill.
    ///
    /// # Errors
    ///
    /// Fails when the shell could not be built, claimed, or moved into the job's directory.
    pub(crate) async fn build_shell(
        &self,
        executor: &MarshExecutor,
        sandbox: &Sandbox,
        fds: HashMap<ShellFd, OpenFile>,
        environment: Option<brush_core::env::ShellEnvironment>,
    ) -> Result<Arc<crate::Shell>, MuxError> {
        let mut builder = brush_core::Shell::builder_with_extensions::<MarshShellExtensions>()
            .external_command_spawner(executor.clone())
            .interactive(false)
            .no_editing(true)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .builtins(brush_builtins::default_builtins(BuiltinSet::BashMode))
            .fds(fds)
            .external_cmd_leads_session(true)
            .enable_option("monitor");
        let variables = environment.as_ref().unwrap_or(&self.profile.environment);
        for (name, variable) in variables.iter() {
            builder = builder.var(name.clone(), variable.clone());
        }
        for (name, value) in git_env(&sandbox.id, &sandbox.uid) {
            let mut variable = ShellVariable::new(&value);
            variable.export();
            builder = builder.var(name, variable);
        }
        // Boxed on purpose. `build` is the deepest await in the managed chain — the brush shell
        // builder, its option defaults and its interpreter setup all nest inside it — and its
        // state machine is stored inline in whatever future awaits it. That makes every caller's
        // frame grow by the size of this one, all the way up to the thread that blocks on it, and
        // in an unoptimised build that is enough to overflow an ordinary thread stack. One
        // allocation per shell moves it to the heap and flattens the chain for every caller.
        let mut shell = Box::pin(builder.build()).await?;
        // Before `attach`: a builtin registered afterwards runs uninstrumented, and a shell whose
        // builtin set differs from another attached shell's breaks the process-wide installation
        // for both.
        for (name, registration) in &self.profile.builtins {
            shell.register_builtin(name.clone(), registration.clone());
        }
        let shell = crate::Shell::attach(executor.clone(), Arc::clone(&self.validator), shell)?;
        // `attach` started the shell at the snapshot root and exported `SNAPSHOT_ROOT_VAR`; the job
        // then moves into the directory it was opened for.
        let root = executor.snapshot_root().unwrap_or_else(|| Path::new(""));
        shell
            .shell_ref()
            .lock()
            .await
            .set_working_dir(root.join(sandbox.dir.as_str()))?;
        Ok(Arc::new(shell))
    }
}

/// Kills every process a job's shell spawned since `mark`.
///
/// Each external led its own session, so its pid is also its process-group id; later stages of a
/// pipeline joined the first's group. A record whose process is already gone — or which never led
/// a group — answers `ESRCH`, which is not a failure: the point is that nothing of the line is
/// left running.
///
/// A builtin-only line has no process to signal at all, which is why [`ShellMux::stop`] with
/// `force` can return `Ok(())` having sent nothing.
///
/// # Errors
///
/// Fails with [`MuxError::JobTermination`] carrying the last errno that was not `ESRCH`; every
/// record is attempted first, so one unkillable process does not skip the rest.
pub(crate) fn kill_since(
    executor: &MarshExecutor,
    mark: usize,
    job: &ShellId,
) -> Result<(), MuxError> {
    let records = executor.spawn_records();
    let mut failure: Option<std::io::Error> = None;
    for record in records.get(mark..).unwrap_or_default() {
        let SpawnRecord::Spawned {
            pid: Some(pid), ..
        } = record
        else {
            continue;
        };
        let Ok(pid) = libc::pid_t::try_from(*pid) else {
            continue;
        };
        // SAFETY: `kill` signals a process group by the negation of its id and has no
        // memory-safety requirements.
        if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                failure = Some(error);
            }
        }
    }
    match failure {
        None => Ok(()),
        Some(source) => Err(MuxError::JobTermination {
            job: job.clone(),
            source,
        }),
    }
}

/// Normalizes a seed-relative directory, or `None` when it escapes the seed.
///
/// Purely lexical, and deliberately so: the seed's own layout decides what exists, and a `..` that
/// climbs past the root must be refused before any path is built from it. A leading `/` names the
/// seed root rather than the filesystem's, because everything here is seed-relative.
fn seed_relative(dir: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for component in dir.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// The deterministic git environment every job's commands run with.
///
/// The authored name is the job's, because that is the identity a reader recognizes in `git log`.
/// The address carries the job's snapshot id as well, because that is the identity the seed's own
/// log records a grant against: a name comes back — a reused pane index, a restarted daemon
/// numbering from one — and a snapshot id does not, so a commit that named only the job would not
/// say which principal actually earned the capability. A detached job has no snapshot and gets the
/// bare name.
///
/// Dates are pinned and configuration files are cut off (`GIT_CONFIG_NOSYSTEM`,
/// `GIT_CONFIG_GLOBAL=/dev/null`) so that a command's effect depends on the seed and the command
/// alone — never on the host user's git configuration. The `git` builtin refuses to commit at all
/// without `GIT_AUTHOR_*` and `GIT_COMMITTER_*`, so this is not decoration.
fn git_env(principal: &ShellId, uid: &SnapshotUid) -> Vec<(String, String)> {
    let name = principal.to_string();
    // The address, unlike the name, has to be one word: a job name may hold spaces, and every
    // character outside an address's alphabet becomes a hyphen so the identity stays well formed
    // whatever the job was called.
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '.' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    let email = if uid.is_empty() {
        format!("{slug}@marsh.local")
    } else {
        format!("{slug}.{uid}@marsh.local")
    };
    [
        ("GIT_AUTHOR_NAME", name.clone()),
        ("GIT_AUTHOR_EMAIL", email.clone()),
        ("GIT_AUTHOR_DATE", FIXED_GIT_DATE.to_string()),
        ("GIT_COMMITTER_NAME", name),
        ("GIT_COMMITTER_EMAIL", email),
        ("GIT_COMMITTER_DATE", FIXED_GIT_DATE.to_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
        ("GIT_PAGER", "cat".to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("LC_ALL", "C".to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellmux::{JobIo, SpawnOptions};

    /// A sandbox's directory is user input that becomes a path under the seed, so the one thing it
    /// must never do is name something outside it.
    #[test]
    fn a_sandbox_directory_cannot_climb_out_of_the_seed() {
        assert_eq!(seed_relative(""), Some(String::new()));
        assert_eq!(seed_relative("."), Some(String::new()));
        assert_eq!(seed_relative("./src"), Some("src".to_string()));
        assert_eq!(seed_relative("/src/"), Some("src".to_string()));
        assert_eq!(seed_relative("deep/../src"), Some("src".to_string()));
        assert_eq!(seed_relative(".."), None);
        assert_eq!(seed_relative("src/../.."), None);
    }

    /// A job name becomes the commit author, and a job name may hold spaces — but an address may
    /// not, and libgit2 refuses the identity rather than the commit. The address also carries the
    /// snapshot id, so a commit records the identity the seed's log recorded the grant against and
    /// not only the reusable name.
    #[test]
    fn a_principals_address_is_one_word_whatever_the_principal_is() {
        let env: HashMap<String, String> =
            git_env(&ShellId::from("a long name"), &SnapshotUid::from("ab12cd34"))
                .into_iter()
                .collect();
        assert_eq!(env["GIT_AUTHOR_NAME"], "a long name");
        assert_eq!(env["GIT_AUTHOR_EMAIL"], "a-long-name.ab12cd34@marsh.local");
        assert_eq!(
            git_env(&ShellId::from("main"), &SnapshotUid::from("ab12cd34"))
                .into_iter()
                .find(|(key, _)| key == "GIT_COMMITTER_EMAIL")
                .map(|(_, value)| value),
            Some("main.ab12cd34@marsh.local".to_string()),
            "a name that was already one word is untouched"
        );
        assert_eq!(
            git_env(&ShellId::from("main"), &SnapshotUid::default())
                .into_iter()
                .find(|(key, _)| key == "GIT_COMMITTER_EMAIL")
                .map(|(_, value)| value),
            Some("main@marsh.local".to_string()),
            "a job with no snapshot has no durable identity to record"
        );
    }

    /// A frontend that answers a geometry and records nothing.
    ///
    /// The test below is about where tasks are created, not about what is delivered, and a
    /// recorder would only add a channel whose drain could be mistaken for the thing under test.
    struct Silent {
        /// The geometry the mux reads once, before any job exists.
        geometry: (u16, u16),
    }

    impl ShellFrontend for Silent {
        fn new(rows: u16, cols: u16) -> Self {
            Self {
                geometry: (rows, cols),
            }
        }

        fn size(&self) -> (u16, u16) {
            self.geometry
        }

        fn bind(&mut self, _mux: Weak<ShellMux>) {}

        fn update(
            &mut self,
            _event: FrontendEvent<'_>,
        ) -> Option<crate::shellmux::frontend::OutputReceipt> {
            None
        }
    }

    /// A job is a set of tasks, and they belong to the mux rather than to whoever asked for it.
    ///
    /// The caller here is a second runtime, which is the shape a status thread, a library
    /// consumer or a detached queue takes. Were the mux to create a job's lifecycle task with an
    /// ambient `tokio::spawn`, it would land on that caller — and be cancelled the moment the
    /// caller's runtime went away, leaving a job in the table that can never produce another byte.
    #[test]
    fn a_job_admitted_from_another_runtime_keeps_its_tasks_on_the_mux() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).expect("seed tree");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        filesystem.register(&seed);
        let executor = MarshExecutor::open_with(&seed, filesystem).expect("open the test seed");

        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the host runtime");
        let mux = host.block_on(async {
            ShellMux::new(
                executor,
                Arc::new(Mutex::new(PolicyValidator::new())),
                MuxProfile::default(),
                Arc::new(Mutex::new(Silent::new(24, 80))),
            )
            .expect("build the mux")
        });

        let idle = host.metrics().num_alive_tasks();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the caller's runtime");
        let job = caller
            .block_on(mux.spawn(
                "",
                None,
                None,
                SpawnOptions {
                    io: JobIo::Pipes,
                    environment: None,
                },
            ))
            .expect("admit the job from the caller's runtime");

        assert_eq!(
            caller.metrics().num_alive_tasks(),
            0,
            "the caller's runtime was given none of the job's tasks"
        );
        assert!(
            host.metrics().num_alive_tasks() > idle,
            "the job's lifecycle and its byte pumps were created on the mux's own runtime"
        );

        drop(caller);
        host.block_on(async {
            mux.stop(&job, true).await.expect("stop the job");
            mux.shutdown().await.expect("shut the mux down");
        });
    }
}
