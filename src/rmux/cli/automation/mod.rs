/// Shared connection, output, and pane-resolution helpers used by every automation command.
mod common;
/// Commands that locate and assert on panes and sessions: `locator`, `find-panes`, and friends.
mod discovery;
/// How a pane's process ended, and the fail-closed mapping onto a command exit code.
mod pane_exit;
/// The `with-session` command: a leased session held for the lifetime of a child process.
mod session;
/// The `pane-snapshot` command and its region/cell rendering.
mod snapshot;
/// Streaming pane output: `stream-pane` and `collect-pane-output`, including lag reporting.
mod stream;
/// Commands that wait on a pane: `wait-pane` and `send-keys` with a wait condition.
mod wait;
/// Resolving a wait to a stable `%id` pane reference and its observed process state.
mod wait_target;

pub(crate) use discovery::{
    run_broadcast_keys, run_expect_pane, run_find_panes, run_find_sessions, run_locator,
};
pub(crate) use session::run_with_session;
pub(crate) use snapshot::run_pane_snapshot;
pub(crate) use stream::{run_collect_pane_output, run_stream_pane};
pub(crate) use wait::{run_send_keys_with_wait, run_wait_pane};

// Managed-pane control (`cli::managed_io`) drives an owned pane through exactly the same
// primitives the automation commands use: one pane-output subscription, one stable `%id`
// reference, and the same fail-closed exit-status reading. Re-exported rather than duplicated
// so both paths keep observing panes the same way.
pub(in crate::cli) use common::{pane_process_state, stable_pane_ref_for_slot, PaneProcessState};
pub(in crate::cli) use pane_exit::PaneExitStatus;
