//! Sending signals to what a [`Shell`] started.
//!
//! There are two populations of processes a driver can reach. Managed jobs — background and
//! stopped ones — are signalled exactly, by [`Shell::signal_job`] and [`Shell::signal_jobs`]:
//! both wait for the shell lock, so they act between lines. The external processes of the line
//! currently executing are never in the job manager (a foreground pipeline is only promoted to a
//! job when it stops), and the shell lock is held for the whole of [`Shell::run`] — so reaching
//! them needs [`Shell::signal_running`], which never waits for that lock and is best-effort: it
//! signals recorded pids, not confirmed-live ones.

use std::collections::BTreeSet;

use brush_core::extensions::ShellExtensions;
use brush_core::traps::TrapSignal;
use marsh_instrument::SpawnRecord;

use super::{MarshError, MarshExecutor, Shell};

/// A signal [`Shell`] can deliver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// `SIGINT`: what Ctrl-C raises. Catchable.
    Interrupt,
    /// `SIGTERM`: the polite request to exit. Catchable.
    Terminate,
    /// `SIGKILL`: uncatchable; the kernel tears the process down.
    Kill,
    /// `SIGHUP`: the controlling terminal went away. Catchable.
    Hangup,
    /// `SIGCONT`: resumes a stopped process.
    Continue,
}

impl Signal {
    /// The name brush-core parses. It accepts the `SIG`-less spelling and prefixes it itself.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "INT",
            Self::Terminate => "TERM",
            Self::Kill => "KILL",
            Self::Hangup => "HUP",
            Self::Continue => "CONT",
        }
    }

    /// The brush-core signal this delivers as.
    fn trap(self) -> Result<TrapSignal, MarshError> {
        Ok(TrapSignal::try_from(self.as_str())?)
    }
}

impl<SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> Shell<SE> {
    /// Sends `signal` to the job `job` names — a bash job spec: `%%`, `%+`, `%-`, or `%N`.
    ///
    /// Waits for the shell lock, so it acts between lines, on background and stopped jobs. The
    /// line currently executing is not a job; [`Self::signal_running`] reaches that.
    ///
    /// # Errors
    ///
    /// Fails when no job matches `job`, and when the job has no process to signal.
    pub async fn signal_job(&self, job: &str, signal: Signal) -> Result<(), MarshError> {
        let trap = signal.trap()?;
        let mut guard = self.shell_ref().lock().await;
        let killed = guard
            .jobs_mut()
            .resolve_job_spec(job)
            .map(|target| target.kill(trap));
        drop(guard);
        match killed {
            Some(result) => Ok(result?),
            None => Err(MarshError::NoSuchJob(job.to_owned())),
        }
    }

    /// Sends `signal` to every managed job, and returns how many accepted it.
    ///
    /// A job whose processes have already exited is skipped, not reported: a broadcast is not the
    /// place to learn that one job ended first.
    ///
    /// # Errors
    ///
    /// Fails when this platform does not know `signal`.
    pub async fn signal_jobs(&self, signal: Signal) -> Result<usize, MarshError> {
        let trap = signal.trap()?;
        let guard = self.shell_ref().lock().await;
        let signalled = guard
            .jobs()
            .jobs
            .iter()
            .filter(|job| job.kill(trap).is_ok())
            .count();
        drop(guard);
        Ok(signalled)
    }

    /// Sends `signal` to every external process this shell started that still accepts one,
    /// without waiting for the shell lock — so it reaches the line currently executing.
    ///
    /// Best-effort, and deliberately so. The executor's spawn log is append-only: brush owns the
    /// child handles and is the only party that learns of an exit, so this cannot tell a live pid
    /// from one the OS has since recycled. It signals the recorded pid, not its process group, so
    /// a signalled process's own children are untouched. A detached executor records nothing and
    /// this returns 0.
    ///
    /// # Errors
    ///
    /// Fails when this platform does not know `signal`.
    pub fn signal_running(&self, signal: Signal) -> Result<usize, MarshError> {
        let trap = signal.trap()?;
        let pids: BTreeSet<i32> = self
            .executor()
            .spawn_records()
            .iter()
            .filter_map(|record| match record {
                SpawnRecord::Spawned { pid, .. } => pid.and_then(|pid| i32::try_from(pid).ok()),
                SpawnRecord::Failed { .. } => None,
            })
            .collect();
        Ok(pids
            .into_iter()
            .filter(|pid| brush_core::sys::signal::kill_process(*pid, trap).is_ok())
            .count())
    }
}
