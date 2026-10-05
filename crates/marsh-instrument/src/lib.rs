//! Native syscall observation and shell-run attribution, entirely in-process.
//!
//! Spawned commands are traced by tracer threads of the host process, reusing unmodified lurk's
//! native types, filters and argument table; the interpreter's own filesystem accesses are
//! reported as host records. Marsh supplies stopped-task observation, filesystem identity and
//! attribution.

mod capture;
mod host;
mod observation;
mod ring;
mod syscall;
mod tracing;
mod wire;

pub use host::HostCall;
pub use syscall::{FileTarget, Syscall};
pub use tracing::{
    ChildEvent, ExecCommand, ExecDecision, ExecHooks, InvocationId, PollScope, RootId, Scoped,
    TraceRun, TraceScope, TraceScopeGuard, TracedChild, Tracing, open_process, signal_process,
};
