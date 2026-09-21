use rmux_proto::{ErrorResponse, HookName, Response, ScopeSelector, Target};

use super::super::{
    client_environment_snapshot, client_spawn_environment,
    scripting_support::{format_context_for_target, render_start_directory_template},
    RequestHandler,
};
#[cfg(windows)]
use super::format_references_pane_pid;
use crate::format_runtime::render_runtime_template;
use crate::hook_runtime::PendingInlineHookFormat;
use crate::pane_terminal_lookup::pane_id_for_target;

impl RequestHandler {
    pub(in crate::handler) async fn handle_pipe_pane(
        &self,
        _requester_pid: u32,
        request: rmux_proto::PipePaneRequest,
    ) -> Response {
        let session_name = request.target.session_name().clone();
        let target = request.target.clone();
        let attached_count = self.attached_count(&session_name).await;
        let write_to_pipe = if !request.stdin && !request.stdout {
            true
        } else {
            request.stdout
        };
        // Three phases, because neither slow half may run under the handler lock: closing a pipe
        // waits for its command's publication boundary, and opening one admits a managed job.
        let mut plan = {
            let mut state = self.state.lock().await;
            let command = match request.command.as_deref() {
                Some(command) => {
                    let runtime = match format_context_for_target(
                        &state,
                        &Target::Pane(target.clone()),
                        attached_count,
                    ) {
                        Ok(runtime) => runtime,
                        Err(error) => return Response::Error(ErrorResponse { error }),
                    };
                    Some(render_runtime_template(command, &runtime, true))
                }
                None => None,
            };

            match state.plan_pipe_pane(
                &target,
                command,
                request.stdin,
                write_to_pipe,
                request.once,
            ) {
                Ok(plan) => plan,
                Err(error) => return Response::Error(ErrorResponse { error }),
            }
        };

        let response = match self.apply_pipe_pane_plan(&mut plan, target.clone()).await {
            Ok(response) => Response::PipePane(response),
            Err(error) => Response::Error(ErrorResponse { error }),
        };

        if matches!(response, Response::PipePane(_)) {
            self.queue_inline_hook(
                HookName::AfterPipePane,
                ScopeSelector::Pane(target.clone()),
                Some(Target::Pane(target)),
                PendingInlineHookFormat::AfterCommand,
            );
        }

        response
    }

    /// Carries out a planned `pipe-pane`, with the handler state unlocked.
    ///
    /// The lock is only taken again to install the new pipe, and only after its job has been
    /// admitted: the pane is revalidated there, because it can be killed while this is waiting.
    ///
    /// # Errors
    ///
    /// Fails when a displaced pipe's log was not published, when the engine refused the new job,
    /// and when the pane went away while that job was being admitted.
    async fn apply_pipe_pane_plan(
        &self,
        plan: &mut crate::pane_terminals::PipePanePlan,
        target: rmux_proto::PaneTarget,
    ) -> Result<rmux_proto::PipePaneResponse, rmux_proto::RmuxError> {
        plan.close_replaced().await?;
        if let Some(pipe) = plan.open().await? {
            self.state.lock().await.commit_pipe_pane(plan, pipe)?;
        }
        Ok(rmux_proto::PipePaneResponse { target })
    }

    pub(in crate::handler) async fn handle_respawn_pane(
        &self,
        requester_pid: u32,
        mut request: rmux_proto::RespawnPaneRequest,
    ) -> Response {
        #[cfg(windows)]
        if request.start_directory.as_ref().is_some_and(|path| {
            format_references_pane_pid(Some(path.as_os_str().to_string_lossy().as_ref()))
        }) {
            self.wait_for_windows_deferred_all_pane_pids().await;
        }
        let session_name = request.target.session_name().clone();
        let target = request.target.clone();
        let socket_path = self.socket_path();
        let client_environment = client_environment_snapshot(requester_pid);
        let spawn_environment = client_spawn_environment(client_environment.as_ref());
        let attached_count = self.attached_count(&session_name).await;
        // The replacement job is opened with the request mutex released, so this serializes
        // against every other pane creation for the length of the transaction.
        let creation = self.pane_creation_transaction().await;
        let (response, respawned_pane_id) = {
            let mut state = self.state.lock().await;
            let target_window = rmux_proto::WindowTarget::with_window(
                request.target.session_name().clone(),
                request.target.window_index(),
            );
            if let Err(error) =
                super::super::require_expected_window_identity(&state, &target_window)
            {
                return Response::Error(ErrorResponse { error });
            }
            request.start_directory = match render_start_directory_template(
                &state,
                &Target::Pane(target),
                attached_count,
                request.start_directory,
            ) {
                Ok(start_directory) => start_directory,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            let pane_id = match pane_id_for_target(
                &state.sessions,
                request.target.session_name(),
                request.target.window_index(),
                request.target.pane_index(),
            ) {
                Ok(pane_id) => pane_id,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            let planned = match state.plan_pane_respawn(
                request,
                &socket_path,
                spawn_environment.as_ref(),
            ) {
                Ok(planned) => planned,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            drop(state);
            let opened = planned.open().await;
            let mut state = self.state.lock().await;
            let committed = match opened {
                Ok((commit, prepared)) => {
                    state.commit_pane_respawn(commit, prepared, |_, _| {})
                }
                Err(error) => Err(error),
            };
            match committed {
                Ok(response) => {
                    self.record_pane_respawn_boundary(pane_id);
                    state.retire_respawned_lifecycle_panes(&[pane_id]);
                    (Response::RespawnPane(response), Some(pane_id))
                }
                Err(error) => (Response::Error(ErrorResponse { error }), None),
            }
        };

        if respawned_pane_id.is_some() {
            // The transaction ends with the commit. What follows emits lifecycle events and refreshes
            // attached clients, and either can run a user hook that itself creates a pane — which would
            // take this same non-reentrant mutex while its own call stack still holds the guard. That
            // deadlocks with every worker idle, which is exactly how it presents.
            drop(creation);

            self.refresh_attached_session(&session_name).await;
        }

        response
    }
}
