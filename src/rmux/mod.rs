//! The terminal multiplexer marsh is, and the system-I/O interface it exposes.
//!
//! `rmux` is a tmux-style multiplexer whose panes, popups and workload helpers are not host
//! processes: every one of them is a job on a single [`ShellMux`](marsh_core::shellmux::ShellMux)
//! over a single leased seed. A pane runs an embedded brush interpreter, each submitted line is
//! staged in that job's btrfs snapshot, and its effects reach the seed only once the policy has
//! granted every capability they amount to. A denied line leaves the seed untouched while the
//! pane stays open, which is why a process exit status and an approval are separate results here.
//!
//! The ownership chain is a straight line: one leased seed → one managed mux → one server and
//! its I/O facade → many shell, execution and observer handles. [`RmuxFrontend`] owns the first
//! three, and opening one is the *only* way to build them: a caller supplies a seed path, a
//! configuration and a policy validator, never an executor, a mux, a profile or a callback queue.
//! Its operations are [`ShellIo`]'s, reached through `Deref`, and [`RmuxFrontend::io`] hands out
//! an independently shareable clone of that same handle for concurrent code. There is no accessor
//! for the mux, the executor or a job's raw descriptors: a caller that could spawn or publish
//! outside the facade would be a hole in the gate the rest of this crate exists to hold.
//!
//! # Every capability, and the route to it
//!
//! | Multiplexer capability | Public application route |
//! |---|---|
//! | Construction, seed/lease ownership | [`RmuxFrontend::open`], [`RmuxFrontend::open_with`] plus [`ShellIo::seeds`]; no raw spawner escape |
//! | Default directory, per-seed policy history | [`ShellIo::default_dir`], [`ShellIo::history`] |
//! | Jobs, one job, current selection | [`ShellIo::snapshot`], [`ShellIo::jobs`], [`ShellIo::job`], [`ShellIo::current_job`], [`ShellIo::shell`] |
//! | Create a shell and keep it | [`ShellIo::open_shell`], [`ShellIo::keep`] |
//! | Run a line, and the finish callback | [`ShellHandle::run_command`], [`ShellIo::on_finish`] |
//! | Selection, graceful and forced stop | [`ShellIo::switch`], [`ShellIo::stop`], [`ShellHandle::wait_closed`] |
//! | Raw input and resize | [`ShellIo::write_input`], [`ShellIo::resize`], [`ShellIo::resize_all`] |
//! | Frontend output, lifecycle and errors | [`ShellIo::observe`], [`ShellIo::output`], typed completion and closure watches |
//! | Shutdown | [`ShellIo::shutdown`], [`RmuxFrontend::wait`], [`RmuxFrontend::shutdown`] |
//! | Additional managed process I/O | [`ShellIo::execute`], separate pipe streams, input end-of-file, [`ShellIo::signal`], bounded collection |
//! | Rich rmux operations | SDK [`Session`](types::Session), [`Window`](types::Window) and [`Pane`](types::Pane) handles, and [`ShellIo::open_protocol`] |
//!
//! Nothing is reachable any other way, which is the point of the table rather than a caveat on
//! it: a capability missing from the right-hand column is one this facade does not grant.
//!
//! # Terminal jobs and pipe jobs
//!
//! A pane is a *terminal* job: one pseudoterminal, one merged output stream carrying the
//! program's standard output, its standard error and the terminal's own replies together, a size
//! programs can query, raw mode, and no end-of-file a writer can send. [`ShellIo::execute`]
//! opens a *pipe* job instead: three real pipes, with byte-exact independent standard output and
//! standard error, no promised order between them, and a real end-of-file through
//! `InputWriter::close`. The choice is made once, when the job opens, because it decides what
//! the bytes *mean* — a helper whose output is data must not have it merged with diagnostics or
//! rewritten by a line discipline.
//!
//! # Approved publication, and everything short of it
//!
//! Bytes observed live are **provisional**. They are real output, but whether the filesystem
//! changes behind them survive is the gate's answer, and the only spelling of that answer is
//! [`Outcome::Published`](types::Outcome::Published). A line can exit zero and be denied; a line
//! can be discarded having already run and spawned processes; a pipe reaching end of file, a
//! command finishing, a job closing and the host shutting down are four different boundaries and
//! not one of them is approval.
//!
//! Ownership of a path is **durable**, and that has a consequence worth planning for. Each
//! transaction's granted capabilities are recorded in the write-ahead log together with the
//! snapshot id that earned them, and reopening the seed reinstalls that history before any shell
//! runs a line, so [`ShellIo::history`] describes the seed rather than this process and a
//! restart hands nobody a clean slate. Owners are identified by snapshot id and never by job
//! name, because a pane index is reused and a restarted daemon renumbers from one — naming them
//! by name would let the next holder inherit the last holder's stake.
//!
//! The consequence: a path left **unstaged** when the host shuts down stays owned by a principal
//! that no longer exists. The policy admits only that owner for staging, checkout or stash, so
//! after the restart no one can edit it — the denial names a snapshot id with no job behind it.
//! Staging a path before shutdown releases it. An application that opens jobs which leave work
//! unstaged should therefore treat "stage or discard before shutdown" as part of its teardown,
//! not as housekeeping it can skip.
//!
//! "Approved" also means marsh's publication gate rather than OS confinement. The daemon's own
//! socket, pseudoterminal allocation, IPC framing and write-ahead-log handling are trusted
//! infrastructure and are not themselves run as gated workloads; a workload can still reach the
//! network, and can still write outside the seed if it names an absolute path there. What this
//! facade does guarantee is that it offers no way *around* the gate: no raw multiplexer, no
//! executor, no mutable validator, no raw job descriptor, no CLI subprocess runner and no
//! file-save helper. [`ShellIo::open_protocol`] is blocking transport to this same daemon once
//! connected, not a way to run a program outside it. Extra builtins belong in the one frozen
//! profile, registered before attachment, and reach native work through the managed command
//! context.
//!
//! The CLI's hidden internal daemon mode and its `rmux -D` foreground mode build the same owner
//! and consume it with [`RmuxFrontend::wait`] instead of keeping a client lease, so an idle daemon
//! still exits on `exit-empty` as upstream's does.

pub use rmux_server::io::*;
pub use rmux_server::{IoError, IoResult, RmuxFrontend, ShellHandle, ShellIo};

/// Everything the facade's own signatures mention, re-exported so a consumer needs no other
/// dependency.
///
/// A caller that had to add `rmux-proto`, `rmux-sdk` or `brush-core` to its manifest merely to
/// name an argument would be pinning three more versions to keep in step with this one — and
/// getting one wrong produces a type error whose cause is a lockfile, not the code. These are the
/// exact types the methods on [`ShellIo`] take and return, and nothing else.
pub mod types {
    /// What a workload *is*: shell text for the embedded interpreter, or an argv vector.
    ///
    /// Non-exhaustive upstream, so match with a wildcard arm.
    pub use rmux_proto::ProcessCommand;
    /// A session's name, for [`ShellIo::session`](super::ShellIo::session).
    pub use rmux_proto::SessionName;

    /// Variables seeded into a job's shell, for
    /// [`ExecutionSpec::environment`](super::ExecutionSpec) and
    /// [`SpawnOptions::environment`](marsh_core::shellmux::SpawnOptions).
    pub use brush_core::env::ShellEnvironment;

    /// Where an observer starts reading a stream, for [`ShellIo::output`](super::ShellIo::output).
    pub use rmux_sdk::PaneOutputStart;
    /// The SDK handles [`ShellIo`](super::ShellIo)'s session, window and pane methods answer with.
    pub use rmux_sdk::{EnsureSession, Pane, PaneRef, Session, Window, WindowRef};

    /// One retained chunk, or the explicit gap that says an observer fell behind.
    pub use rmux_core::events::{OutputCursorItem, OutputEvent};

    /// The snapshot backend an explicit
    /// [`RmuxFrontend::open_with`](super::RmuxFrontend::open_with) is handed.
    pub use rmux_server::Subvolumes;
    /// The daemon's listening configuration and its startup config-file policy, for
    /// [`RmuxFrontend::open`](super::RmuxFrontend::open).
    pub use rmux_server::{ConfigFileSelection, ConfigLoadOptions, DaemonConfig};

    /// The whole rmux wire vocabulary, for
    /// [`ShellIo::open_protocol`](super::ShellIo::open_protocol).
    ///
    /// Requests, responses and their stateful attach/control upgrades are upstream's schema, so
    /// this is the schema itself rather than a wrapper around it.
    pub use rmux_proto as protocol;
    /// A layout's name, for the SDK window handles.
    pub use rmux_proto::LayoutName;

    pub use rmux_client::ClientError;
    /// A blocking protocol connection to this daemon's socket, and what it can fail with.
    pub use rmux_client::connection::Connection;

    /// How much an observer may fall behind before it is told it has.
    pub use rmux_core::events::SubscriptionLimits;

    /// The argument vocabulary the SDK session, window and pane operations take.
    pub use rmux_sdk::{
        EnsureSessionPolicy, PaneRespawnOptions, ProcessSpec, SplitDirection, TerminalSizeSpec,
    };

    /// What a policy decision is *about*, for a caller reading
    /// [`ShellIo::history`](super::ShellIo::history).
    pub use marsh_core::policy::{Action, Event, Principal, Resource};
    /// What the gate produced, and the shell-layer failure underneath a
    /// [`MuxError::Marsh`].
    pub use marsh_core::{
        Denial, GrantedAction, GrantedCapability, MarshError, Publication,
    };

    /// The core vocabulary a shell, a command and its verdict are spelled in.
    ///
    /// [`RunError`] and [`PolicyError`] are here so a native caller can name what a line failed
    /// with — a refused admission, a policy denial carrying the completion it refused, an
    /// unpublished conclusion, a lost answer — without taking a dependency on the core crate.
    pub use marsh_core::shellmux::{
        CommandCompletion, CommandHandle, CommandId, CommandOptions, IdleTerminal, JobDir, JobEnd,
        JobIo, JobView, MuxError, MuxSnapshot, OnFinish, OutputChannel, PolicyError, RunError,
        RunningView, Sandbox, SeedInfo, ShellId, SnapshotUid, SpawnOptions, TerminalGeometry,
        WaitError,
    };
    /// What the gate made of a line, and the signals a running command can be sent.
    pub use marsh_core::{Outcome, Signal};
}
