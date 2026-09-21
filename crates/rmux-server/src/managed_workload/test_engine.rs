//! A real shell engine for the handler tests that have no daemon behind them.
//!
//! [`crate::listener::serve`] installs the facade before a served daemon accepts anything, so
//! production code always finds one. A `RequestHandler` built directly by a unit test does not go
//! through the listener, and the tests that exercise `run-shell`, `if-shell`, the status-line
//! producers, `pipe-pane`, `load-buffer`, `save-buffer` and `source-file` all need a real engine
//! to run against.
//!
//! This builds one, lazily, on first use, through the same [`ShellIo::new`] production goes
//! through — a private seed behind [`marsh_btrfs::fake::CopyTree`], an isolated validator, and the
//! daemon's own uniform profile so `__rmux_io` is registered exactly as it is in production — and
//! it deliberately is not a simplified stand-in. A test running against a different builtin table
//! or a shared policy history would be proving something about a configuration this daemon never
//! ships.

use std::sync::{Arc, Mutex as StdMutex};

use marsh_core::shellmux::TerminalGeometry;

use crate::handler::RequestHandler;
use crate::io::ShellIo;

/// Default geometry for a test engine's terminals; any nonzero pair would do.
const ROWS: u16 = 24;
/// Default width, as above.
const COLS: u16 = 80;

/// The scratch trees the live engines publish into.
///
/// Held for the process's lifetime on purpose: dropping a [`tempfile::TempDir`] deletes the tree
/// its mux publishes into, and a test whose seed vanished underneath it fails for a reason that
/// has nothing to do with what it was testing. The operating system reclaims them when the test
/// binary exits.
static SEEDS: StdMutex<Vec<tempfile::TempDir>> = StdMutex::new(Vec::new());

/// Builds an engine for `handler` and installs it, or answers `None` when one cannot be built.
///
/// `None` is the honest answer outside a Tokio runtime: the mux owns a task per job pump and
/// cannot be constructed without one. A test in that position is not exercising managed work, so
/// the caller reports the ordinary "no engine attached" error rather than panicking here.
pub(crate) fn install(handler: &RequestHandler) -> Option<ShellIo> {
    if let Some(io) = handler.shell_io() {
        return Some(io);
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;

    let scratch = tempfile::tempdir().ok()?;
    let root = scratch.path().canonicalize().ok()?;
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).ok()?;
    let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
    filesystem.register(&seed);

    let (io, events) = ShellIo::new(
        &seed,
        Arc::new(StdMutex::new(marsh_core::PolicyValidator::new())),
        brush_core::env::ShellEnvironment::new(),
        TerminalGeometry {
            rows: ROWS,
            cols: COLS,
        },
        filesystem,
        runtime,
        handler.socket_path(),
    )
    .ok()?;

    SEEDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(scratch);

    // The observation consumer is what turns the engine's queue into this daemon's pane output,
    // command verdicts and job closures. Without it a test would spawn jobs whose bytes nothing
    // ever drains, and the engine's pumps would block on receipts nobody completes.
    tokio::spawn(crate::io::observation::consume(
        io.unleased(),
        RequestHandler::downgrade(handler),
        events,
    ));
    handler.install_shell_io(io.unleased());
    handler.shell_io()
}
