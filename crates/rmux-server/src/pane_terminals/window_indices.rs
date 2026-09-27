use std::collections::BTreeMap;

use rmux_proto::{OptionName, RmuxError, SessionName};

use super::session_mutation::SessionCheckpoint;
use super::{session_not_found, HandlerState};

impl HandlerState {
    pub(in crate::pane_terminals) fn session_base_index(&self, session_name: &SessionName) -> u32 {
        self.options
            .resolve(Some(session_name), OptionName::BaseIndex)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0)
    }

    pub(in crate::pane_terminals) fn renumber_windows_if_enabled(
        &mut self,
        session_name: &SessionName,
    ) -> Result<BTreeMap<u32, u32>, RmuxError> {
        if self
            .options
            .resolve(Some(session_name), OptionName::RenumberWindows)
            != Some("on")
        {
            return Ok(BTreeMap::new());
        }

        self.reindex_windows_from_base(session_name)
    }

    pub(in crate::pane_terminals) fn renumber_transfer_source_sessions(
        &mut self,
        session_names: &[SessionName],
    ) -> Result<(), RmuxError> {
        let mut session_names = session_names.to_vec();
        session_names.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        session_names.dedup();

        for session_name in &session_names {
            if self
                .sessions
                .session(session_name)
                .is_some_and(|session| !session.windows().is_empty())
            {
                let _ = self.renumber_windows_if_enabled(session_name)?;
            }
        }
        for session_name in session_names {
            if self
                .sessions
                .session(&session_name)
                .is_some_and(|session| !session.windows().is_empty())
            {
                self.synchronize_session_group_from(&session_name)?;
                self.sync_pane_lifecycle_dimensions_for_session(&session_name);
            }
        }
        Ok(())
    }

    pub(in crate::pane_terminals) fn reindex_windows_from_base(
        &mut self,
        session_name: &SessionName,
    ) -> Result<BTreeMap<u32, u32>, RmuxError> {
        let base_index = self.session_base_index(session_name);
        let previous_session = self
            .sessions
            .session(session_name)
            .cloned()
            .ok_or_else(|| session_not_found(session_name))?;
        let checkpoint = SessionCheckpoint::capture(self, [(session_name, previous_session)]);

        let session = self
            .sessions
            .session_mut(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let index_map = session.reindex_windows_from(base_index)?;
        if let Err(error) = self.remap_reindexed_window_metadata(session_name, &index_map) {
            checkpoint.restore(self)?;
            return Err(error);
        }
        Ok(index_map)
    }

    pub(in crate::pane_terminals) fn remap_reindexed_window_metadata(
        &mut self,
        session_name: &SessionName,
        index_map: &std::collections::BTreeMap<u32, u32>,
    ) -> Result<(), RmuxError> {
        self.options
            .remap_session_window_indices(session_name, index_map)?;
        self.hooks
            .remap_session_window_indices(session_name, index_map)?;
        self.remap_window_indexed_state(session_name, index_map);
        Ok(())
    }
}

/// Each window slot of `session` paired with the id of the window it holds, in slot order; a
/// linked window's id appears once per slot it occupies.
pub(in crate::pane_terminals) fn window_ids_by_index(
    session: &rmux_core::Session,
) -> BTreeMap<u32, u32> {
    session
        .windows()
        .iter()
        .map(|(index, window)| (*index, window.id().as_u32()))
        .collect()
}
