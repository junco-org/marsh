//! The spawner itself: what the shell hands every external command to.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExternalCommandSpawner, ShellExtensions,
    ShellExtensionsImpl,
};
use brush_core::{Shell, ShellVariable};
use marsh_btrfs::{LibBtrfs, Subvolumes};
use marsh_instrument::{BuiltinHook, BuiltinRecord, SpawnRecord, SpawnRequest};

use super::MarshError;
use super::builtins::SNAPSHOT_ROOT_VAR;
use super::policy::{Principal, escaped_live_principal};
use super::session::{Session, Snapshot};

/// Shell extensions selecting [`MarshExecutor`].
pub type MarshShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter, MarshExecutor>;

/// The shell's spawner, and its handle on the session.
///
/// An [`ExternalCommandSpawner`] that records every external command the shell asks it to start.
/// Attached, it also carries the snapshot — and through it the seed, log and records — that
/// [`Shell`](super::Shell) stages every line in and publishes through.
///
/// There are three states.
///
/// `Default` is the *detached* executor: it delegates to [`DefaultExternalCommandSpawner`] and
/// records nothing, so a shell built with it behaves exactly like a stock one.
///
/// [`Self::open`] is the *seed-level* executor: it holds the seed's lease, its recovered log and
/// nothing else — no snapshot. A `ShellMux` is built over one of these.
///
/// [`Self::snapshot`] is the *attached* executor: the seed-level one plus a snapshot of its own for
/// one principal. A [`Shell`](super::Shell) is built over one of these, and any number of them can
/// run concurrently over a single seed.
///
/// # What it sees
///
/// Only external commands reach a spawner — builtins and shell functions never do. Those are
/// observed instead through the instrumentation [`Shell::attach`](super::Shell::attach) installs,
/// whose records are [`Self::builtin_records`]; the spawn attempts are [`Self::spawn_records`]. The
/// command is spawned exactly as the shell composed it: working directory, environment, file
/// descriptors and process group are untouched. This executor observes, it does not confine.
///
/// # What it publishes, and when
///
/// Publication is [`Shell`](super::Shell)'s: after every line it ran or concluded, once the
/// capability policy granted the line's requests; a denied line is discarded instead, and a line
/// another principal beat to one of its paths comes back as
/// [`Outcome::Stale`](super::Outcome::Stale). The snapshot publishes on its own only when it is
/// dropped — finding nothing, normally, because the `Shell`'s own drop concluded the last
/// boundary; whatever a failed final boundary left, otherwise. A publication in the middle of a
/// line is not possible from here, and deliberately so: every stage of a pipeline runs in its own
/// owned shell clone, so a diff taken from inside one could capture another stage's half-written
/// file.
///
/// # What isolation there is
///
/// The shell's working directory: [`Shell::attach`](super::Shell::attach) starts it at
/// [`snapshot_root`](Self::snapshot_root) and exports
/// [`SNAPSHOT_ROOT_VAR`] so the `git` builtin stops its repository search there. A script that
/// `cd`s to an absolute path outside the snapshot writes wherever it went.
#[derive(Clone, Default)]
pub struct MarshExecutor {
    /// The seed's lease and log, or `None` for a detached executor.
    session: Option<Arc<Session>>,
    /// This shell's snapshot, or `None` for a detached or seed-level executor.
    snapshot: Option<Arc<Snapshot>>,
}

impl std::fmt::Debug for MarshExecutor {
    /// Names what the executor is attached to, which is all a caller can act on; the session's
    /// internals are not part of its interface.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MarshExecutor")
            .field("seed", &self.seed())
            .field("uid", &self.uid())
            .field("principal", &self.principal())
            .finish()
    }
}

impl MarshExecutor {
    /// Attaches to the btrfs seed containing `seed`, over real btrfs.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::open_with`] does.
    pub fn open(seed: &Path) -> Result<Self, MarshError> {
        Self::open_with(seed, Arc::new(LibBtrfs))
    }

    /// Attaches to the seed containing `seed`, taking snapshots through `fs`.
    ///
    /// Takes the seed's exclusive lease and recovers its log; takes no snapshot. Call
    /// [`Self::snapshot`] once per shell for that.
    ///
    /// # Errors
    ///
    /// Fails when no subvolume contains `seed`, when another process holds the seed's lease
    /// ([`marsh_btrfs::Error::SessionBusy`]), or when the log cannot be recovered.
    pub fn open_with(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Self, MarshError> {
        Ok(Self {
            session: Some(Session::open(seed, fs)?),
            snapshot: None,
        })
    }

    /// A fresh snapshot of the seed for `principal`, as an attached executor.
    ///
    /// Every snapshot runs concurrently with every other one's lines: they share the seed's
    /// authority, which serializes only the boundaries. A detached executor stays detached.
    /// Names here are session-local, not recovery credentials. To retain agent ownership across
    /// reopening, use [`crate::shellmux::ShellId::durable`] when spawning a mux job.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken.
    pub fn snapshot(&self, principal: Principal) -> Result<Self, MarshError> {
        let principal = escaped_live_principal(&principal).unwrap_or(principal);
        self.snapshot_for(Some(principal), None)
    }

    /// The same, with the snapshot's own uid for a principal: what a lone [`Shell`](super::Shell)
    /// gets when nobody named one.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken.
    pub(crate) fn snapshot_as_uid(&self) -> Result<Self, MarshError> {
        self.snapshot_for(None, None)
    }

    /// Shared construction for anonymous shells and already-scoped mux identities.
    pub(crate) fn snapshot_for(
        &self,
        principal: Option<Principal>,
        durable_name: Option<Principal>,
    ) -> Result<Self, MarshError> {
        match &self.session {
            None => Ok(Self::default()),
            Some(session) => Ok(Self {
                session: Some(Arc::clone(session)),
                snapshot: Some(session.snapshot(principal, durable_name)?),
            }),
        }
    }

    /// The tree the session publishes into, when it has one.
    pub fn seed(&self) -> Option<&Path> {
        self.session
            .as_ref()
            .map(|session| session.persistence.seed.as_path())
    }

    /// The snapshot the shell runs inside, when attached.
    ///
    /// One shell's own tree. A seed-level executor has none — see [`Self::snapshot_dir`] for the
    /// directory they all live in.
    pub fn snapshot_root(&self) -> Option<&Path> {
        self.snapshot.as_ref().map(|snapshot| snapshot.path())
    }

    /// The directory every snapshot of this seed is taken into: `<state>/snap`.
    ///
    /// The session's, not one shell's, so a *seed-level* executor has it and
    /// [`Self::snapshot_root`] is `None` — which is exactly the pair a daemon needs. It holds no
    /// snapshot of its own and still has to recognize a path that lies inside one of its jobs',
    /// because a client whose shell has already moved into a pane's snapshot hands over paths
    /// spelled that way.
    ///
    /// Canonicalized to match [`Self::snapshot_root`], which is canonicalized when the snapshot is
    /// taken: a prefix that differed by a symlink would not strip.
    pub fn snapshot_dir(&self) -> Option<PathBuf> {
        self.session.as_ref().map(|session| {
            let snap = session.persistence.snap();
            snap.canonicalize().unwrap_or(snap)
        })
    }

    /// This snapshot's id, when attached.
    pub fn uid(&self) -> Option<&str> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.uid().as_str())
    }

    /// Who the shell over this executor acts as, when attached.
    pub fn principal(&self) -> Option<&Principal> {
        self.snapshot.as_ref().map(|snapshot| snapshot.principal())
    }

    /// Makes a built shell this executor's: it starts at the snapshot root, gains the `git` and
    /// `exec` builtins (over stock `exec`), has every builtin it holds instrumented, and exports
    /// [`SNAPSHOT_ROOT_VAR`]. A detached or seed-level executor leaves the shell exactly as built.
    ///
    /// Call once per shell, after the last builtin has been registered — a registration added
    /// afterwards runs uninstrumented. Any number of attached shells may exist per process over
    /// one seed, on one condition instrumentation imposes: every attached shell must be built with
    /// the same builtin names and the same `SE` — each attach replaces the process-wide
    /// installation, and a shell whose builtin the latest installation does not know fails that
    /// builtin with `marsh-instrument: builtin … is not instrumented`. A shell's records are told
    /// apart by its working directory lying inside its own snapshot.
    ///
    /// Reached only through [`Shell::attach`](super::Shell::attach), so an attached shell is
    /// always a gated one.
    ///
    /// # Errors
    ///
    /// Fails when the shell refuses the working directory or the variable.
    pub(crate) fn attach<SE: ShellExtensions>(
        &self,
        shell: &mut Shell<SE>,
    ) -> Result<(), brush_core::Error> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        shell.set_working_dir(snapshot.path())?;
        let mut builtins = shell.builtins().clone();
        builtins.extend(super::builtins::all());
        let hook = Arc::clone(&snapshot.session().hook) as Arc<dyn BuiltinHook>;
        for (name, registration) in marsh_instrument::instrument(builtins, hook) {
            shell.register_builtin(name, registration);
        }
        let mut var = ShellVariable::new(snapshot.path().to_string_lossy().as_ref());
        var.export();
        shell.set_env_global(SNAPSHOT_ROOT_VAR, var)
    }

    /// The snapshot the lines are staged in, when attached.
    pub(crate) const fn attached(&self) -> Option<&Arc<Snapshot>> {
        self.snapshot.as_ref()
    }

    /// Installs this seed's durable capability history into `validator`, once per seed.
    ///
    /// Called wherever an executor is bound to the history its lines will be judged against —
    /// [`Shell::attach`](super::Shell::attach) and `ShellMux::new` — which is strictly before any
    /// of those lines can reach a gate. A detached executor has no seed and therefore no history
    /// to install; it never publishes and never requests.
    pub(crate) fn rehydrate(&self, validator: &std::sync::Mutex<super::PolicyValidator>) {
        if let Some(session) = &self.session {
            session.adopt_into(validator);
        }
    }

    /// Whether an approved publication failed and the seed's log still has to be replayed.
    ///
    /// `false` for a detached executor, which never publishes anything. A seed-level executor
    /// answers for the whole session it leases, not only for a snapshot of its own.
    #[must_use]
    pub fn recovery_required(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.recovery_required())
    }

    /// Every external command this executor was asked to spawn; empty when detached or seed-level.
    #[must_use]
    pub fn spawn_records(&self) -> Vec<SpawnRecord> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.spawns.records())
            .unwrap_or_default()
    }

    /// Every builtin this shell ran; empty when detached or seed-level.
    #[must_use]
    pub fn builtin_records(&self) -> Vec<BuiltinRecord> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.builtin_records())
            .unwrap_or_default()
    }
}

impl ExternalCommandSpawner for MarshExecutor {
    /// Spawns `command` exactly as composed, recording the attempt when attached.
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        let Some(snapshot) = &self.snapshot else {
            return DefaultExternalCommandSpawner.spawn(command, kill_on_drop);
        };
        let request = SpawnRequest::of(&command);
        match DefaultExternalCommandSpawner.spawn(command, kill_on_drop) {
            Ok(child) => {
                snapshot.spawns.spawned(request, child.id());
                Ok(child)
            }
            Err(error) => {
                snapshot.spawns.failed(request, &error);
                Err(error)
            }
        }
    }
}
