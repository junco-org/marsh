//! Stable command receipts retaining the Shell result once, without duplicated status/verdicts.

use super::error::MuxError;
use super::mux::Sandbox;
use crate::shell::ExecutionProgress;
use crate::{ExecutionResult, ShellError};
use marsh_lib::{WaitState, wait_for_completion};
use std::sync::Arc;
use std::time::Duration;

/// Monotonic command identity within one mux lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(pub(crate) u64);
impl CommandId {
    /// Numeric representation for indexing observations.
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl std::fmt::Display for CommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A wait that ended without a command verdict; never a retry permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// Explicit mux shutdown ended the wait.
    #[error("the shell multiplexer was shut down before this completed")]
    Shutdown,
    /// The producer was lost unexpectedly.
    #[error("the task that would have completed this was lost")]
    Aborted,
}

/// One completed command and the ordinary Shell result it produced.
pub struct CommandCompletion {
    /// Identity of this command, not a later command on the same shell.
    pub id: CommandId,
    /// Stable shell identity and logical source location.
    pub shell: Sandbox,
    /// Exact submitted command text.
    pub command: Arc<str>,
    /// Sole execution answer; a nonzero native exit may still be successful publication.
    pub result: Arc<Result<ExecutionResult, ShellError>>,
}
impl CommandCompletion {
    /// Native process status, including a status preserved inside a later Shell error.
    pub fn exit_code(&self) -> Option<i32> {
        let result = self
            .result
            .as_ref()
            .as_ref()
            .map_or_else(ShellError::execution_result, Some);
        result.map(|result| i32::from(u8::from(result.exit_code)))
    }
    /// Whether the Shell accepted this command's supported effects.
    pub fn is_published(&self) -> bool {
        self.result.is_ok()
    }
}
impl std::fmt::Debug for CommandCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCompletion")
            .field("id", &self.id)
            .field("shell", &self.shell)
            .field("command", &self.command)
            .field("exit_code", &self.exit_code())
            .field("error", &self.result.as_ref().as_ref().err())
            .finish()
    }
}

/// Cloneable receipt for one admitted command; dropping it never cancels the command.
#[derive(Clone, Debug)]
pub struct CommandHandle {
    pub(crate) id: CommandId,
    pub(crate) shell: Sandbox,
    pub(crate) text: Arc<str>,
    pub(crate) state: tokio::sync::watch::Receiver<WaitState<CommandCompletion, WaitError>>,
    /// Where this command's execution, once it begins, can be bounded.
    pub(crate) execution: ExecutionProgress,
}
impl CommandHandle {
    /// Identity of the admitted command.
    pub const fn id(&self) -> CommandId {
        self.id
    }
    /// Shell generation in which it was admitted.
    pub const fn shell(&self) -> &Sandbox {
        &self.shell
    }
    /// Original submitted command text.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Whether a verdict or terminal wait failure is already available.
    pub fn is_finished(&self) -> bool {
        !matches!(*self.state.borrow(), WaitState::Pending)
    }
    /// Waits for the same retained answer for every observer.
    pub async fn wait(&self) -> Result<Arc<CommandCompletion>, WaitError> {
        let mut state = self.state.clone();
        wait_for_completion(&mut state, || WaitError::Aborted).await
    }
    /// Ends a command whose input has already been ended, and waits for its actual verdict.
    ///
    /// This is an active termination operation, and only execution is timed. Admission and view
    /// preparation run to completion first. Once the command's producers are running they get
    /// `stdin_grace` to finish on their own, then `SIGTERM` and `terminate_grace`, and are then
    /// cancelled — which discards the command — provided one of them is still alive to end.
    /// Producers that already ended are never discarded because the native service is late to
    /// confirm it. Once they have finished, authorization and publication are awaited whatever
    /// they take, so the answer is the command's own verdict, never a timeout.
    ///
    /// Dropping the returned future stops its timers and cancels nothing. The timers need an
    /// enabled Tokio time driver on the polling runtime.
    ///
    /// # Errors
    ///
    /// As [`Self::wait`].
    pub async fn finish_with_grace(
        &self,
        stdin_grace: Duration,
        terminate_grace: Duration,
    ) -> Result<Arc<CommandCompletion>, WaitError> {
        tokio::select! {
            biased;
            verdict = self.wait() => return verdict,
            () = self.execution.finish_with_grace(stdin_grace, terminate_grace) => {}
        }
        self.wait().await
    }
}

/// Admission failure, an unsuccessful Shell result, or loss of the answer.
#[derive(Debug)]
pub enum RunError {
    /// No command was admitted.
    Admission(MuxError),
    /// An admitted command returned a typed Shell error.
    Execution {
        /// Complete identity, command text, status and Shell verdict.
        completion: Arc<CommandCompletion>,
    },
    /// This observer lost the answer.
    Wait(WaitError),
}
impl RunError {
    /// Completed execution behind the error, if execution reached a verdict.
    pub const fn completion(&self) -> Option<&Arc<CommandCompletion>> {
        match self {
            Self::Execution { completion } => Some(completion),
            Self::Admission(_) | Self::Wait(_) => None,
        }
    }
}
impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => error.fmt(f),
            Self::Wait(error) => error.fmt(f),
            Self::Execution { completion } => match completion.result.as_ref() {
                Err(error) => error.fmt(f),
                Ok(_) => write!(f, "command {} completed", completion.id),
            },
        }
    }
}
impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(error) => Some(error),
            Self::Wait(error) => Some(error),
            Self::Execution { completion } => completion
                .result
                .as_ref()
                .as_ref()
                .err()
                .map(|error| error as &dyn std::error::Error),
        }
    }
}
