use rmux_core::PaneId;
use rmux_proto::{RmuxError, SessionName};

use crate::pane_io::PaneExitEvent;
use crate::pane_terminal_lookup::initial_pane;
use crate::pane_terminal_process::{
    open_pane_terminal, pane_terminal_size, PaneRoute, PaneTerminal, PaneTerminalRequest,
};

use super::lifecycle_state::terminal_size_from_geometry;
use super::{
    pane_terminal_geometry_for_session, session_not_found, CompletedDeferredInitialPane,
    DeferredInitialPaneCommit, DeferredInitialPaneConsoleInputAction, DeferredInitialPaneIdentity,
    DeferredInitialPaneInput, DeferredInitialPaneInputDrain, DeferredInitialPaneInputFlush,
    DeferredInitialPaneSpawn, HandlerState, InitialPaneSpawnOptions, PaneExitMetadata,
    PaneLifecycleSpawn, PasteDelimiters, StartingPane, WindowNameApplication,
};

const STARTING_PANE_INPUT_MAX_BYTES: usize = 64 * 1024;

impl DeferredInitialPaneSpawn {
    /// Opens the deferred pane's job. The handler's state lock must **not** be held.
    ///
    /// The middle phase of the same three-phase transaction every other pane creation runs.
    /// Opening a job awaits a shell build, a snapshot creation and the facade's admission lock,
    /// and awaiting any of those under the daemon's request mutex would stall every other session
    /// and invert against the observation consumer's adoption path, which takes admission first
    /// and handler state second.
    ///
    /// The commit half comes back from both arms, which is what makes this signature differ from
    /// its siblings. Every other path rolls a half-created window or session back when the open
    /// fails; this pane is already on screen, so the failure has to be delivered *into* it, and
    /// that needs the identity the plan was made with.
    pub(crate) async fn open(
        self,
    ) -> (DeferredInitialPaneCommit, Result<PaneTerminal, RmuxError>) {
        let Self {
            request,
            io,
            commit,
        } = self;
        let opened = open_pane_terminal(&io, request).await;
        (commit, opened)
    }
}

impl DeferredInitialPaneCommit {
    /// The session a failed or superseded deferred pane is reported against.
    pub(crate) const fn visible_session_name(&self) -> &SessionName {
        &self.visible_session_name
    }
}

impl HandlerState {
    /// Plans a new session's first pane on the deferred path, and installs its surface.
    ///
    /// Unlike every other plan, this one mutates: the window is named, the pane's transcript and
    /// output channel are created, and the pane is recorded as `Starting` — all before the job
    /// exists. That is what deferring *is*. `new-session` answers immediately, the pane is
    /// visible while its shell is still being built, and input typed at it meanwhile is queued
    /// against the [`StartingPane`] entry created here rather than lost.
    ///
    /// The output generation is reserved here too, by [`Self::insert_pending_pane_output`], and
    /// travels into the job's route — so a chunk that arrives before the commit names a
    /// generation this pane already owns, and one from a pane replaced meanwhile does not.
    ///
    /// # Errors
    ///
    /// Fails when the session or its initial pane is missing, when the profile's environment or
    /// directory cannot be resolved, when no shell facade is bound, and when the surface cannot
    /// be installed. Everything that can fail without leaving state behind is done first.
    pub(crate) fn prepare_deferred_initial_session_terminal(
        &mut self,
        session_name: &SessionName,
        spawn: InitialPaneSpawnOptions<'_>,
    ) -> Result<DeferredInitialPaneSpawn, RmuxError> {
        let pane = initial_pane(&self.sessions, session_name)?;
        let runtime_session_name =
            self.runtime_session_name_for_window(session_name, pane.window_index);
        let (session_id, window_id, requested_cwd, pane_geometry) = {
            let session = self
                .sessions
                .session(session_name)
                .ok_or_else(|| session_not_found(session_name))?;
            let window = session.window_at(pane.window_index).ok_or_else(|| {
                RmuxError::invalid_target(
                    format!("{session_name}:{}", pane.window_index),
                    "window index does not exist in session",
                )
            })?;
            (
                session.id(),
                window.id(),
                session.cwd(),
                pane_terminal_geometry_for_session(
                    session,
                    &self.options,
                    pane.window_index,
                    pane.index,
                    pane.geometry,
                    false,
                    false,
                ),
            )
        };
        let profile = crate::terminal::TerminalProfile::for_initial_session_pane(
            &self.environment,
            &self.options,
            session_name,
            session_id.as_u32(),
            spawn.socket_path,
            spawn.spawn_environment,
            spawn.raw_spawn_environment,
            true,
            spawn.environment_overrides,
            Some(pane.id),
            requested_cwd,
        )?;
        crate::terminal::validate_windows_process_command_for_profile(&profile, spawn.command)?;
        let runtime_window_name = profile.runtime_window_name(spawn.command);
        let initial_window_name = if crate::automatic_rename::automatic_rename_enabled(
            &self.options,
            session_name,
            pane.window_index,
        ) {
            profile.automatic_window_name(spawn.command)
        } else {
            runtime_window_name.clone()
        };
        let initial_title = profile.initial_pane_title();
        let lifecycle_cwd = profile.cwd().to_path_buf();
        let respawn_shell = profile.pane_shell().clone();
        // Resolved before anything is installed: a daemon with no facade bound must fail with no
        // window renamed, no transcript created and no pane left starting forever.
        let io = self.require_shell_io()?;
        self.apply_window_name(
            session_name,
            pane.window_index,
            initial_window_name,
            WindowNameApplication::Initial,
        )?;
        self.terminals.insert_pending_session(
            runtime_session_name.clone(),
            crate::terminal::SessionBaseEnvironment::from_profile(&profile),
        )?;
        self.record_pane_lifecycle_starting(PaneLifecycleSpawn {
            session_id,
            window_id,
            pane_id: pane.id,
            process_command: spawn.command.cloned(),
            working_directory: Some(lifecycle_cwd),
            respawn_shell,
            private_environment: spawn.environment_overrides.map(<[String]>::to_vec),
            respawn_environment: None,
            dimensions: terminal_size_from_geometry(pane.geometry),
            pid: None,
        });
        let generation = self.insert_pending_pane_output(
            &runtime_session_name,
            pane.id,
            pane_geometry,
            initial_title,
        )?;
        self.starting_panes
            .entry(runtime_session_name.clone())
            .or_default()
            .insert(
                pane.id,
                StartingPane {
                    profile: profile.clone(),
                    runtime_window_name: runtime_window_name.clone(),
                    generation,
                    queued_input: Default::default(),
                    queued_input_bytes: 0,
                },
            );

        Ok(DeferredInitialPaneSpawn {
            request: PaneTerminalRequest {
                geometry: pane_geometry,
                profile,
                runtime_window_name,
                command: spawn.command.cloned(),
                route: PaneRoute {
                    session: session_name.clone(),
                    pane: pane.id,
                    generation,
                },
                shell_id: None,
                follow_mux_lifetime: false,
            },
            io,
            commit: DeferredInitialPaneCommit {
                runtime_session_name,
                visible_session_name: session_name.clone(),
                identity: DeferredInitialPaneIdentity::new(pane.id, generation),
            },
        })
    }

    /// Installs a deferred pane's job into the surface that has been waiting for it.
    ///
    /// The identity check is why this is separate from the open. The job was opened with the
    /// request mutex released — for longer here than on any other path, because deferring is the
    /// whole point — so the pane may since have been killed, moved into another session, or
    /// respawned by someone else. Two facts have to still hold: the pane must still resolve in
    /// this runtime session, and the generation reserved for it must still be the current one. A
    /// job failing either is stopped rather than installed, because nothing will ever present it.
    ///
    /// `Ok(None)` is the pane having quietly gone away — a killed session, a superseded start —
    /// which is not an error anyone asked about and produces no message.
    ///
    /// # Errors
    ///
    /// Fails when the pane was superseded while its job was opening, when the window or pane is
    /// missing from the session it resolved into, when the starting-pane record has gone, and
    /// when the terminal or output store refuses the insertion.
    pub(crate) fn commit_deferred_initial_pane(
        &mut self,
        commit: &DeferredInitialPaneCommit,
        mut terminal: PaneTerminal,
    ) -> Result<Option<CompletedDeferredInitialPane>, RmuxError> {
        let identity = commit.identity;
        let pane_id = identity.pane_id();
        let Some(runtime_session_name) =
            self.starting_runtime_session_for_identity(&commit.runtime_session_name, identity)
        else {
            terminal.terminate_in_background();
            return Ok(None);
        };
        let Some(target) = self.pane_target_for_runtime_pane(&runtime_session_name, pane_id) else {
            let _ = self.remove_starting_pane_if_identity(&runtime_session_name, identity);
            terminal.terminate_in_background();
            return Ok(None);
        };
        if let Err(error) =
            self.check_pane_commit_identity(&runtime_session_name, pane_id, identity.generation())
        {
            terminal.terminate_in_background();
            return Err(error);
        }
        let pane_geometry = {
            let session = self
                .sessions
                .session(target.session_name())
                .ok_or_else(|| session_not_found(target.session_name()))?;
            let window = session.window_at(target.window_index()).ok_or_else(|| {
                RmuxError::invalid_target(
                    format!("{}:{}", target.session_name(), target.window_index()),
                    "window index does not exist in session",
                )
            })?;
            let pane = window.pane(target.pane_index()).ok_or_else(|| {
                RmuxError::invalid_target(target.to_string(), "pane index does not exist in window")
            })?;
            pane_terminal_geometry_for_session(
                session,
                &self.options,
                target.window_index(),
                pane.index(),
                pane.geometry(),
                false,
                false,
            )
        };
        // The layout can have moved underneath a pane that spent this long starting, and the job
        // was opened at the geometry the plan saw. Rows and columns are the whole of what a
        // managed pane terminal can be told about its size.
        terminal.resize(pane_terminal_size(pane_geometry));
        let pid = terminal.pid();
        let (queued_input, input_shell) = {
            let starting = self
                .starting_panes
                .get_mut(&runtime_session_name)
                .and_then(|panes| panes.get_mut(&pane_id))
                .filter(|starting| starting.generation == identity.generation())
                .ok_or_else(|| {
                    RmuxError::Server(format!(
                        "missing starting pane state for pane id {} in session {}",
                        pane_id.as_u32(),
                        runtime_session_name
                    ))
                })?;
            let queued_input = starting.queued_input.drain(..).collect::<Vec<_>>();
            starting.queued_input_bytes = 0;
            let input_shell = if queued_input.is_empty() {
                None
            } else {
                Some((terminal.io(), terminal.handle().clone()))
            };
            (queued_input, input_shell)
        };

        if self.terminals.contains_session(&runtime_session_name) {
            self.terminals.insert_pane(
                runtime_session_name.clone(),
                pane_id,
                target.window_index(),
                target.pane_index(),
                terminal,
            )?;
        } else {
            self.terminals
                .insert_session(runtime_session_name.clone(), pane_id, terminal)?;
        }
        let output_sequence = self.activate_pending_pane_output(&runtime_session_name, pane_id)?;
        self.mark_pane_lifecycle_running(pane_id, pid);
        self.update_pane_lifecycle_output_sequence(pane_id, output_sequence);
        self.sync_pane_lifecycle_dimensions_for_session(target.session_name());

        Ok(Some(CompletedDeferredInitialPane {
            runtime_session_name_hint: runtime_session_name,
            identity,
            pane_pid: pid,
            input_shell,
            queued_input,
        }))
    }

    pub(crate) fn take_deferred_initial_pane_input_or_finish(
        &mut self,
        runtime_session_name_hint: &SessionName,
        identity: DeferredInitialPaneIdentity,
    ) -> Result<DeferredInitialPaneInputDrain, RmuxError> {
        let Some(runtime_session_name) =
            self.starting_runtime_session_for_identity(runtime_session_name_hint, identity)
        else {
            return Ok(DeferredInitialPaneInputDrain::Missing);
        };
        let pane_id = identity.pane_id();
        let Some(starting) = self
            .starting_panes
            .get_mut(&runtime_session_name)
            .and_then(|panes| panes.get_mut(&pane_id))
            .filter(|starting| starting.generation == identity.generation())
        else {
            return Ok(DeferredInitialPaneInputDrain::Missing);
        };

        if starting.queued_input.is_empty() {
            // Finish under the same state lock used to observe the empty
            // queue. A writer can now either enqueue before this removal and
            // be drained, or observe no StartingPane and write to the live
            // terminal; it can never enqueue into an entry removed later.
            let _ = self.remove_starting_pane_if_identity(&runtime_session_name, identity);
            return Ok(DeferredInitialPaneInputDrain::Finished {
                runtime_session_name,
            });
        }

        let queued_input = starting.queued_input.drain(..).collect::<Vec<_>>();
        starting.queued_input_bytes = 0;
        let target = self.pane_target_for_runtime_pane(&runtime_session_name, pane_id);
        let Some(target) = target else {
            let _ = self.remove_starting_pane_if_identity(&runtime_session_name, identity);
            return Ok(DeferredInitialPaneInputDrain::Finished {
                runtime_session_name,
            });
        };
        let input_shell = self.terminals.pane_shell(
            &runtime_session_name,
            pane_id,
            target.window_index(),
            target.pane_index(),
        )?;
        let pane_pid = self.terminals.pane_pid(
            &runtime_session_name,
            pane_id,
            target.window_index(),
            target.pane_index(),
        )?;

        Ok(DeferredInitialPaneInputDrain::Flush {
            runtime_session_name,
            flush: DeferredInitialPaneInputFlush {
                input_shell,
                pane_pid,
                queued_input,
            },
        })
    }

    pub(crate) fn finish_deferred_initial_pane_input_after_error(
        &mut self,
        runtime_session_name_hint: &SessionName,
        identity: DeferredInitialPaneIdentity,
    ) {
        let Some(runtime_session_name) =
            self.starting_runtime_session_for_identity(runtime_session_name_hint, identity)
        else {
            return;
        };
        let _ = self.remove_starting_pane_if_identity(&runtime_session_name, identity);
    }

    pub(crate) fn cancel_starting_pane(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> bool {
        self.remove_starting_pane(runtime_session_name, pane_id)
            .is_some()
    }

    /// Reports a deferred pane whose job never opened, into the pane itself.
    ///
    /// The deferred path's replacement for the rollback every other creation performs: there is
    /// no half-created session to remove here — `new-session` already answered and the user is
    /// looking at this pane — so the failure is written into the transcript the prepare installed
    /// and the pane is retired as a dead one.
    ///
    /// Returns the exit event the pane's surface still owes its readers, for the caller to
    /// publish once the state lock is released. `None` when the pane is already gone.
    pub(crate) fn fail_deferred_initial_pane(
        &mut self,
        commit: &DeferredInitialPaneCommit,
        error: &RmuxError,
    ) -> Option<PaneExitEvent> {
        let identity = commit.identity;
        let pane_id = identity.pane_id();
        let runtime_session_name =
            self.starting_runtime_session_for_identity(&commit.runtime_session_name, identity)?;
        let visible_session_name = self
            .pane_target_for_runtime_pane(&runtime_session_name, pane_id)
            .map(|target| target.session_name().clone())
            .unwrap_or_else(|| commit.visible_session_name.clone());
        let _ = self.remove_starting_pane_if_identity(&runtime_session_name, identity);
        let message = format!(
            "failed to spawn pane {} in session {}: {error}",
            pane_id.as_u32(),
            visible_session_name
        );
        self.add_message(message.clone());
        let bytes = format!("{message}\r\n").into_bytes();
        let published = self
            .publish_bytes_to_runtime_pane_transcript(
                &runtime_session_name,
                pane_id,
                Some(identity.generation()),
                bytes,
            )
            .unwrap_or(false);
        if published {
            if let Some(sender) = self
                .pane_outputs
                .get(&runtime_session_name)
                .and_then(|panes| panes.get(&pane_id))
            {
                let _ = sender.send_for_generation(Some(identity.generation()), Vec::new());
            }
        }
        let metadata = PaneExitMetadata {
            status: Some(1),
            signal: None,
            time: Some(chrono::Local::now().timestamp()),
        };
        self.dead_panes
            .entry(runtime_session_name.clone())
            .or_default()
            .insert(pane_id, metadata);
        self.mark_pane_lifecycle_exited(pane_id, metadata);
        Some(PaneExitEvent::eof_published(
            runtime_session_name,
            pane_id,
            Some(identity.generation()),
        ))
    }

    pub(crate) fn pane_is_starting_in_window(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> bool {
        self.starting_pane_in_window(session_name, window_index, pane_index)
            .is_some()
    }

    pub(crate) fn active_pane_is_starting(&self, session_name: &SessionName) -> bool {
        let Some(session) = self.sessions.session(session_name) else {
            return false;
        };
        self.pane_is_starting_in_window(
            session_name,
            session.active_window_index(),
            session.active_pane_index(),
        )
    }

    pub(crate) fn starting_pane_profile_in_window(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Option<&crate::terminal::TerminalProfile> {
        self.starting_pane_in_window(session_name, window_index, pane_index)
            .map(|pane| &pane.profile)
    }

    pub(crate) fn starting_pane_runtime_window_name_in_window(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Option<&str> {
        self.starting_pane_in_window(session_name, window_index, pane_index)
            .and_then(|pane| pane.runtime_window_name.as_deref())
    }

    pub(crate) fn starting_pane_base_environment(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<crate::terminal::SessionBaseEnvironment> {
        self.starting_panes
            .get(runtime_session_name)?
            .get(&pane_id)
            .map(|pane| crate::terminal::SessionBaseEnvironment::from_profile(&pane.profile))
    }

    pub(crate) fn queue_starting_pane_input(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
        bytes: &[u8],
    ) -> Result<bool, RmuxError> {
        if bytes.is_empty() {
            return Ok(false);
        }
        self.queue_starting_pane_input_entry(
            session_name,
            window_index,
            pane_index,
            DeferredInitialPaneInput::Bytes(bytes.to_vec()),
        )
    }

    pub(crate) fn queue_starting_pane_paste_input(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
        bytes: &[u8],
        delimiters: PasteDelimiters,
    ) -> Result<bool, RmuxError> {
        if bytes.is_empty() {
            return Ok(false);
        }
        self.queue_starting_pane_input_entry(
            session_name,
            window_index,
            pane_index,
            DeferredInitialPaneInput::Paste {
                bytes: bytes.to_vec(),
                delimiters,
            },
        )
    }

    pub(crate) fn queue_starting_pane_console_input(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
        action: DeferredInitialPaneConsoleInputAction,
        byte_len: usize,
    ) -> Result<bool, RmuxError> {
        self.queue_starting_pane_input_entry(
            session_name,
            window_index,
            pane_index,
            DeferredInitialPaneInput::Console {
                action,
                byte_len: byte_len.max(1),
            },
        )
    }

    fn queue_starting_pane_input_entry(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
        input: DeferredInitialPaneInput,
    ) -> Result<bool, RmuxError> {
        let Some(pane_id) = self
            .sessions
            .session(session_name)
            .and_then(|session| session.window_at(window_index))
            .and_then(|window| window.pane(pane_index))
            .map(rmux_core::Pane::id)
        else {
            return Ok(false);
        };
        let runtime_session_name = self.runtime_session_name_for_window(session_name, window_index);
        let Some(starting) = self
            .starting_panes
            .get_mut(&runtime_session_name)
            .and_then(|panes| panes.get_mut(&pane_id))
        else {
            return Ok(false);
        };
        let next_len = starting.queued_input_bytes.saturating_add(input.byte_len());
        if next_len > STARTING_PANE_INPUT_MAX_BYTES {
            return Err(RmuxError::Server(format!(
                "pane {}:{window_index}.{pane_index} is still starting and its input queue is full",
                session_name
            )));
        }
        starting.queued_input.push_back(input);
        starting.queued_input_bytes = next_len;
        Ok(true)
    }

    fn starting_pane_in_window(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Option<&StartingPane> {
        let pane_id = self
            .sessions
            .session(session_name)?
            .window_at(window_index)?
            .pane(pane_index)?
            .id();
        let runtime_session_name = self.runtime_session_name_for_window(session_name, window_index);
        self.starting_panes
            .get(&runtime_session_name)?
            .get(&pane_id)
    }

    fn starting_generation_matches(
        &self,
        runtime_session_name: &SessionName,
        identity: DeferredInitialPaneIdentity,
    ) -> bool {
        self.starting_panes
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&identity.pane_id()))
            .is_some_and(|pane| pane.generation == identity.generation())
    }

    pub(crate) fn starting_runtime_session_for_identity(
        &self,
        runtime_session_name_hint: &SessionName,
        identity: DeferredInitialPaneIdentity,
    ) -> Option<SessionName> {
        if self.starting_generation_matches(runtime_session_name_hint, identity) {
            return Some(runtime_session_name_hint.clone());
        }
        self.starting_panes
            .iter()
            .find(|(_, panes)| {
                panes
                    .get(&identity.pane_id())
                    .is_some_and(|pane| pane.generation == identity.generation())
            })
            .map(|(session_name, _)| session_name.clone())
    }

    fn remove_starting_pane_if_identity(
        &mut self,
        runtime_session_name: &SessionName,
        identity: DeferredInitialPaneIdentity,
    ) -> Option<StartingPane> {
        if !self.starting_generation_matches(runtime_session_name, identity) {
            return None;
        }
        self.remove_starting_pane(runtime_session_name, identity.pane_id())
    }

    fn remove_starting_pane(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<StartingPane> {
        let panes = self.starting_panes.get_mut(runtime_session_name)?;
        let removed = panes.remove(&pane_id);
        if panes.is_empty() {
            let _ = self.starting_panes.remove(runtime_session_name);
        }
        removed
    }
}

#[cfg(test)]
#[path = "deferred_initial/tests.rs"]
mod tests;
