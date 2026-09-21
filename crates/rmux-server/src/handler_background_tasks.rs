use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rmux_proto::RmuxError;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

#[cfg(test)]
use super::lifecycle_producer_tasks::LifecycleProducerCancellation;
use super::lifecycle_producer_tasks::{
    run_registered_lifecycle_producer, run_registered_lifecycle_producer_with_cancellation_cleanup,
    LifecycleProducerRegistration,
};
use super::RequestHandler;

const MAX_BACKGROUND_TASKS: usize = 1024;
/// The stack every background task's thread reserves.
///
/// Eight megabytes was sized for the old backend, where a background task's deepest act was a
/// `Command::spawn`. A background task now runs a *managed* workload: `ShellIo::execute` down
/// through the shell builder, the brush interpreter and the publication gate. That chain is
/// genuinely deeper — not merely a larger future, which is already boxed at the `block_on`
/// boundary — and it overflows eight megabytes in an unoptimised build, aborting the whole
/// process rather than failing the one task.
///
/// Thirty-two megabytes is a reservation, not an allocation. Linux commits thread-stack pages on
/// first touch, so a task that never goes deep costs the same resident memory it did before; what
/// grows is virtual address space, which is free on 64-bit and bounded anyway by
/// [`MAX_BACKGROUND_TASKS`].
const BACKGROUND_TASK_STACK_SIZE: usize = 32 * 1024 * 1024;

/// The stack a background task's thread is given.
///
/// [`BACKGROUND_TASK_STACK_SIZE`] is the floor, not the answer. A pinned size also makes
/// `RUST_MIN_STACK` inert for exactly the threads that need it most: the variable is honoured when
/// a thread is spawned without an explicit size, and ignored when one is given — which is why
/// these threads used to abort under a test harness that had set it.
///
/// So the explicit size is the larger of the two: the floor above, or whatever a caller asked
/// for.
fn background_task_stack_size() -> usize {
    std::env::var("RUST_MIN_STACK")
        .ok()
        .and_then(|requested| requested.parse::<usize>().ok())
        .unwrap_or(0)
        .max(BACKGROUND_TASK_STACK_SIZE)
}
const BACKGROUND_TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const BACKGROUND_TASK_JOIN_POLL: Duration = Duration::from_millis(10);

static BACKGROUND_TASK_LIMITER: OnceLock<BackgroundTaskLimiter> = OnceLock::new();

#[cfg(test)]
#[derive(Debug)]
pub(in crate::handler) struct PreAdmittedProducerSpawnPause {
    pub(in crate::handler) reached: Notify,
    released: StdMutex<bool>,
    release: std::sync::Condvar,
}

#[cfg(test)]
impl PreAdmittedProducerSpawnPause {
    pub(in crate::handler) fn release(&self) {
        *self
            .released
            .lock()
            .expect("pre-admitted producer spawn pause lock") = true;
        self.release.notify_one();
    }

    fn wait_in_caller(&self) {
        self.reached.notify_one();
        let mut released = self
            .released
            .lock()
            .expect("pre-admitted producer spawn pause lock");
        while !*released {
            released = self
                .release
                .wait(released)
                .expect("pre-admitted producer spawn pause lock");
        }
    }
}

#[cfg(test)]
impl Default for PreAdmittedProducerSpawnPause {
    fn default() -> Self {
        Self {
            reached: Notify::new(),
            released: StdMutex::new(false),
            release: std::sync::Condvar::new(),
        }
    }
}

#[cfg(test)]
static PRE_ADMITTED_PRODUCER_SPAWN_PAUSES: StdMutex<
    Vec<(usize, &'static str, Arc<PreAdmittedProducerSpawnPause>)>,
> = StdMutex::new(Vec::new());

pub(in crate::handler) struct BackgroundTaskRegistry {
    inner: StdMutex<BackgroundTaskRegistryInner>,
    shutdown: watch::Sender<bool>,
}

struct BackgroundTaskRegistryInner {
    closing: bool,
    tasks: Vec<TrackedBackgroundTask>,
}

struct TrackedBackgroundTask {
    name: &'static str,
    handle: JoinHandle<()>,
}

pub(crate) struct BackgroundTaskShutdown {
    tasks: Vec<TrackedBackgroundTask>,
}

impl BackgroundTaskRegistry {
    pub(in crate::handler) fn new() -> Self {
        let (shutdown, _receiver) = watch::channel(false);
        Self {
            inner: StdMutex::new(BackgroundTaskRegistryInner {
                closing: false,
                tasks: Vec::new(),
            }),
            shutdown,
        }
    }

    fn spawn<Fut, Factory>(
        &self,
        thread_name: &'static str,
        runtime: tokio::runtime::Handle,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.spawn_with_shutdown_policy(
            thread_name,
            runtime,
            BackgroundShutdownPolicy::Cancel,
            factory,
        )
    }

    #[cfg(test)]
    fn spawn_lifecycle_producer<Fut, Factory>(
        &self,
        thread_name: &'static str,
        runtime: tokio::runtime::Handle,
        cancellation: LifecycleProducerCancellation,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.spawn_with_shutdown_policy(
            thread_name,
            runtime,
            BackgroundShutdownPolicy::DrainMutation(cancellation),
            factory,
        )
    }

    fn spawn_with_shutdown_policy<Fut, Factory>(
        &self,
        thread_name: &'static str,
        runtime: tokio::runtime::Handle,
        shutdown_policy: BackgroundShutdownPolicy,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let permit = background_task_limiter().try_acquire()?;
        let mut inner = self
            .inner
            .lock()
            .expect("background task registry mutex must not be poisoned");
        if inner.closing {
            return Err(RmuxError::Server(format!(
                "background task '{thread_name}' rejected during server shutdown"
            )));
        }
        reap_finished_tasks(&mut inner.tasks);

        let mut shutdown = self.shutdown.subscribe();
        let handle = std::thread::Builder::new()
            .name(thread_name.to_owned())
            .stack_size(background_task_stack_size())
            .spawn(move || {
                run_background_task(factory, &runtime, &mut shutdown, permit, shutdown_policy);
            })
            .map_err(|_| {
                RmuxError::Server(format!("failed to spawn background task '{thread_name}'"))
            })?;
        // The registry lock spans spawn plus insertion. Shutdown therefore
        // cannot observe a running thread without also taking ownership of its
        // join handle.
        inner.tasks.push(TrackedBackgroundTask {
            name: thread_name,
            handle,
        });
        Ok(())
    }

    pub(crate) fn begin_shutdown(&self) -> BackgroundTaskShutdown {
        let tasks = {
            let mut inner = self
                .inner
                .lock()
                .expect("background task registry mutex must not be poisoned");
            inner.closing = true;
            self.shutdown.send_replace(true);
            std::mem::take(&mut inner.tasks)
        };
        BackgroundTaskShutdown { tasks }
    }

    #[cfg(test)]
    fn task_running(&self, name: &'static str) -> bool {
        self.inner
            .lock()
            .expect("background task registry mutex must not be poisoned")
            .tasks
            .iter()
            .any(|task| task.name == name && !task.handle.is_finished())
    }
}

enum BackgroundShutdownPolicy {
    Cancel,
    #[cfg(test)]
    DrainMutation(LifecycleProducerCancellation),
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for BackgroundTaskRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock().map_err(|_| fmt::Error)?;
        formatter
            .debug_struct("BackgroundTaskRegistry")
            .field("closing", &inner.closing)
            .field("tracked_tasks", &inner.tasks.len())
            .finish()
    }
}

impl RequestHandler {
    pub(in crate::handler) fn reserve_lifecycle_producer_task(
        &self,
        thread_name: &'static str,
    ) -> Result<LifecycleProducerRegistration, RmuxError> {
        self.lifecycle_producers.try_register().ok_or_else(|| {
            RmuxError::Server(format!(
                "lifecycle producer '{thread_name}' rejected during server shutdown"
            ))
        })
    }

    pub(in crate::handler) fn spawn_pre_admitted_lifecycle_producer_task_handle<Fut>(
        &self,
        thread_name: &'static str,
        admission: LifecycleProducerRegistration,
        task: Fut,
    ) -> tokio::task::JoinHandle<()>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        #[cfg(not(test))]
        let _ = thread_name;
        #[cfg(test)]
        self.pause_before_pre_admitted_producer_spawn(thread_name);
        tokio::spawn(async move {
            let _ = run_registered_lifecycle_producer(admission, task).await;
        })
    }

    pub(in crate::handler) fn spawn_pre_admitted_lifecycle_producer_task_with_cleanup<
        Fut,
        Cleanup,
    >(
        &self,
        thread_name: &'static str,
        admission: LifecycleProducerRegistration,
        task: Fut,
        cleanup: Cleanup,
    ) where
        Fut: Future<Output = ()> + Send + 'static,
        Cleanup: Future<Output = ()> + Send + 'static,
    {
        drop(
            self.spawn_pre_admitted_lifecycle_producer_task_with_cleanup_handle(
                thread_name,
                admission,
                task,
                cleanup,
            ),
        );
    }

    pub(in crate::handler) fn spawn_pre_admitted_lifecycle_producer_task_with_cleanup_handle<
        Fut,
        Cleanup,
    >(
        &self,
        thread_name: &'static str,
        admission: LifecycleProducerRegistration,
        task: Fut,
        cleanup: Cleanup,
    ) -> tokio::task::JoinHandle<()>
    where
        Fut: Future<Output = ()> + Send + 'static,
        Cleanup: Future<Output = ()> + Send + 'static,
    {
        #[cfg(not(test))]
        let _ = thread_name;
        #[cfg(test)]
        self.pause_before_pre_admitted_producer_spawn(thread_name);
        tokio::spawn(async move {
            let _ = run_registered_lifecycle_producer_with_cancellation_cleanup(
                admission, task, cleanup,
            )
            .await;
        })
    }

    #[cfg(test)]
    pub(in crate::handler) fn install_pre_admitted_producer_spawn_pause(
        &self,
        thread_name: &'static str,
    ) -> Arc<PreAdmittedProducerSpawnPause> {
        let handler_key = Arc::as_ptr(&self.lifecycle_producers) as usize;
        let pause = Arc::new(PreAdmittedProducerSpawnPause::default());
        let mut pauses = PRE_ADMITTED_PRODUCER_SPAWN_PAUSES
            .lock()
            .expect("pre-admitted producer spawn pause registry lock");
        assert!(
            !pauses
                .iter()
                .any(|(key, name, _)| *key == handler_key && *name == thread_name),
            "pre-admitted producer spawn pause already installed"
        );
        pauses.push((handler_key, thread_name, Arc::clone(&pause)));
        pause
    }

    #[cfg(test)]
    fn pause_before_pre_admitted_producer_spawn(&self, thread_name: &'static str) {
        let handler_key = Arc::as_ptr(&self.lifecycle_producers) as usize;
        let pause = {
            let mut pauses = PRE_ADMITTED_PRODUCER_SPAWN_PAUSES
                .lock()
                .expect("pre-admitted producer spawn pause registry lock");
            pauses
                .iter()
                .position(|(key, name, _)| *key == handler_key && *name == thread_name)
                .map(|position| pauses.swap_remove(position).2)
        };
        if let Some(pause) = pause {
            pause.wait_in_caller();
        }
    }

    /// Spawns delayed work that can later enter the shared mutation dispatcher.
    #[cfg(test)]
    pub(in crate::handler) fn spawn_lifecycle_producer_task<Fut, Factory>(
        &self,
        thread_name: &'static str,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let admission = self.reserve_lifecycle_producer_task(thread_name)?;
        self.spawn_registered_lifecycle_producer_task(thread_name, admission, factory)
    }

    pub(in crate::handler) fn spawn_background_task<Fut, Factory>(
        &self,
        thread_name: &'static str,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.background_tasks
            .spawn(thread_name, self.background_task_runtime(thread_name)?, factory)
    }

    #[cfg(test)]
    pub(in crate::handler) fn spawn_registered_lifecycle_producer_task<Fut, Factory>(
        &self,
        thread_name: &'static str,
        admission: LifecycleProducerRegistration,
        factory: Factory,
    ) -> Result<(), RmuxError>
    where
        Factory: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let cancellation = admission.cancellation();
        self.background_tasks.spawn_lifecycle_producer(
            thread_name,
            self.background_task_runtime(thread_name)?,
            cancellation,
            move || async move {
                let _ = run_registered_lifecycle_producer(admission, factory()).await;
            },
        )
    }

    #[cfg(test)]
    pub(in crate::handler) fn background_task_running_for_test(&self, name: &'static str) -> bool {
        self.background_tasks.task_running(name)
    }

    /// The runtime a background task drives its future on.
    ///
    /// The daemon's own, so managed work a task admits lands on the same executor as the engine's
    /// pumps and native workers. The ambient runtime is the fallback for a handler built without
    /// a captured one; neither being available means there is no reactor to run on at all, which
    /// is a refusal rather than a task that would hang on its first timer.
    ///
    /// # Errors
    ///
    /// Fails with [`RmuxError::Server`] when no runtime is available.
    fn background_task_runtime(
        &self,
        thread_name: &'static str,
    ) -> Result<tokio::runtime::Handle, RmuxError> {
        self.server_task_runtime()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or_else(|| {
                RmuxError::Server(format!(
                    "background task '{thread_name}' has no runtime to run on"
                ))
            })
    }

    pub(crate) async fn shutdown_background_tasks_and_shell_processes(&self) -> Vec<&'static str> {
        let shutdown = self.background_tasks.begin_shutdown();
        self.shell_processes.close_and_terminate();
        tokio::task::spawn_blocking(move || shutdown.join(BACKGROUND_TASK_SHUTDOWN_GRACE))
            .await
            .unwrap_or_else(|_| vec!["background-task-join-worker"])
    }

    #[cfg(test)]
    pub(in crate::handler) fn shutdown_background_tasks_for_drop(&self) {
        let shutdown = self.background_tasks.begin_shutdown();
        self.shell_processes.close_and_terminate();
        let _ = shutdown.join(BACKGROUND_TASK_SHUTDOWN_GRACE);
    }
}

impl BackgroundTaskShutdown {
    fn join(mut self, timeout: Duration) -> Vec<&'static str> {
        let deadline = Instant::now() + timeout;
        while self.tasks.iter().any(|task| !task.handle.is_finished()) && Instant::now() < deadline
        {
            std::thread::sleep(BACKGROUND_TASK_JOIN_POLL);
        }

        let mut unfinished = Vec::new();
        for task in self.tasks.drain(..) {
            if task.handle.is_finished() {
                let _ = task.handle.join();
            } else {
                unfinished.push(task.name);
            }
        }
        unfinished
    }
}

/// Runs one background task on the daemon's own runtime.
///
/// The thread and its generous stack stay: several of these tasks re-enter the command
/// dispatcher, whose recursion depth is bounded by the stack rather than by a counter, and the
/// joined lifetime is what lets shutdown report which ones did not finish.
///
/// What is gone is the private current-thread runtime each invocation used to build. Managed work
/// admits commands, pumps and native workers onto the *engine's* runtime, and a future driven by
/// a second, short-lived runtime would be registering timers and readiness with a reactor that
/// disappears the moment this task ends. Blocking on the daemon's handle keeps one reactor for
/// the whole daemon while still driving the future on this thread, which is what allows the
/// non-`Send` futures these callers pass.
fn run_background_task<Fut, Factory>(
    factory: Factory,
    runtime: &tokio::runtime::Handle,
    shutdown: &mut watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
    shutdown_policy: BackgroundShutdownPolicy,
) where
    Factory: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    runtime.block_on(async move {
        let mut task = Box::pin(factory());
        tokio::select! {
            biased;
            _ = wait_for_shutdown(shutdown) => {
                match shutdown_policy {
                    BackgroundShutdownPolicy::Cancel => {}
                    #[cfg(test)]
                    BackgroundShutdownPolicy::DrainMutation(cancellation)
                        if cancellation.is_mutating() => {
                            tokio::select! {
                                biased;
                                _ = cancellation.wait_until_pending() => {}
                                _ = task.as_mut() => {}
                            }
                        }
                    #[cfg(test)]
                    BackgroundShutdownPolicy::DrainMutation(_) => {
                    }
                }
            }
            _ = task.as_mut() => {}
        }
    });
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

fn reap_finished_tasks(tasks: &mut Vec<TrackedBackgroundTask>) {
    let mut index = 0;
    while index < tasks.len() {
        if tasks[index].handle.is_finished() {
            let task = tasks.swap_remove(index);
            let _ = task.handle.join();
        } else {
            index += 1;
        }
    }
}

fn background_task_limiter() -> &'static BackgroundTaskLimiter {
    BACKGROUND_TASK_LIMITER.get_or_init(|| BackgroundTaskLimiter::new(MAX_BACKGROUND_TASKS))
}

struct BackgroundTaskLimiter {
    semaphore: Arc<Semaphore>,
    max_tasks: usize,
}

impl BackgroundTaskLimiter {
    fn new(max_tasks: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_tasks)),
            max_tasks,
        }
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, RmuxError> {
        self.semaphore.clone().try_acquire_owned().map_err(|_| {
            RmuxError::Server(format!(
                "too many background tasks; limit is {}",
                self.max_tasks
            ))
        })
    }
}

#[cfg(test)]
#[path = "handler_background_tasks/tests.rs"]
mod tests;
