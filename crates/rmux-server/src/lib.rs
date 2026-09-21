#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::await_holding_lock)]

//! Tokio-based detached RPC server for RMUX.

#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod automatic_rename;
#[cfg(any(unix, windows))]
mod buffer_file_io;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod client_flags;
#[cfg(any(unix, windows))]
mod client_format;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod client_names;
#[cfg(any(unix, windows))]
mod clipboard_protocol;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod clock_mode;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod control;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod control_mode;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod control_notifications;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod copy_mode;
mod daemon;
#[cfg(any(unix, windows))]
mod diagnostic_log;
#[cfg(any(unix, windows))]
mod foreground_probe;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod format_runtime;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod handler;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod handler_support;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod hook_compat;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod hook_runtime;
#[cfg(any(unix, windows))]
mod host_name;
#[cfg(any(unix, windows))]
mod input_keys;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod key_table;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod keys;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod legacy_command;
#[cfg(any(unix, windows))]
mod lifecycle_commit_order;
#[cfg(any(unix, windows))]
mod limits;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod listener;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod listener_options;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod listener_signals;
#[cfg(any(unix, windows))]
mod managed_workload;
#[cfg(any(unix, windows))]
mod mouse;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod outer_terminal;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_indices;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_io;
#[cfg(any(unix, windows))]
mod pane_recovery;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_screen_state;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_scrollbar;
#[cfg(any(unix, windows))]
mod pane_state_journal;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_terminal_lookup;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_repl;
mod pane_terminal_process;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_terminals;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_transcript;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod pane_visible_geometry;
#[cfg(any(unix, windows))]
mod perf_instrument;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod prompt_buffer;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod renderer;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod server_access;
mod signals;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod socket_cleanup;
#[cfg(any(unix, windows))]
mod status_jobs;
#[cfg(any(unix, windows))]
mod status_lines;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod status_ranges;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod terminal;
#[cfg(test)]
mod test_env;
#[cfg(test)]
mod test_shell;
#[cfg(any(unix, windows))]
mod tmux_shim;
#[cfg(unix)]
mod unix_socket;
#[cfg(unix)]
mod unix_socket_access;
#[cfg(any(unix, windows))]
#[cfg_attr(windows, allow(dead_code))]
mod wait_for;
#[cfg(all(any(unix, windows), feature = "web"))]
mod web;
// Console-input retry policy is plain `io::Error` logic with no Win32 calls:
// compile it in test builds on every platform so the policy stays covered.
#[cfg(any(windows, test))]
mod windows_console_input;

/// The complete in-process system-I/O interface this daemon exposes.
pub mod io;
mod shell_frontend;

pub use io::{IoError, IoResult, ShellHandle, ShellIo};
/// The snapshot backend an explicit [`RmuxFrontend::open_with`] is given.
///
/// Re-exported rather than reimplemented: it is `marsh-btrfs`'s own trait, and a consumer
/// substituting a fixture for real btrfs names the same one the executor does.
pub use marsh_btrfs::Subvolumes;

/// Fuzzing entry points for protocol parsers.
#[cfg(all(any(unix, windows), feature = "web", feature = "fuzzing"))]
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
#[cfg(unix)]
#[doc(hidden)]
pub fn run_internal_fifo_reader_helper<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    buffer_file_io::run_internal_fifo_reader_helper(arguments)
}
