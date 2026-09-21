use rmux_proto::PaneTarget;
use tokio::time::{sleep, Duration, Instant};

use super::RequestHandler;

impl RequestHandler {
    pub(crate) async fn wait_for_pane_terminal_for_test(&self, target: &PaneTarget) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let active = {
                let state = self.state.lock().await;
                state
                    .pane_shell_if_alive(
                        target.session_name(),
                        target.window_index(),
                        target.pane_index(),
                    )
                    .is_ok()
            };
            if active {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pane {target} terminal to become active"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    pub(crate) async fn wait_for_pane_startup_to_finish_for_test(&self, target: &PaneTarget) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = {
                let state = self.state.lock().await;
                let terminal_active = state
                    .pane_shell_if_alive(
                        target.session_name(),
                        target.window_index(),
                        target.pane_index(),
                    )
                    .is_ok();
                let still_starting = {
                    #[cfg(windows)]
                    {
                        state.pane_is_starting_in_window(
                            target.session_name(),
                            target.window_index(),
                            target.pane_index(),
                        )
                    }
                    #[cfg(not(windows))]
                    {
                        false
                    }
                };
                terminal_active && !still_starting
            };
            if ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pane {target} startup marker to finish"
            );
            sleep(Duration::from_millis(25)).await;
        }
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
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let pid = {
                let state = self.state.lock().await;
                state
                    .pane_pid_in_window(
                        target.session_name(),
                        target.window_index(),
                        target.pane_index(),
                    )
                    .ok()
            };
            if let Some(pid) = pid {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pane {target} to report a foreground process"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}
