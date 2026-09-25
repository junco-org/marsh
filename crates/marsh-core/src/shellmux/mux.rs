//! The multiplexer: many seeds, many jobs, one `marsh::Shell` per job.
//!
//! One [`ShellMux`] owns one [`crate::Shell`] per job, each over a snapshot taken for that job's
//! principal out of the seed that job's own starting directory lies in. A job's line is that
//! shell's run: the snapshot is refreshed from the seed when another principal has published, the
//! line runs, and the gate translates, checks and publishes or discards it. Everything atomic
//! about a command is the shell's; the mux never snapshots, diffs, checks or publishes.
//!
//! A seed is opened the first time a shell asks for one, and kept until the mux is dropped: the
//! lease, the recovered log and the live capability history all belong to the seed rather than to
//! any job over it. Two seeds share nothing but the builtin recorder, which is process-global and
//! therefore has to be one.
//!
//! What the mux owns is the multiplexing: the streams every shell runs attached to, the registry
//! and the names it draws from, the pumps that keep every job draining whether or not anyone is
//! looking at it, and the delivery of all of that to one frontend.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use brush_builtins::BuiltinSet;
use brush_core::openfiles::OpenFile;
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, ShellFd, ShellVariable};
use tokio::sync::Notify;

use crate::policy::Event;
use crate::shellmux::error::MuxError;
use crate::shellmux::frontend::{FrontendEvent, ShellFrontend, lock_frontend, notify};
use crate::shellmux::ids::{JobDir, ShellId, SnapshotUid};
use crate::shellmux::jobs::{Background, ShellRegistry, validate_size};
use crate::shellmux::types::{MuxProfile, SeedInfo};
use crate::{MarshError, MarshExecutor, MarshShellExtensions, PolicyValidator};

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

/// One sandbox: a job's identity, the seed it publishes into, the directory it works in, and the
/// snapshot its lines run in.
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
    /// The canonical seed this job's lines publish into.
    ///
    /// One mux hosts jobs over several seeds, so this is a property of the job and not of the
    /// mux. It is also the key every metadata and history query about this job is asked with.
    pub seed: PathBuf,
    /// Seed-relative directory its commands start in; the seed root when empty.
    pub dir: JobDir,
    /// Short id naming its snapshot under `snap/`.
    pub uid: SnapshotUid,
}

/// Everything one opened seed owns, shared by every job over it.
///
/// Opened once, on the first shell whose starting directory lies in the seed, and kept until the
/// mux is dropped: the lease, the recovered log and the live capability history outlive the jobs
/// that earned them, so a second job on the same seed must find the first one's claims rather
/// than a fresh history.
pub(crate) struct SeedState {
    /// The seed-level executor: this seed's lease and its recovered log.
    ///
    /// Deliberately never handed out: it is both an ungated spawner and a publication capability.
    pub(crate) executor: MarshExecutor,
    /// The committed capability history every job over *this* seed is checked against.
    ///
    /// One per seed, never one per mux: a policy resource carries no seed identity, so a shared
    /// validator would let one seed's `src/file` answer for another's.
    pub(crate) validator: Arc<Mutex<PolicyValidator>>,
    /// `<state>/snap`: the directory whose immediate children are this seed's per-job snapshots.
    ///
    /// Cached before the layer is consumed, so a path spelled from inside a live job's snapshot
    /// can be recognized without unwrapping the seed-level executor's optional snapshot API.
    pub(crate) snapshot_parent: PathBuf,
}

/// A capability-gated shell multiplexer over as many btrfs seeds as its shells ask for.
///
/// Built from one [`MuxProfile`] every shell it builds is built from, the [`ShellFrontend`] whose
/// default geometry terminal jobs open at and whose callbacks every job's bytes, results and
/// table changes reach, and the snapshot backend seeds are reached through. It leases nothing
/// until a shell names a directory: the seed that directory lies in is discovered and opened
/// then, and kept for the rest of the mux's life.
pub struct ShellMux {
    /// The one profile every shell of this mux is built from: seeded variables and the extra
    /// builtins registered before attachment.
    profile: MuxProfile,
    /// The btrfs operations every seed this mux opens is reached through, real or faked.
    filesystem: Arc<dyn marsh_btrfs::Subvolumes>,
    /// The open shells by principal, the name series they draw from, the unfinished commands and
    /// the default geometry.
    ///
    /// Never held across a shell build, a line, a callback or a reclamation.
    pub(crate) shells: Mutex<ShellRegistry>,
    /// Announces that a launch finished publishing, so a waiting `wait_ready` stops polling.
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
    /// The live state decides what the size *is*; this decides the order the ioctls happen in.
    /// Without it two concurrent resizes could apply in the opposite order to the one the live
    /// state settled on, leaving the terminal disagreeing with every view of it.
    pub(crate) resize_lock: tokio::sync::Mutex<()>,
    /// The command identity series. Monotonic for this mux's whole life.
    pub(crate) command_counter: AtomicU64,
    /// The user interface every job's bytes, results and table changes are delivered to.
    ///
    /// The original allocation, stored behind the trait object it was coerced to: the mux itself
    /// is not generic, because a job pump that carried the frontend's concrete type would make
    /// every internal signature depend on it.
    ///
    /// Before the seeds, so a frontend's retained handles are released before the session leases
    /// are.
    frontend: Arc<Mutex<dyn ShellFrontend>>,
    /// Every seed this mux has opened, keyed by its canonical path.
    ///
    /// Populated lazily, by the first shell whose starting directory lies in each seed, and never
    /// pruned: a seed's lease, its live principal history and its poisoned recovery state have to
    /// outlive the jobs that produced them, so sequential jobs on one seed see one session.
    ///
    /// The lock is also the admission gate: lookup, open and insert happen under it, so two first
    /// shells racing onto one seed cannot both take its lease. Ordered after the shell registry
    /// wherever both are held.
    ///
    /// Last, as the old singular executor was: every job's snapshot holds its own clone of its
    /// session, so a lease outlives every job over it whatever the drop order turns out to be.
    seeds: Mutex<BTreeMap<PathBuf, Arc<SeedState>>>,
}

impl std::fmt::Debug for ShellMux {
    /// Names what the mux is over and how much it holds — never the variables it seeds shells
    /// with. Those are the embedding application's, frequently carry credentials, and a debug
    /// print of a daemon is not where they belong.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Its own statement, deliberately. A guard produced inside a chained `field` call lives
        // to the end of the whole formatter expression, so reading the shells below would hold
        // seed registry → wait for shells while a creation holds shells → waits for seeds.
        let seeds = self
            .seed_registry()
            .keys()
            .cloned()
            .collect::<Vec<PathBuf>>();
        formatter
            .debug_struct("ShellMux")
            .field("seeds", &seeds)
            .field("jobs", &self.jobs().len())
            .finish_non_exhaustive()
    }
}

impl ShellMux {
    /// Opens a mux that leases nothing yet, with `profile` as the one shell profile it builds
    /// every job from, over real btrfs.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::new_with`] does.
    ///
    /// # Panics
    ///
    /// Panics for the reason [`Self::new_with`] does.
    pub fn new<V: ShellFrontend>(
        profile: MuxProfile,
        frontend: Arc<Mutex<V>>,
    ) -> Result<Arc<Self>, MuxError> {
        Self::new_with(profile, frontend, Arc::new(marsh_btrfs::LibBtrfs))
    }

    /// The same, reaching seeds through `filesystem`.
    ///
    /// No seed is discovered, leased, recovered or rehydrated here: a mux hosts shells over as
    /// many seeds as their starting directories name, and each of those is found from the shell
    /// that asked for it. So a host may be built outside any subvolume and still admit a shell in
    /// a valid one later, and a seed's lease, log and validator are the affected
    /// [`open_shell`](Self::open_shell)'s failure rather than this constructor's.
    ///
    /// `frontend` is read for its default geometry before any job exists, and is bound to the
    /// finished mux and told its first state before this returns.
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
    /// [`Self::open_shell`] without one could not register a descriptor with a reactor either.
    pub fn new_with<V: ShellFrontend>(
        profile: MuxProfile,
        frontend: Arc<Mutex<V>>,
        filesystem: Arc<dyn marsh_btrfs::Subvolumes>,
    ) -> Result<Arc<Self>, MuxError> {
        let frontend: Arc<Mutex<dyn ShellFrontend>> = frontend;
        let (rows, cols) = lock_frontend(&frontend).size();
        // Before anything else: a geometry no job could use must not open a session at all.
        validate_size(rows, cols)?;
        // After the geometry check, so a refused geometry still fails before anything is bound.
        let runtime = tokio::runtime::Handle::current();
        let mux = Arc::new(Self {
            profile,
            filesystem,
            shells: Mutex::new(ShellRegistry::new(rows, cols)),
            launched: Notify::new(),
            tasks: Mutex::new(Background::new()),
            runtime,
            resize_lock: tokio::sync::Mutex::new(()),
            command_counter: AtomicU64::new(1),
            frontend: Arc::clone(&frontend),
            seeds: Mutex::new(BTreeMap::new()),
        });
        // One guard for both: a frontend must never be told the table changed by a mux it has not
        // been given a reference to yet.
        let mut bound = lock_frontend(&frontend);
        bound.bind(Arc::downgrade(&mux));
        let _ = bound.update(FrontendEvent::Changed);
        drop(bound);
        Ok(mux)
    }

    /// The seed registry, recovering a poisoned lock like the rest of this module.
    ///
    /// Acquired *after* the shell registry wherever both are held, never before it.
    fn seed_registry(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<SeedState>>> {
        self.seeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The seed a shell starting at `initial_dir` publishes into, opening it if this is its first
    /// shell.
    ///
    /// Returns the canonical starting directory, the canonical seed and the state every job over
    /// that seed shares. The canonical starting directory is discovery's own: recomputing it here
    /// would canonicalize twice, and using the caller's spelling would leave a symlinked path that
    /// does not strip against the seed.
    ///
    /// The request is discovered as given first, because discovery is what rejects a path that
    /// names no subvolume, a mount root, or something that is not a directory at all. Only its
    /// canonical result is then tested against this mux's live job snapshots: a job's snapshot is
    /// itself a subvolume, so a path inside one would otherwise make that snapshot a seed of its
    /// own — a second lease, a second history, and publications into a tree that is about to be
    /// reclaimed. A client whose shell has moved into a pane's snapshot hands over paths spelled
    /// exactly that way, and the place in the seed they stand for is rediscovered.
    ///
    /// Only snapshot parents are matched. An ordinary path under a known seed keeps the seed
    /// discovery found, because a nested subvolume under that seed is its own seed and must still
    /// win the nearest-ancestor walk.
    ///
    /// Lookup, open and insert happen under one acquisition of the registry lock, so two first
    /// shells racing onto one seed cannot both take its lease. An open that fails inserts nothing.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::Marsh`] when the directory names no seed, when the seed's lease is
    /// held elsewhere, or when its log cannot be recovered.
    pub(crate) fn seed_for(
        &self,
        initial_dir: &Path,
    ) -> Result<(PathBuf, PathBuf, Arc<SeedState>), MuxError> {
        let mut registry = self.seed_registry();
        // As given: remapping before this would let an existing *file* inside a snapshot stand
        // for a directory of the same name in the seed, silently starting the shell somewhere
        // the caller never named instead of failing the way discovery does.
        let (mut layer, mut canonical_initial_dir) =
            marsh_btrfs::PersistenceLayer::discover(initial_dir, self.filesystem.as_ref())
                .map_err(|error| MuxError::Marsh(MarshError::Btrfs(error)))?;
        if let Some(host) = Self::host_place(&registry, &canonical_initial_dir) {
            let (mapped, mapped_dir) =
                marsh_btrfs::PersistenceLayer::discover(&host, self.filesystem.as_ref())
                    .map_err(|error| MuxError::Marsh(MarshError::Btrfs(error)))?;
            layer = mapped;
            canonical_initial_dir = mapped_dir;
        }
        let seed = layer.seed.clone();
        if let Some(state) = registry.get(&seed) {
            // A hit reopens nothing: reopening would retake a lease this process already holds,
            // replay a log it already replayed, and sweep the snapshots its live jobs run in.
            let state = Arc::clone(state);
            drop(registry);
            return Ok((canonical_initial_dir, seed, state));
        }
        let snapshot_parent = layer.snap();
        let executor = MarshExecutor::open_discovered(layer, Arc::clone(&self.filesystem))?;
        // After the open, never before it: opening is what materializes the state tree, so a
        // canonicalization attempted earlier would fail on a seed being opened for the first time
        // and cache an unresolved spelling. Jobs canonicalize their own snapshot roots, so such a
        // prefix would never strip and every snapshot path would look like a brand new seed.
        let snapshot_parent = snapshot_parent.canonicalize()?;
        let validator = Arc::new(Mutex::new(PolicyValidator::new()));
        // Before publication: every job over this seed is judged against this validator, so the
        // grants its log already recorded must be in it before another thread can find the entry.
        executor.rehydrate(&validator);
        let state = Arc::new(SeedState {
            executor,
            validator,
            snapshot_parent,
        });
        registry.insert(seed.clone(), Arc::clone(&state));
        drop(registry);
        Ok((canonical_initial_dir, seed, state))
    }

    /// The host path `candidate` stands for when it lies inside a known seed's job snapshot.
    ///
    /// `<snapshot_parent>/<uid>/<rest>` names the same place in the seed as `<seed>/<rest>`. The
    /// uid component is required: the snapshot parent itself is not inside any job's tree.
    /// Prefixes are compared component-wise and the longest is taken first, so a seed whose state
    /// directory nests inside another's cannot be matched by the shorter one.
    fn host_place(
        registry: &BTreeMap<PathBuf, Arc<SeedState>>,
        candidate: &Path,
    ) -> Option<PathBuf> {
        // Canonical against canonical: `candidate` is discovery's own result and the snapshot
        // parent was resolved when its seed was opened, so a prefix differing only by a symlink
        // still strips.
        let mut matched: Option<(usize, PathBuf)> = None;
        for (seed, state) in registry {
            let Ok(inside) = candidate.strip_prefix(&state.snapshot_parent) else {
                continue;
            };
            let mut components = inside.components();
            // The uid, which is what makes this a *job's* tree rather than their shared parent.
            if components.next().is_none() {
                continue;
            }
            let depth = state.snapshot_parent.components().count();
            if matched.as_ref().is_none_or(|(best, _)| depth > *best) {
                matched = Some((depth, seed.join(components.as_path())));
            }
        }
        matched.map(|(_, host)| host)
    }

    /// What this mux will say about every seed it has opened, in canonical-path order.
    ///
    /// Metadata, not capability: the executors themselves stay inside, because handing one out
    /// would hand out an ungated spawner and the ability to publish without a gate. Empty before
    /// the first shell, and this never opens a seed to answer.
    #[must_use]
    pub fn seeds(&self) -> Vec<SeedInfo> {
        self.seed_registry()
            .iter()
            .map(|(seed, state)| SeedInfo {
                seed: seed.clone(),
                snapshot_parent: state.snapshot_parent.clone(),
                recovery_required: state.executor.recovery_required(),
            })
            .collect()
    }

    /// Whether `seed`'s session is waiting for a write-ahead log replay.
    ///
    /// `false` for a seed this mux never opened, which is the honest answer: nothing was ever
    /// published into it from here, so nothing of this process's is unrecovered.
    pub(crate) fn seed_recovery_required(&self, seed: &Path) -> bool {
        self.seed_registry()
            .get(seed)
            .is_some_and(|state| state.executor.recovery_required())
    }

    /// The committed capability history of `seed`, in grant order.
    ///
    /// `seed` is a canonical key, as [`Sandbox::seed`] and [`Self::seeds`] carry it. `None` says
    /// this mux has not opened that seed — which is a different answer from `Some(vec![])`, an
    /// opened seed that has granted nothing. Never opens a seed to answer.
    #[must_use]
    pub fn history(&self, seed: &Path) -> Option<Vec<Event>> {
        let state = Arc::clone(self.seed_registry().get(seed)?);
        let history = state
            .validator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .history()
            .to_vec();
        Some(history)
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

    /// Whether every shell this mux builds carries the builtin `name`.
    ///
    /// The profile is frozen, so this is a property of the mux rather than of any one job: a host
    /// can check that its support builtins were registered before it opens a single pane.
    #[must_use]
    pub fn has_builtin(&self, name: &str) -> bool {
        self.profile.builtins.contains_key(name)
            || crate::shell::builtins::all::<MarshShellExtensions>().contains_key(name)
    }

    /// Creates a sandbox for `id` at the canonical host directory `initial_dir`, inside `seed`,
    /// and takes its snapshot out of `state`.
    ///
    /// `initial_dir` and `seed` both come from [`Self::seed_for`], so the one is canonical and
    /// lies under the other by construction; the suffix between them is the job's directory label
    /// and the place its shell starts inside its own snapshot.
    ///
    /// The snapshot is taken here rather than at the first command because it *is* the job: one
    /// snapshot per principal, retaken from the seed by [`crate::Shell::run`] whenever another
    /// principal has published.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::SandboxDir`] when `initial_dir` is not inside `seed` or its suffix
    /// is not UTF-8, and with whatever taking the snapshot reported.
    pub(crate) fn new_sandbox(
        id: &ShellId,
        initial_dir: &Path,
        seed: PathBuf,
        state: &SeedState,
    ) -> Result<(Sandbox, MarshExecutor), MuxError> {
        let suffix = initial_dir
            .strip_prefix(&seed)
            .map_err(|_| MuxError::SandboxDir {
                path: initial_dir.to_path_buf(),
                reason: "outside discovered seed".to_owned(),
            })?;
        // A lossy conversion here would silently start the shell in a *different* directory from
        // the one that was named, and the label is what `sd` and the prompt then show.
        let relative = suffix.to_str().ok_or_else(|| MuxError::SandboxDir {
            path: initial_dir.to_path_buf(),
            reason: "directory is not UTF-8".to_owned(),
        })?;

        let executor = state
            .executor
            .snapshot_for(Some(id.principal().clone()), id.durable_name().cloned())?;
        // A mux's seeds are always real sessions, so a snapshot with no id is a broken executor
        // rather than the detached case — and a job whose uid were empty could not be told apart
        // from any other by a retained handle.
        let uid = SnapshotUid::from(executor.uid().ok_or(MarshError::NoSnapshot)?);
        Ok((
            Sandbox {
                id: id.clone(),
                seed,
                dir: JobDir::from(relative.to_owned()),
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
    /// is what makes its spawn records the job's. `validator` is its seed's own, shared with every
    /// other job over that seed and with no job over another.
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
        validator: Arc<Mutex<PolicyValidator>>,
        sandbox: &Sandbox,
        fds: HashMap<ShellFd, OpenFile>,
        environment: Option<brush_core::env::ShellEnvironment>,
    ) -> Result<Arc<crate::Shell>, MuxError> {
        let mut builder = brush_core::Shell::builder_with_extensions::<MarshShellExtensions>()
            .external_command_spawner(executor.clone())
            .do_not_inherit_env(environment.is_some())
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
        let shell = crate::Shell::attach(executor.clone(), validator, shell)?;
        // `attach` started the shell at the snapshot root and exported `SNAPSHOT_ROOT_VAR`; the job
        // then moves into the directory it was opened for. A mux job always has a snapshot —
        // `new_sandbox` refused it otherwise — so there is no rootless case to fall back to.
        let root = executor.snapshot_root().ok_or(MarshError::NoSnapshot)?;
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
    // The recorder is released before the first kill: signalling under it would hold every other
    // observer of this shell's spawn log for as long as the kernel takes.
    let mut failure: Option<std::io::Error> = None;
    for pid in executor.spawned_pids_since(mark) {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
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
/// alone — never on the host user's git configuration. With that configuration gone, git would
/// otherwise fall back to guessing an identity from the host's user and hostname, or refuse to
/// commit, so this is not decoration.
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
    use crate::policy::Principal;
    use crate::shellmux::{CommandOptions, JobIo, SpawnOptions};

    /// A job name becomes the commit author, and a job name may hold spaces — but an address may
    /// not, and git refuses a malformed identity rather than making the commit. The address also
    /// carries the snapshot id, so a commit records the identity the seed's log recorded the grant
    /// against and not only the reusable name.
    #[test]
    fn a_principals_address_is_one_word_whatever_the_principal_is() {
        let env: HashMap<String, String> = git_env(
            &ShellId::from("a long name"),
            &SnapshotUid::from("ab12cd34"),
        )
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

        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the host runtime");
        let mux = host.block_on(async {
            ShellMux::new_with(
                MuxProfile::default(),
                Arc::new(Mutex::new(Silent::new(24, 80))),
                filesystem,
            )
            .expect("build the mux")
        });

        let idle = host.metrics().num_alive_tasks();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the caller's runtime");
        let job = caller
            .block_on(mux.open_shell(
                &seed,
                None,
                SpawnOptions {
                    io: JobIo::Pipes,
                    ..SpawnOptions::default()
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
            job.stop(true).await.expect("stop the job");
            mux.shutdown().await.expect("shut the mux down");
        });
    }

    /// Opens a mux over `seed`, registered with a fresh copy-tree backend.
    ///
    /// The runtime is multi-threaded because a job's shell reaches a blocking boundary, and it is
    /// returned so the caller can drive admission and shut the mux down on it.
    fn opened(seed: &Path) -> (tokio::runtime::Runtime, Arc<ShellMux>) {
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        filesystem.register(seed);
        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the host runtime");
        let mux = host.block_on(async {
            ShellMux::new_with(
                MuxProfile::default(),
                Arc::new(Mutex::new(Silent::new(24, 80))),
                filesystem,
            )
            .expect("build the mux")
        });
        (host, mux)
    }

    /// A regular file in a live job's snapshot tree is refused, not mapped onto the seed root.
    ///
    /// The snapshot mapping exists so a shell whose directory is a pane's snapshot publishes into
    /// the seed instead of making that snapshot a seed of its own. It strips the snapshot parent
    /// and then the job's uid — so a plain file sitting where a job tree would be strips to
    /// nothing at all and names the seed root. Answering a caller who pointed at a file with a
    /// shell somewhere else entirely is the silent substitution discovery already refuses to
    /// make, which is why the request has to be discovered before it is mapped.
    #[test]
    fn a_file_in_the_snapshot_tree_never_stands_for_a_directory_in_the_seed() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).expect("seed tree");
        let (host, mux) = opened(&seed);

        let job = host
            .block_on(mux.open_shell(
                &seed,
                None,
                SpawnOptions {
                    io: JobIo::Pipes,
                    ..SpawnOptions::default()
                },
            ))
            .expect("admit the first job");

        let parent = mux
            .seeds()
            .first()
            .expect("one opened seed")
            .snapshot_parent
            .clone();
        let file = parent.join("not-a-job");
        std::fs::write(&file, b"whatever").expect("a plain file beside the job trees");

        let refused = host.block_on(mux.open_shell(
            &file,
            Some(Principal::from("b")),
            SpawnOptions {
                io: JobIo::Pipes,
                ..SpawnOptions::default()
            },
        ));
        assert!(
            matches!(
                refused,
                Err(MuxError::Marsh(MarshError::Btrfs(
                    marsh_btrfs::Error::SeedDir { ref path, .. }
                ))) if path == &file
            ),
            "a file is refused by name, not answered with the seed root: {refused:?}"
        );
        assert_eq!(mux.seeds().len(), 1, "the refusal opened nothing");

        host.block_on(async {
            job.stop(true).await.expect("stop the job");
            mux.shutdown().await.expect("shut the mux down");
        });
    }

    /// A job's own directory maps back to its seed even when the state root is reached through a
    /// symlink.
    ///
    /// The snapshot parent is derived before the state tree exists, so it can only be resolved
    /// once opening has materialized it. Resolving it any earlier caches the unresolved spelling,
    /// and since a job's shell reports a canonical directory, that prefix would never strip: the
    /// pane's own snapshot would be discovered as a brand new seed, taking a second lease and
    /// publishing into a tree that is about to be reclaimed.
    #[test]
    fn a_symlinked_state_root_still_maps_a_snapshot_back_to_its_seed() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).expect("seed tree");
        // Where the state actually lives, reached through the name the layer derives.
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("the real state tree");
        std::os::unix::fs::symlink(&store, root.join(marsh_btrfs::STATE_DIR))
            .expect("a symlinked state root");
        let (host, mux) = opened(&seed);

        let job = host
            .block_on(mux.open_shell(
                &seed,
                None,
                SpawnOptions {
                    io: JobIo::Pipes,
                    ..SpawnOptions::default()
                },
            ))
            .expect("admit the first job");

        let parent = mux
            .seeds()
            .first()
            .expect("one opened seed")
            .snapshot_parent
            .clone();
        assert!(
            parent.starts_with(&store),
            "the snapshot parent resolved through the symlink: {}",
            parent.display()
        );
        // What the job's own shell reports as its directory.
        let cwd = parent
            .join(job.sandbox().uid.as_str())
            .canonicalize()
            .expect("the job's snapshot exists");

        let second = host
            .block_on(mux.open_shell(
                &cwd,
                Some(Principal::from("b")),
                SpawnOptions {
                    io: JobIo::Pipes,
                    ..SpawnOptions::default()
                },
            ))
            .expect("admit a job from the first one's directory");
        assert_eq!(
            second.sandbox().seed,
            seed,
            "the snapshot named its seed, not itself"
        );
        assert_eq!(
            mux.seeds().len(),
            1,
            "no second lease was taken over a live job's snapshot"
        );

        host.block_on(async {
            second.stop(true).await.expect("stop the second job");
            job.stop(true).await.expect("stop the job");
            mux.shutdown().await.expect("shut the mux down");
        });
    }

    /// Opens a pipe shell named `name` over `seed`, runs `text` in it with its input already
    /// closed, and waits for the shell to close on that end of input — handing back the line's
    /// verdict whether or not the policy published it.
    async fn pipe_command(
        mux: &Arc<ShellMux>,
        seed: &Path,
        name: Principal,
        options: SpawnOptions,
        text: &str,
    ) -> Arc<crate::shellmux::CommandCompletion> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let job = mux
                .open_shell(
                    seed,
                    Some(name),
                    SpawnOptions {
                        io: JobIo::Pipes,
                        ..options
                    },
                )
                .await
                .expect("open pipe job");
            job.close_input().await.expect("close input");
            let completion = match job.run_command(text, CommandOptions::default()).await {
                Ok(completion) => completion,
                Err(error) => Arc::clone(error.completion().expect("the line reached a verdict")),
            };
            job.wait_closed().await.expect("job closed");
            completion
        })
        .await
        .expect("command deadline")
    }

    /// A fresh `seed` directory in `scratch`, registered on `filesystem`.
    fn seeded(scratch: &Path, filesystem: &marsh_btrfs::fake::CopyTree) -> PathBuf {
        let seed = scratch.join("seed");
        std::fs::create_dir(&seed).expect("seed directory");
        filesystem.register(&seed);
        seed
    }

    /// A mux with the default profile over `filesystem`.
    fn ownership_mux(filesystem: Arc<marsh_btrfs::fake::CopyTree>) -> Arc<ShellMux> {
        ShellMux::new_with(
            MuxProfile::default(),
            Arc::new(Mutex::new(Silent::new(24, 80))),
            filesystem,
        )
        .expect("build mux")
    }

    /// A supplied environment replaces both ambient and profile variables, while `None`
    /// retains the ordinary inherited/profile environment. Values reach external commands intact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_environment_replaces_inherited_and_profile_variables() {
        let inherited = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo test environment");
        let inherited_value = std::env::var("MARSH_ENV_REPLACEMENT_TEST_VALUE").unwrap_or_default();
        let scratch = tempfile::tempdir().expect("scratch directory");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        let seed = seeded(scratch.path(), &filesystem);
        let mut profile = MuxProfile::default();
        let mut variable = ShellVariable::new("profile");
        variable.export();
        profile
            .environment
            .set_global("MARSH_ENV_REPLACEMENT_PROFILE".to_owned(), variable)
            .expect("profile variable");
        let mux = ShellMux::new_with(
            profile,
            Arc::new(Mutex::new(Silent::new(24, 80))),
            filesystem,
        )
        .expect("build mux");
        let value = "quotes:'\"; dollar:$HOME\nsecond line";
        let mut replacement = brush_core::env::ShellEnvironment::new();
        let mut variable = ShellVariable::new(value);
        variable.export();
        replacement
            .set_global("MARSH_ENV_REPLACEMENT_TEST_VALUE".to_owned(), variable)
            .expect("replacement variable");
        let mut observations = Vec::new();
        for (name, environment, expected) in [
            (
                "replaced",
                Some(replacement),
                format!("unset\nunset\n{value}\n"),
            ),
            (
                "inherited",
                None,
                format!("{inherited}\nprofile\n{inherited_value}\n"),
            ),
        ] {
            let text = format!(
                "/bin/sh -c 'printf \"%s\\n\" \"${{CARGO_MANIFEST_DIR-unset}}\" \
                 \"${{MARSH_ENV_REPLACEMENT_PROFILE-unset}}\" \
                 \"$MARSH_ENV_REPLACEMENT_TEST_VALUE\"' > {name}.txt"
            );
            let completion = pipe_command(
                &mux,
                &seed,
                Principal::from(name),
                SpawnOptions {
                    environment,
                    ..SpawnOptions::default()
                },
                &text,
            )
            .await;
            let actual = std::fs::read_to_string(seed.join(format!("{name}.txt")))
                .expect("published output");
            observations.push((completion, actual, expected));
        }
        mux.shutdown().await.expect("shutdown mux");
        for (completion, actual, expected) in observations {
            assert!(completion.is_published());
            assert_eq!(completion.exit_code, Some(0));
            assert_eq!(actual, expected);
        }
    }

    /// Whether `completion` is the policy refusing the line, which still exited cleanly.
    fn denied(completion: &crate::shellmux::CommandCompletion) -> bool {
        completion.exit_code == Some(0)
            && matches!(
                completion.outcome.as_ref(),
                Ok(crate::Outcome::Denied { .. })
            )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn durable_principal_recovers_without_granting_authority_to_reusable_names() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        let seed = seeded(scratch.path(), &filesystem);
        let durable = SpawnOptions {
            durable: true,
            ..SpawnOptions::default()
        };
        let owner = ShellId::durable("stable-agent");
        let mux = ownership_mux(filesystem.clone());
        assert!(
            pipe_command(
                &mux,
                &seed,
                Principal::from("stable-agent"),
                durable.clone(),
                "printf owned > file.txt"
            )
            .await
            .is_published()
        );

        // Neither the displayed name nor a name copied from the policy namespace grants
        // durable authority, even before any recovered-owner disambiguation exists.
        for impostor in [Principal::from("stable-agent"), owner.principal().clone()] {
            let completion = pipe_command(
                &mux,
                &seed,
                impostor,
                SpawnOptions::default(),
                "printf bad > file.txt; true",
            )
            .await;
            assert!(denied(&completion));
            assert_eq!(
                std::fs::read(seed.join("file.txt")).expect("seed contents"),
                b"owned"
            );
        }
        mux.shutdown().await.expect("close first session");
        drop(mux);

        let reopened = ownership_mux(filesystem);
        let other = pipe_command(
            &reopened,
            &seed,
            Principal::from("another-agent"),
            durable.clone(),
            "printf bad > file.txt; true",
        )
        .await;
        assert!(denied(&other));
        assert_eq!(
            std::fs::read(seed.join("file.txt")).expect("seed contents"),
            b"owned"
        );
        let resumed = pipe_command(
            &reopened,
            &seed,
            Principal::from("stable-agent"),
            durable,
            "printf resumed > file.txt",
        )
        .await;
        reopened.shutdown().await.expect("close resumed session");
        assert!(resumed.is_published());
        assert_eq!(resumed.exit_code, Some(0));
        assert_eq!(
            std::fs::read(seed.join("file.txt")).expect("resumed contents"),
            b"resumed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn durable_opt_in_never_reassigns_legacy_snapshot_ownership() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        let seed = seeded(scratch.path(), &filesystem);
        let mux = ownership_mux(filesystem.clone());
        let original = pipe_command(
            &mux,
            &seed,
            Principal::from("reused"),
            SpawnOptions::default(),
            "printf legacy > file.txt",
        )
        .await;
        assert!(original.is_published());
        let uid = original.shell.uid.as_str();
        mux.shutdown().await.expect("close legacy session");
        drop(mux);

        let reopened = ownership_mux(filesystem);
        // Cover ordinary name reuse, direct snapshot-id impersonation, and both attempts
        // through the durable opt-in. Legacy WAL entries carry no durable agent authority.
        for (impostor, durable) in [("reused", false), (uid, false), ("reused", true), (uid, true)]
        {
            let completion = pipe_command(
                &reopened,
                &seed,
                Principal::from(impostor),
                SpawnOptions {
                    durable,
                    ..SpawnOptions::default()
                },
                "printf bad > file.txt; true",
            )
            .await;
            assert!(denied(&completion), "{impostor} (durable: {durable})");
            assert_eq!(
                std::fs::read(seed.join("file.txt")).expect("legacy contents"),
                b"legacy"
            );
        }
        reopened.shutdown().await.expect("close second session");
    }

    /// A durable shell and a session-local one are different principals, but both would be the
    /// one `%name` a reader types: while either is live, the other spelling is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_durable_name_and_a_reusable_one_never_share_a_live_display_name() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        let seed = seeded(scratch.path(), &filesystem);
        let mux = ownership_mux(filesystem);
        let pipes = |durable| SpawnOptions {
            io: JobIo::Pipes,
            durable,
            ..SpawnOptions::default()
        };
        for (held, other) in [(true, false), (false, true)] {
            let live = mux
                .open_shell(&seed, Some(Principal::from("agent")), pipes(held))
                .await
                .expect("open the first spelling");
            let refused = mux
                .open_shell(&seed, Some(Principal::from("agent")), pipes(other))
                .await;
            assert!(
                matches!(&refused, Err(MuxError::JobExists(id)) if id.as_str() == "agent"),
                "durable {other} beside durable {held}: {:?}",
                refused.map(|shell| shell.id().clone())
            );
            live.stop(true).await.expect("stop the first spelling");
            tokio::time::timeout(std::time::Duration::from_secs(30), live.wait_closed())
                .await
                .expect("close deadline")
                .expect("the first spelling closes");
        }
        mux.shutdown().await.expect("shutdown mux");
    }
}
