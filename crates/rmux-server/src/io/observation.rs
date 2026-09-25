//! The one task that drains the shell engine's observations into this daemon.
//!
//! Everything the engine reports arrives here, in order, on one queue. This task is the only
//! consumer, and its rules are narrow on purpose:
//!
//! * **It never awaits a mux mutation.** Not a stop, not a resize, not a `switch`, not a reply
//!   write. Those are scheduled onto separately owned tasks. If this loop could wait on a mux
//!   operation, and that operation could wait for output capacity, the wait would close a cycle
//!   through this very loop.
//! * **It never holds a handler lock across an await.** The handler's state mutex serializes every
//!   RPC in the daemon; blocking it on a byte pump would stall the whole server.
//! * **It acknowledges output promptly.** Each output message carries a receipt the engine's pump
//!   is waiting on. Publishing the bytes and completing the receipt is all this loop does for
//!   them; anything slower belongs on another task.
//!
//! # Ordering
//!
//! Per job and per stream, order is preserved end to end: the engine's pump produces one chunk at
//! a time and waits for its receipt, so a later chunk cannot overtake an earlier one.
//! [`IoEvent::Changed`](crate::io::IoEvent::Changed) carries no state and may coalesce; the
//! authoritative answer to "what is the table now" is always a fresh snapshot.

use std::sync::Arc;

use marsh_core::shellmux::{JobEnd, OutputChannel};
use rmux_core::events::OutputCursorItem;

use crate::io::{IoEvent, IoPhase, ShellIo};
use crate::shell_frontend::FrontendMessage;

/// Drains `events` until the engine's frontend is dropped.
///
/// `handler` is weak: this task must not keep the daemon's request handler — and through it every
/// session, terminal and subscription — alive after the server has otherwise shut down.
pub(crate) async fn consume(
    io: ShellIo,
    handler: crate::handler::WeakRequestHandler,
    mut events: tokio::sync::mpsc::UnboundedReceiver<FrontendMessage>,
) {
    // One ordered delivery task per job stream. The registry lives with this loop, so every task
    // it owns ends when the engine's frontend is dropped and this function returns.
    // One handle per live job generation, from `Opened` to `Closed`. That is what lets a stream
    // event carry something a receiver can act on rather than a name it would have to re-resolve
    // against a table that may already have reused it.
    let mut handles: std::collections::HashMap<
        marsh_core::shellmux::SnapshotUid,
        crate::io::ShellHandle,
    > = std::collections::HashMap::new();
    // One gate per live job generation, opened when its adoption has finished. A job the server
    // created itself opens immediately; an externally created one opens once it has a surface.
    // Stream workers wait on it before their first chunk, which is how adoption stops being
    // something the ingress loop has to wait for without letting bytes overtake it.
    let mut adoptions: std::collections::HashMap<
        marsh_core::shellmux::SnapshotUid,
        tokio::sync::watch::Receiver<bool>,
    > = std::collections::HashMap::new();
    let mut deliveries: std::collections::HashMap<
        (marsh_core::shellmux::SnapshotUid, OutputChannel),
        tokio::sync::mpsc::UnboundedSender<Delivery>,
    > = std::collections::HashMap::new();

    while let Some(message) = events.recv().await {
        match message {
            FrontendMessage::Changed => {
                io.bus().publish(IoEvent::Changed);
                if let Some(handler) = handler.upgrade() {
                    handler.note_shell_state_changed();
                }
            }
            FrontendMessage::Opened { job } => {
                let handle = io.wrap(job);
                handles.insert(handle.sandbox().uid.clone(), handle.clone());
                io.bus().publish(IoEvent::Opened {
                    job: handle.clone(),
                });
                // Adoption takes the handler's state and may refresh a client. This loop is the
                // sole ingress for every pane and every job, so it must not wait for that: an
                // owned task does the work and this gate is what keeps the job's own bytes
                // ordered behind it without holding anything else up.
                let (opened, gate) = tokio::sync::watch::channel(false);
                adoptions.insert(handle.sandbox().uid.clone(), gate);
                let adopting = io.unleased();
                let adopting_handler = handler.clone();
                io.runtime().spawn(async move {
                    adopt(&adopting, &adopting_handler, handle).await;
                    // Sent whatever the outcome: a job whose adoption failed still has to be able
                    // to produce output, and a gate that never opened would silence it forever.
                    let _ = opened.send(true);
                });
            }
            FrontendMessage::CommandAccepted { command } => {
                io.bus().publish(IoEvent::CommandAccepted { command });
            }
            FrontendMessage::Output {
                shell,
                channel,
                bytes,
                receipt,
            } => {
                // Handed to this stream's own delivery task and nothing more. This loop must stay
                // free: it is the single consumer for every pane and every job, so any wait taken
                // here is a wait taken by all of them.
                let key = (shell.uid.clone(), channel);
                let handle = handles.get(&shell.uid).cloned();
                let gate = adoptions.get(&shell.uid).cloned();
                deliveries
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let (sender, queue) = tokio::sync::mpsc::unbounded_channel();
                        io.runtime().spawn(deliver(
                            io.unleased(),
                            handler.clone(),
                            shell.clone(),
                            handle,
                            gate,
                            channel,
                            queue,
                        ));
                        sender
                    })
                    .send(Delivery::Chunk { bytes, receipt })
                    .ok();
            }
            FrontendMessage::Finished { completion } => {
                io.bus().publish(IoEvent::Finished {
                    completion: Arc::clone(&completion),
                });
                if let Some(handler) = handler.upgrade() {
                    handler.note_shell_command_finished(&completion);
                }
            }
            FrontendMessage::Closed { end } => {
                // Onto the same per-stream queues the bytes went onto, so an end of file can
                // never be applied before the last chunk that preceded it. Dropping the sender
                // afterwards is what lets each delivery task finish once it has drained.
                let mut acks = Vec::new();
                for channel in [
                    OutputChannel::Terminal,
                    OutputChannel::Stdout,
                    OutputChannel::Stderr,
                ] {
                    let key = (end.shell.uid.clone(), channel);
                    match deliveries.remove(&key) {
                        Some(sender) => {
                            let (ack, acked) = tokio::sync::oneshot::channel();
                            if sender
                                .send(Delivery::End {
                                    end: Arc::clone(&end),
                                    ack,
                                })
                                .is_ok()
                            {
                                acks.push(acked);
                            }
                        }
                        // A stream that never produced a byte has no delivery task, so its end is
                        // recorded here rather than being lost.
                        None => drop(io.streams().end(&key)),
                    }
                }

                // Retirement always happens on a task of its own, never on this loop. The
                // observation loop must not await a close: it is the single consumer for every
                // pane and every job, and a worker acks only after delivering its last chunk —
                // a delivery that may itself be waiting on a stalled execution owner. Waiting
                // here would let one slow consumer stall every other pane at close time.
                handles.remove(&end.shell.uid);
                adoptions.remove(&end.shell.uid);
                let closer = io.unleased();
                let handler = handler.clone();
                io.runtime().spawn(async move {
                    if acks.is_empty() {
                        // No stream ever produced a byte, so there is no worker to order behind.
                        // The close still has to *happen*: a one-shot `true`, or a pane closed
                        // before its prompt managed a single write, would otherwise stay visually
                        // alive because nothing ever marked it dead. Before the route is
                        // forgotten, because marking the pane dead is what needs it.
                        if let Some(handler) = handler.upgrade() {
                            handler.apply_shell_closed(&end).await;
                        }
                    } else {
                        // Every live worker must finish first. Picking one of them to retire the
                        // job is not a barrier: a pipe job's stdout worker could forget the route
                        // while stderr's end was still pending, and that pending end would then
                        // recreate the stream entry it was closing.
                        for ack in acks {
                            let _ = ack.await;
                        }
                    }
                    // Publication FIRST. `retire_instance` is what wakes a pending idle
                    // shutdown, and that shutdown closes the observation bus — so retiring before
                    // publishing leaves a window in which this job's `Closed` is dropped on the
                    // floor and every observer's stream simply ends without it. The event carries
                    // the whole `JobEnd`, so forgetting the route first costs it nothing.
                    closer.forget_route(&end.shell.uid);
                    closer.bus().publish(IoEvent::Closed {
                        end: Arc::clone(&end),
                    });
                    closer.retire_instance(&end.shell.uid);
                });
            }
            FrontendMessage::Resized { shell, geometry } => {
                if let Some(job) = handles.get(&shell.uid).cloned() {
                    io.bus().publish(IoEvent::Resized { job, geometry });
                }
            }
            FrontendMessage::DefaultResized { geometry } => {
                io.bus().publish(IoEvent::DefaultResized { geometry });
            }
            FrontendMessage::IoError {
                shell,
                channel,
                error,
            } => {
                if let Some(handler) = handler.upgrade() {
                    handler.note_shell_stream_error(&shell, channel, &error);
                }
                if let Some(job) = handles.get(&shell.uid).cloned() {
                    io.bus().publish(IoEvent::IoError {
                        job,
                        channel,
                        error,
                    });
                }
            }
        }
    }
    io.set_phase(IoPhase::Closed);
}

/// Gives a newly opened job a surface, unless something already claimed one for it.
///
/// The admission lock is what makes this safe: a server-initiated spawn installs its route before
/// releasing that lock, so an `Opened` that races ahead of its own `open_shell` call returning finds
/// the route already there and does nothing. Without it, every server-created pane would race to
/// become a second, duplicate pane.
///
/// A job with no route and no claim is *external*: something created it through the native API
/// rather than through rmux. It is adopted as a detached window — never selected, never kept —
/// because appearing is not the same as being asked for.
async fn adopt(
    io: &ShellIo,
    handler: &crate::handler::WeakRequestHandler,
    job: crate::io::ShellHandle,
) {
    // The claim is taken under the admission lock and the lock is released immediately. Holding
    // it across the adoption would invert this daemon's two locks: ordinary pane creation takes
    // the handler's state first and the admission lock second, so an adoption holding admission
    // while waiting for handler state deadlocks both.
    //
    // A reservation is enough for the invariant that matters. `has_route` is checked under the
    // same lock, so a server-initiated spawn — which installs its own route before releasing
    // admission — is never adopted a second time, and neither is a second observation of this job.
    let claimed = {
        let guard = io.admission_lock().lock().await;
        let claim = if io.has_route(&job.sandbox().uid) {
            None
        } else if job.output_channels().contains(&OutputChannel::Terminal) {
            io.install_route(job.sandbox().uid.clone(), crate::io::Route::Adopting);
            Some(true)
        } else {
            // A pipe job is native work with no presentation. It is observable, and it is
            // deliberately not a window, a popup or the current terminal.
            io.install_route(job.sandbox().uid.clone(), crate::io::Route::Hidden);
            Some(false)
        };
        drop(guard);
        claim
    };

    if claimed != Some(true) {
        return;
    }

    let Some(handler) = handler.upgrade() else {
        // Nothing left to adopt into. The reservation must not be left behind, or this job would
        // be permanently neither adopted nor adoptable.
        io.install_route(job.sandbox().uid.clone(), crate::io::Route::Hidden);
        return;
    };
    // Outside the admission lock, and the only place handler state is taken on this path. The
    // adoption replaces the reservation with a real pane route, or falls back to `Hidden`.
    handler.adopt_external_shell(io, &job).await;
}

/// One item on a stream's ordered delivery queue.
enum Delivery {
    /// Bytes, with the engine receipt whose completion lets that stream's pump read again.
    Chunk {
        /// The bytes, shared rather than copied per consumer.
        bytes: Arc<[u8]>,
        /// Completed once this chunk has been applied everywhere.
        receipt: tokio::sync::oneshot::Sender<()>,
    },
    /// The stream is over, and the job it belonged to has ended.
    End {
        /// How the job ended, for the pane-visible part of its closure.
        end: Arc<JobEnd>,
        /// Completed once this worker has applied the close.
        ///
        /// The job is retired only after *every* live worker has completed one of these, so no
        /// channel can still be ending after the route has been forgotten.
        ack: tokio::sync::oneshot::Sender<()>,
    },
}

/// Applies one stream's items, strictly in order.
///
/// This is where the two guarantees the global loop cannot provide are made.
///
/// **Order.** Every item for this stream passes through this one task, so a chunk is retained,
/// rendered and delivered before the next one begins — and an end of file, which arrives on the
/// same queue, can never be applied ahead of the bytes that preceded it. Spawning per chunk
/// instead would leave the order to whichever task won the state lock, which is a corrupted
/// terminal.
///
/// **Backpressure that stays local.** The engine's receipt is completed at the *end* of each
/// chunk, not when it was queued. So a slow transcript or a stalled execution owner holds up
/// exactly one stream's pump; every other pane, every other job and every control operation are
/// untouched. The queue is unbounded and cannot grow: the engine reads one chunk per receipt, so
/// at most one item is ever outstanding.
async fn deliver(
    io: ShellIo,
    handler: crate::handler::WeakRequestHandler,
    shell: marsh_core::shellmux::Sandbox,
    handle: Option<crate::io::ShellHandle>,
    gate: Option<tokio::sync::watch::Receiver<bool>>,
    channel: OutputChannel,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Delivery>,
) {
    // Before the first byte: a pane's transcript cannot receive output until the pane exists, and
    // an externally created job's surface is built by an adoption running on its own task. Waiting
    // here rather than in the ingress loop is what keeps that wait local to this one stream.
    if let Some(mut gate) = gate {
        while !*gate.borrow_and_update() {
            if gate.changed().await.is_err() {
                break;
            }
        }
    }
    let key = (shell.uid.clone(), channel);
    while let Some(item) = queue.recv().await {
        match item {
            Delivery::Chunk { bytes, receipt } => {
                // One retention step. Everything downstream — the transcript, every observer, the
                // execution owner — shares this one allocation.
                let (event, owner) = io.streams().push(&key, bytes);
                if channel == OutputChannel::Terminal {
                    if let Some(handler) = handler.upgrade() {
                        // Two surfaces, one stream, and each answers only for the route it owns:
                        // a pane-routed job reaches a transcript and a popup-routed job reaches an
                        // overlay's screen. Both are awaited inline rather than scheduled, which
                        // is what makes this task's arrival order the surface's parse order — two
                        // chunks handed to two spawned tasks would race for the surface lock and
                        // corrupt a half-parsed escape sequence.
                        handler.apply_shell_output(&shell, &event).await;
                        handler.apply_popup_output(&shell, event.bytes()).await;
                    }
                }
                if let Some(job) = handle.clone() {
                    io.bus().publish(IoEvent::Output {
                        job,
                        channel,
                        output: event.clone(),
                    });
                }
                // The owner of a managed execution is lossless and bounded, so this may wait. It
                // waits on this stream alone.
                if let Some(owner) = owner {
                    let _ = owner.send(OutputCursorItem::Event(event)).await;
                }
                let _ = receipt.send(());
            }
            Delivery::End { end, ack } => {
                drop(io.streams().end(&key));
                if channel == OutputChannel::Terminal {
                    if let Some(handler) = handler.upgrade() {
                        // Derived from the job's own end, not deposited by the prompt. Carrying
                        // it in `JobEnd` removes the race entirely: a deposit made after this
                        // worker had already looked was dropped, and one made before was
                        // rendered, according to whether `command.wait()` happened to return
                        // first — a difference nobody could observe until the verdict went
                        // missing.
                        //
                        // BEFORE the close, because `apply_shell_closed` emits this pane's end of
                        // file. A verdict rendered after that would appear past the end of the
                        // stream it belongs to. The order is output, then verdict, then EOF.
                        // Only an UNRENDERED verdict. A persistent pane renders its own through
                        // the idle-terminal lease and carries on; when it is later stopped,
                        // `JobEnd::completion` still names that same last command, so rendering
                        // unconditionally would print it a second time long after the user saw it.
                        if let Some(completion) = end.completion.as_ref().filter(|completion| {
                            !io.report_was_rendered(&end.shell.uid, completion.id)
                        }) {
                            for line in marsh_core::shellmux::repl::report_lines(
                                &end.shell.id,
                                &completion.outcome,
                            ) {
                                let bytes = format!("{line}\r\n");
                                handler
                                    .apply_final_report(&end.shell, bytes.as_bytes())
                                    .await;
                            }
                        }
                    }
                    if let Some(handler) = handler.upgrade() {
                        // Still has its route: the route is forgotten only after every worker has
                        // acked, and this is what marks the pane dead and emits its end of file.
                        handler.apply_shell_closed(&end).await;
                    }
                }
                // Last, so the retirement cannot begin until this channel is really finished.
                let _ = ack.send(());
                return;
            }
        }
    }
}
