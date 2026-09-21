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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
#[must_use]
pub fn current_command_context() -> Option<CommandContext> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Runs `body` with `context` installed as the current command context.
pub(crate) async fn with_context<F: Future>(context: CommandContext, body: F) -> F::Output {
    CURRENT.scope(context, body).await
}

/// The cooperative cancellation flag one command's native workers share.
#[derive(Debug, Default)]
struct Cancellation {
    /// Set once, never cleared.
    requested: AtomicBool,
    /// Wakes every future waiting on the flag.
    signal: tokio::sync::Notify,
}

/// The workers one command registered, and whether it still admits new ones.
#[derive(Debug, Default)]
struct Workers {
    /// Cleared before finalization: a retained context cannot start work after its verdict.
    open: bool,
    /// Every registered worker, joined by the core before the command concludes.
    handles: Vec<tokio::task::JoinHandle<()>>,
}

/// The shared half of a command context.
#[derive(Debug)]
struct Inner {
    /// Which command this is the context of.
    id: CommandId,
    /// The sandbox it runs in: its identity, its directory label and its snapshot id.
    sandbox: Sandbox,
    /// The seed this mux publishes into.
    seed: Option<PathBuf>,
    /// The root of this command's own snapshot.
    snapshot_root: Option<PathBuf>,
    /// The runtime the command was admitted on, so a worker started from a foreign thread still
    /// lands on the daemon's own runtime.
    runtime: tokio::runtime::Handle,
    /// The cooperative cancellation flag.
    cancellation: Cancellation,
    /// The registered native workers.
    workers: Mutex<Workers>,
    /// Whether the single join has been started, and how every finisher learns it is over.
    ///
    /// Finalization cannot be "drain the list and await it here". Two things break that:
    /// concurrency — a second caller arriving mid-drain finds the list empty and concludes there
    /// is nothing to wait for — and cancellation — the first caller's stack owns the handles, so
    /// dropping that future detaches the very workers the next caller needs to wait for. Either
    /// way someone reclaims a snapshot while a worker is still writing into it.
    ///
    /// So the join happens exactly once, on a task the *runtime* owns, and every finisher waits on
    /// this. A cancelled finisher takes nothing with it.
    joined: tokio::sync::watch::Sender<bool>,
    /// Set when the join task has been spawned, so it is spawned once.
    joining: AtomicBool,
}

/// A handle on the managed command running on this task.
///
/// Cloneable and cheap. Holding one past the command's end is safe and useless: admission closes
/// before finalization, so [`Self::spawn_blocking`] then refuses.
#[derive(Clone, Debug)]
pub struct CommandContext {
    /// The shared state.
    inner: Arc<Inner>,
}

impl CommandContext {
    /// Builds a context for one admitted command.
    pub(crate) fn new(
        id: CommandId,
        sandbox: Sandbox,
        seed: Option<PathBuf>,
        snapshot_root: Option<PathBuf>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                id,
                sandbox,
                seed,
                snapshot_root,
                runtime,
                cancellation: Cancellation::default(),
                joined: tokio::sync::watch::channel(false).0,
                joining: AtomicBool::new(false),
                workers: Mutex::new(Workers {
                    open: true,
                    handles: Vec::new(),
                }),
            }),
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

    /// The seed this mux publishes into, when it has one.
    #[must_use]
    pub fn seed(&self) -> Option<&Path> {
        self.inner.seed.as_deref()
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
    #[must_use]
    pub fn cancellation_requested(&self) -> bool {
        self.inner.cancellation.requested.load(Ordering::Acquire)
    }

    /// Resolves once cancellation has been requested.
    ///
    /// Already-requested cancellation resolves immediately, so there is no lost-wakeup window
    /// between a check and a wait.
    pub async fn cancelled(&self) {
        loop {
            // Registered before the check, so a request landing between them still wakes this.
            let notified = self.inner.cancellation.signal.notified();
            if self.cancellation_requested() {
                return;
            }
            notified.await;
        }
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
    /// which is the whole point: a retained context cannot start something after its verdict.
    pub fn spawn_blocking<F, T>(&self, operation: F) -> Result<tokio::sync::oneshot::Receiver<T>, MuxError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut workers = self
            .inner
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !workers.open {
            return Err(MuxError::CommandFinalizing(self.inner.id));
        }
        let handle = self.inner.runtime.spawn_blocking(move || {
            let value = operation();
            let _ = sender.send(value);
        });
        workers.handles.push(handle);
        drop(workers);
        Ok(receiver)
    }

    /// Asks every worker of this command to stop.
    pub(crate) fn request_cancellation(&self) {
        self.inner
            .cancellation
            .requested
            .store(true, Ordering::Release);
        self.inner.cancellation.signal.notify_waiters();
    }

    /// Closes admission and joins every registered worker.
    ///
    /// Called by the core before the command's boundary, so no native worker outlives the snapshot
    /// it writes into. A worker that panicked is joined like any other; its failure has already
    /// been observed by whoever held its receiver.
    pub(crate) async fn finish(&self) {
        // Exactly one finisher starts the join, on a task the runtime owns. Everyone else — and
        // the starter too — waits on the shared signal below, so cancelling any of them detaches
        // nothing and a later caller never sees an emptied list it did not wait for.
        if !self.inner.joining.swap(true, Ordering::AcqRel) {
            let handles = {
                let mut workers = self
                    .inner
                    .workers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                workers.open = false;
                let handles = std::mem::take(&mut workers.handles);
                drop(workers);
                handles
            };
            let joined = self.inner.joined.clone();
            self.inner.runtime.spawn(async move {
                for handle in handles {
                    // A worker that panicked is joined like any other; its failure has already
                    // been observed by whoever held its receiver.
                    let _ = handle.await;
                }
                // `send_replace`, not `send`. With no receiver yet subscribed — and the finisher
                // that spawned this only subscribes afterwards — `send` fails AND leaves the
                // value untouched, so every finisher would then wait forever on a flag that was
                // never set. `send_replace` updates the value whether or not anyone is listening.
                let _ = joined.send_replace(true);
            });
        }

        let mut done = self.inner.joined.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                // The sender is gone, which can only mean the join task was itself lost. There is
                // nothing better to wait for, and hanging here would stall a boundary forever.
                return;
            }
        }
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellmux::ids::{JobDir, ShellId, SnapshotUid};

    /// A context with no sandbox of its own, for the finalization tests.
    fn context() -> CommandContext {
        CommandContext::new(
            CommandId(1),
            Sandbox {
                id: ShellId::from("t"),
                dir: JobDir::default(),
                uid: SnapshotUid::from("uid-t"),
            },
            None,
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
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(20), context.finish());
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
