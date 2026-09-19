//! `marsh::Shell`: a brush shell whose every command line is staged in the session's snapshot,
//! instrumented, checked against the capability policy, and only then published — or discarded.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use brush_builtins::BuiltinSet;
use brush_core::extensions::ShellExtensions;
use brush_core::{ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, SourceInfo};

use super::MarshError;
use super::executor::{MarshExecutor, MarshShellExtensions};
use super::policy::{self, Denial, Event, PolicyValidator, Principal};
use super::session::Publication;

/// The shell, shared with whatever drives it: the same type brush-interactive's loop takes.
pub use brush_interactive::ShellRef;

/// How one command line ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every capability was granted and the line's effects are in the seed.
    Published {
        /// What the publication wrote.
        publication: Publication,
        /// The capabilities the line requested, all of them granted.
        granted: Vec<Event>,
    },
    /// At least one capability was refused: the snapshot was retaken from the seed and the history
    /// is as it was before the line.
    Denied {
        /// Everything the line asked for.
        requested: Vec<Event>,
        /// The subset that was refused, with the policy's explanations.
        denials: Vec<Denial>,
    },
    /// A shell over a detached executor: nothing was staged, so nothing was checked or published.
    Detached,
}

/// A brush shell inside a btrfs snapshot of a seed, gated by the capability policy.
///
/// It has no end state: it is the evaluator of a read-eval-publish loop, and every [`Self::run`] or
/// [`Self::conclude`] is one iteration's boundary — the line's effects staged, requested, and
/// published or discarded. The only boundary that is not a line's is `Drop`: whatever arrived after
/// the last iteration (a background job's late write) is gated under the empty command line when
/// the shell goes away, the way the session already publishes its own leftovers.
pub struct Shell<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor> = MarshShellExtensions>
{
    /// The shell the lines run in.
    inner: ShellRef<SE>,
    /// The session the lines are staged in, and the records they leave.
    executor: MarshExecutor,
    /// The committed history every request is judged against.
    validator: Arc<Mutex<PolicyValidator>>,
    /// Who this shell acts as: the session's snapshot id, which is also the `uid` of every
    /// transaction it logs. Empty for a detached shell, which never requests anything.
    principal: Principal,
}

impl Shell<MarshShellExtensions> {
    /// A non-interactive shell inside a snapshot of `seed`, checked by `validator`.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`MarshExecutor::open`] does, and when the shell cannot be built.
    pub async fn new(
        seed: &Path,
        validator: Arc<Mutex<PolicyValidator>>,
    ) -> Result<Self, MarshError> {
        Self::build(MarshExecutor::open(seed)?, validator).await
    }

    /// The same shell over an already-opened executor.
    ///
    /// Profile and rc files are skipped: a command's footprint must be the command's, not the host
    /// user's shell configuration.
    ///
    /// # Errors
    ///
    /// Fails when brush-core cannot build a shell from these options, or when the executor cannot
    /// claim it.
    pub async fn build(
        executor: MarshExecutor,
        validator: Arc<Mutex<PolicyValidator>>,
    ) -> Result<Self, MarshError> {
        let shell = brush_core::Shell::builder_with_extensions::<MarshShellExtensions>()
            .external_command_spawner(executor.clone())
            .interactive(false)
            .no_editing(true)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .builtins(brush_builtins::default_builtins(BuiltinSet::BashMode))
            .build()
            .await?;
        Ok(Self::attach(executor, validator, shell)?)
    }
}

impl<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> Shell<SE> {
    /// Makes a built shell — one built with `.external_command_spawner(executor.clone())` — the
    /// executor's, and wraps it: it starts at the snapshot root, gains the `git` and `exec` builtins
    /// (over stock `exec`), has every builtin it holds instrumented, and exports
    /// [`SNAPSHOT_ROOT_VAR`](super::builtins::SNAPSHOT_ROOT_VAR). Call it after the last builtin was
    /// registered — one added afterwards runs uninstrumented — and once per process: instrumentation
    /// is process-global. A detached executor yields a pass-through shell, exactly as built.
    ///
    /// # Errors
    ///
    /// Fails when the shell refuses the snapshot's working directory or the exported variable.
    pub fn attach(
        executor: MarshExecutor,
        validator: Arc<Mutex<PolicyValidator>>,
        mut shell: brush_core::Shell<SE>,
    ) -> Result<Self, brush_core::Error> {
        executor.attach(&mut shell)?;
        let principal = Principal::from(executor.uid().unwrap_or_default());
        Ok(Self {
            inner: Arc::new(tokio::sync::Mutex::new(shell)),
            executor,
            validator,
            principal,
        })
    }

    /// The shell itself, for a driver that runs its own lines.
    pub const fn shell_ref(&self) -> &ShellRef<SE> {
        &self.inner
    }

    /// The session's executor: what the lines were staged in and recorded by.
    pub const fn executor(&self) -> &MarshExecutor {
        &self.executor
    }

    /// The history every request is judged against.
    pub const fn validator(&self) -> &Arc<Mutex<PolicyValidator>> {
        &self.validator
    }

    /// Who this shell acts as.
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Runs one line, then concludes it: the result is the line's, the outcome the gate's.
    ///
    /// # Errors
    ///
    /// Fails when the shell cannot run the line, and for the reasons the gate fails.
    pub async fn run(&self, line: &str) -> Result<(ExecutionResult, Outcome), MarshError> {
        let mut shell = self.inner.lock().await;
        let params = shell.default_exec_params();
        let result = shell
            .run_string(line, &SourceInfo::default(), &params)
            .await?;
        let outcome = self.conclude(&mut shell, line)?;
        drop(shell);
        Ok((result, outcome))
    }

    /// The boundary after a line the caller ran itself (brush-interactive's loop runs its own
    /// lines).
    ///
    /// Translates what the line left, checks it, publishes or discards. After a discard the shell
    /// may be standing in a directory the line created: it is moved back to the snapshot root.
    ///
    /// # Errors
    ///
    /// Fails when the trees cannot be compared, when the transaction cannot be logged or applied,
    /// when the snapshot cannot be retaken, or when the shell refuses the snapshot root.
    pub fn conclude(
        &self,
        shell: &mut brush_core::Shell<SE>,
        cmd: &str,
    ) -> Result<Outcome, MarshError> {
        let outcome = self.gate(cmd)?;
        if let (Outcome::Denied { .. }, Some(root)) = (&outcome, self.executor.snapshot_root())
            && !shell.working_dir().exists()
        {
            shell.set_working_dir(root)?;
        }
        Ok(outcome)
    }

    /// Translate → check → publish or discard.
    ///
    /// Lock order everywhere: the session's state (inside `Pending`), then the validator; the
    /// validator lock is never held across an await.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "`pending` holds the session lock for the whole boundary on purpose, and both \
                  arms consume it rather than letting it fall out of scope"
    )]
    fn gate(&self, cmd: &str) -> Result<Outcome, MarshError> {
        let Some(session) = self.executor.session() else {
            return Ok(Outcome::Detached);
        };
        let pending = session.pending(cmd)?;
        let requested = policy::translate(
            &self.principal,
            session.snapshot(),
            pending.ops(),
            pending.builtins(),
        );
        let verdict = self
            .validator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check(&requested);
        match verdict {
            Ok(()) => Ok(Outcome::Published {
                publication: pending.publish()?,
                granted: requested,
            }),
            Err(denials) => {
                pending.discard()?;
                Ok(Outcome::Denied { requested, denials })
            }
        }
    }
}

impl<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> Drop for Shell<SE> {
    /// Gates what arrived since the last iteration.
    ///
    /// A failure here has nowhere to go and is dropped; the session's own drop then retries the
    /// publication and, failing that, keeps the snapshot for the next session's recovery.
    fn drop(&mut self) {
        let _ = self.gate("");
    }
}
