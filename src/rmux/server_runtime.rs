//! Tokio runtime policy for long-lived RMUX daemon entrypoints, and the entrypoint itself.

use std::io;
use std::num::NonZeroUsize;
use std::time::Duration;

use marsh::rmux::RmuxFrontend;
use marsh::rmux::types::{ShellEnvironment, TerminalGeometry};
use rmux_server::DaemonConfig;
use tokio::runtime::{Builder, Runtime};

/// Runs a daemon in the foreground until it is asked to stop.
///
/// The CLI's `-D` and its hidden auto-start mode. The same bootstrap a library consumer performs,
/// with one deliberate difference: the owner it builds is consumed immediately by
/// [`RmuxFrontend::wait`], which drops the native client lease before awaiting. A CLI daemon that
/// kept one would never honour `exit-empty` or an idle shutdown, and the user would be left with a
/// server they did not ask to keep.
///
/// The process's own working directory becomes the daemon's default starting directory — the one
/// a request that names none gets — which is what makes `cd <seed> && rmux -D` the whole setup.
/// Nothing is leased here: each shell's seed is discovered from its own starting directory.
///
/// Errors are converted to [`io::Error`] here because this *is* the CLI boundary; inside the
/// library they stay typed.
///
/// # Errors
///
/// Fails when the socket is already held, and with whatever the daemon reported on its way out.
pub(crate) async fn run_daemon(config: DaemonConfig) -> io::Result<()> {
    let cwd = std::env::current_dir()?;
    let frontend = RmuxFrontend::open(
        config,
        &cwd,
        ShellEnvironment::default(),
        // A daemon with no attached client still has to open terminals at *some* size. The
        // classic default is the honest choice: a client that attaches resizes everything to its
        // own geometry immediately.
        TerminalGeometry { rows: 24, cols: 80 },
    )
    .await
    .map_err(|error| io::Error::other(error.to_string()))?;
    frontend
        .wait()
        .await
        .map_err(|error| io::Error::other(error.to_string()))
}

// Command-queue/source-file dispatch has a large debug future. A 2 MiB worker
// stack can abort the daemon before a valid set-option request reaches an
// await; use the same bounded stack budget as the release test workers.
const DAEMON_WORKER_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
// Upstream ran one worker because every pane was an external PTY child: the daemon only had to
// forward bytes. Here a submitted line is interpreted, staged and gated *inside* this process, so
// one worker would let a single CPU-heavy pane monopolize IPC, render and other panes' progress.
// Two is the floor that keeps a control request answerable while a line runs; eight bounds idle
// RSS and cross-thread wakeups on the common local daemon.
const DAEMON_MIN_WORKER_THREADS: usize = 2;
const DAEMON_MAX_WORKER_THREADS: usize = 8;
const DAEMON_MAX_BLOCKING_THREADS: usize = 128;
const DAEMON_BLOCKING_THREAD_KEEP_ALIVE: Duration = Duration::from_secs(2);

/// Builds the runtime used by daemon entrypoints.
///
/// Pane interpreters, IPC handlers, attach forwarding, and web-share tasks all run on
/// this runtime. Keep the scheduler small but never single-threaded: render and status work is
/// already coalesced, while an in-process shell line must not be able to starve the rest.
pub(crate) fn build_daemon_runtime() -> io::Result<Runtime> {
    Builder::new_multi_thread()
        .worker_threads(daemon_worker_threads())
        .thread_stack_size(DAEMON_WORKER_THREAD_STACK_SIZE)
        .max_blocking_threads(DAEMON_MAX_BLOCKING_THREADS)
        .thread_keep_alive(DAEMON_BLOCKING_THREAD_KEEP_ALIVE)
        .enable_io()
        .enable_time()
        .build()
}

/// Available parallelism clamped into the daemon's worker-thread bounds.
fn daemon_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(DAEMON_MIN_WORKER_THREADS, NonZeroUsize::get)
        .clamp(DAEMON_MIN_WORKER_THREADS, DAEMON_MAX_WORKER_THREADS)
}
