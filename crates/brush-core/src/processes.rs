//! Process management

use futures::FutureExt;

use crate::{error, sys};

/// A waitable future that will yield the results of a child process's execution.
pub(crate) type WaitableChildProcess = std::pin::Pin<
    Box<dyn futures::Future<Output = Result<std::process::Output, std::io::Error>> + Send + Sync>,
>;

/// How the shell learns about a child process's stops and exit.
enum ChildWaiter {
    /// A Tokio-managed child: the shell reaps it itself.
    Reaped(WaitableChildProcess),
    /// An embedder-managed child: stops and exit arrive as events; the shell never waits on
    /// its pid.
    Hosted(tokio::sync::mpsc::UnboundedReceiver<sys::process::HostedEvent>),
}

/// Tracks a child process being awaited.
pub struct ChildProcess {
    /// Source of the child's stops and exit.
    waiter: ChildWaiter,
    /// If available, the process ID of the child.
    pid: Option<sys::process::ProcessId>,
    /// If available, the process group ID of the child.
    pgid: Option<sys::process::ProcessId>,
}

impl ChildProcess {
    /// Wraps a child process and its future.
    pub fn new(
        child: sys::process::Child,
        pid: Option<sys::process::ProcessId>,
        pgid: Option<sys::process::ProcessId>,
    ) -> Self {
        let waiter = match child.into_inner() {
            sys::process::ChildInner::Tokio(child) => {
                ChildWaiter::Reaped(Box::pin(child.wait_with_output()))
            }
            sys::process::ChildInner::Hosted(child) => ChildWaiter::Hosted(child.events),
        };
        Self { waiter, pid, pgid }
    }

    /// Returns the process's ID.
    pub const fn pid(&self) -> Option<sys::process::ProcessId> {
        self.pid
    }

    /// Returns the process's group ID.
    pub const fn pgid(&self) -> Option<sys::process::ProcessId> {
        self.pgid
    }

    /// Waits for the process to exit or stop.
    ///
    /// A Tokio-managed child's stops are detected by a non-blocking, pid-specific `waitid` on
    /// `SIGCHLD`; a hosted child's stops and exit are taken from its event channel, and its pid
    /// is never waited on.
    pub async fn wait(&mut self) -> Result<ProcessWaitResult, error::Error> {
        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigtstp = sys::signal::tstp_signal_listener()?;
        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigchld = sys::signal::chld_signal_listener()?;

        #[allow(clippy::ignored_unit_patterns)]
        loop {
            match &mut self.waiter {
                ChildWaiter::Reaped(exec_future) => tokio::select! {
                    output = exec_future => {
                        break Ok(ProcessWaitResult::Completed(output?))
                    },
                    _ = sigtstp.recv() => {
                        break Ok(ProcessWaitResult::Stopped)
                    },
                    _ = sigchld.recv() => {
                        if let Some(pid) = self.pid {
                            if sys::signal::poll_for_stopped_child(pid)? {
                                break Ok(ProcessWaitResult::Stopped);
                            }
                        }
                    },
                    _ = sys::signal::await_ctrl_c() => {
                        // SIGINT got thrown. Handle it and continue looping. The child should
                        // have received it as well, and either handled it or ended up getting
                        // terminated (in which case we'll see the child exit).
                    },
                },
                ChildWaiter::Hosted(events) => tokio::select! {
                    event = events.recv() => {
                        break match event {
                            Some(event) => Ok(hosted_wait_result(event)),
                            None => Err(sys::process::hosted_events_closed().into()),
                        }
                    },
                    _ = sigtstp.recv() => {
                        break Ok(ProcessWaitResult::Stopped)
                    },
                    _ = sys::signal::await_ctrl_c() => {
                        // As above: the child's exit (if any) arrives as an event.
                    },
                },
            }
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<std::process::Output, error::Error>> {
        match &mut self.waiter {
            ChildWaiter::Reaped(exec_future) => exec_future
                .now_or_never()
                .map(|result| result.map_err(Into::into)),
            ChildWaiter::Hosted(events) => loop {
                // A stop is not completion; skip it like a reaped child's poll would.
                match events.try_recv() {
                    Ok(sys::process::HostedEvent::Stopped) => (),
                    Ok(sys::process::HostedEvent::Exited(status)) => {
                        break Some(Ok(hosted_output(status)));
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break None,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        break Some(Err(sys::process::hosted_events_closed().into()));
                    }
                }
            },
        }
    }
}

/// Maps a hosted child's event to a wait result.
const fn hosted_wait_result(event: sys::process::HostedEvent) -> ProcessWaitResult {
    match event {
        sys::process::HostedEvent::Stopped => ProcessWaitResult::Stopped,
        sys::process::HostedEvent::Exited(status) => {
            ProcessWaitResult::Completed(hosted_output(status))
        }
    }
}

/// A hosted child's output: its streams are never captured (external commands inherit the
/// shell's file descriptors), so only the status is meaningful.
const fn hosted_output(status: std::process::ExitStatus) -> std::process::Output {
    std::process::Output {
        status,
        stdout: vec![],
        stderr: vec![],
    }
}

/// Represents the result of waiting for an executing process.
pub enum ProcessWaitResult {
    /// The process completed.
    Completed(std::process::Output),
    /// The process stopped and has not yet completed.
    Stopped,
}
