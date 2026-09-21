//! The status line's `#(command)` producers, as managed work.
//!
//! A status job is the one workload in this daemon that is *read synchronously*. The status line
//! is rendered from inside the renderer, which has no `await` to give: it asks for a value and
//! gets whatever the cache holds right now. That constraint is unchanged and non-negotiable, so
//! this module keeps its shape — a keyed cache, a staleness check, a bounded number of live
//! producers — and replaces only what was underneath it.
//!
//! Underneath it used to be a `/bin/sh` child per generation: a raw host spawn, an owned process
//! group, a polling thread that drained the pipe every 10ms, and a termination dance to reap the
//! descendants that inherited the write end. All of that is gone. A status job is now a managed
//! pipe job scheduled on the daemon runtime, cancelled through its own job rather than through
//! signals to a process group, and — this is the part a user can see — published into the cache
//! only when the gate approved it.
//!
//! # What a refusal now does
//!
//! Upstream wrote an empty string into the cache whenever a generation produced nothing: a
//! timeout, a spawn failure, a cancelled job. The status line blanked. That is wrong for a
//! producer whose last good answer is still the best answer available, and it is especially wrong
//! for a *refused* one, because blanking on a policy decision reads as "the command printed
//! nothing" rather than "the command was not allowed to run". So a generation that does not
//! produce an approved value leaves the previous one exactly where it was, and the slot simply
//! tries again at the next tick.
//!
//! # Limits that are preserved exactly
//!
//! * 256 cached entries, evicted oldest-completed-first;
//! * 32 live producers, above which a stale slot is not refreshed at all;
//! * 750ms per generation on Unix, 5s on Windows;
//! * 64KiB of retained output per generation.
//!
//! The cap is now a *combined* budget across standard output and standard error rather than a
//! standard-output-only one, because a managed collection retains both streams. Upstream sent
//! standard error to `/dev/null`; [`status_job_stdout`] still reads only `stdout`, so a chatty
//! diagnostic cannot reach the status line — it can only consume budget the command's real output
//! would otherwise have had.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use marsh_core::shellmux::ShellId;
use rmux_proto::{ProcessCommand, RmuxError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::io::{CapturedOutput, ExecutionSpec, IoResult, ShellHandle, ShellIo};
use crate::managed_workload;
use crate::terminal::TerminalProfile;

#[cfg(windows)]
const STATUS_JOB_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(windows))]
const STATUS_JOB_TIMEOUT: Duration = Duration::from_millis(750);
const STATUS_JOB_CACHE_LIMIT: usize = 256;
const STATUS_JOB_OUTPUT_LIMIT: usize = 64 * 1024;
const STATUS_JOB_ACTIVE_LIMIT: usize = 32;
/// Names this path in [`crate::diagnostic_log::record_workload_not_run`].
///
/// A `#(command)` producer renders a string and has no error channel of its own, so every way one
/// can fail to yield a value is invisible to the user by construction: the slot simply keeps its
/// previous approved text. This is the only record that distinguishes "refused by the gate" from
/// "never ran" afterwards.
const STATUS_JOB: &str = "status-job";

pub(crate) struct StatusJobRuntime {
    inner: Arc<StatusJobRuntimeInner>,
}

struct StatusJobRuntimeInner {
    state: Mutex<StatusJobRuntimeState>,
    shutdown: Mutex<()>,
    owners: AtomicUsize,
}

#[derive(Default)]
struct StatusJobRuntimeState {
    closing: bool,
    next_job_id: u64,
    /// Mints the stable job name each new cache slot keeps for the rest of its life.
    next_shell_id: u64,
    cache: HashMap<StatusJobKey, StatusJobCacheEntry>,
    active: HashMap<u64, ActiveStatusJob>,
}

/// One generation of one cache slot, while it runs.
struct ActiveStatusJob {
    /// Set to `true` to ask this generation to stop at its first opportunity.
    cancel: watch::Sender<bool>,
    /// The worker task. Dropping it detaches rather than aborts, which is why cancellation is a
    /// signal the worker acts on rather than something done to it.
    worker: JoinHandle<()>,
    /// The admitted job, from the moment the worker has one until it concludes.
    ///
    /// Shutdown needs it because the worker is not guaranteed to still be there: a task the
    /// runtime dropped cannot stop the job it admitted, and a job nobody stops outlives the
    /// shutdown that was meant to end it.
    job: Option<(ShellIo, ShellHandle)>,
    completed: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StatusJobKey {
    command: String,
    shell: Option<OsString>,
    cwd: Option<OsString>,
    environment: Option<Arc<Vec<(OsString, OsString)>>>,
}

impl StatusJobKey {
    fn new(command: &str, profile: Option<&TerminalProfile>) -> Self {
        Self {
            command: command.to_owned(),
            shell: profile.map(|profile| profile.shell().as_os_str().to_owned()),
            cwd: profile.map(|profile| profile.cwd().as_os_str().to_owned()),
            environment: profile.map(status_job_environment_key),
        }
    }
}

fn status_job_environment_key(profile: &TerminalProfile) -> Arc<Vec<(OsString, OsString)>> {
    let mut environment = profile
        .raw_environment()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect::<Vec<_>>();
    environment.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Arc::new(environment)
}

struct StatusJobCacheEntry {
    output: String,
    updated_at: Option<Instant>,
    in_flight: bool,
    /// The job name every generation of this slot runs as.
    ///
    /// Minted once, when the slot is created, and reused for the rest of its life. A status
    /// producer is a *recurring* workload, so a fresh principal per generation would write a new
    /// name into the policy history every `status-interval` and make the slot's own approved state
    /// unreachable to the next run. Because `in_flight` admits one generation per slot at a time,
    /// the name is never live twice. Evicting the slot retires it.
    shell_id: ShellId,
}

impl StatusJobCacheEntry {
    /// An empty slot that has never produced a value.
    const fn new(shell_id: ShellId) -> Self {
        Self {
            output: String::new(),
            updated_at: None,
            in_flight: false,
            shell_id,
        }
    }
}

impl StatusJobRuntime {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(StatusJobRuntimeInner {
                state: Mutex::new(StatusJobRuntimeState::default()),
                shutdown: Mutex::new(()),
                owners: AtomicUsize::new(1),
            }),
        }
    }

    /// The value this `#(command)` currently renders as, scheduling a refresh when it is stale.
    ///
    /// Synchronous, and it always answers immediately: the renderer has no way to wait. A stale
    /// slot returns its previous value *and* puts a new generation on the daemon runtime, so the
    /// next render picks up the result. That is exactly tmux's observable behaviour, where the
    /// first expansion of a job is empty and each later one carries the previous run's output.
    ///
    /// `io` is `None` before the daemon has bound its shell facade, and in the renderer's own unit
    /// tests. That is an ordinary answer, not a failure: the cached value is returned and nothing
    /// is scheduled, because there is nothing to schedule onto.
    pub(crate) fn cached_output(
        &self,
        io: Option<&ShellIo>,
        command: &str,
        profile: Option<&TerminalProfile>,
        cache_ttl: Duration,
    ) -> String {
        let now = Instant::now();
        let key = StatusJobKey::new(command, profile);
        let mut state = self.inner.lock_state();
        reap_completed_workers(&mut state);
        if state.closing {
            return state
                .cache
                .get(&key)
                .map(|entry| entry.output.clone())
                .unwrap_or_default();
        }

        ensure_status_job_cache_capacity(&mut state.cache, &key, now);
        let active_limit_reached = state.active.len() >= STATUS_JOB_ACTIVE_LIMIT;
        if !state.cache.contains_key(&key) {
            let shell_id = status_job_shell_id(state.next_shell_id);
            state.next_shell_id = state.next_shell_id.wrapping_add(1);
            state
                .cache
                .insert(key.clone(), StatusJobCacheEntry::new(shell_id));
        }
        let entry = state
            .cache
            .get_mut(&key)
            .expect("the slot was just created when it was missing");
        let cached = entry.output.clone();
        let stale = entry
            .updated_at
            .is_none_or(|updated_at| now.duration_since(updated_at) >= cache_ttl);
        if !stale || entry.in_flight || active_limit_reached {
            return cached;
        }
        let Some(io) = io else {
            return cached;
        };

        entry.in_flight = true;
        let shell_id = entry.shell_id.clone();
        let job_id = state.next_job_id;
        state.next_job_id = state.next_job_id.wrapping_add(1);

        let (cancel, cancelled) = watch::channel(false);
        let worker_io = io.unleased();
        let worker_inner = Arc::downgrade(&self.inner);
        let worker_key = key;
        let worker_command = command.to_owned();
        let worker_profile = profile.cloned();
        let worker = io.runtime().spawn(async move {
            let output = produce_status_job_value(
                &worker_io,
                &worker_inner,
                job_id,
                &worker_command,
                worker_profile.as_ref(),
                shell_id,
                cancelled,
            )
            .await;
            if let Some(runtime) = worker_inner.upgrade() {
                runtime.complete_job(job_id, &worker_key, output);
            }
        });
        state.active.insert(
            job_id,
            ActiveStatusJob {
                cancel,
                worker,
                job: None,
                completed: false,
            },
        );
        cached
    }

    /// Closes the runtime and ends every generation still running.
    ///
    /// There is nothing left to *join*: the workers are tasks on the daemon runtime, not the OS
    /// threads this used to own, and a synchronous caller cannot await a task. The name is kept
    /// because three callers depend on it, and because what it guarantees is unchanged — when it
    /// returns, no generation can publish into the cache and every job still open has been told to
    /// stop. Idempotent: a second call finds an empty ledger and a `closing` flag already set.
    pub(crate) fn shutdown_and_join(&self) {
        let _shutdown = self
            .inner
            .shutdown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (io, shell) in self.inner.begin_shutdown() {
            // Forced, so the whole job goes and nothing it staged is published on the way out.
            let runtime = io.runtime();
            runtime.spawn(async move {
                let _ = io.stop(&shell, true).await;
            });
        }
        self.inner.finish_shutdown();
    }

    #[cfg(test)]
    fn active_job_count(&self) -> usize {
        self.inner
            .lock_state()
            .active
            .values()
            .filter(|job| !job.completed)
            .count()
    }

    #[cfg(test)]
    fn seed_cache(&self, key: StatusJobKey, entry: StatusJobCacheEntry) {
        self.inner.lock_state().cache.insert(key, entry);
    }

    #[cfg(test)]
    pub(crate) fn seed_completed_output(&self, command: &str, output: &str) {
        let key = StatusJobKey::new(command, None);
        let mut state = self.inner.lock_state();
        let shell_id = status_job_shell_id(state.next_shell_id);
        state.next_shell_id = state.next_shell_id.wrapping_add(1);
        state.cache.insert(
            key,
            StatusJobCacheEntry {
                output: output.to_owned(),
                updated_at: Some(Instant::now()),
                in_flight: false,
                shell_id,
            },
        );
    }

    #[cfg(test)]
    fn cache_entry_in_flight(&self, key: &StatusJobKey) -> bool {
        self.inner
            .lock_state()
            .cache
            .get(key)
            .is_some_and(|entry| entry.in_flight)
    }
}

impl Default for StatusJobRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for StatusJobRuntime {
    fn clone(&self) -> Self {
        self.inner.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for StatusJobRuntime {
    fn drop(&mut self) {
        if self.inner.owners.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shutdown_and_join();
        }
    }
}

impl fmt::Debug for StatusJobRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.lock_state();
        formatter
            .debug_struct("StatusJobRuntime")
            .field("closing", &state.closing)
            .field("cached_jobs", &state.cache.len())
            .field("active_jobs", &state.active.len())
            .finish()
    }
}

impl StatusJobRuntimeInner {
    fn lock_state(&self) -> MutexGuard<'_, StatusJobRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records the job one generation admitted, so shutdown can reach it.
    fn record_job(&self, job_id: u64, io: ShellIo, shell: ShellHandle) {
        if let Some(job) = self.lock_state().active.get_mut(&job_id) {
            job.job = Some((io, shell));
        }
    }

    /// Retires one generation, publishing its value only if it produced one.
    ///
    /// `None` — a timeout, a cancellation, an engine refusal, an unapproved verdict — leaves
    /// `output` and `updated_at` untouched, so the slot keeps rendering its last approved value
    /// and becomes stale again immediately, which schedules the next attempt on the next render.
    ///
    /// The process's exit status is deliberately not consulted. Upstream cached the standard
    /// output of a command that exited nonzero, `#()` has no channel to report a status through,
    /// and a status producer that reports failure by printing is entirely ordinary. The gate's
    /// verdict is the one thing checked, because it is the one thing that decides whether the
    /// value may be trusted.
    fn complete_job(&self, job_id: u64, key: &StatusJobKey, output: Option<String>) {
        let mut state = self.lock_state();
        let closing = state.closing;
        if let Some(entry) = state.cache.get_mut(key) {
            if let Some(output) = output {
                // Refuses to publish during shutdown: the value would never be rendered, and
                // writing it would resurrect a slot the teardown has already released.
                if !closing {
                    entry.output = output;
                    entry.updated_at = Some(Instant::now());
                }
            }
            entry.in_flight = false;
        }
        if let Some(job) = state.active.get_mut(&job_id) {
            job.completed = true;
            // The job has concluded; there is nothing left for shutdown to force.
            job.job = None;
        }
    }

    /// Closes admission, signals every generation, and hands back the jobs to force.
    fn begin_shutdown(&self) -> Vec<(ShellIo, ShellHandle)> {
        let mut state = self.lock_state();
        state.closing = true;
        for entry in state.cache.values_mut() {
            entry.in_flight = false;
        }
        let mut jobs = Vec::new();
        for job in state.active.values_mut() {
            // Signalled before the forced stops so a worker that is mid-collection ends its own
            // execution rather than observing a job that vanished under it.
            let _ = job.cancel.send(true);
            if let Some(admitted) = job.job.take() {
                jobs.push(admitted);
            }
        }
        jobs
    }

    fn finish_shutdown(&self) {
        self.lock_state().active.clear();
    }
}

/// Frees the admission slots of generations that are over.
///
/// A worker marks its slot completed immediately before it returns; a worker the runtime dropped
/// never will, and its handle is the only thing that knows it is gone. Either answer frees the
/// slot, so a lost task cannot hold one of the 32 admissions for the rest of the daemon's life.
fn reap_completed_workers(state: &mut StatusJobRuntimeState) {
    state
        .active
        .retain(|_, job| !job.completed && !job.worker.is_finished());
}

fn ensure_status_job_cache_capacity(
    jobs: &mut HashMap<StatusJobKey, StatusJobCacheEntry>,
    key: &StatusJobKey,
    now: Instant,
) {
    if jobs.len() < STATUS_JOB_CACHE_LIMIT || jobs.contains_key(key) {
        return;
    }

    let Some(oldest_key) = jobs
        .iter()
        .filter(|(_, entry)| !entry.in_flight)
        .min_by_key(|(_, entry)| entry.updated_at.unwrap_or(now))
        .map(|(key, _)| key.clone())
    else {
        return;
    };
    jobs.remove(&oldest_key);
}

/// The stable job name of the `n`th cache slot this runtime has created.
fn status_job_shell_id(slot: u64) -> ShellId {
    ShellId::from(format!("rmux-status-{slot}"))
}

/// Runs one generation and returns the value it earned the right to publish.
///
/// `None` for every way a generation can fail to earn one: the specification could not be built,
/// the engine refused the job, the command outran its timeout, shutdown cancelled it, the
/// collection failed, or the gate did not approve the work. The caller keeps the previous value in
/// all six cases.
///
/// Cancellation goes through the job rather than the collection: dropping the collection would
/// stop *watching* a command that is still running, which is the difference between a timeout that
/// ends the work and one that merely stops reporting on it.
async fn produce_status_job_value(
    io: &ShellIo,
    inner: &Weak<StatusJobRuntimeInner>,
    job_id: u64,
    command: &str,
    profile: Option<&TerminalProfile>,
    shell_id: ShellId,
    mut cancel: watch::Receiver<bool>,
) -> Option<String> {
    let spec = match status_job_spec(io, command, profile, shell_id) {
        Ok(spec) => spec,
        Err(error) => {
            crate::diagnostic_log::record_workload_not_run(
                STATUS_JOB,
                command,
                &format!("could not be prepared: {error}"),
            );
            tracing::debug!("status job '{command}' could not be prepared: {error}");
            return None;
        }
    };
    let execution = match managed_workload::start(io, spec).await {
        Ok(execution) => execution,
        Err(error) => {
            crate::diagnostic_log::record_workload_not_run(
                STATUS_JOB,
                command,
                &format!("not admitted: {error}"),
            );
            tracing::debug!("status job '{command}' was not admitted: {error}");
            return None;
        }
    };

    let shell = execution.shell().clone();
    if let Some(runtime) = inner.upgrade() {
        runtime.record_job(job_id, io.unleased(), shell.clone());
    }

    // Truncating, because upstream already killed the child at the cap and returned the prefix;
    // erroring here would throw away output the status line used to show.
    let collect = execution.collect(managed_workload::truncating(STATUS_JOB_OUTPUT_LIMIT));
    tokio::pin!(collect);
    // Separated from the arms on purpose: abandoning this generation has to *await* the
    // collection, and an arm body cannot re-borrow the future the `select!` is polling.
    let finished = tokio::select! {
        result = &mut collect => Some(result),
        () = tokio::time::sleep(STATUS_JOB_TIMEOUT) => None,
        () = status_job_cancelled(&mut cancel) => None,
    };
    let Some(collected) = finished else {
        abandon(io, &shell, collect).await;
        return None;
    };

    let captured = match collected {
        Ok(captured) => captured,
        Err(error) => {
            crate::diagnostic_log::record_workload_not_run(
                STATUS_JOB,
                command,
                &format!("collection failed: {error}"),
            );
            tracing::debug!("status job '{command}' failed: {error}");
            return None;
        }
    };
    if let Err(error) = managed_workload::require_published(&captured) {
        // `#(command)` renders a string and has no error channel, so the user sees the slot's
        // previous value rather than a diagnostic. The recorded line is the only thing that makes
        // the decision explainable afterwards, which is why it cannot be a `tracing` call: this
        // daemon links the `tracing` facade with no subscriber, so those are compiled-in no-ops.
        crate::diagnostic_log::record_workload_not_run(
            STATUS_JOB,
            command,
            &format!("completed without publication: {error}"),
        );
        return None;
    }
    Some(status_job_stdout(captured.stdout))
}

/// Ends a generation whose value will not be used, and waits for it to actually be over.
///
/// The forced stop is what ends the producer. That is not obvious for a status job, because a
/// status producer is the embedded interpreter rather than a child process: it has no pid to
/// kill, and a builtin writing its output runs inline on the daemon's own runtime, so there is no
/// poll point at which a cooperative cancellation could reach it. What reaches it is the forced
/// stop closing the job's output read ends — see `ShellMux::stop` — after which its next write
/// fails and the line ends with that error.
///
/// Awaiting the collection afterwards is what makes this generation's slot free only once the job
/// is genuinely finished, rather than merely told to finish. It cannot outlast the stop: the
/// streams end, the verdict lands, and the drain returns.
async fn abandon(
    io: &ShellIo,
    shell: &ShellHandle,
    collect: Pin<&mut impl Future<Output = IoResult<CapturedOutput>>>,
) {
    // Forced, so the whole job dies and nothing it staged reaches the seed.
    let _ = io.stop(shell, true).await;
    let _ = collect.await;
}

/// What one generation runs, where, and as whom.
///
/// With a profile this reproduces upstream's `env_clear` plus the profile's own environment and
/// working directory. Without one it inherits the mux profile and starts in the daemon's default
/// directory, which is what upstream's bare `$SHELL -c` did by inheriting the daemon's
/// environment. Either way the text is interpreted by the embedded brush interpreter rather than
/// handed to `/bin/sh`, so marsh's builtins and instrumentation apply to it.
fn status_job_spec(
    io: &ShellIo,
    command: &str,
    profile: Option<&TerminalProfile>,
    shell_id: ShellId,
) -> Result<ExecutionSpec, RmuxError> {
    let process = ProcessCommand::Shell(command.to_owned());
    match profile {
        Some(profile) => managed_workload::spec(
            io,
            profile.cwd(),
            profile.raw_environment(),
            Some(shell_id),
            process,
        ),
        None => Ok(ExecutionSpec {
            directory: io.default_dir(),
            id: Some(shell_id),
            process,
            environment: None,
        }),
    }
}

/// Resolves when shutdown asks this generation to stop.
///
/// Never resolves spuriously, and resolves immediately when shutdown already began — a generation
/// that raced the signal must not wait out its whole timeout.
async fn status_job_cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
    // The sender lives in the active ledger, so this is only reached once the entry was removed —
    // which happens after this generation was already told to stop or reaped.
    std::future::pending::<()>().await;
}

fn status_job_stdout(stdout: Vec<u8>) -> String {
    let mut output = String::from_utf8_lossy(&stdout).into_owned();
    while output.ends_with(['\r', '\n']) {
        output.pop();
    }
    output
}

#[cfg(test)]
#[path = "status_jobs/tests.rs"]
mod tests;
