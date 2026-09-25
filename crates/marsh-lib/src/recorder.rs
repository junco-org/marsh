//! An in-memory log of records, with the ids its producer stamps them with.
//!
//! Two things a recorder's users repeatedly need, and one they repeatedly get wrong. They need an
//! id allocated *before* the record it belongs to exists — a begin id has to be handed back to a
//! caller that will close the lifecycle later — and they need to observe the log without stopping
//! the producer. What they get wrong is observation: cloning the whole log to count it, or to read
//! one scalar out of each record, copies every argument vector and every path in it.
//!
//! [`Recorder::with_records`] is the answer to that, and [`Recorder::records`] is written in terms
//! of it: a caller that wants the records takes them, and a caller that wants a projection of them
//! pays for the projection only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// An append-only log of `R`, plus the id series its producer stamps records with.
///
/// Allocation and append are separate operations: [`Self::next_id`] hands out the id, and
/// [`Self::push`] appends whatever record was built from it. That is what lets a begin/end pair
/// share one id, and it is why the snapshot order is the *append* order — under concurrency, two
/// producers may allocate their ids in one order and append in the other.
///
/// Poisoning is recovered rather than propagated everywhere below. The guarded code is a slice
/// read and a `push`; a poisoned lock therefore means some unrelated thread died while observing,
/// and losing the whole log to that would defeat the recording.
pub struct Recorder<R> {
    /// The log, in append order.
    records: Mutex<Vec<R>>,
    /// The next id to hand out.
    next: AtomicU64,
}

impl<R> Default for Recorder<R> {
    /// An empty log whose first id is 0.
    ///
    /// Written out rather than derived: a derived `Default` would demand `R: Default`, which a
    /// record type has no reason to implement.
    fn default() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }
}

impl<R> Recorder<R> {
    /// Allocates the next id.
    ///
    /// Zero-based and wrapping, in the order the calls are made. Nothing is appended: the caller
    /// builds its record around the id and pushes it, or — for a lifecycle's second half — reuses
    /// an id allocated earlier.
    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Appends one record.
    pub fn push(&self, record: R) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }

    /// Reads the appended records in place and returns whatever `read` made of them.
    ///
    /// The projection an observer actually wants — a count, a few scalars — costs no clone of the
    /// records themselves. `read` runs **under the recorder's lock**, so it must stay a pure
    /// computation over the slice: no I/O, no signalling, no reentry into this recorder, and no
    /// application callback. Nothing borrowed from the slice can escape, because `U` is owned.
    pub fn with_records<U>(&self, read: impl FnOnce(&[R]) -> U) -> U {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let projection = read(&records);
        drop(records);
        projection
    }

    /// Every record appended so far, in append order.
    ///
    /// A snapshot: later appends are not visible in a vector already returned.
    pub fn records(&self) -> Vec<R>
    where
        R: Clone,
    {
        self.with_records(<[R]>::to_vec)
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A record that cannot be cloned, so a projection that reached for `records()` would not
    /// compile.
    struct Event {
        id: u64,
        payload: String,
    }

    /// Ids are the producer's to hand out, and no two producers may receive the same one.
    #[test]
    fn concurrent_producers_each_get_a_distinct_id() {
        let recorder: Recorder<Event> = Recorder::default();
        let mut allocated = Vec::new();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| (0..64).map(|_| recorder.next_id()).collect::<Vec<_>>()))
                .collect();
            for worker in workers {
                allocated.extend(worker.join().expect("worker"));
            }
        });
        allocated.sort_unstable();
        assert_eq!(allocated, (0..8 * 64).collect::<Vec<_>>());
    }

    /// A snapshot is a value, not a view: what a caller already holds cannot change under it.
    #[test]
    fn a_snapshot_does_not_observe_later_appends() {
        let recorder: Recorder<String> = Recorder::default();
        recorder.push("first".to_owned());
        let early = recorder.records();
        recorder.push("second".to_owned());

        assert_eq!(early, vec!["first".to_owned()]);
        assert_eq!(
            recorder.records(),
            vec!["first".to_owned(), "second".to_owned()]
        );
    }

    /// The projection reads the slice in place: a record type that cannot be cloned at all is
    /// still fully observable.
    #[test]
    fn a_projection_reads_records_that_cannot_be_cloned() {
        let recorder = Recorder::default();
        for _ in 0..3 {
            let id = recorder.next_id();
            recorder.push(Event {
                id,
                payload: format!("payload {id}"),
            });
        }

        assert_eq!(recorder.with_records(<[Event]>::len), 3);
        assert_eq!(
            recorder
                .with_records(|records| records.iter().map(|event| event.id).collect::<Vec<_>>()),
            vec![0, 1, 2]
        );
        assert_eq!(
            recorder.with_records(|records| records.last().map(|event| event.payload.clone())),
            Some("payload 2".to_owned())
        );
    }

    /// An observer dying mid-projection poisons the mutex. The log survives it, because a lost
    /// recording is worse than a lost observation.
    #[test]
    fn a_poisoned_log_keeps_recording() {
        let recorder: Recorder<String> = Recorder::default();
        recorder.push("before".to_owned());
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                recorder.with_records(|_| panic!("an observer died holding the lock"));
            });
            assert!(poisoner.join().is_err(), "the observer must have panicked");
        });

        recorder.push("after".to_owned());
        assert_eq!(
            recorder.records(),
            vec!["before".to_owned(), "after".to_owned()]
        );
    }
}
