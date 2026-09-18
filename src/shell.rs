//! The shell a [`MarshExecutor`] runs commands in.
//!
//! One recipe, so an embedder and this crate's own tests exercise the same shell: a builtin set
//! that differed between them would make the record log evidence about a shell nobody runs.

use std::path::Path;

use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell};

use crate::executor::{MarshExecutor, MarshShellExtensions};

/// Builds a non-interactive shell for `executor`.
///
/// Profile and rc files are skipped: a command's footprint must be the command's, not the host
/// user's shell configuration. The working directory starts at the snapshot root, which is what
/// keeps a command's redirections out of the seed — the interpreter resolves those itself, without
/// ever reaching the spawner.
///
/// An embedder that builds its own shell needs `.external_command_spawner(executor.clone())`,
/// `.builtins(executor.builtins())` and `.working_dir(<executor.snapshot_root()>)`, then
/// [`MarshExecutor::export_snapshot_root`] on the built shell.
///
/// # Errors
///
/// Fails when brush-core cannot build a shell from these options.
pub async fn build_shell(
    executor: &MarshExecutor,
) -> Result<Shell<MarshShellExtensions>, brush_core::Error> {
    let mut shell = Shell::builder_with_extensions::<MarshShellExtensions>()
        .external_command_spawner(executor.clone())
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .maybe_working_dir(executor.snapshot_root().map(Path::to_path_buf))
        .builtins(executor.builtins())
        .build()
        .await?;
    executor.export_snapshot_root(&mut shell)?;
    Ok(shell)
}
