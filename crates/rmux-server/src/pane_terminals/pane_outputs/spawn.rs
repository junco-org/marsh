use rmux_core::{PaneGeometry, PaneId, Utf8Config};
use rmux_proto::{RmuxError, SessionName, TerminalSize};
use crate::pane_io::{pane_output_channel, PaneOutputSender};
use crate::pane_terminals::HandlerState;
use crate::pane_transcript::{PaneTranscript, SharedPaneTranscript};

pub(in crate::pane_terminals) struct PaneOutputSpawn {
    pub(in crate::pane_terminals) geometry: PaneGeometry,
    pub(in crate::pane_terminals) initial_title: Option<String>,
    /// The output generation this pane's bytes are stamped with.
    ///
    /// Reserved by the creating transaction *before* its job is admitted, not allocated here.
    /// The job is opened with no handler lock held, so its first chunk can arrive before this
    /// surface exists; the route it was installed with names this number, and a chunk carrying
    /// any other one belongs to a generation this pane has already replaced.
    pub(in crate::pane_terminals) generation: u64,
}

impl HandlerState {
    #[cfg(windows)]
    pub(in crate::pane_terminals) fn insert_pending_pane_output(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        geometry: PaneGeometry,
        initial_title: Option<String>,
    ) -> Result<u64, RmuxError> {
        let transcript = PaneTranscript::shared(
            self.history_limit_for_session(session_name),
            TerminalSize {
                cols: geometry.cols(),
                rows: geometry.rows(),
            },
        );
        {
            let mut transcript = transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned");
            transcript.set_utf8_config(Utf8Config::from_options(&self.options));
            transcript.set_input_buffer_limit(self.input_buffer_limit());
            transcript.set_alternate_screen_enabled(
                self.alternate_screen_enabled_for_pane_id(session_name, pane_id),
            );
            transcript.set_title_rename_enabled(
                self.title_rename_enabled_for_pane_id(session_name, pane_id),
            );
        }
        seed_initial_pane_title(&transcript, initial_title.as_deref());
        let pane_output = pane_output_channel();

        if self
            .transcripts
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "pane transcript already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }

        if self
            .pane_outputs
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "pane output channel already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }

        self.transcripts
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, transcript);
        self.pane_outputs
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, pane_output.clone());
        let generation = self.reserve_pane_output_generation(session_name, pane_id);
        pane_output.set_generation(generation);
        if let Some(dead_panes) = self.dead_panes.get_mut(session_name) {
            let _ = dead_panes.remove(&pane_id);
        }
        self.update_pane_lifecycle_output_sequence(pane_id, generation);
        self.clear_attached_submitted_line(session_name, pane_id);
        Ok(generation)
    }

    /// Hands a pending pane's surface over to the job that has now opened for it.
    ///
    /// The surface itself — transcript, output channel and the generation both are stamped with —
    /// was installed by [`Self::insert_pending_pane_output`] before the job existed, which is the
    /// whole point of the deferred path. Nothing is started here: a pane's bytes arrive through
    /// the observation consumer, which routes them by the generation this returns, on both
    /// platforms.
    ///
    /// # Errors
    ///
    /// Fails when the pending transcript or output channel is missing, which means the surface
    /// this commit was supposed to activate was never installed or has already been torn down.
    #[cfg(windows)]
    pub(in crate::pane_terminals) fn activate_pending_pane_output(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Result<u64, RmuxError> {
        if !self
            .transcripts
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "missing pending pane transcript for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }
        let pane_output = self
            .pane_outputs
            .get(session_name)
            .and_then(|panes| panes.get(&pane_id))
            .ok_or_else(|| {
                RmuxError::Server(format!(
                    "missing pending pane output channel for pane id {} in session {}",
                    pane_id.as_u32(),
                    session_name
                ))
            })?;
        let generation = self.pane_output_generation(session_name, pane_id);
        pane_output.set_generation(generation);
        Ok(generation)
    }

    pub(in crate::pane_terminals) fn insert_pane_output(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        spawn: PaneOutputSpawn,
    ) -> Result<(), RmuxError> {
        let transcript = PaneTranscript::shared(
            self.history_limit_for_session(session_name),
            TerminalSize {
                cols: spawn.geometry.cols(),
                rows: spawn.geometry.rows(),
            },
        );
        {
            let mut transcript = transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned");
            transcript.set_utf8_config(Utf8Config::from_options(&self.options));
            transcript.set_input_buffer_limit(self.input_buffer_limit());
            transcript.set_alternate_screen_enabled(
                self.alternate_screen_enabled_for_pane_id(session_name, pane_id),
            );
            transcript.set_title_rename_enabled(
                self.title_rename_enabled_for_pane_id(session_name, pane_id),
            );
        }
        seed_initial_pane_title(&transcript, spawn.initial_title.as_deref());
        let pane_output = pane_output_channel();

        if self
            .transcripts
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "pane transcript already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }

        if self
            .pane_outputs
            .get(session_name)
            .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "pane output channel already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }

        self.transcripts
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, transcript);
        self.pane_outputs
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, pane_output.clone());
        let generation = spawn.generation;
        self.seed_pane_output_generation(session_name, pane_id, generation);
        pane_output.set_generation(generation);
        if let Some(dead_panes) = self.dead_panes.get_mut(session_name) {
            let _ = dead_panes.remove(&pane_id);
        }
        self.update_pane_lifecycle_output_sequence(pane_id, generation);
        self.clear_attached_submitted_line(session_name, pane_id);
        Ok(())
    }

    pub(in crate::pane_terminals) fn reset_pane_output(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        spawn: PaneOutputSpawn,
    ) -> Result<(), RmuxError> {
        self.reset_pane_output_with_sender(session_name, pane_id, spawn, None)
    }

    pub(in crate::pane_terminals) fn reset_pane_output_with_sender(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
        spawn: PaneOutputSpawn,
        retained_sender: Option<PaneOutputSender>,
    ) -> Result<(), RmuxError> {
        let transcript = PaneTranscript::shared(
            self.history_limit_for_session(session_name),
            TerminalSize {
                cols: spawn.geometry.cols(),
                rows: spawn.geometry.rows(),
            },
        );
        {
            let mut transcript = transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned");
            transcript.set_utf8_config(Utf8Config::from_options(&self.options));
            transcript.set_input_buffer_limit(self.input_buffer_limit());
            transcript.set_alternate_screen_enabled(
                self.alternate_screen_enabled_for_pane_id(session_name, pane_id),
            );
            transcript.set_title_rename_enabled(
                self.title_rename_enabled_for_pane_id(session_name, pane_id),
            );
        }
        seed_initial_pane_title(&transcript, spawn.initial_title.as_deref());
        if retained_sender.is_some()
            && self
                .pane_outputs
                .get(session_name)
                .is_some_and(|panes| panes.contains_key(&pane_id))
        {
            return Err(RmuxError::Server(format!(
                "pane output channel already exists for pane id {} in session {}",
                pane_id.as_u32(),
                session_name
            )));
        }
        self.transcripts
            .entry(session_name.clone())
            .or_default()
            .insert(pane_id, transcript);
        let pane_output = match retained_sender {
            Some(sender) => {
                self.pane_outputs
                    .entry(session_name.clone())
                    .or_default()
                    .insert(pane_id, sender.clone());
                sender
            }
            None => self
                .pane_outputs
                .entry(session_name.clone())
                .or_default()
                .entry(pane_id)
                .or_insert_with(pane_output_channel)
                .clone(),
        };
        let generation = spawn.generation;
        self.seed_pane_output_generation(session_name, pane_id, generation);
        pane_output.reset_generation(generation);
        if let Some(dead_panes) = self.dead_panes.get_mut(session_name) {
            let _ = dead_panes.remove(&pane_id);
        }
        self.update_pane_lifecycle_output_sequence(pane_id, generation);
        self.clear_attached_submitted_line(session_name, pane_id);
        Ok(())
    }
}

fn seed_initial_pane_title(transcript: &SharedPaneTranscript, initial_title: Option<&str>) {
    let fallback;
    let title = match initial_title.filter(|title| !title.is_empty()) {
        Some(title) => title,
        None => {
            let Some(hostname) = crate::host_name::local_hostname() else {
                return;
            };
            fallback = hostname;
            &fallback
        }
    };
    let mut transcript = transcript
        .lock()
        .expect("pane transcript mutex must not be poisoned");
    if transcript.title().is_empty() {
        transcript.set_title(title);
    }
}
