use std::io;

use rmux_core::{
    key_code_lookup_bits, key_code_to_bytes, key_string_lookup_key, key_string_lookup_string,
};
use rmux_proto::{
    ErrorResponse, OptionName, PaneTarget, Response, RmuxError, SendKeysResponse, SessionName,
};

use crate::input_keys::{encode_key_with_backspace, encode_mouse_event, ExtendedKeyFormat};
use crate::io::{IoError, ShellHandle, ShellIo};
use crate::keys::parse_key_code;
use crate::pane_terminals::{session_not_found, HandlerState, PasteDelimiters};

/// Canonical error for a pane input write whose target process is gone.
/// `PaneTerminalStore::pane_shell_if_alive` reports the same text.
const DEAD_PANE_INPUT_ERROR: &str = "target pane has exited";

pub(in crate::handler) struct PaneInputWrite {
    session_name: SessionName,
    window_index: u32,
    pane_index: u32,
    sink: PaneInputSink,
}

impl PaneInputWrite {
    pub(super) fn session_name(&self) -> &SessionName {
        &self.session_name
    }
}

enum PaneInputSink {
    /// The pane's managed job, and the facade its input is admitted through.
    ///
    /// Resolved under the state lock and carried rather than re-resolved at write time: the
    /// handle is generation-bound, so bytes prepared for this pane can never land in whatever
    /// job later takes its name.
    Shell(ShellIo, ShellHandle),
    Disabled,
    #[cfg(test)]
    CapturedForTest,
}

/// Whether resolving a pane input write should treat an exited child process
/// as an error. Paste-buffer rejects dead remain-on-exit panes like tmux;
/// attached input and send-keys must not use child-process liveness as a pane
/// liveness gate (dead-pane write errors are tolerated downstream instead).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::handler) enum PaneInputLiveness {
    TolerateDead,
    RejectDead,
}

pub(in crate::handler) fn prepare_pane_input_write(
    state: &mut HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
    liveness: PaneInputLiveness,
) -> Result<PaneInputWrite, RmuxError> {
    prepare_pane_input_write_with_encoding(state, target, bytes, liveness, None)
}

/// Prepares a write for a pasted body, which must reach the destination byte
/// for byte.
///
/// `delimiters` says whether the payload carries the bracketed-paste envelope
/// the destination announced: a pane consumes control sequences from its input
/// whether or not an envelope surrounds them, so the envelope only decides how
/// a payload that cannot carry one is reported.
pub(in crate::handler) fn prepare_pane_bracketed_paste_write(
    state: &mut HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
    liveness: PaneInputLiveness,
    delimiters: PasteDelimiters,
) -> Result<PaneInputWrite, RmuxError> {
    prepare_pane_input_write_with_encoding(state, target, bytes, liveness, Some(delimiters))
}

fn prepare_pane_input_write_with_encoding(
    state: &mut HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
    liveness: PaneInputLiveness,
    paste: Option<PasteDelimiters>,
) -> Result<PaneInputWrite, RmuxError> {
    let session_name = target.session_name().clone();
    let window_index = target.window_index();
    let pane_index = target.pane_index();
    let pane_id = pane_id_for_input_target(state, target)?;
    if state.pane_input_is_disabled(pane_id) {
        #[cfg(not(test))]
        let _ = bytes;
        return Ok(PaneInputWrite {
            session_name,
            window_index,
            pane_index,
            sink: PaneInputSink::Disabled,
        });
    }
    #[cfg(test)]
    if state.append_pane_input_capture_for_test(target, bytes) {
        return Ok(PaneInputWrite {
            session_name,
            window_index,
            pane_index,
            sink: PaneInputSink::CapturedForTest,
        });
    }
    let (io, handle) = match liveness {
        PaneInputLiveness::RejectDead => {
            state.pane_shell_if_alive(&session_name, window_index, pane_index)?
        }
        PaneInputLiveness::TolerateDead => {
            state.pane_shell(&session_name, window_index, pane_index)?
        }
    };
    let _ = paste;
    #[cfg(not(test))]
    let _ = bytes;
    Ok(PaneInputWrite {
        session_name,
        window_index,
        pane_index,
        sink: PaneInputSink::Shell(io, handle),
    })
}

pub(super) fn prepare_attached_pane_input_writes(
    state: &mut HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
) -> Result<Vec<PaneInputWrite>, RmuxError> {
    prepare_synchronized_pane_input_writes(state, target, bytes)
}

pub(super) fn prepare_synchronized_pane_input_writes(
    state: &mut HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
) -> Result<Vec<PaneInputWrite>, RmuxError> {
    synchronized_input_targets(state, target)?
        .into_iter()
        .map(|target| {
            prepare_pane_input_write(state, &target, bytes, PaneInputLiveness::TolerateDead)
        })
        .collect()
}

pub(super) fn synchronized_input_targets(
    state: &HandlerState,
    target: &PaneTarget,
) -> Result<Vec<PaneTarget>, RmuxError> {
    let session_name = target.session_name();
    let window_index = target.window_index();
    let pane_index = target.pane_index();
    let synchronized =
        state
            .options
            .resolve_for_window(session_name, window_index, OptionName::SynchronizePanes)
            == Some("on");
    let panes = {
        let session = state
            .sessions
            .session(session_name)
            .ok_or_else(|| session_not_found(session_name))?;
        let window = session.window_at(window_index).ok_or_else(|| {
            RmuxError::invalid_target(
                format!("{session_name}:{window_index}"),
                "window index does not exist in session",
            )
        })?;
        let Some(target_pane) = window.pane(pane_index) else {
            return Err(RmuxError::invalid_target(
                target.to_string(),
                "pane index does not exist in window",
            ));
        };
        if synchronized {
            window
                .panes()
                .iter()
                .map(|pane| (pane.index(), pane.id()))
                .collect::<Vec<_>>()
        } else {
            vec![(pane_index, target_pane.id())]
        }
    };

    Ok(panes
        .into_iter()
        .filter(|(_, pane_id)| {
            !state.pane_is_dead(session_name, *pane_id) && !state.pane_input_is_disabled(*pane_id)
        })
        .map(|(pane_index, _)| {
            PaneTarget::with_window(session_name.clone(), window_index, pane_index)
        })
        .collect())
}

pub(super) async fn write_bytes_to_target(
    write: PaneInputWrite,
    bytes: Vec<u8>,
    key_count: usize,
) -> Response {
    match write_bytes_to_target_io(write, bytes).await {
        Ok(()) => Response::SendKeys(SendKeysResponse { key_count }),
        Err(error) => Response::Error(ErrorResponse { error }),
    }
}

pub(super) async fn write_bytes_to_targets(
    writes: Vec<PaneInputWrite>,
    bytes: Vec<u8>,
    key_count: usize,
) -> Response {
    for write in writes {
        if let Err(error) = write_bytes_to_target_io(write, bytes.clone()).await {
            return Response::Error(ErrorResponse { error });
        }
    }
    Response::SendKeys(SendKeysResponse { key_count })
}

pub(in crate::handler) async fn write_bytes_to_target_io(
    write: PaneInputWrite,
    bytes: Vec<u8>,
) -> Result<(), RmuxError> {
    write_bytes_to_target_io_classified(write, bytes)
        .await
        .map_err(PaneInputWriteFailure::into_error)
}

async fn write_bytes_to_target_io_classified(
    write: PaneInputWrite,
    bytes: Vec<u8>,
) -> Result<(), PaneInputWriteFailure> {
    if bytes.is_empty() {
        return Ok(());
    }
    let PaneInputWrite {
        session_name,
        window_index,
        pane_index,
        sink,
    } = write;
    match sink {
        PaneInputSink::Disabled => Ok(()),
        PaneInputSink::Shell(io, handle) => match io.write_input(&handle, &bytes).await {
            Ok(()) => Ok(()),
            Err(error) => Err(PaneInputWriteFailure::from_shell(
                &error,
                &session_name,
                window_index,
                pane_index,
            )),
        },
        #[cfg(test)]
        PaneInputSink::CapturedForTest => Ok(()),
    }
}

pub(in crate::handler) async fn write_attached_bytes_to_target_io(
    write: PaneInputWrite,
    bytes: Vec<u8>,
) -> Result<(), RmuxError> {
    match write_bytes_to_target_io_classified(write, bytes).await {
        Ok(()) => Ok(()),
        Err(failure) => failure.into_attached_result(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneInputFailureKind {
    PaneGone,
    Other,
}

#[derive(Debug)]
struct PaneInputWriteFailure {
    kind: PaneInputFailureKind,
    error: RmuxError,
}

impl PaneInputWriteFailure {
    /// Classifies a managed-input failure the way the pseudoterminal write it replaced was
    /// classified.
    ///
    /// A job that closed, was stopped, had its name taken by a later generation or whose engine
    /// is shutting down is the managed equivalent of the broken pipe a dead pane's master used to
    /// report: the bytes had nowhere to land. Attached input drops those rather than closing the
    /// client's attach, exactly as it did before. A transport failure still carries a real
    /// `io::Error`, so it keeps the descriptor-level classification.
    fn from_shell(
        error: &IoError,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Self {
        if let IoError::Transport(transport) = error {
            return Self::from_transport(transport, session_name, window_index, pane_index);
        }
        if shell_input_target_is_gone(error) {
            return Self::pane_gone();
        }
        Self::other(RmuxError::Server(format!(
            "failed to write to pane {session_name}:{window_index}.{pane_index}: {error}"
        )))
    }

    fn from_transport(
        error: &io::Error,
        session_name: &SessionName,
        window_index: u32,
        pane_index: u32,
    ) -> Self {
        if is_dead_pane_write_error(error) {
            return Self::pane_gone();
        }
        Self::other(RmuxError::Server(format!(
            "failed to write to pane {session_name}:{window_index}.{pane_index}: {error}"
        )))
    }

    fn pane_gone() -> Self {
        Self {
            kind: PaneInputFailureKind::PaneGone,
            error: RmuxError::Server(DEAD_PANE_INPUT_ERROR.to_owned()),
        }
    }

    fn other(error: RmuxError) -> Self {
        Self {
            kind: PaneInputFailureKind::Other,
            error,
        }
    }

    fn into_error(self) -> RmuxError {
        self.error
    }

    /// Attached input is best-effort once it leaves the state lock: a pane may
    /// exit between target resolution and the write. Drop only that input in
    /// that typed case; command paths still receive [`Self::into_error`], and
    /// every other failure still closes the attach.
    fn into_attached_result(self) -> Result<(), RmuxError> {
        match self.kind {
            PaneInputFailureKind::PaneGone => Ok(()),
            PaneInputFailureKind::Other => Err(self.error),
        }
    }
}

/// Whether a managed input write failed because the destination is no longer there.
///
/// Each of these says the same thing the PTY path learned from `EPIPE`: the job this handle names
/// closed, is closing, never existed, no longer accepts input, or the engine behind it is going
/// away. None of them is a reason to tear down the client's attach; the keystroke simply had
/// nowhere to go.
fn shell_input_target_is_gone(error: &IoError) -> bool {
    match error {
        IoError::Closed => true,
        IoError::Mux(mux) => matches!(
            **mux,
            marsh_core::shellmux::MuxError::StaleJob(_)
                | marsh_core::shellmux::MuxError::NoSuchJob(_)
                | marsh_core::shellmux::MuxError::JobClosing(_)
                | marsh_core::shellmux::MuxError::InputClosed(_)
                | marsh_core::shellmux::MuxError::ShuttingDown
        ),
        _ => false,
    }
}

pub(in crate::handler) fn is_dead_pane_write_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    ) || is_unix_pty_eio(error)
}

fn is_unix_pty_eio(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
}

pub(super) fn pane_id_for_input_target(
    state: &HandlerState,
    target: &PaneTarget,
) -> Result<rmux_core::PaneId, RmuxError> {
    super::super::require_expected_pane_identity(state, target)?;
    let session_name = target.session_name();
    let window_index = target.window_index();
    let pane_index = target.pane_index();
    let session = state
        .sessions
        .session(session_name)
        .ok_or_else(|| session_not_found(session_name))?;
    let window = session.window_at(window_index).ok_or_else(|| {
        RmuxError::invalid_target(
            format!("{session_name}:{window_index}"),
            "window index does not exist in session",
        )
    })?;
    window
        .pane(pane_index)
        .map(rmux_core::Pane::id)
        .ok_or_else(|| {
            RmuxError::invalid_target(target.to_string(), "pane index does not exist in window")
        })
}

pub(super) fn encode_tokens_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    tokens: &[String],
) -> Result<Vec<u8>, RmuxError> {
    let mut bytes = Vec::new();
    for token in tokens {
        if let Some(key) = parse_key_code(token) {
            let Some(encoded) = encode_key_for_target(state, target, key)? else {
                return Err(RmuxError::Server(format!(
                    "key {} cannot be sent to a pane",
                    key_string_lookup_key(key_code_lookup_bits(key), false)
                )));
            };
            bytes.extend_from_slice(&encoded);
        } else {
            bytes.extend_from_slice(token.as_bytes());
        }
    }
    Ok(bytes)
}

pub(super) fn encode_key_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    key: rmux_core::KeyCode,
) -> Result<Option<Vec<u8>>, RmuxError> {
    let pane_mode = pane_input_mode(state, target)?;
    let format =
        ExtendedKeyFormat::parse(state.options.resolve(None, OptionName::ExtendedKeysFormat));
    let backspace = state
        .options
        .resolve(None, OptionName::Backspace)
        .and_then(key_string_lookup_string)
        .and_then(key_code_to_bytes)
        .and_then(|bytes| (bytes.len() == 1).then_some(bytes[0]))
        .unwrap_or(0x7f);
    Ok(encode_key_with_backspace(pane_mode, format, key, backspace))
}

pub(super) fn pane_input_mode(state: &HandlerState, target: &PaneTarget) -> Result<u32, RmuxError> {
    let pane_id = state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .and_then(|window| window.pane(target.pane_index()))
        .map(|pane| pane.id())
        .ok_or_else(|| {
            RmuxError::invalid_target(target.to_string(), "pane index does not exist in session")
        })?;
    let pane_mode = state
        .pane_screen_state(target.session_name(), pane_id)
        .map(|screen_state| screen_state.mode)
        .unwrap_or_default();
    Ok(pane_mode)
}

pub(super) fn encode_mouse_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    event: &crate::mouse::AttachedMouseEvent,
) -> Result<Vec<u8>, RmuxError> {
    let session = state
        .sessions
        .session(target.session_name())
        .ok_or_else(|| session_not_found(target.session_name()))?;
    let window = session.window_at(target.window_index()).ok_or_else(|| {
        RmuxError::invalid_target(target.to_string(), "window index does not exist in session")
    })?;
    let pane = window.pane(target.pane_index()).ok_or_else(|| {
        RmuxError::invalid_target(target.to_string(), "pane index does not exist in session")
    })?;
    if event.ignore || event.pane_id != Some(pane.id()) {
        return Ok(Vec::new());
    }

    let pane_mode = state
        .pane_screen_state(target.session_name(), pane.id())
        .map(|screen_state| screen_state.mode)
        .unwrap_or_default();
    let adjusted_y = match event.status_at {
        Some(0) if event.raw.y >= event.status_lines => event.raw.y - event.status_lines,
        _ => event.raw.y,
    };
    let Some(geometry) = crate::mouse::pane_content_geometry_for_target(state, target) else {
        return Ok(Vec::new());
    };
    let Some((x, y)) = relative_mouse_position(event.raw.x, adjusted_y, geometry) else {
        return Ok(Vec::new());
    };
    Ok(encode_mouse_event(pane_mode, &event.raw, x, y).unwrap_or_default())
}

fn relative_mouse_position(
    x: u16,
    y: u16,
    geometry: rmux_core::PaneGeometry,
) -> Option<(u16, u16)> {
    if x < geometry.x()
        || x >= geometry.x().saturating_add(geometry.cols())
        || y < geometry.y()
        || y >= geometry.y().saturating_add(geometry.rows())
    {
        return None;
    }
    Some((x - geometry.x(), y - geometry.y()))
}

pub(super) fn expand_send_key_tokens(
    _state: &HandlerState,
    _target: &PaneTarget,
    tokens: &[String],
    _expand_formats: bool,
) -> Result<Vec<String>, RmuxError> {
    Ok(tokens.to_vec())
}

#[cfg(test)]
mod mouse_geometry_tests {
    use rmux_core::PaneGeometry;
    use rmux_proto::{OptionName, ScopeSelector, SetOptionMode, TerminalSize, WindowTarget};

    use super::*;

    #[test]
    fn left_scrollbar_application_mouse_coordinates_start_at_content_zero() {
        let mut state = HandlerState::default();
        let session_name = SessionName::new("mouse-left-scrollbar").expect("valid session");
        state
            .sessions
            .create_session(session_name.clone(), TerminalSize { cols: 20, rows: 8 })
            .expect("session creation");
        state
            .sessions
            .session_mut(&session_name)
            .expect("created session")
            .resize_active_window_geometry(
                TerminalSize { cols: 20, rows: 8 },
                TerminalSize { cols: 20, rows: 7 },
            );
        let window = WindowTarget::with_window(session_name.clone(), 0);
        for (option, value) in [
            (OptionName::PaneScrollbars, "on"),
            (OptionName::PaneScrollbarsPosition, "left"),
            (OptionName::PaneScrollbarsStyle, "width=2,pad=1"),
        ] {
            state
                .options
                .set(
                    ScopeSelector::Window(window.clone()),
                    option,
                    value.to_owned(),
                    SetOptionMode::Replace,
                )
                .expect("scrollbar option");
        }
        let target = PaneTarget::with_window(session_name, 0, 0);

        let geometry = crate::mouse::pane_content_geometry_for_target(&state, &target)
            .expect("pane content geometry");

        assert_eq!(geometry, PaneGeometry::new(3, 0, 17, 7));
        assert_eq!(relative_mouse_position(3, 0, geometry), Some((0, 0)));
        assert_eq!(relative_mouse_position(19, 0, geometry), Some((16, 0)));
        assert_eq!(relative_mouse_position(2, 0, geometry), None);
    }
}
