//! `marsh::Shell`: a brush shell whose every command line is staged in its own btrfs snapshot,
//! instrumented, checked against the capability policy, and only then published — or discarded.
//!
//! The pieces are this module's children: `executor` holds the `ExternalCommandSpawner` and the
//! snapshot handle, `session` the seed and the snapshots hanging off it, `policy` the validator
//! and the translation of a line's effects into requests, `builtins` the `git` and `exec`
//! builtins, `input` the gate between interactive lines, and `error` the infrastructure failure
//! type.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use brush_builtins::BuiltinSet;
use brush_core::extensions::ShellExtensions;
use brush_core::{ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, SourceInfo};

pub mod builtins;
mod error;
mod executor;
pub mod input;
pub mod policy;
mod session;
mod signal;

pub use error::MarshError;
pub use executor::{MarshExecutor, MarshShellExtensions};
pub use policy::{Denial, PolicyValidator};
pub use session::{
    GrantedAction, GrantedCapability, Publication, PublishMeta, SnapshotUid, StalePath,
};
pub use signal::Signal;

use policy::{Event, Principal};

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
    /// Another principal published one of this line's paths after its snapshot was taken: the
    /// line was discarded and the snapshot retaken from the seed; rerun it.
    Stale {
        /// Everything the line asked for.
        requested: Vec<Event>,
        /// The paths someone else won, and the transaction that won them.
        stale: Vec<StalePath>,
    },
    /// The line was thrown away unchecked, at the caller's request (a forced stop): the snapshot
    /// was retaken from the seed and nothing was published or recorded in the history.
    Discarded,
    /// A shell over a detached executor: nothing was staged, so nothing was checked or published.
    Detached,
}

/// A brush shell inside a btrfs snapshot of a seed, gated by the capability policy.
///
/// One snapshot per shell, retaken from the seed whenever another principal has published: open
/// the seed once with [`MarshExecutor::open`], call [`MarshExecutor::snapshot`] once per
/// principal, and build a shell over each. [`Self::new`] is the one-shell shortcut that does both.
///
/// It has no end state: it is the evaluator of a read-eval-publish loop, and every [`Self::run`] or
/// [`Self::conclude`] is one iteration's boundary — the line's effects staged, requested, and
/// published or discarded. The only boundary that is not a line's is `Drop`: whatever arrived after
/// the last iteration (a background job's late write) is gated under the empty command line when
/// the shell goes away, the way the snapshot already publishes its own leftovers.
pub struct Shell<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor> = MarshShellExtensions>
{
    /// The shell the lines run in.
    inner: ShellRef<SE>,
    /// The snapshot the lines are staged in, and the records they leave.
    executor: MarshExecutor,
    /// The committed history every request is judged against.
    validator: Arc<Mutex<PolicyValidator>>,
    /// Who this shell acts as: the principal its snapshot was taken for, which defaults to that
    /// snapshot's id. Empty for a detached shell, which never requests anything.
    principal: Principal,
    /// Whether this shell's final boundary discards instead of gating.
    ///
    /// A standalone shell's drop is a real boundary: it is the end of the read-eval-publish loop,
    /// and whatever a background job wrote after the last line is gated there like anything else.
    /// A mux-owned shell's drop is not: every command it ran already concluded explicitly, so the
    /// only thing a final gate could find is residue from an abort, a shutdown or a forced stop —
    /// work nobody asked to have published. Those shells discard instead.
    discard_on_drop: std::sync::atomic::AtomicBool,
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
    /// A seed-level executor — one straight from [`MarshExecutor::open`] — takes a snapshot of its
    /// own here, under its uid as principal; an attached executor is used as given.
    ///
    /// Profile and rc files are skipped: a command's footprint must be the command's, not the host
    /// user's shell configuration.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken, when brush-core cannot build a shell from these
    /// options, or when the executor cannot claim it.
    pub async fn build(
        executor: MarshExecutor,
        validator: Arc<Mutex<PolicyValidator>>,
    ) -> Result<Self, MarshError> {
        let executor = if executor.seed().is_some() && executor.snapshot_root().is_none() {
            executor.snapshot_as_uid()?
        } else {
            executor
        };
        let shell = brush_core::Shell::builder_with_extensions::<MarshShellExtensions>()
            .external_command_spawner(executor.clone())
            .interactive(false)
            .no_editing(true)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .builtins(brush_builtins::default_builtins(BuiltinSet::BashMode))
            .build()
            .await?;
        Self::attach(executor, validator, shell)
    }
}

impl<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> Shell<SE> {
    /// Makes a built shell — one built with the *attached* executor's clone, as
    /// `.external_command_spawner(executor.clone())` — the executor's, and wraps it: it starts at
    /// the snapshot root, gains the `git` and `exec` builtins (over stock `exec`), has every
    /// builtin it holds instrumented, and exports
    /// [`SNAPSHOT_ROOT_VAR`](super::builtins::SNAPSHOT_ROOT_VAR). Call it after the last builtin
    /// was registered — one added afterwards runs uninstrumented. A detached executor yields a
    /// pass-through shell, exactly as built.
    ///
    /// # Errors
    ///
    /// Fails with [`MarshError::NoSnapshot`] when the executor names a seed but carries no
    /// snapshot — the brush shell was then built with a spawner clone that would record
    /// nothing — and when the shell refuses the snapshot's working directory or the exported
    /// variable.
    pub fn attach(
        executor: MarshExecutor,
        validator: Arc<Mutex<PolicyValidator>>,
        mut shell: brush_core::Shell<SE>,
    ) -> Result<Self, MarshError> {
        if executor.seed().is_some() && executor.snapshot_root().is_none() {
            return Err(MarshError::NoSnapshot);
        }
        // Before this shell can gate anything: the seed's own log is what says which resources a
        // previous process left dirty and who owns them. A process that started with an empty
        // history would judge every one of them unowned.
        executor.rehydrate(&validator);
        executor.attach(&mut shell)?;
        let principal = executor
            .principal()
            .cloned()
            .unwrap_or_else(|| Principal::from(""));
        Ok(Self {
            inner: Arc::new(tokio::sync::Mutex::new(shell)),
            executor,
            validator,
            principal,
            discard_on_drop: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Makes this shell's final boundary a discard rather than a gate.
    ///
    /// For the shells a `ShellMux` owns. Their commands conclude explicitly, one boundary each, so
    /// a final gate can only ever see what an abort or a forced stop left behind — and turning
    /// that into a publication would grant a capability nobody checked.
    pub(crate) fn discard_on_drop(&self) {
        self.discard_on_drop
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// The shell itself, for a driver that runs its own lines.
    pub const fn shell_ref(&self) -> &ShellRef<SE> {
        &self.inner
    }

    /// The snapshot's executor: what the lines were staged in and recorded by.
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

    /// Retakes the snapshot from the seed when another principal has published since the last
    /// boundary, so the shell always sees the current seed.
    ///
    /// A detached shell is a no-op. Call before a line the caller runs itself; [`Self::run`] calls
    /// it first.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be retaken or the shell refuses the snapshot root.
    pub fn refresh(&self, shell: &mut brush_core::Shell<SE>) -> Result<(), MarshError> {
        blocking_boundary(|| {
            if let Some(snapshot) = self.executor.attached() {
                snapshot.refresh()?;
                // The retake may have dropped a directory another principal deleted.
                if !shell.working_dir().exists() {
                    shell.set_working_dir(snapshot.path())?;
                }
            }
            Ok(())
        })
    }

    /// Runs one line, then concludes it: the result is the line's, the outcome the gate's.
    ///
    /// The snapshot is refreshed first — retaken from the seed when another principal published
    /// since the last boundary — so a line always starts from the current seed.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be retaken, when the shell cannot run the line, and for the
    /// reasons the gate fails.
    pub async fn run(&self, line: &str) -> Result<(ExecutionResult, Outcome), MarshError> {
        let mut shell = self.inner.lock().await;
        self.refresh(&mut shell)?;
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
        blocking_boundary(|| {
            let outcome = self.gate(cmd)?;
            if let (Outcome::Denied { .. } | Outcome::Stale { .. }, Some(root)) =
                (&outcome, self.executor.snapshot_root())
                && !shell.working_dir().exists()
            {
                shell.set_working_dir(root)?;
            }
            Ok(outcome)
        })
    }

    /// The boundary for a line the caller cut short — a forced stop — instead of concluding
    /// normally.
    ///
    /// Dumps the line's records and discards whatever it staged without translating or checking
    /// it: the attempt happened (its records are still written), but nothing enters the history.
    /// The shell may be standing in a directory the line created: it is moved back to the snapshot
    /// root, exactly as [`Self::conclude`] does after its own discards.
    ///
    /// # Errors
    ///
    /// Fails when the records cannot be dumped or the snapshot cannot be retaken.
    pub fn discard(
        &self,
        shell: &mut brush_core::Shell<SE>,
        cmd: &str,
    ) -> Result<Outcome, MarshError> {
        blocking_boundary(|| {
            let Some(snapshot) = self.executor.attached() else {
                return Ok(Outcome::Detached);
            };
            snapshot.pending(cmd)?.discard()?;
            if !shell.working_dir().exists() {
                shell.set_working_dir(snapshot.path())?;
            }
            Ok(Outcome::Discarded)
        })
    }

    /// Translate → check for a lost race → check the policy → publish or discard.
    ///
    /// Lock order everywhere: the seed's authority and the snapshot's state (both inside
    /// `Pending`), then the validator; the validator lock is never held across an await.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "`pending` holds the seed's authority for the whole boundary on purpose, and \
                  every arm consumes it rather than letting it fall out of scope"
    )]
    fn gate(&self, cmd: &str) -> Result<Outcome, MarshError> {
        let Some(snapshot) = self.executor.attached() else {
            return Ok(Outcome::Detached);
        };
        // Before anything is diffed: a session whose approved publication failed may have a seed
        // that is neither its old state nor its new one, and a boundary taken against that would
        // compare against — and publish on top of — a state nobody has verified. Every shell over
        // this session refuses, not only the one that failed.
        if snapshot.session().recovery_required() {
            return Err(MarshError::RecoveryRequired);
        }
        let pending = snapshot.pending(cmd)?;
        let requested = policy::translate(
            &self.principal,
            snapshot.path(),
            pending.ops(),
            pending.builtins(),
        );
        let stale = pending.stale(&requested);
        if !stale.is_empty() {
            pending.discard()?;
            return Ok(Outcome::Stale { requested, stale });
        }
        let verdict = self
            .validator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check(&requested);
        match verdict {
            Ok(()) => Ok(Outcome::Published {
                publication: pending.publish(&requested)?,
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
    /// Closes the last boundary: a gate for a standalone shell, a discard for a mux-owned one.
    ///
    /// A standalone shell's drop really is the end of its read-eval-publish loop, so whatever
    /// arrived after the last line — a background job's late write — is translated and checked
    /// here exactly as a line would be. It is still a *gate*: the validator decides, and a denied
    /// leftover is discarded.
    ///
    /// A mux-owned shell has already concluded every command it ran. What a gate could find here
    /// is only what an abort, a shutdown or a forced stop left, and publishing that would grant a
    /// capability nobody asked about, so those shells discard instead.
    ///
    /// A failure has nowhere to go and is dropped; the snapshot's own drop then reclaims the tree,
    /// or keeps it when an approved publication still owes the log a replay.
    fn drop(&mut self) {
        if self
            .discard_on_drop
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some(snapshot) = self.executor.attached() {
                let _ = snapshot.pending("").and_then(session::Pending::discard);
            }
            return;
        }
        let _ = self.gate("");
    }
}

/// Runs `operation`, letting a multi-threaded runtime move other tasks off this worker first.
///
/// Every boundary is blocking filesystem work: a tree diff, a durable log write, a subvolume
/// operation. Running it straight on an async worker stalls every other task that worker was
/// serving, which on a daemon means one pane's publication freezing the panes next to it.
///
/// `block_in_place` is the fix, but only a multi-threaded runtime has anywhere to move the work
/// to — it panics on a current-thread runtime — and a caller with no runtime at all must simply
/// run. Both of those keep the plain synchronous path, so this is safe to call from anywhere.
///
/// Deliberately *inside* the boundary rather than around the whole interpreter: wrapping an async
/// shell in one unabortable blocking closure would make a forced stop unable to interrupt it.
fn blocking_boundary<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle)
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(operation)
        }
        _ => operation(),
    }
}
