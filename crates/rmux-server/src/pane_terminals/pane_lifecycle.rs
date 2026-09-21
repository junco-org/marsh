use std::collections::HashSet;
use std::path::Path;

use rmux_core::{PaneId, Session};
use rmux_proto::{
    KillPaneResponse, PaneTarget, RespawnPaneRequest, RespawnPaneResponse, RmuxError, SessionName,
    WindowTarget,
};

use crate::pane_terminal_lookup::{initial_pane, SessionPane};
use crate::pane_terminal_process::{
    open_pane_terminal, PaneRoute, PaneTerminal, PaneTerminalRequest,
};
use crate::terminal::{validate_process_command, SessionBaseEnvironment, TerminalProfile};

use super::lifecycle_state::terminal_size_from_geometry;
use super::{
    pane_terminal_geometry_for_session, session_not_found, HandlerState, InitialPaneSpawnOptions,
    KilledPaneHookContext, KilledPaneResult, PaneLifecycleSpawn, PaneOutputSpawn,
    SessionTransferSnapshot, WindowNameApplication, WindowSpawnOptions,
};

#[path = "pane_lifecycle/preview.rs"]
mod preview;

#[path = "pane_lifecycle/split.rs"]
mod split;

#[path = "pane_lifecycle/linked_kill.rs"]
mod linked_kill;
pub(in crate::pane_terminals) use linked_kill::LinkedWindowTransferRemovalPlan;

use preview::preview_kill_pane;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupedLastPaneAction {
    KillSharedPane,
    RemoveAddressedAlias,
}

/// One pane terminal's creation, decided but not yet performed.
///
/// A pane is created in three steps, and this value is what crosses the boundary between the
/// first two. Everything that needs [`HandlerState`] — the layout cell, the resolved environment,
/// the shell decision, the pane's stable id and the output generation reserved for it — is
/// computed while the daemon's request mutex is held. The job itself is then opened with that
/// mutex *released*, because opening one awaits a shell build, a snapshot creation and the
/// facade's admission lock; awaiting those under the request mutex would stall every other
/// session in the daemon and invert against the adoption path, which takes admission first and
/// handler state second.
///
/// Owned, for the same reason: nothing here may borrow the state the caller is about to let go of.
pub(in crate::pane_terminals) struct PlannedPaneTerminal {
    /// What the engine is asked for, including the route its bytes are stamped with.
    request: PaneTerminalRequest,
    /// The facade the job is admitted through, resolved while the state lock was still held.
    io: crate::io::ShellIo,
    /// The runtime session the pane is committed into.
    runtime_session_name: SessionName,
    /// The surface state the commit installs.
    output: PaneOutputSpawn,
    /// The respawn metadata the commit records.
    lifecycle: PaneLifecycleSpawn,
    /// The automatic window name this pane's command implies, for a caller that applies one.
    automatic_window_name: Option<String>,
    /// Where the pane sat in its window when the plan was made.
    pane_index: u32,
}

impl PlannedPaneTerminal {
    /// Assembles a plan whose pane is not a window's first, for the split path.
    pub(in crate::pane_terminals) const fn for_split(
        request: PaneTerminalRequest,
        io: crate::io::ShellIo,
        runtime_session_name: SessionName,
        output: PaneOutputSpawn,
        lifecycle: PaneLifecycleSpawn,
        pane_index: u32,
    ) -> Self {
        Self {
            request,
            io,
            runtime_session_name,
            output,
            lifecycle,
            automatic_window_name: None,
            pane_index,
        }
    }

    pub(in crate::pane_terminals) fn automatic_window_name(&self) -> Option<&str> {
        self.automatic_window_name.as_deref()
    }

    /// Opens the job this plan describes. The handler's state lock must not be held.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`open_pane_terminal`] fails. Nothing has been committed, so the
    /// caller's own rollback is all that is left to do.
    pub(in crate::pane_terminals) async fn open(
        self,
    ) -> Result<PreparedWindowTerminal, RmuxError> {
        let Self {
            request,
            io,
            runtime_session_name,
            output,
            lifecycle,
            automatic_window_name: _,
            pane_index,
        } = self;
        let terminal = open_pane_terminal(&io, request).await?;
        Ok(PreparedWindowTerminal {
            terminal,
            runtime_session_name,
            output,
            lifecycle,
            pane_index,
        })
    }
}

/// Where an externally created job ended up after it was adopted.
pub(crate) struct AdoptedExternalJob {
    /// The session whose window now presents the job.
    pub(crate) session_name: SessionName,
    /// The stable pane id the job's route must name.
    pub(crate) pane_id: PaneId,
    /// The output generation the adoption reserved for it.
    pub(crate) generation: u64,
}

/// A pane's replacement terminal, planned but not opened.
pub(crate) struct PlannedPaneRespawn {
    plan: PlannedPaneTerminal,
    commit: PaneRespawnCommit,
}

/// What a respawn still needs once its replacement job is open.
pub(crate) struct PaneRespawnCommit {
    target: PaneTarget,
    runtime_session_name: SessionName,
    session_name: SessionName,
    window_index: u32,
    pane_index: u32,
    pane_id: PaneId,
    window_id: rmux_core::WindowId,
    window_name: String,
    automatic_window_name: Option<String>,
    /// Whether the pane being replaced had not finished starting.
    pane_was_starting: bool,
    /// Whether the pane being replaced still had a live job, for the replaced-pane hook.
    pane_was_alive: bool,
}

impl PlannedPaneRespawn {
    /// Opens the replacement job. The handler's state lock must not be held.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`PlannedPaneTerminal::open`] fails. Nothing has been torn down, so
    /// the pane being replaced is still intact and still the user's.
    pub(crate) async fn open(
        self,
    ) -> Result<(PaneRespawnCommit, PreparedWindowTerminal), RmuxError> {
        let prepared = self.plan.open().await?;
        Ok((self.commit, prepared))
    }
}

/// A window's initial pane terminal, planned but not opened.
pub(crate) struct PlannedWindowTerminal {
    plan: PlannedPaneTerminal,
    commit: WindowTerminalCommit,
}

/// What a window's initial pane still needs once its job is open.
pub(crate) struct WindowTerminalCommit {
    session_name: SessionName,
    window_index: u32,
    initial_window_name: Option<String>,
}

impl PlannedWindowTerminal {
    /// Opens the job this plan describes. The handler's state lock must not be held.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`PlannedPaneTerminal::open`] fails. Nothing has been committed, so
    /// the caller's own rollback of the half-created window is all that is left to do.
    pub(crate) async fn open(
        self,
    ) -> Result<(WindowTerminalCommit, PreparedWindowTerminal), RmuxError> {
        let prepared = self.plan.open().await?;
        Ok((self.commit, prepared))
    }
}

/// A new session's first pane terminal, planned but not opened.
///
/// Carries the window name the commit applies. That name is derived from the profile and the
/// command, both of which only exist once the plan has been made, and applying it before the job
/// opens would rename a window whose pane may never arrive.
pub(crate) struct PlannedInitialSessionTerminal {
    plan: PlannedPaneTerminal,
    commit: InitialSessionCommit,
}

/// What a new session's first pane still needs once its job is open.
pub(crate) struct InitialSessionCommit {
    session_name: SessionName,
    window_index: u32,
    initial_window_name: Option<String>,
}

impl PlannedInitialSessionTerminal {
    /// Opens the job this plan describes. The handler's state lock must not be held.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`PlannedPaneTerminal::open`] fails. Nothing has been committed, so
    /// the caller's own rollback of the half-created session is all that is left to do.
    pub(crate) async fn open(
        self,
    ) -> Result<(InitialSessionCommit, PreparedWindowTerminal), RmuxError> {
        let prepared = self.plan.open().await?;
        Ok((self.commit, prepared))
    }
}

/// One pane terminal's creation, with its job open and nothing committed yet.
pub(crate) struct PreparedWindowTerminal {
    terminal: PaneTerminal,
    runtime_session_name: SessionName,
    output: PaneOutputSpawn,
    lifecycle: PaneLifecycleSpawn,
    pane_index: u32,
}

impl PreparedWindowTerminal {
    /// The three pieces a commit installs, for a caller that owns its own insertion order.
    pub(in crate::pane_terminals) fn into_parts(
        self,
    ) -> (PaneTerminal, PaneOutputSpawn, PaneLifecycleSpawn) {
        (self.terminal, self.output, self.lifecycle)
    }

    /// The job the open produced, for a caller that has to name it back to whoever asked.
    ///
    /// Read before the commit consumes this, because the name is the only part of the answer the
    /// caller could not already know: the shell prompt's anonymous `&` asks the engine to
    /// allocate one, and `%3 started` has to say which one it got.
    pub(crate) fn shell_id(&self) -> &marsh_core::shellmux::ShellId {
        self.terminal.handle().id()
    }

    /// The output generation the plan reserved and this job's route was installed with.
    ///
    /// Read by a commit that has to verify the pane's identity *before* its own teardown starts,
    /// rather than leaving the check to [`HandlerState::install_prepared_window_terminal`] after
    /// the state it reads has already been dismantled.
    pub(in crate::pane_terminals) const fn generation(&self) -> u64 {
        self.output.generation
    }

    /// Stops the job this prepared terminal opened, for a commit that refused it.
    ///
    /// Nothing else will ever present it: it was opened for a pane that has since been replaced,
    /// and leaving it running would leave a shell alive in the seed with no surface and no way
    /// for anyone to reach it.
    pub(in crate::pane_terminals) fn abandon(self) {
        self.terminal.terminate_in_background();
    }
}

impl HandlerState {
    /// Plans the terminal for the first pane of `window_index` in `session`.
    ///
    /// `session` may be a *preview* of a mutation that has not been applied yet — a respawned
    /// window, a window about to be created — which is what lets the layout cell and the profile
    /// be computed against the shape the pane will actually have without mutating live state
    /// before the job exists.
    ///
    /// # Errors
    ///
    /// Fails when the window or its initial pane is missing from `session`, when the profile's
    /// environment or directory cannot be resolved, and when no shell facade is bound.
    pub(in crate::pane_terminals) fn plan_window_terminal(
        &mut self,
        session: &Session,
        window_index: u32,
        spawn: WindowSpawnOptions<'_>,
        base_environment: Option<&SessionBaseEnvironment>,
    ) -> Result<PlannedPaneTerminal, RmuxError> {
        let window = session.window_at(window_index).ok_or_else(|| {
            RmuxError::invalid_target(
                format!("{}:{window_index}", session.name()),
                "window index does not exist in session",
            )
        })?;
        let pane = window.pane(0).ok_or_else(|| {
            RmuxError::Server(format!(
                "initial pane missing for session {}:{window_index}",
                session.name()
            ))
        })?;
        let pane_geometry = pane_terminal_geometry_for_session(
            session,
            &self.options,
            window_index,
            pane.index(),
            pane.geometry(),
            false,
            false,
        );
        // Named exactly when *this request* carried a directory. A window that fell back to the
        // session's own recorded directory, and a respawn whose directory was replayed out of
        // provenance, are both inherited: neither is a caller asking for a place, so neither may
        // fail the spawn for being outside this daemon's seed.
        let named_directory = spawn
            .start_directory
            .filter(|path| !path.as_os_str().is_empty())
            .is_some()
            && !spawn.inherited_start_directory;
        let mut profile = TerminalProfile::for_session(
            &self.environment,
            &self.options,
            session.name(),
            session.id().as_u32(),
            spawn.socket_path,
            base_environment,
            spawn.spawn_environment,
            true,
            spawn.environment_overrides,
            Some(pane.id()),
            spawn
                .start_directory
                .filter(|path| !path.as_os_str().is_empty())
                .or(session.cwd()),
        )?;
        if !named_directory {
            profile = profile.inherit_cwd();
        }
        if let Some(shell) = spawn.respawn_shell {
            profile = profile.with_respawn_shell(shell.clone());
        }
        let automatic_window_name = profile.automatic_window_name(spawn.command);
        let runtime_window_name = profile.runtime_window_name(spawn.command);
        let initial_title = profile.initial_pane_title();
        let lifecycle_cwd = profile.cwd().to_path_buf();
        let respawn_shell = profile.pane_shell().clone();
        let io = self.require_shell_io()?;
        let runtime_session_name =
            self.runtime_session_name_for_window(session.name(), window_index);
        let generation = self.reserve_pane_output_generation(&runtime_session_name, pane.id());

        Ok(PlannedPaneTerminal {
            request: PaneTerminalRequest {
                geometry: pane_geometry,
                profile,
                runtime_window_name,
                command: spawn.command.cloned(),
                route: PaneRoute {
                    session: session.name().clone(),
                    pane: pane.id(),
                    generation,
                },
                shell_id: spawn.shell_id.clone(),
                follow_mux_lifetime: spawn.follow_mux_lifetime,
            },
            io,
            runtime_session_name,
            output: PaneOutputSpawn {
                geometry: pane_geometry,
                initial_title,
                generation,
            },
            lifecycle: PaneLifecycleSpawn {
                session_id: session.id(),
                window_id: window.id(),
                pane_id: pane.id(),
                process_command: spawn.command.cloned(),
                working_directory: Some(lifecycle_cwd),
                respawn_shell,
                private_environment: spawn.environment_overrides.map(<[String]>::to_vec),
                respawn_environment: spawn.respawn_environment.map(<[String]>::to_vec),
                dimensions: terminal_size_from_geometry(pane_geometry),
                pid: None,
            },
            automatic_window_name,
            pane_index: pane.index(),
        })
    }

    /// Gives an already-open job a window, without opening a second one.
    ///
    /// The sibling of [`Self::plan_window_terminal`] for a job rmux did not create. Everything
    /// after the spawn is identical — the same layout cell, the same reserved output generation,
    /// the same [`Self::install_prepared_window_terminal`] — and the spawn itself is replaced by
    /// the handle the engine announced. Routing an adoption through window *creation* instead
    /// would open a shell only to kill it, and that kill announces its own `Opened`/`Closed` pair
    /// back into the consumer that is standing in this call.
    ///
    /// The window is created detached and is never selected: a job appearing is not the same as a
    /// user asking for it, and stealing focus would move whatever they were actually doing.
    ///
    /// No prompt reader is started, and the profile built here never reaches the job. An adopted
    /// job already has its own command, its own environment and, if it is interactive, whatever is
    /// already reading its terminal; the profile exists so rmux's own metadata — the pane's
    /// directory, its reported shell, its window name — has the same shape as every other pane's.
    ///
    /// # Errors
    ///
    /// Fails when no session can be found or created for the job, when the window cannot be
    /// created, and when the profile cannot be resolved. Nothing is installed in any failing case,
    /// so the caller can fall back to routing the job hidden.
    pub(crate) fn adopt_external_job_window(
        &mut self,
        io: &crate::io::ShellIo,
        job: &crate::io::ShellHandle,
        socket_path: &Path,
    ) -> Result<AdoptedExternalJob, RmuxError> {
        let session_name = self.session_for_adoption(io)?;
        let size = self
            .sessions
            .session(&session_name)
            .ok_or_else(|| session_not_found(&session_name))?
            .window()
            .size();
        let base_index = self
            .options
            .resolve(Some(&session_name), rmux_proto::OptionName::BaseIndex)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let pane_id = self.sessions.allocate_pane_id();
        let window_index = {
            let session = self
                .sessions
                .session_mut(&session_name)
                .ok_or_else(|| session_not_found(&session_name))?;
            let (window_index, _) =
                session.create_window_at_or_above_with_pane_id(size, base_index, pane_id)?;
            session.rename_window(window_index, job.id().as_str().to_owned())?;
            window_index
        };
        let runtime_session_name =
            self.runtime_session_name_for_window(&session_name, window_index);

        let session = self
            .sessions
            .session(&session_name)
            .ok_or_else(|| session_not_found(&session_name))?
            .clone();
        let window = session.window_at(window_index).ok_or_else(|| {
            RmuxError::Server(format!(
                "adopted window {session_name}:{window_index} disappeared while being created"
            ))
        })?;
        let pane = window.pane(0).ok_or_else(|| {
            RmuxError::Server(format!(
                "adopted window {session_name}:{window_index} has no initial pane"
            ))
        })?;
        let pane_geometry = pane_terminal_geometry_for_session(
            &session,
            &self.options,
            window_index,
            pane.index(),
            pane.geometry(),
            false,
            false,
        );
        let base_environment =
            self.session_base_environment_for_window(&session_name, window_index);
        let profile = TerminalProfile::for_session(
            &self.environment,
            &self.options,
            &session_name,
            session.id().as_u32(),
            socket_path,
            base_environment.as_ref(),
            None,
            true,
            None,
            Some(pane_id),
            session.cwd(),
        )?;
        let initial_title = profile.initial_pane_title();
        let lifecycle_cwd = profile.cwd().to_path_buf();
        let respawn_shell = profile.pane_shell().clone();
        let generation = self.reserve_pane_output_generation(&runtime_session_name, pane_id);
        let terminal = PaneTerminal::new(
            job.clone(),
            io.unleased(),
            crate::pane_terminal_process::pane_terminal_size(pane_geometry),
            None,
            profile,
        );
        let prepared = PreparedWindowTerminal {
            terminal,
            runtime_session_name: runtime_session_name.clone(),
            output: PaneOutputSpawn {
                geometry: pane_geometry,
                initial_title,
                generation,
            },
            lifecycle: PaneLifecycleSpawn {
                session_id: session.id(),
                window_id: window.id(),
                pane_id,
                process_command: None,
                working_directory: Some(lifecycle_cwd),
                respawn_shell,
                private_environment: None,
                respawn_environment: None,
                dimensions: terminal_size_from_geometry(pane_geometry),
                pid: None,
            },
            pane_index: pane.index(),
        };
        self.install_prepared_window_terminal(
            &runtime_session_name,
            window_index,
            prepared,
            None,
        )?;

        Ok(AdoptedExternalJob {
            session_name,
            pane_id,
            generation,
        })
    }

    /// The session an externally created job is adopted into.
    ///
    /// Preference order, and each step exists for a reason: the session showing the engine's
    /// *current* job puts a new shell next to the one the user is already looking at; the earliest
    /// surviving session is a stable answer that does not depend on selection; and with no session
    /// at all there is nothing to attach to, so one is created under rmux's own automatic naming
    /// and this job becomes its initial pane.
    ///
    /// # Errors
    ///
    /// Fails when a session has to be created and the session store refuses to create one.
    fn session_for_adoption(
        &mut self,
        io: &crate::io::ShellIo,
    ) -> Result<SessionName, RmuxError> {
        let current = io
            .current_job()
            .and_then(|view| io.route_for(&view.sandbox))
            .map(|(session_name, _, _)| session_name)
            .filter(|session_name| self.sessions.session(session_name).is_some());
        if let Some(session_name) = current {
            return Ok(session_name);
        }
        let mut names = self
            .sessions
            .iter()
            .map(|(session_name, _)| session_name.clone())
            .collect::<Vec<_>>();
        names.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        if let Some(session_name) = names.into_iter().next() {
            return Ok(session_name);
        }
        let base_index = self
            .options
            .resolve(None, rmux_proto::OptionName::BaseIndex)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        self.sessions.create_auto_named_session_with_base_index(
            rmux_proto::TerminalSize { cols: 80, rows: 24 },
            base_index,
        )
    }

    /// Commits a prepared pane terminal into `runtime_session_name`, if it is still wanted.
    ///
    /// The identity check is the whole reason this is separate from the plan. The job was opened
    /// with the request mutex released, so between the plan and here the pane may have been
    /// killed, moved into another session, or respawned by someone else — and installing a job
    /// into a pane that is no longer the one it was opened for would put one shell's output on
    /// another shell's screen. Two facts have to still hold: the pane must still exist in this
    /// runtime session, and the generation reserved for it must still be the current one.
    ///
    /// A job that fails the check is stopped rather than installed, because nothing will ever
    /// present it.
    ///
    /// # Errors
    ///
    /// Fails when the pane is gone or has been superseded, and when the terminal or output store
    /// refuses the insertion. The job is stopped in every failing case.
    pub(in crate::pane_terminals) fn install_prepared_window_terminal(
        &mut self,
        runtime_session_name: &SessionName,
        window_index: u32,
        prepared: PreparedWindowTerminal,
        retained_output_sender: Option<crate::pane_io::PaneOutputSender>,
    ) -> Result<PaneId, RmuxError> {
        let PreparedWindowTerminal {
            terminal,
            runtime_session_name: _,
            output,
            lifecycle,
            pane_index,
        } = prepared;
        let pane_id = lifecycle.pane_id;
        if let Err(error) = self.check_pane_commit_identity(runtime_session_name, pane_id, output.generation) {
            terminal.terminate_in_background();
            return Err(error);
        }
        self.terminals.insert_pane(
            runtime_session_name.clone(),
            pane_id,
            window_index,
            pane_index,
            terminal,
        )?;
        if let Err(error) = self.reset_pane_output_with_sender(
            runtime_session_name,
            pane_id,
            output,
            retained_output_sender,
        ) {
            if let Some(terminal) = self.terminals.remove_pane(runtime_session_name, pane_id) {
                terminal.terminate_in_background();
            }
            return Err(error);
        }
        self.record_pane_lifecycle_spawn(lifecycle);
        let output_sequence = self.pane_output_generation(runtime_session_name, pane_id);
        self.update_pane_lifecycle_output_sequence(pane_id, output_sequence);
        Ok(pane_id)
    }

    /// Whether the pane a job was opened for is still the pane the job may be installed into.
    ///
    /// `generation` is the number the plan reserved and the job's route was installed with. It
    /// having been superseded means another transaction already claimed this pane — a respawn, or
    /// a second creation — and this job's bytes belong to a surface that no longer exists.
    ///
    /// # Errors
    ///
    /// Fails when the pane has disappeared from this runtime session, and when its output
    /// generation has moved past the reservation.
    pub(in crate::pane_terminals) fn check_pane_commit_identity(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
        generation: u64,
    ) -> Result<(), RmuxError> {
        if self
            .pane_target_for_runtime_pane(runtime_session_name, pane_id)
            .is_none()
        {
            return Err(RmuxError::Server(format!(
                "pane id {} disappeared from session {runtime_session_name} while its shell was \
                 opening",
                pane_id.as_u32()
            )));
        }
        let current = self.pane_output_generation(runtime_session_name, pane_id);
        if current != generation {
            return Err(RmuxError::Server(format!(
                "pane id {} in session {runtime_session_name} was replaced while its shell was \
                 opening: expected output generation {generation}, found {current}",
                pane_id.as_u32()
            )));
        }
        Ok(())
    }

    /// Plans the terminal for a brand-new session's first pane.
    ///
    /// # Errors
    ///
    /// Fails when the session or its initial pane is missing, and when the profile's environment
    /// or directory cannot be resolved.
    pub(crate) fn plan_initial_session_terminal(
        &mut self,
        session_name: &SessionName,
        spawn: InitialPaneSpawnOptions<'_>,
    ) -> Result<PlannedInitialSessionTerminal, RmuxError> {
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
        let profile = TerminalProfile::for_initial_session_pane(
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
        let io = self.require_shell_io()?;
        let generation = self.reserve_pane_output_generation(&runtime_session_name, pane.id);

        Ok(PlannedInitialSessionTerminal {
            plan: PlannedPaneTerminal {
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
                runtime_session_name,
                output: PaneOutputSpawn {
                    geometry: pane_geometry,
                    initial_title,
                    generation,
                },
                lifecycle: PaneLifecycleSpawn {
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
                },
                automatic_window_name: None,
                pane_index: pane.index,
            },
            commit: InitialSessionCommit {
                session_name: session_name.clone(),
                window_index: pane.window_index,
                initial_window_name,
            },
        })
    }

    /// Runs a whole initial-session-terminal transaction without releasing a lock, for tests.
    ///
    /// The counterpart of [`Self::insert_window_terminal`], with the same restriction: production
    /// callers hold the request mutex and must drop it across the spawn.
    ///
    /// # Errors
    ///
    /// Fails for the reasons the plan, the spawn or the commit fail.
    #[cfg(test)]
    pub(crate) async fn insert_initial_session_terminal(
        &mut self,
        session_name: &SessionName,
        spawn: InitialPaneSpawnOptions<'_>,
    ) -> Result<(), RmuxError> {
        let planned = self.plan_initial_session_terminal(session_name, spawn)?;
        let (commit, prepared) = planned.open().await?;
        self.commit_initial_session_terminal(commit, prepared)
    }

    /// Installs the session's first pane terminal, if that pane is still the one it was opened for.
    ///
    /// # Errors
    ///
    /// Fails when the pane was removed or superseded while the job was opening, when the window
    /// name cannot be applied, and when the terminal or output store refuses the insertion. The
    /// job is stopped in every failing case, so a rejected session leaves no orphaned shell.
    pub(crate) fn commit_initial_session_terminal(
        &mut self,
        commit: InitialSessionCommit,
        prepared: PreparedWindowTerminal,
    ) -> Result<(), RmuxError> {
        let InitialSessionCommit {
            session_name,
            window_index,
            initial_window_name,
        } = commit;
        let PreparedWindowTerminal {
            terminal,
            runtime_session_name,
            output,
            lifecycle,
            pane_index: _,
        } = prepared;
        let pane_id = lifecycle.pane_id;
        if let Err(error) =
            self.check_pane_commit_identity(&runtime_session_name, pane_id, output.generation)
        {
            terminal.terminate_in_background();
            return Err(error);
        }
        if let Err(error) = self.apply_window_name(
            &session_name,
            window_index,
            initial_window_name,
            WindowNameApplication::Initial,
        ) {
            terminal.terminate_in_background();
            return Err(error);
        }
        self.terminals
            .insert_session(runtime_session_name.clone(), pane_id, terminal)?;
        if let Err(error) = self.insert_pane_output(&runtime_session_name, pane_id, output) {
            if let Some(terminals) = self.terminals.remove_session(&runtime_session_name) {
                let mut terminals = terminals;
                terminate_removed_terminals(&mut terminals);
            }
            return Err(error);
        }
        self.record_pane_lifecycle_spawn(lifecycle);
        let output_sequence = self.pane_output_generation(&runtime_session_name, pane_id);
        self.update_pane_lifecycle_output_sequence(pane_id, output_sequence);

        Ok(())
    }

    pub(crate) fn resize_window_terminal_runtime(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
    ) -> Result<(), RmuxError> {
        #[cfg(test)]
        {
            self.window_runtime_resize_count = self.window_runtime_resize_count.saturating_add(1);
        }
        let (runtime_session_name, pane_geometries) = {
            let session = self
                .sessions
                .session(session_name)
                .ok_or_else(|| session_not_found(session_name))?;
            let window = session.window_at(window_index).ok_or_else(|| {
                RmuxError::invalid_target(
                    format!("{session_name}:{window_index}"),
                    "window index does not exist in session",
                )
            })?;
            let runtime_session_name =
                self.runtime_session_name_for_window(session_name, window_index);
            let pane_geometries = window
                .panes()
                .iter()
                .map(|pane| {
                    let (alternate_on, copy_mode_active) =
                        self.pane_viewport_state(session_name, window_index, pane.id());
                    SessionPane {
                        id: pane.id(),
                        window_index,
                        index: pane.index(),
                        geometry: pane_terminal_geometry_for_session(
                            session,
                            &self.options,
                            window_index,
                            pane.index(),
                            pane.geometry(),
                            alternate_on,
                            copy_mode_active,
                        ),
                        }
                })
                .collect::<Vec<_>>();
            (runtime_session_name, pane_geometries)
        };
        self.terminals
            .resize_session(&runtime_session_name, &pane_geometries)?;
        self.resize_transcripts(&runtime_session_name, &pane_geometries);
        Ok(())
    }

    pub(crate) fn resize_terminals(&mut self, session_name: &SessionName) -> Result<(), RmuxError> {
        for (runtime_session_name, pane_geometries) in
            self.session_pane_terminal_geometries_by_runtime(session_name)?
        {
            self.terminals
                .resize_session(&runtime_session_name, &pane_geometries)?;
            self.resize_transcripts(&runtime_session_name, &pane_geometries);
        }
        self.sync_pane_lifecycle_dimensions_for_session(session_name);
        Ok(())
    }

    /// Plans the terminal for the initial pane of an existing window.
    ///
    /// # Errors
    ///
    /// Fails when the session, window or pane is missing, and when the profile's environment or
    /// directory cannot be resolved.
    pub(crate) fn plan_window_terminal_at(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        spawn: WindowSpawnOptions<'_>,
    ) -> Result<PlannedWindowTerminal, RmuxError> {
        self.plan_window_terminal_at_with_environment(session_name, window_index, spawn, None)
    }

    /// Plans the terminal for the initial pane of an existing window, against a given base
    /// environment.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::plan_window_terminal_at`] fails.
    pub(crate) fn plan_window_terminal_at_with_environment(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        spawn: WindowSpawnOptions<'_>,
        base_environment_override: Option<&SessionBaseEnvironment>,
    ) -> Result<PlannedWindowTerminal, RmuxError> {
        let session = self
            .sessions
            .session(session_name)
            .cloned()
            .ok_or_else(|| session_not_found(session_name))?;
        let captured_base_environment =
            self.session_base_environment_for_window(session_name, window_index);
        let base_environment = base_environment_override.or(captured_base_environment.as_ref());
        let plan = self.plan_window_terminal(&session, window_index, spawn.clone(), base_environment)?;
        let initial_window_name = if crate::automatic_rename::automatic_rename_enabled(
            &self.options,
            session_name,
            window_index,
        ) {
            plan.automatic_window_name().map(str::to_owned)
        } else {
            plan.request.runtime_window_name.clone()
        };
        Ok(PlannedWindowTerminal {
            plan,
            commit: WindowTerminalCommit {
                session_name: session_name.clone(),
                window_index,
                initial_window_name,
            },
        })
    }

    /// Runs a whole window-terminal transaction without releasing a lock, for tests.
    ///
    /// Production callers must not use this. They hold the daemon's request mutex around the
    /// plan and the commit and have to drop it across the spawn; a test drives `HandlerState`
    /// directly, holds nothing anyone else contends for, and only wants the three steps in order.
    ///
    /// # Errors
    ///
    /// Fails for the reasons the plan, the spawn or the commit fail.
    #[cfg(test)]
    pub(crate) async fn insert_window_terminal(
        &mut self,
        session_name: &SessionName,
        window_index: u32,
        spawn: WindowSpawnOptions<'_>,
    ) -> Result<(), RmuxError> {
        let planned = self.plan_window_terminal_at(session_name, window_index, spawn)?;
        let (commit, prepared) = planned.open().await?;
        self.commit_window_terminal(commit, prepared)
    }

    /// Installs a window's initial pane terminal, if that pane is still the one it was opened for.
    ///
    /// # Errors
    ///
    /// Fails when the pane was removed or superseded while the job was opening, when the window
    /// name cannot be applied, and when the terminal or output store refuses the insertion. The
    /// job is stopped in every failing case.
    pub(crate) fn commit_window_terminal(
        &mut self,
        commit: WindowTerminalCommit,
        prepared: PreparedWindowTerminal,
    ) -> Result<(), RmuxError> {
        let WindowTerminalCommit {
            session_name,
            window_index,
            initial_window_name,
        } = commit;
        let PreparedWindowTerminal {
            terminal,
            runtime_session_name,
            output,
            lifecycle,
            pane_index,
        } = prepared;
        let pane_id = lifecycle.pane_id;
        if let Err(error) =
            self.check_pane_commit_identity(&runtime_session_name, pane_id, output.generation)
        {
            terminal.terminate_in_background();
            return Err(error);
        }
        if let Err(error) = self.apply_window_name(
            &session_name,
            window_index,
            initial_window_name,
            WindowNameApplication::Initial,
        ) {
            terminal.terminate_in_background();
            return Err(error);
        }
        self.terminals.insert_pane(
            runtime_session_name.clone(),
            pane_id,
            window_index,
            pane_index,
            terminal,
        )?;
        if let Err(error) = self.insert_pane_output(&runtime_session_name, pane_id, output) {
            if let Some(terminal) = self.terminals.remove_pane(&runtime_session_name, pane_id) {
                terminal.terminate_in_background();
            }
            return Err(error);
        }
        self.record_pane_lifecycle_spawn(lifecycle);
        let output_sequence = self.pane_output_generation(&runtime_session_name, pane_id);
        self.update_pane_lifecycle_output_sequence(pane_id, output_sequence);

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn kill_pane(&mut self, target: PaneTarget) -> Result<KilledPaneResult, RmuxError> {
        self.kill_pane_with_options(target, false)
    }

    pub(crate) fn kill_pane_with_options(
        &mut self,
        target: PaneTarget,
        kill_all_except: bool,
    ) -> Result<KilledPaneResult, RmuxError> {
        self.kill_pane_with_grouped_last_pane_action(
            target,
            kill_all_except,
            GroupedLastPaneAction::KillSharedPane,
        )
    }

    pub(crate) fn remove_pane_alias_with_options(
        &mut self,
        target: PaneTarget,
        kill_all_except: bool,
    ) -> Result<KilledPaneResult, RmuxError> {
        self.kill_pane_with_grouped_last_pane_action(
            target,
            kill_all_except,
            GroupedLastPaneAction::RemoveAddressedAlias,
        )
    }

    fn kill_pane_with_grouped_last_pane_action(
        &mut self,
        target: PaneTarget,
        kill_all_except: bool,
        grouped_last_pane_action: GroupedLastPaneAction,
    ) -> Result<KilledPaneResult, RmuxError> {
        let session_name = target.session_name().clone();
        let previous_session = self
            .sessions
            .session(&session_name)
            .cloned()
            .ok_or_else(|| session_not_found(&session_name))?;
        let before_pane_options = self.pane_option_slots_for_session(&session_name)?;
        let (hook_context, pane_id, addressed_last_pane, remove_session) = {
            let window = previous_session
                .window_at(target.window_index())
                .ok_or_else(|| {
                    RmuxError::invalid_target(
                        format!("{}:{}", target.session_name(), target.window_index()),
                        "window index does not exist in session",
                    )
                })?;
            let pane = window.pane(target.pane_index()).ok_or_else(|| {
                RmuxError::invalid_target(
                    target.to_string(),
                    "pane index does not exist in session",
                )
            })?;
            let pane_id = pane.id();
            let hook_context = KilledPaneHookContext {
                target: target.clone(),
                pane_id: pane_id.as_u32(),
                window_id: window.id().as_u32(),
                window_name: window.name().unwrap_or_default().to_owned(),
            };
            let addressed_last_pane = !kill_all_except && window.pane_count() == 1;
            (
                hook_context,
                pane_id,
                addressed_last_pane,
                addressed_last_pane && previous_session.windows().len() == 1,
            )
        };
        let linked_window_family = self.window_link_count(&session_name, target.window_index()) > 1;
        let grouped_session_family =
            self.window_linked_session_count(&session_name, target.window_index()) > 1;
        let remove_complete_family = grouped_last_pane_action
            == GroupedLastPaneAction::KillSharedPane
            && (linked_window_family || grouped_session_family);
        if addressed_last_pane && remove_complete_family {
            return self.kill_last_linked_pane(target, hook_context, pane_id);
        }
        if remove_session {
            let mut affected_sessions =
                self.window_linked_session_family_list(&session_name, target.window_index());
            if affected_sessions.is_empty() {
                affected_sessions.push(session_name.clone());
            }
            affected_sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            affected_sessions.dedup();
            self.ensure_panes_exist(&session_name, &[pane_id])?;
            let current_runtime_owner = self.sessions.runtime_owner(&session_name);
            let next_runtime_owner = self.sessions.runtime_owner_transfer_target(&session_name);
            let removed_session = self.sessions.remove_session(&session_name)?;
            self.clear_marked_pane_if_id(pane_id);
            let _ = self.options.remove_session(&session_name);
            let _ = self.environment.remove_session(&session_name);
            self.remove_session_terminals(
                &session_name,
                current_runtime_owner.as_ref(),
                next_runtime_owner.as_ref(),
            )?;
            let removed_pane_ids = self.pane_ids_no_longer_referenced([pane_id]);
            return Ok(KilledPaneResult {
                response: KillPaneResponse {
                    target,
                    window_destroyed: true,
                },
                hook_context,
                session_destroyed: true,
                removed_session_id: Some(removed_session.id().as_u32()),
                removed_pane_ids,
                affected_sessions,
                destroyed_sessions: vec![(session_name, removed_session.id().as_u32())],
                reindexed_windows: Vec::new(),
            });
        }
        if addressed_last_pane
            && grouped_last_pane_action == GroupedLastPaneAction::RemoveAddressedAlias
            && linked_window_family
        {
            return self.remove_last_pane_addressed_window_alias(target, hook_context);
        }

        let window_index = target.window_index();
        let runtime_session_name =
            self.runtime_session_name_for_window(&session_name, window_index);
        let linked_slots = self.window_link_slots_for(&session_name, window_index);
        let before_pane_options = if linked_slots.len() > 1 {
            self.window_linked_session_family_list(&session_name, window_index)
                .into_iter()
                .map(|linked_session| {
                    let snapshot = if linked_session == session_name {
                        before_pane_options.clone()
                    } else {
                        self.pane_option_slots_for_session(&linked_session)?
                    };
                    Ok((linked_session, snapshot))
                })
                .collect::<Result<Vec<_>, RmuxError>>()?
        } else {
            vec![(session_name.clone(), before_pane_options)]
        };
        let preview_outcome = preview_kill_pane(&self.sessions, &target, kill_all_except)?;
        self.ensure_window_panes_exist(
            &session_name,
            window_index,
            preview_outcome.removed_pane_ids(),
        )?;
        let transfer_snapshot = SessionTransferSnapshot::capture(self);

        let committed_outcome = {
            let session = self
                .sessions
                .session_mut(&session_name)
                .ok_or_else(|| session_not_found(&session_name))?;
            if kill_all_except {
                session.kill_other_panes_in_window(target.window_index(), target.pane_index())?
            } else {
                session.kill_pane_in_window(target.window_index(), target.pane_index())?
            }
        };
        debug_assert_eq!(committed_outcome, preview_outcome);
        let removed_pane_ids = committed_outcome.removed_pane_ids().to_vec();
        if committed_outcome.window_destroyed() {
            let destroyed_window =
                WindowTarget::with_window(session_name.clone(), target.window_index());
            let _ = self.options.remove_window(&destroyed_window);
            let _ = self.hooks.remove_window(&destroyed_window);
            self.clear_auto_named_window(&session_name, target.window_index());
            let _ = self.detach_window_link_slot(&session_name, target.window_index());
        }
        let mut affected_sessions = if committed_outcome.window_destroyed() {
            vec![session_name.clone()]
        } else {
            let synchronize_result = (|| {
                self.synchronize_linked_window_from_slot(&session_name, window_index)?;
                let mut synchronized_sessions = HashSet::new();
                for slot in &linked_slots {
                    if synchronized_sessions.insert(slot.session_name.clone()) {
                        self.synchronize_session_group_from(&slot.session_name)?;
                    }
                }
                Ok::<_, RmuxError>(
                    self.window_linked_session_family_list(&session_name, window_index),
                )
            })();
            match synchronize_result {
                Ok(sessions) => sessions,
                Err(error) => {
                    transfer_snapshot.restore(self);
                    return Err(error);
                }
            }
        };
        let mut reindexed_windows = Vec::new();
        if committed_outcome.window_destroyed() {
            match self.renumber_windows_if_enabled(&session_name) {
                Ok(index_map) if !index_map.is_empty() => {
                    reindexed_windows.push((session_name.clone(), index_map));
                }
                Ok(_) => {}
                Err(error) => {
                    transfer_snapshot.restore(self);
                    return Err(error);
                }
            }
        }

        #[cfg(windows)]
        let terminal_pane_ids = committed_outcome
            .removed_pane_ids()
            .iter()
            .copied()
            .filter(|pane_id| {
                self.terminals
                    .ensure_panes_exist(&runtime_session_name, &[*pane_id])
                    .is_ok()
            })
            .collect::<Vec<_>>();
        #[cfg(not(windows))]
        let terminal_pane_ids = committed_outcome.removed_pane_ids().to_vec();
        let mut removed_terminals = if terminal_pane_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            match self
                .terminals
                .remove_pane_batch(&runtime_session_name, &terminal_pane_ids)
            {
                Ok(removed_terminals) => removed_terminals,
                Err(error) => {
                    transfer_snapshot.restore(self);
                    return Err(error);
                }
            }
        };
        let removed_outputs =
            self.remove_pane_outputs(&runtime_session_name, committed_outcome.removed_pane_ids());

        if let Err(error) = self.resize_terminals(&session_name) {
            let terminal_rollback = self
                .terminals
                .insert_existing_panes(&runtime_session_name, removed_terminals);
            self.insert_existing_pane_outputs(&runtime_session_name, removed_outputs);
            transfer_snapshot.restore(self);
            let resize_rollback = self.resize_terminals(&session_name);
            terminal_rollback.map_err(|rollback_error| {
                RmuxError::Server(format!(
                    "failed to restore pane terminals for runtime session {runtime_session_name} after {error}: {rollback_error}"
                ))
            })?;
            resize_rollback.map_err(|rollback_error| {
                RmuxError::Server(format!(
                    "failed to roll back session {session_name} after {error}: {rollback_error}"
                ))
            })?;
            return Err(error);
        }
        for pane_id in committed_outcome.removed_pane_ids() {
            self.clear_marked_pane_if_id(*pane_id);
        }
        #[cfg(windows)]
        for pane_id in committed_outcome.removed_pane_ids() {
            let _ = self.cancel_starting_pane(&runtime_session_name, *pane_id);
        }
        terminate_removed_terminals(&mut removed_terminals);
        self.remove_pane_lifecycles(committed_outcome.removed_pane_ids());

        if committed_outcome.window_destroyed() {
            for synchronized_session in self.synchronize_session_group_from(&session_name)? {
                if !affected_sessions.contains(&synchronized_session) {
                    affected_sessions.push(synchronized_session);
                }
            }
        }
        self.sync_pane_lifecycle_dimensions_for_session(&session_name);
        for (affected_session, before) in before_pane_options {
            self.rekey_pane_options_after_session_change(&before, &affected_session)?;
        }

        let removed_pane_ids = self.pane_ids_no_longer_referenced(removed_pane_ids);
        Ok(KilledPaneResult {
            response: KillPaneResponse {
                target,
                window_destroyed: committed_outcome.window_destroyed(),
            },
            hook_context,
            session_destroyed: false,
            removed_session_id: None,
            removed_pane_ids,
            affected_sessions,
            destroyed_sessions: Vec::new(),
            reindexed_windows,
        })
    }

    fn remove_last_pane_addressed_window_alias(
        &mut self,
        target: PaneTarget,
        hook_context: KilledPaneHookContext,
    ) -> Result<KilledPaneResult, RmuxError> {
        let session_name = target.session_name().clone();
        let mut affected_sessions =
            self.window_linked_session_family_list(&session_name, target.window_index());
        affected_sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        affected_sessions.dedup();
        let result = self.unlink_window(
            rmux_proto::WindowTarget::with_window(session_name.clone(), target.window_index()),
            false,
        )?;
        debug_assert!(
            result.removed_pane_ids.is_empty(),
            "removing one surviving window alias must preserve its shared pane runtime"
        );
        Ok(KilledPaneResult {
            response: KillPaneResponse {
                target,
                window_destroyed: true,
            },
            hook_context,
            session_destroyed: false,
            removed_session_id: None,
            removed_pane_ids: result.removed_pane_ids,
            affected_sessions,
            destroyed_sessions: Vec::new(),
            reindexed_windows: Vec::new(),
        })
    }

    /// Plans a pane's replacement terminal, without disturbing the pane it replaces.
    ///
    /// Nothing is torn down here. The old job keeps running, its output keeps reaching its pane,
    /// and the respawn is still fully reversible — which matters because the replacement job is
    /// opened with the request mutex released, and every way that can fail must leave the pane the
    /// user is looking at exactly as it was.
    ///
    /// # Errors
    ///
    /// Fails when the target does not resolve, when the workload is unusable, when the pane is
    /// still running and `-k` was not given, and when the profile cannot be resolved.
    pub(crate) fn plan_pane_respawn(
        &mut self,
        request: RespawnPaneRequest,
        socket_path: &Path,
        spawn_environment: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<PlannedPaneRespawn, RmuxError> {
        let RespawnPaneRequest {
            target,
            kill,
            mut start_directory,
            mut environment,
            command,
            process_command,
        } = request;
        let requested_process_command = process_command
            .or_else(|| crate::legacy_command::from_legacy_command(command.as_deref()));
        validate_process_command(requested_process_command.as_ref())?;
        let session_name = target.session_name().clone();
        let window_index = target.window_index();
        let pane_index = target.pane_index();
        let runtime_session_name =
            self.runtime_session_name_for_window(&session_name, window_index);
        let (session_id, window_id, window_name, pane_id, pane_geometry, requested_cwd) = {
            let session = self
                .sessions
                .session(&session_name)
                .ok_or_else(|| session_not_found(&session_name))?;
            let window = session.window_at(window_index).ok_or_else(|| {
                RmuxError::invalid_target(
                    format!("{session_name}:{window_index}"),
                    "window index does not exist in session",
                )
            })?;
            let pane = window.pane(pane_index).ok_or_else(|| {
                RmuxError::invalid_target(
                    target.to_string(),
                    "pane index does not exist in session",
                )
            })?;
            (
                session.id(),
                window.id(),
                window.name().unwrap_or_default().to_owned(),
                pane.id(),
                pane_terminal_geometry_for_session(
                    session,
                    &self.options,
                    window_index,
                    pane.index(),
                    pane.geometry(),
                    false,
                    false,
                ),
                session.cwd().map(Path::to_path_buf),
            )
        };

        let provenance = self.pane_respawn_provenance(pane_id);
        let process_command = requested_process_command.or_else(|| {
            provenance
                .as_ref()
                .and_then(|provenance| provenance.process_command.clone())
        });
        validate_process_command(process_command.as_ref())?;
        // Whether the *caller* named a directory, decided before any fallback fills one in. A
        // provenance or session directory is a replay of what an earlier pane resolved to, not a
        // request, and treating it as one makes a respawn refuse a directory the original pane
        // was perfectly happy with.
        let named_directory = start_directory.is_some();
        if start_directory.is_none() {
            start_directory = provenance
                .as_ref()
                .and_then(|provenance| provenance.working_directory.clone());
        }
        let respawn_environment = provenance
            .as_ref()
            .map(|provenance| provenance.private_environment.clone())
            .unwrap_or_else(|| environment.clone().unwrap_or_default());
        if environment.is_none() {
            environment = Some(respawn_environment.clone());
        }

        #[cfg(windows)]
        let pane_was_starting =
            self.pane_is_starting_in_window(&session_name, window_index, pane_index);
        #[cfg(not(windows))]
        let pane_was_starting = false;

        let pane_was_alive = !pane_was_starting
            && self
                .terminals
                .pane_is_alive(&runtime_session_name, pane_id)?;
        if (pane_was_starting || pane_was_alive) && !kill {
            return Err(RmuxError::ProcessStillRunning);
        }
        let base_environment = self.session_base_environment_for_pane_target(&target);
        let mut profile = TerminalProfile::for_session(
            &self.environment,
            &self.options,
            &session_name,
            session_id.as_u32(),
            socket_path,
            base_environment.as_ref(),
            spawn_environment,
            true,
            environment.as_deref(),
            Some(pane_id),
            start_directory.as_deref().or(requested_cwd.as_deref()),
        )?;
        if !named_directory {
            // The directory came from provenance or from the session, not from this request. It
            // is a replay of what an earlier pane resolved to — frequently the daemon's own
            // process cwd — so it must not be refused as if someone had asked for it.
            profile = profile.inherit_cwd();
        }
        if let Some(provenance) = provenance.as_ref() {
            profile = profile.with_respawn_shell(provenance.shell.clone());
        }
        let automatic_window_name = profile.automatic_window_name(process_command.as_ref());
        let runtime_window_name = profile.runtime_window_name(process_command.as_ref());
        let initial_title = profile.initial_pane_title();
        let lifecycle_cwd = profile.cwd().to_path_buf();
        let respawn_shell = profile.pane_shell().clone();
        let io = self.require_shell_io()?;
        let generation = self.reserve_pane_output_generation(&runtime_session_name, pane_id);

        Ok(PlannedPaneRespawn {
            plan: PlannedPaneTerminal {
                request: PaneTerminalRequest {
                    geometry: pane_geometry,
                    profile,
                    runtime_window_name,
                    command: process_command.clone(),
                    route: PaneRoute {
                        session: session_name.clone(),
                        pane: pane_id,
                        generation,
                    },
                    shell_id: None,
                    follow_mux_lifetime: false,
                },
                io,
                runtime_session_name: runtime_session_name.clone(),
                output: PaneOutputSpawn {
                    geometry: pane_geometry,
                    initial_title,
                    generation,
                },
                lifecycle: PaneLifecycleSpawn {
                    session_id,
                    window_id,
                    pane_id,
                    process_command,
                    working_directory: Some(lifecycle_cwd),
                    respawn_shell,
                    private_environment: environment,
                    respawn_environment: Some(respawn_environment),
                    dimensions: terminal_size_from_geometry(pane_geometry),
                    pid: None,
                },
                automatic_window_name: automatic_window_name.clone(),
                pane_index,
            },
            commit: PaneRespawnCommit {
                target,
                runtime_session_name,
                session_name,
                window_index,
                pane_index,
                pane_id,
                window_id,
                window_name,
                automatic_window_name,
                pane_was_starting,
                pane_was_alive,
            },
        })
    }

    /// Replaces a pane's terminal with the one that was opened for it.
    ///
    /// The old job is stopped here and not before: up to this point the respawn could still be
    /// abandoned, and a pane whose shell had already been killed for a replacement that never
    /// arrived would be a pane the user lost for nothing.
    ///
    /// # Errors
    ///
    /// Fails when the pane was removed or superseded while the replacement was opening, and when
    /// the terminal or output store refuses the replacement. The new job is stopped in the first
    /// case, where it has no surface to be installed into.
    pub(crate) fn commit_pane_respawn(
        &mut self,
        commit: PaneRespawnCommit,
        prepared: PreparedWindowTerminal,
        mut on_replaced_active_pane: impl FnMut(&mut Self, &KilledPaneHookContext),
    ) -> Result<RespawnPaneResponse, RmuxError> {
        let PaneRespawnCommit {
            target,
            runtime_session_name,
            session_name,
            window_index,
            pane_index,
            pane_id,
            window_id,
            window_name,
            automatic_window_name,
            pane_was_starting,
            pane_was_alive,
        } = commit;
        let PreparedWindowTerminal {
            terminal,
            runtime_session_name: _,
            output,
            lifecycle,
            pane_index: _,
        } = prepared;
        if let Err(error) =
            self.check_pane_commit_identity(&runtime_session_name, pane_id, output.generation)
        {
            terminal.terminate_in_background();
            return Err(error);
        }
        let _ = pane_was_starting;

        #[cfg(windows)]
        if pane_was_starting {
            // Keep the deferred pane and its accepted input intact until every fallible step for
            // the replacement has succeeded. A rejected respawn must leave the original pane able
            // to finish startup and flush its queued input.
            let _ = self.cancel_starting_pane(&runtime_session_name, pane_id);
            on_replaced_active_pane(
                self,
                &KilledPaneHookContext {
                    target: target.clone(),
                    pane_id: pane_id.as_u32(),
                    window_id: window_id.as_u32(),
                    window_name: window_name.clone(),
                },
            );
        }

        if let Some(pipe) = self.remove_pane_pipe(&runtime_session_name, pane_id) {
            pipe.stop();
        }
        if let Some(previous) = self.terminals.remove_pane(&runtime_session_name, pane_id) {
            previous.terminate_in_background();
            if pane_was_alive {
                on_replaced_active_pane(
                    self,
                    &KilledPaneHookContext {
                        target: target.clone(),
                        pane_id: pane_id.as_u32(),
                        window_id: window_id.as_u32(),
                        window_name: window_name.clone(),
                    },
                );
            }
        }
        self.terminals.insert_pane(
            runtime_session_name.clone(),
            pane_id,
            window_index,
            pane_index,
            terminal,
        )?;
        self.reset_pane_output(&runtime_session_name, pane_id, output)?;
        self.apply_window_name(
            &session_name,
            window_index,
            automatic_window_name,
            WindowNameApplication::AutomaticUpdate,
        )?;
        self.record_pane_lifecycle_spawn(lifecycle);
        let output_sequence = self.pane_output_generation(&runtime_session_name, pane_id);
        self.update_pane_lifecycle_output_sequence(pane_id, output_sequence);
        self.sync_pane_lifecycle_dimensions_for_session(&session_name);

        Ok(RespawnPaneResponse { target })
    }

}

pub(in crate::pane_terminals) fn terminate_removed_terminals(
    terminals: &mut std::collections::HashMap<PaneId, crate::pane_terminal_process::PaneTerminal>,
) {
    for terminal in terminals.drain().map(|(_, terminal)| terminal) {
        terminal.terminate_in_background();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use marsh_core::shellmux::{JobView, Sandbox, SnapshotUid};
    use rmux_proto::{
        CapturePaneRequest, LinkWindowRequest, ListPanesRequest, NewSessionRequest,
        NewWindowRequest, Request, Response, SelectPaneRequest, SplitDirection, SplitWindowRequest,
        SplitWindowTarget, TerminalSize, UnlinkWindowRequest,
    };

    use super::{PaneId, PaneTarget, RespawnPaneRequest, SessionName, WindowTarget};
    use crate::handler::RequestHandler;
    use crate::io::{Route, ShellIo};

    fn session_name(value: &str) -> SessionName {
        SessionName::new(value).expect("valid session name")
    }

    async fn create_session(handler: &RequestHandler, value: &str) -> SessionName {
        let session = session_name(value);
        let response = handler
            .handle(Request::NewSession(NewSessionRequest {
                session_name: session.clone(),
                detached: true,
                size: Some(TerminalSize { cols: 80, rows: 24 }),
                environment: None,
            }))
            .await;
        assert!(matches!(response, Response::NewSession(_)), "{response:?}");
        handler
            .wait_for_pane_startup_to_finish_for_test(&PaneTarget::new(session.clone(), 0))
            .await;
        session
    }

    async fn create_window(handler: &RequestHandler, session: &SessionName, index: u32) {
        let response = handler
            .handle(Request::NewWindow(Box::new(NewWindowRequest {
                target: session.clone(),
                name: None,
                detached: true,
                environment: None,
                command: None,
                start_directory: None,
                target_window_index: Some(index),
                insert_at_target: false,
                process_command: None,
            })))
            .await;
        assert!(matches!(response, Response::NewWindow(_)), "{response:?}");
    }

    async fn link_window(handler: &RequestHandler, owner: &SessionName, alias: &SessionName) {
        let response = handler
            .handle(Request::LinkWindow(LinkWindowRequest {
                source: WindowTarget::with_window(owner.clone(), 0),
                target: WindowTarget::with_window(alias.clone(), 0),
                after: false,
                before: false,
                kill_destination: true,
                detached: true,
            }))
            .await;
        assert!(matches!(response, Response::LinkWindow(_)), "{response:?}");
    }

    async fn unlink_window(handler: &RequestHandler, session: &SessionName, kill_if_last: bool) {
        let response = handler
            .handle(Request::UnlinkWindow(UnlinkWindowRequest {
                target: WindowTarget::with_window(session.clone(), 0),
                kill_if_last,
            }))
            .await;
        assert!(
            matches!(response, Response::UnlinkWindow(_)),
            "{response:?}"
        );
    }

    /// Every live job this facade currently presents through a pane, with that pane's route.
    ///
    /// Read back from the facade rather than from a captured index: the route is the only thing
    /// that says which pane a job's bytes belong to, and it is keyed by snapshot instance.
    fn routed_pane_jobs(io: &ShellIo) -> Vec<(JobView, SessionName, PaneId, u64)> {
        io.jobs()
            .into_iter()
            .filter(|view| !view.closing)
            .filter_map(|view| {
                io.route_for(&view.sandbox)
                    .map(|(session, pane, generation)| (view, session, pane, generation))
            })
            .collect()
    }

    /// Waits for exactly one live pane-routed job that is not `excluded`.
    ///
    /// A pane's job is admitted with no handler lock held, and a respawn leaves the job it
    /// replaces visible while it closes, so "the pane's job right now" is a thing to wait for
    /// rather than to read once. `excluded` names the instance being replaced, which is what
    /// makes "the replacement" unambiguous without consulting an index.
    async fn wait_for_pane_job(
        io: &ShellIo,
        excluded: Option<&SnapshotUid>,
    ) -> (JobView, SessionName, PaneId, u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut candidates = routed_pane_jobs(io);
            candidates.retain(|(view, ..)| Some(&view.sandbox.uid) != excluded);
            if candidates.len() == 1 {
                return candidates.remove(0);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected exactly one live pane-routed job, found {}",
                candidates.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Publishes one chunk through the daemon's real output path, as the engine's consumer does.
    async fn publish(handler: &RequestHandler, shell: &Sandbox, bytes: &[u8]) {
        let event = rmux_core::events::OutputEvent::from_shared(0, Arc::from(bytes), Vec::new());
        handler.apply_shell_output(shell, &event).await;
    }

    /// What `capture-pane -p` says the pane's transcript holds.
    async fn capture(handler: &RequestHandler, target: &PaneTarget) -> String {
        let response = handler
            .handle(Request::CapturePane(Box::new(CapturePaneRequest {
                target: target.clone(),
                start: None,
                end: None,
                print: true,
                buffer_name: None,
                alternate: false,
                escape_ansi: false,
                escape_sequences: false,
                include_format: false,
                hyperlinks: false,
                line_numbers: false,
                join_wrapped: false,
                use_mode_screen: false,
                preserve_trailing_spaces: false,
                do_not_trim_spaces: false,
                pending_input: false,
                quiet: false,
                start_is_absolute: false,
                end_is_absolute: false,
            })))
            .await;
        let Response::CapturePane(captured) = response else {
            panic!("capture-pane failed: {response:?}");
        };
        let output = captured.output.expect("capture-pane -p returns stdout");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A superseded output generation's bytes must never land in the pane that replaced it.
    ///
    /// A respawn keeps the pane. The same stable [`PaneId`] presents a different job afterwards,
    /// and while the job being replaced is still closing its route names that same pane — so
    /// nothing in either job's own identity separates the two surfaces. The output generation the
    /// pane reserved for each of them is the only thing that does, which is why this drives the
    /// daemon's real publication path with the retiring job's sandbox and then asks the pane,
    /// through `capture-pane`, what it actually holds.
    ///
    /// The pre-respawn route is reinstalled rather than waited for. The window in which both
    /// routes name this pane closes on the daemon's runtime, whenever the retiring job's closure
    /// gets there, and is not something a test can hold open. Reinstalling reproduces exactly what
    /// that window contains — a live route naming this pane with the superseded generation — so
    /// the generation is the only thing left that can reject the bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn superseded_generation_output_never_reaches_the_respawned_pane() {
        let handler = RequestHandler::new();
        let session = create_session(&handler, "respawn-output-generation").await;
        let target = PaneTarget::new(session.clone(), 0);
        let io = handler
            .shell_io()
            .expect("a unit-test handler binds its own engine");

        let (retiring, route_session, pane, superseded) = wait_for_pane_job(&io, None).await;
        publish(&handler, &retiring.sandbox, b"before-respawn\r\n").await;
        let captured = capture(&handler, &target).await;
        assert!(
            captured.contains("before-respawn"),
            "a pane's own generation must reach its transcript: {captured:?}"
        );

        // The seed root is named explicitly so this regression measures the output generation and
        // nothing else. A unit-test engine leases a scratch seed while the daemon's own process
        // directory is the crate being tested, and a respawn that inherits the latter is a
        // separate concern from the one under test here.
        let seed = io
            .executor_info()
            .seed
            .expect("the test engine leases a seed");
        let respawned = handler
            .handle(Request::RespawnPane(Box::new(RespawnPaneRequest {
                target: target.clone(),
                kill: true,
                start_directory: Some(seed),
                environment: None,
                command: None,
                process_command: None,
            })))
            .await;
        assert!(
            matches!(respawned, Response::RespawnPane(_)),
            "{respawned:?}"
        );
        handler
            .wait_for_pane_startup_to_finish_for_test(&target)
            .await;

        let (replacement, _, replacement_pane, live) =
            wait_for_pane_job(&io, Some(&retiring.sandbox.uid)).await;
        assert_eq!(
            replacement_pane, pane,
            "a respawn keeps the pane's stable id, which is what makes the generation load-bearing"
        );
        assert_ne!(
            live, superseded,
            "a respawn must reserve the next output generation for its replacement job"
        );

        io.install_route(
            retiring.sandbox.uid.clone(),
            Route::Pane {
                session: route_session.clone(),
                pane,
                generation: superseded,
            },
        );
        publish(&handler, &retiring.sandbox, b"superseded-generation\r\n").await;
        publish(&handler, &replacement.sandbox, b"live-generation\r\n").await;

        let captured = capture(&handler, &target).await;
        assert!(
            captured.contains("live-generation"),
            "the replacement generation must reach the pane it was reserved for: {captured:?}"
        );
        assert!(
            !captured.contains("superseded-generation"),
            "the replaced generation's bytes must be refused by the pane that replaced it: \
             {captured:?}"
        );

        // The control that makes the refusal above mean something. The very same sandbox, routed
        // to the very same pane, differing only in the generation its route carries — so the
        // bytes land. Without this, a route that had simply gone missing would pass the test.
        io.install_route(
            retiring.sandbox.uid.clone(),
            Route::Pane {
                session: route_session,
                pane,
                generation: live,
            },
        );
        publish(&handler, &retiring.sandbox, b"regenerated-route\r\n").await;
        let captured = capture(&handler, &target).await;
        assert!(
            captured.contains("regenerated-route"),
            "only the generation may reject a routed job's bytes, not the route itself: \
             {captured:?}"
        );
    }

    /// Removing one alias of a linked pane keeps its job; removing the last one ends it.
    ///
    /// [`PaneTerminal`](crate::pane_terminal_process::PaneTerminal) deliberately has no `Drop`: a
    /// linked window is one shell presented through several aliases, and the map removal that
    /// detaching an alias performs must not signal the shell every surviving alias is still
    /// showing. `HandlerState::unlink_window` is where that decision lives — `link_count == 1`
    /// kills the window, and the sibling branch transfers the shared runtime to a surviving slot
    /// instead — so both halves are asserted here against the job's own closure watch rather than
    /// against the contents of a map.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_last_alias_of_a_linked_pane_ends_its_job() {
        let handler = RequestHandler::new();
        let owner = create_session(&handler, "linked-alias-owner").await;
        let io = handler
            .shell_io()
            .expect("a unit-test handler binds its own engine");
        // Captured before the alias session exists, so "the shared job" needs no disambiguation.
        let (shared, ..) = wait_for_pane_job(&io, None).await;
        let shared_job = io.shell(&shared.id).expect("the pane's job is live");

        let alias = create_session(&handler, "linked-alias-peer").await;
        // A window of its own, so detaching the shared alias leaves the session standing and this
        // test measures an alias removal rather than a session teardown.
        create_window(&handler, &alias, 1).await;
        link_window(&handler, &owner, &alias).await;

        unlink_window(&handler, &alias, false).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(250), shared_job.wait_closed())
                .await
                .is_err(),
            "removing one alias of a linked pane must not signal the shell the others still show"
        );
        assert_eq!(
            io.job(shared_job.id()).map(|view| view.sandbox.uid),
            Some(shared.sandbox.uid.clone()),
            "the surviving alias must still present the very same job instance"
        );

        unlink_window(&handler, &owner, true).await;

        tokio::time::timeout(Duration::from_secs(10), shared_job.wait_closed())
            .await
            .expect("removing the last alias must end the shared job")
            .expect("the job's closure is observed, not aborted");
    }

    async fn split(handler: &RequestHandler, session: &SessionName) {
        let response = handler
            .handle(Request::SplitWindow(SplitWindowRequest {
                target: SplitWindowTarget::Session(session.clone()),
                direction: SplitDirection::Vertical,
                before: false,
                environment: None,
            }))
            .await;
        assert!(matches!(response, Response::SplitWindow(_)), "{response:?}");
    }

    async fn select_pane(handler: &RequestHandler, session: &SessionName, pane_index: u32) {
        let response = handler
            .handle(Request::SelectPane(Box::new(SelectPaneRequest {
                target: PaneTarget::with_window(session.clone(), 0, pane_index),
                title: None,
                input_disabled: None,
                preserve_zoom: false,
                style: None,
            })))
            .await;
        assert!(matches!(response, Response::SelectPane(_)), "{response:?}");
    }

    /// Which pane rmux calls active, read back through `list-panes` rather than from the state.
    async fn active_pane_index(handler: &RequestHandler, session: &SessionName) -> Option<u32> {
        let response = handler
            .handle(Request::ListPanes(Box::new(ListPanesRequest {
                target: session.clone(),
                target_window_index: Some(0),
                format: Some("#{pane_index}:#{pane_active}".to_owned()),
                filter: None,
                sort_order: None,
                reversed: false,
            })))
            .await;
        let Response::ListPanes(listed) = response else {
            panic!("list-panes failed: {response:?}");
        };
        String::from_utf8_lossy(&listed.output.stdout)
            .lines()
            .find_map(|line| line.strip_suffix(":1").map(str::to_owned))
            .and_then(|index| index.parse().ok())
    }

    /// Waits for rmux's own selection to catch up with the engine's.
    ///
    /// The mux-to-rmux direction is applied on an owned task, off the observation consumer, so it
    /// is something to wait for rather than to read once.
    async fn wait_for_active_pane(handler: &RequestHandler, session: &SessionName, expected: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let active = active_pane_index(handler, session).await;
            if active == Some(expected) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "rmux never selected the pane the engine switched to: active={active:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Selection is one decision, shared by the surface and the engine, in both directions.
    ///
    /// Plan line 347. An rmux `select-pane` commit makes that pane's job the engine's current
    /// terminal, and a native [`ShellIo::switch`](crate::io::ShellIo::switch) selects the pane the
    /// switched-to job is mapped to — resolved through the job's route, never a cached index.
    ///
    /// The last assertions are about the thing that makes two directions safe at all. Each side
    /// records the stable `(SnapshotUid, PaneId)` it is about to install before installing it, so
    /// the change it propagates arrives at the other side already agreed and stops there. Without
    /// that, the native switch below would select a pane, the selection would switch the engine,
    /// and the two would trade the same decision back and forth for as long as the daemon runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn selection_synchronises_between_rmux_and_the_engine_without_bouncing() {
        let handler = RequestHandler::new();
        let session = create_session(&handler, "selection-sync").await;
        split(&handler, &session).await;
        handler
            .wait_for_pane_startup_to_finish_for_test(&PaneTarget::with_window(
                session.clone(),
                0,
                1,
            ))
            .await;
        let io = handler
            .shell_io()
            .expect("a unit-test handler binds its own engine");

        // Whichever pane the split left active, this settles it on pane 0 — so each selection
        // below is a real change rather than a re-selection of the pane already shown.
        select_pane(&handler, &session, 0).await;

        select_pane(&handler, &session, 1).await;
        let second = io
            .current_job()
            .expect("an rmux selection must make the selected pane's job the current terminal");
        let (_, second_pane, _) = io
            .route_for(&second.sandbox)
            .expect("the selected job is routed to a pane");

        select_pane(&handler, &session, 0).await;
        let first = io
            .current_job()
            .expect("an rmux selection must make the selected pane's job the current terminal");
        let (_, first_pane, _) = io
            .route_for(&first.sandbox)
            .expect("the selected job is routed to a pane");
        assert_ne!(
            first_pane, second_pane,
            "each explicit selection must move the engine's current terminal to its own pane"
        );
        assert_eq!(active_pane_index(&handler, &session).await, Some(0));

        // The other direction: nothing touches rmux, only the engine's own selection moves.
        let handle = io
            .shell(&second.id)
            .expect("the other pane's job is still live");
        io.switch(&handle)
            .await
            .expect("a native switch selects a live terminal job");
        wait_for_active_pane(&handler, &session, 1).await;

        // And it settles. A propagated change must not come back as a new one: the identity both
        // sides now agree on is the same stable instance, so neither side has anything left to
        // announce.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            active_pane_index(&handler, &session).await,
            Some(1),
            "the pane the engine selected must stay selected"
        );
        assert_eq!(
            io.current_job().map(|view| view.sandbox.uid),
            Some(second.sandbox.uid),
            "the job rmux selected back must stay the engine's current terminal"
        );
    }
}
