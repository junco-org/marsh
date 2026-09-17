//! The [`CommandExecutor`] itself: what the shell hands every simple command to.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use brush_btrfs::{LibBtrfs, Subvolumes};
use brush_builtin::SNAPSHOT_ROOT_VAR;
use brush_builtins::BuiltinSet;
use brush_core::builtins::Registration;
use brush_core::commands::{ShellForCommand, SimpleCommand};
use brush_core::extensions::{DefaultErrorFormatter, ShellExtensions, ShellExtensionsImpl};
use brush_core::{
    CommandExecutor, DefaultCommandExecutor, ExecutionExitCode, ExecutionSpawnResult, ShellVariable,
};
use brush_core::results::ExecutionWaitResult;
use brush_instrument::{BuiltinRecord, CommandKind, CommandRecord};

use crate::session::{Publication, Session};
use crate::MarshError;

/// Shell extensions selecting [`MarshExecutor`].
pub type MarshShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter, MarshExecutor>;

/// Runs the shell inside a btrfs snapshot of a seed tree.
///
/// A [`CommandExecutor`] that publishes each command's effects into the seed through a write-ahead
/// log, and records every simple command and builtin it dispatches.
///
/// `Default` is the detached executor: it delegates to [`DefaultCommandExecutor`] and records
/// nothing, so a shell built with it behaves exactly like a stock one.
///
/// # What attaching does to the shell
///
/// At its first dispatch an attached executor moves the shell's working directory from the seed
/// into the same place in the snapshot, and exports [`SNAPSHOT_ROOT_VAR`] so the `git` builtin
/// stops its repository search at the snapshot root. A working directory that is already inside
/// the snapshot, or outside the seed entirely, is left alone: this executor observes, it does not
/// confine.
///
/// The interpreter resolves a command's *redirections* before the executor runs, so the first
/// command of a shell started inside the seed writes its redirection targets into the seed
/// directly; from the second command on everything goes through the snapshot.
/// [`build_shell`](crate::build_shell) avoids that by starting the shell at
/// [`snapshot_root`](Self::snapshot_root).
///
/// # What it publishes, and when
///
/// A command the executor sees through to a result is published as soon as that result is
/// observed. A command handed back to the interpreter still running — a stage of a multi-stage
/// pipeline, a background job's process — is *deferred*: its id is remembered and its effects are
/// published at the next dispatch into the shell's own sequential control flow, which is the first
/// moment the interpreter guarantees its predecessors have been awaited.
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

    /// Publishes whatever the snapshot currently differs from the seed by.
    ///
    /// # Errors
    ///
    /// Fails with [`MarshError::Detached`] when this executor has no session, and otherwise for
    /// the reasons a publication fails.
    pub fn publish(&self) -> Result<Publication, MarshError> {
        self.session
            .as_ref()
            .ok_or(MarshError::Detached)?
            .publish(None)
    }

    /// Every simple command this executor dispatched; empty when detached.
    #[must_use]
    pub fn command_records(&self) -> Vec<CommandRecord> {
        self.session
            .as_ref()
            .map(|session| session.commands.records())
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

impl CommandExecutor for MarshExecutor {
    async fn execute<SE: ShellExtensions>(
        &self,
        mut command: SimpleCommand<'_, SE>,
    ) -> Result<ExecutionSpawnResult, brush_core::Error> {
        let Some(session) = &self.session else {
            return DefaultCommandExecutor.execute(command).await;
        };

        attach(session, command.shell_mut()).map_err(io_error)?;

        // A dispatch into the shell's own shell — rather than into an owned clone, which is what
        // every stage of a multi-stage pipeline gets — means the interpreter has already awaited
        // whatever ran before it, so deferred effects can be published now.
        let sequential = matches!(command.shell(), ShellForCommand::ParentShell(_));
        if sequential && session.has_pending() {
            session.publish(None).map_err(io_error)?;
        }

        let argv: Vec<String> = command.args.iter().map(ToString::to_string).collect();
        let kind = classify(
            command.shell(),
            &command.command_name,
            command.use_functions,
        );
        let id = session
            .commands
            .begin(&argv, command.shell().working_dir(), kind);

        // An external command dispatched into the parent shell without job control is awaited here
        // so its exit code and its effects are both observed; the interpreter would otherwise await
        // it immediately afterwards, so nothing else can be waiting on this future.
        let inline = sequential && !command.shell().options().enable_job_control;

        match DefaultCommandExecutor.execute(command).await {
            Err(error) => {
                session.commands.end(id, ExecutionExitCode::from(&error).into());
                // The command's own error is what the shell reports; a publication failure on top
                // of it would replace the diagnostic the user needs with an infrastructure one.
                let _ = session.publish(Some(id));
                Err(error)
            }
            Ok(ExecutionSpawnResult::Completed(result)) => {
                session.commands.end(id, result.exit_code.into());
                session.publish(Some(id)).map_err(io_error)?;
                Ok(ExecutionSpawnResult::Completed(result))
            }
            Ok(ExecutionSpawnResult::StartedProcess(child)) if inline => {
                let pid = child.pid();
                match ExecutionSpawnResult::StartedProcess(child).wait().await? {
                    ExecutionWaitResult::Completed(result) => {
                        session.commands.end(id, result.exit_code.into());
                        session.publish(Some(id)).map_err(io_error)?;
                        Ok(ExecutionSpawnResult::Completed(result))
                    }
                    // Only reachable from a SIGTSTP delivered to a job-control-free shell. The
                    // child is handed back untouched and its effects join the deferred set.
                    ExecutionWaitResult::Stopped(child) => {
                        session.commands.spawned(id, pid);
                        session.defer(id);
                        Ok(ExecutionSpawnResult::StartedProcess(child))
                    }
                }
            }
            Ok(ExecutionSpawnResult::StartedProcess(child)) => {
                session.commands.spawned(id, child.pid());
                session.defer(id);
                Ok(ExecutionSpawnResult::StartedProcess(child))
            }
            Ok(ExecutionSpawnResult::StartedTask(handle)) => {
                session.commands.spawned(id, None);
                session.defer(id);
                Ok(ExecutionSpawnResult::StartedTask(handle))
            }
        }
    }
}

/// Moves a shell sitting in the seed into the snapshot, and exports the git search boundary.
///
/// Idempotent: a working directory already under the snapshot fails the `strip_prefix` and is left
/// alone, and the variable is only set when it is absent.
fn attach<SE: ShellExtensions>(
    session: &Session,
    shell: &mut ShellForCommand<'_, SE>,
) -> Result<(), MarshError> {
    let cwd = std::fs::canonicalize(shell.working_dir())
        .unwrap_or_else(|_| shell.working_dir().to_path_buf());
    if let Ok(relative) = cwd.strip_prefix(&session.persistence.seed) {
        shell
            .set_working_dir(session.snapshot().join(relative))
            .map_err(|error| MarshError::Io(std::io::Error::other(error)))?;
    }
    if shell.env_var(SNAPSHOT_ROOT_VAR).is_none() {
        let mut var = ShellVariable::new(session.snapshot().to_string_lossy().as_ref());
        var.export();
        shell
            .set_env_global(SNAPSHOT_ROOT_VAR, var)
            .map_err(|error| MarshError::Io(std::io::Error::other(error)))?;
    }
    Ok(())
}

/// What the command name will resolve to, mirroring `SimpleCommand::execute`'s own order.
fn classify<SE: ShellExtensions>(
    shell: &ShellForCommand<'_, SE>,
    name: &str,
    use_functions: bool,
) -> CommandKind {
    let builtin = shell.builtins().get(name);
    if shell.options().posix_mode
        && builtin.is_some_and(|registration| {
            !registration.disabled && registration.special_builtin
        })
    {
        return CommandKind::Builtin;
    }
    if use_functions && shell.funcs().get(name).is_some() {
        return CommandKind::Function;
    }
    if builtin.is_some_and(|registration| !registration.disabled) {
        return CommandKind::Builtin;
    }
    CommandKind::External
}

/// Renders an infrastructure failure as the shell's own I/O error.
///
/// There is no executor-shaped variant in `brush_core::ErrorKind`, and inventing one would mean
/// modifying brush: a publication that cannot proceed is reported as what it is underneath, an I/O
/// failure against the seed.
fn io_error(error: MarshError) -> brush_core::Error {
    brush_core::Error::from(brush_core::ErrorKind::IoError(std::io::Error::other(error)))
}
