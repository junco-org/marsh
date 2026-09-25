use std::path::PathBuf;
use std::time::Duration;

use rmux_core::{
    formats::{is_truthy, render_list_sessions_line, FormatContext},
    LifecycleEvent, PaneId, WindowId, WINDOW_ALERTFLAGS,
};
use rmux_proto::request::NewSessionExtRequest;
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::{
    ErrorResponse, HookName, KillSessionRequest, KillSessionResponse, ListSessionsResponse,
    NewSessionResponse, OptionName, Response, RmuxError, ScopeSelector, SessionId, SessionName,
    WindowTarget,
};

use crate::format_runtime::{render_runtime_template, RuntimeFormatContext};
use crate::hook_runtime::PendingInlineHookFormat;
use crate::pane_terminals::InitialPaneSpawnOptions;
use crate::terminal::{parse_environment_assignments, validate_process_command};

#[path = "handler_session/client_environment.rs"]
mod client_environment;
#[path = "handler_session/control_mode.rs"]
mod control_mode;
#[path = "handler_session/has.rs"]
mod has;
#[path = "handler_session/list.rs"]
mod list;
#[path = "handler_session/options.rs"]
mod options;
#[path = "handler_session/output.rs"]
mod output;

/// How long an emptied server waits for its own teardown before deciding it is idle.
///
/// Long enough for the panes a kill just terminated to close and drain — that is milliseconds,
/// not seconds — and short enough that a teardown which has genuinely wedged cannot make this
/// daemon unkillable. See [`RequestHandler::request_shutdown_if_server_empty`].
const EMPTY_SERVER_QUIET_GRACE: Duration = Duration::from_secs(1);

enum KillSessionExecution {
    Completed(Response),
    OnlyLinkedWindowChanged,
}

#[derive(Clone, Copy)]
struct OnlyWindowPrecondition {
    window_id: WindowId,
    kill_if_last: bool,
}

impl KillSessionExecution {
    fn into_unconditional_response(self) -> Response {
        match self {
            Self::Completed(response) => response,
            Self::OnlyLinkedWindowChanged => {
                unreachable!("unconditional kill-session cannot lose a window precondition")
            }
        }
    }
}

impl From<Response> for KillSessionExecution {
    fn from(response: Response) -> Self {
        Self::Completed(response)
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(in crate::handler) struct RenameSessionIdentityPause {
    pub(in crate::handler) reached: tokio::sync::Notify,
    pub(in crate::handler) release: tokio::sync::Notify,
}

#[cfg(test)]
static RENAME_SESSION_IDENTITY_PAUSE: std::sync::Mutex<
    Option<(SessionName, std::sync::Arc<RenameSessionIdentityPause>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
#[derive(Debug, Default)]
pub(in crate::handler) struct RenameSessionControlCommitPause {
    pub(in crate::handler) reached: tokio::sync::Notify,
    pub(in crate::handler) release: tokio::sync::Notify,
}

#[cfg(test)]
static RENAME_SESSION_CONTROL_COMMIT_PAUSE: std::sync::Mutex<
    Vec<(SessionName, std::sync::Arc<RenameSessionControlCommitPause>)>,
> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
#[derive(Debug, Default)]
pub(in crate::handler) struct KillSessionWebPrunePause {
    pub(in crate::handler) reached: tokio::sync::Notify,
    pub(in crate::handler) release: tokio::sync::Notify,
}

#[cfg(test)]
static KILL_SESSION_WEB_PRUNE_PAUSE: std::sync::Mutex<
    Option<(SessionName, std::sync::Arc<KillSessionWebPrunePause>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
#[derive(Debug, Default)]
pub(in crate::handler) struct KillSessionSubscriptionRekeyPause {
    pub(in crate::handler) reached: tokio::sync::Notify,
    pub(in crate::handler) release: tokio::sync::Notify,
}

#[cfg(test)]
static KILL_SESSION_SUBSCRIPTION_REKEY_PAUSE: std::sync::Mutex<
    Option<(
        SessionName,
        std::sync::Arc<KillSessionSubscriptionRekeyPause>,
    )>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
#[derive(Debug, Default)]
pub(in crate::handler) struct KillSessionSelectionIdentityPause {
    pub(in crate::handler) reached: tokio::sync::Notify,
    pub(in crate::handler) release: tokio::sync::Notify,
}

#[cfg(test)]
static KILL_SESSION_SELECTION_IDENTITY_PAUSE: std::sync::Mutex<
    Vec<(
        SessionName,
        std::sync::Arc<KillSessionSelectionIdentityPause>,
    )>,
> = std::sync::Mutex::new(Vec::new());

use client_environment::{
    new_session_client_environment, new_session_raw_client_environment,
    raw_environment_from_assignments,
};
use list::{sort_list_sessions, ListSessionSnapshot};
use options::resolve_session_creation_options;

use super::attach_support::{surviving_attached_resize_targets, SessionDetachOnDestroy};
use super::scripting_support::format_context_for_target;
use super::target_support::{pane_id_target, requester_environment_pane_id};
use super::{
    command_output_from_lines, initial_session_spawn_environment, parse_session_sort_order,
    prepare_lifecycle_event, resolve_existing_session_target, update_environment_from_client,
    PaneOutputSubscriptionKeySnapshot, PendingShutdownReason, RequestHandler, SessionSortOrder,
};

impl RequestHandler {
    #[cfg(test)]
    pub(in crate::handler) fn install_rename_session_control_commit_pause(
        &self,
        session_name: SessionName,
    ) -> std::sync::Arc<RenameSessionControlCommitPause> {
        let pause = std::sync::Arc::new(RenameSessionControlCommitPause::default());
        let mut pauses = RENAME_SESSION_CONTROL_COMMIT_PAUSE
            .lock()
            .expect("rename-session control commit pause lock");
        pauses.retain(|(paused_session, _)| paused_session != &session_name);
        pauses.push((session_name, pause.clone()));
        pause
    }

    #[cfg(test)]
    async fn pause_before_rename_session_control_commit(&self, session_name: &SessionName) {
        let pause = RENAME_SESSION_CONTROL_COMMIT_PAUSE
            .lock()
            .expect("rename-session control commit pause lock")
            .iter()
            .find(|(paused_session, _)| paused_session == session_name)
            .map(|(_, pause)| pause.clone());
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_one();
        pause.release.notified().await;
        RENAME_SESSION_CONTROL_COMMIT_PAUSE
            .lock()
            .expect("rename-session control commit pause lock")
            .retain(|(paused_session, current)| {
                paused_session != session_name || !std::sync::Arc::ptr_eq(current, &pause)
            });
    }

    #[cfg(test)]
    pub(in crate::handler) fn install_kill_session_selection_identity_pause(
        &self,
        session_name: SessionName,
    ) -> std::sync::Arc<KillSessionSelectionIdentityPause> {
        let pause = std::sync::Arc::new(KillSessionSelectionIdentityPause::default());
        let mut pauses = KILL_SESSION_SELECTION_IDENTITY_PAUSE
            .lock()
            .expect("kill-session selection identity pause lock");
        pauses.retain(|(paused_session, _)| paused_session != &session_name);
        pauses.push((session_name, pause.clone()));
        pause
    }

    #[cfg(test)]
    async fn pause_after_kill_session_selection_identity_capture(
        &self,
        session_name: &SessionName,
    ) {
        // Consume the pause before waiting so a nested kill of the same
        // session can make progress while this request is suspended. The
        // deterministic race hook is intentionally one-shot.
        let pause = {
            let mut pauses = KILL_SESSION_SELECTION_IDENTITY_PAUSE
                .lock()
                .expect("kill-session selection identity pause lock");
            pauses
                .iter()
                .position(|(paused_session, _)| paused_session == session_name)
                .map(|index| pauses.remove(index).1)
        };
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_one();
        pause.release.notified().await;
    }

    #[cfg(test)]
    pub(in crate::handler) fn install_kill_session_subscription_rekey_pause(
        &self,
        session_name: SessionName,
    ) -> std::sync::Arc<KillSessionSubscriptionRekeyPause> {
        let pause = std::sync::Arc::new(KillSessionSubscriptionRekeyPause::default());
        *KILL_SESSION_SUBSCRIPTION_REKEY_PAUSE
            .lock()
            .expect("kill-session subscription rekey pause lock") =
            Some((session_name, pause.clone()));
        pause
    }

    #[cfg(test)]
    async fn pause_before_kill_session_subscription_rekey(&self, session_name: &SessionName) {
        let pause = KILL_SESSION_SUBSCRIPTION_REKEY_PAUSE
            .lock()
            .expect("kill-session subscription rekey pause lock")
            .as_ref()
            .filter(|(paused_session, _)| paused_session == session_name)
            .map(|(_, pause)| pause.clone());
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_one();
        pause.release.notified().await;
        let mut installed = KILL_SESSION_SUBSCRIPTION_REKEY_PAUSE
            .lock()
            .expect("kill-session subscription rekey pause lock");
        if installed
            .as_ref()
            .is_some_and(|(_, current)| std::sync::Arc::ptr_eq(current, &pause))
        {
            installed.take();
        }
    }

    // Its one caller is `handler_web_tests.rs`, which is itself behind `web`; without the feature
    // there is no Web prune to pause.
    #[cfg(all(test, feature = "web"))]
    pub(in crate::handler) fn install_kill_session_web_prune_pause(
        &self,
        session_name: SessionName,
    ) -> std::sync::Arc<KillSessionWebPrunePause> {
        let pause = std::sync::Arc::new(KillSessionWebPrunePause::default());
        *KILL_SESSION_WEB_PRUNE_PAUSE
            .lock()
            .expect("kill-session Web prune pause lock") = Some((session_name, pause.clone()));
        pause
    }

    #[cfg(test)]
    async fn pause_before_kill_session_web_prune(
        &self,
        removed_sessions: &[(SessionName, SessionId)],
    ) {
        let pause = KILL_SESSION_WEB_PRUNE_PAUSE
            .lock()
            .expect("kill-session Web prune pause lock")
            .as_ref()
            .filter(|(paused_session, _)| {
                removed_sessions
                    .iter()
                    .any(|(session_name, _)| session_name == paused_session)
            })
            .map(|(_, pause)| pause.clone());
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_one();
        pause.release.notified().await;
        let mut installed = KILL_SESSION_WEB_PRUNE_PAUSE
            .lock()
            .expect("kill-session Web prune pause lock");
        if installed
            .as_ref()
            .is_some_and(|(_, current)| std::sync::Arc::ptr_eq(current, &pause))
        {
            *installed = None;
        }
    }

    #[cfg(test)]
    pub(in crate::handler) fn install_rename_session_identity_pause(
        &self,
        session_name: SessionName,
    ) -> std::sync::Arc<RenameSessionIdentityPause> {
        let pause = std::sync::Arc::new(RenameSessionIdentityPause::default());
        *RENAME_SESSION_IDENTITY_PAUSE
            .lock()
            .expect("rename-session identity pause lock") = Some((session_name, pause.clone()));
        pause
    }

    #[cfg(test)]
    async fn pause_after_rename_session_identity_capture(&self, session_name: &SessionName) {
        let pause = RENAME_SESSION_IDENTITY_PAUSE
            .lock()
            .expect("rename-session identity pause lock")
            .as_ref()
            .filter(|(paused_session, _)| paused_session == session_name)
            .map(|(_, pause)| pause.clone());
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_one();
        pause.release.notified().await;
        let mut installed = RENAME_SESSION_IDENTITY_PAUSE
            .lock()
            .expect("rename-session identity pause lock");
        if installed.as_ref().is_some_and(|(paused_session, current)| {
            paused_session == session_name && std::sync::Arc::ptr_eq(current, &pause)
        }) {
            *installed = None;
        }
    }

    pub(in crate::handler) async fn destroy_unattached_sessions_for_option_scope(
        &self,
        scope: &OptionScopeSelector,
    ) {
        let mut candidates = {
            let state = self.state.lock().await;
            destroy_unattached_candidate_sessions(&state, scope)
        };
        self.destroy_unattached_sessions(std::mem::take(&mut candidates))
            .await;
    }

    pub(in crate::handler) async fn destroy_unattached_sessions(
        &self,
        mut candidates: Vec<(SessionName, SessionId)>,
    ) {
        candidates.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        candidates.dedup();

        for (session_name, session_id) in candidates {
            if !self
                .session_should_destroy_when_unattached(&session_name, session_id)
                .await
            {
                continue;
            }
            if self
                .attached_count_for_session_identity(&session_name, session_id)
                .await
                != 0
            {
                continue;
            }
            let _ = self
                .handle_kill_session_identity(
                    KillSessionRequest {
                        target: session_name,
                        kill_all_except_target: false,
                        clear_alerts: false,
                        kill_group: false,
                    },
                    session_id,
                )
                .await;
        }
        self.refresh_hook_identity_aliases().await;
    }

    async fn session_should_destroy_when_unattached(
        &self,
        session_name: &SessionName,
        session_id: SessionId,
    ) -> bool {
        let state = self.state.lock().await;
        state
            .sessions
            .session(session_name)
            .is_some_and(|session| session.id() == session_id)
            && state
                .options
                .resolve(Some(session_name), OptionName::DestroyUnattached)
                == Some("on")
    }

    pub(in crate::handler) async fn handle_new_session(
        &self,
        requester_pid: u32,
        request: rmux_proto::NewSessionRequest,
    ) -> Response {
        self.handle_new_session_ext(
            requester_pid,
            NewSessionExtRequest {
                session_name: Some(request.session_name),
                working_directory: None,
                detached: request.detached,
                size: request.size,
                environment: request.environment,
                group_target: None,
                attach_if_exists: false,
                detach_other_clients: false,
                kill_other_clients: false,
                flags: None,
                window_name: None,
                print_session_info: false,
                print_format: None,
                command: None,
                process_command: None,
                client_environment: None,
                skip_environment_update: false,
            },
        )
        .await
    }

    pub(in crate::handler) async fn handle_new_session_ext(
        &self,
        requester_pid: u32,
        request: NewSessionExtRequest,
    ) -> Response {
        if request.group_target.is_some()
            && (request.window_name.is_some() || request.command.is_some())
        {
            return Response::Error(ErrorResponse {
                error: RmuxError::Server("command or window name given with target".to_owned()),
            });
        }
        let client_environment = match new_session_client_environment(
            requester_pid,
            request.client_environment.as_deref(),
        ) {
            Ok(environment) => environment,
            Err(error) => return Response::Error(ErrorResponse { error }),
        };
        let spawn_environment = initial_session_spawn_environment(client_environment.as_ref());
        let raw_spawn_environment = if request.client_environment.is_some() {
            client_environment
                .as_ref()
                .map(raw_environment_from_assignments)
        } else {
            new_session_raw_client_environment(requester_pid)
        };

        if request.attach_if_exists && request.group_target.is_none() {
            if let Some(existing) = request.session_name.as_ref() {
                let existing_session_id = {
                    let state = self.state.lock().await;
                    state.sessions.session(existing).map(rmux_core::Session::id)
                };
                if let Some(session_id) = existing_session_id {
                    let mut session_name = existing.clone();
                    self.pause_before_created_session_control_attach(&session_name)
                        .await;
                    if !request.skip_environment_update {
                        let mut state = self.state.lock().await;
                        let Some(current_name) =
                            state.sessions.iter().find_map(|(name, session)| {
                                (session.id() == session_id).then(|| name.clone())
                            })
                        else {
                            return Response::Error(ErrorResponse {
                                error: crate::pane_terminals::session_not_found(&session_name),
                            });
                        };
                        if let Some(client_environment) = client_environment.as_ref() {
                            update_environment_from_client(
                                &mut state,
                                &current_name,
                                client_environment,
                            );
                        }
                        session_name = current_name;
                    }
                    if !request.detached
                        && (request.detach_other_clients || request.kill_other_clients)
                    {
                        if let Err(error) = self
                            .detach_other_attach_clients_for_session_identity(
                                &session_name,
                                session_id,
                                requester_pid,
                                request.kill_other_clients,
                            )
                            .await
                        {
                            return Response::Error(ErrorResponse { error });
                        }
                    }
                    self.prepare_existing_session_control_attach(
                        requester_pid,
                        &session_name,
                        session_id,
                    )
                    .await;
                    self.queue_suppressed_inline_hook(
                        HookName::AfterNewSession,
                        PendingInlineHookFormat::AfterCommand,
                    );
                    return Response::NewSession(NewSessionResponse {
                        session_name,
                        detached: false,
                        output: None,
                    });
                }
            }
        }

        let requested_size = request.size;
        let detached = request.detached;
        let environment_overrides = request.environment;
        let environment_assignments = match environment_overrides.as_deref() {
            Some(overrides) => match parse_environment_assignments(overrides) {
                Ok(assignments) => Some(assignments),
                Err(error) => return Response::Error(ErrorResponse { error }),
            },
            None => None,
        };
        let group_target = request.group_target;
        let working_directory = request.working_directory;
        let requester_cwd_pane_id = if working_directory
            .as_ref()
            .is_some_and(|path| path.contains("#{"))
        {
            let socket_path = self.socket_path();
            requester_environment_pane_id(requester_pid, &socket_path)
        } else {
            None
        };
        let requester_cwd_target = match requester_cwd_pane_id {
            Some(pane_id) => {
                let state = self.state.lock().await;
                pane_id_target(&state.sessions, pane_id)
            }
            None => None,
        };
        let requester_cwd_attached_count = match requester_cwd_target.as_ref() {
            Some(target) => self.attached_count(target.session_name()).await,
            None => 0,
        };
        let working_directory_uses_requester_context = requester_cwd_target.is_some();
        let command = request.command;
        let requested_process_command = request
            .process_command
            .or_else(|| crate::legacy_command::from_legacy_command(command.as_deref()));
        let requested_name = request.session_name;
        let socket_path = self.socket_path();
        let mut planned_initial_terminal = None;
        // The initial pane's job is opened between the locked phases below. Serialized against
        // every other pane creation, because each of them releases the request mutex across that
        // spawn and would otherwise be able to roll back a layout the other had already changed.
        let creation = self.pane_creation_transaction().await;
        let (response, silence_template_session, created_session_id) = {
            let mut state = self.state.lock().await;
            let creation_options = resolve_session_creation_options(
                &state.options,
                requested_size,
                requested_process_command,
            );
            if let Err(error) = validate_process_command(creation_options.process_command.as_ref())
            {
                return Response::Error(ErrorResponse { error });
            }
            let size = creation_options.size;
            let base_index = creation_options.base_index;
            let process_command = creation_options.process_command;
            let working_directory = match (requester_cwd_target.as_ref(), working_directory) {
                (Some(target), Some(template)) => {
                    let context = match format_context_for_target(
                        &state,
                        target,
                        requester_cwd_attached_count,
                    ) {
                        Ok(context) => context,
                        Err(error) => return Response::Error(ErrorResponse { error }),
                    };
                    Some(render_runtime_template(&template, &context, false))
                }
                (_, working_directory) => working_directory,
            };
            let (session_name, created_group) = match (requested_name.clone(), group_target.clone())
            {
                (Some(session_name), Some(group_target)) => {
                    let created_group = match state.sessions.create_grouped_session_with_base_index(
                        session_name.clone(),
                        size,
                        base_index,
                        group_target,
                    ) {
                        Ok(created) => created,
                        Err(error) => return Response::Error(ErrorResponse { error }),
                    };
                    (session_name, Some(created_group))
                }
                (Some(session_name), None) => {
                    if let Err(error) = state.sessions.create_session_with_base_index(
                        session_name.clone(),
                        size,
                        base_index,
                    ) {
                        return Response::Error(ErrorResponse { error });
                    }
                    (session_name, None)
                }
                (None, Some(group_target)) => {
                    let created_group = match state
                        .sessions
                        .create_auto_grouped_session_with_base_index(size, base_index, group_target)
                    {
                        Ok(created) => created,
                        Err(error) => return Response::Error(ErrorResponse { error }),
                    };
                    (created_group.session_name.clone(), Some(created_group))
                }
                (None, None) => {
                    let session_name = match state
                        .sessions
                        .create_auto_named_session_with_base_index(size, base_index)
                    {
                        Ok(session_name) => session_name,
                        Err(error) => return Response::Error(ErrorResponse { error }),
                    };
                    (session_name, None)
                }
            };

            if let Some(window_name) = request.window_name.as_ref() {
                let active_window = state
                    .sessions
                    .session(&session_name)
                    .map(|session| session.active_window_index())
                    .expect("newly created session must exist");
                if let Some(session) = state.sessions.session_mut(&session_name) {
                    session
                        .rename_window(active_window, window_name.clone())
                        .expect("newly created session must accept an initial window name");
                }
            }

            if !request.skip_environment_update {
                if let Some(client_environment) = client_environment.as_ref() {
                    update_environment_from_client(&mut state, &session_name, client_environment);
                }
            }
            if let Some(environment_assignments) = environment_assignments.as_ref() {
                for (name, value) in environment_assignments {
                    state.environment.set(
                        ScopeSelector::Session(session_name.clone()),
                        name.clone(),
                        value.clone(),
                    );
                }
            }

            if let Some(template) = working_directory.as_deref() {
                let rendered = if working_directory_uses_requester_context {
                    template.to_owned()
                } else {
                    let session = state
                        .sessions
                        .session(&session_name)
                        .expect("newly created session must exist before cwd assignment");
                    let context = RuntimeFormatContext::new(FormatContext::from_session(session))
                        .with_state(&state)
                        .with_session(session);
                    render_runtime_template(template, &context, false)
                };
                let session = state
                    .sessions
                    .session_mut(&session_name)
                    .expect("newly created session must accept cwd assignment");
                session.set_cwd((!rendered.is_empty()).then(|| PathBuf::from(rendered)));
            }

            if let Some(template_session) = created_group
                .as_ref()
                .and_then(|created| created.template_session.as_ref())
                .and_then(|template| state.sessions.session(template))
                .cloned()
            {
                state.synchronize_window_alias_options_from_session(&template_session);
                if let Err(error) =
                    state.synchronize_pane_alias_options_from_session(&template_session)
                {
                    return Response::Error(ErrorResponse { error });
                }
            }

            let needs_terminal = created_group
                .as_ref()
                .map(|created| created.template_session.is_none())
                .unwrap_or(true);
            if needs_terminal {
                let spawn_options = InitialPaneSpawnOptions {
                    socket_path: &socket_path,
                    spawn_environment: spawn_environment.as_ref(),
                    raw_spawn_environment: raw_spawn_environment.as_deref(),
                    environment_overrides: environment_overrides.as_deref(),
                    command: process_command.as_ref(),
                };
                // Planned here, opened once this lock is released. Opening the job awaits a
                // shell build and the facade's admission lock, and awaiting either under the
                // daemon's request mutex would stall every other session and invert against
                // the observation consumer's adoption path.
                match state.plan_initial_session_terminal(&session_name, spawn_options) {
                    Ok(planned) => {
                        planned_initial_terminal = Some((session_name.clone(), planned));
                    }
                    Err(error) => {
                        let _removed = state.sessions.remove_session(&session_name);
                        let _ = state.environment.remove_session(&session_name);
                        return Response::Error(ErrorResponse { error });
                    }
                }
            }
            if request.window_name.is_some() {
                let active_window = state
                    .sessions
                    .session(&session_name)
                    .map(|session| session.active_window_index())
                    .expect("newly created session must still exist");
                let target = WindowTarget::with_window(session_name.clone(), active_window);
                if let Err(error) = state.disable_automatic_rename_for_window(&target) {
                    return Response::Error(ErrorResponse { error });
                }
            }

            let created_session_id = state
                .sessions
                .session(&session_name)
                .expect("newly created session must still exist")
                .id();
            let silence_template_session =
                created_group.and_then(|created| created.template_session);
            (
                Response::NewSession(NewSessionResponse {
                    session_name,
                    detached,
                    output: None,
                }),
                silence_template_session,
                created_session_id,
            )
        };

        if let Some((terminal_session_name, planned)) = planned_initial_terminal {
            let opened = planned.open().await;
            let mut state = self.state.lock().await;
            let outcome = match opened {
                Ok((commit, prepared)) => state.commit_initial_session_terminal(commit, prepared),
                Err(error) => Err(error),
            };
            if let Err(error) = outcome {
                // The session exists but its only pane has no shell, which is not a session
                // anyone can use. Removing it is the same rollback the locked path performed.
                let _removed = state.sessions.remove_session(&terminal_session_name);
                let _ = state.environment.remove_session(&terminal_session_name);
                drop(state);
                return Response::Error(ErrorResponse { error });
            }
        }
        // The transaction ends with the commit, exactly as its own contract states: plan, open,
        // commit and no further. Everything below attaches a control client and runs this
        // session's lifecycle hooks, and a hook is an arbitrary user command that may create a
        // pane of its own. Carrying the creation serialization into that would stall every other
        // session on this daemon for the length of those commands, and close a cycle straight
        // back onto this very guard whenever the work being awaited is itself another creation.
        drop(creation);

        let Response::NewSession(success) = &response else {
            return response;
        };
        let session_name = success.session_name.clone();
        if !detached {
            self.pause_before_created_session_control_attach(&session_name)
                .await;
        }
        if !detached && (request.detach_other_clients || request.kill_other_clients) {
            if let Err(error) = self
                .detach_other_attach_clients_for_session_identity(
                    &session_name,
                    created_session_id,
                    requester_pid,
                    request.kill_other_clients,
                )
                .await
            {
                return Response::Error(ErrorResponse { error });
            }
        }
        super::scripting_support::record_queued_new_session_transition(
            created_session_id,
            session_name.clone(),
        );
        self.finish_new_session_lifecycle(
            requester_pid,
            &session_name,
            created_session_id,
            silence_template_session.as_ref(),
            detached,
        )
        .await;

        if !request.print_session_info {
            return response;
        }

        match self
            .render_new_session_output(created_session_id, request.print_format.as_deref())
            .await
        {
            Ok((current_session_name, output)) => Response::NewSession(NewSessionResponse {
                session_name: current_session_name,
                detached,
                output: Some(output),
            }),
            Err(error) => Response::Error(ErrorResponse { error }),
        }
    }

    pub(in crate::handler) async fn handle_kill_session(
        &self,
        request: rmux_proto::KillSessionRequest,
    ) -> Response {
        let expected_session_id = explicit_session_id_target(&request.target);
        self.handle_kill_session_with_identity(request, expected_session_id, None, None)
            .await
            .into_unconditional_response()
    }

    pub(in crate::handler) async fn handle_kill_session_identity(
        &self,
        request: rmux_proto::KillSessionRequest,
        session_id: SessionId,
    ) -> Response {
        self.handle_kill_session_with_identity(request, Some(session_id), None, None)
            .await
            .into_unconditional_response()
    }

    pub(in crate::handler) async fn handle_kill_session_if_only_linked_window_identity(
        &self,
        request: rmux_proto::KillSessionRequest,
        session_id: SessionId,
        window_id: WindowId,
        kill_if_last: bool,
    ) -> Option<Response> {
        match self
            .handle_kill_session_with_identity(
                request,
                Some(session_id),
                None,
                Some(OnlyWindowPrecondition {
                    window_id,
                    kill_if_last,
                }),
            )
            .await
        {
            KillSessionExecution::Completed(response) => Some(response),
            KillSessionExecution::OnlyLinkedWindowChanged => None,
        }
    }

    pub(in crate::handler) async fn handle_kill_expired_session_lease_identity(
        &self,
        request: rmux_proto::KillSessionRequest,
        session_id: SessionId,
        lease_token: u64,
    ) -> Response {
        self.handle_kill_session_with_identity(request, Some(session_id), Some(lease_token), None)
            .await
            .into_unconditional_response()
    }

    async fn handle_kill_session_with_identity(
        &self,
        request: rmux_proto::KillSessionRequest,
        expected_session_id: Option<SessionId>,
        expected_expired_lease_token: Option<u64>,
        expected_only_window: Option<OnlyWindowPrecondition>,
    ) -> KillSessionExecution {
        let (session_name, selected_session_id) = {
            let state = self.state.lock().await;
            if let Some(session_id) = expected_session_id {
                let Some(session) = state.sessions.session_by_id(session_id) else {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::SessionNotFound(request.target.to_string()),
                    })
                    .into();
                };
                let session_name = session.name().clone();
                if let Err(error) = super::require_expected_session_identity(&state, &session_name)
                {
                    return Response::Error(ErrorResponse { error }).into();
                }
                (session_name, session_id)
            } else {
                match resolve_existing_session_target(
                    &state.sessions,
                    "kill-session",
                    &request.target,
                ) {
                    Ok(session_name) => {
                        if let Err(error) =
                            super::require_expected_session_identity(&state, &session_name)
                        {
                            return Response::Error(ErrorResponse { error }).into();
                        }
                        let session_id = state
                            .sessions
                            .session(&session_name)
                            .expect("resolved session must exist")
                            .id();
                        (session_name, session_id)
                    }
                    Err(error) => return Response::Error(ErrorResponse { error }).into(),
                }
            }
        };

        if request.clear_alerts {
            let (response, refresh_session_name) = {
                let mut state = self.state.lock().await;
                let current_session_name = if expected_session_id.is_some() {
                    let Some(session) = state.sessions.session_by_id(selected_session_id) else {
                        return Response::Error(ErrorResponse {
                            error: RmuxError::SessionNotFound(session_name.to_string()),
                        })
                        .into();
                    };
                    session.name().clone()
                } else {
                    session_name.clone()
                };
                let Some(session) = state.sessions.session_mut(&current_session_name) else {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::SessionNotFound(current_session_name.to_string()),
                    })
                    .into();
                };
                if session.id() != selected_session_id {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::SessionNotFound(current_session_name.to_string()),
                    })
                    .into();
                }
                let window_indexes = session.windows().keys().copied().collect::<Vec<_>>();
                for window_index in window_indexes {
                    if let Some(window) = session.window_at_mut(window_index) {
                        window.clear_alert_flags(WINDOW_ALERTFLAGS);
                    }
                    let _ = session.clear_all_winlink_alert_flags(window_index);
                }
                (
                    Response::KillSession(KillSessionResponse { existed: true }),
                    current_session_name,
                )
            };
            self.refresh_attached_session(&refresh_session_name).await;
            self.refresh_control_session(&refresh_session_name).await;
            return response.into();
        }

        #[cfg(test)]
        self.pause_after_kill_session_selection_identity_capture(&session_name)
            .await;

        let (
            response,
            queued_lifecycle_events,
            removed_pane_ids,
            removed_sessions,
            removed_attached_sessions,
            resize_window_ids,
            subscriptions_removed,
        ) = {
            let mut state = self.state.lock().await;
            let session_name = if expected_session_id.is_some() {
                let Some(session) = state.sessions.session_by_id(selected_session_id) else {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::SessionNotFound(session_name.to_string()),
                    })
                    .into();
                };
                session.name().clone()
            } else {
                session_name.clone()
            };
            if state
                .sessions
                .session(&session_name)
                .is_none_or(|session| session.id() != selected_session_id)
            {
                return Response::Error(ErrorResponse {
                    error: RmuxError::SessionNotFound(session_name.to_string()),
                })
                .into();
            }
            if let Some(expected_window) = expected_only_window {
                let session = state
                    .sessions
                    .session(&session_name)
                    .expect("validated session must exist");
                let mut windows = session.windows().iter();
                let only_window = windows.next();
                let only_window_is_still_linked =
                    only_window.is_some_and(|(window_index, window)| {
                        windows.next().is_none()
                            && window.id() == expected_window.window_id
                            && (expected_window.kill_if_last
                                || state.window_link_count(&session_name, *window_index) > 1)
                    });
                if !only_window_is_still_linked {
                    return KillSessionExecution::OnlyLinkedWindowChanged;
                }
            }
            if let Some(token) = expected_expired_lease_token {
                if !self.claim_expired_session_lease(selected_session_id, token) {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::owned_session_lease_lost(session_name),
                    })
                    .into();
                }
            }
            let sessions_to_remove = if request.kill_all_except_target {
                let mut sessions = state
                    .sessions
                    .iter()
                    .map(|(name, session)| (name.clone(), session.id()))
                    .filter(|(name, _)| name != &session_name)
                    .collect::<Vec<_>>();
                sessions.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
                sessions
            } else if request.kill_group {
                let mut sessions = state.sessions.session_group_members(&session_name);
                if sessions.is_empty() {
                    sessions.push(session_name.clone());
                }
                sessions
                    .into_iter()
                    .filter_map(|name| {
                        let id = state.sessions.session(&name)?.id();
                        Some((name, id))
                    })
                    .collect()
            } else {
                vec![(session_name.clone(), selected_session_id)]
            };
            // kill-session -a may remove unrelated session families, while a
            // grouped owner removal may transfer surviving runtimes. Capture
            // all stable pane identities so both cases reconcile atomically.
            let subscription_keys = PaneOutputSubscriptionKeySnapshot::capture_all(&state);
            let mut queued_events = Vec::new();
            let mut removed_pane_ids = Vec::new();
            let mut removed_sessions: Vec<(SessionName, SessionId)> = Vec::new();
            let mut removed_attached_sessions = Vec::new();
            let mut removed_window_ids = Vec::new();
            let mut response_error = None;

            for (session_name, session_id) in &sessions_to_remove {
                if state
                    .sessions
                    .session(session_name)
                    .is_none_or(|session| session.id() != *session_id)
                {
                    continue;
                }
                let current_runtime_owner = state.sessions.runtime_owner(session_name);
                if current_runtime_owner.as_ref() == Some(session_name)
                    && !state.contains_session_terminals(session_name)
                {
                    return Response::Error(ErrorResponse {
                        error: RmuxError::Server(format!(
                            "missing pane terminals for session {}",
                            session_name
                        )),
                    })
                    .into();
                }
            }

            for (session_name, session_id) in &sessions_to_remove {
                if state
                    .sessions
                    .session(session_name)
                    .is_none_or(|session| session.id() != *session_id)
                {
                    continue;
                }
                let current_runtime_owner = state.sessions.runtime_owner(session_name);
                let next_runtime_owner = state.sessions.runtime_owner_transfer_target(session_name);
                let detach_on_destroy = SessionDetachOnDestroy::capture(&state, session_name);

                match state.sessions.remove_session(session_name) {
                    Ok(removed_session) => {
                        state.retire_removed_lifecycle_targets();
                        removed_window_ids.extend(
                            removed_session
                                .windows()
                                .values()
                                .map(rmux_core::Window::id),
                        );
                        removed_pane_ids.extend(session_pane_ids(&removed_session));
                        removed_sessions.push((session_name.clone(), removed_session.id()));
                        removed_attached_sessions.push((
                            session_name.clone(),
                            removed_session.id(),
                            detach_on_destroy,
                        ));
                        let session_closed = LifecycleEvent::SessionClosed {
                            session_name: session_name.clone(),
                            session_id: Some(removed_session.id().as_u32()),
                        };
                        let implicit_unlink_teardown = expected_only_window.is_some();
                        let mut removal_events =
                            Vec::with_capacity(removed_session.windows().len() + 1);
                        // Explicit kill-session closes the session first. An
                        // implicit unlink teardown lets the window hook consume
                        // its state before the session hook cleans that state up.
                        if !implicit_unlink_teardown {
                            removal_events.push(session_closed.clone());
                        }
                        for (window_index, window) in removed_session.windows() {
                            removal_events.push(LifecycleEvent::WindowUnlinked {
                                session_name: session_name.clone(),
                                target: Some(WindowTarget::with_window(
                                    session_name.clone(),
                                    *window_index,
                                )),
                                window_id: Some(window.id().as_u32()),
                                window_name: Some(window.name().unwrap_or_default().to_owned()),
                            });
                        }
                        if implicit_unlink_teardown {
                            removal_events.push(session_closed);
                        }
                        for event in removal_events {
                            queued_events.push(prepare_lifecycle_event(&mut state, &event));
                        }
                        let _ = state.options.remove_session(session_name);
                        let _ = state.environment.remove_session(session_name);
                        let _ = state.hooks.remove_session(session_name);
                        if let Err(error) = state.remove_session_terminals(
                            session_name,
                            current_runtime_owner.as_ref(),
                            next_runtime_owner.as_ref(),
                        ) {
                            response_error = Some(error);
                            break;
                        }
                    }
                    Err(RmuxError::SessionNotFound(_)) => {}
                    Err(error) => {
                        response_error = Some(error);
                        break;
                    }
                }
            }

            let response = response_error.map_or_else(
                || Response::KillSession(KillSessionResponse { existed: true }),
                |error| Response::Error(ErrorResponse { error }),
            );
            let removed_pane_ids = state.pane_ids_no_longer_referenced(removed_pane_ids);
            self.record_panes_closed_as_killed(&removed_pane_ids);
            #[cfg(test)]
            self.pause_before_kill_session_subscription_rekey(&session_name)
                .await;
            // Keep removals and owner rekeys in the same state -> registry
            // transaction. Apply the committed delta even when a later
            // multi-session removal made the aggregate response an error.
            let subscriptions_removed = self.apply_pane_output_subscription_reconciliation(
                subscription_keys.reconcile_after(&state),
            );
            (
                response,
                queued_events,
                removed_pane_ids,
                removed_sessions,
                removed_attached_sessions,
                removed_window_ids,
                subscriptions_removed,
            )
        };

        if subscriptions_removed {
            let _ = self.request_shutdown_if_pending();
        }

        // Rehome control clients before publishing SessionClosed effects, but
        // defer their later ClientSessionChanged tickets until the teardown
        // batch reserved by the state mutation has been published.
        let mut prepared_rehomes = Vec::with_capacity(removed_attached_sessions.len());
        for (removed_session_name, removed_session_id, detach_on_destroy) in
            removed_attached_sessions
        {
            prepared_rehomes.push(
                self.prepare_destroy_session_rehome(
                    &removed_session_name,
                    removed_session_id,
                    detach_on_destroy,
                )
                .await,
            );
        }

        for event in queued_lifecycle_events {
            self.emit_prepared(event).await;
        }
        for prepared in &mut prepared_rehomes {
            for event in prepared.control_lifecycle_events.drain(..) {
                self.emit_prepared(event).await;
            }
        }
        for prepared in prepared_rehomes {
            self.exit_prepared_attached_session_identity(prepared).await;
        }

        #[cfg(test)]
        self.pause_before_kill_session_web_prune(&removed_sessions)
            .await;
        #[cfg(all(unix, feature = "web"))]
        {
            self.web_shares.remove_targets_for_panes(&removed_pane_ids);
            self.web_shares
                .remove_targets_for_sessions(&removed_sessions);
        }
        #[cfg(not(all(unix, feature = "web")))]
        let _ = &removed_sessions;
        if !removed_pane_ids.is_empty() {
            self.forget_pane_snapshot_coalescers(&removed_pane_ids);
        }
        self.remove_session_leases(&removed_sessions);
        let removed_session_names = removed_sessions
            .iter()
            .map(|(session_name, _)| session_name.clone())
            .collect::<Vec<_>>();
        for removed_session_name in &removed_session_names {
            self.cancel_session_silence_timers(removed_session_name)
                .await;
        }
        let (resize_targets, refresh_sessions) = {
            let state = self.state.lock().await;
            let resize_targets = surviving_attached_resize_targets(&state, resize_window_ids);
            let mut refresh_sessions = resize_targets
                .iter()
                .flat_map(|target| {
                    state.window_linked_session_family_list(
                        target.session_name(),
                        target.window_index(),
                    )
                })
                .collect::<Vec<_>>();
            refresh_sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            refresh_sessions.dedup();
            (resize_targets, refresh_sessions)
        };
        for resize_target in resize_targets {
            let _ = self
                .reconcile_attached_window_size_and_emit(&resize_target)
                .await;
        }
        for refresh_session in refresh_sessions {
            self.refresh_attached_session(&refresh_session).await;
        }

        let _ = self.queue_shutdown_if_server_empty().await;

        response.into()
    }

    pub(in crate::handler) async fn handle_rename_session(
        &self,
        request: rmux_proto::RenameSessionRequest,
    ) -> Response {
        let (session_name, session_id) = {
            let state = self.state.lock().await;
            let session_name = match resolve_existing_session_target(
                &state.sessions,
                "rename-session",
                &request.target,
            ) {
                Ok(session_name) => session_name,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            let session_id = state
                .sessions
                .session(&session_name)
                .expect("resolved session must exist")
                .id();
            (session_name, session_id)
        };
        #[cfg(test)]
        self.pause_after_rename_session_identity_capture(&session_name)
            .await;
        let new_name = request.new_name;
        if session_name == new_name {
            return Response::RenameSession(rmux_proto::RenameSessionResponse { session_name });
        }
        let response = {
            let mut state = self.state.lock().await;
            if state
                .sessions
                .session(&session_name)
                .is_none_or(|session| session.id() != session_id)
            {
                return Response::Error(ErrorResponse {
                    error: RmuxError::SessionNotFound(session_name.to_string()),
                });
            }
            if state.sessions.contains_session(&new_name) {
                return Response::Error(ErrorResponse {
                    error: RmuxError::DuplicateSession(new_name.to_string()),
                });
            }
            let previous_subscription_keys = {
                let mut seen = std::collections::HashSet::new();
                session_pane_ids(
                    state
                        .sessions
                        .session(&session_name)
                        .expect("validated rename source must exist"),
                )
                .into_iter()
                .filter(|pane_id| seen.insert(*pane_id))
                .filter_map(|pane_id| state.pane_output_subscription_key_for_pane_id(pane_id))
                .collect::<Vec<_>>()
            };

            match state.rename_session(&session_name, &new_name) {
                Ok(()) => {
                    let subscription_rekeys = previous_subscription_keys
                        .into_iter()
                        .filter_map(|previous| {
                            state
                                .pane_output_subscription_key_for_pane_id(previous.pane_id())
                                .filter(|current| current != &previous)
                                .map(|current| (previous, current))
                        })
                        .collect::<Vec<_>>();
                    // The live subscribe path takes the same state ->
                    // subscriptions lock order, so no late old-name record
                    // can land after this rename commits its rekeys.
                    self.rekey_pane_output_subscriptions(&subscription_rekeys);
                    self.rekey_retained_exited_pane_outputs(&session_name, &new_name, session_id);
                    self.rekey_session_silence_timers_locked(
                        &state,
                        &session_name,
                        &new_name,
                        session_id,
                    );
                    self.rename_session_lease(&session_name, &new_name, session_id);
                    self.rekey_web_session(&session_name, &new_name, session_id);
                    let mut active_attach = self.active_attach.lock().await;
                    active_attach.rename_session(&session_name, session_id, &new_name);
                    drop(active_attach);
                    #[cfg(test)]
                    self.pause_before_rename_session_control_commit(&session_name)
                        .await;
                    self.rename_control_session(&session_name, session_id, &new_name)
                        .await;
                    Response::RenameSession(rmux_proto::RenameSessionResponse {
                        session_name: new_name.clone(),
                    })
                }
                Err(error) => Response::Error(ErrorResponse { error }),
            }
        };
        if matches!(response, Response::RenameSession(_)) {
            let event = LifecycleEvent::SessionRenamed {
                session_name: new_name.clone(),
            };
            self.emit_for_session_identity(event, session_id).await;
            self.refresh_attached_session(&new_name).await;
        }

        response
    }

    pub(in crate::handler) async fn handle_list_sessions(
        &self,
        request: rmux_proto::ListSessionsRequest,
    ) -> Response {
        let socket_path = self.socket_path();
        let has_explicit_sort = request.sort_order.is_some();
        let sort_order = match parse_session_sort_order(request.sort_order.as_deref()) {
            Some(sort_order) => sort_order,
            None if request.sort_order.is_some() => {
                return Response::Error(ErrorResponse {
                    error: RmuxError::Server(rmux_core::INVALID_SORT_ORDER.to_owned()),
                });
            }
            None => SessionSortOrder::Name,
        };
        let state = self.state.lock().await;
        let mut sessions = state
            .sessions
            .iter()
            .map(|(session_name, session)| ListSessionSnapshot {
                name: session_name.clone(),
                id: session.id().as_u32(),
                created_at: session.created_at(),
                activity_at: session.activity_at(),
            })
            .collect::<Vec<_>>();
        sort_list_sessions(
            &mut sessions,
            sort_order,
            request.reversed && has_explicit_sort,
        );

        let active_attach = self.active_attach.lock().await;
        let active_control = self.active_control.lock().await;
        let lines = sessions
            .iter()
            .filter_map(|session| state.sessions.session(&session.name))
            .filter_map(|session| {
                let attached_count = active_attach.attached_count(session.name())
                    + active_control.attached_count(session.name());
                let active_window_index = session.active_window_index();
                let active_window = session.window();
                let mut context = FormatContext::from_session(session)
                    .with_session_attached(attached_count)
                    .with_window(active_window_index, active_window, true, false);
                if let Some(pane) = active_window.active_pane() {
                    context = context.with_window_pane(active_window, pane);
                }
                let runtime = RuntimeFormatContext::new(context)
                    .with_state(&state)
                    .with_socket_path(&socket_path)
                    .with_session(session)
                    .with_window(active_window_index, active_window);
                let runtime = match active_window.active_pane() {
                    Some(pane) => runtime.with_pane(pane),
                    None => runtime,
                };
                if let Some(filter) = request.filter.as_deref() {
                    let expanded = render_runtime_template(filter, &runtime, false);
                    if !is_truthy(&expanded) {
                        return None;
                    }
                }

                Some(render_list_sessions_line(
                    &runtime,
                    request.format.as_deref(),
                ))
            })
            .collect::<Vec<_>>();

        Response::ListSessions(ListSessionsResponse {
            output: command_output_from_lines(&lines),
        })
    }

    /// Queues `exit-empty` if the last session has gone, and fires it if it may fire now.
    ///
    /// The queueing step waits for this emptying's own teardown first — see
    /// [`Self::wait_for_emptied_server_to_go_quiet`] — so the evaluation here is not racing the
    /// panes the caller has just killed.
    pub(crate) async fn request_shutdown_if_server_empty(&self) -> bool {
        if !self.queue_shutdown_if_server_empty().await {
            return false;
        }

        self.request_shutdown_if_pending()
    }

    pub(in crate::handler) async fn queue_shutdown_if_server_empty(&self) -> bool {
        // Queued unconditionally when the last session disappears, native work or not: the
        // request records a fact about the UI, and dropping it here would mean a daemon that had
        // one headless helper running at the wrong moment never honoured `exit-empty` at all.
        // Whether it may actually fire is decided later, and re-decided on retry, by
        // `pending_idle_shutdown_state`.
        let should_shutdown = {
            let state = self.state.lock().await;
            state.sessions.is_empty()
                && matches!(
                    state.options.resolve(None, OptionName::ExitEmpty),
                    Some("on")
                )
        };
        if !should_shutdown {
            return false;
        }
        self.queue_shutdown_request(PendingShutdownReason::ExitEmpty);
        // Scheduled, never awaited here. The reason is queued and this returns immediately, so a
        // `kill-session` or `kill-window` answers at once even when a native lease or a headless
        // helper keeps the daemon busy — blocking the mutation on a teardown it started would add
        // the whole grace to every such response. The owned waiter below re-evaluates once the
        // daemon is genuinely quiet.
        self.schedule_emptied_server_shutdown_retry();
        true
    }

    /// Re-evaluates the queued shutdown once the teardown this emptying started has finished.
    ///
    /// The mutation that emptied this server terminated its panes on the way through, and a
    /// terminated pane's job stays *live work* by the idle check's own honest measure until the
    /// engine has closed it and its delivery workers have drained — which happens on the daemon's
    /// runtime, after this mutation has answered. Without this wait the very next evaluation sees
    /// the pane the caller just killed, defers, and leaves `exit-empty` to a retry: the daemon
    /// waiting on itself.
    ///
    /// [`ShellHandle::wait_closed`](marsh_core::shellmux::ShellHandle::wait_closed) is
    /// deliberately not the boundary, and this is the trap worth naming: it resolves when the
    /// snapshot is reclaimed, which is *before* the delivery workers drain and before the instance
    /// is retired. Waiting on it would return while the job was still counted.
    ///
    /// Bounded, so a teardown that has wedged cannot wedge the request that asked for it. An
    /// expired bound changes nothing else: the evaluation that follows still sees the live
    /// instance, still defers, and still schedules the retry that decides later — which is
    /// precisely what happened before this wait existed. It must never fire regardless; tearing
    /// the daemon down mid-teardown is the failure this exists to avoid, and an explicit
    /// `kill-server` bypasses idle deferral for the case that genuinely cannot wait.
    fn schedule_emptied_server_shutdown_retry(&self) {
        let Some(io) = self.shell_io() else {
            return;
        };
        let handler = self.clone();
        io.runtime().spawn(async move {
            let _ = tokio::time::timeout(EMPTY_SERVER_QUIET_GRACE, io.wait_quiet()).await;
            handler.request_shutdown_if_pending();
        });
    }
}

fn destroy_unattached_candidate_sessions(
    state: &crate::pane_terminals::HandlerState,
    scope: &OptionScopeSelector,
) -> Vec<(SessionName, SessionId)> {
    match scope {
        OptionScopeSelector::ServerGlobal
        | OptionScopeSelector::SessionGlobal
        | OptionScopeSelector::WindowGlobal => state
            .sessions
            .iter()
            .map(|(session_name, session)| (session_name.clone(), session.id()))
            .collect(),
        OptionScopeSelector::Session(session_name) => state
            .sessions
            .session(session_name)
            .map(|session| vec![(session_name.clone(), session.id())])
            .unwrap_or_default(),
        OptionScopeSelector::Window(target) => state
            .sessions
            .session(target.session_name())
            .map(|session| vec![(target.session_name().clone(), session.id())])
            .unwrap_or_default(),
        OptionScopeSelector::Pane(target) => state
            .sessions
            .session(target.session_name())
            .map(|session| vec![(target.session_name().clone(), session.id())])
            .unwrap_or_default(),
    }
}

fn session_pane_ids(session: &rmux_core::Session) -> Vec<PaneId> {
    session
        .windows()
        .values()
        .flat_map(|window| window.panes().iter().map(|pane| pane.id()))
        .collect()
}

fn explicit_session_id_target(target: &SessionName) -> Option<SessionId> {
    let raw_id = target.as_str().strip_prefix('$')?;
    if raw_id.is_empty() || !raw_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw_id.parse::<u32>().ok().map(SessionId::new)
}
