//! The gate between interactive lines, as an [`InputBackend`] wrapper: the previous line is
//! concluded — published or discarded — before the next one is read.

use brush_core::ShellExtensions;
use brush_interactive::{InputBackend, InteractivePrompt, ReadResult, ShellError, ShellRef};

use super::{MarshError, MarshExecutor, Outcome, Shell as MarshShell};

/// The refusal a boundary that could not take the interpreter reports.
///
/// A held lock at a boundary is a bug: the interactive loop released its guard before reading and
/// nothing else holds it there. A blocking wait would turn that bug into a hung prompt.
fn locked() -> ShellError {
    ShellError::IoError(std::io::Error::other(
        "the shell is locked at a publication boundary",
    ))
}

/// Renders a gate failure as the interactive layer's I/O error, the way `entry` renders a
/// config-loading failure.
pub fn shell_error(error: MarshError) -> ShellError {
    ShellError::IoError(std::io::Error::other(error))
}

/// Tells the user what the gate refused or discarded; grants and pass-throughs are silent.
pub fn report(outcome: &Outcome) {
    match outcome {
        Outcome::Denied { requested, denials } => {
            eprintln!(
                "marsh: {} of {} capabilities denied; the line's changes were discarded",
                denials.len(),
                requested.len()
            );
            for denial in denials {
                eprintln!("  - {denial}");
            }
        }
        Outcome::Discarded => {
            eprintln!("marsh: the line was discarded; nothing was published");
        }
        Outcome::Published { .. } | Outcome::Detached => {}
    }
}

/// An input backend that concludes the previous line before reading the next one.
///
/// The interactive loop reads at the top of every iteration, once the previous line has been
/// fully awaited and with the shell lock released: a diff taken here never sees a half-written
/// pipeline stage. The line a session ends on (`exit`) is never followed by a read, so the loop's
/// caller concludes it through [`Self::conclude_pending`]. The snapshot is refreshed after that
/// boundary and before the next read, so an interactive line always starts from the seed as other
/// principals left it.
pub struct GatingBackend<'s, IB, SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> {
    /// The backend that actually reads.
    inner: IB,
    /// The gate; a detached shell makes this wrapper a plain pass-through.
    shell: &'s MarshShell<SE>,
    /// The line read last and not yet concluded; `None` before the first read and after each
    /// boundary.
    pending: Option<String>,
}

impl<'s, IB, SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>>
    GatingBackend<'s, IB, SE>
{
    /// Wraps `inner`, gating through `shell`.
    pub const fn new(inner: IB, shell: &'s MarshShell<SE>) -> Self {
        Self {
            inner,
            shell,
            pending: None,
        }
    }

    /// Runs `body` on the interpreter, at a publication boundary.
    ///
    /// Synchronous by contract of [`InputBackend::read_line`], so the shell is taken with
    /// `try_lock`: the interactive loop released its guard before reading and nothing else holds
    /// it at this boundary, so a held lock here is a bug, reported rather than waited on. A
    /// blocking lock would turn that bug into a hung prompt.
    fn with_shell<T>(
        &self,
        body: impl FnOnce(&mut brush_core::Shell<SE>) -> Result<T, MarshError>,
    ) -> Result<T, ShellError> {
        let mut guard = self.shell.shell_ref().try_lock().map_err(|_| locked())?;
        let outcome = body(&mut guard).map_err(shell_error);
        drop(guard);
        outcome
    }

    /// Concludes the last line read — the empty command line when there is none — and forgets it.
    ///
    /// The line is taken *before* the lock is asked for, so a boundary that cannot be reached
    /// does not leave the same line pending for the next read to conclude a second time.
    ///
    /// This is the *final* boundary, the one a loop's caller reaches after the session's last
    /// line. It concludes rather than gates, so a line whose reads were invalidated is evaluated
    /// again here: there is no next read to re-offer it to.
    pub async fn conclude_pending(&mut self) -> Result<(), ShellError> {
        let line = self.pending.take().unwrap_or_default();
        let mut guard = self.shell.shell_ref().try_lock().map_err(|_| locked())?;
        let concluded = self.shell.conclude(&mut guard, &line).await;
        drop(guard);
        // Outside the guard: the report is the user's, not the interpreter's.
        report(&concluded.map_err(shell_error)?);
        Ok(())
    }

    /// The boundary between two interactive lines: the gate, and nothing else.
    ///
    /// `Some(line)` is the line having to be evaluated again — it read something another
    /// principal published while it ran. Nothing is reported and no input is read: the loop is
    /// handed the same line back and runs it against the resynchronized tree, which is what makes
    /// a replay indistinguishable from the user having typed it a moment later.
    fn gate_pending(&mut self) -> Result<Option<String>, ShellError> {
        let line = self.pending.take().unwrap_or_default();
        let settled = self.with_shell(|shell| {
            let outcome = self.shell.gate(&line)?;
            // The gate discarded and retook the tree for every ending but a publication, so the
            // shell may be standing in a directory that no longer exists.
            self.shell.recover_directory(shell)?;
            Ok(outcome)
        })?;
        match settled {
            Some(outcome) => {
                report(&outcome);
                Ok(None)
            }
            None => Ok(Some(line)),
        }
    }

    /// Retakes the snapshot from the seed, at the same boundary and under the same lock policy.
    fn refresh(&self) -> Result<(), ShellError> {
        self.with_shell(|shell| self.shell.refresh(shell))
    }
}

impl<IB: InputBackend, SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>> InputBackend
    for GatingBackend<'_, IB, SE>
{
    fn read_line(
        &mut self,
        shell: &ShellRef<impl ShellExtensions>,
        prompt: InteractivePrompt,
    ) -> Result<ReadResult, ShellError> {
        if let Some(line) = self.gate_pending()? {
            // Re-offered, not re-read: the user typed this once and the replay is this layer's
            // business, not theirs.
            self.pending = Some(line.clone());
            return Ok(ReadResult::Input(line));
        }
        self.refresh()?;
        let result = self.inner.read_line(shell, prompt)?;
        if let ReadResult::Input(line) | ReadResult::BoundCommand(line) = &result {
            self.pending = Some(line.clone());
        }
        Ok(result)
    }

    fn get_read_buffer(&self) -> Option<(String, usize)> {
        self.inner.get_read_buffer()
    }

    fn set_read_buffer(&mut self, buffer: String, cursor: usize) {
        self.inner.set_read_buffer(buffer, cursor);
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::PolicyValidator;

    /// A shell with no seed: the gate is a pass-through, so what these assert is the lock policy
    /// rather than a publication verdict.
    async fn detached() -> MarshShell {
        MarshShell::build(
            MarshExecutor::default(),
            Arc::new(Mutex::new(PolicyValidator::new())),
        )
        .await
        .expect("a detached shell")
    }

    /// Both boundary operations are synchronous, so the interpreter is taken with `try_lock`: a
    /// shell somebody else is holding at a boundary is a bug to report, and a blocking lock would
    /// turn it into a prompt that never returns. Once the holder lets go, the same two calls
    /// succeed.
    #[tokio::test]
    async fn a_locked_interpreter_is_reported_rather_than_waited_on() {
        let shell = detached().await;
        let mut gate = GatingBackend::new((), &shell);

        let held = shell
            .shell_ref()
            .try_lock()
            .expect("nothing holds a fresh interpreter");
        for refused in [
            gate.conclude_pending()
                .await
                .expect_err("a held interpreter concludes nothing"),
            gate.refresh()
                .expect_err("a held interpreter refreshes nothing"),
        ] {
            let ShellError::IoError(error) = refused else {
                panic!("the boundary reports the interactive layer's I/O error, got {refused:?}");
            };
            assert!(
                error.to_string().contains("publication boundary"),
                "the refusal names the boundary it could not reach, got {error}"
            );
        }
        drop(held);

        gate.conclude_pending()
            .await
            .expect("a free interpreter concludes the pending line");
        gate.refresh().expect("a free interpreter retakes its seed");
    }
}
