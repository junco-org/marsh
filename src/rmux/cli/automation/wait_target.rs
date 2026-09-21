use rmux_client::Connection;
use rmux_proto::{
    encode_internal_pane_exit_probe, PaneId, PaneSnapshotResponse, PaneTarget, PaneTargetRef,
    ResolveTargetType, Response, SessionId, SessionName, Target,
};

use crate::cli_args::TargetSpec;
use crate::cli_response::tmux_cli_error_message;

use super::super::{listed_pane_index_matches_target, ExitFailure};
use super::common::{pane_snapshot, resolve_pane_slot};
use super::pane_exit::PaneExitStatus;

const MAX_RENAME_RETRIES: usize = 8;

/// A pane wait target pinned to durable session and pane ids so renames cannot retarget it.
#[derive(Debug, Clone)]
pub(super) struct StableWaitTarget {
    session_id: SessionId,
    pane_id: PaneId,
    session_name: SessionName,
}

impl StableWaitTarget {
    /// The pane reference used for requests, built from the last known session name.
    pub(super) fn target_ref(&self) -> PaneTargetRef {
        PaneTargetRef::by_id(self.session_name.clone(), self.pane_id)
    }

    /// Refreshes the cached session name first, so the returned reference survives a rename.
    pub(super) fn refreshed_target_ref(
        &mut self,
        connection: &mut Connection,
        command_name: &'static str,
    ) -> Result<PaneTargetRef, ExitFailure> {
        match self.refresh_session_name(connection, command_name)? {
            SessionNameRefresh::Unchanged | SessionNameRefresh::Changed => Ok(self.target_ref()),
            SessionNameRefresh::Gone(error) => Err(error),
        }
    }

    /// Re-resolves the pinned session id to its current name, reporting rename or disappearance.
    fn refresh_session_name(
        &mut self,
        connection: &mut Connection,
        command_name: &'static str,
    ) -> Result<SessionNameRefresh, ExitFailure> {
        let response = connection
            .resolve_target(
                Some(self.session_id.to_string()),
                ResolveTargetType::Session,
                false,
                false,
            )
            .map_err(ExitFailure::from)?;
        let session_name = match response {
            Response::ResolveTarget(response) => match response.target {
                Target::Session(session_name) => session_name,
                target => {
                    return Err(ExitFailure::new(
                        1,
                        format!(
                            "protocol error: resolve-target produced a {} target while refreshing a pane wait",
                            target_kind_name(&target)
                        ),
                    ));
                }
            },
            Response::Error(error) => {
                return Ok(SessionNameRefresh::Gone(ExitFailure::new(
                    1,
                    tmux_cli_error_message(command_name, &error.error),
                )));
            }
            other => {
                return Err(ExitFailure::new(
                    1,
                    format!(
                        "protocol error: unexpected '{}' response while refreshing a pane wait",
                        other.command_name()
                    ),
                ));
            }
        };
        if session_name == self.session_name {
            return Ok(SessionNameRefresh::Unchanged);
        }
        self.session_name = session_name;
        Ok(SessionNameRefresh::Changed)
    }
}

/// Liveness of the waited pane's process, as observed through a pane exit probe.
pub(super) enum StableWaitProcessState {
    Alive,
    Exited {
        status: PaneExitStatus,
        retained: bool,
    },
    TargetGone,
}

/// Outcome of re-resolving a pinned session id to its current name.
enum SessionNameRefresh {
    Unchanged,
    Changed,
    Gone(ExitFailure),
}

/// Outcome of one pane process probe, distinguishing a real state from a stale session name.
enum ProcessLookup {
    State(StableWaitProcessState),
    TargetUnavailable,
}

/// Pins the pane selected by `target` (or the current pane) to stable session and pane ids.
pub(super) fn resolve(
    connection: &mut Connection,
    target: Option<&TargetSpec>,
    command_name: &'static str,
) -> Result<StableWaitTarget, ExitFailure> {
    let slot = resolve_pane_slot(connection, target, command_name)?;
    for_slot(connection, &slot, command_name)
}

/// Pins an already resolved pane slot to stable session and pane ids.
pub(super) fn for_slot(
    connection: &mut Connection,
    slot: &PaneTarget,
    command_name: &'static str,
) -> Result<StableWaitTarget, ExitFailure> {
    let (session_id, pane_id) = pane_identity_for_slot(connection, slot, command_name)?;
    Ok(StableWaitTarget {
        session_id,
        pane_id,
        session_name: slot.session_name().clone(),
    })
}

/// Captures a pane snapshot, retrying across session renames up to `MAX_RENAME_RETRIES`.
pub(super) fn snapshot(
    connection: &mut Connection,
    target: &mut StableWaitTarget,
) -> Result<PaneSnapshotResponse, ExitFailure> {
    for _ in 0..MAX_RENAME_RETRIES {
        match pane_snapshot(connection, target.target_ref()) {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => match target.refresh_session_name(connection, "pane-snapshot")? {
                SessionNameRefresh::Changed => {}
                SessionNameRefresh::Unchanged => return Err(error),
                SessionNameRefresh::Gone(error) => return Err(error),
            },
        }
    }
    Err(repeated_rename_error())
}

/// Probes the pane's process, retrying across session renames and mapping loss to a stale exit.
pub(super) fn process_state(
    connection: &mut Connection,
    target: &mut StableWaitTarget,
) -> Result<StableWaitProcessState, ExitFailure> {
    for _ in 0..MAX_RENAME_RETRIES {
        match query_process_state(connection, target)? {
            ProcessLookup::State(state) => return Ok(state),
            ProcessLookup::TargetUnavailable => {
                match target.refresh_session_name(connection, "wait-pane")? {
                    SessionNameRefresh::Changed => {}
                    SessionNameRefresh::Unchanged => {
                        return Ok(StableWaitProcessState::Exited {
                            status: PaneExitStatus::stale(),
                            retained: false,
                        });
                    }
                    SessionNameRefresh::Gone(_) => {
                        return Ok(StableWaitProcessState::TargetGone);
                    }
                }
            }
        }
    }
    Err(repeated_rename_error())
}

/// Looks up the stable session and pane ids backing an index-based pane slot.
fn pane_identity_for_slot(
    connection: &mut Connection,
    target: &PaneTarget,
    command_name: &'static str,
) -> Result<(SessionId, PaneId), ExitFailure> {
    let response = connection
        .list_panes_in_window(
            target.session_name().clone(),
            Some(target.window_index()),
            Some("#{pane_index}\t#{pane-base-index}\t#{pane_id}\t#{session_id}\n".to_owned()),
        )
        .map_err(ExitFailure::from)?;
    let output = match response {
        Response::ListPanes(response) => response.output,
        Response::Error(error) => {
            return Err(ExitFailure::new(
                1,
                tmux_cli_error_message(command_name, &error.error),
            ));
        }
        other => return Err(unexpected_response(&other, "resolving pane wait identity")),
    };
    let text = String::from_utf8_lossy(output.stdout());
    for line in text.lines() {
        let mut fields = line.split('\t');
        if !listed_pane_index_matches_target(
            target,
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default(),
        ) {
            continue;
        }
        if let Some((pane_id, session_id)) = fields
            .next()
            .and_then(parse_pane_id)
            .zip(fields.next().and_then(parse_session_id))
        {
            return Ok((session_id, pane_id));
        }
        break;
    }
    Err(ExitFailure::new(
        1,
        format!("unable to resolve stable pane identity for target {target}"),
    ))
}

/// Runs one internal pane exit probe and decodes the pane's liveness and exit status.
fn query_process_state(
    connection: &mut Connection,
    target: &StableWaitTarget,
) -> Result<ProcessLookup, ExitFailure> {
    let response = connection
        .list_panes_in_window(
            target.session_name.clone(),
            None,
            Some(encode_internal_pane_exit_probe(
                target.session_id,
                target.pane_id,
            )),
        )
        .map_err(ExitFailure::from)?;
    let output = match response {
        Response::ListPanes(response) => response.output,
        Response::Error(_) => return Ok(ProcessLookup::TargetUnavailable),
        other => return Err(unexpected_response(&other, "reading pane process state")),
    };
    for line in String::from_utf8_lossy(output.stdout()).lines() {
        let mut fields = line.split('\t');
        if fields.next().and_then(parse_pane_id) != Some(target.pane_id) {
            continue;
        }
        let dead = fields.next() == Some("1");
        if !dead {
            return Ok(ProcessLookup::State(StableWaitProcessState::Alive));
        }
        return Ok(ProcessLookup::State(StableWaitProcessState::Exited {
            status: PaneExitStatus::known(
                parse_i32_field(fields.next()),
                parse_i32_field(fields.next()),
            ),
            retained: fields.next() == Some("1"),
        }));
    }
    Ok(ProcessLookup::TargetUnavailable)
}

/// Parses a `%`-prefixed pane id such as `%3`.
fn parse_pane_id(value: &str) -> Option<PaneId> {
    value
        .strip_prefix('%')?
        .parse::<u32>()
        .ok()
        .map(PaneId::new)
}

/// Parses a `$`-prefixed session id such as `$1`.
fn parse_session_id(value: &str) -> Option<SessionId> {
    value
        .strip_prefix('$')?
        .parse::<u32>()
        .ok()
        .map(SessionId::new)
}

/// Parses an optional probe field as `i32`, treating an empty field as absent.
fn parse_i32_field(value: Option<&str>) -> Option<i32> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok())
}

/// The human-readable kind word for a resolved `Target`.
const fn target_kind_name(target: &Target) -> &'static str {
    match target {
        Target::Session(_) => "session",
        Target::Window(_) => "window",
        Target::Pane(_) => "pane",
    }
}

/// Builds the protocol-error failure for a response that does not fit `context`.
fn unexpected_response(response: &Response, context: &str) -> ExitFailure {
    ExitFailure::new(
        1,
        format!(
            "protocol error: unexpected '{}' response while {context}",
            response.command_name()
        ),
    )
}

/// The failure returned when a pane's session kept being renamed past the retry budget.
fn repeated_rename_error() -> ExitFailure {
    ExitFailure::new(1, "pane target session changed repeatedly while waiting")
}
