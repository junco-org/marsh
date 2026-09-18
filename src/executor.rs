//! The spawner itself: what the shell hands every external command to.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use brush_btrfs::{LibBtrfs, Subvolumes};
use brush_builtin::SNAPSHOT_ROOT_VAR;
use brush_builtins::BuiltinSet;
use brush_core::builtins::Registration;
use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExternalCommandSpawner, ShellExtensions,
    ShellExtensionsImpl,
};
use brush_core::{ExecutionResult, Shell, ShellVariable, SourceInfo};
use brush_instrument::{BuiltinRecord, SpawnRecord, SpawnRequest};

use crate::MarshError;
use crate::session::{Publication, Session};

/// Shell extensions selecting [`MarshExecutor`].
pub type MarshShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter, MarshExecutor>;

/// Runs the shell inside a btrfs snapshot of a seed tree.
///
/// An [`ExternalCommandSpawner`] that records every external command the shell asks it to start
/// and publishes the snapshot's changes back into the seed through a write-ahead log.
///
/// `Default` is the detached executor: it delegates to [`DefaultExternalCommandSpawner`] and
/// records nothing, so a shell built with it behaves exactly like a stock one.
///
/// # What it sees
///
/// Only external commands reach a spawner — builtins and shell functions never do. Those are
/// observed instead through the instrumented builtin map [`Self::builtins`] returns, whose records
/// are [`Self::builtin_records`]; the spawn attempts are [`Self::spawn_records`]. The command is
/// spawned exactly as the shell composed it: working directory, environment, file descriptors and
/// process group are untouched. This executor observes, it does not confine.
///
/// # What it publishes, and when
///
/// The seed is published by [`Self::run`] after each command line, by an explicit
/// [`Self::publish`], and once more when the last clone of the executor drops. A publication in
/// the middle of a line is not possible from here, and deliberately so: every stage of a pipeline
/// runs in its own owned shell clone, so a diff taken from inside one could capture another
/// stage's half-written file.
///
/// # What isolation there is
///
/// The shell's working directory: [`build_shell`](crate::build_shell) starts it at
/// [`snapshot_root`](Self::snapshot_root), and [`Self::export_snapshot_root`] exports
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
    /// ([`brush_btrfs::Error::SessionBusy`]), when the log cannot be recovered, or when the
    /// snapshot cannot be taken.
    pub fn open_with(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Self, MarshError> {
        Ok(Self {
            session: Some(Session::open(seed, fs)?),
        })
    }

    /// The tree this executor publishes into, when attached.
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

    /// Stock bash-mode builtins minus `exec`, plus `git`, instrumented when attached.
    ///
    /// `exec` replaces the process image, which would skip the record dumps and lose every builtin
    /// record of the session. A shell that cannot report is not the shell this executor runs
    /// commands in.
    ///
    /// Instrumentation is process-global, so this is called once per shell: building two
    /// instrumented shells in one process makes the second one's hook the only one that receives
    /// anything.
    #[must_use]
    pub fn builtins<SE: ShellExtensions>(&self) -> HashMap<String, Registration<SE>> {
        let mut builtins = brush_builtins::default_builtins::<SE>(BuiltinSet::BashMode);
        builtins.remove("exec");
        builtins.extend(brush_builtin::git_builtins());
        match &self.session {
            Some(session) => brush_instrument::instrument(builtins, session.hook.clone()),
            None => builtins,
        }
    }

    /// Publishes whatever the snapshot currently differs from the seed by, attributing it to the
    /// command line `cmd` (empty when nothing named it).
    ///
    /// # Errors
    ///
    /// Fails with [`MarshError::Detached`] when this executor has no session, and otherwise for
    /// the reasons a publication fails.
    pub fn publish(&self, cmd: &str) -> Result<Publication, MarshError> {
        self.session
            .as_ref()
            .ok_or(MarshError::Detached)?
            .publish(cmd)
    }

    /// Runs one command line in `shell`, then publishes what it left in the snapshot.
    ///
    /// `Shell::run_string` returns only after every foreground stage of the line has been awaited,
    /// which is what makes this the publication boundary: a diff taken here never sees a
    /// half-written pipeline stage. Background jobs (`&`) are the exception and are published by a
    /// later `run`, [`Self::publish`], or the drop.
    ///
    /// # Errors
    ///
    /// Fails with [`MarshError::Detached`] when this executor has no session — in which case the
    /// line is not run at all — when the shell fails to run the line, and for the reasons a
    /// publication fails.
    pub async fn run<SE: ShellExtensions>(
        &self,
        shell: &mut Shell<SE>,
        line: &str,
    ) -> Result<(ExecutionResult, Publication), MarshError> {
        let session = self.session.as_ref().ok_or(MarshError::Detached)?;
        let params = shell.default_exec_params();
        let result = shell
            .run_string(line, &SourceInfo::default(), &params)
            .await?;
        let publication = session.publish(line)?;
        Ok((result, publication))
    }

    /// Exports [`SNAPSHOT_ROOT_VAR`] into `shell`: the boundary the `git` builtin will not search
    /// above. A no-op when detached.
    ///
    /// # Errors
    ///
    /// Fails when the shell refuses the assignment.
    pub fn export_snapshot_root<SE: ShellExtensions>(
        &self,
        shell: &mut Shell<SE>,
    ) -> Result<(), brush_core::Error> {
        let Some(root) = self.snapshot_root() else {
            return Ok(());
        };
        let mut var = ShellVariable::new(root.to_string_lossy().as_ref());
        var.export();
        shell.set_env_global(SNAPSHOT_ROOT_VAR, var)
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
