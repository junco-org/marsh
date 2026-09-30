//! Process management utilities

pub(crate) type ProcessId = i32;

/// A spawned external command, as returned by an
/// [`ExternalCommandSpawner`](crate::extensions::ExternalCommandSpawner).
///
/// Either a Tokio-managed child, which the shell reaps itself, or a [`HostedChild`], whose
/// reaping and stop reporting belong to the embedder.
pub struct Child {
    inner: ChildInner,
}

/// The backing of a [`Child`].
pub(crate) enum ChildInner {
    /// A Tokio-managed child; the shell waits on its pid.
    Tokio(tokio::process::Child),
    /// An embedder-managed child; the shell never waits on its pid.
    Hosted(HostedChild),
}

/// A child whose reaping and stop reporting belong to the embedder; the shell never waits on
/// its pid.
///
/// The shell learns about the child's stops and its exit solely through `events`, and never
/// calls `waitpid`/`waitid` for `pid` (doing so from a thread of a process that also ptraces
/// the child would consume the tracer's stops or reap the tracee). Signals the shell sends for
/// job control still target `pid` / its process group directly.
pub struct HostedChild {
    /// Process ID of the child.
    pub pid: u32,
    /// Stop and exit events for the child, in the order they happened. The embedder sends at
    /// most one [`HostedEvent::Exited`], as the last event; closing the channel without sending
    /// one is reported to the shell as an error.
    pub events: tokio::sync::mpsc::UnboundedReceiver<HostedEvent>,
    /// Forcibly terminates the child; used by [`Child::start_kill`].
    pub kill: Box<dyn FnMut() -> std::io::Result<()> + Send + Sync>,
}

/// A state change of a [`HostedChild`], reported by the embedder.
#[derive(Clone, Copy, Debug)]
pub enum HostedEvent {
    /// The child stopped (e.g. on `SIGTSTP`) and has not exited.
    Stopped,
    /// The child exited or was terminated by a signal, with the given status.
    Exited(std::process::ExitStatus),
}

impl Child {
    /// Wraps an embedder-managed child.
    pub const fn hosted(child: HostedChild) -> Self {
        Self {
            inner: ChildInner::Hosted(child),
        }
    }

    /// Returns the process ID of the child. For a Tokio-managed child this is `None` once it
    /// has been reaped.
    pub fn id(&self) -> Option<u32> {
        match &self.inner {
            ChildInner::Tokio(child) => child.id(),
            ChildInner::Hosted(child) => Some(child.pid),
        }
    }

    /// Starts killing the child without waiting for it to exit.
    pub fn start_kill(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            ChildInner::Tokio(child) => child.start_kill(),
            ChildInner::Hosted(child) => (child.kill)(),
        }
    }

    /// Waits for the child to exit. For a hosted child, this consumes events until the next
    /// [`HostedEvent::Exited`], skipping [`HostedEvent::Stopped`].
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.inner {
            ChildInner::Tokio(child) => child.wait().await,
            ChildInner::Hosted(child) => loop {
                match child.events.recv().await {
                    Some(HostedEvent::Exited(status)) => break Ok(status),
                    Some(HostedEvent::Stopped) => (),
                    None => break Err(hosted_events_closed()),
                }
            },
        }
    }

    pub(crate) fn into_inner(self) -> ChildInner {
        self.inner
    }
}

impl From<tokio::process::Child> for Child {
    fn from(child: tokio::process::Child) -> Self {
        Self {
            inner: ChildInner::Tokio(child),
        }
    }
}

/// The error reported when a hosted child's event channel closes before its exit was sent.
pub(crate) fn hosted_events_closed() -> std::io::Error {
    std::io::Error::other("hosted child's event channel closed before it reported an exit")
}

// `kill_on_drop`: the shell always passes false here; brush-core 0.5.0 has no
// `CreateOptions::kill_external_commands_on_drop`.
pub(crate) fn spawn(command: std::process::Command, kill_on_drop: bool) -> std::io::Result<Child> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(kill_on_drop);
    command.spawn().map(Child::from)
}
