//! The user interface one mux delivers to, and the observations it delivers.
//!
//! A frontend is a display over however many jobs the mux owns: a CLI writing one job's bytes to
//! the real terminal, a full-screen tab bar, a terminal multiplexer's session tree, a service. It
//! is *given* to [`ShellMux::new`] rather than built by it, because the geometry the mux opens
//! every pseudoterminal at is the frontend's own ([`ShellFrontend::size`]), and because a frontend
//! that could not exist before the mux did would have nowhere to render the mux's construction
//! failure.
//!
//! This crate ships no implementation of the trait. It is the contract the mux delivers to, and
//! whose implementations are the embedding applications'.
//!
//! The traffic is one-way. Everything a frontend *observes* arrives as a [`FrontendEvent`];
//! everything a frontend *does* is an ordinary mux call on the [`Weak<ShellMux>`] it was bound to:
//!
//! | UI action | Mux call |
//! |---|---|
//! | Add a shell | [`ShellMux::open_shell`] |
//! | Find a shell by principal | [`ShellMux::get_shell`] |
//! | Remove a shell | [`Shell::stop`] with `force` |
//! | Graceful close | [`Shell::stop`] without it |
//! | Submit a command line | [`Shell::run_command`] |
//! | Raw input, terminal reply | [`Shell::write_input`] |
//! | End a pipe shell's input | [`Shell::close_input`] |
//! | Resize one shell | [`Shell::resize`] |
//! | Resize everything | [`ShellMux::resize_all`] |
//! | Read one shell's keyboard | [`Shell::idle_terminal`] |
//!
//! Selecting a tab is deliberately absent: which shell a display is looking at is the frontend's
//! own state, not the collection's.
//!
//! There is no second controller and no snapshot type in the event stream:
//! [`FrontendEvent::Changed`] says the display is out of date, and [`ShellMux::snapshot`] answers
//! what it is now, consistently, under one lock.
//!
//! # Backpressure
//!
//! Every callback is synchronous and short, and its borrowed data is valid only until it returns.
//! None of them runs under a registry or live-state lock, so a callback may queue work with
//! Tokio — but it must not synchronously reenter a mutating mux method, and the frontend's own
//! mutex must be released before awaiting one.
//!
//! That leaves one problem a synchronous callback cannot solve: a job producing output faster than
//! the frontend can consume it. [`ShellFrontend::update`] may therefore answer a
//! [`FrontendEvent::Output`] with a receipt — a [`oneshot::Receiver`](tokio::sync::oneshot) the
//! mux awaits, outside every lock, before reading the next chunk of *that* stream. Withholding a
//! receipt slows that one stream down to the frontend's pace and nothing else: other jobs, other
//! channels and every control operation keep running. Dropping a receipt uncompleted is reported
//! as a broken pipe on that stream; it is never turned into a successful write nobody consumed.

use std::sync::{Mutex, MutexGuard, PoisonError, Weak};

use crate::shellmux::command::{CommandCompletion, CommandHandle};
use crate::shellmux::types::{JobEnd, OutputChannel, TerminalGeometry};
use crate::shellmux::{Sandbox, Shell, ShellMux};

/// A receipt a frontend returns to slow one output stream to its own pace.
pub type OutputReceipt = tokio::sync::oneshot::Receiver<()>;

/// A user interface over the jobs one [`ShellMux`] owns.
///
/// Passed to [`ShellMux::new`] as an `Arc<Mutex<V>>`, which is what lets the frontend be driven
/// from the embedding application and from the mux's own byte pumps without either waiting on the
/// other for longer than one callback.
pub trait ShellFrontend: Send + 'static {
    /// A frontend for a terminal of `rows` × `cols`.
    ///
    /// Rows before columns, in that order, everywhere in this module.
    fn new(rows: u16, cols: u16) -> Self
    where
        Self: Sized;

    /// The geometry every job's pseudoterminal is opened at by default, rows first.
    ///
    /// Read once by [`ShellMux::new`], before any job exists: a geometry with a zero dimension is
    /// refused there, exactly as [`ShellMux::resize_all`] refuses one later. A job may still be
    /// opened at a size of its own through
    /// [`SpawnOptions`](crate::shellmux::SpawnOptions); this is the default the others get.
    fn size(&self) -> (u16, u16);

    /// Binds this frontend to the mux that will deliver to it, or detaches it.
    ///
    /// A [`Weak`] rather than a strong reference, because the frontend outlives the mux by
    /// construction: the caller holds the original `Arc<Mutex<V>>`. An empty weak reference means
    /// detached, and a failed [`upgrade`](Weak::upgrade) is an absent session rather than an error.
    ///
    /// On detach a frontend releases the live [`Shell`] objects and per-session buffers it holds;
    /// a recorder may keep its historical observations.
    fn bind(&mut self, mux: Weak<ShellMux>);

    /// Delivers one observation.
    ///
    /// Returning `Some(receipt)` is meaningful for [`FrontendEvent::Output`] only, and means "do
    /// not read more of this stream until I complete this". Every other event ignores it, because
    /// no other event carries bytes that could be lost by being produced too fast.
    fn update(&mut self, event: FrontendEvent<'_>) -> Option<OutputReceipt>;
}

/// One observation a mux delivers to its frontend.
///
/// Everything borrowed is the mux's and is valid only for the length of the call. Shared values
/// (`Arc`) may be retained.
#[derive(Debug)]
pub enum FrontendEvent<'a> {
    /// The shell collection moved on, so the display is out of date.
    ///
    /// Carries no state: [`ShellMux::snapshot`] is what a frontend reads afterwards, and a queued
    /// snapshot would only be a second, staler answer to the same question. Redundant deliveries
    /// are harmless and coalescing them is allowed.
    Changed,
    /// A shell is open, with the object its input and its waits go through.
    ///
    /// Delivered once the shell's streams and interpreter are published and its geometry is
    /// settled. A shell whose construction *failed* never produces this: its failure arrives as
    /// [`Self::Closed`] carrying the error.
    Opened(&'a Shell),
    /// A command was admitted into a shell.
    ///
    /// Delivered before the command can produce output or finish, so a frontend that wants to
    /// correlate a shell's bytes with the command that caused them has the receipt first.
    CommandAccepted {
        /// The receipt for the admitted command.
        command: &'a CommandHandle,
    },
    /// Bytes one of a job's output streams produced.
    ///
    /// A chunk, not a line and not text: it may split a UTF-8 sequence or an escape sequence, and
    /// preserving it byte for byte is what makes a full-screen program work and a captured buffer
    /// correct.
    ///
    /// A terminal job produces [`OutputChannel::Terminal`] only — one merged stream, because that
    /// is what a terminal is; there is no separate stderr to be had, and inferring one would be
    /// invention. A pipe job produces [`OutputChannel::Stdout`] and [`OutputChannel::Stderr`]
    /// independently, with no promised order between them.
    ///
    /// Answer with a receipt to slow this stream down; see the module documentation.
    Output {
        /// The job that produced them, identified by [`Sandbox::uid`] as well as by name, so a
        /// reused name never mixes two jobs' contents.
        shell: &'a Sandbox,
        /// Which stream they came from.
        channel: OutputChannel,
        /// The bytes, exactly as they were read.
        bytes: &'a [u8],
    },
    /// A command ended and its boundary was decided.
    ///
    /// Delivered exactly once per admitted command, launch failures included. It does **not** mean
    /// the job closed: a job outlives the commands run in it. It does not mean the command's
    /// effects are visible either — that is [`Outcome::Published`](crate::Outcome::Published)
    /// inside the completion, and a command can exit zero and be denied.
    Finished {
        /// The verdict, shared so a frontend may retain it past the callback.
        completion: &'a std::sync::Arc<CommandCompletion>,
    },
    /// A job's streams are over and its snapshot is reclaimed: the handle for it is now dead.
    ///
    /// Delivered after every byte of every one of that job's streams, so a frontend appending to a
    /// transcript sees the last chunk before the end. A job whose construction failed reports that
    /// failure here without ever having produced [`Self::Opened`] or a byte of output.
    Closed {
        /// How the job ended, and with what.
        end: &'a std::sync::Arc<JobEnd>,
    },
    /// One job's terminal was resized.
    Resized {
        /// The job whose terminal changed.
        shell: &'a Sandbox,
        /// Its new size.
        geometry: TerminalGeometry,
    },
    /// The default geometry future terminal jobs open at was changed.
    DefaultResized {
        /// The new default.
        geometry: TerminalGeometry,
    },
    /// Reading one of a job's streams failed for a reason that is not end of file.
    ///
    /// That stream is over; the job is not, and this is not an exit status. A frontend renders it
    /// as a diagnostic, never as a command result.
    IoError {
        /// The job whose stream failed.
        shell: &'a Sandbox,
        /// Which stream failed.
        channel: OutputChannel,
        /// What the read reported.
        error: &'a std::io::Error,
    },
}

/// Delivers `event` to `frontend` under one short acquisition of [`lock_frontend`].
pub(crate) fn notify(
    frontend: &Mutex<dyn ShellFrontend>,
    event: FrontendEvent<'_>,
) -> Option<OutputReceipt> {
    lock_frontend(frontend).update(event)
}

/// The frontend, recovering a poisoned lock like the rest of this module.
///
/// A frontend that panicked in one callback left the mux's own state untouched, and refusing to
/// deliver to it afterwards would silently stop a session that is otherwise still running. Held
/// across more than one call only to bind a fresh mux and announce its first state, and to detach
/// at shutdown.
pub(crate) fn lock_frontend(
    frontend: &Mutex<dyn ShellFrontend>,
) -> MutexGuard<'_, dyn ShellFrontend> {
    frontend.lock().unwrap_or_else(PoisonError::into_inner)
}
