use std::collections::HashMap;

use rmux_core::{events::PaneOutputSubscriptionKey, PaneId};
use rmux_proto::{PaneTarget, RmuxError, SessionName};

use crate::pane_io::PaneOutputSender;
use crate::pane_terminal_lookup::{missing_pane_terminal, pane_id_for_target};
use crate::pane_transcript::SharedPaneTranscript;

use super::{session_not_found, HandlerState};

#[path = "pane_outputs/submitted.rs"]
mod submitted;

#[path = "pane_outputs/exit_refresh.rs"]
mod exit_refresh;
#[path = "pane_outputs/spawn.rs"]
mod spawn;

pub(in crate::pane_terminals) use self::spawn::PaneOutputSpawn;
pub(super) use self::submitted::AttachedSubmittedLine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneExitMetadata {
    pub(crate) status: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) time: Option<i64>,
}

impl PaneExitMetadata {
    pub(crate) const fn without_exit_details() -> Self {
        Self {
            status: None,
            signal: None,
            time: None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct RemovedPaneOutputs {
    dead_panes: HashMap<PaneId, PaneExitMetadata>,
    transcripts: HashMap<PaneId, SharedPaneTranscript>,
    pane_outputs: HashMap<PaneId, PaneOutputSender>,
    pane_output_generations: HashMap<PaneId, u64>,
    attached_submitted_rows: HashMap<PaneId, AttachedSubmittedLine>,
}

impl RemovedPaneOutputs {
    pub(in crate::pane_terminals) fn pane_output_sender(
        &self,
        pane_id: PaneId,
    ) -> Option<PaneOutputSender> {
        self.pane_outputs.get(&pane_id).cloned()
    }

    /// The output generation that was recorded for `pane_id`, for a caller that keeps the pane.
    ///
    /// A removal that is really a *replacement* of one pane's runtime keeps the pane's identity,
    /// and the reservation its replacement was opened with is part of that identity.
    pub(in crate::pane_terminals) fn pane_output_generation(&self, pane_id: PaneId) -> Option<u64> {
        self.pane_output_generations.get(&pane_id).copied()
    }
}

impl HandlerState {
    /// The exit this runtime pane has already been observed to have, if it has one.
    ///
    /// A pane's exit is announced by the engine and recorded by
    /// [`Self::mark_runtime_pane_dead_with_status`] on the way through
    /// `RequestHandler::note_shell_closed`, so a pane that really exited is found in `dead_panes`
    /// with the gate's own verdict. What is left here is the case that seeding missed: a job whose
    /// generation is gone from the engine while nothing recorded a status for it. There is no wait
    /// status to report for that — the shell is embedded in this daemon and never was an OS child
    /// — so it is recorded without exit details rather than with an invented code.
    pub(crate) fn observe_runtime_pane_exit(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Result<Option<PaneExitMetadata>, RmuxError> {
        #[cfg(windows)]
        if self
            .starting_panes
            .get(runtime_session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Ok(None);
        }

        if self
            .pane_target_for_runtime_pane(runtime_session_name, pane_id)
            .is_none()
        {
            return Ok(None);
        }

        if let Some(metadata) = self
            .dead_panes
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .copied()
        {
            self.mark_pane_lifecycle_exited(pane_id, metadata);
            return Ok(Some(metadata));
        }

        if self
            .terminals
            .pane_is_alive(runtime_session_name, pane_id)?
        {
            return Ok(None);
        }
        let metadata = PaneExitMetadata::without_exit_details();
        self.dead_panes
            .entry(runtime_session_name.clone())
            .or_default()
            .insert(pane_id, metadata);
        self.mark_pane_lifecycle_exited(pane_id, metadata);
        Ok(Some(metadata))
    }

    pub(crate) fn mark_pane_dead_without_exit_details(
        &mut self,
        target: &PaneTarget,
    ) -> Result<(), RmuxError> {
        let pane_id = pane_id_for_target(
            &self.sessions,
            target.session_name(),
            target.window_index(),
            target.pane_index(),
        )?;
        let runtime_session_name =
            self.runtime_session_name_for_window(target.session_name(), target.window_index());
        let metadata = PaneExitMetadata::without_exit_details();
        self.dead_panes
            .entry(runtime_session_name)
            .or_default()
            .insert(pane_id, metadata);
        self.mark_pane_lifecycle_exited(pane_id, metadata);
        Ok(())
    }

    /// The transcript and output ring of one runtime pane, for a ShellMux job's terminal stream.
    ///
    /// Keyed by the *runtime* session name the caller already resolved with
    /// [`Self::resolve_pane_event_runtime_session`], not by the session a job's route was
    /// installed in: a linked or moved window keeps the stable pane id but changes which runtime
    /// session owns its transcript, so resolving from a remembered name would publish into the
    /// pane's former home.
    pub(crate) fn runtime_pane_transcript_and_output(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<(SharedPaneTranscript, PaneOutputSender)> {
        let transcript = self
            .transcripts
            .get(runtime_session_name)?
            .get(&pane_id)?
            .clone();
        let output = self
            .pane_outputs
            .get(runtime_session_name)?
            .get(&pane_id)?
            .clone();
        Some((transcript, output))
    }

    /// Records the gated status of a ShellMux job whose pane is about to run the exit pipeline.
    ///
    /// An embedded shell has no OS child to reap, so [`Self::observe_runtime_pane_exit`] would
    /// find nothing to report. Seeding `dead_panes` first makes that lookup answer with the
    /// engine's own verdict instead — the publication gate's decision, not a guessed wait status.
    /// `signal` stays `None` because no signal was delivered; inventing `128 + n` from a shell's
    /// exit code would claim one that never happened.
    pub(crate) fn mark_runtime_pane_dead_with_status(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
        status: Option<i32>,
    ) {
        let metadata = PaneExitMetadata {
            status,
            signal: None,
            time: Some(chrono::Local::now().timestamp()),
        };
        self.dead_panes
            .entry(runtime_session_name.clone())
            .or_default()
            .insert(pane_id, metadata);
        self.mark_pane_lifecycle_exited(pane_id, metadata);
    }

    pub(crate) fn append_bytes_to_runtime_pane_transcript(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> Result<(), RmuxError> {
        let transcript = self
            .transcripts
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned()
            .ok_or_else(|| {
                RmuxError::Server(format!(
                    "missing pane transcript for pane id {} in session {}",
                    pane_id.as_u32(),
                    runtime_session_name
                ))
            })?;
        if let Some(output) = self
            .pane_outputs
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned()
        {
            output.mutate_transcript(
                &transcript,
                crate::pane_io::PaneInvalidationReason::TranscriptMutation,
                |transcript| {
                    transcript.append_bytes(bytes);
                    ((), !bytes.is_empty())
                },
            );
        } else {
            transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned")
                .append_bytes(bytes);
        }
        Ok(())
    }

    /// Publishes synthetic pane bytes and applies them to the transcript at
    /// the same output-state linearization point as PTY output.
    #[cfg(windows)]
    pub(crate) fn publish_bytes_to_runtime_pane_transcript(
        &mut self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
        generation: Option<u64>,
        bytes: Vec<u8>,
    ) -> Result<bool, RmuxError> {
        let transcript = self
            .transcripts
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned()
            .ok_or_else(|| {
                RmuxError::Server(format!(
                    "missing pane transcript for pane id {} in session {}",
                    pane_id.as_u32(),
                    runtime_session_name
                ))
            })?;
        let output = self
            .pane_outputs
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned()
            .ok_or_else(|| {
                RmuxError::Server(format!(
                    "missing pane output for pane id {} in session {}",
                    pane_id.as_u32(),
                    runtime_session_name
                ))
            })?;
        Ok(output
            .publish_for_generation_with_invalidation(generation, bytes, |bytes| {
                let append = transcript
                    .lock()
                    .expect("pane transcript mutex must not be poisoned")
                    .append_bytes_with_effects(bytes);
                let invalidation = append
                    .recovery_rebase_required
                    .then_some(crate::pane_io::PaneInvalidationReason::TranscriptMutation);
                ((), Vec::new(), invalidation)
            })
            .is_some())
    }

    pub(crate) fn pane_output_for_target(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Result<PaneOutputSender, RmuxError> {
        let pane_id = pane_id_for_target(&self.sessions, session_name, window_index, pane_index)?;
        let runtime_session_name = self.runtime_session_name_for_window(session_name, window_index);
        self.pane_outputs
            .get(&runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned()
            .ok_or_else(|| missing_pane_terminal(session_name, window_index, pane_index))
    }

    pub(crate) fn pane_output_subscription_key_for_target(
        &self,
        target: &rmux_proto::PaneTarget,
    ) -> Result<PaneOutputSubscriptionKey, RmuxError> {
        let pane_id = pane_id_for_target(
            &self.sessions,
            target.session_name(),
            target.window_index(),
            target.pane_index(),
        )?;
        let runtime_session_name =
            self.runtime_session_name_for_window(target.session_name(), target.window_index());
        Ok(PaneOutputSubscriptionKey::new(
            runtime_session_name,
            pane_id,
        ))
    }

    pub(crate) fn pane_output_subscription_key_for_pane_id(
        &self,
        pane_id: PaneId,
    ) -> Option<PaneOutputSubscriptionKey> {
        self.pane_alias_targets(pane_id)
            .into_iter()
            .find_map(|target| self.pane_output_subscription_key_for_target(&target).ok())
    }

    pub(crate) fn pane_output_subscription_keys_for_kill(
        &self,
        target: &rmux_proto::PaneTarget,
        kill_all_except: bool,
    ) -> Result<Vec<PaneOutputSubscriptionKey>, RmuxError> {
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
        let runtime_session_name =
            self.runtime_session_name_for_window(target.session_name(), target.window_index());
        let keys = window
            .panes()
            .iter()
            .filter(|pane| {
                if kill_all_except {
                    pane.index() != target.pane_index()
                } else {
                    pane.index() == target.pane_index()
                }
            })
            .map(|pane| PaneOutputSubscriptionKey::new(runtime_session_name.clone(), pane.id()))
            .collect();
        Ok(keys)
    }

    pub(crate) fn subscribe_runtime_pane_output(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<crate::pane_io::PaneOutputReceiver> {
        self.pane_outputs
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .map(PaneOutputSender::subscribe)
    }

    #[cfg(windows)]
    pub(crate) fn subscribe_runtime_pane_output_from_oldest(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<crate::pane_io::PaneOutputReceiver> {
        self.pane_outputs
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .map(PaneOutputSender::subscribe_from_oldest)
    }

    pub(crate) fn runtime_pane_output_drain_handles(
        &self,
        runtime_session_name: &SessionName,
        pane_id: PaneId,
    ) -> (
        Option<crate::pane_io::PaneOutputReceiver>,
        Option<PaneOutputSender>,
    ) {
        let sender = self
            .pane_outputs
            .get(runtime_session_name)
            .and_then(|panes| panes.get(&pane_id))
            .cloned();
        let receiver = sender.as_ref().map(PaneOutputSender::subscribe);
        (receiver, sender)
    }

    pub(crate) fn session_pane_outputs(
        &self,
        session_name: &SessionName,
    ) -> Result<Vec<(u32, PaneOutputSender)>, RmuxError> {
        let _session = self
            .sessions
            .session(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let runtime_session_name = self.runtime_session_name(session_name);
        let Some(pane_outputs) = self.pane_outputs.get(&runtime_session_name) else {
            return Ok(Vec::new());
        };
        let mut outputs = pane_outputs
            .iter()
            .map(|(pane_id, sender)| (pane_id.as_u32(), sender.clone()))
            .collect::<Vec<_>>();
        outputs.sort_by_key(|(pane_id, _)| *pane_id);
        Ok(outputs)
    }

    pub(in crate::pane_terminals) fn remove_session_pane_outputs(
        &mut self,
        session_name: &SessionName,
    ) -> RemovedPaneOutputs {
        let dead_panes = self.dead_panes.remove(session_name).unwrap_or_default();
        let attached_submitted_rows = self
            .attached_submitted_rows
            .remove(session_name)
            .unwrap_or_default();
        RemovedPaneOutputs {
            dead_panes,
            transcripts: self.transcripts.remove(session_name).unwrap_or_default(),
            pane_outputs: self.pane_outputs.remove(session_name).unwrap_or_default(),
            pane_output_generations: self
                .pane_output_generations
                .remove(session_name)
                .unwrap_or_default(),
            attached_submitted_rows,
        }
    }

    pub(in crate::pane_terminals) fn remove_pane_output(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<(SharedPaneTranscript, PaneOutputSender)> {
        if let Some(dead_panes) = self.dead_panes.get_mut(session_name) {
            let _ = dead_panes.remove(&pane_id);
        }
        self.clear_attached_submitted_line(session_name, pane_id);
        if let Some(generations) = self.pane_output_generations.get_mut(session_name) {
            let _ = generations.remove(&pane_id);
        }
        let transcript = self
            .transcripts
            .get_mut(session_name)
            .and_then(|panes| panes.remove(&pane_id));
        let pane_output = self
            .pane_outputs
            .get_mut(session_name)
            .and_then(|panes| panes.remove(&pane_id));
        match (transcript, pane_output) {
            (Some(transcript), Some(pane_output)) => Some((transcript, pane_output)),
            _ => None,
        }
    }

    pub(in crate::pane_terminals) fn remove_pane_outputs(
        &mut self,
        session_name: &SessionName,
        pane_ids: &[PaneId],
    ) -> RemovedPaneOutputs {
        let mut removed = RemovedPaneOutputs::default();
        for pane_id in pane_ids {
            if let Some(metadata) = self
                .dead_panes
                .get_mut(session_name)
                .and_then(|dead_panes| dead_panes.remove(pane_id))
            {
                removed.dead_panes.insert(*pane_id, metadata);
            }
            if let Some(absolute_y) = self.take_attached_submitted_line(session_name, *pane_id) {
                removed.attached_submitted_rows.insert(*pane_id, absolute_y);
            }
            if let Some(transcript) = self
                .transcripts
                .get_mut(session_name)
                .and_then(|panes| panes.remove(pane_id))
            {
                removed.transcripts.insert(*pane_id, transcript);
            }
            if let Some(pane_output) = self
                .pane_outputs
                .get_mut(session_name)
                .and_then(|panes| panes.remove(pane_id))
            {
                removed.pane_outputs.insert(*pane_id, pane_output);
            }
            if let Some(generation) = self
                .pane_output_generations
                .get_mut(session_name)
                .and_then(|panes| panes.remove(pane_id))
            {
                removed.pane_output_generations.insert(*pane_id, generation);
            }
        }
        removed
    }

    pub(in crate::pane_terminals) fn insert_existing_pane_outputs(
        &mut self,
        session_name: &SessionName,
        removed_outputs: RemovedPaneOutputs,
    ) {
        self.dead_panes
            .entry(session_name.clone())
            .or_default()
            .extend(removed_outputs.dead_panes);
        self.attached_submitted_rows
            .entry(session_name.clone())
            .or_default()
            .extend(removed_outputs.attached_submitted_rows);
        self.transcripts
            .entry(session_name.clone())
            .or_default()
            .extend(removed_outputs.transcripts);
        self.pane_outputs
            .entry(session_name.clone())
            .or_default()
            .extend(removed_outputs.pane_outputs);
        self.pane_output_generations
            .entry(session_name.clone())
            .or_default()
            .extend(removed_outputs.pane_output_generations);
    }

    /// Claims the next output generation for one pane, and records it as that pane's current one.
    ///
    /// Called by a creating transaction *before* it releases the handler's state lock to open the
    /// pane's job, which is what makes the number usable as the job's route: nothing else can
    /// hand out the same generation while the spawn is in flight, so a chunk that arrives before
    /// the pane's surface exists is still attributable to exactly one pane and one job.
    ///
    /// A reservation that is never committed — a spawn that failed, a pane whose window moved out
    /// from under it — is simply skipped. Generations only have to be unique and increasing.
    pub(in crate::pane_terminals) fn reserve_pane_output_generation(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> u64 {
        let generations = self
            .pane_output_generations
            .entry(session_name.clone())
            .or_default();
        let next = generations
            .get(&pane_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(pane_id, next);
        next
    }

    pub(crate) fn pane_output_generation(
        &self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> u64 {
        self.pane_output_generations
            .get(session_name)
            .and_then(|panes| panes.get(&pane_id))
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn pane_output_generation_for_target(
        &self,
        target: &PaneTarget,
        pane_id: PaneId,
    ) -> u64 {
        let runtime_session_name =
            self.runtime_session_name_for_window(target.session_name(), target.window_index());
        self.pane_output_generation(&runtime_session_name, pane_id)
    }

    pub(in crate::pane_terminals) fn seed_pane_output_generation(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        generation: u64,
    ) {
        self.pane_output_generations
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, generation);
    }

    pub(crate) fn move_pane_outputs_between_sessions(
        &mut self,
        source_session: &SessionName,
        destination_session: &SessionName,
        pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session || pane_ids.is_empty() {
            return Ok(());
        }

        let outputs = self.remove_pane_outputs(source_session, pane_ids);
        if outputs.transcripts.len() != pane_ids.len()
            || outputs.pane_outputs.len() != pane_ids.len()
        {
            self.insert_existing_pane_outputs(source_session, outputs);
            return Err(RmuxError::Server(format!(
                "missing pane transcript for transfer from session {source_session}"
            )));
        }

        let destination_transcripts = self
            .transcripts
            .entry(destination_session.clone())
            .or_default();
        let destination_outputs = self
            .pane_outputs
            .entry(destination_session.clone())
            .or_default();
        if pane_ids.iter().any(|pane_id| {
            destination_transcripts.contains_key(pane_id)
                || destination_outputs.contains_key(pane_id)
        }) {
            self.insert_existing_pane_outputs(source_session, outputs);
            return Err(RmuxError::Server(format!(
                "pane transcript already exists in session {destination_session}"
            )));
        }

        self.insert_existing_pane_outputs(destination_session, outputs);
        if let Err(error) =
            self.pipes
                .move_between_sessions(source_session, destination_session, pane_ids)
        {
            let restored = self.remove_pane_outputs(destination_session, pane_ids);
            self.insert_existing_pane_outputs(source_session, restored);
            return Err(error);
        }
        self.refresh_transcript_limits_for_session(destination_session);
        Ok(())
    }

    pub(crate) fn swap_pane_outputs_between_sessions(
        &mut self,
        source_session: &SessionName,
        source_pane_ids: &[PaneId],
        destination_session: &SessionName,
        destination_pane_ids: &[PaneId],
    ) -> Result<(), RmuxError> {
        if source_session == destination_session {
            return Ok(());
        }

        let source_outputs = self.remove_pane_outputs(source_session, source_pane_ids);
        let destination_outputs =
            self.remove_pane_outputs(destination_session, destination_pane_ids);
        if source_outputs.transcripts.len() != source_pane_ids.len()
            || source_outputs.pane_outputs.len() != source_pane_ids.len()
            || destination_outputs.transcripts.len() != destination_pane_ids.len()
            || destination_outputs.pane_outputs.len() != destination_pane_ids.len()
        {
            self.insert_existing_pane_outputs(source_session, source_outputs);
            self.insert_existing_pane_outputs(destination_session, destination_outputs);
            return Err(RmuxError::Server(
                "missing pane transcript for cross-session swap".to_owned(),
            ));
        }

        self.insert_existing_pane_outputs(source_session, destination_outputs);
        self.insert_existing_pane_outputs(destination_session, source_outputs);
        if let Err(error) = self.pipes.swap_between_sessions(
            source_session,
            source_pane_ids,
            destination_session,
            destination_pane_ids,
        ) {
            let restored_source = self.remove_pane_outputs(source_session, destination_pane_ids);
            let restored_destination =
                self.remove_pane_outputs(destination_session, source_pane_ids);
            self.insert_existing_pane_outputs(source_session, restored_destination);
            self.insert_existing_pane_outputs(destination_session, restored_source);
            return Err(error);
        }
        self.refresh_transcript_limits_for_session(source_session);
        self.refresh_transcript_limits_for_session(destination_session);
        Ok(())
    }
}
