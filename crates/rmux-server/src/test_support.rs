//! Explicit test-only construction through configured ordinary Shell builders.

use crate::io::IoResult;
use crate::{DaemonConfig, RmuxFrontend};
use marsh_btrfs::Subvolumes;
use marsh_core::shellmux::TerminalGeometry;
use marsh_core::ShellEnvironment;
use std::path::Path;
use std::sync::Arc;

/// Opens a real daemon using the configured test backend for its Shell constructors.
pub async fn open_frontend(
    config: DaemonConfig,
    initial_dir: &Path,
    environment: ShellEnvironment,
    geometry: TerminalGeometry,
    filesystem: Arc<dyn Subvolumes>,
) -> IoResult<RmuxFrontend> {
    RmuxFrontend::open_configured(
        config,
        initial_dir,
        environment,
        geometry,
        move |profile, frontend| marsh_core::test_support::mux(profile, frontend, filesystem),
    )
    .await
}
