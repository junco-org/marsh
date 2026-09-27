use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rmux_proto::{
    ClientTerminalContext, ControlMode, PaneStateCursorRequest, PaneStateSubscriptionId,
    PaneTarget, Response, SessionName, SubscribePaneOutputRefRequest, SubscribePaneOutputResponse,
    SubscribePaneStateRequest, SubscribePaneStateResponse, SubscribePaneStreamRequest,
    SubscribePaneStreamResponse, TerminalSize,
};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::mode_tree_support::ParsedModeTreeCommand;
use super::scripting_support::QueueExecutionContext;
use super::{QueuedLifecycleEvent, RequestHandler};
use crate::control::{ControlModeUpgrade, ControlServerEvent, CONTROL_SERVER_EVENT_CAPACITY};
use crate::outer_terminal::OuterTerminalContext;
use crate::test_fixtures::{wait_until, SubscribeRequest};
use rmux_core::command_parser::CommandParser;

impl SubscribeRequest for SubscribePaneStateRequest {
    type Success = SubscribePaneStateResponse;

    async fn subscribe(self, handler: &RequestHandler, connection_id: u64) -> Response {
        handler
            .handle_subscribe_pane_state(connection_id, self)
            .await
    }

    fn success(response: Response) -> Result<Self::Success, Response> {
        match response {
            Response::SubscribePaneState(success) => Ok(*success),
            other => Err(other),
        }
    }
}

impl SubscribeRequest for SubscribePaneStreamRequest {
    type Success = SubscribePaneStreamResponse;

    async fn subscribe(self, handler: &RequestHandler, connection_id: u64) -> Response {
        handler
            .handle_subscribe_pane_stream(connection_id, self)
            .await
    }

    fn success(response: Response) -> Result<Self::Success, Response> {
        match response {
            Response::SubscribePaneStream(success) => Ok(*success),
            other => Err(other),
        }
    }
}

impl SubscribeRequest for SubscribePaneOutputRefRequest {
    type Success = SubscribePaneOutputResponse;

    async fn subscribe(self, handler: &RequestHandler, connection_id: u64) -> Response {
        handler
            .handle_subscribe_pane_output_ref(connection_id, self)
            .await
    }

    fn success(response: Response) -> Result<Self::Success, Response> {
        match response {
            Response::SubscribePaneOutput(success) => Ok(success),
            other => Err(other),
        }
    }
}

/// How often the pane waits below re-check the pane.
const PANE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Services an attached client's control transport with the accounting a real
/// client performs, and answers with the drain task's handle.
///
/// A simulated attach that holds its receiver without reading it leaves every
/// refresh retained against the client's bounded control backlog, so the
/// server legitimately closes it as overloaded. Plain controls need the
/// explicit release below; deep/coalesced switches own their reservation and
/// release it on drop, which is why each control is dropped after accounting.
pub(in crate::handler) async fn spawn_accounted_attach_control_drain(
    handler: &RequestHandler,
    requester_pid: u32,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<crate::pane_io::AttachControl>,
) -> tokio::task::JoinHandle<()> {
    let control_backlog = {
        let active_attach = handler.active_attach.lock().await;
        active_attach
            .by_pid
            .get(&requester_pid)
            .expect("attached client exists")
            .control_backlog
            .clone()
    };
    tokio::spawn(async move {
        while let Some(control) = control_rx.recv().await {
            crate::pane_io::release_attach_control_backlog(
                &control_backlog,
                control.received_backlog_units(),
            );
        }
    })
}

impl RequestHandler {
    pub(crate) async fn wait_for_pane_terminal_for_test(&self, target: &PaneTarget) {
        self.wait_for_pane_liveness(target, true, 10, "terminal to become active")
            .await;
    }

    pub(crate) async fn wait_for_pane_startup_to_finish_for_test(&self, target: &PaneTarget) {
        self.wait_for_pane_liveness(target, true, 15, "startup marker to finish")
            .await;
    }

    /// Waits up to five seconds for `target`'s pane process to exit.
    ///
    /// # Panics
    ///
    /// Panics when the pane is still alive at the deadline.
    pub(crate) async fn wait_for_pane_exit_for_test(&self, target: &PaneTarget) {
        self.wait_for_pane_liveness(target, false, 5, "to exit")
            .await;
    }

    /// Waits up to `timeout_secs` until `target`'s pane is `alive` (or, when false, gone),
    /// panicking with `what` the pane failed to do.
    async fn wait_for_pane_liveness(
        &self,
        target: &PaneTarget,
        alive: bool,
        timeout_secs: u64,
        what: &str,
    ) {
        let timeout = Duration::from_secs(timeout_secs);
        wait_until(timeout, PANE_POLL_INTERVAL, async || {
            let state = self.state.lock().await;
            let is_alive = state
                .pane_shell_if_alive(
                    target.session_name(),
                    target.window_index(),
                    target.pane_index(),
                )
                .is_ok();
            if is_alive == alive {
                Ok(())
            } else {
                Err(())
            }
        })
        .await
        .unwrap_or_else(|()| panic!("timed out waiting for pane {target} {what}"));
    }

    /// Waits until `target` reports a foreground process, and answers with it.
    ///
    /// A pane's process appears a moment *after* the request that created or respawned it has
    /// answered. The job is opened, committed and visible, but the shell it execs has not yet
    /// claimed the terminal's foreground process group, so
    /// [`pane_pid_in_window`](crate::pane_terminals::HandlerState::pane_pid_in_window) reports
    /// none. A test that reads the pid immediately after the response is reading that gap rather
    /// than a pane that has no process.
    ///
    /// Deliberately *not* a wait inside the request path: making `new-window` block until its
    /// pane has an OS child would trade a surprising answer for a slow one, and an idle embedded
    /// shell — which is a legitimate pane — would never satisfy it at all.
    ///
    /// # Panics
    ///
    /// Panics when no process appears before the deadline, which is the pane failing to start.
    pub(crate) async fn wait_for_pane_pid_for_test(&self, target: &PaneTarget) -> u32 {
        wait_until(Duration::from_secs(15), PANE_POLL_INTERVAL, async || {
            let state = self.state.lock().await;
            state
                .pane_pid_in_window(
                    target.session_name(),
                    target.window_index(),
                    target.pane_index(),
                )
                .map_err(|_| ())
        })
        .await
        .unwrap_or_else(|()| {
            panic!("timed out waiting for pane {target} to report a foreground process")
        })
    }

    /// Dispatches every lifecycle hook already queued on `events`, in order.
    ///
    /// # Panics
    ///
    /// Panics when the receiver lagged, which means the test lost events.
    pub(crate) async fn drain_lifecycle_hooks_for_test(
        &self,
        events: &mut broadcast::Receiver<QueuedLifecycleEvent>,
    ) {
        loop {
            match events.try_recv() {
                Ok(event) => self.dispatch_lifecycle_hook(event).await,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(skipped)) => {
                    panic!("lifecycle events lagged during test: {skipped}");
                }
            }
        }
    }

    /// Registers plain control client `requester_pid`, attached to `session` when given, and
    /// answers with its control id and event receiver.
    ///
    /// # Panics
    ///
    /// Panics when the client cannot be attached to `session`.
    pub(crate) async fn register_control_for_test(
        &self,
        requester_pid: u32,
        session: Option<&SessionName>,
    ) -> (u64, mpsc::Receiver<ControlServerEvent>) {
        self.register_control_in_context_for_test(
            requester_pid,
            session,
            OuterTerminalContext::default(),
        )
        .await
    }

    /// [`register_control_for_test`](Self::register_control_for_test) for a client whose outer
    /// terminal reports UTF-8.
    pub(crate) async fn register_utf8_control_for_test(
        &self,
        requester_pid: u32,
        session: Option<&SessionName>,
    ) -> (u64, mpsc::Receiver<ControlServerEvent>) {
        let terminal_context =
            OuterTerminalContext::default().with_client_terminal(&ClientTerminalContext {
                terminal_features: Vec::new(),
                utf8: true,
            });
        self.register_control_in_context_for_test(requester_pid, session, terminal_context)
            .await
    }

    /// Registers plain control client `requester_pid` with `terminal_context`, attached to
    /// `session` when given.
    async fn register_control_in_context_for_test(
        &self,
        requester_pid: u32,
        session: Option<&SessionName>,
        terminal_context: OuterTerminalContext,
    ) -> (u64, mpsc::Receiver<ControlServerEvent>) {
        let (event_tx, event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
        let upgrade = ControlModeUpgrade {
            initial_command_count: 0,
            mode: ControlMode::Plain,
            terminal_context,
        };
        let control_id = self
            .register_control_with_closing(
                requester_pid,
                upgrade,
                event_tx,
                Arc::new(AtomicBool::new(false)),
            )
            .await;
        if let Some(session) = session {
            self.set_control_session(requester_pid, Some(session.clone()))
                .await
                .expect("control client attaches to the test session");
        }
        (control_id, event_rx)
    }

    /// Reads up to 16 pane-state events after `after_revision` from `subscription_id` without
    /// waiting, and answers with the raw response.
    pub(crate) async fn read_pane_state_cursor_for_test(
        &self,
        connection_id: u64,
        subscription_id: PaneStateSubscriptionId,
        after_revision: u64,
    ) -> Response {
        self.handle_pane_state_cursor(
            connection_id,
            PaneStateCursorRequest {
                subscription_id,
                after_revision,
                wait: false,
                max_events: Some(16),
            },
        )
        .await
    }

    /// Declares `size` for attached client `attach_pid` under a fresh size sequence, as a
    /// client resize does, and bumps the attach epoch.
    ///
    /// # Panics
    ///
    /// Panics when `attach_pid` is not attached.
    pub(crate) async fn declare_client_size_for_test(&self, attach_pid: u32, size: TerminalSize) {
        let size_sequence = self.next_client_size_sequence();
        {
            let mut active_attach = self.active_attach.lock().await;
            let active = active_attach
                .by_pid
                .get_mut(&attach_pid)
                .expect("attached client remains registered");
            active.set_declared_client_size(size);
            active.size_sequence = size_sequence;
        }
        self.bump_active_attach_epoch();
    }
}

/// Parses the argv `argv` into the mode-tree command it names.
///
/// # Panics
///
/// Panics when `argv` does not parse or names no mode-tree command.
pub(in crate::handler) fn parse_mode_tree(argv: &[&str]) -> ParsedModeTreeCommand {
    let parsed = CommandParser::new()
        .parse_arguments(argv)
        .unwrap_or_else(|error| panic!("{argv:?} parses: {error:?}"));
    RequestHandler::parse_mode_tree_queue_command(parsed.commands()[0].clone())
        .expect("mode-tree command parses")
        .expect("mode-tree command recognized")
}

/// Opens the mode tree `argv` names for `requester_pid`, as a queued command would.
///
/// # Panics
///
/// Panics when the mode tree cannot be opened.
pub(in crate::handler) async fn open_mode_tree(
    handler: &RequestHandler,
    requester_pid: u32,
    argv: &[&str],
) {
    handler
        .execute_queued_mode_tree(
            requester_pid,
            parse_mode_tree(argv),
            &QueueExecutionContext::without_caller_cwd(),
        )
        .await
        .unwrap_or_else(|error| panic!("{argv:?} opens: {error:?}"));
}
