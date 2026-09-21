//! Owned observations, and the bus that carries them.
//!
//! The core delivers its observations as borrowed [`FrontendEvent`](marsh_core::shellmux::FrontendEvent)s valid only for the length of
//! one callback. That is the right contract for a frontend, and the wrong one for an application:
//! an observer wants to keep an event, forward it to another task, or look at it after the fact.
//! So everything a frontend sees is copied once, into the owned [`IoEvent`] variants here, and
//! published on a bus with explicit sequencing and explicit lag.
//!
//! # What this is not
//!
//! It is not an IPC message stream. Nothing here is serialized, and no Rust outcome or error is
//! invented as a wire notification. Remote observers use rmux's own existing protocol through the
//! SDK; this is the in-process view.
//!
//! # Backpressure and lag
//!
//! Observers are bounded and independent. A slow one falls behind and is told so — explicitly,
//! with the sequence it expected and the sequence it can resume from — rather than being allowed
//! to stall the job producing the bytes. Recovering a *terminal screen* from a gap is not this
//! layer's job: a job snapshot cannot reconstruct lost terminal bytes, and rmux's own recovery and
//! surface streams exist for exactly that.

use std::sync::Arc;

use marsh_core::shellmux::{
    CommandCompletion, CommandHandle, JobEnd, MuxSnapshot, OutputChannel,
    TerminalGeometry,
};

use crate::io::{IoError, IoResult, ShellHandle};

/// How many envelopes one observer may fall behind before it is told about a gap.
///
/// Deliberately the same order as rmux's own retained output ring: an observer that cannot keep up
/// with this many events is not going to be rescued by a larger buffer, and an unbounded queue
/// would turn a slow observer into unbounded daemon memory.
pub(crate) const EVENT_BUS_CAPACITY: usize = rmux_core::events::DEFAULT_OUTPUT_RING_CAPACITY;

/// Where the service is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoPhase {
    /// Accepting work.
    Running,
    /// Teardown has begun: no new work is admitted, outstanding work is finishing or being
    /// cancelled.
    Closing,
    /// Teardown is complete. Read-only state is still readable; nothing else is.
    Closed,
}

/// One observation, with its position in the stream.
#[derive(Debug)]
pub struct IoEnvelope {
    /// Position in this service's single observation sequence, starting at zero.
    ///
    /// Sequences are for *detecting gaps*, not for correlating outcomes. Use
    /// [`CommandId`](marsh_core::shellmux::CommandId) to correlate a command with its verdict; a
    /// position in an event stream says nothing about which command produced it.
    pub sequence: u64,
    /// What happened.
    pub event: IoEvent,
}

/// One owned observation.
///
/// # Why every variant that names a job carries a handle
///
/// A [`ShellHandle`], never a bare identity. That is what keeps a stream event *actionable* when
/// it is delivered late: the receiver can write to that job, resize it or stop it, without
/// re-resolving a name the table may already have reused.
///
/// The consumer keeps one handle per snapshot id from [`Opened`](Self::Opened) through
/// [`Closed`](Self::Closed) — exactly the window in which these events can occur — so no handle
/// is ever minted for a job whose row is gone, and none claims a liveness nobody has.
///
/// A handle is generation-bound, so acting through one whose job has since closed fails rather
/// than reaching whatever took its name. That failure is the truthful answer, and it is why these
/// events can be acted on directly instead of being treated as merely advisory.
#[derive(Debug)]
pub enum IoEvent {
    /// The job table or the selection moved on.
    ///
    /// Carries no state, may be coalesced, and may be redundant.
    /// [`ShellIo::snapshot`](crate::io::ShellIo::snapshot) is the authoritative answer.
    Changed,
    /// A job's streams and shell are open.
    Opened {
        /// A handle on the job, bound to this generation of its name.
        job: ShellHandle,
    },
    /// A command was admitted into a job, before it could produce output or finish.
    CommandAccepted {
        /// The receipt. Waiting on it is safe from any number of holders.
        command: CommandHandle,
    },
    /// Bytes one of a job's streams produced.
    ///
    /// A terminal job produces [`OutputChannel::Terminal`] only, merged. A pipe job produces
    /// [`OutputChannel::Stdout`] and [`OutputChannel::Stderr`] independently, with no promised
    /// order between them.
    Output {
        /// The job that produced them, as a generation-bound handle.
        ///
        /// A handle rather than a bare identity, so a late stream event stays *actionable*: the
        /// receiver can write to it, resize it or stop it without re-resolving a name that may
        /// since have been reused. The consumer keeps one handle per snapshot id from `Opened`
        /// through `Closed`, which is exactly the window in which these events can occur.
        job: ShellHandle,
        /// Which stream.
        channel: OutputChannel,
        /// The bytes, shared rather than copied per observer.
        output: rmux_core::events::OutputEvent,
    },
    /// A command ended and its boundary was decided.
    ///
    /// Not a statement that the job closed, and not a statement that anything was published.
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
        /// The job whose terminal changed, as a generation-bound handle.
        job: ShellHandle,
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
    /// That stream is over; the job is not, and this is not an exit status.
    IoError {
        /// The job whose stream failed, as a generation-bound handle.
        job: ShellHandle,
        /// Which stream.
        channel: OutputChannel,
        /// What the read reported.
        error: Arc<std::io::Error>,
    },
    /// The service moved to a new phase.
    PhaseChanged {
        /// The phase it moved to.
        phase: IoPhase,
    },
}

/// A consistent look at the service and the core behind it.
#[derive(Clone, Debug)]
pub struct IoSnapshot {
    /// Where the service is in its life.
    pub phase: IoPhase,
    /// The core's own snapshot: jobs, unfinished commands, selection and default geometry, all
    /// taken under one lock.
    pub state: MuxSnapshot,
    /// The sequence the first event *after* this snapshot will carry.
    ///
    /// Everything before it is already reflected in `state`; everything from it onwards arrives on
    /// the stream. That is what makes "in the snapshot or in the events, never neither" true.
    pub next_event_sequence: u64,
}

/// A snapshot and the stream that continues from it.
///
/// Registered atomically: the cursor is installed and the snapshot taken under one acquisition of
/// the bus lock, so a mutation concurrent with this call is either already in `snapshot` or
/// arrives later on `events`. It is never lost between them, and a redundant
/// [`IoEvent::Changed`] is harmless.
#[derive(Debug)]
pub struct Observation {
    /// The state at the moment of subscription.
    pub snapshot: IoSnapshot,
    /// Everything that happened afterwards.
    pub events: IoEventStream,
}

/// One observer's position in the observation stream.
#[derive(Debug)]
pub struct IoEventStream {
    /// The underlying bounded subscription.
    receiver: tokio::sync::broadcast::Receiver<Arc<IoEnvelope>>,
    /// The sequence this observer expects next, for reporting a gap precisely.
    expected: u64,
    /// An envelope read while resolving a gap, returned before anything new.
    held: Option<Arc<IoEnvelope>>,
}

impl IoEventStream {
    /// The next observation, or `None` once the service has closed its bus.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::Lagged`] when this observer fell behind: `expected` is the sequence
    /// it wanted, `resume` the oldest one still available. The events between them are gone, and
    /// no amount of retrying will bring them back — a caller that needs consistent state
    /// resubscribes through [`ShellIo::observe`](crate::io::ShellIo::observe) for a fresh
    /// snapshot, and a caller that needs a *terminal screen* uses rmux's own recovery or surface
    /// stream, because a job snapshot cannot reconstruct lost terminal bytes.
    ///
    /// # Examples
    ///
    /// The recovery loop, and the classification it performs. `follow` is compiled against the
    /// live API; everything below it runs.
    ///
    /// ```
    /// use rmux_server::io::{IoError, IoResult, ShellIo};
    ///
    /// /// Reads observations until the bus closes, resubscribing across every gap.
    /// async fn follow(io: &ShellIo) -> IoResult<usize> {
    ///     let mut observation = io.observe();
    ///     let mut resubscriptions = 0;
    ///     loop {
    ///         match observation.events.recv().await {
    ///             Ok(Some(_envelope)) => {}
    ///             // The bus closed with the service. There is nothing left to resubscribe to,
    ///             // and nothing was lost on the way out.
    ///             Ok(None) => return Ok(resubscriptions),
    ///             // A gap. The events between `expected` and `resume` are gone for good —
    ///             // retrying resumes *after* them — so the only route back to a consistent
    ///             // view is a fresh subscription, whose snapshot restates what they described.
    ///             Err(IoError::Lagged { .. }) => {
    ///                 observation = io.observe();
    ///                 resubscriptions += 1;
    ///             }
    ///             Err(error) => return Err(error),
    ///         }
    ///     }
    /// }
    /// // Compiled, not called: it needs a live host. What it decides is checked below, which
    /// // does not.
    /// let _ = follow;
    ///
    /// /// The same four-way decision, over a result alone.
    /// fn action(result: IoResult<Option<u64>>) -> &'static str {
    ///     match result {
    ///         Ok(Some(_sequence)) => "apply",
    ///         Ok(None) => "stop",
    ///         Err(IoError::Lagged { .. }) => "resubscribe",
    ///         Err(_) => "propagate",
    ///     }
    /// }
    ///
    /// assert_eq!(action(Ok(Some(41))), "apply");
    /// // End of stream is not a gap: nothing was lost, the service simply closed.
    /// assert_eq!(action(Ok(None)), "stop");
    /// assert_eq!(action(Err(IoError::Lagged { expected: 12, resume: 30 })), "resubscribe");
    /// // Every other failure propagates. Resubscribing would not fix any of them.
    /// assert_eq!(action(Err(IoError::Closed)), "propagate");
    ///
    /// // The gap is measurable, which is what makes it reportable rather than merely alarming.
    /// let gap = IoError::Lagged { expected: 12, resume: 30 };
    /// let IoError::Lagged { expected, resume } = gap else {
    ///     unreachable!("constructed as a gap")
    /// };
    /// assert_eq!(resume - expected, 18);
    /// // What no resubscription restores is a terminal *screen*. Those bytes come back through
    /// // rmux's own recovery and surface streams; a job snapshot cannot reconstruct them.
    /// ```
    pub async fn recv(&mut self) -> IoResult<Option<Arc<IoEnvelope>>> {
        if let Some(held) = self.held.take() {
            self.expected = held.sequence.saturating_add(1);
            return Ok(Some(held));
        }
        loop {
            match self.receiver.recv().await {
                Ok(envelope) => {
                    self.expected = envelope.sequence.saturating_add(1);
                    return Ok(Some(envelope));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(None),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let expected = self.expected;
                    // One more read, to learn precisely where the stream resumes. Reporting a gap
                    // without its resume point would leave the caller unable to say what it
                    // missed.
                    match self.receiver.recv().await {
                        Ok(envelope) => {
                            let resume = envelope.sequence;
                            self.held = Some(envelope);
                            return Err(IoError::Lagged { expected, resume });
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(IoError::Lagged {
                                expected,
                                resume: expected,
                            });
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
    }
}

/// The bus every observation is published on.
///
/// One sequence for the whole service, assigned under the same lock a subscription registers
/// under, which is what makes [`Observation`]'s "snapshot or stream, never neither" guarantee
/// hold.
#[derive(Debug)]
pub(crate) struct EventBus {
    /// The broadcast channel, taken at teardown.
    ///
    /// Behind an option because closing it is a *guarantee*, not a side effect: an observer that
    /// still holds a stream after the host has shut down must be told the stream is over, and the
    /// only thing that tells it is the last sender going away. A bus kept alive by a retained
    /// facade clone would leave that observer blocked in `recv` for the life of the process.
    sender: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<Arc<IoEnvelope>>>>,
    /// Next sequence to assign, behind the registration lock.
    next: std::sync::Mutex<u64>,
}

impl EventBus {
    /// A bus with no observers.
    pub(crate) fn new() -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(EVENT_BUS_CAPACITY);
        Self {
            sender: std::sync::Mutex::new(Some(sender)),
            next: std::sync::Mutex::new(0),
        }
    }

    /// Publishes one observation, assigning it the next sequence.
    ///
    /// A publication with no observers is not an error: the sequence still advances, so a later
    /// observer's positions stay meaningful.
    pub(crate) fn publish(&self, event: IoEvent) {
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence = *next;
        *next = sequence.saturating_add(1);
        let envelope = Arc::new(IoEnvelope { sequence, event });
        // Under the sequence lock on purpose: two publications must reach every subscriber in the
        // order their sequences were assigned.
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = sender.send(envelope);
        }
        drop(next);
    }

    /// Takes the registration lock, for a caller that must snapshot atomically with subscribing.
    pub(crate) fn registration(&self) -> std::sync::MutexGuard<'_, u64> {
        self.next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers an observer while the caller already holds the registration lock.
    pub(crate) fn subscribe_locked(
        &self,
        registration: &std::sync::MutexGuard<'_, u64>,
    ) -> (IoEventStream, u64) {
        let sequence = **registration;
        (
            IoEventStream {
                receiver: self.receiver(),
                expected: sequence,
                held: None,
            },
            sequence,
        )
    }

    /// A receiver on the live bus, or one that is already closed.
    ///
    /// Subscribing to a closed bus is legitimate and must not panic: a caller may observe a host
    /// that has already shut down, and the honest answer is a stream that immediately ends rather
    /// than one that blocks forever.
    fn receiver(&self) -> tokio::sync::broadcast::Receiver<Arc<IoEnvelope>> {
        match self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            Some(sender) => sender.subscribe(),
            None => {
                let (sender, receiver) = tokio::sync::broadcast::channel(1);
                drop(sender);
                receiver
            }
        }
    }

    /// Closes the bus, ending every outstanding stream.
    ///
    /// Called once, after the final phase has been published, so an observer sees
    /// [`IoPhase::Closed`] and *then* end of stream rather than a stream that silently stops.
    /// Until this runs, a retained facade clone keeps the sender alive and every outstanding
    /// [`IoEventStream::recv`] would block for the life of the process.
    pub(crate) fn close(&self) {
        drop(
            self.sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
    }
}
