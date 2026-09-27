//! Explicit test-only storage injection. Production construction never exposes a backend.

use crate::shellmux::{MuxError, MuxProfile, ShellFrontend, ShellMux};
use crate::{Shell, ShellBuilder};
use marsh_btrfs::Subvolumes;
use std::sync::{Arc, Mutex};

/// Configures normal Shell construction with a test backend in the same authority registry.
pub fn shell_builder(filesystem: Arc<dyn Subvolumes>) -> ShellBuilder {
    let mut builder = Shell::builder();
    builder.backend = Some(filesystem);
    builder
}

/// Creates a mux whose shells use the configured test builder, not caller-owned stage handles.
pub fn mux<V: ShellFrontend>(
    profile: MuxProfile,
    frontend: Arc<Mutex<V>>,
    filesystem: Arc<dyn Subvolumes>,
) -> Result<Arc<ShellMux>, MuxError> {
    ShellMux::with_builder_factory(
        profile,
        frontend,
        Arc::new(move || shell_builder(Arc::clone(&filesystem))),
    )
}
