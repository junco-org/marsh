#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::await_holding_lock)]

//! Tokio-based detached RPC server for RMUX.

mod automatic_rename;
mod buffer_file_io;
mod client_flags;
mod client_format;
mod client_names;
mod clipboard_protocol;
mod clock_mode;
mod control;
mod control_mode;
mod control_notifications;
mod copy_mode;
mod daemon;
mod diagnostic_log;
mod foreground_probe;
mod format_runtime;
mod handler;
mod handler_support;
mod hook_compat;
mod hook_runtime;
mod host_name;
mod input_keys;
/// The complete in-process system-I/O interface this daemon exposes.
pub mod io;
mod key_table;
mod keys;
mod legacy_command;
mod lifecycle_commit_order;
mod limits;
mod listener;
mod listener_options;
mod listener_signals;
mod managed_workload;
mod mouse;
mod outer_terminal;
mod pane_indices;
mod pane_io;
mod pane_recovery;
mod pane_repl;
mod pane_screen_state;
mod pane_scrollbar;
mod pane_state_journal;
mod pane_terminal_lookup;
mod pane_terminal_process;
mod pane_terminals;
mod pane_transcript;
mod pane_visible_geometry;
mod perf_instrument;
mod prompt_buffer;
mod renderer;
mod server_access;
mod shell_frontend;
mod signals;
mod socket_cleanup;
mod status_jobs;
mod status_lines;
mod status_ranges;
mod terminal;
#[cfg(test)]
mod test_env;
#[cfg(test)]
mod test_names;
#[cfg(test)]
mod test_shell;
mod tmux_shim;
mod unix_socket;
mod unix_socket_access;
mod wait_for;
#[cfg(all(unix, feature = "web"))]
mod web;

pub use io::{IoError, IoResult, ShellHandle, ShellIo};
/// The snapshot backend an explicit [`RmuxFrontend::open_with`] is given.
///
/// Re-exported rather than reimplemented: it is `marsh-btrfs`'s own trait, and a consumer
/// substituting a fixture for real btrfs names the same one the executor does.
pub use marsh_btrfs::Subvolumes;

/// Fuzzing entry points for protocol parsers.
#[cfg(all(unix, feature = "web", feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing {
    /// Feeds arbitrary bytes into the server-side share client-frame parser.
    pub fn websocket_client_frame(data: &[u8]) {
        crate::web::fuzz_client_frame(data);
    }
}

/// The rmux wire and CLI version this daemon speaks.
///
/// This is the vendored server package's own version, deliberately kept equal to the pinned
/// upstream release: `rmux_client::upgrade::daemon_status_matches_current_client` compares the
/// daemon's reported version against the client library's byte for byte, so the application must
/// report this constant rather than the version of whatever crate embeds it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use daemon::{
    default_socket_path, ConfigFileSelection, ConfigLoadOptions, DaemonConfig, RmuxFrontend,
};

/// Runs the private platform FIFO reader helper when its hidden invocation flag is present.
///
/// This is an implementation detail shared by the full `rmux` and `rmux-daemon`
/// entrypoints. Normal invocations return `None`; helper invocations write the
/// FIFO payload to standard output and return the process exit code. Calling
/// this function during normal process startup also advertises the current
/// executable as a helper host for embedded [`RmuxFrontend`] instances.
#[doc(hidden)]
pub fn run_internal_fifo_reader_helper<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    buffer_file_io::run_internal_fifo_reader_helper(arguments)
}
