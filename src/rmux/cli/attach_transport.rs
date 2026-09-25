use std::path::{Path, PathBuf};

use rmux_client::AttachError;
use rmux_client::attach_terminal_with_initial_bytes;
use rmux_client::attach_terminal_with_initial_bytes_and_resize_geometry;
use rmux_client::{AttachSessionUpgrade, AttachTransition, ClientError, Connection, connect};
use rmux_proto::request::{AttachSessionExt2Request, AttachSessionExt3Request, ListClientsRequest};
use rmux_proto::{
    CAPABILITY_ATTACH_RENDER, CAPABILITY_ATTACH_RESIZE_GEOMETRY, ErrorResponse, Response,
};

use crate::client_terminal::ATTACH_TERMINAL_REQUIRED_MESSAGE;

use super::{ExitFailure, expect_command_success, unexpected_response};

/// An accepted attach upgrade held back so the rest of the command queue can run first.
pub(super) struct QueuedAttachSession {
    upgrade: AttachSessionUpgrade,
    capabilities: AttachClientCapabilities,
    socket_path: PathBuf,
}

/// Whether a queued `attach-session` produced a pending upgrade or already finished.
pub(super) enum QueuedAttachSessionResult {
    Detached(Box<QueuedAttachSession>),
    Completed(i32),
}

/// Attach features negotiated with the server for this client.
struct AttachClientCapabilities {
    resize_geometry: bool,
}

impl QueuedAttachSession {
    /// Takes over the terminal for the deferred upgrade and returns the attach exit code.
    pub(super) fn run(self) -> Result<i32, ExitFailure> {
        let Self {
            upgrade,
            capabilities,
            socket_path,
        } = self;
        run_attach_upgrade(upgrade, &capabilities, &socket_path)
    }
}

/// Requests `attach-session` and defers the terminal takeover until the queue has drained.
pub(super) fn begin_queued_attach(
    connection: Connection,
    request: AttachSessionExt2Request,
    socket_path: &Path,
) -> Result<QueuedAttachSessionResult, ExitFailure> {
    require_attach_terminal()?;
    let (transition, capabilities) = begin_attach(connection, request, socket_path)?;
    match transition {
        AttachTransition::Upgraded(upgrade) => Ok(QueuedAttachSessionResult::Detached(Box::new(
            QueuedAttachSession {
                upgrade,
                capabilities,
                socket_path: socket_path.to_path_buf(),
            },
        ))),
        AttachTransition::Rejected(response) => {
            expect_command_success(response, "attach-session")?;
            Ok(QueuedAttachSessionResult::Completed(0))
        }
    }
}

/// Reports whether this process already appears as an attached client on the server.
pub(super) fn queued_attach_session_is_active(socket_path: &Path) -> Result<bool, ExitFailure> {
    let mut connection = match connect(socket_path) {
        Ok(connection) => connection,
        Err(error) => {
            let failure = ExitFailure::from_client_connect(socket_path, error);
            return if failure.is_server_absent() {
                Ok(false)
            } else {
                Err(failure)
            };
        }
    };
    let response = connection
        .list_clients(ListClientsRequest {
            format: Some("#{client_pid}\n".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_session: None,
        })
        .map_err(ExitFailure::from)?;
    match response {
        Response::ListClients(response) => {
            let requester_pid = std::process::id().to_string();
            Ok(response
                .command_output()
                .stdout()
                .split(|byte| *byte == b'\n')
                .any(|line| line == requester_pid.as_bytes()))
        }
        Response::Error(ErrorResponse { error }) => Err(ExitFailure::new(1, error.to_string())),
        response => Err(unexpected_response("list-clients", &response)),
    }
}

/// Attaches immediately over `connection`, running the terminal session to completion.
pub(super) fn attach_with_connection(
    connection: Connection,
    request: AttachSessionExt2Request,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    require_attach_terminal()?;
    let (transition, capabilities) = begin_attach(connection, request, socket_path)?;
    match transition {
        AttachTransition::Upgraded(upgrade) => {
            run_attach_upgrade(upgrade, &capabilities, socket_path)
        }
        AttachTransition::Rejected(response) => {
            expect_command_success(response, "attach-session")?;
            Ok(0)
        }
    }
}

/// Fails with the standard message unless stdio is a terminal suitable for attaching.
pub(super) fn require_attach_terminal() -> Result<(), ExitFailure> {
    crate::client_terminal::require_attach_terminal()
        .map_err(|message| ExitFailure::new(1, message))
}

/// Negotiates attach capabilities and starts the `attach-session` upgrade handshake.
fn begin_attach(
    mut connection: Connection,
    request: AttachSessionExt2Request,
    socket_path: &Path,
) -> Result<(AttachTransition, AttachClientCapabilities), ExitFailure> {
    let resize_geometry = connection
        .supports_capability(CAPABILITY_ATTACH_RESIZE_GEOMETRY)
        .map_err(ExitFailure::from)
        .map_err(|error| {
            error.with_startup_context("query attach resize capability", Some(socket_path))
        })?;
    let render = connection
        .supports_capability(CAPABILITY_ATTACH_RENDER)
        .map_err(ExitFailure::from)
        .map_err(|error| {
            error.with_startup_context("query attach render capability", Some(socket_path))
        })?;
    let mut advertised = Vec::new();
    if render {
        advertised.push(CAPABILITY_ATTACH_RENDER.to_owned());
    }
    let transition = if !advertised.is_empty() {
        connection
            .begin_attach_with_capabilities(AttachSessionExt3Request::from_ext2(
                request, advertised,
            ))
            .map_err(ExitFailure::from)
            .map_err(|error| {
                error.with_startup_context("request terminal attach", Some(socket_path))
            })?
    } else {
        connection
            .begin_attach_with_target_spec(request)
            .map_err(ExitFailure::from)
            .map_err(|error| {
                error.with_startup_context("request terminal attach", Some(socket_path))
            })?
    };
    Ok((transition, AttachClientCapabilities { resize_geometry }))
}

/// Drives the upgraded stream as an attached terminal, honoring negotiated capabilities.
fn run_attach_upgrade(
    upgrade: AttachSessionUpgrade,
    capabilities: &AttachClientCapabilities,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let &AttachClientCapabilities { resize_geometry } = capabilities;
    let (stream, initial_bytes) = upgrade.into_parts();
    if resize_geometry {
        attach_terminal_with_initial_bytes_and_resize_geometry(stream, initial_bytes)
            .map_err(|error| attach_terminal_exit_failure(error, socket_path))?;
    } else {
        attach_terminal_with_initial_bytes(stream, initial_bytes)
            .map_err(|error| attach_terminal_exit_failure(error, socket_path))?;
    }
    Ok(0)
}

/// Maps an attach failure to an exit failure, reporting missing-terminal errors clearly.
fn attach_terminal_exit_failure(error: ClientError, socket_path: &Path) -> ExitFailure {
    if attach_terminal_failed_because_stdio_is_not_terminal(&error) {
        ExitFailure::new(1, ATTACH_TERMINAL_REQUIRED_MESSAGE)
    } else {
        ExitFailure::from(error).with_startup_context("run terminal attach", Some(socket_path))
    }
}

/// Reports whether the attach failed because stdio is not a terminal.
const fn attach_terminal_failed_because_stdio_is_not_terminal(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Attach(AttachError::Termios(errno))
            if matches!(errno.raw_os_error(), libc::ENOTTY | libc::ENODEV)
    )
}
