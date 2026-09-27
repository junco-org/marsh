//! Bounded retention: keeping the newest of a stream and forgetting the oldest.
//!
//! Two shapes keep recurring wherever a long-lived owner remembers a stream it cannot keep in full.
//! One remembers *which* keys it has seen — finished waits, closed panes — so a late duplicate can
//! be recognised; the other retains the items themselves under both a count and a size ceiling.
//! Both evict oldest-first, and both are easy to get subtly wrong by hand: a membership index and
//! its age queue drifting apart, a repeated key silently refreshing its age, a retained-size sum
//! that saturates into apparent validity instead of forcing an eviction.
//!
//! What an evicted item *means* stays with the caller. [`FifoSet`] forgets an old key without
//! telling anyone, because its callers only ask whether a key is still remembered.
//! [`BoundedRetention`] hands every evicted item back, in eviction order, to a callback the caller
//! supplies at the push that caused it — the owner's own bookkeeping runs once per item, while the
//! item is still owned, with no batch collected first.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// A set that remembers at most `limit` keys, forgetting the earliest-inserted first.
///
/// Age is insertion age, not use: inserting a key that is already present changes nothing, and
/// only removing it lets a later insertion give it a new, youngest position. That is FIFO, not LRU,
/// and nothing here sorts.
///
/// The one invariant a single standard collection cannot hold is that `keys` and `order` name
/// exactly the same members, each once: the map answers membership in one probe, the deque says
/// which member is oldest. Membership is a `HashMap<K, ()>` rather than a `HashSet<K>` because the
/// stable entry API decides presence and inserts with a single hash probe, and clones the key for
/// the age queue only when it is actually new.
#[derive(Debug)]
pub struct FifoSet<K> {
    /// Membership, one probe per question.
    #[allow(
        clippy::zero_sized_map_values,
        reason = "the entry API is the single-probe insertion HashSet does not stabilise"
    )]
    keys: HashMap<K, ()>,
    /// The same members, oldest first.
    order: VecDeque<K>,
    /// The most members retained once an insertion returns.
    limit: usize,
}

impl<K> FifoSet<K> {
    /// An empty set that will retain at most `limit` keys.
    ///
    /// Allocates nothing until the first insertion that is retained; a zero limit never allocates.
    #[allow(clippy::zero_sized_map_values, reason = "see the `keys` field")]
    pub fn new(limit: usize) -> Self {
        Self {
            keys: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    /// How many keys are currently remembered.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no key is currently remembered.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

impl<K: Eq + Hash> FifoSet<K> {
    /// Remembers `key` as the youngest member, forgetting the oldest if that exceeds the limit.
    ///
    /// Returns `true` when `key` was absent — including under a zero limit, where it is dropped at
    /// once rather than stored and immediately evicted — and `false` when it was already present,
    /// in which case its age is left as it was. `K: Clone` is needed only for a new key, which the
    /// set holds twice: once to probe, once to age.
    pub fn insert(&mut self, key: K) -> bool
    where
        K: Clone,
    {
        if self.limit == 0 {
            return true;
        }
        match self.keys.entry(key) {
            Entry::Occupied(_) => return false,
            Entry::Vacant(entry) => {
                self.order.push_back(entry.key().clone());
                entry.insert(());
            }
        }
        // At most `limit` members before this insertion, so at most one is now one too many.
        if self.order.len() > self.limit
            && let Some(expired) = self.order.pop_front()
        {
            self.keys.remove(&expired);
        }
        true
    }

    /// Forgets `key`, so that a later insertion gives it a fresh position.
    ///
    /// Returns whether it was remembered; forgetting an absent key changes nothing.
    pub fn remove(&mut self, key: &K) -> bool {
        if self.keys.remove(key).is_none() {
            return false;
        }
        self.order.retain(|candidate| candidate != key);
        true
    }

    /// Whether `key` is currently remembered.
    pub fn contains(&self, key: &K) -> bool {
        self.keys.contains_key(key)
    }
}

/// A FIFO of items retained under both an item-count ceiling and a charge ceiling.
///
/// Each item is pushed with a charge the caller computed — bytes of payload, bytes of record plus
/// heap, whatever the owner budgets by — and that charge is stored beside it, one word per item, so
/// eviction never recomputes a cost that might have changed and the container needs no weight trait
/// or stored function. The invariant this type exists for, which no standard collection holds, is
/// that [`Self::retained_bytes`] is exactly the sum of the stored charges and never exceeds the
/// charge ceiling, while [`Self::len`] never exceeds the item ceiling.
///
/// Admission is decided before the item is stored, by subtraction: an item that fits evicts exactly
/// the oldest items it must, and an item that could never fit evicts everything and then itself.
/// No sum is ever saturated, so an arbitrary ceiling — `usize::MAX` included — is enforced rather
/// than silently overrun.
#[derive(Debug)]
pub struct BoundedRetention<T> {
    /// Retained items with their stored charges, oldest first.
    entries: VecDeque<(T, usize)>,
    /// The most items retained once a push returns.
    item_limit: usize,
    /// The largest total charge retained once a push returns.
    byte_limit: usize,
    /// The sum of the charges in `entries`.
    retained_bytes: usize,
}

impl<T> BoundedRetention<T> {
    /// An empty retention bounded by `item_limit` items and `byte_limit` total charge, reserving
    /// room for `initial_capacity` items up front.
    pub fn new(item_limit: usize, byte_limit: usize, initial_capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(initial_capacity),
            item_limit,
            byte_limit,
            retained_bytes: 0,
        }
    }

    /// Appends `item` with charge `bytes`, first evicting the oldest items it does not fit beside.
    ///
    /// Every evicted item is handed to `on_evict` in FIFO order, each before the next is removed.
    /// When `item` could never fit — a zero item ceiling, or a charge above the charge ceiling —
    /// every retained item is evicted and then `item` itself is handed to `on_evict` without being
    /// stored. A zero charge fits under a zero charge ceiling whenever the item ceiling allows.
    pub fn push(&mut self, item: T, bytes: usize, mut on_evict: impl FnMut(T)) {
        let fits = self.item_limit != 0 && bytes <= self.byte_limit;
        // `byte_limit - bytes` is evaluated only for an item that fits, so it cannot underflow;
        // either way the loop ends no later than the deque empties.
        while (!fits
            || self.entries.len() >= self.item_limit
            || self.retained_bytes > self.byte_limit - bytes)
            && let Some(evicted) = self.evict_oldest()
        {
            on_evict(evicted);
        }
        if !fits {
            on_evict(item);
            return;
        }
        // The loop left `retained_bytes <= byte_limit - bytes`, so this sum cannot overflow.
        self.retained_bytes += bytes;
        self.entries.push_back((item, bytes));
    }

    /// The retained items, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.entries.iter().map(|(item, _)| item)
    }

    /// How many items are retained.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no item is retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The total charge of the retained items.
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Discards every retained item without handing any to an eviction callback.
    ///
    /// The allocated capacity is kept for reuse.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    /// Removes the oldest item and releases its stored charge.
    fn evict_oldest(&mut self) -> Option<T> {
        let (item, bytes) = self.entries.pop_front()?;
        self.retained_bytes -= bytes;
        Some(item)
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// The members of `set` among `candidates`, in candidate order.
    fn members<'a>(set: &FifoSet<String>, candidates: &[&'a str]) -> Vec<&'a str> {
        candidates
            .iter()
            .copied()
            .filter(|candidate| set.contains(&(*candidate).to_owned()))
            .collect()
    }

    /// The retained items, oldest first.
    fn retained(retention: &BoundedRetention<String>) -> Vec<&str> {
        retention.iter().map(String::as_str).collect()
    }

    /// Pushes every `(item, charge)` in order, returning what the pushes evicted, oldest first.
    fn push_all(retention: &mut BoundedRetention<String>, items: &[(&str, usize)]) -> Vec<String> {
        let mut evicted = Vec::new();
        for &(name, bytes) in items {
            retention.push(name.to_owned(), bytes, |item| evicted.push(item));
        }
        evicted
    }

    #[test]
    fn a_fifo_set_forgets_by_insertion_age_not_by_use() {
        const ALL: [&str; 4] = ["a", "b", "c", "d"];
        let mut set = FifoSet::new(2);

        assert!(set.insert("a".to_owned()));
        assert!(set.insert("b".to_owned()));
        assert!(!set.insert("a".to_owned()), "a repeated key is not new");
        assert!(set.insert("c".to_owned()));
        assert_eq!(
            members(&set, &ALL),
            ["b", "c"],
            "the repeat did not refresh a"
        );
        assert_eq!(set.len(), 2);

        assert!(set.remove(&"b".to_owned()));
        assert!(set.insert("b".to_owned()));
        assert!(set.insert("d".to_owned()));
        assert_eq!(
            members(&set, &ALL),
            ["b", "d"],
            "reinsertion made b younger than c"
        );
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn a_zero_limit_fifo_set_remembers_nothing() {
        let mut set = FifoSet::new(0);

        assert!(set.insert("a".to_owned()));
        assert!(set.insert("a".to_owned()), "the key was never stored");
        assert!(!set.contains(&"a".to_owned()));
        assert!(set.is_empty());
    }

    #[test]
    fn removing_an_absent_key_changes_nothing() {
        let mut set = FifoSet::new(2);
        assert!(set.insert("a".to_owned()));

        assert!(!set.remove(&"b".to_owned()));
        assert!(set.contains(&"a".to_owned()));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn one_push_evicts_as_many_oldest_items_as_its_charge_needs() {
        let mut retention = BoundedRetention::new(2, 5, 0);
        let evicted = push_all(&mut retention, &[("a", 2), ("b", 2), ("c", 4)]);
        assert_eq!(evicted, ["a", "b"]);
        assert_eq!(retained(&retention), ["c"]);
        assert_eq!(retention.retained_bytes(), 4);

        assert_eq!(
            push_all(&mut retention, &[("d", 6)]),
            ["c", "d"],
            "an item over the ceiling evicts everything, then itself"
        );
        assert!(retention.is_empty());
        assert_eq!(retention.retained_bytes(), 0);
    }

    #[test]
    fn a_zero_item_limit_hands_every_item_straight_back() {
        let mut retention = BoundedRetention::new(0, 5, 0);
        assert_eq!(push_all(&mut retention, &[("a", 0)]), ["a"]);
        assert!(retention.is_empty());
        assert_eq!(retention.retained_bytes(), 0);
    }

    #[test]
    fn zero_charges_fit_under_a_zero_charge_limit() {
        let mut retention = BoundedRetention::new(2, 0, 0);
        assert!(push_all(&mut retention, &[("a", 0), ("b", 0)]).is_empty());
        assert_eq!(retained(&retention), ["a", "b"]);

        let evicted = push_all(&mut retention, &[("c", 0)]);
        assert_eq!(evicted, ["a"], "the item ceiling still applies");
        assert_eq!(retained(&retention), ["b", "c"]);

        assert_eq!(push_all(&mut retention, &[("d", 1)]), ["b", "c", "d"]);
        assert!(retention.is_empty());
    }

    #[test]
    fn clearing_discards_silently_and_the_retention_stays_usable() {
        let mut retention = BoundedRetention::new(2, 5, 2);
        let evicted = push_all(&mut retention, &[("a", 2), ("b", 3)]);
        assert!(evicted.is_empty(), "nothing to evict");

        retention.clear();
        assert!(retention.is_empty());
        assert_eq!(retention.retained_bytes(), 0);

        let evicted = push_all(&mut retention, &[("c", 5), ("d", 1)]);
        assert_eq!(evicted, ["c"], "the charge ceiling counts from zero again");
        assert_eq!(retained(&retention), ["d"]);
        assert_eq!(retention.retained_bytes(), 1);
    }

    #[test]
    fn a_maximal_charge_ceiling_is_enforced_rather_than_saturated() {
        let mut retention = BoundedRetention::new(2, usize::MAX, 0);
        let evicted = push_all(&mut retention, &[("max", usize::MAX), ("one", 1)]);
        assert_eq!(evicted, ["max"]);
        assert_eq!(retained(&retention), ["one"]);
        assert_eq!(retention.retained_bytes(), 1);
    }
}
