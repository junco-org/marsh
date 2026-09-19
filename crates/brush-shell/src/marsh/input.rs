//! The gate between interactive lines, as an [`InputBackend`] wrapper: the previous line is
//! concluded — published or discarded — before the next one is read.

use brush_core::ShellExtensions;
use brush_interactive::{InputBackend, InteractivePrompt, ReadResult, ShellError, ShellRef};

use super::{MarshError, MarshExecutor, Outcome, Shell as MarshShell};

/// Renders a gate failure as the interactive layer's I/O error, the way `entry` renders a
/// config-loading failure.
pub(crate) fn shell_error(error: MarshError) -> ShellError {
    ShellError::IoError(std::io::Error::other(error))
}

/// Tells the user what the gate refused; grants and pass-throughs are silent.
pub(crate) fn report(outcome: &Outcome) {
    if let Outcome::Denied { requested, denials } = outcome {
        eprintln!(
            "marsh: {} of {} capabilities denied; the line's changes were discarded",
            denials.len(),
            requested.len()
        );
        for denial in denials {
            eprintln!("  - {denial}");
        }
    }
}

/// An input backend that concludes the previous line before reading the next one.
///
/// The interactive loop reads at the top of every iteration, once the previous line has been
/// fully awaited and with the shell lock released: a diff taken here never sees a half-written
/// pipeline stage. The line a session ends on (`exit`) is never followed by a read, so the loop's
/// caller concludes it through [`Self::conclude_pending`].
pub(crate) struct GatingBackend<
    's,
    IB,
    SE: ShellExtensions<ExternalCommandSpawner = MarshExecutor>,
> {
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
    pub(crate) const fn new(inner: IB, shell: &'s MarshShell<SE>) -> Self {
        Self {
            inner,
            shell,
            pending: None,
        }
    }

    /// Concludes the last line read — the empty command line when there is none — and forgets it.
    ///
    /// Synchronous by contract of [`InputBackend::read_line`], so the shell is taken with
    /// `try_lock`: the interactive loop released its guard before reading and nothing else holds
    /// it at this boundary, so a held lock here is a bug, reported rather than waited on.
    pub(crate) fn conclude_pending(&mut self) -> Result<(), ShellError> {
        let line = self.pending.take().unwrap_or_default();
        let mut guard = self.shell.shell_ref().try_lock().map_err(|_| {
            ShellError::IoError(std::io::Error::other(
                "the shell is locked at a publication boundary",
            ))
        })?;
        let outcome = self
            .shell
            .conclude(&mut guard, &line)
            .map_err(shell_error)?;
        drop(guard);
        report(&outcome);
        Ok(())
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
        self.conclude_pending()?;
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
