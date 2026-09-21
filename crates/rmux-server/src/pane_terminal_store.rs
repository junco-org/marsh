use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::BorrowedFd;
#[cfg(unix)]
use std::path::PathBuf;

use rmux_core::PaneId;
use rmux_proto::{PaneTarget, RmuxError, SessionName};
#[cfg(test)]
use rmux_proto::TerminalSize;
#[cfg(unix)]
use tracing::debug;

use crate::io::{ShellHandle, ShellIo};
use crate::pane_terminal_lookup::{missing_pane_terminal, SessionPane};
use crate::pane_terminal_process::{pane_terminal_size, PaneTerminal};
use crate::terminal::{SessionBaseEnvironment, TerminalProfile};

#[derive(Debug, Default)]
pub(super) struct PaneTerminalStore {
    sessions: HashMap<SessionName, HashMap<PaneId, PaneTerminal>>,
    session_environments: HashMap<SessionName, SessionBaseEnvironment>,
    #[cfg(test)]
    fail_next_resizes: usize,
}

impl PaneTerminalStore {
    pub(super) fn contains_session(&self, session_name: &SessionName) -> bool {
        self.sessions.contains_key(session_name)
    }

    pub(super) fn insert_session(
        &mut self,
        session_name: SessionName,
        pane_id: PaneId,
        terminal: PaneTerminal,
    ) -> Result<(), RmuxError> {
        if self.sessions.contains_key(&session_name) {
            return Err(RmuxError::Server(format!(
                "pane terminals already exist for session {session_name}"
            )));
        }

        let environment = SessionBaseEnvironment::from_profile(terminal.profile());
        let previous = self
            .sessions
            .insert(session_name.clone(), HashMap::from([(pane_id, terminal)]));
        debug_assert!(previous.is_none());
        let previous_environment = self.session_environments.insert(session_name, environment);
        debug_assert!(previous_environment.is_none());
        Ok(())
    }

    #[cfg(windows)]
    pub(super) fn insert_pending_session(
        &mut self,
        session_name: SessionName,
        environment: SessionBaseEnvironment,
    ) -> Result<(), RmuxError> {
        if self.sessions.contains_key(&session_name) {
            return Err(RmuxError::Server(format!(
                "pane terminals already exist for session {session_name}"
            )));
        }

        let previous = self.sessions.insert(session_name.clone(), HashMap::new());
        debug_assert!(previous.is_none());
        let previous_environment = self.session_environments.insert(session_name, environment);
        debug_assert!(previous_environment.is_none());
        Ok(())
    }

    pub(super) fn insert_pane(
        &mut self,
        session_name: SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
        terminal: PaneTerminal,
    ) -> Result<(), RmuxError> {
        let session_panes = self.sessions.get_mut(&session_name).ok_or_else(|| {
            RmuxError::Server(format!("missing pane terminals for session {session_name}"))
        })?;

        if session_panes.insert(pane_id, terminal).is_some() {
            return Err(RmuxError::Server(format!(
                "pane terminal already exists for {}:{window_index}.{pane_index}",
                session_name,
            )));
        }

        Ok(())
    }

    pub(super) fn rename_session(
        &mut self,
        session_name: &SessionName,
        new_name: &SessionName,
    ) -> Result<(), RmuxError> {
        if !self.sessions.contains_key(session_name) {
            return Err(missing_session_terminals(session_name));
        }
        if self.sessions.contains_key(new_name) {
            return Err(RmuxError::Server(format!(
                "pane terminals already exist for session {new_name}"
            )));
        }

        let mut sessions = std::mem::take(&mut self.sessions);
        let panes = sessions
            .remove(session_name)
            .expect("prevalidated session terminals must exist");
        let replaced = sessions.insert(new_name.clone(), panes);
        debug_assert!(replaced.is_none());
        self.sessions = sessions;
        if let Some(environment) = self.session_environments.remove(session_name) {
            let previous = self
                .session_environments
                .insert(new_name.clone(), environment);
            debug_assert!(previous.is_none());
        }
        Ok(())
    }

    pub(super) fn resize_session(
        &mut self,
        session_name: &SessionName,
        pane_geometries: &[SessionPane],
    ) -> Result<(), RmuxError> {
        #[cfg(test)]
        if self.fail_next_resizes > 0 {
            self.fail_next_resizes -= 1;
            return Err(RmuxError::Server(
                "injected pane terminal resize failure".to_owned(),
            ));
        }

        let session_panes = self.sessions.get_mut(session_name).ok_or_else(|| {
            RmuxError::Server(format!("missing pane terminals for session {session_name}"))
        })?;

        for pane in pane_geometries {
            let target =
                PaneTarget::with_window(session_name.clone(), pane.window_index, pane.index);
            let terminal = session_panes
                .get_mut(&pane.id)
                .ok_or_else(|| RmuxError::Server(format!("missing pane terminal for {target}")))?;

            terminal.resize(pane_terminal_size(pane.geometry));
        }

        Ok(())
    }

    pub(super) fn ensure_panes_exist(
        &self,
        session_name: &SessionName,
        pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        let session_panes = self.sessions.get(session_name).ok_or_else(|| {
            RmuxError::Server(format!("missing pane terminals for session {session_name}"))
        })?;

        for pane_id in pane_ids {
            if !session_panes.contains_key(pane_id) {
                return Err(RmuxError::Server(format!(
                    "missing pane terminal for pane id {} in session {}",
                    pane_id.as_u32(),
                    session_name
                )));
            }
        }

        Ok(())
    }

    /// A borrowed descriptor on one pane's terminal, for metadata probes only.
    #[cfg(unix)]
    pub(super) fn pane_terminal_fd(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<BorrowedFd<'_>, RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pair = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;

        pair.terminal_fd()
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))
    }

    /// The facade and job one pane's input and control operations go through.
    ///
    /// The handle is generation-bound, so a caller that keeps it across an `await` and acts on it
    /// afterwards reaches this job or nothing — never whichever job later took its name.
    pub(super) fn pane_shell(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<(ShellIo, ShellHandle), RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pair = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;

        Ok((pair.io(), pair.handle().clone()))
    }

    /// The same, refusing a pane whose job has closed.
    ///
    /// The error text is the one every caller of this path has always produced for a pane whose
    /// process is gone; `handler_pane::io_encoding` matches on it.
    pub(super) fn pane_shell_if_alive(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<(ShellIo, ShellHandle), RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pair = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;
        if !pair.is_alive() {
            return Err(RmuxError::Server("target pane has exited".to_owned()));
        }

        Ok((pair.io(), pair.handle().clone()))
    }

    /// The geometry one pane's terminal was last asked for.
    #[cfg(test)]
    pub(super) fn pane_size(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<TerminalSize, RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pair = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;

        Ok(pair.size())
    }

    pub(super) fn pane_is_alive(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Result<bool, RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        Ok(session_panes
            .get(&pane_id)
            .is_some_and(PaneTerminal::is_alive))
    }

    /// The process group in the foreground of one pane's terminal, when there is one.
    ///
    /// `None` for an idle pane: its shell is embedded in this daemon and has no child.
    pub(super) fn pane_pid(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<Option<u32>, RmuxError> {
        let terminal = self
            .sessions
            .get(session_name)
            .and_then(|panes| panes.get(&pane_id))
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;
        Ok(terminal.pid())
    }

    #[cfg(unix)]
    pub(super) fn pane_tty_path(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<PathBuf, RmuxError> {
        let terminal = self
            .sessions
            .get(session_name)
            .and_then(|panes| panes.get(&pane_id))
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;
        terminal
            .tty_path()
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))
    }

    pub(super) fn remove_session(
        &mut self,
        session_name: &SessionName,
    ) -> Option<HashMap<PaneId, PaneTerminal>> {
        let _ = self.session_environments.remove(session_name);
        self.sessions.remove(session_name)
    }

    pub(super) fn remove_pane(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<PaneTerminal> {
        self.sessions
            .get_mut(session_name)
            .and_then(|session_panes| session_panes.remove(&pane_id))
    }

    pub(super) fn remove_pane_batch(
        &mut self,
        session_name: &SessionName,
        pane_ids: &[PaneId],
    ) -> Result<HashMap<PaneId, PaneTerminal>, RmuxError> {
        let session_panes = self
            .sessions
            .get_mut(session_name)
            .ok_or_else(|| missing_session_terminals(session_name))?;
        ensure_session_contains_panes(session_name, session_panes, pane_ids)?;

        Ok(pane_ids
            .iter()
            .copied()
            .map(|pane_id| {
                (
                    pane_id,
                    session_panes
                        .remove(&pane_id)
                        .expect("prevalidated pane terminal must exist"),
                )
            })
            .collect())
    }

    pub(super) fn insert_existing_panes(
        &mut self,
        session_name: &SessionName,
        panes: HashMap<PaneId, PaneTerminal>,
    ) -> Result<(), RmuxError> {
        let session_panes = self
            .sessions
            .get_mut(session_name)
            .ok_or_else(|| missing_session_terminals(session_name))?;
        ensure_session_accepts_panes(session_name, session_panes, panes.keys().copied())?;
        session_panes.extend(panes);
        Ok(())
    }

    pub(super) fn move_panes_between_sessions(
        &mut self,
        source_session: &SessionName,
        destination_session: &SessionName,
        pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session || pane_ids.is_empty() {
            return Ok(());
        }

        let mut source_panes = self
            .sessions
            .remove(source_session)
            .ok_or_else(|| missing_session_terminals(source_session))?;
        let Some(mut destination_panes) = self.sessions.remove(destination_session) else {
            self.sessions.insert(source_session.clone(), source_panes);
            return Err(missing_session_terminals(destination_session));
        };

        let result = (|| -> Result<(), RmuxError> {
            ensure_session_contains_panes(source_session, &source_panes, pane_ids)?;
            ensure_session_accepts_panes(
                destination_session,
                &destination_panes,
                pane_ids.iter().copied(),
            )?;

            for pane_id in pane_ids {
                let terminal = source_panes
                    .remove(pane_id)
                    .expect("prevalidated source pane terminal must exist");
                destination_panes.insert(*pane_id, terminal);
            }

            Ok(())
        })();

        self.sessions.insert(source_session.clone(), source_panes);
        self.sessions
            .insert(destination_session.clone(), destination_panes);
        result
    }

    pub(super) fn swap_panes_between_sessions(
        &mut self,
        source_session: &SessionName,
        source_pane_ids: &[PaneId],
        destination_session: &SessionName,
        destination_pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session {
            return Ok(());
        }

        let mut source_panes = self
            .sessions
            .remove(source_session)
            .ok_or_else(|| missing_session_terminals(source_session))?;
        let Some(mut destination_panes) = self.sessions.remove(destination_session) else {
            self.sessions.insert(source_session.clone(), source_panes);
            return Err(missing_session_terminals(destination_session));
        };

        let result = (|| -> Result<(), RmuxError> {
            ensure_session_contains_panes(source_session, &source_panes, source_pane_ids)?;
            ensure_session_contains_panes(
                destination_session,
                &destination_panes,
                destination_pane_ids,
            )?;

            let removed_source = source_pane_ids
                .iter()
                .copied()
                .map(|pane_id| {
                    (
                        pane_id,
                        source_panes
                            .remove(&pane_id)
                            .expect("prevalidated source pane terminal must exist"),
                    )
                })
                .collect::<HashMap<_, _>>();
            let removed_destination = destination_pane_ids
                .iter()
                .copied()
                .map(|pane_id| {
                    (
                        pane_id,
                        destination_panes
                            .remove(&pane_id)
                            .expect("prevalidated destination pane terminal must exist"),
                    )
                })
                .collect::<HashMap<_, _>>();

            ensure_session_accepts_panes(
                source_session,
                &source_panes,
                removed_destination.keys().copied(),
            )?;
            ensure_session_accepts_panes(
                destination_session,
                &destination_panes,
                removed_source.keys().copied(),
            )?;

            source_panes.extend(removed_destination);
            destination_panes.extend(removed_source);
            Ok(())
        })();

        self.sessions.insert(source_session.clone(), source_panes);
        self.sessions
            .insert(destination_session.clone(), destination_panes);
        result
    }

    pub(super) fn pane_profile(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<&TerminalProfile, RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pane = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;

        Ok(pane.profile())
    }

    pub(super) fn session_base_environment(
        &self,
        session_name: &SessionName,
    ) -> Option<SessionBaseEnvironment> {
        self.session_environments.get(session_name).cloned()
    }

    pub(super) fn pane_base_environment(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<SessionBaseEnvironment> {
        self.sessions
            .get(session_name)?
            .get(&pane_id)
            .map(|pane| SessionBaseEnvironment::from_profile(pane.profile()))
    }

    pub(super) fn pane_runtime_window_name(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
        window_index: u32,
        pane_index: u32,
    ) -> Result<Option<&str>, RmuxError> {
        let session_panes = self
            .sessions
            .get(session_name)
            .ok_or_else(|| RmuxError::SessionNotFound(session_name.to_string()))?;
        let pane = session_panes
            .get(&pane_id)
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))?;

        Ok(pane.runtime_window_name())
    }

    /// Resumes every pane whose command is stopped.
    ///
    /// Follows engine state rather than reaping anything: the request goes to the running
    /// command's own process groups, and a pane with no running command has nothing to resume.
    #[cfg(unix)]
    pub(super) fn continue_stopped_panes(&self) {
        for (session_name, panes) in &self.sessions {
            for (pane_id, terminal) in panes {
                if terminal.continue_if_stopped() {
                    debug!(
                        session = %session_name,
                        pane_id = pane_id.as_u32(),
                        pid = terminal.pid(),
                        "continued stopped pane process group"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn fail_resizes_for_test(&mut self, count: usize) {
        self.fail_next_resizes = count;
    }
}

fn missing_session_terminals(session_name: &SessionName) -> RmuxError {
    RmuxError::Server(format!("missing pane terminals for session {session_name}"))
}

fn ensure_session_contains_panes(
    session_name: &SessionName,
    session_panes: &HashMap<PaneId, PaneTerminal>,
    pane_ids: &[PaneId],
) -> Result<(), RmuxError> {
    for pane_id in pane_ids {
        if !session_panes.contains_key(pane_id) {
            return Err(RmuxError::Server(format!(
                "missing pane terminal for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }
    }

    Ok(())
}

fn ensure_session_accepts_panes<I>(
    session_name: &SessionName,
    session_panes: &HashMap<PaneId, PaneTerminal>,
    pane_ids: I,
) -> Result<(), RmuxError>
where
    I: IntoIterator<Item = PaneId>,
{
    for pane_id in pane_ids {
        if session_panes.contains_key(&pane_id) {
            return Err(RmuxError::Server(format!(
                "pane terminal already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }
    }

    Ok(())
}
