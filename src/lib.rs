//! marsh — brush, the bash-compatible shell, with `MarshExecutor` plugged in as its
//! `ExternalCommandSpawner`: every external command is recorded, builtins are instrumented,
//! and with `--marsh-seed DIR` the shell runs inside a btrfs snapshot of `DIR` and publishes its
//! effects back through a write-ahead log once junco-policy's capability check grants them.
//!
//! The gated shell, its executor and its policy are re-exported at this crate's root; the
//! vendored, patched `brush-shell`'s own public modules keep their names. The `brush` binary is
//! `crates/brush-shell`'s own.
pub use brush_shell::marsh::{
    Denial, MarshError, MarshExecutor, MarshShellExtensions, Outcome, PolicyValidator,
    PublishMeta, Publication, Shell, ShellRef, builtins, policy,
};
pub use brush_shell::{args, bundled, config, entry, events};
