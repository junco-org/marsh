use rmux_core::LifecycleEvent;
use rmux_proto::{
    DaemonStatusResponse, KillServerResponse, Response, ShutdownIfIdleResponse, RMUX_WIRE_VERSION,
};
use std::sync::atomic::Ordering;

use super::target_support::requester_environment_context;
use super::{prepare_lifecycle_event, PendingShutdownReason, RequestHandler};

impl RequestHandler {
    pub(in crate::handler) async fn handle_daemon_status(
        &self,
        requester_pid: u32,
        requester_connection_id: u64,
    ) -> Response {
        let (session_count, client_count) = self
            .daemon_activity_counts(Some(requester_connection_id))
            .await;
        Response::DaemonStatus(DaemonStatusResponse {
            rmux_version: env!("CARGO_PKG_VERSION").to_owned(),
            wire_version: RMUX_WIRE_VERSION,
            session_count,
            client_count,
            config_loading: self.config_loading_for_requester(requester_pid),
        })
    }

    fn config_loading_for_requester(&self, requester_pid: u32) -> bool {
        if !self.config_loading_active() {
            return false;
        }
        let context = requester_environment_context(requester_pid, &self.socket_path());
        context.source_depth.is_none()
    }

    pub(in crate::handler) async fn handle_shutdown_if_idle(
        &self,
        requester_connection_id: u64,
    ) -> Response {
        let (session_count, client_count) = self
            .daemon_activity_counts(Some(requester_connection_id))
            .await;
        // A headless API consumer has no session and no client, and a helper job admitted a
        // microsecond ago is in nobody's window list yet. Reporting idle here would let a
        // `shutdown-if-idle` racing it take the daemon out from under work that is already
        // running, so native activity vetoes the decision without inventing a session or client
        // count in the wire response.
        let shutdown = session_count == 0
            && client_count == 0
            && !self.has_persistent_web_listener()
            && !self.has_native_shell_activity();
        if shutdown {
            self.retained_exited_outputs
                .lock()
                .expect("retained exited output mutex must not be poisoned")
                .clear();
            self.queue_shutdown_request(PendingShutdownReason::SeamlessUpgradeIdle);
        }

        Response::ShutdownIfIdle(ShutdownIfIdleResponse {
            shutdown,
            session_count,
            client_count,
        })
    }

    pub(in crate::handler) async fn handle_kill_server(&self) -> Response {
        let queued_lifecycle_events = {
            let mut state = self.state.lock().await;
            let sessions = state
                .sessions
                .iter()
                .map(|(session_name, session)| (session_name.clone(), session.id()))
                .collect::<Vec<_>>();
            sessions
                .into_iter()
                .map(|(session_name, session_id)| {
                    prepare_lifecycle_event(
                        &mut state,
                        &LifecycleEvent::SessionClosed {
                            session_name,
                            session_id: Some(session_id.as_u32()),
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        self.retained_exited_outputs
            .lock()
            .expect("retained exited output mutex must not be poisoned")
            .clear();
        // Shutdown hooks are accepted below and drained before the daemon exits. Close the
        // wait-for store first so an accepted hook cannot block that drain indefinitely.
        self.shutdown_wait_for();
        for event in queued_lifecycle_events {
            self.emit_prepared(event).await;
        }
        self.queue_shutdown_request(PendingShutdownReason::KillServer);
        Response::KillServer(KillServerResponse)
    }

    async fn daemon_activity_counts(
        &self,
        excluded_detached_connection: Option<u64>,
    ) -> (usize, usize) {
        let session_count = {
            let state = self.state.lock().await;
            state.sessions.len()
        };
        let attach_count = self.active_attach.lock().await.by_pid.len();
        let control_count = self.active_control.lock().await.by_pid.len();
        let detached_request_count = self.active_detached_requests.load(Ordering::SeqCst);
        let detached_connection_count = self
            .active_detached_connections
            .lock()
            .expect("active detached connection mutex must not be poisoned")
            .iter()
            .filter(|connection_id| Some(**connection_id) != excluded_detached_connection)
            .count();
        (
            session_count,
            attach_count + control_count + detached_request_count + detached_connection_count,
        )
    }

    /// Whether the in-process facade is doing something an idle check must not ignore.
    ///
    /// Covers native-client leases, accepted operations and admitted job instances until their
    /// closure or failed open — deliberately wider than `mux.jobs()`, which only lists what is
    /// already visible. Retained bytes from a stream that has ended are not activity: a reader
    /// keeping a transcript alive must not pin the daemon open forever.
    ///
    /// Answers `false` before the listener has installed the facade, which is the truth: nothing
    /// native can have been admitted yet.
    pub(in crate::handler) fn has_native_shell_activity(&self) -> bool {
        self.shell_io()
            .is_some_and(|io| crate::io::ShellIo::has_activity(&io))
    }
}
