//! Builtin and command lifecycle instrumentation for a stock brush shell.
//!
//! A builtin runs *inside* the shell process. An external tracer sees a builtin's syscalls but
//! never the invocation itself: `git add foo` executed as a builtin looks like a few reads and
//! writes under `.git/`. An embedder that needs to attribute effects to commands must therefore be
//! told by the shell itself.
//!
//! [`instrument`] wraps a stock builtin map so a [`BuiltinHook`] observes every builtin's begin and
//! end; [`RecordingHook`] is the canonical hook, keeping a [`BuiltinRecord`] log in memory that
//! [`dump_records`] writes and [`parse_records`] reads back. [`SpawnRecorder`] is the same
//! vocabulary one level up, for a `brush_core::extensions::ExternalCommandSpawner`: a
//! [`SpawnRecord`] per external command the shell hands to its spawner, stamped from the same
//! clock.
//!
//! The third stream is the syscalls themselves. [`RecordingHook::shared`] also owns one system
//! `strace` attached to the host, and hands every [`TraceLine`] it decodes to the registered root
//! the call belongs to — which is what lets an embedder know the *files* a line read and wrote
//! rather than the commands it named. [`Scoped`] and [`TraceScope`] are how work is attributed to
//! a root; [`RecordingHook::drain`] is how a caller proves every syscall issued so far has been
//! accounted for.
//!
//! Nothing here is a shell-visible feature: a script cannot see the instrumentation, write to it,
//! or turn it off. The shell is stock — only the builtin map it was built with is different.

mod hooks;
mod record;
mod strace;

pub use hooks::{BuiltinHook, instrument};
pub use record::{
    BuiltinRecord, RecordingHook, SpawnRecord, SpawnRecorder, SpawnRequest, current_tid,
    dump_records, now_micros, parse_records,
};
pub use strace::{
    Call, Scoped, TraceLine, TraceObserver, TraceScope, TraceScopeGuard, parse_quoted, split_args,
};
