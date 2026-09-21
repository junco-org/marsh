//! Keystroke, text and control-event injection into a *running Windows console process*.
//!
//! # What this is
//!
//! This is **not** a PTY backend. Every pane, popup and workload helper in the rmux daemon runs
//! on the shared `marsh-core` `ShellMux`; no `PtyMaster`, `PtyChild`, `ChildCommand`, descriptor
//! reader loop or `ConPTY` fallback survives anywhere in that crate. What lives here is a different
//! mechanic that merely happened to share an upstream crate with the backend: attaching to
//! another process's console and writing input records into it. It is the console analogue of the
//! `rmux_os::process` probes — a platform input primitive the shell engine has no equivalent for,
//! not a second execution path.
//!
//! # Why it is its own crate
//!
//! The Win32 calls below are `unsafe`, and `rmux-server` is `#![forbid(unsafe_code)]` — a lint
//! chosen precisely because it cannot be waived by a local `#[allow]`. Downgrading it to `deny`
//! so one module could opt out would re-open unsafe code to all 380k lines of the daemon. Giving
//! the FFI its own crate gives it an owner, a boundary and an audit surface instead, the same
//! shape as `marsh-btrfs`, `marsh-wal` and `marsh-instrument`.
//!
//! # Provenance
//!
//! `console_input` is vendored from `rmux-pty/src/windows_console_input.rs` at
//! <https://github.com/Helvesec/rmux> commit `1f4571e74f36be0c033c6294d1616c7d3a6fbda1`, because
//! the migration plan permits the Git-sourced `rmux-pty` only as a test client terminal harness
//! and never as a server runtime dependency. Its code is upstream's, changed only by:
//!
//! - the locally defined [`ProcessId`], which replaced `use crate::ProcessId;` and now reports a
//!   rejected pid as an [`std::io::Error`] rather than a `PtyError`;
//! - the removal of the batch-key and mouse-drag writers, which no rmux call site reaches.
//!
//! Keep it that way. A diff against the pinned revision is the only review this FFI gets.
//!
//! # Platforms
//!
//! Empty off Windows. It is a path dependency of `rmux-server` under
//! `[target.'cfg(windows)'.dependencies]`, so a unix build never sees it at all; building it
//! anyway keeps the manifest and this documentation honest on the host the workspace is developed
//! on.

#![cfg_attr(not(windows), allow(unused))]

#[cfg(windows)]
mod console_input;

#[cfg(windows)]
pub use console_input::{
    send_windows_console_interrupt, write_windows_console_key,
    write_windows_console_key_reporting_processed_input, write_windows_console_utf8, ProcessId,
    WindowsConsoleKeyEvent,
};
