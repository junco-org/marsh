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
mod access;
mod error;
mod executor;
pub mod input;
pub mod policy;
mod session;
mod signal;

pub use error::MarshError;
pub use executor::{MarshExecutor, MarshShellExtensions};
/// The native workers of one logical command, owned where a line is evaluated rather than where
/// it is scheduled.
pub(crate) use executor::Workers;
pub use policy::{Denial, PolicyValidator};
pub use session::{GrantedAction, GrantedCapability, Publication, PublishMeta, SnapshotUid};
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
    /// since the last boundary — so a line always starts from the current seed. If the line reads
    /// something another principal publishes while it is running, it is unwound, the snapshot is
    /// resynchronized and the *same* line is evaluated again; the result and outcome are the last
    /// evaluation's, which is the only one that ever asked for a capability.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be retaken, when the shell cannot run the line, and for the
    /// reasons the gate fails.
    pub async fn run(&self, line: &str) -> Result<(ExecutionResult, Outcome), MarshError> {
        let mut shell = self.inner.lock().await;
        let mut last = None;
        let settled = self.settle(&mut shell, line, None, false, &mut last).await;
        drop(shell);
        // `unwrap_or_default` is unreachable: `settle` was told the line had not run, so it ran it
        // at least once before it could answer.
        Ok((last.unwrap_or_default(), settled?))
    }

    /// The same, with the native workers of one logical command.
    ///
    /// The multiplexer above this owns admission, receipts and the terminal; the workers a line's
    /// builtins register are owned here, because joining them is part of evaluating the line and a
    /// replay has to join the abandoned evaluation's before it may reseed the tree.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::run`] does.
    pub(crate) async fn run_with_workers(
        &self,
        line: &str,
        workers: Arc<Workers>,
    ) -> (Option<ExecutionResult>, Result<Outcome, MarshError>) {
        let mut shell = self.inner.lock().await;
        let mut last = None;
        let settled = self
            .settle(&mut shell, line, Some(&workers), false, &mut last)
            .await;
        drop(shell);
        // The pair, not a `Result` of one: a boundary that broke has no verdict and the line still
        // ran. What it asked the shell to do — `exit`, above all — is recorded by the caller from
        // this result, and a failure here may not swallow that request.
        (last, settled)
    }

    /// The boundary after a line the caller ran itself (brush-interactive's loop runs its own
    /// lines).
    ///
    /// Translates what the line left, checks it, publishes or discards. A line whose reads were
    /// invalidated while it ran is evaluated again here, through this same shell, because the
    /// caller has already consumed its input and cannot re-offer the line. After a discard the
    /// shell may be standing in a directory the line created: it is moved back to the snapshot
    /// root.
    ///
    /// # Errors
    ///
    /// Fails when the trees cannot be compared, when the transaction cannot be logged or applied,
    /// when the snapshot cannot be retaken, or when the shell refuses the snapshot root.
    pub async fn conclude(
        &self,
        shell: &mut brush_core::Shell<SE>,
        cmd: &str,
    ) -> Result<Outcome, MarshError> {
        self.settle(shell, cmd, None, true, &mut None).await
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
            // A forced stop is allowed to reclaim a tree whose evidence is incomplete: nothing it
            // observed is about to become a grant, so an unresolved call costs nothing.
            snapshot.pending(cmd)?.discard()?;
            if !shell.working_dir().exists() {
                shell.set_working_dir(snapshot.path())?;
            }
            Ok(Outcome::Discarded)
        })
    }

    /// Evaluate → gate → resynchronize and evaluate again, until the gate answers.
    ///
    /// `ran` says whether the caller already ran `cmd` itself once. The loop is the whole
    /// read-dependency mechanism: everything below it — the tracer, the snapshot's read set, the
    /// interruption flag — exists to decide when to go round again.
    ///
    /// `last` receives each evaluation's result as it is produced, and therefore survives a
    /// boundary failure: a line that asked the shell to exit said so before the boundary broke,
    /// and losing that would leave the shell open on a request it had already made. It stays
    /// `None` only when the caller ran the line itself and no replay was needed.
    async fn settle(
        &self,
        shell: &mut brush_core::Shell<SE>,
        cmd: &str,
        workers: Option<&Arc<Workers>>,
        ran: bool,
        last: &mut Option<ExecutionResult>,
    ) -> Result<Outcome, MarshError> {
        let mut ran = ran;
        loop {
            if !ran {
                self.refresh(shell)?;
                *last = Some(self.evaluate(shell, cmd).await?);
            }
            if let Some(workers) = workers {
                // Before the boundary, and with the interpreter still held: a native worker that
                // outlived this would be writing into a tree the gate is about to reset.
                workers.finish().await;
                // A forced stop outranks everything below. The line is thrown away unchecked —
                // the attempt happened and its records are still dumped — and it is never
                // evaluated again, because a caller that asked for this command to end did not
                // ask for it to run a second time.
                if workers.cancellation_requested() {
                    return self.discard(shell, cmd);
                }
            }
            // An interrupted evaluation is not gated at all: what it observed is already known to
            // be out of date, and translating it would request capabilities for a line that is
            // about to be run again.
            let settled = if self.executor.interrupted() {
                self.resynchronize(cmd)?;
                None
            } else {
                blocking_boundary(|| self.gate(cmd))?
            };
            // Every ending but a publication retook the tree, so the shell may be standing in a
            // directory that no longer exists.
            self.recover_directory(shell)?;
            if let Some(outcome) = settled {
                return Ok(outcome);
            }
            // Invalidated: the tree is the seed's again and the abandoned evaluation's footprint
            // has been forgotten. Only admission is left to reopen.
            if let Some(workers) = workers {
                workers.reopen();
            }
            ran = false;
        }
    }

    /// Runs `cmd` once, under this shell's trace scope, signalling its processes if it is
    /// invalidated while it runs.
    ///
    /// The scope is what attributes the syscalls to this snapshot; the signal is what stops an
    /// external command that would otherwise run to completion into a tree about to be retaken.
    /// Brush is left to unwind and reap its own foreground work — this never waits for a process
    /// itself, because the interpreter already does.
    async fn evaluate(
        &self,
        shell: &mut brush_core::Shell<SE>,
        cmd: &str,
    ) -> Result<ExecutionResult, MarshError> {
        let params = shell.default_exec_params();
        let mark = self.executor.spawn_record_count();
        // Boxed for the same reason a mux's is: an interpreter run's state machine is as deep as
        // the script it is running, and storing it inline would push that depth into every frame
        // above it.
        let source = SourceInfo::default();
        let line = Box::pin(shell.run_string(cmd, &source, &params));
        let mut line = marsh_instrument::Scoped::new(line, self.executor.scope());
        let result = tokio::select! {
            result = &mut line => result,
            () = self.executor.interruption() => {
                // One signal, to this evaluation's own processes only. Brush's own wait is what
                // observes them ending, and the interpreter unwinds from there.
                let _ = self.executor.signal_since(mark, Signal::Interrupt);
                line.await
            }
        };
        Ok(result?)
    }

    /// Throws away an interrupted evaluation and puts the tree back at the current seed.
    ///
    /// The evidence is still dumped — the attempt happened — and the footprint it was decided
    /// from is forgotten, so the replay starts with an empty read and write set.
    ///
    /// # Errors
    ///
    /// Fails when the records cannot be dumped or the snapshot cannot be retaken.
    fn resynchronize(&self, cmd: &str) -> Result<(), MarshError> {
        blocking_boundary(|| {
            let Some(snapshot) = self.executor.attached() else {
                return Ok(());
            };
            snapshot.pending(cmd)?.discard()
        })
    }

    /// Drain the evidence → translate → check the policy → publish or discard.
    ///
    /// `None` is loop control, not an outcome: this line read something another principal has
    /// since published, the staged tree has been discarded and retaken, and the caller must
    /// evaluate the line again.
    ///
    /// Lock order everywhere: the seed's authority and the snapshot's state (both inside
    /// `Pending`), then the validator; the validator lock is never held across an await, and the
    /// instrumentation is drained *before* the authority is taken, never under it.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "`pending` holds the seed's authority for the whole boundary on purpose, and \
                  every arm consumes it rather than letting it fall out of scope"
    )]
    fn gate(&self, cmd: &str) -> Result<Option<Outcome>, MarshError> {
        let Some(snapshot) = self.executor.attached() else {
            return Ok(Some(Outcome::Detached));
        };
        // Before anything is diffed: a session whose approved publication failed may have a seed
        // that is neither its old state nor its new one, and a boundary taken against that would
        // compare against — and publish on top of — a state nobody has verified. Every shell over
        // this session refuses, not only the one that failed.
        if snapshot.session().recovery_required() {
            return Err(MarshError::RecoveryRequired);
        }
        // Outside every publication lock: this waits on the decoder, and the decoder classifies
        // under the very authority a boundary holds.
        self.executor.drain()?;

        let pending = snapshot.pending(cmd)?;
        if pending.needs_sync() {
            pending.discard()?;
            return Ok(None);
        }
        // Before anything is translated: a git whose effects are unknown or unattributable left
        // this tree, so nothing in it can be requested, and nothing enters the history.
        if let Some(failure) = pending.git_failure() {
            let failure = std::io::Error::other(failure.to_string());
            pending.discard()?;
            return Err(MarshError::Io(failure));
        }
        let requested =
            match policy::translate(&self.principal, pending.edits(), pending.git_records()) {
                Ok(requested) => requested,
                Err(error) => {
                    pending.discard()?;
                    return Err(error);
                }
            };
        let verdict = self
            .validator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check(&requested);
        Ok(Some(match verdict {
            Ok(()) => Outcome::Published {
                publication: pending.publish(&requested)?,
                granted: requested,
            },
            Err(denials) => {
                pending.discard()?;
                Outcome::Denied { requested, denials }
            }
        }))
    }

    /// Puts the shell back at the snapshot root when the directory it was standing in is gone.
    ///
    /// Every boundary that did not publish retakes the tree from the seed, which can remove a
    /// directory the line itself created.
    fn recover_directory(&self, shell: &mut brush_core::Shell<SE>) -> Result<(), MarshError> {
        if let Some(root) = self.executor.snapshot_root()
            && !shell.working_dir().exists()
        {
            shell.set_working_dir(root)?;
        }
        Ok(())
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
    /// Nothing is ever *replayed* here. A leftover has no submitted line to run again — the gate
    /// judges it under the empty command — so an invalidated one is safely discarded instead,
    /// which is what the best-effort contract of a destructor allows.
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
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(operation)
        }
        _ => operation(),
    }
}
