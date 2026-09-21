//! What this daemon does with the shell engine's observations.
//!
//! [`crate::io::observation::consume`] is a single task draining one queue, and every rule it
//! follows is about not blocking: it never awaits a mux mutation, never takes a handler lock, and
//! acknowledges each output receipt as soon as the bytes are retained. The methods here are the
//! handler side of that contract, so each of them is either a short synchronous step or a
//! scheduling decision. None of them awaits, and none of them takes [`HandlerState`]'s mutex on
//! the consumer's thread — that mutex serializes every RPC in the daemon, and a byte pump is the
//! last thing that should be allowed to hold it.
//!
//! # Whose acknowledgement is whose
//!
//! The engine's output receipt is completed by the *consumer*, right after the chunk is retained
//! and shared. It is deliberately not the render's acknowledgement: publishing into a pane needs
//! [`HandlerState`], and making a shell's pump wait for the daemon's request mutex would let one
//! busy pane throttle every other pane's output and every control request. Per-stream ordering
//! survives anyway, because the engine produces one chunk at a time per stream and the owned tasks
//! scheduled here run in the order they were spawned on one runtime.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use marsh_core::shellmux::{
    CommandCompletion, JobEnd, MuxError, OutputChannel, Sandbox, ShellId,
};
use marsh_core::Outcome;
use rmux_core::LifecycleEvent;
use rmux_proto::{ProcessCommand, RmuxError, SessionName, WindowTarget};

use super::{prepare_lifecycle_event_if_enabled, RequestHandler, SelectionTransitionSnapshot};
use crate::io::{ShellHandle, ShellIo};
use crate::pane_io::PaneExitEvent;
use crate::pane_terminals::{HandlerState, NewWindowOptions, WindowSpawnOptions};

impl RequestHandler {
    /// Binds this daemon's shell facade, once, at the start of [`crate::listener::serve`].
    ///
    /// The first binding wins. A second call is a programming error — there is exactly one mux, one
    /// facade and one observation consumer per daemon — and replacing the installed facade would
    /// silently strand every handler that had already read the first one, including the idle check
    /// that keeps `exit-empty` from racing a headless consumer. Keeping the first binding makes
    /// that mistake loud and harmless instead of loud and wrong.
    /// Holds this daemon's pane-creation transaction, when a facade is installed.
    ///
    /// `None` when there is none: pane creation will then fail for the ordinary "no shell engine"
    /// reason, and taking a lock to protect a transaction that cannot happen would be ceremony.
    pub(crate) async fn pane_creation_transaction(
        &self,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        match self.shell_io() {
            Some(io) => Some(io.pane_creation_transaction().await),
            None => None,
        }
    }

    pub(crate) fn install_shell_io(&self, io: ShellIo) {
        let mut slot = self
            .shell_io
            .lock()
            .expect("shell facade mutex must not be poisoned");
        if slot.is_some() {
            drop(slot);
            tracing::error!("rmux shell facade is already installed; keeping the first binding");
            return;
        }
        // Handlers are not the owning library host, so they must never hold a native-client lease:
        // one taken here would keep the daemon from ever deciding it is idle.
        *slot = Some(io.unleased());
    }

    /// The installed shell facade, or `None` before the listener has bound one.
    ///
    /// Every clone is unleased. `None` is an ordinary answer in tests and in the window between
    /// constructing a handler and serving on it; it is not a failure.
    pub(crate) fn shell_io(&self) -> Option<ShellIo> {
        self.shell_io
            .lock()
            .expect("shell facade mutex must not be poisoned")
            .as_ref()
            .map(ShellIo::unleased)
    }

    /// Runs one owned follow-up task for an observation, on the daemon's runtime.
    ///
    /// The consumer's thread is the engine's; everything that needs the handler's state or an
    /// `await` belongs here instead. Preferring the captured server runtime over the ambient one
    /// keeps work originating on a status thread or a detached queue on the same executor as the
    /// rest of the daemon.
    fn spawn_shell_task<F>(&self, what: &'static str, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if let Some(runtime) = self.server_task_runtime() {
            runtime.spawn(task);
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(task);
            return;
        }
        tracing::warn!("dropping {what} because no Tokio runtime is available");
    }

    /// The job table or the selection moved on.
    ///
    /// The observation carries no state, so there is nothing to apply: the authoritative answer is
    /// a fresh read of the mux, and the surfaces that render job state are refreshed from it. That
    /// refresh needs the handler's state mutex and several awaits, so it is scheduled rather than
    /// performed here; coalescing two of these into one refresh is harmless, which is exactly why
    /// the engine is allowed to coalesce them in the first place.
    ///
    /// The engine's *selection* is carried on this same observation, because `table.current` only
    /// ever moves in [`ShellIo::switch`] and `switch` announces exactly this. So the mux-to-rmux
    /// half of selection synchronisation is applied here, before the refresh, so the refresh that
    /// follows already renders the selection it caused.
    pub(crate) fn note_shell_state_changed(&self) {
        let handler = self.clone();
        self.spawn_shell_task("shell state refresh", async move {
            handler.apply_shell_selection().await;
            handler.refresh_all_attached_sessions().await;
        });
    }

    /// A native `switch` chose a job; select the rmux pane that presents it.
    ///
    /// This is the mux-to-rmux half of plan line 347. Only a `switch` moves the engine's current
    /// job, so anything found here was chosen deliberately — by the pane prompt's `fg`, by a
    /// library consumer, or by this daemon's own opposite-direction push.
    ///
    /// That last case is why the claim is taken before anything else: the push stored the
    /// identity it was about to install, so its own echo arrives here, matches, and stops. The
    /// comparison is on the stable `(SnapshotUid, PaneId)` pair rather than a name or an index,
    /// because a respawn hands the same pane id to a different snapshot and a move hands the same
    /// index to a different pane — either of which would suppress a real change or fail to
    /// suppress an echo.
    ///
    /// The route is resolved through [`ShellIo::route_for`] every time. A cached index would
    /// address whatever now occupies the slot after a move, a link or a renumber; the stable id
    /// the route carries is re-resolved against the live session that currently owns it.
    ///
    /// A job with no pane — a hidden helper, a popup, a failed spawn — selects nothing. A popup
    /// keeps its owner's overlay and is never given a duplicate pane.
    ///
    /// No handler lock is held on entry, and the selection is applied through the ordinary
    /// `select-window`/`select-pane` handlers so hooks, control notifications and the affected
    /// sessions' refreshes are exactly the ones a user-issued selection produces. Only the
    /// mapped session's own selection moves; no client is switched to another session.
    pub(crate) async fn apply_shell_selection(&self) {
        let Some(io) = self.shell_io() else {
            return;
        };
        let Some(current) = io.current_job() else {
            return;
        };
        let Some((session, pane, generation)) = io.route_for(&current.sandbox) else {
            return;
        };
        let Some(claim) = io.claim_selection(&current.sandbox.uid, pane) else {
            return;
        };
        let resolved = {
            let state = self.state.lock().await;
            resolve_runtime_pane(&state, &session, pane, generation)
                .and_then(|runtime| state.pane_target_for_runtime_pane(&runtime, pane))
                .map(|target| {
                    let active_window = state
                        .sessions
                        .session(target.session_name())
                        .map(rmux_core::Session::active_window_index);
                    let window_moves = active_window != Some(target.window_index());
                    (target, window_moves)
                })
        };
        let Some((target, window_moves)) = resolved else {
            // The pane has not been committed yet, or it is retiring. Nothing was applied, so
            // the agreement is handed back and the next observation tries again rather than
            // treating an unapplied selection as settled.
            io.restore_selection(claim);
            return;
        };

        // Pane first, then the window, and the order is load-bearing. Selecting the window first
        // would commit that window's *current* active pane — a different job — and the nested
        // rmux-to-mux push would switch the engine to it before this one corrected it a moment
        // later. Selecting the pane first makes it the window's active pane without moving the
        // session, so the window commit that follows finds the identity already agreed and
        // suppresses its own push. Both nested pushes therefore see this claim and stop.
        //
        // `preserve_zoom`, because this is a selection and not a layout command: a native reader
        // bringing a job up must not silently unzoom the window the user zoomed.
        let response = self
            .handle_select_pane(rmux_proto::SelectPaneRequest {
                target: target.clone(),
                title: None,
                input_disabled: None,
                preserve_zoom: true,
                style: None,
            })
            .await;
        if let rmux_proto::Response::Error(error) = response {
            tracing::debug!(
                shell = current.id.as_str(),
                "selected shell job's pane could not be selected: {}",
                error.error
            );
            return;
        }

        if !window_moves {
            return;
        }
        let response = self
            .handle_select_window(
                None,
                rmux_proto::SelectWindowRequest {
                    target: rmux_proto::WindowTarget::with_window(
                        target.session_name().clone(),
                        target.window_index(),
                    ),
                },
            )
            .await;
        if let rmux_proto::Response::Error(error) = response {
            tracing::debug!(
                shell = current.id.as_str(),
                "selected shell job's window could not be selected: {}",
                error.error
            );
        }
    }

    /// An rmux selection committed; make the selected pane's job the engine's current terminal.
    ///
    /// This is the rmux-to-mux half of plan line 347, and every commit that changes which pane a
    /// session shows calls it: `select-pane` by target and by id, `select-pane` by direction,
    /// `last-pane`, the attached mouse focus commit, `select-window`, and `switch-client`. The
    /// last explicit selection is what the singleton calls its current terminal.
    ///
    /// Called with the handler's state mutex released. The lock taken here is a short read that
    /// resolves the pane's live job, and it is released again before [`ShellIo::switch`] — which
    /// awaits the engine's own admission and must never be awaited under the daemon's request
    /// mutex.
    ///
    /// The claim is taken before the switch so the `Changed` the switch announces already finds
    /// this identity agreed and stops there. A refused switch — a pane whose job is closing, a
    /// host that has shut down — hands the claim back, because nothing was propagated and the
    /// next selection of that same pane must not be mistaken for an echo.
    ///
    /// A pane whose job has already exited is deliberately skipped: `remain-on-exit` leaves it on
    /// screen and selectable, and making a dead shell the engine's current terminal would name a
    /// job no native caller can do anything with.
    pub(in crate::handler) async fn select_shell_for_pane_target(
        &self,
        target: &rmux_proto::PaneTarget,
    ) {
        self.select_shell_for_selection(
            target.session_name(),
            Some((target.window_index(), target.pane_index())),
        )
        .await;
    }

    /// The same, for a commit that named a session or a window rather than a pane.
    ///
    /// `select-window` and `switch-client` settle on a window or a session and leave the pane to
    /// whatever that window already calls active, so the pane is read back here rather than
    /// guessed by the caller.
    pub(in crate::handler) async fn select_shell_for_active_pane(
        &self,
        session: &rmux_proto::SessionName,
    ) {
        self.select_shell_for_selection(session, None).await;
    }

    /// Resolves the selected pane's live job and makes it the engine's current terminal.
    ///
    /// `exact` is the window and pane a command named; `None` asks for the session's active
    /// window and that window's active pane. Reading the active pane back for an exact commit
    /// would answer for the wrong window — `select-pane -t other:1.2` does not move the session's
    /// active window — and demanding one from a window-level commit would make the caller guess.
    async fn select_shell_for_selection(
        &self,
        session: &rmux_proto::SessionName,
        exact: Option<(u32, u32)>,
    ) {
        let resolved = {
            let state = self.state.lock().await;
            let Some(live) = state.sessions.session(session) else {
                return;
            };
            let window_index = match exact {
                Some((window_index, _)) => window_index,
                None => live.active_window_index(),
            };
            let Some(window) = live.window_at(window_index) else {
                return;
            };
            let pane_index = match exact {
                Some((_, pane_index)) => pane_index,
                None => window.active_pane_index(),
            };
            let Some(pane_id) = window.pane(pane_index).map(rmux_core::Pane::id) else {
                return;
            };
            state
                .pane_shell_if_alive(session, window_index, pane_index)
                .ok()
                .map(|(io, handle)| (io, handle, pane_id))
        };
        let Some((io, handle, pane_id)) = resolved else {
            return;
        };
        let Some(claim) = io.claim_selection(&handle.sandbox().uid, pane_id) else {
            return;
        };
        if let Err(error) = io.switch(&handle).await {
            io.restore_selection(claim);
            tracing::debug!(
                shell = handle.id().as_str(),
                "selected pane's job refused to become the engine's current terminal: {error}"
            );
        }
    }

    /// Bytes from a job's terminal stream, for the pane that presents it.
    ///
    /// Only jobs with a mapped pane reach a transcript. A hidden pipe helper, a popup-only surface
    /// and a failed spawn have no pane by construction, and inventing one for them would put a
    /// workload's data on a screen nobody asked to see it on.
    ///
    /// Performed inline rather than scheduled, and that is the whole ordering guarantee. Two
    /// chunks handed to two spawned tasks acquire the state mutex in whatever order the runtime
    /// wakes them, which is not the order the engine produced them in; awaiting here means the
    /// caller's per-stream delivery order *is* the pane's order. The engine's receipt is completed
    /// after this returns, so a pane that cannot keep up applies backpressure to its own stream's
    /// pump and to nothing else.
    ///
    /// The parser's replies — a device-attribute answer, a palette response — go back through the
    /// managed input path, and only while the generation that asked is still running a command.
    /// A reply written to an idle job reaches the prompt reader, which would consume it as
    /// something the user typed; a closing job and a reused name are the same mistake, later.
    pub(crate) async fn apply_shell_output(
        &self,
        shell: &Sandbox,
        event: &rmux_core::events::OutputEvent,
    ) {
        // An empty chunk is this daemon's end-of-file marker on a pane's ring. Ordinary output
        // never is one, so publishing it would fabricate an exit boundary mid-stream.
        if event.bytes().is_empty() {
            return;
        }
        let Some(io) = self.shell_io() else {
            return;
        };
        let Some((session, pane, generation)) = io.route_for(shell) else {
            return;
        };
        let target = {
            let state = self.state.lock().await;
            resolve_runtime_pane(&state, &session, pane, generation)
                .and_then(|runtime| state.runtime_pane_transcript_and_output(&runtime, pane))
        };
        let Some((transcript, output)) = target else {
            return;
        };
        let alerts = self.pane_alert_callback();
        let replies = crate::pane_io::publish_shell_pane_bytes(
            &session,
            pane,
            &transcript,
            &output,
            Some(&alerts),
            event.bytes().to_vec(),
        );
        if replies.is_empty() {
            return;
        }
        // A terminal reply belongs to the program that asked the question, and only while it is
        // still asking. Delivering one to an idle job would hand it to the prompt reader, which
        // cannot tell a device-attribute answer from something the user typed — the pane would
        // execute `[?1;2c`. A closing job is the same case one step further along, and a
        // generation whose name was reused is a different pane entirely. Each of those is
        // discarded rather than written; only genuine user input reaches an idle prompt.
        let Some(view) = io.job(&shell.id) else {
            return;
        };
        if view.sandbox.uid != shell.uid || view.closing || view.running.is_none() {
            return;
        }
        let Ok(job) = io.shell(&shell.id) else {
            return;
        };
        if job.sandbox().uid != shell.uid {
            return;
        }
        if let Err(error) = io.write_input(&job, &replies).await {
            tracing::debug!(
                shell = shell.id.as_str(),
                "dropping terminal replies for a job that no longer accepts input: {error}"
            );
        }
    }

    /// Renders a prompt's late verdict into its pane, at the job's close barrier.
    ///
    /// Same publisher as ordinary output, deliberately: the bytes have to reach the transcript the
    /// same way the command's own output did, or they would appear in a different order or not at
    /// all. It is called by the job's retirement after every delivery worker has acknowledged its
    /// end and before the route is forgotten, which is the only window where the report is both
    /// ordered behind the job's last output and still addressable.
    ///
    /// A prompt cannot publish this itself: `wait_closed()` resolves when the snapshot is
    /// reclaimed, which is earlier than both of those, so a direct publish races the job's own
    /// end of file and may land after the surface it names is gone.
    ///
    /// The terminal channel only. A pipe job has no surface to report onto and no prompt to owe a
    /// report in the first place.
    pub(crate) async fn apply_final_report(&self, shell: &Sandbox, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let event = rmux_core::events::OutputEvent::from_shared(0, Arc::from(bytes), Vec::new());
        self.apply_shell_output(shell, &event).await;
    }

    /// A command ended and the gate decided.
    ///
    /// This is not a job boundary and not a pane exit: an interactive shell runs the next line in
    /// the same snapshot. What is worth acting on here is a verdict that refused publication,
    /// because that is the case a zero exit status hides — the program ran, printed, exited zero,
    /// and changed nothing in the seed. It goes to the operator diagnostic log with the status it
    /// exited with, so the two can be read together.
    ///
    /// Two surfaces consume the verdict directly rather than through this log, and both arrive
    /// with their own slices: the interactive shell prompt renders it through `repl::report_lines`
    /// into the job's own terminal, and `display-popup -E`/`-EE` decides its close policy from the
    /// completed command's status in `handler_overlay/popup_job.rs`.
    pub(crate) fn note_shell_command_finished(&self, completion: &Arc<CommandCompletion>) {
        if completion.is_published() {
            return;
        }
        crate::diagnostic_log::record_shell_command_unapproved(
            completion.shell.id.as_str(),
            completion.shell.uid.as_str(),
            &completion.command,
            verdict_label(completion.outcome.as_ref()),
        );
        tracing::info!(
            shell = completion.shell.id.as_str(),
            verdict = verdict_label(completion.outcome.as_ref()),
            exit_code = completion.exit_code,
            "shell command completed without publication"
        );
    }

    /// A job's streams are over and its snapshot is reclaimed.
    ///
    /// For a job with a mapped pane this is the pane's exit. The status handed to the existing
    /// pane-exit pipeline is the *gated job* status: a known nonzero process code is preserved,
    /// `0` means the line was published, and `1` covers a refusal, a discard and an infrastructure
    /// failure. That is what lets an unchanged rmux client fail a denied one-shot workload without
    /// a new wire variant, while native callers keep the real exit code and the full outcome.
    ///
    /// Seeding the status before dispatching matters: an embedded shell has no OS child, so the
    /// pipeline's own exit observation would find nothing and give up.
    ///
    /// Awaited on the same per-stream delivery queue the job's chunks arrive on, which is what
    /// keeps the end-of-file marker behind the last byte. Scheduling it independently would let a
    /// reader see the boundary before output the program had already written.
    pub(crate) async fn apply_shell_closed(&self, end: &Arc<JobEnd>) {
        let Some(io) = self.shell_io() else {
            return;
        };
        let Some((session, pane, generation)) = io.route_for(&end.shell) else {
            return;
        };
        let status = gated_pane_status(end);
        let (runtime, output) = {
            let mut state = self.state.lock().await;
            let Some(runtime) = resolve_runtime_pane(&state, &session, pane, generation) else {
                return;
            };
            state.mark_runtime_pane_dead_with_status(&runtime, pane, status);
            let output = state
                .runtime_pane_transcript_and_output(&runtime, pane)
                .map(|(_, output)| output);
            (runtime, output)
        };
        if let Some(output) = output {
            let _ = output.send_for_generation(Some(generation), Vec::new());
        }
        self.handle_pane_exit_event(PaneExitEvent::eof_published(
            runtime,
            pane,
            Some(generation),
        ))
        .await;
    }

    /// One of a job's output streams failed for a reason that is not end of file.
    ///
    /// That stream is over; the job is not. This is a diagnostic about this daemon's own plumbing
    /// and is deliberately never turned into a pane exit: doing so would report a status no
    /// program produced and tear down a shell that is still running.
    pub(crate) fn note_shell_stream_error(
        &self,
        shell: &Sandbox,
        channel: OutputChannel,
        error: &Arc<std::io::Error>,
    ) {
        let channel = channel_label(channel);
        crate::diagnostic_log::record_shell_stream_error(
            shell.id.as_str(),
            shell.uid.as_str(),
            channel,
            &error.to_string(),
        );
        tracing::warn!(
            shell = shell.id.as_str(),
            channel,
            "shell output stream failed: {error}"
        );
    }

    /// A terminal job appeared that rmux did not create.
    ///
    /// Something opened it through the native API, so it has a real principal and a real snapshot
    /// but no surface. It gets one here: a detached window in an existing session, named after the
    /// shell id, presenting the job that already exists.
    ///
    /// The caller has *released* the creation-or-adoption lock and left a
    /// [`Route::Adopting`](crate::io::Route::Adopting) reservation in its place, which is what
    /// makes taking handler state here safe: ordinary pane creation takes handler state first and
    /// admission second, so an adoption that still held admission while waiting for state would
    /// invert the two. The reservation carries the same guarantee the lock did — nothing else will
    /// adopt or route this job — without the ordering hazard.
    ///
    /// Exactly one of two things replaces that reservation before this returns: a real
    /// [`Route::Pane`](crate::io::Route::Pane), or [`Route::Hidden`](crate::io::Route::Hidden)
    /// when no window could be given to it. Leaving `Adopting` behind would make the job
    /// permanently neither adopted nor adoptable.
    ///
    /// The job is not restarted, not re-environed and not selected. It is not given a prompt
    /// reader either: whatever opened it owns its input, and a second reader on the same terminal
    /// would race the first for every keystroke.
    pub(crate) async fn adopt_external_shell(&self, io: &ShellIo, job: &ShellHandle) {
        let socket_path = self.socket_path();
        let adopted = {
            let mut state = self.state.lock().await;
            state.adopt_external_job_window(io, job, &socket_path)
        };
        let adopted = match adopted {
            Ok(adopted) => adopted,
            Err(error) => {
                io.install_route(job.sandbox().uid.clone(), crate::io::Route::Hidden);
                tracing::warn!(
                    shell = job.id().as_str(),
                    "externally created shell job could not be adopted as a window; routed \
                     hidden: {error}"
                );
                return;
            }
        };
        io.install_route(
            job.sandbox().uid.clone(),
            crate::io::Route::Pane {
                session: adopted.session_name.clone(),
                pane: adopted.pane_id,
                generation: adopted.generation,
            },
        );
        tracing::info!(
            shell = job.id().as_str(),
            session = adopted.session_name.as_str(),
            pane = adopted.pane_id.as_u32(),
            "adopted an externally created shell job as a detached window"
        );
        self.refresh_attached_session(&adopted.session_name).await;
    }

    /// Opens a window for a job the shell prompt asked for, beside the prompt that asked.
    ///
    /// `sd NAME DIR` and a trailing `&` create a job, and a job with no surface is one the user
    /// cannot see, select or type into. This runs the same three-step window creation
    /// `new-window` runs — plan under the state lock, open the job with that lock released,
    /// commit against the pane identity the plan reserved — so a prompt-created window gets
    /// rmux's ordinary profile, environment, layout and naming rather than a second, thinner
    /// creation path beside it.
    ///
    /// It exists as a method on the handler because a prompt task holds a [`ShellIo`] and a
    /// [`ShellHandle`] and nothing else; every entry into that transaction is a
    /// [`HandlerState`] method, and [`ShellIo::handler`] is the one way back to the state that
    /// owns them.
    ///
    /// Five things are deliberately not what `new-window` does:
    ///
    /// * **The session is resolved from the prompt's own job**, not named by a client, because
    ///   there is no client: the new window belongs beside the window the line was typed in. The
    ///   route's pane is resolved to the runtime session that owns it *now*, so a prompt whose
    ///   pane has since been moved or linked still opens beside itself.
    /// * **The window is detached**, and nothing here selects it or calls
    ///   [`ShellIo::switch`](crate::io::ShellIo::switch). `sd` and `&` add a job; they do not
    ///   move the user to it. `fg` is the line that does.
    /// * **`follow_mux_lifetime` is `true`**, so the command reaches `spawn` rather than being
    ///   reserved afterwards with `close_on_finish`. That is what preserves the engine's
    ///   anonymous-job rules the user asked for: an unnamed `&` closes itself when its command
    ///   ends, and `keep` can still cancel that closure. A one-shot reservation would take
    ///   `keep` away.
    /// * **`default-command` is not applied.** The prompt's own grammar already says what to
    ///   run: `&` carries a command and `sd` carries none. Substituting rmux's configured
    ///   command would run something nobody typed, and for an unnamed `sd` it would turn an idle
    ///   prompt job into a self-closing one-shot.
    /// * **No `after-new-window` hook is queued.** Inline hooks are drained by the request
    ///   dispatch that queued them, and a line typed at a prompt is not a request; queueing one
    ///   here would attribute it to whichever client command happened to run next.
    ///
    /// # Errors
    ///
    /// Fails when this server has no seed, when the directory names nothing in it, when the
    /// prompt's own pane no longer has a session to open beside, and for every reason planning,
    /// opening or committing a window fails. A window whose job never opened is rolled back, so
    /// a failure leaves no empty window on screen.
    pub(crate) async fn spawn_repl_window(
        &self,
        io: &ShellIo,
        prompt: &ShellHandle,
        shell_id: Option<ShellId>,
        dir: &str,
        cmd: Option<&str>,
    ) -> Result<ShellId, RmuxError> {
        let start_directory = repl_start_directory(io, dir)?;
        let command = cmd.map(|cmd| ProcessCommand::Shell(cmd.to_owned()));
        // A named job's name is its identity, so the window wears it and stops renaming itself
        // after whatever it happens to be running. An unnamed `&` has no name yet — the engine
        // allocates one during the spawn, after the plan — so it keeps rmux's automatic naming.
        let window_name = shell_id.as_ref().map(|id| id.as_str().to_owned());
        let socket_path = self.socket_path();
        // One sentence for the one thing that can be wrong here: the prompt has outlived the
        // window it was typed in, so there is nothing for a new window to appear beside.
        let orphaned = || {
            RmuxError::Server(format!(
                "{}: this job has no window to open another beside",
                prompt.id().reference()
            ))
        };
        let Some((route_session, pane_id, generation)) = io.route_for(prompt.sandbox()) else {
            return Err(orphaned());
        };

        // The job is opened between the two locked phases below, with the request mutex released,
        // for the reason `handle_new_window` states: opening one awaits a shell build, a snapshot
        // creation and the facade's admission lock, and awaiting any of those under the daemon's
        // request mutex stalls every other session and inverts against the adoption path.
        let creation = io.pane_creation_transaction().await;
        let (session_name, planned, timer_mutation, selection_before) = {
            let mut state = self.state.lock().await;
            let session_name = prompt_window_session(&state, &route_session, pane_id, generation)
                .ok_or_else(orphaned)?;
            let timer_sessions = state.sessions.session_group_members(&session_name);
            let timer_mutation =
                self.plan_window_mutation_silence_timers_locked(&state, timer_sessions);
            let selection_before = SelectionTransitionSnapshot::capture(&state);
            let planned = state.plan_window(
                &session_name,
                NewWindowOptions {
                    name: window_name,
                    detached: true,
                    spawn: WindowSpawnOptions {
                        start_directory: Some(&start_directory),
                        inherited_start_directory: false,
                        command: command.as_ref(),
                        socket_path: &socket_path,
                        spawn_environment: None,
                        environment_overrides: None,
                        respawn_shell: None,
                        respawn_environment: None,
                        shell_id,
                        follow_mux_lifetime: true,
                    },
                },
            )?;
            (session_name, planned, timer_mutation, selection_before)
        };
        let opened = planned.open().await;

        let (opened_id, lifecycle_events) = {
            let mut state = self.state.lock().await;
            let (commit, terminal_commit, prepared) = match opened {
                Ok(opened) => opened,
                Err((commit, error)) => {
                    state.roll_back_planned_window(commit);
                    return Err(error);
                }
            };
            let opened_id = prepared.shell_id().clone();
            let response = state.commit_planned_window(commit, prepared, terminal_commit)?;
            let mut timer_targets = Vec::new();
            for timer_session_name in state.sessions.session_group_members(&session_name) {
                let Some(session) = state.sessions.session(&timer_session_name) else {
                    continue;
                };
                timer_targets.extend(session.windows().keys().copied().map(|window_index| {
                    WindowTarget::with_window(timer_session_name.clone(), window_index)
                }));
            }
            self.apply_window_mutation_silence_timers_locked(
                &state,
                timer_mutation,
                Vec::new(),
                &[],
                timer_targets,
            );
            // Detached creation cannot move a session's active window, so this normally prepares
            // nothing. It stays because it is the check, not the effect: if a group or link
            // synchronization did move one, the clients watching have to be told.
            let mut lifecycle_events = selection_before
                .prepare_session_window_changes(&mut state, std::slice::from_ref(&session_name));
            lifecycle_events.extend(prepare_lifecycle_event_if_enabled(
                &mut state,
                &LifecycleEvent::WindowLinked {
                    session_name: session_name.clone(),
                    target: Some(response.target),
                },
            ));
            (opened_id, lifecycle_events)
        };

        // The transaction ends with the commit. What follows emits lifecycle events and refreshes
        // attached clients, and either can run a user hook that itself creates a pane — which would
        // take this same non-reentrant mutex while its own call stack still holds the guard. That
        // deadlocks with every worker idle, which is exactly how it presents.
        drop(creation);

        for event in lifecycle_events {
            self.emit_prepared(event).await;
        }
        self.refresh_attached_session(&session_name).await;
        Ok(opened_id)
    }
}

/// The runtime session that currently owns `pane`, starting from the session its route named.
///
/// Routes record a stable pane id and the session the pane was created in; linking, moving and
/// renaming windows can change which runtime session owns that pane's transcript afterwards.
///
/// `generation` is the output generation the route was installed with, and it is the rejection
/// test a job's own identity cannot provide. A respawn reserves the next generation, opens a
/// replacement job and leaves the old one closing: for the moments both exist, the retiring job's
/// route still names the pane, and only the generation tells the two apart. Bytes and closures
/// from the older number are refused rather than published into the pane that replaced them.
fn resolve_runtime_pane(
    state: &HandlerState,
    session: &rmux_proto::SessionName,
    pane: rmux_core::PaneId,
    generation: u64,
) -> Option<rmux_proto::SessionName> {
    state.resolve_pane_event_runtime_session(session, pane, Some(generation))
}

/// The visible session a prompt's own pane is in right now.
///
/// Two steps, and neither is skippable. The route records the session the pane was *created* in
/// and the generation it was created at, so [`resolve_runtime_pane`] answers which runtime
/// session owns that pane today — a pane that has since been moved, linked or respawned is
/// resolved against its own generation rather than against a name that has moved on. A runtime
/// session is not an addressable one, though: window creation is applied to the session a user
/// names, so the runtime answer is turned back into the visible session that presents the pane.
///
/// `None` when the pane no longer exists anywhere, which is a prompt outliving its own window.
fn prompt_window_session(
    state: &HandlerState,
    route_session: &SessionName,
    pane: rmux_core::PaneId,
    generation: u64,
) -> Option<SessionName> {
    let runtime = resolve_runtime_pane(state, route_session, pane, generation)?;
    state
        .pane_target_for_runtime_pane(&runtime, pane)
        .map(|target| target.session_name().clone())
}

/// The host path a prompt's seed-relative directory names, for the profile to be resolved over.
///
/// The prompt speaks in seed-relative directories — that is what `repl::job_dir` produces and
/// what [`ShellIo::spawn`](crate::io::ShellIo::spawn) consumes — while the window transaction
/// resolves a profile over a host path and converts it back with
/// [`seed_relative_path`](crate::terminal::seed_relative_path). Joining onto the seed is the
/// exact inverse of that conversion, so the job still opens over the directory that was typed.
/// A leading `/` is the prompt's spelling of the seed root, never the filesystem's, so it is
/// stripped rather than allowed to replace the seed.
///
/// The existence check is here because the profile's own directory resolution is built to *fall
/// back*: an inherited directory that does not exist quietly becomes the home directory, which
/// is right for a pane nobody gave a directory to and wrong for one the user named. Without this
/// the diagnostic for `sd api nope` would name the fallback instead of `nope`.
///
/// # Errors
///
/// Fails when this server has no seed, and when the directory names nothing inside it.
fn repl_start_directory(io: &ShellIo, dir: &str) -> Result<PathBuf, RmuxError> {
    let Some(seed) = io.executor_info().seed else {
        return Err(RmuxError::Server(
            "this server has no seed to open a job directory under".to_owned(),
        ));
    };
    let requested = seed.join(dir.trim_start_matches('/'));
    if !requested.is_dir() {
        return Err(RmuxError::Server(format!(
            "{dir}: no such directory in the seed"
        )));
    }
    Ok(requested)
}

/// The status an unchanged rmux client sees for a closed shell-backed pane.
///
/// Four distinct things collapse into one integer here, which is the whole reason this is written
/// down rather than inferred at each call site:
///
/// * an infrastructure failure that prevented any verdict is `1`; it is not an exit status, and
///   there is no honest code for "the machinery broke";
/// * a job that closed without ever running a command is `0` — nothing was refused;
/// * a known nonzero process status is preserved as itself, because that is what the program said;
/// * otherwise the *gate* answers: `0` when the line was published, `1` when it was denied, stale,
///   discarded, detached or failed.
///
/// A zero exit with a refused publication therefore reports `1`. That is the point: the workload
/// changed nothing, and a client that only reads the status must not be told it succeeded. Native
/// callers keep [`CommandCompletion::exit_code`] and the full outcome and lose nothing.
fn gated_pane_status(end: &JobEnd) -> Option<i32> {
    if end.error.is_some() {
        return Some(1);
    }
    let Some(completion) = end.completion.as_ref() else {
        return Some(0);
    };
    if let Some(code) = completion.exit_code {
        if code != 0 {
            return Some(code);
        }
    }
    if completion.is_published() {
        Some(0)
    } else {
        Some(1)
    }
}

/// A stable one-word name for a verdict, for logs that are read by people.
///
/// The five outcomes stay distinct: flattening "refused", "someone else won the path" and "thrown
/// away unchecked" into one word would lose exactly the distinction an operator is looking for.
const fn verdict_label(outcome: &Result<Outcome, MuxError>) -> &'static str {
    match outcome {
        Ok(Outcome::Published { .. }) => "published",
        Ok(Outcome::Denied { .. }) => "denied",
        Ok(Outcome::Stale { .. }) => "stale",
        Ok(Outcome::Discarded) => "discarded",
        Ok(Outcome::Detached) => "detached",
        Err(_) => "failed",
    }
}

/// A stable one-word name for an output stream, for the same logs.
const fn channel_label(channel: OutputChannel) -> &'static str {
    match channel {
        OutputChannel::Terminal => "terminal",
        OutputChannel::Stdout => "stdout",
        OutputChannel::Stderr => "stderr",
    }
}
