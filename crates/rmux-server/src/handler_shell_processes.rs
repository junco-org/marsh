//! The daemon's ledger of live helper workloads, and how shutdown reaches them.
//!
//! Upstream held a [`rmux_os::process_tree::ProcessTreeController`] per helper and killed the
//! process group at shutdown. There is no process group to own any more: a helper is a managed
//! pipe job, its processes belong to the shell engine, and stopping one means forcing that job and
//! discarding whatever it staged.
//!
//! What survives unchanged is the shape of the ledger, because all three of its jobs still matter:
//!
//! * an **admission limit**, so a configuration that fires helpers in a loop cannot exhaust the
//!   daemon;
//! * a **closing flag** every helper polls, so work admitted just before shutdown does not keep
//!   the daemon alive waiting for a 300-second timeout;
//! * a **cancellation signal** the owning task selects on, so a helper stops at the first
//!   opportunity rather than when something notices it later.
//!
//! # Who actually cancels
//!
//! Both ends, and deliberately. The owning task is the one that can stop *cleanly* — it knows
//! whether it is mid-collection, and it holds the execution it is waiting on — so shutdown signals
//! it and lets it act. But an owner can also be gone: a detached hook, a task whose future was
//! dropped. So the ledger additionally forces every job it still holds. A job forced twice is
//! forced once, and a job nobody forces would outlive the shutdown that was supposed to end it.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::watch;

use crate::io::{ShellHandle, ShellIo};

/// How many helper workloads may be live at once.
const MAX_SHELL_PROCESS_GROUPS: usize = 1024;

pub(in crate::handler) struct ShellProcessRegistry {
    inner: StdMutex<ShellProcessRegistryInner>,
    closing: AtomicBool,
    /// Broadcast to every live helper when shutdown begins.
    cancel: watch::Sender<bool>,
    limit: usize,
}

#[derive(Default)]
struct ShellProcessRegistryInner {
    closing: bool,
    next_id: u64,
    /// The job behind each live registration, so shutdown can force one whose owner is gone.
    jobs: HashMap<u64, (ShellIo, ShellHandle)>,
}

#[derive(Debug)]
pub(in crate::handler) enum ShellProcessRegistrationError {
    Closing,
    LimitReached { limit: usize },
}

/// One live helper's place in the ledger.
///
/// Dropping it unregisters; it never stops the job by itself, because a helper that finished
/// normally has already published its verdict and killing it on the way out would discard it.
pub(in crate::handler) struct ShellProcessGuard {
    id: u64,
    registry: Arc<ShellProcessRegistry>,
    cancel: watch::Receiver<bool>,
}

impl ShellProcessRegistry {
    pub(in crate::handler) fn new() -> Self {
        let (cancel, _receiver) = watch::channel(false);
        Self {
            inner: StdMutex::new(ShellProcessRegistryInner::default()),
            closing: AtomicBool::new(false),
            cancel,
            limit: MAX_SHELL_PROCESS_GROUPS,
        }
    }

    /// Admits one managed helper, or explains why it cannot be admitted.
    ///
    /// # Errors
    ///
    /// Fails with [`ShellProcessRegistrationError::Closing`] during shutdown and
    /// [`ShellProcessRegistrationError::LimitReached`] at the admission limit.
    pub(in crate::handler) fn register(
        self: &Arc<Self>,
        io: &ShellIo,
        job: &ShellHandle,
    ) -> Result<ShellProcessGuard, ShellProcessRegistrationError> {
        let mut inner = self
            .inner
            .lock()
            .expect("shell process registry mutex must not be poisoned");
        if inner.closing {
            return Err(ShellProcessRegistrationError::Closing);
        }
        if inner.jobs.len() >= self.limit {
            return Err(ShellProcessRegistrationError::LimitReached { limit: self.limit });
        }

        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        inner.jobs.insert(id, (io.unleased(), job.clone()));
        Ok(ShellProcessGuard {
            id,
            registry: Arc::clone(self),
            cancel: self.cancel.subscribe(),
        })
    }

    /// Closes admission and ends every helper still on the ledger.
    ///
    /// Synchronous on purpose: it is reached from both the asynchronous shutdown path and a
    /// test's drop, and neither should have to arrange a runtime to ask the daemon to stop. The
    /// forced stops are scheduled onto the engine's own runtime, which is where every other
    /// managed operation already runs.
    pub(in crate::handler) fn close_and_terminate(&self) {
        let jobs = {
            let mut inner = self
                .inner
                .lock()
                .expect("shell process registry mutex must not be poisoned");
            inner.closing = true;
            self.closing.store(true, Ordering::SeqCst);
            inner.jobs.drain().map(|(_, job)| job).collect::<Vec<_>>()
        };
        // Signalled before the forced stops so an owner that is mid-collection gets to end its own
        // execution with its own error text rather than observing a job that vanished under it.
        let _ = self.cancel.send(true);

        for (io, job) in jobs {
            let runtime = io.runtime();
            runtime.spawn(async move {
                let _ = io.stop(&job, true).await;
            });
        }
    }

    fn unregister(&self, id: u64) {
        self.inner
            .lock()
            .expect("shell process registry mutex must not be poisoned")
            .jobs
            .remove(&id);
    }
}

impl Default for ShellProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ShellProcessRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock().map_err(|_| fmt::Error)?;
        formatter
            .debug_struct("ShellProcessRegistry")
            .field("closing", &inner.closing)
            .field("active_processes", &inner.jobs.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl ShellProcessGuard {
    pub(in crate::handler) fn shutdown_started(&self) -> bool {
        self.registry.closing.load(Ordering::SeqCst)
    }

    /// Resolves when shutdown asks this helper to stop.
    ///
    /// Never resolves spuriously, and resolves immediately when shutdown already began — an owner
    /// that raced the signal must not wait out its whole timeout.
    pub(in crate::handler) async fn cancelled(&mut self) {
        if *self.cancel.borrow() {
            return;
        }
        while self.cancel.changed().await.is_ok() {
            if *self.cancel.borrow() {
                return;
            }
        }
        // The sender lives as long as the registry, so this is only reached if the registry itself
        // was dropped — which means nothing is left to wait for.
        std::future::pending::<()>().await;
    }
}

impl Drop for ShellProcessGuard {
    fn drop(&mut self) {
        self.registry.unregister(self.id);
    }
}
