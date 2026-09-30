//! `pipe-pane`: a pane's output into a command, and that command's output back into the pane.
//!
//! The command is a managed pipe job now, not a child of the daemon. Four consequences are worth
//! stating, because each of them used to be an accident of `std::process::Command`:
//!
//! * the text runs in the **embedded shell**, not an implicit `/bin/sh -c`, so marsh's builtins
//!   and instrumentation are in front of it like they are in front of a pane's own lines;
//! * `-O` writes the pane's bytes into the job's real standard input and ends it with a real end
//!   of file, so `cat >> log` closes its file instead of waiting for a descriptor that never goes
//!   away;
//! * `-I` forwards the job's standard output and standard error back through the destination
//!   pane's managed input, never a pseudoterminal master — the engine owns that terminal and it
//!   is the engine that orders this against whatever the pane's shell is already reading;
//! * the log is **staged**. A `pipe-pane -o 'cat >> log'` reaches the seed only when the gate
//!   approves the command's boundary, which is why closing a pipe waits for that verdict and
//!   reports a refusal rather than claiming the pipe closed cleanly.
//!
//! # Why closing is the interesting part
//!
//! Opening a pipe is one admission. Closing one has to do four things in order: stop feeding it,
//! deliver end of file, wait for the command to finish *and be gated*, and only then say whether
//! the user's log exists. A command that is still executing and will not finish is signalled and
//! then forced — and a forced job is discarded, which is a failure to report, not a clean close.
//! A command that is still being set up, or whose producers have finished and whose verdict is
//! being processed, is not a stuck consumer and is waited for, however long the disk takes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use marsh_core::shellmux::{CommandCompletion, CommandHandle, WaitError};
use rmux_core::events::OutputCursorItem;
use rmux_core::PaneId;
use rmux_proto::{ProcessCommand, RmuxError, SessionName};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::io::{ExecutionParts, InputWriter, OutputStream, ShellHandle, ShellIo};
use crate::managed_workload;
use crate::pane_io::{PaneOutputReceiver, PaneOutputSender};
use crate::terminal::TerminalProfile;

/// How long a closing pipe's executing command is given to finish before it is signalled, and
/// then forced.
///
/// This is the bound the pipe's own child waiter has always worked to, kept exactly. It applies
/// at each escalation step: once for the command to notice end of file, once more after
/// `SIGTERM`, because a logger that flushes on a signal deserves the same chance as one that
/// notices its input ending. It bounds execution only: a command still being set up is not yet
/// reading its input, and one whose producers have finished is having its verdict processed.
/// Neither has a fixed wall-clock deadline. The same bound caps draining the `-O` writer.
const PIPE_TERMINATION_GRACE: Duration = Duration::from_millis(250);

/// How many pane output chunks may be queued for a `-O` pipe's standard input.
///
/// The backpressure contract, unchanged: a pipe command that stops reading makes the pane output
/// forwarder wait at this depth rather than letting the daemon buffer a pane without bound.
const PANE_INPUT_QUEUE: usize = 64;

#[derive(Default)]
pub(crate) struct PanePipeStore {
    sessions: HashMap<SessionName, HashMap<PaneId, ActivePanePipe>>,
}

impl std::fmt::Debug for PanePipeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanePipeStore")
            .field("sessions", &self.sessions.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PanePipeStore {
    pub(crate) fn contains(&self, session_name: &SessionName, pane_id: PaneId) -> bool {
        self.sessions
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
    }

    pub(crate) fn insert(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        pipe: ActivePanePipe,
    ) -> Option<ActivePanePipe> {
        self.sessions
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, pipe)
    }

    pub(crate) fn remove(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<ActivePanePipe> {
        self.sessions
            .get_mut(session_name)
            .and_then(|panes| panes.remove(&pane_id))
    }

    pub(crate) fn remove_session(
        &mut self,
        session_name: &SessionName,
    ) -> HashMap<PaneId, ActivePanePipe> {
        self.sessions.remove(session_name).unwrap_or_default()
    }

    pub(crate) fn rename_session(
        &mut self,
        session_name: &SessionName,
        new_name: &SessionName,
    ) -> Result<(), RmuxError> {
        if !self.sessions.contains_key(session_name) {
            return Ok(());
        }
        if self.sessions.contains_key(new_name) {
            return Err(RmuxError::Server(format!(
                "pane pipes already exist for session {new_name}"
            )));
        }

        let mut sessions = std::mem::take(&mut self.sessions);
        let panes = sessions
            .remove(session_name)
            .expect("prevalidated pane pipes must exist");
        let replaced = sessions.insert(new_name.clone(), panes);
        debug_assert!(replaced.is_none());
        self.sessions = sessions;
        Ok(())
    }

    pub(crate) fn move_between_sessions(
        &mut self,
        source_session: &SessionName,
        destination_session: &SessionName,
        pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session || pane_ids.is_empty() {
            return Ok(());
        }

        let removed = self.remove_selected(source_session, pane_ids);
        if let Err(error) =
            self.ensure_destination_accepts(destination_session, removed.keys().copied())
        {
            self.sessions
                .entry(source_session.clone())
                .or_default()
                .extend(removed);
            return Err(error);
        }
        self.sessions
            .entry(destination_session.clone())
            .or_default()
            .extend(removed);
        Ok(())
    }

    pub(crate) fn swap_between_sessions(
        &mut self,
        source_session: &SessionName,
        source_pane_ids: &[PaneId],
        destination_session: &SessionName,
        destination_pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session {
            return Ok(());
        }

        let removed_source = self.remove_selected(source_session, source_pane_ids);
        let removed_destination = self.remove_selected(destination_session, destination_pane_ids);

        if let Err(error) =
            self.ensure_destination_accepts(source_session, removed_destination.keys().copied())
        {
            self.sessions
                .entry(source_session.clone())
                .or_default()
                .extend(removed_source);
            self.sessions
                .entry(destination_session.clone())
                .or_default()
                .extend(removed_destination);
            return Err(error);
        }
        if let Err(error) =
            self.ensure_destination_accepts(destination_session, removed_source.keys().copied())
        {
            self.sessions
                .entry(source_session.clone())
                .or_default()
                .extend(removed_source);
            self.sessions
                .entry(destination_session.clone())
                .or_default()
                .extend(removed_destination);
            return Err(error);
        }

        self.sessions
            .entry(source_session.clone())
            .or_default()
            .extend(removed_destination);
        self.sessions
            .entry(destination_session.clone())
            .or_default()
            .extend(removed_source);
        Ok(())
    }

    fn remove_selected(
        &mut self,
        session_name: &SessionName,
        pane_ids: &[PaneId],
    ) -> HashMap<PaneId, ActivePanePipe> {
        let session = self.sessions.entry(session_name.clone()).or_default();
        let mut removed = HashMap::new();
        for pane_id in pane_ids {
            if let Some(pipe) = session.remove(pane_id) {
                removed.insert(*pane_id, pipe);
            }
        }
        removed
    }

    fn ensure_destination_accepts<I>(
        &self,
        session_name: &SessionName,
        pane_ids: I,
    ) -> Result<(), RmuxError>
    where
        I: IntoIterator<Item = PaneId>,
    {
        let session = self.sessions.get(session_name);
        for pane_id in pane_ids {
            if session.is_some_and(|pipes| pipes.contains_key(&pane_id)) {
                return Err(RmuxError::Server(format!(
                    "pane pipe already exists for pane id {} in session {}",
                    pane_id.as_u32(),
                    session_name
                )));
            }
        }
        Ok(())
    }
}

/// How a pipe was asked to end.
///
/// The difference is not how much of the log was written. It is whether anyone asked for this
/// command's work at all: a requested close is a boundary the gate may approve, and a teardown is
/// the pipe's pane, window, session or daemon going away with nothing left to publish to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PipeStop {
    /// Someone asked this pipe to close and is waiting for the verdict.
    ///
    /// End of file is delivered and the command reaches its own boundary, so `cat > log` exits
    /// zero, is gated normally, and the log the user asked for is published.
    Close,
    /// The pipe was torn down with whatever it belonged to.
    ///
    /// Nobody asked for the log. The command is cancelled, which discards it through core
    /// admission rather than handing it the clean end of file that would make it publishable.
    Discard,
}

/// One live `pipe-pane`, from the store's point of view.
///
/// Everything the pipe actually owns lives in its supervising task; what is kept here is the way
/// to ask that task to end, and the way to hear what the gate said when it did.
pub(crate) struct ActivePanePipe {
    /// Asks the supervisor to end this pipe, and says which of the two ends it is.
    stop_tx: watch::Sender<Option<PipeStop>>,
    /// The supervisor's one report: `Ok` for an approved close, `Err` for anything else.
    closed_rx: oneshot::Receiver<Result<(), RmuxError>>,
}

impl std::fmt::Debug for ActivePanePipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivePanePipe").finish_non_exhaustive()
    }
}

impl ActivePanePipe {
    /// Opens a pipe for one pane.
    ///
    /// The job is a one-off, so it draws an automatic name rather than minting a principal that
    /// nothing will ever refer to again. Its directory and environment are the pane's, exactly as
    /// the child's were.
    ///
    /// # Errors
    ///
    /// Fails when the engine refused the job and when the pane's environment carries non-UTF-8
    /// data. Not for the directory: `pipe-pane` has no `-c`, so a pipe's directory is always the
    /// one its pane inherited, and [`managed_workload::spec`] falls such a directory back to the
    /// seed root rather than refusing something nobody asked for.
    pub(crate) async fn spawn(
        profile: &TerminalProfile,
        pane_output: PaneOutputSender,
        io: ShellIo,
        handle: ShellHandle,
        command: &str,
        read_from_pipe: bool,
        write_to_pipe: bool,
    ) -> Result<Self, RmuxError> {
        let spec = managed_workload::spec(
            &io,
            profile.cwd(),
            profile.raw_environment(),
            None,
            ProcessCommand::Shell(command.to_owned()),
        )?;
        let ExecutionParts {
            shell,
            command: receipt,
            stdin,
            stdout,
            stderr,
        } = managed_workload::start(&io, spec).await?.into_parts();

        let (stop_tx, stop_rx) = watch::channel(None);
        let (closed_tx, closed_rx) = oneshot::channel();
        let runtime = io.runtime().clone();

        let input = write_to_pipe.then(|| {
            let (pipe_tx, pipe_rx) = mpsc::channel(PANE_INPUT_QUEUE);
            PaneInput {
                producer: runtime.spawn(forward_pane_output_to_pipe(
                    stop_rx.clone(),
                    pane_output.subscribe(),
                    pipe_tx,
                )),
                writer: runtime.spawn(forward_pane_bytes_to_pipe(pipe_rx, stdin.clone())),
            }
        });
        let outputs = if read_from_pipe {
            vec![
                runtime.spawn(forward_pipe_output_to_pane(
                    stdout,
                    io.unleased(),
                    handle.clone(),
                    "standard output",
                )),
                runtime.spawn(forward_pipe_output_to_pane(
                    stderr,
                    io.unleased(),
                    handle,
                    "standard error",
                )),
            ]
        } else {
            // No `-I`, but these are real pipes rather than the null device the child used to be
            // given, so they still have to be read or the command blocks writing into a full one.
            vec![
                runtime.spawn(drain_pipe_output(stdout)),
                runtime.spawn(drain_pipe_output(stderr)),
            ]
        };

        runtime.spawn(supervise(PipeJob {
            io: io.unleased(),
            shell,
            command: receipt,
            text: command.to_owned(),
            stdin,
            input,
            outputs,
            stop_rx,
            closed_tx,
        }));

        Ok(Self { stop_tx, closed_rx })
    }

    /// Closes this pipe and reports what the gate did with it.
    ///
    /// For the paths where a user is waiting on the answer: a `pipe-pane` with no command, a
    /// `pipe-pane -o` toggle. A command that keeps executing cannot hold the request open: the
    /// supervisor signals and then forces it. Setup and verdict processing are awaited.
    ///
    /// # Errors
    ///
    /// Fails when the command's effects were not published — the log the user asked for does not
    /// exist — and when it had to be forced.
    pub(crate) async fn close(self) -> Result<(), RmuxError> {
        let _ = self.stop_tx.send(Some(PipeStop::Close));
        match self.closed_rx.await {
            Ok(outcome) => outcome,
            // The supervisor went away without reporting, which happens only when the runtime
            // itself is going away. That is not this pipe's failure to attribute to the caller.
            Err(_) => Ok(()),
        }
    }

    /// Tears this pipe down without waiting for its verdict.
    ///
    /// For the paths where nothing asked for the log and nothing is left to give it to: a pane
    /// being killed, a window or session being torn down, the daemon going away. The command is
    /// cancelled rather than closed, so whatever it staged is discarded at the core's admission
    /// boundary instead of being handed the end of file that would let it finish and publish.
    pub(crate) fn stop(self) {
        let _ = self.stop_tx.send(Some(PipeStop::Discard));
    }
}

/// The pane-to-standard-input half of a `-O` pipe.
struct PaneInput {
    /// Reads the pane's output into the bounded queue. Aborted when the pipe closes: the pane
    /// keeps producing, and a closing pipe must stop consuming it at a chunk boundary.
    producer: JoinHandle<()>,
    /// Drains that queue into the job's standard input. Awaited rather than aborted, so bytes the
    /// queue already accepted are not truncated halfway through a write.
    writer: JoinHandle<()>,
}

/// One pipe's owned work, handed to its supervisor.
struct PipeJob {
    /// The engine, for tearing a discarded pipe's job down.
    io: ShellIo,
    /// The pipe command's own job.
    shell: ShellHandle,
    /// Its receipt, which is where the publication verdict arrives.
    command: CommandHandle,
    /// The command text, for diagnostics.
    text: String,
    /// The job's standard input, closed by the supervisor for a real end of file.
    stdin: InputWriter,
    /// The `-O` forwarders, absent when the pane's output is not being piped.
    input: Option<PaneInput>,
    /// The standard output and standard error consumers, forwarding or draining.
    outputs: Vec<JoinHandle<()>>,
    /// Set when someone ends this pipe, to whichever of the two ends it is.
    stop_rx: watch::Receiver<Option<PipeStop>>,
    /// Where the close verdict is delivered.
    closed_tx: oneshot::Sender<Result<(), RmuxError>>,
}

/// Runs one pipe to its end and decides what its closer is told.
async fn supervise(job: PipeJob) {
    let PipeJob {
        io,
        shell,
        command,
        text,
        stdin,
        input,
        outputs,
        mut stop_rx,
        closed_tx,
    } = job;

    // Either someone ended the pipe or the command ended by itself. The verdict is read again
    // below either way: `CommandHandle::wait` answers every holder, including a late one.
    let stop = tokio::select! {
        biased;
        stop = wait_for_pipe_stop(&mut stop_rx) => Some(stop),
        _ = command.wait() => None,
    };

    // A torn-down pipe is cancelled before anything else, and before its input is ended. End of
    // file is precisely what lets `cat > log` finish and be gated, so delivering it first would
    // publish a log nobody asked for; the cancellation has to reach core admission while the
    // command is still running, which is what makes the work discarded rather than approved.
    if stop == Some(PipeStop::Discard) {
        let _ = io.stop(&shell, true).await;
    }

    // Stop feeding it before ending its input, and end that input for real. A logger blocked in
    // `read` needs the write end to go away; a Ctrl-D byte would just be more input to it.
    if let Some(input) = input {
        input.producer.abort();
        let _ = input.producer.await;
        match stop {
            // Nothing to flush into: the command is already cancelled, and the close grace is a
            // budget for finishing work that is going to be published.
            Some(PipeStop::Discard) => {
                input.writer.abort();
                let _ = input.writer.await;
            }
            _ => join_within_grace(input.writer).await,
        }
    }
    let _ = stdin.close().await;

    let outcome = match stop {
        // End of file is delivered. What remains is the command's own choice: finish and be
        // gated, or be signalled and then forced — and a forced command's verdict, not the force,
        // is what gets reported, because a force cannot undo an approval already sealed.
        Some(PipeStop::Close) => publication(
            command.finish_with_grace(PIPE_TERMINATION_GRACE).await,
            &text,
        ),
        Some(PipeStop::Discard) => discarded(command.wait().await, &text),
        None => publication(command.wait().await, &text),
    };
    // Nobody is waiting on this close, so a refusal is recorded instead of raised.
    if let Err(Err(error)) = closed_tx.send(outcome) {
        tracing::warn!("{error}");
    }

    // Drained after the verdict is delivered, not before: a `-I` forwarder writing into a busy
    // pane must not hold up the request that closed this pipe, and it must not be cut off either,
    // or a short pipe command loses its last bytes.
    //
    // These consumers stay independently spawned tasks, joined here rather than folded into the
    // `select!` above, and that is load-bearing rather than stylistic. A builtin runs inline on
    // its runtime task and reaches this collection over the mux's bounded queue, so the job's
    // pipe is only read while something is draining that queue. Polling these arms alongside the
    // stop signal would abandon them the moment the pipe ends, and the interpreter's next write
    // would then block a runtime worker on a queue nobody drains — permanently, one worker per
    // pipe. Spawned and joined, they keep draining for the whole life of the job, including
    // while this task is parked in `command.wait()` above.
    for task in outputs {
        let _ = task.await;
    }
    let _ = shell.wait_closed().await;
}

/// Turns one command's verdict into what its pipe's closer is told.
///
/// A zero exit code is not the interesting half. The staged writes of `pipe-pane -o 'cat >> log'`
/// reach the seed only on an approved boundary, so a denied, stale, discarded or infrastructure-
/// failed completion means the log does not exist and must be said so.
fn publication(
    result: Result<Arc<CommandCompletion>, WaitError>,
    text: &str,
) -> Result<(), RmuxError> {
    let completion = match result {
        Ok(completion) => completion,
        Err(error) => {
            return Err(RmuxError::Server(format!(
                "pipe-pane command '{text}' ended without a verdict: {error}"
            )))
        }
    };
    if completion.is_published() {
        return Ok(());
    }
    Err(RmuxError::Server(format!(
        "pipe-pane command '{text}' was not approved, so nothing it wrote exists:\n{}",
        managed_workload::completion_report(&completion).trim_end()
    )))
}

/// Turns a torn-down pipe's verdict into what a late holder of its receipt is told.
///
/// The cancellation above is what discards the log; this only reports it. A completion that still
/// says published means the gate had already approved that boundary before the cancellation
/// landed, which nothing can undo and which must not be described as a loss.
fn discarded(
    result: Result<Arc<CommandCompletion>, WaitError>,
    text: &str,
) -> Result<(), RmuxError> {
    if matches!(&result, Ok(completion) if completion.is_published()) {
        return Ok(());
    }
    Err(RmuxError::Server(format!(
        "pipe-pane command '{text}' was discarded with the pane it belonged to, so nothing it \
         wrote exists"
    )))
}

/// Awaits one forwarder for the termination grace, then stops waiting for it.
///
/// A pipe command that stopped reading its input leaves the writer blocked; the close cannot be
/// held open by that, and the bytes it was mid-way through were never going to be read.
async fn join_within_grace(mut task: JoinHandle<()>) {
    if tokio::time::timeout(PIPE_TERMINATION_GRACE, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

/// Waits for someone to end this pipe, and says which of the two ends it is.
async fn wait_for_pipe_stop(stop_rx: &mut watch::Receiver<Option<PipeStop>>) -> PipeStop {
    loop {
        if let Some(stop) = *stop_rx.borrow() {
            return stop;
        }
        if stop_rx.changed().await.is_err() {
            // Every sender is gone, so this pipe was dropped without being closed. Nobody is
            // waiting for the log and nobody asked for it, which is a teardown.
            return PipeStop::Discard;
        }
    }
}

/// Moves one pane's output into the bounded queue feeding a `-O` pipe.
async fn forward_pane_output_to_pipe(
    mut stop_rx: watch::Receiver<Option<PipeStop>>,
    mut pane_output: PaneOutputReceiver,
    pipe_tx: mpsc::Sender<Vec<u8>>,
) {
    loop {
        tokio::select! {
            biased;
            _ = wait_for_pipe_stop(&mut stop_rx) => break,
            next = pane_output.recv() => {
                match next {
                    OutputCursorItem::Event(event) => {
                        if stop_rx.borrow().is_some() {
                            break;
                        }
                        let bytes = event.into_bytes();
                        if bytes.is_empty() {
                            break;
                        }
                        if pipe_tx.send(bytes).await.is_err() {
                            break;
                        }
                    }
                    OutputCursorItem::Gap(_) => continue,
                }
            }
        }
    }
}

/// Writes queued pane bytes into a `-O` pipe's managed standard input.
///
/// Byte for byte: this is a pipe, and a pane's output is not text. The close at the end is the
/// end of file a command reading to exhaustion is waiting for, delivered as soon as the pane's
/// output is over rather than only when the supervisor notices.
async fn forward_pane_bytes_to_pipe(mut pipe_rx: mpsc::Receiver<Vec<u8>>, stdin: InputWriter) {
    while let Some(bytes) = pipe_rx.recv().await {
        if bytes.is_empty() || stdin.write_all(&bytes).await.is_err() {
            break;
        }
    }
    let _ = stdin.close().await;
}

/// Feeds a `pipe-pane -I` command's output back into the pane as input.
///
/// Written through the facade rather than a pseudoterminal master, because the pane's terminal
/// belongs to the engine now and it is the engine that orders input against whatever the shell is
/// already reading.
///
/// Standard output and standard error each get one of these, and they are independent: the engine
/// promises ordering within a stream and nothing at all between them, so nothing here merges the
/// two or claims an order for their bytes.
async fn forward_pipe_output_to_pane(
    mut stream: OutputStream,
    io: ShellIo,
    handle: ShellHandle,
    channel: &'static str,
) {
    loop {
        match stream.recv().await {
            Ok(Some(OutputCursorItem::Event(event))) => {
                let bytes = event.into_bytes();
                if bytes.is_empty() {
                    continue;
                }
                if io.write_input(&handle, &bytes).await.is_err() {
                    break;
                }
            }
            Ok(Some(OutputCursorItem::Gap(gap))) => {
                // An owner stream is lossless, so this is not expected. It is reported and
                // stepped over rather than papered over with bytes nothing produced.
                tracing::warn!("pipe-pane {channel} reported a gap: {gap:?}");
                continue;
            }
            Ok(None) | Err(_) => break,
        }
    }
}

/// Reads one of the command's streams to end of file with nowhere to put it.
///
/// Upstream pointed the child's descriptors at the null device. A managed job has real pipes, so
/// the bytes still have to be consumed; they just have nowhere to go.
async fn drain_pipe_output(mut stream: OutputStream) {
    while let Ok(Some(_)) = stream.recv().await {}
}
