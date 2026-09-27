//! One initializer per key, and a wake-up for everyone who waited on it.
//!
//! Two independent stream kinds elect an initializer the same way: the first caller that finds
//! nothing ready for a key builds it, every later caller for that key waits, and when the builder
//! is done — or the key is torn down under it — every waiter wakes and looks again. What "ready"
//! means is the caller's alone and differs between them, so it arrives as a value the caller has
//! already decided to offer: a cache the caller judges insufficient is simply never passed in.
//!
//! The wake-up is a notification, not a result, which is why this is not [`crate::WaitState`]:
//! nothing terminal is retained, a woken waiter re-reads its own state and may well be elected
//! itself. A token names one initialization for its whole life, so completion reaches the right
//! record even after the key it was elected under has been renamed.
//!
//! Synchronization is the caller's. The gate is plain state, consulted under whatever lock already
//! guards the caches it is routed beside; it owns no mutex and spawns nothing.

use std::collections::HashMap;
use std::hash::Hash;

use tokio::sync::watch;

/// Where one caller stands in a key's initialization.
#[derive(Debug)]
pub enum InitializationRoute<T> {
    /// The caller's own readiness check passed; this is the value it supplied, moved back out.
    Ready(T),
    /// The caller is the key's initializer, and must hand this token to
    /// [`InitializationGate::finish`] once it is done, whether or not it succeeded.
    Initialize {
        /// Names this initialization until it finishes, across any rekey of its key.
        token: u64,
    },
    /// Another caller is initializing the key. The receiver changes to `true` when that
    /// initialization finishes or its key is cancelled, and closes if the gate drops the
    /// initialization without either; in every case the waiter routes again.
    Wait(watch::Receiver<bool>),
}

/// The initialization in flight for one key.
#[derive(Debug)]
struct PendingInitialization {
    /// The identity its initializer was handed in [`InitializationRoute::Initialize`].
    token: u64,
    /// Every waiter holds a receiver of this; it only ever publishes `true`.
    completion: watch::Sender<bool>,
}

impl PendingInitialization {
    /// Ends the initialization, if there is one, and wakes its waiters.
    fn release(pending: Option<Self>) {
        // A key nobody waited on has no receivers, and that is not a failure.
        if let Some(pending) = pending {
            let _ = pending.completion.send(true);
        }
    }
}

/// At most one initializer per key, and the waiters that follow it.
///
/// The invariant is the reason this is a type rather than a bare map: a key has at most one
/// pending initialization, and a pending initialization keeps the token it was elected with
/// wherever [`rekey`](Self::rekey) moves it. Tokens count up from one and saturate at
/// [`u64::MAX`] instead of wrapping. Each gate keeps its own counter and its own elections, so a
/// caller that routes two kinds of initialization over the same keys keeps two gates.
///
/// Dropping the gate drops every pending initialization, and its waiters observe their receiver
/// closing rather than blocking forever.
#[derive(Debug)]
pub struct InitializationGate<K> {
    pending: HashMap<K, PendingInitialization>,
    next_token: u64,
}

/// An empty gate. Written out rather than derived: a derive would demand `K: Default`, which no
/// key needs to be.
impl<K> Default for InitializationGate<K> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            next_token: 0,
        }
    }
}

impl<K> InitializationGate<K> {
    /// How many keys have an initialization in flight.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl<K: Eq + Hash> InitializationGate<K> {
    /// Routes one caller for `key`.
    ///
    /// A supplied `ready` value wins before any pending work is consulted: it is returned as
    /// [`InitializationRoute::Ready`] untouched, and an initialization already in flight for the
    /// key is neither joined nor disturbed. Otherwise the caller waits on the key's initializer if
    /// there is one, or becomes it. Only an election clones `key`; waiting clones nothing.
    pub fn route<T>(&mut self, key: &K, ready: Option<T>) -> InitializationRoute<T>
    where
        K: Clone,
    {
        if let Some(value) = ready {
            return InitializationRoute::Ready(value);
        }
        if let Some(pending) = self.pending.get(key) {
            return InitializationRoute::Wait(pending.completion.subscribe());
        }
        self.next_token = self.next_token.saturating_add(1);
        let token = self.next_token;
        let (completion, _) = watch::channel(false);
        self.pending
            .insert(key.clone(), PendingInitialization { token, completion });
        InitializationRoute::Initialize { token }
    }

    /// Ends the initialization elected with `token` and wakes its waiters, under whatever key it
    /// now lives.
    ///
    /// A token that already finished, was cancelled with its key, or was replaced by a rekey names
    /// nothing, so a late finish can never release a later initializer of the same key.
    pub fn finish(&mut self, token: u64) {
        let finished = self
            .pending
            .extract_if(|_, pending| pending.token == token)
            .next();
        PendingInitialization::release(finished.map(|(_, pending)| pending));
    }

    /// Ends `key`'s initialization, if it has one, and wakes its waiters.
    ///
    /// Its initializer's own later [`finish`](Self::finish) then names nothing.
    pub fn cancel(&mut self, key: &K) {
        PendingInitialization::release(self.pending.remove(key));
    }

    /// Moves `previous`'s initialization, token and waiters included, to `current`.
    ///
    /// Nothing is cloned or allocated unless a record actually moves: a `previous` with nothing in
    /// flight leaves `current` as it was. A record already at `current` is replaced, and its
    /// waiters see their receiver close. Rekeying a key onto itself keeps its initialization.
    pub fn rekey(&mut self, previous: &K, current: &K)
    where
        K: Clone,
    {
        if let Some(pending) = self.pending.remove(previous) {
            self.pending.insert(current.clone(), pending);
        }
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A ready value that cannot be cloned: the gate hands it back or the test does not compile.
    #[derive(Debug, PartialEq, Eq)]
    struct Payload(u32);

    /// Where one waiter stands, read without awaiting, so a waiter that is still blocked fails an
    /// assertion instead of hanging the test.
    #[derive(Debug, PartialEq, Eq)]
    enum Observed {
        /// Its initialization is still in flight.
        Blocked,
        /// Its initialization finished or was cancelled, and said so.
        Released,
        /// Its initialization was dropped without a word.
        Closed,
    }

    fn observe(waiter: &watch::Receiver<bool>) -> Observed {
        match waiter.has_changed() {
            Ok(false) => Observed::Blocked,
            Ok(true) => panic!("a notified initialization has already left the gate"),
            Err(_) if *waiter.borrow() => Observed::Released,
            Err(_) => Observed::Closed,
        }
    }

    fn key(name: &str) -> String {
        name.to_owned()
    }

    /// Routes `name` with nothing ready and requires the caller to be elected.
    fn elect(gate: &mut InitializationGate<String>, name: &str) -> u64 {
        let InitializationRoute::Initialize { token } = gate.route::<Payload>(&key(name), None)
        else {
            panic!("{name} should elect an initializer");
        };
        token
    }

    /// Routes `name` with nothing ready and requires the caller to wait.
    fn join(gate: &mut InitializationGate<String>, name: &str) -> watch::Receiver<bool> {
        let InitializationRoute::Wait(waiter) = gate.route::<Payload>(&key(name), None) else {
            panic!("{name} should wait on its initializer");
        };
        waiter
    }

    #[tokio::test]
    async fn one_initializer_per_key_wakes_every_waiter_before_re_election() {
        let mut gate = InitializationGate::default();
        let token = elect(&mut gate, "pane");
        let waiters = (0..3)
            .map(|_| {
                let mut waiter = join(&mut gate, "pane");
                tokio::spawn(async move {
                    waiter.changed().await.expect("the initializer notifies");
                    *waiter.borrow()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(gate.pending_count(), 1);
        tokio::task::yield_now().await;
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));

        gate.finish(token);
        for waiter in waiters {
            assert!(waiter.await.expect("waiter task"));
        }
        assert_eq!(gate.pending_count(), 0);
        assert_ne!(elect(&mut gate, "pane"), token);
    }

    #[test]
    fn keys_elect_and_finish_independently() {
        let mut gate = InitializationGate::default();
        let left = elect(&mut gate, "left");
        let right = elect(&mut gate, "right");
        assert_ne!(left, right);
        let left_waiter = join(&mut gate, "left");
        let right_waiter = join(&mut gate, "right");
        assert_eq!(gate.pending_count(), 2);

        gate.finish(left);
        assert_eq!(observe(&left_waiter), Observed::Released);
        assert_eq!(observe(&right_waiter), Observed::Blocked);
        assert_eq!(gate.pending_count(), 1);
    }

    #[test]
    fn cancelling_one_key_leaves_another_initializing() {
        let mut gate = InitializationGate::default();
        let cancelled = elect(&mut gate, "cancelled");
        let _other = elect(&mut gate, "other");
        let cancelled_waiter = join(&mut gate, "cancelled");
        let other_waiter = join(&mut gate, "other");

        gate.cancel(&key("cancelled"));
        assert_eq!(observe(&cancelled_waiter), Observed::Released);
        assert_eq!(observe(&other_waiter), Observed::Blocked);
        gate.cancel(&key("absent"));
        assert_eq!(gate.pending_count(), 1);

        // The cancelled key elects afresh, and its old initializer finishing late releases no one.
        let _replacement = elect(&mut gate, "cancelled");
        let replacement_waiter = join(&mut gate, "cancelled");
        gate.finish(cancelled);
        assert_eq!(observe(&replacement_waiter), Observed::Blocked);
        assert_eq!(gate.pending_count(), 2);
    }

    #[test]
    fn a_stale_token_does_not_release_a_later_initializer() {
        let mut gate = InitializationGate::default();
        let stale = elect(&mut gate, "pane");
        gate.finish(stale);
        let current = elect(&mut gate, "pane");
        let waiter = join(&mut gate, "pane");

        gate.finish(stale);
        assert_eq!(observe(&waiter), Observed::Blocked);
        let _still_waiting = join(&mut gate, "pane");

        gate.finish(current);
        assert_eq!(observe(&waiter), Observed::Released);
    }

    #[test]
    fn a_ready_value_takes_precedence_without_disturbing_an_initializer() {
        let mut gate = InitializationGate::default();
        let token = elect(&mut gate, "pane");
        let waiter = join(&mut gate, "pane");

        let InitializationRoute::Ready(value) = gate.route(&key("pane"), Some(Payload(7))) else {
            panic!("a supplied value is ready");
        };
        assert_eq!(value, Payload(7));
        assert_eq!(gate.pending_count(), 1);
        assert_eq!(observe(&waiter), Observed::Blocked);
        let joined = join(&mut gate, "pane");

        gate.finish(token);
        assert_eq!(observe(&waiter), Observed::Released);
        assert_eq!(observe(&joined), Observed::Released);
        // With nothing in flight, a ready value elects no one either.
        assert!(matches!(
            gate.route(&key("pane"), Some(Payload(8))),
            InitializationRoute::Ready(Payload(8))
        ));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn completion_follows_an_initialization_to_its_new_key() {
        let mut gate = InitializationGate::default();
        let token = elect(&mut gate, "before");
        let waiter = join(&mut gate, "before");

        gate.rekey(&key("before"), &key("after"));
        assert_eq!(gate.pending_count(), 1);
        let follower = join(&mut gate, "after");
        let _vacated = elect(&mut gate, "before");
        let vacated_waiter = join(&mut gate, "before");

        gate.finish(token);
        assert_eq!(observe(&waiter), Observed::Released);
        assert_eq!(observe(&follower), Observed::Released);
        assert_eq!(observe(&vacated_waiter), Observed::Blocked);
        let _re_elected = elect(&mut gate, "after");
    }

    #[test]
    fn a_rekey_that_moves_nothing_leaves_every_initialization_in_place() {
        let mut gate = InitializationGate::default();
        let destination = elect(&mut gate, "destination");
        let destination_waiter = join(&mut gate, "destination");

        gate.rekey(&key("vacant"), &key("destination"));
        assert_eq!(gate.pending_count(), 1);
        assert_eq!(observe(&destination_waiter), Observed::Blocked);
        let _vacant = elect(&mut gate, "vacant");

        gate.rekey(&key("destination"), &key("destination"));
        assert_eq!(gate.pending_count(), 2);
        assert_eq!(observe(&destination_waiter), Observed::Blocked);
        gate.finish(destination);
        assert_eq!(observe(&destination_waiter), Observed::Released);
    }

    #[test]
    fn rekeying_onto_an_occupied_key_replaces_its_initialization() {
        let mut gate = InitializationGate::default();
        let moved = elect(&mut gate, "source");
        let replaced = elect(&mut gate, "destination");
        let moved_waiter = join(&mut gate, "source");
        let replaced_waiter = join(&mut gate, "destination");

        gate.rekey(&key("source"), &key("destination"));
        assert_eq!(gate.pending_count(), 1);
        assert_eq!(observe(&replaced_waiter), Observed::Closed);
        assert_eq!(observe(&moved_waiter), Observed::Blocked);

        gate.finish(replaced);
        assert_eq!(observe(&moved_waiter), Observed::Blocked);
        gate.finish(moved);
        assert_eq!(observe(&moved_waiter), Observed::Released);
        assert_eq!(gate.pending_count(), 0);
    }
}
