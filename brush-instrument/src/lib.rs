//! Builtin and command lifecycle instrumentation for a stock brush shell.
//!
//! A builtin runs *inside* the shell process. An external tracer sees a builtin's syscalls but
//! never the invocation itself: `git add foo` executed as a builtin looks like a few reads and
//! writes under `.git/`. An embedder that needs to attribute effects to commands must therefore be
//! told by the shell itself.
//!
//! [`instrument`] wraps a stock builtin map so a [`BuiltinHook`] observes every builtin's begin and
//! end; [`RecordingHook`] is the canonical hook, keeping a [`BuiltinRecord`] log in memory that
//! [`dump_records`] writes and [`parse_records`] reads back. [`CommandRecorder`] is the same
//! vocabulary one level up, for a `brush_core::CommandExecutor`: a [`CommandRecord`] per simple
//! command the shell dispatches, builtin or not, stamped from the same clock. Nothing here is a
//! shell-visible feature: a script cannot see the instrumentation, write to it, or turn it off. The
//! shell is stock — only the builtin map it was built with is different.

mod hooks;
#[cfg(target_os = "linux")]
mod record;

pub use hooks::{BuiltinHook, instrument};
#[cfg(target_os = "linux")]
pub use record::{
    BuiltinRecord, CommandKind, CommandRecord, CommandRecorder, RecordingHook, current_tid,
    dump_records, now_micros, parse_records,
};
