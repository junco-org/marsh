//! The spawner itself: what the shell hands every external command to.

use std::path::Path;
use std::sync::Arc;

use marsh_btrfs::{LibBtrfs, Subvolumes};
use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExternalCommandSpawner, ShellExtensions,
    ShellExtensionsImpl,
};
use brush_core::{Shell, ShellVariable};
use marsh_instrument::{BuiltinRecord, SpawnRecord, SpawnRequest};

use super::MarshError;
use super::builtins::SNAPSHOT_ROOT_VAR;
use super::session::Session;

/// Shell extensions selecting [`MarshExecutor`].
pub type MarshShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter, MarshExecutor>;

/// The shell's spawner, and its handle on the session.
///
/// An [`ExternalCommandSpawner`] that records every external command the shell asks it to start.
/// Attached, it also carries the session — seed, snapshot, log and records — that
/// [`Shell`](super::Shell) stages every line in and publishes through.
///
/// `Default` is the detached executor: it delegates to [`DefaultExternalCommandSpawner`] and
/// records nothing, so a shell built with it behaves exactly like a stock one.
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
/// capability policy granted the line's requests; a denied line is discarded instead. The session
/// publishes on its own only when it is dropped — finding nothing, normally, because the `Shell`'s
/// own drop concluded the last boundary; whatever a failed final boundary left, otherwise. A
/// publication in the middle of a line is not possible from here, and deliberately so: every stage
/// of a pipeline runs in its own owned shell clone, so a diff taken from inside one could capture
/// another stage's half-written file.
///
/// # What isolation there is
///
/// The shell's working directory: [`Shell::attach`](super::Shell::attach) starts it at
/// [`snapshot_root`](Self::snapshot_root) and exports
/// [`SNAPSHOT_ROOT_VAR`] so the `git` builtin stops its repository search there. A script that
/// `cd`s to an absolute path outside the snapshot writes wherever it went.
#[derive(Clone, Default)]
pub struct MarshExecutor {
    /// The attached session, or `None` for a detached executor.
    session: Option<Arc<Session>>,
}

impl std::fmt::Debug for MarshExecutor {
    /// Names what the executor is attached to, which is all a caller can act on; the session's
    /// internals are not part of its interface.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MarshExecutor")
            .field("seed", &self.seed())
            .field("uid", &self.uid())
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
    /// # Errors
    ///
    /// Fails when no subvolume contains `seed`, when another process holds the seed's lease
    /// ([`marsh_btrfs::Error::SessionBusy`]), when the log cannot be recovered, or when the
    /// snapshot cannot be taken.
    pub fn open_with(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Self, MarshError> {
        Ok(Self {
            session: Some(Session::open(seed, fs)?),
        })
    }

    /// The tree the session publishes into, when attached.
    pub fn seed(&self) -> Option<&Path> {
        self.session
            .as_ref()
            .map(|session| session.persistence.seed.as_path())
    }

    /// The snapshot the shell runs inside, when attached.
    pub fn snapshot_root(&self) -> Option<&Path> {
        self.session.as_ref().map(|session| session.snapshot())
    }

    /// This session's snapshot id, when attached.
    pub fn uid(&self) -> Option<&str> {
        self.session.as_ref().map(|session| session.uid())
    }

    /// Makes a built shell this executor's: it starts at the snapshot root, gains the `git` and
    /// `exec` builtins (over stock `exec`), has every builtin it holds instrumented, and exports
    /// [`SNAPSHOT_ROOT_VAR`]. A detached executor leaves the shell exactly as built.
    ///
    /// Call once, after the last builtin has been registered — a registration added afterwards
    /// runs uninstrumented. Instrumentation is process-global, so one attached shell per process.
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
        let Some(session) = &self.session else {
            return Ok(());
        };
        shell.set_working_dir(session.snapshot())?;
        let mut builtins = shell.builtins().clone();
        builtins.extend(super::builtins::all());
        for (name, registration) in marsh_instrument::instrument(builtins, session.hook.clone()) {
            shell.register_builtin(name, registration);
        }
        let mut var = ShellVariable::new(session.snapshot().to_string_lossy().as_ref());
        var.export();
        shell.set_env_global(SNAPSHOT_ROOT_VAR, var)
    }

    /// The session the lines are staged in, when attached.
    pub(crate) const fn session(&self) -> Option<&Arc<Session>> {
        self.session.as_ref()
    }

    /// Every external command this executor was asked to spawn; empty when detached.
    #[must_use]
    pub fn spawn_records(&self) -> Vec<SpawnRecord> {
        self.session
            .as_ref()
            .map(|session| session.spawns.records())
            .unwrap_or_default()
    }

    /// Every builtin the instrumented map reported; empty when detached.
    #[must_use]
    pub fn builtin_records(&self) -> Vec<BuiltinRecord> {
        self.session
            .as_ref()
            .map(|session| session.hook.records())
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
        let Some(session) = &self.session else {
            return DefaultExternalCommandSpawner.spawn(command, kill_on_drop);
        };
        let request = SpawnRequest::of(&command);
        match DefaultExternalCommandSpawner.spawn(command, kill_on_drop) {
            Ok(child) => {
                session.spawns.spawned(request, child.id());
                Ok(child)
            }
            Err(error) => {
                session.spawns.failed(request, &error);
                Err(error)
            }
        }
    }
}
