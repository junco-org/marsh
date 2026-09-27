//! Shared fixtures for the issue #182 outer-terminal title/path regressions.
//!
//! Everything here observes the same seams the daemon uses: the control queue a
//! client really drains, the per-client memory the server really keeps, and the
//! production overlay-barrier rules that decide which queued frame is drawn.

use super::*;

use crate::pane_io::replay_client_visible_payloads;

pub(super) const TITLE_OPEN: &str = "\u{1b}]0;";
pub(super) const TITLE_CLOSE: char = '\u{7}';
pub(super) const PATH_OPEN: &str = "\u{1b}]7;";
pub(super) const PATH_CLOSE: char = '\u{7}';

/// A terminal family that advertises the `title` capability (TSL/FSL).
pub(super) fn title_capable_context() -> OuterTerminalContext {
    OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color")])
}

pub(super) async fn set_global(handler: &RequestHandler, option: OptionName, value: &str) {
    handler
        .set_option(ScopeSelector::Global, option, value)
        .await;
}

/// A terminal family advertising both the title and OSC 7 templates.
pub(super) async fn enable_osc7(handler: &RequestHandler) {
    let features = (
        ScopeSelector::Global,
        OptionName::TerminalFeatures,
        "xterm*:osc7",
    );
    handler
        .handle_ok(SetOptionRequest {
            mode: SetOptionMode::Append,
            ..Fixture::fixture(features)
        })
        .await;
}

/// Attaches `attach_pid` to `session` from a terminal that advertises the title capability.
pub(super) async fn attach_title_capable_client(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
    attach_pid: u32,
) -> mpsc::UnboundedReceiver<AttachControl> {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let context = title_capable_context();
    let _attach_id = handler
        .register_attach_with_terminal_context(attach_pid, session.clone(), control_tx, context)
        .await;
    control_rx
}

/// Registers one client with its own identity and geometry, exactly as
/// `listener.rs` publishes a fresh attach.
pub(super) async fn attach_sized_client(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
    attach_pid: u32,
    client_size: TerminalSize,
) -> mpsc::UnboundedReceiver<AttachControl> {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let registration = AttachRegistration {
        terminal_context: title_capable_context(),
        client_size: Some(client_size),
        ..Fixture::fixture((control_tx, current_owner_uid()))
    };
    handler
        .register_attach_with_access(attach_pid, session.clone(), None, registration)
        .await
        .expect("attach registration succeeds");
    control_rx
}

/// What the render told this client's outer terminal to show.
pub(super) fn client_title_of(target: &crate::pane_io::AttachTarget) -> Option<&str> {
    target
        .client_title
        .as_ref()
        .and_then(|rendered| rendered.state().title())
}

/// The OSC 0 payloads carried by one render frame, in order.
pub(super) fn titles_in(frame: &[u8]) -> Vec<String> {
    payloads_in(frame, TITLE_OPEN, TITLE_CLOSE)
}

/// The OSC 7 payloads carried by one render frame, in order.
pub(super) fn paths_in(frame: &[u8]) -> Vec<String> {
    payloads_in(frame, PATH_OPEN, PATH_CLOSE)
}

fn payloads_in(frame: &[u8], open: &str, close: char) -> Vec<String> {
    let text = String::from_utf8_lossy(frame);
    text.split(open)
        .skip(1)
        .filter_map(|rest| rest.split_once(close))
        .map(|(payload, _)| payload.to_owned())
        .collect()
}

/// Every OSC 0 payload this client's outer terminal really receives, replayed
/// through the production overlay-barrier rules.
pub(super) fn delivered_titles(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) -> Vec<String> {
    replay_client_visible_payloads(control_rx)
        .iter()
        .flat_map(|payload| titles_in(payload))
        .collect()
}

/// Every OSC 7 payload this client's outer terminal really receives.
pub(super) fn delivered_paths(
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) -> Vec<String> {
    replay_client_visible_payloads(control_rx)
        .iter()
        .flat_map(|payload| paths_in(payload))
        .collect()
}

/// What the server believes this client's outer terminal currently shows.
pub(super) async fn remembered_title(handler: &RequestHandler, attach_pid: u32) -> Option<String> {
    let active_attach = handler.active_attach.lock().await;
    active_attach
        .by_pid
        .get(&attach_pid)
        .expect("attach is active")
        .client_title
        .title()
        .map(str::to_owned)
}

/// The pane whose transcript the attached client renders.
pub(super) async fn active_pane_id(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
) -> rmux_core::PaneId {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(session)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0).map(rmux_core::Pane::id))
        .expect("window pane exists")
}
