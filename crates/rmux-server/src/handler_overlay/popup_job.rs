//! The shell behind a `display-popup`, and the surface it paints on.
//!
//! A popup is a terminal a user is looking at that happens not to be a pane: it has a screen, a
//! parser, a size, keyboard input and an exit. So it is backed by exactly what a pane is backed by
//! — one terminal job on this daemon's single [`ShellMux`](marsh_core::shellmux::ShellMux) — and
//! not by a private pseudoterminal with its own child, its own reader thread and its own
//! signal-escalation ladder, which is what it used to be.
//!
//! # What that replaces
//!
//! | Was | Is |
//! |---|---|
//! | `rmux_pty::ChildCommand` + `spawn` | [`ShellIo::spawn`] with terminal I/O, under the admission lock |
//! | a descriptor reader thread per popup | the daemon's one observation consumer, through [`RequestHandler::apply_popup_output`] |
//! | `PtyIo::write_all` / `PtyIo::resize` | [`ShellIo::write_input`] / [`ShellIo::resize`] |
//! | `SIGHUP` → `SIGTERM` → `SIGKILL` on a thread | [`ShellIo::stop`] with `force`, which retires the job and discards what it staged |
//! | `child.try_wait()` polled every 50ms | the command's own completion, or the job's closure |
//!
//! # Whose status closes the popup
//!
//! `display-popup -E` closes on a zero status and `-EE` closes on any status, so *which* status is
//! not a detail. Two cases, and they are genuinely different:
//!
//! * a popup created with a command is a **one-shot**. Its status is that command's completion —
//!   the real one, gated: a known nonzero exit is preserved, and an exit of zero whose publication
//!   the policy refused reports `1`, because the work the user asked for did not happen.
//! * a popup created without one is **interactive**. It has a shell prompt, and the user may run
//!   any number of commands in it. None of those is the popup's exit; treating the first of them
//!   as one would close the popup the moment the user ran `ls`. Its status is the job's closure.
//!
//! # What still does not get a shell
//!
//! `no_job` popups, scrollable-text overlays and menus share this renderer and nothing else. They
//! are views over bytes the daemon already has. None of them reaches [`spawn_popup_job`], so none
//! of them allocates a job, a principal or a snapshot — sharing a renderer is not a reason to open
//! a shell.

use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use marsh_core::shellmux::{
    CommandHandle, CommandOptions, JobIo, Sandbox, SpawnOptions, TerminalGeometry,
};
use rmux_core::input::InputParser;
use rmux_core::{GridRenderOptions, Screen, Style};
use rmux_proto::{RmuxError, TerminalSize};

use crate::io::{Route, ShellHandle, ShellIo};
use crate::terminal::{parse_environment_assignments, TerminalProfile};

use super::super::{attach_support::ActiveAttachIdentity, RequestHandler};
pub(super) use super::popup_io::PopupIoReceipt;
#[cfg(test)]
use super::popup_io::POPUP_IO_QUEUE_CAPACITY;
use super::popup_io::{PopupIoOperation, PopupIoQueue};

pub(in super::super) struct PopupSurface {
    parser: InputParser,
    screen: Screen,
}

impl std::fmt::Debug for PopupSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopupSurface").finish_non_exhaustive()
    }
}

impl PopupSurface {
    pub(super) fn new(size: TerminalSize) -> Self {
        Self {
            parser: InputParser::new(),
            screen: Screen::new(size, 0),
        }
    }

    pub(super) fn append(&mut self, bytes: &[u8]) {
        self.parser.parse(bytes, &mut self.screen);
    }

    /// The terminal's answers to queries the program in this popup sent.
    ///
    /// A primary-device-attribute request, a palette query: the parser answers them while parsing,
    /// and those answers belong in the program's input. Draining them here rather than inside
    /// [`Self::append`] keeps the surface lock off the write path — the caller takes them, drops
    /// the lock, and only then writes.
    pub(super) fn take_replies(&mut self) -> Vec<u8> {
        self.parser.take_replies()
    }

    #[cfg(test)]
    pub(in crate::handler) fn append_for_test(&mut self, bytes: &[u8]) {
        self.append(bytes);
    }

    pub(super) fn resize(&mut self, size: TerminalSize) {
        self.screen.resize(size);
    }

    pub(super) fn mode(&self) -> u32 {
        self.screen.mode()
    }

    /// Visible rows of the popup surface, each rendered from a fresh SGR state.
    ///
    /// Popup content is terminal output, not a status format: capturing it
    /// without sequences discarded every colour and attribute the process
    /// emitted. Each row carries its own state because the renderer repositions
    /// the cursor per row, and `style` only paints the cells the process left
    /// unstyled, so `display-popup -s` keeps colouring the popup background.
    pub(super) fn rows(&self, style: &Style) -> Vec<Vec<u8>> {
        let options = GridRenderOptions {
            with_sequences: true,
            include_empty_cells: true,
            trim_spaces: false,
            ..GridRenderOptions::default()
        };
        (0..usize::from(self.screen.size().rows))
            .filter_map(|row| {
                self.screen
                    .render_visible_line_independent_with_default_style(row, options, style)
            })
            .collect()
    }
}

/// Everything needed to end one popup's job, without keeping the overlay alive.
///
/// Cloned into the I/O queue's cancellation hook and into the waiter, neither of which owns the
/// popup: the overlay does. That separation is what makes dropping an overlay sufficient to stop
/// its shell.
#[derive(Debug, Clone)]
struct PopupShellControl {
    io: ShellIo,
    handle: ShellHandle,
}

impl PopupShellControl {
    /// Ends the popup's job, forcefully.
    ///
    /// Forced because a popup is being taken off the screen: there is no user left to read a
    /// prompt, and a graceful stop would wait for a program that may never exit. Forced also means
    /// *discarded*, so whatever the popup's shell staged and had not yet had approved does not
    /// reach the seed on the way out — closing a window is not a way to publish.
    ///
    /// Synchronous, because it is reached from `Drop` and from the I/O queue's cancellation hook.
    /// The stop itself is scheduled onto the engine's own runtime, which is where every other
    /// managed operation already runs.
    fn terminate(&self) {
        let io = self.io.unleased();
        let handle = self.handle.clone();
        io.runtime().clone().spawn(async move {
            if let Err(error) = io.stop(&handle, true).await {
                tracing::debug!(
                    shell = handle.id().as_str(),
                    "popup shell was already gone when its overlay closed: {error}"
                );
            }
        });
    }
}

/// Ties the popup's job to the last overlay holding it.
#[derive(Debug)]
struct PopupShellLifetime {
    control: PopupShellControl,
}

#[derive(Debug, Clone)]
pub(in super::super) struct PopupJob {
    /// The job's snapshot identity, which is how an observation finds this popup.
    ///
    /// The uid rather than the name: a popup's shell is anonymous and its name may be reused, and
    /// a surface that accepted bytes by name would eventually paint another job's output.
    uid: marsh_core::shellmux::SnapshotUid,
    /// The one-shot command this popup was created to run, if it was created to run one.
    ///
    /// Retained rather than re-resolved because a receipt outlives its job: the waiter must still
    /// be able to read the verdict of a command whose job has already closed.
    pending_command: Option<CommandHandle>,
    io_queue: PopupIoQueue,
    shell: Arc<PopupShellLifetime>,
}

impl PopupJob {
    pub(super) fn enqueue_write(&self, bytes: &[u8]) -> io::Result<PopupIoReceipt> {
        self.io_queue
            .enqueue(PopupIoOperation::Write(bytes.to_vec()))
    }

    pub(super) fn enqueue_resize(&self, size: TerminalSize) -> io::Result<PopupIoReceipt> {
        self.io_queue.enqueue(PopupIoOperation::Resize(size))
    }

    /// The job generation this popup presents.
    pub(super) fn uid(&self) -> &marsh_core::shellmux::SnapshotUid {
        &self.uid
    }

    pub(in super::super) fn terminate(&self) {
        // Cancelling the queue drains every pending receipt before stopping the
        // job, so a caller waiting on a write learns it was cancelled rather
        // than waiting out the receipt timeout.
        self.io_queue.cancel();
    }

    /// Whether this popup's shell is still a live job in the engine.
    ///
    /// The managed replacement for polling a child's `try_wait`. `closing` is checked as well as
    /// presence because a forced stop marks the job closing before its snapshot is reclaimed, and
    /// a test asserting that a replaced popup was ended must not have to wait out reclamation to
    /// see it. The uid is compared so a name reused by a later popup never reads as this one.
    #[cfg(test)]
    pub(in crate::handler) fn shell_is_live_for_test(&self) -> bool {
        let control = &self.shell.control;
        control
            .io
            .job(control.handle.id())
            .is_some_and(|view| {
                view.sandbox.uid == control.handle.sandbox().uid && !view.closing
            })
    }

    /// Replaces this popup's I/O worker with one that reports every write to `writer`.
    ///
    /// `writer` is *synchronous*, and that is the whole reason for the [`spawn_blocking`] hop.
    /// The queue's executor is now awaited inline on a runtime thread — production's two
    /// operations are `ShellIo::write_input` and `ShellIo::resize`, which are async — so calling a
    /// blocking closure from inside it parks that runtime thread for as long as the closure runs.
    /// On a `#[tokio::test]`, which is current-thread, that thread is the *only* one: the receipt
    /// deadline is a timer that can never fire, and a test written to prove that a stuck popup
    /// write times out instead observes it succeeding the instant the fixture releases it.
    ///
    /// So a blocked write is modelled where a blocked write belongs — on a blocking thread —
    /// leaving the runtime free to run the deadline, the attached client and the rest of the
    /// server, which is precisely the property these tests exist to measure.
    ///
    /// [`spawn_blocking`]: tokio::task::spawn_blocking
    #[cfg(test)]
    pub(in crate::handler) fn with_test_writer<F>(&self, writer: F) -> Self
    where
        F: Fn(Vec<u8>) -> io::Result<()> + Send + Sync + 'static,
    {
        let shell = Arc::clone(&self.shell);
        let cancellation_shell = shell.control.clone();
        let writer = Arc::new(writer);
        Self {
            uid: self.uid.clone(),
            pending_command: self.pending_command.clone(),
            io_queue: PopupIoQueue::spawn_with_cancel(
                move |operation| {
                    let writer = Arc::clone(&writer);
                    async move {
                        match operation {
                            PopupIoOperation::Write(bytes) => {
                                blocking_test_io(move || writer(bytes)).await
                            }
                            PopupIoOperation::Resize(_) => Ok(()),
                        }
                    }
                },
                move || cancellation_shell.terminate(),
            ),
            shell,
        }
    }

    /// Replaces this popup's I/O worker with one that reports every resize to `resize`.
    ///
    /// The synchronous callback runs off the runtime thread for the same reason as
    /// [`with_test_writer`](Self::with_test_writer).
    #[cfg(test)]
    pub(in crate::handler) fn with_test_resize<F>(&self, resize: F) -> Self
    where
        F: Fn(TerminalSize) -> io::Result<()> + Send + Sync + 'static,
    {
        let shell = Arc::clone(&self.shell);
        let cancellation_shell = shell.control.clone();
        let resize = Arc::new(resize);
        Self {
            uid: self.uid.clone(),
            pending_command: self.pending_command.clone(),
            io_queue: PopupIoQueue::spawn_with_cancel(
                move |operation| {
                    let resize = Arc::clone(&resize);
                    async move {
                        match operation {
                            PopupIoOperation::Write(_) => Ok(()),
                            PopupIoOperation::Resize(size) => {
                                blocking_test_io(move || resize(size)).await
                            }
                        }
                    }
                },
                move || cancellation_shell.terminate(),
            ),
            shell,
        }
    }
}

/// Runs one synchronous test callback on a blocking thread and reports its result.
///
/// A join failure is reported as an I/O error rather than unwrapped: the only way to reach it is
/// a panic inside the fixture's own callback, and surfacing that as a failed popup write keeps
/// the assertion in the test that panicked instead of tearing down the queue worker.
#[cfg(test)]
async fn blocking_test_io<F>(operation: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .unwrap_or_else(|error| Err(io::Error::other(error)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum PopupDragMode {
    Off,
    Move { dx: u16, dy: u16 },
    Resize,
}

/// Opens the terminal job one `display-popup` runs in.
///
/// The returned byte vector is the popup's initial screen content, and it is empty: a job's first
/// bytes arrive through the observation consumer like every other byte, and synthesising a
/// starting frame here would paint something the shell never wrote.
///
/// # The admission transaction
///
/// The spawn and the route installation happen under one hold of [`ShellIo::admission_lock`]. That
/// is not optional: the engine's `Opened` observation can reach the consumer *before* `spawn`
/// returns here, and a job the consumer finds unrouted is treated as externally created and
/// adopted as a window. Installing `Route::Popup` before the lock is released is what tells the
/// consumer this job already has a surface — one that is emphatically not a pane.
///
/// # Errors
///
/// Fails when the popup's start directory was *named* by the caller and lies outside this
/// daemon's seed, when the environment holds non-UTF-8 data, when the engine refuses the job, and
/// when the one-shot command cannot be admitted into it. A directory nobody named — the daemon's
/// own process cwd, which every popup inherits when `display-popup -d` is absent — is not an
/// error: it falls back to the seed root, exactly as a pane's does.
pub(super) async fn spawn_popup_job(
    io: &ShellIo,
    size: TerminalSize,
    profile: &TerminalProfile,
    shell_command: Option<&str>,
    environment: &[String],
) -> Result<(PopupJob, Vec<u8>), RmuxError> {
    let overrides = parse_environment_assignments(environment)?;
    let environment = popup_environment(profile, &overrides)?;
    let directory = profile.seed_relative_dir(&io.executor_info())?;
    let geometry = TerminalGeometry {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
    };

    let handle = {
        let admission = io.admission_lock().lock().await;
        let handle = io
            .spawn(
                &directory,
                None,
                None,
                SpawnOptions {
                    io: JobIo::Terminal {
                        geometry: Some(geometry),
                    },
                    environment: Some(environment),
                },
            )
            .await
            .map_err(crate::managed_workload::io_error)?;
        io.install_route(handle.sandbox().uid.clone(), Route::Popup);
        drop(admission);
        handle
    };

    // Admitted after the route, never before: a command that produced output while the job was
    // still unrouted would have its first bytes adopted into a window instead of this popup.
    let command = match shell_command {
        Some(text) => {
            let started = io
                .start_in(
                    &handle,
                    text,
                    CommandOptions {
                        on_finish: None,
                        // The popup ran one thing and that thing is over; the closure is recorded
                        // at admission so nothing can slip a second line into the gap between the
                        // command's last byte and its verdict.
                        close_on_finish: true,
                    },
                )
                .await;
            match started {
                Ok(command) => Some(command),
                Err(error) => {
                    // The job was admitted and will never run anything. Leaving it open would hold
                    // a snapshot until the daemon shut down.
                    let _ = io.stop(&handle, true).await;
                    io.forget_route(&handle.sandbox().uid);
                    return Err(crate::managed_workload::io_error(error));
                }
            }
        }
        None => {
            // An interactive popup gets the same shell prompt a pane gets, on the same idle
            // terminal lease. A second prompt implementation for popups would be a second editor,
            // a second history and a second set of key bindings to keep in step.
            crate::pane_repl::spawn(io.unleased(), handle.clone());
            None
        }
    };

    let control = PopupShellControl {
        io: io.unleased(),
        handle: handle.clone(),
    };
    let queue_control = control.clone();
    let operation_io = io.unleased();
    let operation_handle = handle.clone();
    let io_queue = PopupIoQueue::spawn_with_cancel(
        move |operation| {
            let io = operation_io.unleased();
            let handle = operation_handle.clone();
            async move {
                match operation {
                    PopupIoOperation::Write(bytes) => io
                        .write_input(&handle, &bytes)
                        .await
                        .map_err(|error| io::Error::other(error.to_string())),
                    PopupIoOperation::Resize(size) => io
                        .resize(
                            &handle,
                            TerminalGeometry {
                                rows: size.rows.max(1),
                                cols: size.cols.max(1),
                            },
                        )
                        .await
                        .map_err(|error| io::Error::other(error.to_string())),
                }
            }
        },
        move || queue_control.terminate(),
    );

    Ok((
        PopupJob {
            uid: handle.sandbox().uid.clone(),
            pending_command: command,
            io_queue,
            shell: Arc::new(PopupShellLifetime { control }),
        },
        Vec::new(),
    ))
}

/// The complete environment a popup's shell is built with.
///
/// The profile's resolved environment first — `TERM`, `RMUX`, `TMUX`, `RMUX_PANE`, `TMUX_PANE` and
/// every option-store override and removal already folded in — and then `display-popup -e`
/// assignments on top, which is the precedence upstream produced by clearing the environment and
/// applying the two in that order.
fn popup_environment(
    profile: &TerminalProfile,
    overrides: &std::collections::HashMap<String, String>,
) -> Result<brush_core::env::ShellEnvironment, RmuxError> {
    let mut pairs: Vec<(std::ffi::OsString, std::ffi::OsString)> = profile
        .raw_environment()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    for (name, value) in overrides {
        let name = std::ffi::OsString::from(name);
        match pairs.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, existing)) => *existing = std::ffi::OsString::from(value),
            None => pairs.push((name, std::ffi::OsString::from(value))),
        }
    }
    crate::terminal::shell_environment_from_pairs(
        pairs
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str())),
    )
}

impl RequestHandler {
    /// Paints one chunk of a popup-routed job's terminal stream onto its surface.
    ///
    /// Called from the observation consumer's per-stream delivery task, inline and awaited, which
    /// is the whole ordering guarantee: the chunk is parsed before the next one is handed over, so
    /// the popup's escape-sequence state cannot be corrupted by two chunks racing for the surface
    /// lock. It is deliberately not an independent bounded observer — a popup that fell behind
    /// would receive an explicit gap, and a terminal emulator cannot resynchronise from one.
    ///
    /// The route check comes first and is a single map lookup, so the pane path — which is every
    /// other terminal job in the daemon — pays for a hash lookup rather than for scanning the
    /// attached-client table.
    pub(crate) async fn apply_popup_output(&self, shell: &Sandbox, bytes: &[u8]) {
        // An empty chunk is this daemon's end-of-file marker. Parsing it would do nothing and
        // scheduling a refresh for it would repaint a popup for no reason.
        if bytes.is_empty() {
            return;
        }
        let Some(io) = self.shell_io() else {
            return;
        };
        if !io.is_popup_route(&shell.uid) {
            return;
        }
        let Some((identity, popup_id, surface)) = self.popup_surface_for_job(&shell.uid).await
        else {
            return;
        };
        let replies = {
            let mut surface = surface.lock().expect("popup surface");
            surface.append(bytes);
            surface.take_replies()
        };
        if !replies.is_empty() {
            // Straight through managed input, not the popup's I/O queue: a terminal reply belongs
            // to the program that asked the question and must keep its place among that program's
            // own bytes, while the queue exists to serialize *user* input against resizes.
            if let Ok(job) = io.shell(&shell.id) {
                if job.sandbox().uid == shell.uid {
                    if let Err(error) = io.write_input(&job, &replies).await {
                        tracing::debug!(
                            shell = shell.id.as_str(),
                            "dropping popup terminal replies: {error}"
                        );
                    }
                }
            }
        }
        let _ = self.popup_reader_tick(identity, popup_id).await;
    }

    /// The popup presenting one job generation, if one still is.
    ///
    /// Scans the attached clients, which is affordable only because the caller has already
    /// established that this job is popup-routed: there is at most one popup per attached client,
    /// and a popup produces far fewer chunks than the panes this daemon is pumping.
    async fn popup_surface_for_job(
        &self,
        uid: &marsh_core::shellmux::SnapshotUid,
    ) -> Option<(ActiveAttachIdentity, u64, Arc<StdMutex<PopupSurface>>)> {
        let active_attach = self.active_attach.lock().await;
        for (attach_pid, active) in &active_attach.by_pid {
            let Some(super::state::ClientOverlayState::Popup(popup)) = active.overlay.as_ref()
            else {
                continue;
            };
            if popup.job.as_ref().is_some_and(|job| job.uid() == uid) {
                return Some((
                    active.identity(*attach_pid),
                    popup.id,
                    Arc::clone(&popup.surface),
                ));
            }
        }
        None
    }

    /// Watches one popup's shell and reports the status its close policy is decided from.
    ///
    /// `command` is `Some` for a one-shot popup and `None` for an interactive one, and that is the
    /// whole difference: a one-shot's verdict is its command's, while an interactive popup must
    /// wait for the job itself to close. Waiting on a command in the interactive case would take
    /// the first line the user typed for the popup's exit.
    pub(super) fn spawn_popup_waiter(
        &self,
        identity: ActiveAttachIdentity,
        popup_id: u64,
        job: PopupJob,
    ) {
        // The overlay owns the popup's lifetime. The waiter keeps only the facade, the handle and
        // the command it needs, so dropping an overlay without an explicit teardown still stops
        // the shell and releases the I/O queue.
        //
        // The facade comes from the job rather than from `self.shell_io()`: this job was admitted
        // through one, so one exists by construction, and reading it back from the handler would
        // add a way for the waiter to silently not exist.
        let io = job.shell.control.io.unleased();
        let handle = job.shell.control.handle.clone();
        let command = job.pending_command.clone();
        drop(job);
        let handler = self.clone();
        io.runtime().clone().spawn(async move {
            let status = match command {
                Some(command) => match command.wait().await {
                    Ok(completion) => gated_status(
                        completion.exit_code,
                        completion.is_published(),
                        false,
                    ),
                    // No verdict was produced at all: teardown or a lost producer. There is no
                    // honest exit code for that, and `1` is what every other refusal reports.
                    Err(_) => 1,
                },
                None => match handle.wait_closed().await {
                    Ok(end) => {
                        let failed = end.error.is_some();
                        match end.completion.as_ref() {
                            Some(completion) => gated_status(
                                completion.exit_code,
                                completion.is_published(),
                                failed,
                            ),
                            // Closed without ever running a line: nothing was refused.
                            None => i32::from(failed),
                        }
                    }
                    Err(_) => 1,
                },
            };
            let _ = handler.popup_job_finished(identity, popup_id, status).await;
        });
    }
}

/// The status `display-popup -E`/`-EE` decides from.
///
/// Four facts collapse into one integer, which is why it is written down once:
///
/// * an infrastructure failure is `1` — no program produced a status, and there is no honest code
///   for "the machinery broke";
/// * a known nonzero process status is preserved as itself, because that is what the program said;
/// * an exit of zero whose publication was approved is `0`;
/// * an exit of zero whose publication was refused is `1`.
///
/// That last line is the point. `display-popup -E 'make install > /etc/thing'` whose write the
/// policy refused must not close as though it had succeeded: the command changed nothing.
const fn gated_status(exit_code: Option<i32>, published: bool, failed: bool) -> i32 {
    if failed {
        return 1;
    }
    match exit_code {
        Some(code) if code != 0 => code,
        _ => {
            if published {
                0
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
#[path = "popup_job_tests.rs"]
mod tests;
