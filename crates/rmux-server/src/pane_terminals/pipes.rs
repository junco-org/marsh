use std::collections::HashMap;

use rmux_core::PaneId;
use rmux_proto::{PaneTarget, RmuxError, SessionName};

use super::pane_pipe::ActivePanePipe;
use super::HandlerState;
use crate::io::{ShellHandle, ShellIo};
use crate::pane_io::PaneOutputSender;
use crate::pane_terminal_lookup::pane_id_for_target;
use crate::terminal::TerminalProfile;

/// What one `pipe-pane` request decided while the handler state was locked.
///
/// The two slow halves of the request — waiting for a displaced pipe's publication boundary and
/// admitting the new pipe's managed job — happen with the lock released, so what the decision
/// produced has to be carried across that gap rather than re-derived on the other side of it.
pub(crate) struct PipePanePlan {
    /// The runtime session the pipe is filed under; a linked window has its own.
    session_name: SessionName,
    /// The pane this pipe belongs to. Stable across index churn, which is why it is the key.
    pane_id: PaneId,
    /// The window index, for revalidation and diagnostics.
    window_index: u32,
    /// The pane index, same.
    pane_index: u32,
    /// The pipe this request displaces. Closed with the lock released, because closing waits for
    /// the command's verdict.
    replaced: Option<ActivePanePipe>,
    /// What to open, absent for a request that only closes.
    open: Option<PipePaneOpen>,
}

/// Everything a new pipe needs, captured while the lock was held.
struct PipePaneOpen {
    /// The pane's directory and environment.
    profile: TerminalProfile,
    /// The pane output a `-O` pipe subscribes to.
    pane_output: PaneOutputSender,
    /// The engine.
    io: ShellIo,
    /// The pane a `-I` pipe writes back into.
    handle: ShellHandle,
    /// The command text, already rendered.
    command: String,
    /// `-I`: forward the command's output into the pane.
    read_from_pipe: bool,
    /// `-O`: forward the pane's output into the command.
    write_to_pipe: bool,
}

impl PipePanePlan {
    /// Closes the pipe this request displaces.
    ///
    /// Waits for that command's own boundary: a `pipe-pane -o 'cat >> log'` whose writes the gate
    /// refused has to be reported, because the log the user asked for does not exist and no later
    /// part of this request would ever mention it.
    ///
    /// # Errors
    ///
    /// Fails when the displaced pipe's command was not published, and when it had to be forced.
    pub(crate) async fn close_replaced(&mut self) -> Result<(), RmuxError> {
        match self.replaced.take() {
            Some(pipe) => pipe.close().await,
            None => Ok(()),
        }
    }

    /// Admits the new pipe's managed job, for a request that opens one.
    ///
    /// # Errors
    ///
    /// Fails when the engine refused the job, when the pane's directory is outside the seed, and
    /// when its environment carries non-UTF-8 data.
    pub(crate) async fn open(&mut self) -> Result<Option<ActivePanePipe>, RmuxError> {
        let Some(open) = self.open.take() else {
            return Ok(None);
        };
        ActivePanePipe::spawn(
            &open.profile,
            open.pane_output,
            open.io,
            open.handle,
            &open.command,
            open.read_from_pipe,
            open.write_to_pipe,
        )
        .await
        .map(Some)
    }
}

impl HandlerState {
    /// Decides what one `pipe-pane` request does, without doing the slow half of it.
    ///
    /// # Errors
    ///
    /// Fails when the target does not resolve, when the pane has exited, and when the pane has no
    /// output to subscribe to.
    pub(crate) fn plan_pipe_pane(
        &mut self,
        target: &PaneTarget,
        command: Option<String>,
        read_from_pipe: bool,
        write_to_pipe: bool,
        once: bool,
    ) -> Result<PipePanePlan, RmuxError> {
        let session_name = target.session_name().clone();
        let window_index = target.window_index();
        let pane_index = target.pane_index();
        let pane_id = pane_id_for_target(&self.sessions, &session_name, window_index, pane_index)?;
        let runtime_session_name =
            self.runtime_session_name_for_window(&session_name, window_index);

        let replaced = self.remove_pane_pipe(&runtime_session_name, pane_id);
        let mut plan = PipePanePlan {
            session_name: runtime_session_name.clone(),
            pane_id,
            window_index,
            pane_index,
            replaced,
            open: None,
        };

        // `-o` against an existing pipe is a close, and an empty or absent command is a close.
        // Both leave `open` unset, which is what makes them stop here rather than reopen.
        if once && plan.replaced.is_some() {
            return Ok(plan);
        }
        let Some(command) = command.filter(|command| !command.is_empty()) else {
            return Ok(plan);
        };

        let (io, handle) = self.pane_shell_if_alive(&session_name, window_index, pane_index)?;
        let pane_output = self.pane_output_for_target(&session_name, window_index, pane_index)?;
        let profile = self
            .terminals
            .pane_profile(&runtime_session_name, pane_id, window_index, pane_index)?
            .clone();
        plan.open = Some(PipePaneOpen {
            profile,
            pane_output,
            io,
            handle,
            command,
            read_from_pipe,
            write_to_pipe,
        });
        Ok(plan)
    }

    /// Installs a pipe whose job was admitted with the lock released.
    ///
    /// The pane is revalidated first. Killing it while the job was being admitted would otherwise
    /// file this pipe under a pane that no longer exists, where nothing would ever close it.
    ///
    /// # Errors
    ///
    /// Fails when the pane went away while the job was being admitted, having stopped the job.
    pub(crate) fn commit_pipe_pane(
        &mut self,
        plan: &PipePanePlan,
        pipe: ActivePanePipe,
    ) -> Result<(), RmuxError> {
        if let Err(error) = self.terminals.pane_shell_if_alive(
            &plan.session_name,
            plan.pane_id,
            plan.window_index,
            plan.pane_index,
        ) {
            pipe.stop();
            return Err(error);
        }
        if let Some(previous) = self.pipes.insert(&plan.session_name, plan.pane_id, pipe) {
            previous.stop();
        }
        Ok(())
    }

    pub(crate) fn pane_has_pipe(
        &self,
        session_name: &SessionName,
        window_index: u32,
        pane_id: PaneId,
    ) -> bool {
        let runtime_session_name = self.runtime_session_name_for_window(session_name, window_index);
        self.pipes.contains(&runtime_session_name, pane_id)
    }

    pub(in crate::pane_terminals) fn remove_session_pipes(
        &mut self,
        session_name: &SessionName,
    ) -> HashMap<PaneId, ActivePanePipe> {
        self.pipes.remove_session(session_name)
    }

    pub(in crate::pane_terminals) fn remove_pane_pipe(
        &mut self,
        session_name: &SessionName,
        pane_id: PaneId,
    ) -> Option<ActivePanePipe> {
        self.pipes.remove(session_name, pane_id)
    }
}
