//! One pane's terminal, which is one generation of one ShellMux job.
//!
//! A pane used to own a pseudoterminal and a child process outright: it held the master, reaped
//! the child, signalled its process group and read its bytes. None of that is here any more. The
//! pane now holds a [`ShellHandle`], and every operation on it — input, resize, termination — goes
//! back through [`ShellIo`], which is the only thing that knows whether the job is still the one
//! this handle names.
//!
//! What remains local is metadata a *server* legitimately probes and the engine does not model:
//! the terminal's name, the foreground process group of whatever the shell is currently running,
//! the geometry rmux last asked for, and the profile the pane was created with.
//!
//! Pane creation itself is serialized, one transaction against the next.
//!
//! A pane is created in three steps — plan it with the handler's state locked, open its job with
//! that lock *released*, commit it with the lock taken again — because opening a job awaits a
//! shell build, a snapshot creation and the facade's admission lock, and awaiting any of those
//! under the daemon's request mutex stalls every other session and inverts against the adoption
//! path, which takes admission first and handler state second.
//!
//! Releasing the state lock mid-transaction is what makes this necessary. Two creations that
//! interleave would each have taken a rollback snapshot of a session the other has since mutated,
//! so the first failure to roll back would discard the second's window. Serializing the whole
//! transaction removes that case outright, and costs nothing that matters: creating a pane was
//! already serialized by the state mutex it no longer holds, while every other request, the
//! observation consumer and output publication now run freely alongside it.
//! The lock itself lives on the facade, as `ShellIo::pane_creation_transaction`, so it is per
//! daemon rather than per process: two independent daemons share no session model and have
//! nothing to roll back over each other, and a process-wide lock would make every one of them
//! queue behind every other.

use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::time::Duration;

use rmux_core::{PaneGeometry, PaneId};
use rmux_proto::{ProcessCommand, RmuxError, SessionName, TerminalSize};

use marsh_core::shellmux::{CommandOptions, JobIo, ShellId, SpawnOptions, TerminalGeometry};

use crate::io::{Route, ShellHandle, ShellIo};
use crate::terminal::{validate_process_command, TerminalProfile};

/// How long a graceful stop is given before the job is stopped by force.
const GRACEFUL_TERMINATION_TIMEOUT: Duration = Duration::from_millis(100);
/// How long a forced stop is waited on before the pane stops caring.
const HARD_TERMINATION_TIMEOUT: Duration = Duration::from_millis(500);

/// The surface a newly opened pane job is routed to.
///
/// Installed under the facade's creation-or-adoption lock, in the same critical section as the
/// spawn, so the observation consumer cannot see this job's `Opened` before its route exists and
/// adopt it a second time as an externally created shell.
#[derive(Clone, Debug)]
pub(crate) struct PaneRoute {
    /// The session the pane's window lives in when it is created.
    pub(crate) session: SessionName,
    /// The pane's stable id, which survives moves, links and renames.
    pub(crate) pane: PaneId,
    /// The output generation the caller has reserved for this job's bytes.
    ///
    /// Reserved before the spawn rather than read back after the pane's transcript is installed,
    /// because the spawn happens with no handler lock held: the engine can deliver this job's
    /// first chunk before the commit runs. Carrying the reserved number means a late chunk from a
    /// *previous* generation — a pane that was respawned while this job was opening — is rejected
    /// against the generation its own route names, instead of landing in the replacement pane.
    pub(crate) generation: u64,
}

/// One pane's managed terminal job.
#[derive(Debug)]
pub(crate) struct PaneTerminal {
    /// The job generation this pane presents.
    handle: ShellHandle,
    /// The facade the job was admitted through, and the only route back to it.
    io: ShellIo,
    /// The geometry rmux last asked for.
    ///
    /// Held here rather than read back from the terminal because it is the *layout's* answer:
    /// rmux decides a pane's cells and tells the engine, and a program that resized its own
    /// terminal underneath has not changed what the layout says the pane is.
    size: TerminalSize,
    /// Whether a stop has already been requested for this job.
    termination_attempted: bool,
    /// The window name this pane's command implies, when it implies one.
    runtime_window_name: Option<String>,
    /// The spawn metadata the pane was created with.
    profile: TerminalProfile,
}

impl PaneTerminal {
    /// Wraps an admitted job as the terminal of one pane.
    pub(crate) fn new(
        handle: ShellHandle,
        io: ShellIo,
        size: TerminalSize,
        runtime_window_name: Option<String>,
        profile: TerminalProfile,
    ) -> Self {
        Self {
            handle,
            io,
            size,
            termination_attempted: false,
            runtime_window_name,
            profile,
        }
    }

    /// The job this pane presents, for input, observation and stable-id routing.
    pub(crate) const fn handle(&self) -> &ShellHandle {
        &self.handle
    }

    /// An unleased facade clone bound to the same host as [`Self::handle`].
    pub(crate) fn io(&self) -> ShellIo {
        self.io.unleased()
    }

    /// The geometry rmux last asked this pane's terminal for.
    #[cfg(test)]
    pub(crate) const fn size(&self) -> TerminalSize {
        self.size
    }

    /// Asks the engine for a new size, and records it as this pane's answer immediately.
    ///
    /// The physical `ioctl` is the engine's to perform and is therefore asynchronous, while every
    /// caller here is a synchronous layout mutation holding the daemon's request mutex. Recording
    /// the size first and dispatching the application is what keeps those two facts compatible:
    /// a `#{pane_width}` read straight after a resize answers what was asked for rather than
    /// racing the `ioctl`.
    ///
    /// Two resizes dispatched out of order still settle correctly. The engine applies sizes under
    /// its own per-mux resize lock and re-reads the latest desired geometry inside it, so the
    /// terminal ends at whichever size the job table settled on — never at the older of the two.
    pub(crate) fn resize(&mut self, size: TerminalSize) {
        self.size = size;
        let io = self.io.unleased();
        let handle = self.handle.clone();
        let id = self.handle.id().clone();
        let geometry = TerminalGeometry {
            rows: size.rows.max(1),
            cols: size.cols.max(1),
        };
        self.io.runtime().spawn(async move {
            if let Err(error) = io.resize(&handle, geometry).await {
                tracing::debug!(
                    shell = id.as_str(),
                    rows = geometry.rows,
                    cols = geometry.cols,
                    "pane terminal refused a resize: {error}"
                );
            }
        });
    }

    /// A borrowed descriptor on this pane's terminal, for metadata probes only.
    ///
    /// Read by the foreground-process and current-path probes, which ask the kernel what the
    /// terminal's foreground process group is. It is never used for I/O: reading it would steal
    /// bytes from the engine's own pump, and writing it would bypass the ordering and closure
    /// checks that [`ShellIo::write_input`] exists to enforce.
    pub(crate) fn terminal_fd(&self) -> Option<BorrowedFd<'_>> {
        self.handle.shell().terminal_fd().ok()
    }

    /// The process group currently in the foreground of this pane's terminal, when one is.
    ///
    /// `None` for an idle pane, and that is the honest answer: the shell is embedded in this
    /// daemon, so an idle pane has no child process at all. A terminal with no foreground group
    /// reports none, and the daemon's own identity is refused explicitly — handing that back
    /// would give a caller a pid it could signal, and signalling it would take down the server.
    ///
    /// Best effort, as it always was: a group whose leader has exited can linger as a terminal's
    /// foreground group until something else claims it.
    pub(crate) fn pid(&self) -> Option<u32> {
        let foreground = rmux_os::process::unix::foreground_pid(self.terminal_fd()?)?;
        (foreground != std::process::id()).then_some(foreground)
    }

    /// The path of this pane's terminal, when the kernel named it.
    pub(crate) fn tty_path(&self) -> Option<PathBuf> {
        self.handle
            .shell()
            .tty_path()
            .map(std::path::Path::to_path_buf)
    }

    /// Whether the job this pane presents is still open.
    ///
    /// Compared by snapshot id, not by name: a job that closed and whose name was reused is a
    /// different pane, and answering "alive" for it would let input reach a shell the caller
    /// never addressed.
    pub(crate) fn is_alive(&self) -> bool {
        self.io
            .job(self.handle.id())
            .is_some_and(|view| view.sandbox.uid == self.handle.sandbox().uid)
    }

    /// Resumes this pane's stopped command, reporting whether anything was asked to continue.
    ///
    /// Addresses the running command's own process groups through the engine rather than a pid
    /// this server guessed. An idle pane has nothing to continue.
    pub(crate) fn continue_if_stopped(&self) -> bool {
        if self.pid().is_none() {
            return false;
        }
        self.io
            .signal(&self.handle, marsh_core::Signal::Continue)
            .is_ok()
    }

    /// The spawn metadata this pane was created with.
    pub(crate) const fn profile(&self) -> &TerminalProfile {
        &self.profile
    }

    /// The window name this pane's command implies.
    pub(crate) fn runtime_window_name(&self) -> Option<&str> {
        self.runtime_window_name.as_deref()
    }

    /// Stops this pane's job: gracefully first, by force once the grace has run out.
    ///
    /// Idempotent per pane. The wait happens on the daemon's runtime rather than on the caller,
    /// because every caller is a synchronous layout or lifecycle mutation holding the request
    /// mutex, and blocking that for the length of a process teardown would stall every other
    /// session in the daemon.
    pub(crate) fn terminate_with_bounded_grace(&mut self) {
        if self.termination_attempted {
            return;
        }
        self.termination_attempted = true;
        let io = self.io.unleased();
        let handle = self.handle.clone();
        self.io.runtime().spawn(stop_with_grace(io, handle));
    }

    /// Stops this pane's job without waiting for it, consuming the pane.
    ///
    /// This is the *only* way a pane terminal stops its job, and it is deliberately explicit.
    /// There is no `Drop` implementation: a linked pane is one shell presented through several
    /// aliases, and dropping one alias' record — which a `HashMap::remove` during a window move,
    /// a session transfer or a rollback does routinely — must not signal the shell every other
    /// alias is still showing. Termination is therefore an owner's decision rather than a
    /// side effect of a value going out of scope, and each removal path states which it is.
    pub(crate) fn terminate_in_background(mut self) {
        self.terminate_with_bounded_grace();
    }
}

/// Stops `handle` gracefully, then by force if it has not closed in time.
///
/// The forced stop is not skipped when the graceful one reports an error: a graceful stop can be
/// refused for reasons that leave the job perfectly alive, and a pane the user killed has to go.
async fn stop_with_grace(io: ShellIo, handle: ShellHandle) {
    if io.stop(&handle, false).await.is_ok()
        && tokio::time::timeout(GRACEFUL_TERMINATION_TIMEOUT, handle.wait_closed())
            .await
            .is_ok()
    {
        return;
    }
    if let Err(error) = io.stop(&handle, true).await {
        tracing::debug!(
            shell = handle.id().as_str(),
            "forced pane terminal stop failed: {error}"
        );
    }
    let _ = tokio::time::timeout(HARD_TERMINATION_TIMEOUT, handle.wait_closed()).await;
}

/// Everything one pane's terminal is opened with, owned so it can cross an `await`.
///
/// Owned rather than borrowed because that is the whole point of the transaction it belongs to:
/// the caller computes this under the handler's state lock, *releases* the lock, and only then
/// opens the job. A borrowed request would pin the state the caller is trying to let go of.
pub(crate) struct PaneTerminalRequest {
    /// The layout cell the pane occupies.
    pub(crate) geometry: PaneGeometry,
    /// The environment, directory and shell decision the pane was prepared with.
    pub(crate) profile: TerminalProfile,
    /// The window name this pane's command implies, when it implies one.
    pub(crate) runtime_window_name: Option<String>,
    /// The workload the pane was asked for, if any.
    pub(crate) command: Option<ProcessCommand>,
    /// The surface the job's bytes belong to.
    pub(crate) route: PaneRoute,
    /// The shell id to open under, for a caller that already parsed one.
    ///
    /// `None` for every ordinary rmux pane: the engine allocates from its own anonymous series.
    pub(crate) shell_id: Option<ShellId>,
    /// Whether the job's lifetime is the engine's decision rather than rmux's.
    ///
    /// `false` for ordinary panes, whose explicit one-shot command closes the pane when it ends.
    /// `true` for a job created through the shell prompt's `&` and directory forms, where core's
    /// anonymous-job rules — automatic closure, cancellable by `keep` — are the semantics the
    /// user asked for and rmux must not override.
    pub(crate) follow_mux_lifetime: bool,
}

/// Opens one pane's terminal as a managed job, and routes it to that pane.
///
/// The spawn and the route installation share one hold of the facade's creation-or-adoption lock.
/// That is the whole reason the lock exists: the engine announces `Opened` from its own queue, and
/// an announcement that arrived before this function returned would otherwise reach the
/// observation consumer as an unmapped job and be adopted as a *second*, externally created
/// window for the pane being created right here.
///
/// The handler's state lock must **not** be held across this call. It awaits a shell build, a
/// snapshot creation and the admission lock, and admission is also taken by the observation
/// consumer's adoption path — which needs handler state. Holding both in the opposite order is a
/// lock inversion, and the deadlock it produces has no timeout and nothing logged.
///
/// # What the pane starts
///
/// An embedded pane with no command is opened idle and given the prompt reader: it is a shell
/// waiting for a program, not a program, and giving it a command would close it the moment that
/// command finished. Every other combination has a line, composed by
/// [`TerminalProfile::pane_workload_line`], and the line is what the pane *is* — so it is admitted
/// with `close_on_finish` and gets no prompt.
///
/// The line is admitted separately from the creation, because creation no longer takes one at
/// all. An ordinary pane's closure therefore belongs to its *command*, which `keep` may not
/// revoke — so selecting a fast one-shot pane between its last byte and its verdict cannot
/// quietly make it permanent. [`PaneTerminalRequest::follow_mux_lifetime`] is the one case that
/// wants core's cancellable automatic closure instead, and says so through
/// [`SpawnOptions::automatic_close`](marsh_core::shellmux::SpawnOptions::automatic_close).
///
/// # Errors
///
/// Fails when the workload is unusable (an empty argv), when the profile's environment, directory
/// or configured shell cannot be expressed to the engine, and with whatever the engine reported
/// about the name, the directory, the geometry or the shell. A failed spawn admitted no job — the
/// engine releases the name and closes every descriptor it opened — so there is nothing to
/// tombstone and nothing for the consumer to adopt. A job that opened but whose command was
/// refused is stopped here rather than left running with no surface.
pub(crate) async fn open_pane_terminal(
    io: &ShellIo,
    request: PaneTerminalRequest,
) -> Result<PaneTerminal, RmuxError> {
    let PaneTerminalRequest {
        geometry,
        profile,
        runtime_window_name,
        command,
        route,
        shell_id,
        follow_mux_lifetime,
    } = request;
    validate_process_command(command.as_ref())?;
    let line = profile.pane_workload_line(command.as_ref())?;
    let environment = profile.shell_environment()?;
    // The profile's own directory, as a host path; the core discovers the seed it lies in.
    let size = pane_terminal_size(geometry);
    // A pane whose lifetime the engine owns keeps core's anonymous-shell rules: it reclaims
    // itself once its line ends, and `keep` may cancel that. Every ordinary pane closes because
    // its own one-shot command said so, which `keep` may not revoke.
    let automatic_close = follow_mux_lifetime && shell_id.is_none() && line.is_some();
    let options = SpawnOptions {
        io: JobIo::Terminal {
            geometry: Some(TerminalGeometry {
                rows: size.rows,
                cols: size.cols,
            }),
        },
        environment: Some(environment),
        automatic_close,
    };

    let admission = io.admission_lock().lock().await;
    let handle = io
        .open_shell(profile.cwd(), shell_id, options)
        .await
        .map_err(|error| {
            RmuxError::spawn_failed(format!(
                "{} shell: {error}",
                rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX
            ))
        })?;
    io.install_route(
        handle.sandbox().uid.clone(),
        Route::Pane {
            session: route.session,
            pane: route.pane,
            generation: route.generation,
        },
    );
    drop(admission);

    match line.as_deref() {
        // Scheduled after the route and outside the admission lock. The pane *is* this line, so
        // it is not awaited to completion: the pane has to exist and draw while it runs.
        Some(line) => {
            if let Err(error) = io
                .start_command(
                    &handle,
                    line,
                    CommandOptions {
                        // An engine-owned pane already reclaims itself; every other one closes
                        // because this command said so.
                        close_on_finish: !automatic_close,
                        on_accept: None,
                    },
                )
                .await
            {
                // The shell exists and nothing will ever present it: the pane it was opened for
                // is about to be rolled back. Leaving it would keep a snapshot, a principal and
                // an idle terminal alive with no surface and no owner.
                io.install_route(handle.sandbox().uid.clone(), Route::Failed);
                io.runtime().spawn(stop_with_grace(io.unleased(), handle));
                return Err(RmuxError::spawn_failed(format!(
                    "{} shell: {error}",
                    rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX
                )));
            }
        }
        // An idle embedded pane, which needs the prompt reader: nothing else in this daemon reads
        // a pane's keyboard, composes a prompt or decides when a typed line is finished, so
        // without it a user attaches, types, and watches keystrokes pile up on a slave nobody is
        // reading.
        None => crate::pane_repl::spawn(io.unleased(), handle.clone()),
    }

    Ok(PaneTerminal::new(
        handle,
        io.unleased(),
        size,
        runtime_window_name,
        profile,
    ))
}

/// The geometry a pane's terminal is opened or resized at.
///
/// Clamped away from zero in both dimensions, because a zero-sized terminal is not a size the
/// engine accepts and a layout cell can legitimately compute to zero mid-rearrangement.
pub(crate) fn pane_terminal_size(geometry: PaneGeometry) -> TerminalSize {
    TerminalSize::new(geometry.cols().max(1), geometry.rows().max(1))
}

#[cfg(test)]
mod tests {
    use rmux_core::PaneGeometry;
    use rmux_proto::TerminalSize;

    use super::pane_terminal_size;

    #[test]
    fn pane_terminal_size_never_opens_or_resizes_a_zero_sized_terminal() {
        assert_eq!(
            pane_terminal_size(PaneGeometry::new(0, 0, 0, 0)),
            TerminalSize::new(1, 1)
        );
        assert_eq!(
            pane_terminal_size(PaneGeometry::new(0, 23, 80, 0)),
            TerminalSize::new(80, 1)
        );
    }
}
