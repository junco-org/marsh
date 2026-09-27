use rmux_client::Connection;
use rmux_proto::{
    PaneId, PaneSnapshotResponse, PaneTarget, PaneTargetRef, ResolveTargetType, Response,
    SessionId, SessionName, Target, encode_internal_pane_exit_probe,
};

use crate::cli_args::TargetSpec;

use super::super::{ExitFailure, resolve_pane_target_or_current};
use super::common::{
    command_error, list_panes_output, pane_snapshot, parse_i32_field, parse_pane_id,
    protocol_mismatch, slot_row_fields, target_kind_name,
};
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
                return Ok(SessionNameRefresh::Gone(command_error(
                    command_name,
                    &error,
                )));
            }
            other => return Err(protocol_mismatch(&other, "while refreshing a pane wait")),
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
    let slot = resolve_pane_target_or_current(connection, target, command_name)?;
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
    let output = list_panes_output(
        connection,
        target.session_name().clone(),
        Some(target.window_index()),
        "#{pane_index}\t#{pane-base-index}\t#{pane_id}\t#{session_id}\n".to_owned(),
        "while resolving pane wait identity",
    )?
    .map_err(|error| command_error(command_name, &error))?;
    let listing = String::from_utf8_lossy(output.stdout());
    slot_row_fields(&listing, target)
        .and_then(|mut fields| {
            let pane_id = fields.next().and_then(parse_pane_id)?;
            Some((fields.next().and_then(parse_session_id)?, pane_id))
        })
        .ok_or_else(|| {
            ExitFailure::new(
                1,
                format!("unable to resolve stable pane identity for target {target}"),
            )
        })
}

/// Runs one internal pane exit probe and decodes the pane's liveness and exit status.
fn query_process_state(
    connection: &mut Connection,
    target: &StableWaitTarget,
) -> Result<ProcessLookup, ExitFailure> {
    let Ok(output) = list_panes_output(
        connection,
        target.session_name.clone(),
        None,
        encode_internal_pane_exit_probe(target.session_id, target.pane_id),
        "while reading pane process state",
    )?
    else {
        return Ok(ProcessLookup::TargetUnavailable);
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

/// Parses a `$`-prefixed session id such as `$1`.
fn parse_session_id(value: &str) -> Option<SessionId> {
    value
        .strip_prefix('$')?
        .parse::<u32>()
        .ok()
        .map(SessionId::new)
}

/// The failure returned when a pane's session kept being renamed past the retry budget.
fn repeated_rename_error() -> ExitFailure {
    ExitFailure::new(1, "pane target session changed repeatedly while waiting")
}
