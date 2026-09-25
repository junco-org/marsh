//! The managed context a registered builtin reaches its own command through.
//!
//! An extra builtin registered in a [`MuxProfile`](crate::shellmux::MuxProfile) frequently has
//! real work to do: read a file, drain a FIFO, enumerate a directory. That work is blocking, it
//! can outlive the line that started it, and it writes into a snapshot the mux may be about to
//! reclaim. A builtin that spawned it and walked away would leave a worker writing into a tree
//! that has already been discarded — the exact bug this module exists to prevent.
//!
//! So a managed command runs inside a scoped context. While it runs,
//! [`current_command_context`] answers with a [`CommandContext`]; a builtin registers its blocking
//! work through [`CommandContext::spawn_blocking`], and the core joins every registered worker
//! before it concludes, discards or reclaims anything. Cancellation is cooperative: a forced stop
//! *asks*, through [`CommandContext::cancellation_requested`] and
//! [`CommandContext::cancelled`], and a worker that never looks is waited for rather than
//! pre-empted. Nothing here claims to interrupt arbitrary Rust or arbitrary syscalls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::shell::Workers;
use crate::shellmux::command::CommandId;
use crate::shellmux::error::MuxError;
use crate::shellmux::mux::Sandbox;

tokio::task_local! {
    /// The context of the command running on this task, when one is.
    static CURRENT: CommandContext;
}

/// The context of the managed command running on the current task.
///
/// `None` outside a managed command: a builtin whose work only makes sense inside one should
/// refuse rather than fall back to doing it unmanaged. The context does not propagate into a
/// `tokio::spawn`ed task, a subshell or a pipeline stage, which is why builtins that need it must
/// be invoked as top-level parent-shell builtins.
///
/// The handle is stamped with the *current* evaluation, not the one the task-local was installed
/// for. A line that was invalidated and is being run again installs no new context — it is the
/// same logical command — so reading the generation here is what lets the replay's builtins
/// register work while a handle somebody kept from the abandoned evaluation cannot.
#[must_use]
pub fn current_command_context() -> Option<CommandContext> {
    CURRENT
        .try_with(|context| CommandContext {
            inner: Arc::clone(&context.inner),
            generation: context.inner.workers.generation(),
        })
        .ok()
}

/// Runs `body` with `context` installed as the current command context.
pub(crate) async fn with_context<F: Future>(context: CommandContext, body: F) -> F::Output {
    CURRENT.scope(context, body).await
}

/// The shared half of a command context.
#[derive(Debug)]
struct Inner {
    /// Which command this is the context of.
    id: CommandId,
    /// The sandbox it runs in: its identity, its seed, its directory label and its snapshot id.
    sandbox: Sandbox,
    /// The root of this command's own snapshot.
    snapshot_root: Option<PathBuf>,
    /// The workers this command owns, which the shell below the mux joins at every boundary.
    ///
    /// Shared rather than duplicated: a line is evaluated below this layer, so the party that has
    /// to join a worker before it reseeds a tree is the same party that has to decide whether the
    /// line runs again. A second worker table up here could only ever disagree with that one.
    workers: Arc<Workers>,
}

/// A handle on the managed command running on this task.
///
/// Cloneable and cheap. Holding one past the command's end is safe and useless: admission closes
/// before finalization, so [`Self::spawn_blocking`] then refuses — and so does a handle retained
/// across a replay, because the generation it was taken at has been superseded.
#[derive(Clone, Debug)]
pub struct CommandContext {
    /// The shared state.
    inner: Arc<Inner>,
    /// The evaluation this handle was taken during.
    ///
    /// A line whose reads were invalidated is evaluated again in a new generation, and the tree it
    /// ran in is retaken first. Work started through a handle from the abandoned evaluation would
    /// land in that retaken tree, so the generation travels with the handle and is checked at
    /// every registration.
    generation: u64,
}

impl CommandContext {
    /// Builds a context for one admitted command.
    pub(crate) fn new(
        id: CommandId,
        sandbox: Sandbox,
        snapshot_root: Option<PathBuf>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let workers = Arc::new(Workers::new(runtime, snapshot_root.clone()));
        let generation = workers.generation();
        Self {
            inner: Arc::new(Inner {
                id,
                sandbox,
                snapshot_root,
                workers,
            }),
            generation,
        }
    }

    /// Which command this is the context of.
    #[must_use]
    pub fn id(&self) -> CommandId {
        self.inner.id
    }

    /// The sandbox this command runs in.
    ///
    /// Immutable, and the authoritative answer: a helper rebasing an operand must use this rather
    /// than an environment variable a script can rewrite.
    #[must_use]
    pub fn sandbox(&self) -> &Sandbox {
        &self.inner.sandbox
    }

    /// The seed this command's job publishes into.
    ///
    /// One mux hosts jobs over several seeds, so this is the *job's* seed rather than a property
    /// of the host: borrowed from the sandbox, which is the one authoritative copy.
    #[must_use]
    pub fn seed(&self) -> &Path {
        &self.inner.sandbox.seed
    }

    /// The root of this command's own snapshot, when it has one.
    ///
    /// Staged writes belong under this path. Writing into [`seed`](Self::seed) directly bypasses
    /// the publication boundary entirely and is never correct for workload work.
    #[must_use]
    pub fn snapshot_root(&self) -> Option<&Path> {
        self.inner.snapshot_root.as_deref()
    }

    /// Whether a forced stop has asked this command's work to end.
    ///
    /// A request, not an interruption. A worker that never checks is joined rather than killed.
    /// A handle from a superseded evaluation answers `true` unconditionally: whatever it was
    /// doing is no longer wanted, whether or not anybody asked the command itself to stop.
    #[must_use]
    pub fn cancellation_requested(&self) -> bool {
        self.inner.workers.cancellation_requested() || self.superseded()
    }

    /// Resolves once cancellation has been requested.
    ///
    /// Already-requested cancellation resolves immediately, so there is no lost-wakeup window
    /// between a check and a wait.
    pub async fn cancelled(&self) {
        if self.superseded() {
            return;
        }
        self.inner.workers.cancelled().await;
    }

    /// Runs `operation` on a blocking worker this command owns.
    ///
    /// The worker is registered before this returns, so the core joins it before concluding,
    /// discarding or reclaiming: a worker can never still be writing into a snapshot that has
    /// already been reset. It runs on the runtime the command was admitted on, which is what makes
    /// this callable from a status thread or a detached queue.
    ///
    /// The returned receiver carries the operation's value. Dropping it does not cancel the
    /// worker — the work is already owned — it only stops observing the result.
    ///
    /// # Errors
    ///
    /// Fails with [`MuxError::CommandFinalizing`] once this command has stopped admitting work,
    /// which is the whole point: a retained context cannot start something after its verdict, and
    /// a handle from an evaluation that was abandoned and run again cannot start something in the
    /// replay.
    pub fn spawn_blocking<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::sync::oneshot::Receiver<T>, MuxError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.inner
            .workers
            .spawn_blocking(self.generation, operation)
            .map_err(|()| MuxError::CommandFinalizing(self.inner.id))
    }

    /// The workers this command owns, for the evaluator below the mux.
    pub(crate) fn workers(&self) -> Arc<Workers> {
        Arc::clone(&self.inner.workers)
    }

    /// Asks every worker of this command to stop.
    pub(crate) fn request_cancellation(&self) {
        self.inner.workers.request_cancellation();
    }

    /// Closes admission and joins every registered worker of the current evaluation.
    ///
    /// Called by the core before the command's boundary, so no native worker outlives the snapshot
    /// it writes into. A worker that panicked is joined like any other; its failure has already
    /// been observed by whoever held its receiver.
    pub(crate) async fn finish(&self) {
        self.inner.workers.finish().await;
    }

    /// Whether the evaluation this handle was taken during has been superseded by a replay.
    fn superseded(&self) -> bool {
        self.inner.workers.superseded(self.generation)
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::shellmux::ids::{JobDir, ShellId, SnapshotUid};

    /// A context with no sandbox of its own, for the finalization tests.
    fn context() -> CommandContext {
        CommandContext::new(
            CommandId(1),
            Sandbox {
                id: ShellId::from("t"),
                seed: PathBuf::from("/seed"),
                dir: JobDir::default(),
                uid: SnapshotUid::from("uid-t"),
            },
            None,
            tokio::runtime::Handle::current(),
        )
    }

    /// Finalizing a command that registered no workers must still complete.
    ///
    /// The join runs on a task of its own, so with nothing to join it can set the completion flag
    /// before the finisher that spawned it has subscribed. A `watch::Sender::send` in that window
    /// fails *and leaves the value untouched*, and every finisher then waits forever on a flag
    /// nobody will ever set again. This is the zero-worker race, and it is the common case: most
    /// commands register no native workers at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finishing_with_no_workers_completes() {
        let context = context();
        tokio::time::timeout(std::time::Duration::from_secs(5), context.finish())
            .await
            .expect("a command with no workers finalizes immediately");
    }

    /// A line that was invalidated and is being run again opens a new generation, and a handle
    /// somebody kept from the abandoned one may not start work in it.
    ///
    /// The work would land in a tree the replay has already thrown away and retaken, which is the
    /// same hazard admission closing at a verdict protects against — a replay is simply that
    /// verdict happening twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handle_from_an_abandoned_evaluation_cannot_start_work() {
        let retained = context();
        let workers = retained.workers();

        // What the shell below the mux does between two evaluations of one line.
        workers.finish().await;
        workers.reopen();

        assert!(
            matches!(
                retained.spawn_blocking(|| ()),
                Err(MuxError::CommandFinalizing(_))
            ),
            "the abandoned evaluation's handle is refused"
        );
        assert!(
            retained.cancellation_requested(),
            "and reads as cancelled, so a worker holding one stops looking for work"
        );

        // The handle `current_command_context` hands the replay's builtins is the live one.
        let fresh = CommandContext {
            inner: Arc::clone(&retained.inner),
            generation: workers.generation(),
        };
        let done = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&done);
        let receiver = fresh
            .spawn_blocking(move || worker.store(true, Ordering::Release))
            .expect("the replay admits its own work");
        receiver.await.expect("the worker ran");
        assert!(done.load(Ordering::Acquire));

        // And the replay's boundary still joins it, exactly as the first evaluation's did.
        tokio::time::timeout(std::time::Duration::from_secs(5), fresh.finish())
            .await
            .expect("the replay's workers are joined");
    }

    /// Every concurrent finisher waits for the same join, and a cancelled one takes nothing with
    /// it.
    ///
    /// The worker is owned by a runtime task, not by the first finisher's stack, so dropping that
    /// finisher mid-await cannot detach it — and a later finisher must still observe the worker as
    /// joined rather than find an emptied list and conclude there was nothing to wait for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_finisher_does_not_strand_a_worker() {
        let context = context();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Arc::clone(&done);
        let _receiver = context
            .spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                worker.store(true, Ordering::Release);
            })
            .expect("an open context admits work");

        // Cancelled well before the worker finishes.
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(20), context.finish());
        assert!(cancelled.await.is_err(), "the first finisher is cancelled");

        tokio::time::timeout(std::time::Duration::from_secs(5), context.finish())
            .await
            .expect("a later finisher still completes");
        assert!(
            done.load(Ordering::Acquire),
            "the worker was joined, not detached by the cancelled finisher"
        );
    }
}
