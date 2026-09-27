//! Native syscall observation and shell-run attribution.
//!
//! The package-owned `marsh-trace` companion reuses unmodified lurk's native types, filters and
//! argument table. Marsh supplies stopped-task observation, filesystem identity and attribution.

mod capture;
mod helper;
mod observation;
mod syscall;
mod tracing;
mod wire;

#[doc(hidden)]
pub use helper::run_tracer_helper;
pub use syscall::{FileTarget, Syscall};
pub use tracing::{
    InvocationId, PollScope, RootId, Scoped, TraceRun, TraceScope, TraceScopeGuard, Tracing,
};
