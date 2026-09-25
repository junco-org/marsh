//! The adapter between the shell engine's frontend contract and this daemon.
//!
//! [`FrontendQueue`] is the one implementation of
//! [`ShellFrontend`](marsh_core::shellmux::ShellFrontend) in this process. It exists before the
//! mux does — the mux reads its geometry during construction — so it cannot call back into the
//! mux and cannot do any work in a callback beyond copying.
//!
//! So it queues. Every observation is copied into an owned [`FrontendMessage`] and pushed onto a
//! channel the daemon drains on its own task. That is what keeps three separate rules true at
//! once:
//!
//! * **No reentrancy.** A callback that touched the mux would be doing so under the mux's own
//!   announcement path, and the first frontend that did it would deadlock.
//! * **No loss.** The queue is unbounded for control events, which are small and rare, and
//!   *receipted* for output, which is neither. Withholding an output receipt slows exactly one
//!   stream down to the consumer's pace.
//! * **No ownership.** This is an event sink and nothing more. The mux is owned and operated by
//!   [`ShellIo`](crate::io::ShellIo), which is built around it in the same factory that builds
//!   this queue, so the required [`ShellFrontend::bind`] hook deliberately retains nothing. A
//!   reference kept here would keep the core — and through it the seed's lease — alive for as
//!   long as the frontend lived, which is forever.

use std::sync::Arc;

use marsh_core::shellmux::{
    CommandCompletion, CommandHandle, FrontendEvent, JobEnd, OutputChannel, Sandbox, Shell,
    ShellFrontend, ShellMux, TerminalGeometry,
};

/// One observation, owned, on its way to the daemon's consumer.
#[derive(Debug)]
pub(crate) enum FrontendMessage {
    /// The job table or the selection moved on.
    Changed,
    /// A job's streams and shell are open.
    Opened {
        /// The core handle.
        job: Shell,
    },
    /// A command was admitted.
    CommandAccepted {
        /// Its receipt.
        command: CommandHandle,
    },
    /// Bytes one of a job's streams produced, with the receipt the pump is waiting on.
    Output {
        /// The job.
        shell: Sandbox,
        /// Which stream.
        channel: OutputChannel,
        /// The bytes, copied once and shared from here on.
        bytes: Arc<[u8]>,
        /// Completed when the consumer has taken them.
        receipt: tokio::sync::oneshot::Sender<()>,
    },
    /// A command ended and its boundary was decided.
    Finished {
        /// The verdict.
        completion: Arc<CommandCompletion>,
    },
    /// A job's streams are over and its snapshot is reclaimed.
    Closed {
        /// How it ended.
        end: Arc<JobEnd>,
    },
    /// One job's terminal was resized.
    Resized {
        /// The job.
        shell: Sandbox,
        /// Its new size.
        geometry: TerminalGeometry,
    },
    /// The default geometry changed.
    DefaultResized {
        /// The new default.
        geometry: TerminalGeometry,
    },
    /// One of a job's streams failed.
    IoError {
        /// The job.
        shell: Sandbox,
        /// Which stream.
        channel: OutputChannel,
        /// What the read reported.
        error: Arc<std::io::Error>,
    },
}

/// The daemon's shell-engine frontend: a queue, and nothing an owner could be mistaken for.
///
/// Constructed by [`ShellIo::new`](crate::io::ShellIo) immediately before the mux it observes,
/// because the mux reads its geometry during its own construction. That same factory takes
/// [`Self::receiver`] out, so there is no second party to hand this to and nothing to validate
/// after the fact.
#[derive(Debug)]
pub(crate) struct FrontendQueue {
    /// The default geometry terminal jobs open at.
    geometry: TerminalGeometry,
    /// The queue's producer end.
    sender: tokio::sync::mpsc::UnboundedSender<FrontendMessage>,
    /// The queue's consumer end, taken by the factory that built this.
    ///
    /// An `Option` because [`ShellFrontend::new`] has to return a *complete* queue: the receiver
    /// exists before there is anyone to give it to.
    pub(crate) receiver: Option<tokio::sync::mpsc::UnboundedReceiver<FrontendMessage>>,
}

impl FrontendQueue {
    /// Queues one message, ignoring a consumer that has already gone away.
    fn queue(&self, message: FrontendMessage) {
        let _ = self.sender.send(message);
    }
}

impl ShellFrontend for FrontendQueue {
    fn new(rows: u16, cols: u16) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        Self {
            geometry: TerminalGeometry { rows, cols },
            sender,
            receiver: Some(receiver),
        }
    }

    fn size(&self) -> (u16, u16) {
        (self.geometry.rows, self.geometry.cols)
    }

    /// Retains nothing, deliberately.
    ///
    /// The contract offers the mux back to its frontend, and this frontend has no use for it: it
    /// copies observations onto a channel and never reads the mux. The owner is
    /// [`ShellIo`](crate::io::ShellIo), which holds the one strong reference; a second one here
    /// would keep the core, and with it the seed's exclusive lease, alive forever.
    fn bind(&mut self, _mux: std::sync::Weak<ShellMux>) {}

    fn update(&mut self, event: FrontendEvent<'_>) -> Option<tokio::sync::oneshot::Receiver<()>> {
        match event {
            FrontendEvent::Changed => {
                self.queue(FrontendMessage::Changed);
                None
            }
            FrontendEvent::Opened(job) => {
                self.queue(FrontendMessage::Opened { job: job.clone() });
                None
            }
            FrontendEvent::CommandAccepted { command } => {
                self.queue(FrontendMessage::CommandAccepted {
                    command: command.clone(),
                });
                None
            }
            FrontendEvent::Output {
                shell,
                channel,
                bytes,
            } => {
                // The one copy. Everything downstream — the transcript, the retained ring, every
                // observer, every owner — shares this allocation rather than making its own.
                let (receipt, wait) = tokio::sync::oneshot::channel();
                self.queue(FrontendMessage::Output {
                    shell: shell.clone(),
                    channel,
                    bytes: Arc::from(bytes),
                    receipt,
                });
                Some(wait)
            }
            FrontendEvent::Finished { completion } => {
                self.queue(FrontendMessage::Finished {
                    completion: Arc::clone(completion),
                });
                None
            }
            FrontendEvent::Closed { end } => {
                self.queue(FrontendMessage::Closed {
                    end: Arc::clone(end),
                });
                None
            }
            FrontendEvent::Resized { shell, geometry } => {
                self.queue(FrontendMessage::Resized {
                    shell: shell.clone(),
                    geometry,
                });
                None
            }
            FrontendEvent::DefaultResized { geometry } => {
                self.geometry = geometry;
                self.queue(FrontendMessage::DefaultResized { geometry });
                None
            }
            FrontendEvent::IoError {
                shell,
                channel,
                error,
            } => {
                self.queue(FrontendMessage::IoError {
                    shell: shell.clone(),
                    channel,
                    // `std::io::Error` is not cloneable and its source chain is worth keeping, so
                    // it is re-wrapped around the same kind and message rather than stringified.
                    error: Arc::new(std::io::Error::new(error.kind(), error.to_string())),
                });
                None
            }
        }
    }
}
