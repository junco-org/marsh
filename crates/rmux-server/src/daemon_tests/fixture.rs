//! The seed the daemon's own unit tests open an [`RmuxFrontend`] over.
//!
//! [`RmuxFrontend::open_with`] takes the directory its shells start in by default and the backend
//! seeds are reached through. Nothing below is about storage — these tests exercise socket
//! lifetimes — so the seed here is a plain directory behind
//! [`marsh_btrfs::fake::CopyTree`]. The daemon cannot tell the difference, and the tests keep
//! running on hosts with no btrfs.

use std::sync::Arc;

use marsh_core::shellmux::TerminalGeometry;
use tempfile::TempDir;

use crate::{DaemonConfig, RmuxFrontend};

/// Default geometry for a test daemon's terminals; any nonzero pair would do.
const ROWS: u16 = 24;
/// Default width, as above.
const COLS: u16 = 80;

/// Binds a daemon for `config` over a private seed, and returns the scratch tree that seed lives
/// in.
///
/// The caller must hold the [`TempDir`] for as long as the frontend lives: dropping it deletes the
/// tree the engine publishes into, and a daemon whose seed vanished underneath it is not what any
/// of these tests mean to exercise. Binding it before the frontend is enough, since locals drop in
/// reverse declaration order.
///
/// Must be called from within a multi-threaded Tokio runtime, which is what the daemon supports.
///
/// # Panics
///
/// Panics when the scratch tree cannot be built or the daemon cannot be opened. Both are broken
/// fixtures rather than behaviours under test.
pub(super) async fn daemon(config: DaemonConfig) -> (RmuxFrontend, TempDir) {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let root = scratch.path().canonicalize().expect("canonical scratch");
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).expect("seed tree");
    let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
    filesystem.register(&seed);

    let frontend = crate::test_support::open_frontend(
        config,
        &seed,
        brush_core::env::ShellEnvironment::new(),
        TerminalGeometry {
            rows: ROWS,
            cols: COLS,
        },
        filesystem,
    )
    .await
    .expect("open the test daemon");
    (frontend, scratch)
}
