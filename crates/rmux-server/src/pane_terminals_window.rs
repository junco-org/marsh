use rmux_core::{OptionStore, Session};
use rmux_proto::{
    KillWindowResponse, LastWindowResponse, NewWindowResponse, NextWindowResponse, OptionName,
    PaneId, PreviousWindowResponse, RenameWindowResponse, RmuxError, ScopeSelector,
    SelectWindowResponse, SessionName, SetOptionMode, WindowTarget,
};
use std::collections::HashSet;

#[path = "pane_terminals/window_link_commands.rs"]
mod window_link_commands;
#[path = "pane_terminals/window_movement.rs"]
mod window_movement;

use super::{
    session_not_found, HandlerState, KilledWindowResult, NewWindowOptions, PreparedWindowTerminal,
    RemovedWindowHookContext, RespawnWindowOptions, SessionTransferSnapshot, WindowSpawnOptions,
};
use crate::terminal::validate_process_command;

#[path = "pane_terminals/window_removal.rs"]
mod window_removal;

use window_removal::build_window_removal_plan;
pub(super) use window_removal::window_pane_ids;

pub(crate) struct RespawnWindowResult {
    pub(crate) response: rmux_proto::RespawnWindowResponse,
    pub(crate) retained_pane_id: PaneId,
    pub(crate) removed_pane_ids: Vec<PaneId>,
    pub(crate) refresh_sessions: Vec<SessionName>,
}

impl HandlerState {
    /// Plans a new window: applies it to the session store and plans its pane's terminal.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::plan_window_at_requested_index`] fails.
    pub(crate) fn plan_window(
        &mut self,
        session_name: &SessionName,
        options: NewWindowOptions<'_>,
    ) -> Result<PlannedNewWindow, RmuxError> {
        self.plan_window_at_requested_index(session_name, None, false, options)
    }

    /// Plans a new window at a requested index.
    ///
    /// The session mutation happens here, under the caller's lock, and [`NewWindowCommit`] keeps
    /// what is needed to undo it: the window's pane terminal is opened afterwards with the lock
    /// released, and a spawn that fails must leave no empty window behind.
    ///
    /// # Errors
    ///
    /// Fails when the session is missing, when one of its windows has no pane runtime, when the
    /// requested index is taken, and when the pane's profile cannot be resolved. The session
    /// store is restored before returning in every case that had already mutated it.
    pub(crate) fn plan_window_at_requested_index(
        &mut self,
        session_name: &SessionName,
        target_window_index: Option<u32>,
        insert_at_target: bool,
        options: NewWindowOptions<'_>,
    ) -> Result<PlannedNewWindow, RmuxError> {
        let NewWindowOptions {
            name,
            detached,
            spawn,
        } = options;
        let explicit_name = name.is_some();
        let previous_session = self
            .sessions
            .session(session_name)
            .cloned()
            .ok_or_else(|| session_not_found(session_name))?;
        ensure_session_panes_exist(self, session_name, &previous_session)?;
        let size = previous_session.window().size();

        let base_index = self
            .options
            .resolve(Some(session_name), OptionName::BaseIndex)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let mutation_snapshot = SessionTransferSnapshot::capture(self);
        let pane_id = self.sessions.allocate_pane_id();
        let session_mutation = (|| -> Result<_, RmuxError> {
            let session = self
                .sessions
                .session_mut(session_name)
                .ok_or_else(|| session_not_found(session_name))?;
            let (window_index, pane_id, index_map) = match target_window_index {
                Some(window_index) => {
                    let index_map = if insert_at_target {
                        session.make_room_for_window(window_index)?
                    } else if session.window_at(window_index).is_some() {
                        return Err(RmuxError::Server(format!(
                            "create window failed: index {window_index} in use"
                        )));
                    } else {
                        std::collections::BTreeMap::new()
                    };
                    session.insert_window_with_initial_pane_with_id(window_index, size, pane_id)?;
                    (window_index, pane_id, index_map)
                }
                None => {
                    let (window_index, pane_id) = session
                        .create_window_at_or_above_with_pane_id(size, base_index, pane_id)?;
                    (window_index, pane_id, std::collections::BTreeMap::new())
                }
            };
            if let Some(name) = name {
                session.rename_window(window_index, name)?;
            }
            if !detached {
                session.select_window(window_index)?;
            }
            Ok((window_index, pane_id, index_map))
        })();
        let (window_index, pane_id, index_map) = match session_mutation {
            Ok(result) => result,
            Err(error) => {
                mutation_snapshot.restore(self);
                return Err(error);
            }
        };

        if let Err(error) = self.remap_session_group_window_metadata(session_name, &index_map) {
            mutation_snapshot.restore(self);
            return Err(error);
        }

        let planned = match self.plan_window_terminal_at(session_name, window_index, spawn) {
            Ok(planned) => planned,
            Err(error) => {
                mutation_snapshot.restore(self);
                return Err(error);
            }
        };

        Ok(PlannedNewWindow {
            planned,
            commit: NewWindowCommit {
                session_name: session_name.clone(),
                window_index,
                pane_id,
                explicit_name,
                index_map,
                mutation_snapshot,
            },
        })
    }

    /// Undoes a planned window whose pane's job never opened.
    pub(crate) fn roll_back_planned_window(&mut self, commit: NewWindowCommit) {
        commit.mutation_snapshot.restore(self);
    }

    /// Installs a planned window's pane terminal and finishes the creation.
    ///
    /// # Errors
    ///
    /// Fails when the pane was removed or superseded while its job was opening, and for the
    /// reasons the automatic-rename, group-synchronization and terminal installation steps fail.
    /// The window is removed again in the first case; the others leave rmux's own diagnostics.
    pub(crate) fn commit_planned_window(
        &mut self,
        commit: NewWindowCommit,
        prepared: PreparedWindowTerminal,
        terminal_commit: super::WindowTerminalCommit,
    ) -> Result<NewWindowResponse, RmuxError> {
        let NewWindowCommit {
            session_name,
            window_index,
            pane_id,
            explicit_name,
            index_map,
            mutation_snapshot,
        } = commit;
        if let Err(error) = self.commit_window_terminal(terminal_commit, prepared) {
            mutation_snapshot.restore(self);
            return Err(error);
        }
        let target = WindowTarget::with_window(session_name.clone(), window_index);
        if explicit_name {
            self.disable_automatic_rename_for_window(&target)?;
        }

        debug_assert_eq!(
            self.sessions
                .session(&session_name)
                .and_then(|session| session.pane_id_in_window(window_index, 0)),
            Some(pane_id)
        );
        self.synchronize_session_group_from_with_window_selection_map(&session_name, &index_map)?;
        self.sync_pane_lifecycle_dimensions_for_session(&session_name);

        Ok(NewWindowResponse { target })
    }

    pub(crate) fn kill_window(
        &mut self,
        target: WindowTarget,
        kill_others: bool,
    ) -> Result<KilledWindowResult, RmuxError> {
        let session_name = target.session_name().clone();
        let target_index = target.window_index();
        let (removal_plan, removed_windows) = {
            let session = self
                .sessions
                .session(&session_name)
                .ok_or_else(|| session_not_found(&session_name))?;
            let removal_plan =
                build_window_removal_plan(self, session, &session_name, target_index, kill_others)?;
            let removed_windows = removal_plan
                .iter()
                .map(|planned_window| {
                    let window = self
                        .sessions
                        .session(&planned_window.session_name)
                        .and_then(|session| session.window_at(planned_window.window_index))
                        .ok_or_else(|| {
                            RmuxError::invalid_target(
                                format!(
                                    "{}:{}",
                                    planned_window.session_name, planned_window.window_index
                                ),
                                "window index does not exist in session",
                            )
                        })?;
                    Ok(RemovedWindowHookContext {
                        target: WindowTarget::with_window(
                            planned_window.session_name.clone(),
                            planned_window.window_index,
                        ),
                        window_id: window.id().as_u32(),
                        window_name: window.name().unwrap_or_default().to_owned(),
                    })
                })
                .collect::<Result<Vec<_>, RmuxError>>()?;
            (removal_plan, removed_windows)
        };
        let removed_pane_ids = removal_plan
            .iter()
            .flat_map(|planned_window| planned_window.pane_ids.iter().copied())
            .collect::<Vec<_>>();

        let mut removals_by_session = std::collections::HashMap::<SessionName, usize>::new();
        for planned_window in &removal_plan {
            *removals_by_session
                .entry(planned_window.session_name.clone())
                .or_default() += 1;
        }
        let mut destroyed_session_names = removal_plan
            .iter()
            .filter_map(|planned_window| {
                let session_name = &planned_window.session_name;
                let removed_count = removals_by_session.get(session_name).copied()?;
                self.sessions
                    .session(session_name)
                    .is_some_and(|session| session.windows().len() == removed_count)
                    .then(|| session_name.clone())
            })
            .collect::<Vec<_>>();
        let mut seen_destroyed_sessions = HashSet::new();
        destroyed_session_names
            .retain(|session_name| seen_destroyed_sessions.insert(session_name.clone()));
        let removed_window_ids = removed_windows
            .iter()
            .map(|removed_window| rmux_proto::WindowId::new(removed_window.window_id))
            .collect::<Vec<_>>();

        let sessions_to_synchronize = removal_plan
            .iter()
            .map(|planned_window| planned_window.session_name.clone())
            .collect::<HashSet<_>>();
        let mut removed_terminals = HashSet::new();
        for planned_window in removal_plan {
            let planned_target = WindowTarget::with_window(
                planned_window.session_name.clone(),
                planned_window.window_index,
            );
            let _removed_window = self
                .sessions
                .session_mut(&planned_window.session_name)
                .ok_or_else(|| session_not_found(&planned_window.session_name))?
                .remove_window_allowing_empty(planned_window.window_index)?;
            let _ = self.options.remove_window(&planned_target);
            let _ = self.hooks.remove_window(&planned_target);
            self.clear_auto_named_window(&planned_window.session_name, planned_window.window_index);
            let _ = self
                .detach_window_link_slot(&planned_window.session_name, planned_window.window_index);

            for pane_id in planned_window.pane_ids {
                if !removed_terminals.insert((planned_window.runtime_session_name.clone(), pane_id))
                {
                    continue;
                }
                if !self.remove_pane_terminal_from_runtime(
                    &planned_window.runtime_session_name,
                    pane_id,
                ) {
                    return Err(RmuxError::Server(format!(
                        "missing pane terminal for pane id {} in session {}",
                        pane_id.as_u32(),
                        planned_window.runtime_session_name
                    )));
                }
            }
        }

        let mut destroyed_sessions = Vec::new();
        let mut session_removal_order = destroyed_session_names.clone();
        session_removal_order.sort_by(|left, right| {
            let left_is_owner = self.sessions.runtime_owner(left).as_ref() == Some(left);
            let right_is_owner = self.sessions.runtime_owner(right).as_ref() == Some(right);
            left_is_owner
                .cmp(&right_is_owner)
                .then_with(|| left.as_str().cmp(right.as_str()))
        });
        for destroyed_session_name in session_removal_order {
            let current_runtime_owner = self.sessions.runtime_owner(&destroyed_session_name);
            let next_runtime_owner = self
                .sessions
                .runtime_owner_transfer_target(&destroyed_session_name);
            let removed_session = self.sessions.remove_session(&destroyed_session_name)?;
            destroyed_sessions.push((destroyed_session_name.clone(), removed_session.id()));
            let _ = self.options.remove_session(&destroyed_session_name);
            let _ = self.environment.remove_session(&destroyed_session_name);
            self.remove_session_terminals(
                &destroyed_session_name,
                current_runtime_owner.as_ref(),
                next_runtime_owner.as_ref(),
            )?;
        }
        destroyed_sessions.sort_by_key(|(destroyed_session_name, _)| {
            destroyed_session_names
                .iter()
                .position(|candidate| candidate == destroyed_session_name)
                .unwrap_or(usize::MAX)
        });

        let mut reindexed_windows = Vec::new();
        for synchronized_session in &sessions_to_synchronize {
            if destroyed_session_names.contains(synchronized_session) {
                continue;
            }
            let index_map = self.renumber_windows_if_enabled(synchronized_session)?;
            if !index_map.is_empty() {
                reindexed_windows.push((synchronized_session.clone(), index_map));
            }
        }
        let active_window = self
            .sessions
            .session(&session_name)
            .map_or(target_index, Session::active_window_index);
        for synchronized_session in sessions_to_synchronize {
            if destroyed_session_names.contains(&synchronized_session) {
                continue;
            }
            self.synchronize_session_group_from(&synchronized_session)?;
        }
        let removed_pane_ids = self.pane_ids_no_longer_referenced(removed_pane_ids);

        Ok(KilledWindowResult {
            response: KillWindowResponse {
                target: WindowTarget::with_window(session_name, active_window),
            },
            removed_windows,
            removed_pane_ids,
            destroyed_sessions,
            removed_window_ids,
            reindexed_windows,
        })
    }

    pub(crate) fn select_window(
        &mut self,
        target: WindowTarget,
    ) -> Result<SelectWindowResponse, RmuxError> {
        let session = self
            .sessions
            .session_mut(target.session_name())
            .ok_or_else(|| session_not_found(target.session_name()))?;
        // Session::select_window already clears alert flags on the newly-selected window.
        session.select_window(target.window_index())?;

        Ok(SelectWindowResponse { target })
    }

    pub(crate) fn rename_window(
        &mut self,
        target: WindowTarget,
        new_name: String,
    ) -> Result<RenameWindowResponse, RmuxError> {
        {
            let session = self
                .sessions
                .session_mut(target.session_name())
                .ok_or_else(|| session_not_found(target.session_name()))?;
            session.rename_window(target.window_index(), new_name)?;
        }
        self.disable_automatic_rename_for_window(&target)?;
        self.synchronize_linked_window_options_from_slot(
            target.session_name(),
            target.window_index(),
        );
        self.clear_auto_named_window_family(target.session_name(), target.window_index());
        self.synchronize_window_alias_family_from_slot(
            target.session_name(),
            target.window_index(),
        )?;

        Ok(RenameWindowResponse { target })
    }

    pub(crate) fn disable_automatic_rename_for_window(
        &mut self,
        target: &WindowTarget,
    ) -> Result<(), RmuxError> {
        self.options.set(
            ScopeSelector::Window(target.clone()),
            OptionName::AutomaticRename,
            "off".to_owned(),
            SetOptionMode::Replace,
        )?;
        Ok(())
    }

    pub(crate) fn next_window(
        &mut self,
        session_name: &SessionName,
        alerts_only: bool,
    ) -> Result<NextWindowResponse, RmuxError> {
        let session = self
            .sessions
            .session_mut(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let window_index = if alerts_only {
            session.next_window_with_alerts()?
        } else {
            session.next_window()?
        };

        Ok(NextWindowResponse {
            target: WindowTarget::with_window(session_name.clone(), window_index),
        })
    }

    pub(crate) fn previous_window(
        &mut self,
        session_name: &SessionName,
        alerts_only: bool,
    ) -> Result<PreviousWindowResponse, RmuxError> {
        let session = self
            .sessions
            .session_mut(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let window_index = if alerts_only {
            session.previous_window_with_alerts()?
        } else {
            session.previous_window()?
        };

        Ok(PreviousWindowResponse {
            target: WindowTarget::with_window(session_name.clone(), window_index),
        })
    }

    pub(crate) fn last_window(
        &mut self,
        session_name: &SessionName,
    ) -> Result<LastWindowResponse, RmuxError> {
        let session = self
            .sessions
            .session_mut(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let window_index = session.last_window()?;

        Ok(LastWindowResponse {
            target: WindowTarget::with_window(session_name.clone(), window_index),
        })
    }

    pub(crate) fn resize_window(
        &mut self,
        request: rmux_proto::ResizeWindowRequest,
    ) -> Result<rmux_proto::ResizeWindowResponse, RmuxError> {
        let session_name = request.target.session_name().clone();
        let window_index = request.target.window_index();

        let response = self.mutate_session_and_resize_window_terminal(
            &session_name,
            window_index,
            |session| {
                let current_size = session
                    .window_at(window_index)
                    .ok_or_else(|| {
                        RmuxError::invalid_target(
                            format!("{session_name}:{window_index}"),
                            "window index does not exist in session",
                        )
                    })?
                    .size();

                let mut sx = current_size.cols;
                let mut sy = current_size.rows;

                if let Some(width) = request.width {
                    sx = width;
                }
                if let Some(height) = request.height {
                    sy = height;
                }

                if let Some(adjustment) = request.adjustment {
                    use rmux_proto::ResizeWindowAdjustment;
                    match adjustment {
                        ResizeWindowAdjustment::Left(amount) => {
                            sx = sx.saturating_sub(amount);
                        }
                        ResizeWindowAdjustment::Right(amount) => {
                            sx = sx.saturating_add(amount);
                        }
                        ResizeWindowAdjustment::Up(amount) => {
                            sy = sy.saturating_sub(amount);
                        }
                        ResizeWindowAdjustment::Down(amount) => {
                            sy = sy.saturating_add(amount);
                        }
                        ResizeWindowAdjustment::LargestLinkedSession
                        | ResizeWindowAdjustment::SmallestLinkedSession => {}
                    }
                }

                sx = sx.max(1);
                sy = sy.max(1);

                session.resize_window(
                    window_index,
                    rmux_proto::TerminalSize { cols: sx, rows: sy },
                )?;

                Ok(rmux_proto::ResizeWindowResponse {
                    target: request.target.clone(),
                })
            },
        )?;
        self.options.set(
            ScopeSelector::Window(request.target.clone()),
            OptionName::WindowSize,
            "manual".to_owned(),
            SetOptionMode::Replace,
        )?;
        self.synchronize_linked_window_options_from_slot(&session_name, window_index);
        Ok(response)
    }

    /// Plans a window respawn against a preview of the layout it will produce.
    ///
    /// Nothing is torn down here and no old runtime state is touched: the replacement's profile
    /// is resolved against a cloned session, so every fallible step before the job opens is a
    /// true no-op on the window the user is still looking at.
    ///
    /// # Errors
    ///
    /// Fails when the target does not resolve, when a pane is still active and `-k` was not
    /// given, and when the replacement profile cannot be resolved.
    pub(crate) fn plan_window_respawn(
        &mut self,
        target: WindowTarget,
        options: RespawnWindowOptions<'_>,
    ) -> Result<PlannedWindowRespawn, RmuxError> {
        let RespawnWindowOptions { kill, spawn } = options;
        let session_name = target.session_name().clone();
        let window_index = target.window_index();

        let previous_session = self
            .sessions
            .session(&session_name)
            .cloned()
            .ok_or_else(|| session_not_found(&session_name))?;
        let pane_ids = window_pane_ids(&previous_session, &session_name, window_index)?;

        // Without -k, reject if any pane terminal is still present (i.e. process may be running).
        if !kill
            && pane_ids.iter().any(|id| {
                self.ensure_window_panes_exist(&session_name, window_index, &[*id])
                    .is_ok()
            })
        {
            return Err(RmuxError::Server(
                "window still active; use -k to force respawn".to_owned(),
            ));
        }

        let pane_id = pane_ids
            .first()
            .copied()
            .ok_or_else(|| RmuxError::Server("window has no panes".to_owned()))?;
        let provenance = self.pane_respawn_provenance(pane_id);
        let process_command = spawn.command.cloned().or_else(|| {
            provenance
                .as_ref()
                .and_then(|provenance| provenance.process_command.clone())
        });
        validate_process_command(process_command.as_ref())?;
        // The caller's directory, then the retiring pane's provenance: a respawn with no `-c`
        // restarts where its pane actually was, and `plan_pane_respawn` completes the chain with
        // this host's default when neither answered.
        let start_directory = spawn
            .start_directory
            .filter(|path| !path.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                provenance
                    .as_ref()
                    .and_then(|provenance| provenance.working_directory.clone())
            });
        let respawn_environment = provenance
            .as_ref()
            .map(|provenance| provenance.private_environment.clone());
        let respawn_shell = provenance.as_ref().map(|provenance| &provenance.shell);
        let environment_overrides = spawn
            .environment_overrides
            .map(<[String]>::to_vec)
            .or_else(|| respawn_environment.clone());
        let spawn = WindowSpawnOptions {
            start_directory: start_directory.as_deref(),
            command: process_command.as_ref(),
            socket_path: spawn.socket_path,
            spawn_environment: spawn.spawn_environment,
            environment_overrides: environment_overrides.as_deref(),
            respawn_shell,
            respawn_environment: respawn_environment.as_deref(),
            shell_id: None,
            follow_mux_lifetime: false,
        };
        let removed_pane_ids = pane_ids
            .iter()
            .copied()
            .filter(|id| *id != pane_id)
            .collect::<Vec<_>>();
        let runtime_session_name =
            self.runtime_session_name_for_window(&session_name, window_index);
        let base_environment =
            self.session_base_environment_for_window(&session_name, window_index);
        let mut pane_option_sessions =
            self.window_linked_session_family_list(&session_name, window_index);
        if pane_option_sessions.is_empty() {
            pane_option_sessions.push(session_name.clone());
        }
        pane_option_sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        pane_option_sessions.dedup();
        let before_pane_options = pane_option_sessions
            .into_iter()
            .map(|affected_session| {
                self.pane_option_slots_for_session(&affected_session)
                    .map(|snapshot| (affected_session, snapshot))
            })
            .collect::<Result<Vec<_>, RmuxError>>()?;

        let mut respawned_session = previous_session.clone();
        respawned_session.respawn_window_with_pane_id(window_index, pane_id)?;
        respawned_session.select_window(window_index)?;
        let planned = self.plan_window_terminal(
            &respawned_session,
            window_index,
            spawn,
            base_environment.as_ref(),
        )?;
        let automatic_name_applied = apply_prepared_automatic_window_name(
            &self.options,
            self.tracks_auto_named_window(&session_name, window_index),
            &mut respawned_session,
            window_index,
            &planned,
        );

        Ok(PlannedWindowRespawn {
            planned,
            commit: WindowRespawnCommit {
                target,
                session_name,
                runtime_session_name,
                window_index,
                pane_id,
                pane_ids,
                removed_pane_ids,
                previous_session,
                respawned_session,
                automatic_name_applied,
                before_pane_options,
            },
        })
    }

    /// Replaces a window's runtime with the terminal that was opened for it.
    ///
    /// The identity check happens *before* anything is removed, unlike the creation path where
    /// there is nothing to remove. The teardown below is driven by the pane ids the plan saw, and
    /// a window that was killed or respawned by someone else while this replacement was opening
    /// no longer owns them: discovering that only after removing the terminals would take down
    /// whatever had taken their place and then "restore" a layout that had already been replaced.
    ///
    /// # Errors
    ///
    /// Fails when the pane was removed or superseded while the replacement was opening, and for
    /// the reasons the session replacement, alias synchronization and option rekeying fail. The
    /// previous layout and runtime are restored when the installation itself is refused.
    pub(crate) fn commit_window_respawn(
        &mut self,
        commit: WindowRespawnCommit,
        prepared: PreparedWindowTerminal,
    ) -> Result<RespawnWindowResult, RmuxError> {
        let WindowRespawnCommit {
            target,
            session_name,
            runtime_session_name,
            window_index,
            pane_id,
            pane_ids,
            removed_pane_ids,
            previous_session,
            respawned_session,
            automatic_name_applied,
            before_pane_options,
        } = commit;

        if let Err(error) =
            self.check_pane_commit_identity(&runtime_session_name, pane_id, prepared.generation())
        {
            prepared.abandon();
            return Err(error);
        }

        let mut removed_terminals = pane_ids
            .iter()
            .filter_map(|pane_id| {
                self.terminals
                    .remove_pane(&runtime_session_name, *pane_id)
                    .map(|terminal| (*pane_id, terminal))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let removed_outputs = self.remove_pane_outputs(&runtime_session_name, &pane_ids);
        // Keep the output channel for the stable pane identity. Existing SDK subscribers own
        // receivers for this sender; replacing the channel would leave a registry record that can
        // never observe the respawned process. The generation the replacement was opened with
        // rejects late output from the retiring job while preserving the receiver identity.
        let retained_output_sender = removed_outputs.pane_output_sender(pane_id);
        // The reservation belongs to that same surviving identity, so it survives the removal
        // with it. Letting it go would leave this pane with no recorded generation at all, and
        // the installation's own identity check — which reads exactly this number — would then
        // refuse the replacement this commit had just opened for it.
        if let Some(generation) = removed_outputs.pane_output_generation(pane_id) {
            self.seed_pane_output_generation(&runtime_session_name, pane_id, generation);
        }
        self.replace_session(&session_name, respawned_session)?;

        if let Err(error) = self.install_prepared_window_terminal(
            &runtime_session_name,
            window_index,
            prepared,
            retained_output_sender,
        ) {
            self.replace_session(&session_name, previous_session)?;
            self.terminals
                .insert_existing_panes(&runtime_session_name, removed_terminals)?;
            self.insert_existing_pane_outputs(&runtime_session_name, removed_outputs);
            return Err(error);
        }

        for old_pane_id in &pane_ids {
            if let Some(pipe) = self.remove_pane_pipe(&runtime_session_name, *old_pane_id) {
                pipe.stop();
            }
        }
        for removed_pane_id in &removed_pane_ids {
            self.clear_marked_pane_if_id(*removed_pane_id);
        }
        self.remove_pane_lifecycles(&removed_pane_ids);
        if automatic_name_applied {
            self.mark_auto_named_window(&session_name, window_index);
        }
        super::terminate_removed_terminals(&mut removed_terminals);

        // The window model is shared through explicit link aliases as well as
        // session groups. Synchronize every alias before deciding which pane
        // identities disappeared; otherwise a linked slot can keep destroyed
        // sibling panes artificially reachable after the runtime was replaced.
        let mut synchronized_sessions =
            self.synchronize_linked_window_family_from_slot(&session_name, window_index)?;
        synchronized_sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        synchronized_sessions.dedup();
        for synchronized_session in &synchronized_sessions {
            self.sync_pane_lifecycle_dimensions_for_session(synchronized_session);
        }
        for (affected_session, before) in before_pane_options {
            self.rekey_pane_options_after_session_change(&before, &affected_session)?;
        }
        let removed_pane_ids = self.pane_ids_no_longer_referenced(removed_pane_ids);

        Ok(RespawnWindowResult {
            response: rmux_proto::RespawnWindowResponse { target },
            retained_pane_id: pane_id,
            removed_pane_ids,
            refresh_sessions: synchronized_sessions,
        })
    }
}

fn apply_prepared_automatic_window_name(
    options: &OptionStore,
    tracked: bool,
    session: &mut Session,
    window_index: u32,
    prepared: &super::PlannedPaneTerminal,
) -> bool {
    let Some(name) = prepared.automatic_window_name() else {
        return false;
    };
    let session_name = session.name().clone();
    let should_apply = session.window_at(window_index).is_some_and(|window| {
        window.name().is_none()
            && crate::automatic_rename::window_allows_automatic_rename(
                options,
                &session_name,
                window_index,
                window,
                tracked,
            )
    });
    if should_apply {
        session
            .window_at_mut(window_index)
            .expect("prevalidated respawn window exists")
            .set_automatic_name(name.to_owned());
    }
    should_apply
}

fn link_window_destination_index(
    session: &Session,
    target_window_index: u32,
    after: bool,
    before: bool,
) -> Result<u32, RmuxError> {
    if !(after || before) {
        return Ok(target_window_index);
    }

    if session.window_at(target_window_index).is_none() {
        return Err(RmuxError::invalid_target(
            format!("{}:{target_window_index}", session.name()),
            "window index does not exist in session",
        ));
    }

    if before {
        Ok(target_window_index)
    } else {
        target_window_index.checked_add(1).ok_or_else(|| {
            RmuxError::Server(format!(
                "window index space exhausted for session {}",
                session.name()
            ))
        })
    }
}

fn request_target_string(target: &rmux_proto::MoveWindowTarget) -> String {
    match target {
        rmux_proto::MoveWindowTarget::Session(session_name) => session_name.to_string(),
        rmux_proto::MoveWindowTarget::Window(target) => target.to_string(),
    }
}

fn ensure_session_panes_exist(
    state: &HandlerState,
    session_name: &SessionName,
    session: &Session,
) -> Result<(), RmuxError> {
    for (window_index, window) in session.windows() {
        let pane_ids = window
            .panes()
            .iter()
            .map(|pane| pane.id())
            .collect::<Vec<_>>();
        if !pane_ids.is_empty() {
            state.ensure_window_panes_exist(session_name, *window_index, &pane_ids)?;
        }
    }
    Ok(())
}

/// A new window applied to the session store, with its pane's job not yet opened.
pub(crate) struct PlannedNewWindow {
    planned: super::PlannedWindowTerminal,
    commit: NewWindowCommit,
}

/// What a new window still needs once its job is open, and what it needs to undo if it is not.
pub(crate) struct NewWindowCommit {
    session_name: SessionName,
    window_index: u32,
    pane_id: rmux_core::PaneId,
    /// Whether the caller named the window, which disables automatic renaming for it.
    explicit_name: bool,
    /// The window reindexing this creation caused, replayed into the session group.
    index_map: std::collections::BTreeMap<u32, u32>,
    /// The layout to restore if the pane's job never arrives.
    mutation_snapshot: SessionTransferSnapshot,
}

impl PlannedNewWindow {
    /// Opens the new window's pane job. The handler's state lock must not be held.
    ///
    /// On failure the commit is handed back so the caller can restore the layout this creation
    /// has already applied; returning only the error would leave an empty window on screen.
    ///
    /// # Errors
    ///
    /// Fails for the reasons opening a pane terminal fails.
    pub(crate) async fn open(
        self,
    ) -> Result<
        (
            NewWindowCommit,
            super::WindowTerminalCommit,
            PreparedWindowTerminal,
        ),
        (NewWindowCommit, RmuxError),
    > {
        match self.planned.open().await {
            Ok((terminal_commit, prepared)) => Ok((self.commit, terminal_commit, prepared)),
            Err(error) => Err((self.commit, error)),
        }
    }
}

/// A window respawn decided against a preview, with its replacement job not yet opened.
pub(crate) struct PlannedWindowRespawn {
    planned: super::PlannedPaneTerminal,
    commit: WindowRespawnCommit,
}

/// What a window respawn still needs once its replacement job is open.
pub(crate) struct WindowRespawnCommit {
    target: WindowTarget,
    session_name: SessionName,
    runtime_session_name: SessionName,
    window_index: u32,
    /// The pane identity the window keeps across the respawn.
    pane_id: rmux_core::PaneId,
    /// Every pane the window had, all of whose terminals are replaced by one.
    pane_ids: Vec<rmux_core::PaneId>,
    /// The panes that disappear because the window collapses to a single pane.
    removed_pane_ids: Vec<rmux_core::PaneId>,
    /// The layout to restore if the replacement cannot be installed.
    previous_session: Session,
    /// The layout the respawn produces, applied only once the replacement is in hand.
    respawned_session: Session,
    /// Whether the preview's automatic name was applied, for the auto-name tracking flag.
    automatic_name_applied: bool,
    /// Pane-option snapshots to rekey against, per affected session.
    before_pane_options: Vec<(SessionName, super::pane_option_rekey::PaneSlotSnapshot)>,
}

impl PlannedWindowRespawn {
    /// Opens the replacement job. The handler's state lock must not be held.
    ///
    /// # Errors
    ///
    /// Fails for the reasons opening a pane terminal fails. Nothing has been torn down, so the
    /// window being respawned is still intact.
    pub(crate) async fn open(
        self,
    ) -> Result<(WindowRespawnCommit, PreparedWindowTerminal), RmuxError> {
        let prepared = self.planned.open().await?;
        Ok((self.commit, prepared))
    }
}
