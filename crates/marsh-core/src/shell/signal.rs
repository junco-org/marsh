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

impl MarshExecutor {
    /// Sends `signal` to every process group this executor started since `start`.
    ///
    /// Each external command led its own session, so its pid is also its process-group id and a
    /// later pipeline stage joined the first's group. `start` is a
    /// [`spawn_record_count`](MarshExecutor::spawn_record_count) taken earlier, which is what
    /// makes "this evaluation's processes" expressible: a replay signals what it started and
    /// never what an earlier attempt already left behind.
    ///
    /// A record whose process is already gone — or which never led a group — answers `ESRCH`,
    /// which is not a failure: the point is that nothing of the line is left running. A
    /// builtin-only line has no process to signal at all and succeeds having sent nothing.
    ///
    /// # Errors
    ///
    /// Fails with the last errno that was not `ESRCH`; every record is attempted first, so one
    /// unkillable process does not skip the rest.
    pub(crate) fn signal_since(&self, start: usize, signal: Signal) -> std::io::Result<()> {
        let number = match signal {
            Signal::Interrupt => libc::SIGINT,
            Signal::Terminate => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Hangup => libc::SIGHUP,
            Signal::Continue => libc::SIGCONT,
        };
        // The recorder is released before the first signal: signalling under it would hold every
        // other observer of this shell's spawn log for as long as the kernel takes.
        let mut failure: Option<std::io::Error> = None;
        for pid in self.spawned_pids_since(start) {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                continue;
            };
            // SAFETY: `kill` signals a process group by the negation of its id and has no
            // memory-safety requirements.
            if unsafe { libc::kill(-pid, number) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    failure = Some(error);
                }
            }
        }
        match failure {
            None => Ok(()),
            Some(error) => Err(error),
        }
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
        // Collected out of the recorder first, then sorted and deduplicated here: the recorder's
        // log is append-order and may repeat a pid, and neither ordering nor uniqueness is
        // something to work out while holding it.
        let pids: BTreeSet<i32> = self
            .executor()
            .spawned_pids_since(0)
            .into_iter()
            .filter_map(|pid| i32::try_from(pid).ok())
            .collect();
        Ok(pids
            .into_iter()
            .filter(|pid| brush_core::sys::signal::kill_process(*pid, trap).is_ok())
            .count())
    }
}
