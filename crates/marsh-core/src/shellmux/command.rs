//! One submitted command line: the receipt reserved when it is admitted, and the verdict it
//! eventually resolves to.
//!
//! A command is not a job. A job is a named sandbox that outlives the commands run in it; a
//! command is one line submitted into one job, and the two have separate identities, separate
//! lifetimes and separate completion boundaries. Confusing them is how a fast second line steals
//! the first line's receipt, which is the exact failure this module exists to make impossible.
//!
//! Every admitted command gets a [`CommandId`], a text, and a completion watch, all allocated
//! under the same job-table lock that admits it and before anything can run. A
//! [`CommandHandle`] is a cheap clone of that reservation: any number of them may exist, they may
//! be created before or after the command ends, and each resolves to the same
//! [`CommandCompletion`]. Nothing here occupies the single
//! [`OnFinish`](crate::shellmux::OnFinish) slot, which stays available for the one legacy caller
//! that only needs a status.

use std::sync::Arc;

use crate::Outcome;
use crate::shellmux::error::MuxError;
use crate::shellmux::mux::Sandbox;

/// One command's identity within one mux's lifetime.
///
/// Unique while the mux lives and never reused, so a late observer holding one can tell whether
/// the verdict it is looking at is the one it asked about. Not persistent and not a wire
/// identifier: a new mux starts the series again, and nothing outside this process should store
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(pub(crate) u64);

impl CommandId {
    /// The raw counter value, for a caller that has to key a map by it.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "#{}", self.0)
    }
}

/// Why a wait ended without a verdict.
///
/// Neither variant says anything about what the command did to the seed. A command that was
/// admitted may have run, may have spawned processes and may have staged changes before its waiter
/// lost the answer; the publication gate is what decides whether any of it became visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// The whole mux was shut down before an ordinary result could be delivered.
    ///
    /// Explicit teardown, not a failure: the host asked for it. Work still in flight at that point
    /// is discarded rather than gated.
    #[error("the shell multiplexer was shut down before this completed")]
    Shutdown,
    /// The producer of this result was lost: a panicked task, a cancelled runtime, a dropped
    /// reservation.
    ///
    /// Unexpected, and deliberately distinct from [`Self::Shutdown`]. It is **not** evidence that
    /// nothing happened, so a caller must not treat it as permission to retry an effectful
    /// command.
    #[error("the task that would have completed this was lost")]
    Aborted,
}

/// What one command line became.
///
/// The five things a caller can distinguish are deliberately kept apart here, because collapsing
/// any two of them loses a real difference:
///
/// * `exit_code` is the *process* status, in the shell's convention. `None` means no execution
///   result was obtained at all — the line never ran, or the interpreter itself failed. A gate or
///   storage failure that happens *after* a real exit does not erase the exit code it already has.
/// * `outcome` is the *publication* verdict: [`Outcome::Published`], [`Outcome::Denied`],
///   [`Outcome::Stale`], [`Outcome::Discarded`], [`Outcome::Detached`], or an infrastructure
///   [`MuxError`]. A command can exit zero and be denied.
#[derive(Debug)]
pub struct CommandCompletion {
    /// Which command this is the verdict for.
    pub id: CommandId,
    /// The job it ran in, identified by [`Sandbox::uid`] as well as by name.
    pub shell: Sandbox,
    /// The line as submitted, retained independently of the job and of any later command.
    pub command: Arc<str>,
    /// The process status, or `None` when no execution result was obtained.
    pub exit_code: Option<i32>,
    /// What the gate made of the line, or the infrastructure failure that prevented a verdict.
    pub outcome: Arc<Result<Outcome, MuxError>>,
}

impl CommandCompletion {
    /// The status in the legacy `OnFinish` convention: the real code, or `-1` when there is none.
    ///
    /// Only the callback boundary uses this. Every native reader should match on
    /// [`exit_code`](Self::exit_code) instead, because `-1` is also a perfectly ordinary status.
    #[must_use]
    pub const fn legacy_status(&self) -> i32 {
        match self.exit_code {
            Some(code) => code,
            None => -1,
        }
    }

    /// Whether the gate published this line's effects.
    ///
    /// The only condition under which the line's staged changes are in the seed. A zero exit code
    /// is not it, and neither is a successful read of its output.
    #[must_use]
    pub fn is_published(&self) -> bool {
        matches!(self.outcome.as_ref(), Ok(Outcome::Published { .. }))
    }
}

/// The state one command's watch carries.
#[derive(Clone, Debug)]
pub(crate) enum CommandState {
    /// Admitted; no verdict yet.
    Pending,
    /// Concluded, with the verdict every waiter shares.
    Done(Arc<CommandCompletion>),
    /// The mux was torn down before a verdict could be produced.
    Shutdown,
}

/// A receipt for one admitted command.
///
/// Cloneable and cheap. Any number of holders may wait on the same command, before or after it
/// ends, and all of them observe the identical [`CommandCompletion`]. The receipt keeps the
/// command's text and verdict alive independently of the job: it stays readable after the job
/// closed, after its name was reused, and after later commands ran.
#[derive(Clone, Debug)]
pub struct CommandHandle {
    /// This command's identity.
    pub(crate) id: CommandId,
    /// The job it was admitted into.
    pub(crate) shell: Sandbox,
    /// The line as submitted.
    pub(crate) text: Arc<str>,
    /// The shared verdict.
    pub(crate) state: tokio::sync::watch::Receiver<CommandState>,
}

impl CommandHandle {
    /// This command's identity.
    #[must_use]
    pub const fn id(&self) -> CommandId {
        self.id
    }

    /// The job this command was admitted into.
    #[must_use]
    pub const fn shell(&self) -> &Sandbox {
        &self.shell
    }

    /// The line as submitted, byte for byte.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this command has already concluded.
    ///
    /// A cheap, non-blocking look. `false` is only a statement about this instant.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !matches!(*self.state.borrow(), CommandState::Pending)
    }

    /// Waits for this command's verdict.
    ///
    /// Resolves once for every holder, whenever they ask: a waiter attached after the command
    /// already ended gets the stored answer without blocking. A forced stop that discarded the
    /// line resolves `Ok` with [`Outcome::Discarded`] and no exit code — that *is* the verdict,
    /// not a lost one. A known launch failure resolves `Ok` with the infrastructure error inside
    /// the completion.
    ///
    /// # Errors
    ///
    /// Fails with [`WaitError::Shutdown`] when the whole mux was torn down before a verdict could
    /// be produced, and with [`WaitError::Aborted`] when the producer was lost unexpectedly.
    pub async fn wait(&self) -> Result<Arc<CommandCompletion>, WaitError> {
        let mut state = self.state.clone();
        loop {
            {
                let current = state.borrow_and_update();
                match &*current {
                    CommandState::Done(completion) => {
                        let completion = Arc::clone(completion);
                        drop(current);
                        return Ok(completion);
                    }
                    CommandState::Shutdown => return Err(WaitError::Shutdown),
                    CommandState::Pending => {}
                }
            }
            // The sender going away with the value still `Pending` is the producer being lost:
            // nothing will ever resolve this, and a waiter must not hang on it.
            state.changed().await.map_err(|_| WaitError::Aborted)?;
        }
    }
}
