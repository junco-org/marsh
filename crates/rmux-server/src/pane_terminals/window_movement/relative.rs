use std::collections::BTreeMap;

use rmux_core::Session;
use rmux_proto::{
    MoveWindowRequest, MoveWindowResponse, MoveWindowTarget, RmuxError, WindowTarget,
};

use super::{
    ensure_session_panes_exist, link_window_destination_index, session_not_found, HandlerState,
};
use crate::pane_terminals::session_mutation::SessionCheckpoint;

impl HandlerState {
    pub(super) fn move_window_relative(
        &mut self,
        request: MoveWindowRequest,
    ) -> Result<MoveWindowResponse, RmuxError> {
        if request.renumber {
            return Err(RmuxError::Server(
                "move-window -r does not accept -a or -b".to_owned(),
            ));
        }
        let source = request.source.ok_or_else(|| {
            RmuxError::Server("move-window -a/-b requires a source window".to_owned())
        })?;
        let MoveWindowTarget::Window(target) = request.target else {
            return Err(RmuxError::invalid_target(
                source.session_name().to_string(),
                "move-window -a/-b requires a destination window target",
            ));
        };

        if source.session_name() == target.session_name() {
            return self.move_window_relative_within_session(
                source,
                target,
                request.after,
                request.before,
                request.detached,
            );
        }

        self.move_window_relative_across_sessions(
            source,
            target,
            request.after,
            request.before,
            request.detached,
        )
    }

    fn move_window_relative_across_sessions(
        &mut self,
        source: WindowTarget,
        target: WindowTarget,
        after: bool,
        before: bool,
        detached: bool,
    ) -> Result<MoveWindowResponse, RmuxError> {
        let source_session_name = source.session_name().clone();
        let target_session_name = target.session_name().clone();
        self.reject_window_move_between_grouped_sessions(
            &source_session_name,
            &target_session_name,
        )?;
        let previous_source_session = self
            .sessions
            .session(&source_session_name)
            .cloned()
            .ok_or_else(|| session_not_found(&source_session_name))?;
        let previous_target_session = self
            .sessions
            .session(&target_session_name)
            .cloned()
            .ok_or_else(|| session_not_found(&target_session_name))?;
        let checkpoint = SessionCheckpoint::capture(
            self,
            [
                (&source_session_name, previous_source_session),
                (&target_session_name, previous_target_session),
            ],
        );
        let [(_, previous_source_session), (_, previous_target_session)] = checkpoint.sessions();

        ensure_session_panes_exist(self, &source_session_name, previous_source_session)?;
        ensure_session_panes_exist(self, &target_session_name, previous_target_session)?;
        let destination_index = link_window_destination_index(
            previous_target_session,
            target.window_index(),
            after,
            before,
        )?;

        let index_map = {
            let session = self
                .sessions
                .session_mut(&target_session_name)
                .ok_or_else(|| session_not_found(&target_session_name))?;
            session.make_room_for_window(destination_index)?
        };

        if let Err(error) =
            self.remap_session_group_window_metadata(&target_session_name, &index_map)
        {
            checkpoint.restore(self)?;
            return Err(error);
        }

        let destination = WindowTarget::with_window(target_session_name.clone(), destination_index);
        match self.move_window_across_sessions(
            source,
            destination,
            false,
            detached,
            Some(&index_map),
        ) {
            Ok(response) => Ok(response),
            Err(error) => {
                checkpoint.restore(self)?;
                Err(error)
            }
        }
    }

    fn move_window_relative_within_session(
        &mut self,
        source: WindowTarget,
        target: WindowTarget,
        after: bool,
        before: bool,
        detached: bool,
    ) -> Result<MoveWindowResponse, RmuxError> {
        let session_name = source.session_name().clone();
        let previous_session = self
            .sessions
            .session(&session_name)
            .cloned()
            .ok_or_else(|| session_not_found(&session_name))?;
        let checkpoint = SessionCheckpoint::capture(self, [(&session_name, previous_session)]);
        let [(_, previous_session)] = checkpoint.sessions();

        ensure_session_panes_exist(self, &session_name, previous_session)?;
        let destination_index =
            link_window_destination_index(previous_session, target.window_index(), after, before)?;

        let (index_map, adjusted_source_index) = {
            let session = self
                .sessions
                .session_mut(&session_name)
                .ok_or_else(|| session_not_found(&session_name))?;
            let index_map = session.make_room_for_window(destination_index)?;
            let adjusted_source_index = index_map
                .get(&source.window_index())
                .copied()
                .unwrap_or(source.window_index());
            (index_map, adjusted_source_index)
        };

        let remap_result = self.remap_session_group_window_metadata(&session_name, &index_map);
        if let Err(error) = remap_result {
            checkpoint.restore(self)?;
            return Err(error);
        }

        let adjusted_source =
            WindowTarget::with_window(session_name.clone(), adjusted_source_index);
        let winlink_alert_map = relative_move_winlink_alert_map(
            previous_session,
            &index_map,
            adjusted_source_index,
            destination_index,
        );
        let response = self.move_window_within_session(
            adjusted_source,
            destination_index,
            false,
            detached,
            &winlink_alert_map,
        );
        if let Err(error) = response {
            checkpoint.restore(self)?;
            return Err(error);
        }

        response
    }
}

fn relative_move_winlink_alert_map(
    previous_session: &Session,
    insertion_map: &BTreeMap<u32, u32>,
    adjusted_source_index: u32,
    destination_index: u32,
) -> BTreeMap<u32, u32> {
    previous_session
        .windows()
        .keys()
        .map(|&previous_index| {
            let shifted_index = insertion_map
                .get(&previous_index)
                .copied()
                .unwrap_or(previous_index);
            let final_index = if shifted_index == adjusted_source_index {
                destination_index
            } else {
                shifted_index
            };
            (previous_index, final_index)
        })
        .collect()
}
