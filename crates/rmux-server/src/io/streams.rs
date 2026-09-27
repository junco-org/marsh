//! Per-stream retention: one shared ring per job stream, plus the lossless owner channel an
//! [`Execution`](crate::io::Execution) arms before its command starts.
//!
//! Two kinds of reader want a job's bytes, and they want different guarantees.
//!
//! * The **owner** of a managed execution wants every byte. It armed its receivers before the
//!   command was admitted, precisely so a fast process cannot finish before anyone is listening.
//!   It is bounded, so a slow owner applies backpressure to its own job rather than growing the
//!   daemon; it is never dropped silently.
//! * An **observer** wants to watch without being able to stall anything. It gets an independent
//!   cursor over a bounded ring, and an explicit [`OutputCursorItem::Gap`] when it falls behind.
//!
//! Both read the same bytes. Each chunk the core hands the frontend is copied **once**, into an
//! `Arc<[u8]>`, and shared from there: `rmux_core::events::OutputEvent` already stores shared
//! bytes and `OutputRing::push_shared` already takes them, so there is no second byte-copying ring
//! to build.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use marsh_core::shellmux::{OutputChannel, Principal};
use rmux_core::events::{
    OutputCursor, OutputCursorItem, OutputEvent, OutputRing, DEFAULT_OUTPUT_RING_CAPACITY,
    DEFAULT_RECENT_LIVE_BUFFER_CAPACITY,
};

/// How many chunks an owner may fall behind before its job's pump waits.
///
/// Bounded on purpose. An unbounded owner queue converts a slow consumer into unbounded daemon
/// memory; this converts it into backpressure on the one job it belongs to, which is what the core
/// already expects and what every other stream is unaffected by.
const OWNER_CAPACITY: usize = 64;

/// One job stream's identity.
pub(crate) type StreamKey = (Principal, OutputChannel);

/// One stream's retained bytes and its registered readers.
#[derive(Debug)]
struct Stream {
    /// Bounded retention every observer reads through its own cursor.
    ring: OutputRing,
    /// The lossless owner of a managed execution, when one armed itself.
    owner: Option<tokio::sync::mpsc::Sender<OutputCursorItem>>,
    /// Wakes observers blocked on an empty ring.
    signal: Arc<tokio::sync::Notify>,
    /// Set once the stream has ended, so an observer stops waiting.
    ended: bool,
    /// How many observers still hold a cursor into this stream.
    ///
    /// An ended stream is kept alive while any of them remain, so a reader that fell behind still
    /// drains its retained bytes — and its explicit gaps — before it is told the stream is over.
    /// Dropping the storage at end of file instead would turn "you fell behind" into "there was
    /// never anything here", which is byte loss reported as a clean end.
    readers: usize,
}

impl Stream {
    /// A stream with rmux's own retention defaults and no readers.
    fn new() -> Self {
        Self {
            ring: OutputRing::new(
                DEFAULT_OUTPUT_RING_CAPACITY,
                DEFAULT_RECENT_LIVE_BUFFER_CAPACITY,
            ),
            owner: None,
            signal: Arc::new(tokio::sync::Notify::new()),
            ended: false,
            readers: 0,
        }
    }
}

/// Every job stream this host retains.
#[derive(Debug, Default)]
pub(crate) struct Streams {
    /// Live streams and ended tombstones, behind ONE lock.
    ///
    /// They cannot be two locks. A reader that consulted the tombstones and then took a second
    /// lock to create its entry could be overtaken in between by the end that removes the stream
    /// and records the tombstone — and would then create a fresh, not-ended entry for a stream
    /// that is over, which is the resurrection this tombstone exists to prevent. One lock makes
    /// check-and-create atomic with end-and-remember.
    state: Mutex<State>,
}

/// Everything [`Streams`] holds, so one lock covers all of it.
#[derive(Debug, Default)]
struct State {
    /// Keyed by principal and channel, so a reused job name never mixes two generations.
    streams: HashMap<StreamKey, Stream>,
    /// Streams that ended and were reclaimed, most recent last.
    ///
    /// Without this, observing a job that has already closed *recreates* its entry — empty and
    /// not ended — and the observer then waits forever for bytes that can never arrive. The
    /// existing closed-host test does not catch it, because there the whole facade refuses first;
    /// this is the closed-*job*, live-host case.
    ///
    /// Bounded, because a long-lived daemon must not accumulate one entry per job it has ever
    /// run. Forgetting the oldest is safe: an observer arriving that long after a job ended is
    /// asking about something the retention rings no longer hold either.
    finished: std::collections::VecDeque<StreamKey>,
}

/// How many ended streams are remembered, so observing a just-closed job answers rather than
/// waits.
const FINISHED_TOMBSTONES: usize = 1024;

impl Streams {
    /// Arms a lossless owner receiver for one stream, before any byte can be produced.
    ///
    /// Called while the job is still idle. That ordering is the whole point: a pipe execution
    /// creates an idle job, arms both receivers, and only then admits its command, so the first
    /// bytes of a fast process cannot be produced before anyone is listening.
    pub(crate) fn arm_owner(
        &self,
        key: StreamKey,
    ) -> tokio::sync::mpsc::Receiver<OutputCursorItem> {
        let (sender, receiver) = tokio::sync::mpsc::channel(OWNER_CAPACITY);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.streams.entry(key).or_insert_with(Stream::new).owner = Some(sender);
        drop(state);
        receiver
    }

    /// Records one chunk, returning the owner sender that still has to accept it.
    ///
    /// The ring is updated under the lock; the owner delivery is *not*, because sending into a
    /// full bounded channel waits, and waiting under this lock would stall every other stream.
    pub(crate) fn push(
        &self,
        key: &StreamKey,
        bytes: Arc<[u8]>,
    ) -> (
        OutputEvent,
        Option<tokio::sync::mpsc::Sender<OutputCursorItem>>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let stream = state.streams.entry(key.clone()).or_insert_with(Stream::new);
        let sequence = stream.ring.push_shared(Arc::clone(&bytes));
        let event = OutputEvent::from_shared(sequence, bytes, Vec::new());
        let owner = stream.owner.clone();
        stream.signal.notify_waiters();
        drop(state);
        (event, owner)
    }

    /// Marks a stream ended, so observers stop waiting and owners see end of file.
    ///
    /// The storage goes with it *only* when nobody is reading. An ended stream with observers
    /// still attached is kept until they drain, because those retained bytes are theirs.
    pub(crate) fn end(
        &self,
        key: &StreamKey,
    ) -> Option<tokio::sync::mpsc::Sender<OutputCursorItem>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let stream = state.streams.entry(key.clone()).or_insert_with(Stream::new);
        stream.ended = true;
        let owner = stream.owner.take();
        stream.signal.notify_waiters();
        if stream.readers == 0 {
            // Nothing is reading this and nothing ever will: retained bytes of an ended stream are
            // not activity, and keeping them would make a long-lived daemon's memory a function of
            // how many jobs it has ever run. The tombstone is recorded under the same lock, so an
            // observer cannot slip between the removal and the record.
            state.streams.remove(key);
            remember_finished(&mut state, key);
        }
        drop(state);
        owner
    }

    /// Releases one observer's claim, reclaiming an ended stream once the last one is gone.
    pub(crate) fn release(&self, key: &StreamKey) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(stream) = state.streams.get_mut(key) {
            stream.readers = stream.readers.saturating_sub(1);
            if stream.ended && stream.readers == 0 {
                state.streams.remove(key);
                remember_finished(&mut state, key);
            }
        }
        drop(state);
    }

    /// Registers an observer cursor at `start`.
    pub(crate) fn observe(
        &self,
        key: StreamKey,
        start: rmux_sdk::PaneOutputStart,
        job_closed: bool,
    ) -> (OutputCursor, Arc<tokio::sync::Notify>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Checked and created under ONE lock. Split across two, an end could remove the stream and
        // record its tombstone in between, and this would then create a fresh not-ended entry for
        // a stream that is over — an observer waiting forever for bytes that can never arrive.
        // Two sources, because neither alone is sufficient. The caller's `job_closed` comes from
        // the job's own closure watch and never ages out, but it is read before this lock and so
        // can miss a stream that ended in between. The tombstone closes exactly that window, but
        // it is bounded and an old entry is evicted. Together they are complete: a recently ended
        // stream is caught by the tombstone, an old one by the job's closure.
        let already_ended = job_closed || state.finished.iter().any(|known| *known == key);
        let stream = state.streams.entry(key).or_insert_with(Stream::new);
        if already_ended {
            stream.ended = true;
        }
        stream.readers += 1;
        let cursor = match start {
            rmux_sdk::PaneOutputStart::Oldest => stream.ring.cursor_from_oldest(),
            // `PaneOutputStart` is `#[non_exhaustive]`; anything this build does not know starts
            // from now, which retains nothing it was not asked for.
            _ => stream.ring.cursor_from_now(),
        };
        let signal = Arc::clone(&stream.signal);
        drop(state);
        (cursor, signal)
    }

    /// Reads the next item for `cursor`, or reports that the stream has ended.
    pub(crate) fn poll(&self, key: &StreamKey, cursor: &mut OutputCursor) -> Poll {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(stream) = state.streams.get_mut(key) else {
            drop(state);
            return Poll::Ended;
        };
        if let Some(item) = stream.ring.poll_cursor(cursor) {
            drop(state);
            return Poll::Item(item);
        }
        let ended = stream.ended;
        drop(state);
        if ended {
            Poll::Ended
        } else {
            Poll::Pending
        }
    }
}

/// What one observer poll found.
pub(crate) enum Poll {
    /// A retained event or an explicit gap.
    Item(OutputCursorItem),
    /// Nothing yet; wait on the stream's signal.
    Pending,
    /// The stream is over and nothing more will arrive.
    Ended,
}

/// Records that a stream ended and its storage is gone, under the caller's existing lock.
fn remember_finished(state: &mut State, key: &StreamKey) {
    state.finished.push_back(key.clone());
    while state.finished.len() > FINISHED_TOMBSTONES {
        state.finished.pop_front();
    }
}
