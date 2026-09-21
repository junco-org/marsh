use std::sync::Arc;

use rmux_core::command_parser::ParsedCommands;
use rmux_proto::{RmuxError, SessionName, Target};

use super::super::lifecycle_support::{LeaseResolution, LifecycleTargetLease};
use super::super::RequestHandler;
use super::format_context::{
    format_context_for_target_with_server_values, parser_with_parse_time_context,
};
use super::parser_context::command_parser_from_state;
use super::queue::QueueExecutionContext;
use crate::hook_runtime::current_hook_formats;
use crate::terminal::TerminalProfile;

/// What the diagnostic log calls work dispatched by [`RequestHandler::spawn_hook_shell_fallback`].
const HOOK_SHELL_FALLBACK: &str = "hook-shell-fallback";

impl RequestHandler {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::handler) async fn execute_hook_command(
        &self,
        requester_pid: u32,
        command: &str,
    ) -> Result<(), RmuxError> {
        self.execute_hook_command_with_context(requester_pid, command, None)
            .await
    }

    pub(in crate::handler) async fn execute_hook_command_with_context(
        &self,
        requester_pid: u32,
        command: &str,
        current_target: Option<Target>,
    ) -> Result<(), RmuxError> {
        self.execute_hook_command_with_target_binding(requester_pid, command, current_target, None)
            .await
    }

    pub(in crate::handler) async fn execute_hook_command_with_target_binding(
        &self,
        requester_pid: u32,
        command: &str,
        current_target: Option<Target>,
        retained_target: Option<Arc<LifecycleTargetLease>>,
    ) -> Result<(), RmuxError> {
        let parsed = self
            .parse_hook_command(command, current_target.as_ref(), retained_target.as_ref())
            .await?;
        let context = QueueExecutionContext::without_caller_cwd();
        let context = if parsed.target_retired {
            // A retired lifecycle target is a tombstone, not a CMD_FIND_CANFAIL
            // fallback. Keeping it implicit makes target-dependent commands fail
            // closed instead of selecting an unrelated live session, while
            // target-independent commands in later hook entries still run.
            context
                .with_implicit_current_target(parsed.current_target)
                .forbid_missing_current_target_fallback()
        } else {
            context.with_current_target(parsed.current_target)
        };
        let execution = self.execute_parsed_commands(
            requester_pid,
            parsed.commands,
            context
                .with_retained_lifecycle_target(retained_target)
                .with_mouse_event(super::queued_command_mouse_event()),
        );
        execution.await.map(|_| ())
    }

    #[allow(dead_code)]
    async fn parse_hook_command(
        &self,
        command: &str,
        current_target: Option<&Target>,
        retained_target: Option<&Arc<LifecycleTargetLease>>,
    ) -> Result<ParsedHookCommand, RmuxError> {
        let (mut parser, current_target, target_retired) = {
            let socket_path = self.socket_path();
            let state = self.state.lock().await;
            let (parser_target, current_target, target_retired) = match retained_target {
                Some(lease) => match lease.resolve(&state) {
                    LeaseResolution::Live(target) => (Some(target.clone()), Some(target), false),
                    LeaseResolution::Retired(snapshot) => {
                        (None, Some(snapshot.original().clone()), true)
                    }
                    LeaseResolution::Replaced => (None, current_target.cloned(), true),
                },
                None => (current_target.cloned(), current_target.cloned(), false),
            };
            let mut parser = command_parser_from_state(&state);
            if let Some(target) = parser_target.as_ref() {
                let context =
                    format_context_for_target_with_server_values(&state, target, 0, &socket_path)?;
                parser = parser_with_parse_time_context(parser, &context);
            }
            (parser, current_target, target_retired)
        };
        for (name, value) in current_hook_formats() {
            parser = parser.with_format_value(name, value);
        }
        match parser.parse_one_group(command) {
            Ok(commands) => Ok(ParsedHookCommand {
                commands,
                current_target,
                target_retired,
            }),
            Err(error) if error.message().starts_with("unknown command: ") => {
                let profile = self
                    .hook_shell_profile(current_target.as_ref(), retained_target)
                    .await?;
                self.spawn_hook_shell_fallback(command, &profile)?;
                Ok(ParsedHookCommand {
                    commands: ParsedCommands::default(),
                    current_target,
                    target_retired,
                })
            }
            Err(error) => Err(super::command_parse_error_to_rmux(error)),
        }
    }

    /// Runs a hook entry that rmux's own grammar does not recognise as a shell command.
    ///
    /// tmux's hook syntax predates rmux's command set, so an unrecognised entry is a shell
    /// command rather than an error. Its presentation is unchanged — fire and forget, output
    /// discarded, no timeout invented for it — but it is no longer an untracked child of the
    /// daemon. It is an admitted managed job: it appears on the helper ledger, shutdown ends it,
    /// and whatever it staged goes through the gate like every other requested workload.
    ///
    /// The refusal of such a job has nowhere to be displayed, by construction, so it is recorded
    /// rather than raised. Turning it into an error here would fail the *hook dispatch*, which
    /// would then skip the later entries that have nothing to do with it.
    ///
    /// Recorded means [`crate::diagnostic_log`], not the `tracing` lines beside it: this
    /// workspace links the `tracing` facade with no subscriber, so those are no-ops. A job that
    /// ran and was refused is written by the verdict log every completion passes through; a job
    /// that never ran, was not admitted, or ended without a verdict has no completion to describe
    /// it, so each of those three is recorded here as `workload-not-run` instead. Without that a
    /// hook entry could be dropped with no trace anywhere — it is the one caller with no error
    /// channel of its own.
    ///
    /// An entry reaches this function only when rmux's grammar rejects it at *dispatch* time,
    /// which registration already forbids: every writer of the hook store canonicalises the body
    /// against the live parser. The reachable case is a `command-alias` that shadows the stored
    /// entry's command name after it was stored, because alias resolution beats the builtin table.
    ///
    /// # Errors
    ///
    /// Fails when the engine is unavailable and when the workload could not be composed — a
    /// directory outside the seed, or an environment carrying non-UTF-8 data.
    fn spawn_hook_shell_fallback(
        &self,
        command: &str,
        profile: &TerminalProfile,
    ) -> Result<(), RmuxError> {
        let io = crate::managed_workload::handler_facade(self)?;
        let spec = crate::managed_workload::spec(
            &io,
            profile.cwd(),
            profile.raw_environment(),
            None,
            rmux_proto::ProcessCommand::Shell(command.to_owned()),
        )?;
        let command = command.to_owned();
        let shell_processes = self.shell_processes.clone();
        // On the daemon's own runtime: a hook can be dispatched from a status thread or a
        // detached queue, and the job it admits has to land on the same executor as everything
        // else the engine owns.
        io.runtime().clone().spawn(async move {
            let execution = match crate::managed_workload::start(&io, spec).await {
                Ok(execution) => execution,
                Err(error) => {
                    crate::diagnostic_log::record_workload_not_run(
                        HOOK_SHELL_FALLBACK,
                        &command,
                        &format!("could not start: {error}"),
                    );
                    tracing::warn!("failed to run legacy shell hook command: {error}");
                    return;
                }
            };
            let guard = shell_processes.register(&io, execution.shell());
            let shell = execution.shell().clone();
            let mut guard = match guard {
                Ok(guard) => Some(guard),
                Err(reason) => {
                    crate::diagnostic_log::record_workload_not_run(
                        HOOK_SHELL_FALLBACK,
                        &command,
                        &format!("not admitted: {reason:?}"),
                    );
                    tracing::warn!("legacy shell hook command rejected: {reason:?}");
                    let _ = io.stop(&shell, true).await;
                    return;
                }
            };
            let collect = execution.collect(crate::managed_workload::DISCARD);
            tokio::pin!(collect);
            let interrupted = async {
                match guard.as_mut() {
                    Some(guard) => guard.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                result = &mut collect => match result {
                    Ok(captured) if !captured.completion.is_published() => {
                        tracing::info!(
                            "legacy shell hook command '{command}' completed without publication"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        crate::diagnostic_log::record_workload_not_run(
                            HOOK_SHELL_FALLBACK,
                            &command,
                            &format!("ended without a verdict: {error}"),
                        );
                        tracing::warn!("legacy shell hook command '{command}' failed: {error}");
                    }
                },
                () = interrupted => {
                    let _ = io.stop(&shell, true).await;
                }
            }
        });
        Ok(())
    }

    async fn hook_shell_profile(
        &self,
        current_target: Option<&Target>,
        retained_target: Option<&Arc<LifecycleTargetLease>>,
    ) -> Result<TerminalProfile, RmuxError> {
        let state = self.state.lock().await;
        let retained_resolution = retained_target.map(|lease| lease.resolve(&state));
        let resolved_target = match retained_resolution.as_ref() {
            Some(LeaseResolution::Live(target)) => Some(target),
            Some(LeaseResolution::Retired(snapshot)) => {
                if let Some(profile) = snapshot.terminal_profile() {
                    return Ok(profile.clone());
                }
                None
            }
            Some(LeaseResolution::Replaced) => {
                return Err(RmuxError::Server(
                    "lifecycle hook target was replaced before shell execution".to_owned(),
                ));
            }
            None => current_target,
        };
        let session_name = target_session_name(resolved_target);
        let session_id = session_name
            .and_then(|name| state.sessions.session(name))
            .map(|session| session.id().as_u32());
        let (base_environment, pane_id) = match resolved_target {
            Some(Target::Pane(target)) => (
                state.session_base_environment_for_pane_target(target),
                state
                    .sessions
                    .session(target.session_name())
                    .and_then(|session| session.window_at(target.window_index()))
                    .and_then(|window| window.pane(target.pane_index()))
                    .map(rmux_core::Pane::id),
            ),
            _ => (None, None),
        };

        TerminalProfile::for_run_shell(
            &state.environment,
            &state.options,
            session_name,
            session_id,
            &self.socket_path(),
            base_environment.as_ref(),
            !self.config_loading_active(),
            pane_id,
            None,
        )
    }
}

struct ParsedHookCommand {
    commands: ParsedCommands,
    current_target: Option<Target>,
    target_retired: bool,
}

fn target_session_name(target: Option<&Target>) -> Option<&SessionName> {
    match target {
        Some(Target::Session(session_name)) => Some(session_name),
        Some(Target::Window(target)) => Some(target.session_name()),
        Some(Target::Pane(target)) => Some(target.session_name()),
        None => None,
    }
}
