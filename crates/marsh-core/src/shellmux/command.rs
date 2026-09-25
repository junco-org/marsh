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

use marsh_lib::{WaitState, wait_for_completion};

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
///   [`Outcome::Discarded`], [`Outcome::Detached`], or an infrastructure [`MuxError`]. A command
///   can exit zero and be denied.
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
    pub(crate) state: tokio::sync::watch::Receiver<WaitState<CommandCompletion, WaitError>>,
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
        !matches!(*self.state.borrow(), WaitState::Pending)
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
        // The sender going away with the value still pending is the producer being lost: nothing
        // will ever resolve this, and a waiter must not hang on it.
        wait_for_completion(&mut state, || WaitError::Aborted).await
    }
}

/// A command the publication gate refused.
///
/// Carries the completion whole rather than a message: the command's identity, its principal and
/// generation, its *actual* process exit code, the capabilities it requested and the
/// [`Denial`](crate::Denial)s it collected are all still there, because "the program exited zero
/// and the policy refused it" is a different fact from "the program failed" and a caller
/// frequently has to report both.
pub struct PolicyError {
    /// The refused command's verdict, shared with every receipt on it.
    completion: Arc<CommandCompletion>,
}

impl PolicyError {
    /// Wraps one denied completion.
    ///
    /// Private: the only thing that may construct this is the boundary that observed
    /// [`Outcome::Denied`], so a caller can never manufacture a denial that never happened.
    pub(crate) const fn new(completion: Arc<CommandCompletion>) -> Self {
        Self { completion }
    }

    /// The refused command's completion.
    #[must_use]
    pub const fn completion(&self) -> &Arc<CommandCompletion> {
        &self.completion
    }
}

impl std::fmt::Debug for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PolicyError")
            .field("completion", &self.completion)
            .finish()
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "command {} was denied by policy",
            self.completion.id
        )
    }
}

impl std::error::Error for PolicyError {}

/// Why running one line did not end in an approved publication.
///
/// The four reasons are kept apart because a caller acts differently on each, and collapsing any
/// two of them loses a real difference:
///
/// * [`Admission`](Self::Admission) — the line was never accepted, so nothing ran at all.
/// * [`Policy`](Self::Policy) — it ran, and the gate refused it. The exit code it carries is the
///   program's own and says nothing about the refusal.
/// * [`Unpublished`](Self::Unpublished) — it ran and reached a boundary that published nothing:
///   [`Outcome::Discarded`], [`Outcome::Detached`], or an infrastructure failure. The discriminant
///   stays inspectable in [`CommandCompletion::outcome`]; a discard is not a retry permit.
/// * [`Wait`](Self::Wait) — *this caller* lost the answer. It is not evidence that nothing
///   happened, so it is never permission to rerun an effectful command.
pub enum RunError {
    /// The line was refused before anything could run.
    Admission(MuxError),
    /// The line ran and the publication gate refused it.
    Policy(PolicyError),
    /// The line concluded without an approved publication, for a reason that is not a denial.
    Unpublished {
        /// The completion, whose `outcome` carries the original discriminant or error.
        completion: Arc<CommandCompletion>,
    },
    /// The answer was lost: teardown, or a producer that went away.
    Wait(WaitError),
}

impl RunError {
    /// The completion behind this failure, when the line got far enough to have one.
    ///
    /// `Some` for a denial and for an unpublished conclusion; `None` for a refused admission and
    /// for a lost answer, neither of which produced a verdict at all.
    #[must_use]
    pub const fn completion(&self) -> Option<&Arc<CommandCompletion>> {
        match self {
            Self::Policy(denied) => Some(denied.completion()),
            Self::Unpublished { completion } => Some(completion),
            Self::Admission(_) | Self::Wait(_) => None,
        }
    }

    /// The infrastructure failure an unpublished conclusion carries, when it carries one.
    fn infrastructure(&self) -> Option<&MuxError> {
        match self {
            Self::Unpublished { completion } => completion.outcome.as_ref().as_ref().err(),
            Self::Admission(_) | Self::Policy(_) | Self::Wait(_) => None,
        }
    }
}

impl std::fmt::Debug for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => formatter.debug_tuple("Admission").field(error).finish(),
            Self::Policy(denied) => formatter.debug_tuple("Policy").field(denied).finish(),
            Self::Unpublished { completion } => formatter
                .debug_struct("Unpublished")
                .field("completion", completion)
                .finish(),
            Self::Wait(error) => formatter.debug_tuple("Wait").field(error).finish(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => write!(formatter, "{error}"),
            Self::Policy(denied) => write!(formatter, "{denied}"),
            Self::Unpublished { completion } => match self.infrastructure() {
                Some(error) => write!(formatter, "{error}"),
                None => write!(
                    formatter,
                    "command {} completed without an approved publication",
                    completion.id
                ),
            },
            Self::Wait(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RunError {
    /// The failure underneath, never flattened into this one's message.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(error) => Some(error),
            Self::Policy(denied) => Some(denied),
            Self::Unpublished { .. } => self
                .infrastructure()
                .map(|error| error as &dyn std::error::Error),
            Self::Wait(error) => Some(error),
        }
    }
}

impl From<MuxError> for RunError {
    fn from(error: MuxError) -> Self {
        Self::Admission(error)
    }
}

impl From<WaitError> for RunError {
    fn from(error: WaitError) -> Self {
        Self::Wait(error)
    }
}
