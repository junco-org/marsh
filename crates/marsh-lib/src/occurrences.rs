//! Recovering which key became which when a table that repeats values is rearranged.
//!
//! A table may hold one value under several keys — a window linked at several indices — and a
//! rearrangement may change every key while each value keeps its number of occurrences. Two owned
//! callers recovered the old-key to new-key correspondence from a before and an after snapshot in
//! the same way: bucket each snapshot's keys by value in encounter order, then pair the buckets
//! positionally, refusing a value whose occurrence count changed.
//!
//! So the bucketing and pairing are shared and the policy is not. Which occurrences a caller
//! pairs explicitly before the positional rest, which keys it leaves out of a snapshot, and what a
//! changed count means stay with the caller, as the entries it hands in and the error it builds.

use std::collections::BTreeMap;

/// Buckets keys by the value they map to, each bucket in encounter order.
///
/// Every key moves into its value's bucket: nothing is cloned and nothing within a bucket is
/// sorted. Entries handed over in ascending key order — as a [`BTreeMap`] iterates — therefore
/// produce buckets in ascending key order too.
pub fn group_keys_by_value<K, V: Ord>(
    entries: impl IntoIterator<Item = (K, V)>,
) -> BTreeMap<V, Vec<K>> {
    let mut by_value = BTreeMap::<V, Vec<K>>::new();
    for (key, value) in entries {
        by_value.entry(value).or_default().push(key);
    }
    by_value
}

/// Extends `output` with the positional correspondence between two groupings of the same values.
///
/// Values are matched in ascending order, and the `n`th key a value has in `before` maps to the
/// `n`th key it has in `after`. A value missing from `after` has no occurrences there; a value only
/// `after` has is ignored, since nothing before corresponds to it. Entries already in `output` stay
/// unless a paired key overwrites them, and an empty `before` adds nothing.
///
/// # Errors
///
/// Returns `mismatch` of the first value, in ascending order, whose occurrence count differs
/// between the two groupings. The pairs of every earlier value are already in `output` by then: a
/// caller that must not observe them extends a map it discards along with the error.
pub fn extend_occurrence_map<K: Ord, V: Ord, E>(
    output: &mut BTreeMap<K, K>,
    before: BTreeMap<V, Vec<K>>,
    mut after: BTreeMap<V, Vec<K>>,
    mismatch: impl FnOnce(&V) -> E,
) -> Result<(), E> {
    for (value, old_keys) in before {
        let new_keys = after.remove(&value).unwrap_or_default();
        if old_keys.len() != new_keys.len() {
            return Err(mismatch(&value));
        }
        output.extend(old_keys.into_iter().zip(new_keys));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key that cannot be cloned, so neither helper can be tempted to duplicate one.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Token(&'static str);

    /// Maps each window index a window occupied before to the one it occupies after, the way a
    /// window remap consumes these helpers.
    fn remap(
        output: &mut BTreeMap<u32, u32>,
        before: &BTreeMap<u32, u32>,
        after: BTreeMap<u32, u32>,
    ) -> Result<(), u32> {
        extend_occurrence_map(
            output,
            group_keys_by_value(before.iter().map(|(&index, &id)| (index, id))),
            group_keys_by_value(after),
            |&id| id,
        )
    }

    #[test]
    fn occurrences_pair_positionally_and_ignore_new_values() {
        let before = BTreeMap::from([(1, 7), (4, 7), (9, 8)]);
        let after = BTreeMap::from([(2, 7), (5, 7), (6, 9), (10, 8)]);
        let mut output = BTreeMap::from([(100, 200)]);

        assert_eq!(remap(&mut output, &before, after), Ok(()));
        assert_eq!(
            output,
            BTreeMap::from([(1, 2), (4, 5), (9, 10), (100, 200)]),
            "value 9 exists only after and maps nothing; the unrelated entry survives"
        );
    }

    #[test]
    fn a_changed_occurrence_count_returns_the_callers_error() {
        let before = BTreeMap::from([(1, 7), (2, 8), (3, 8)]);
        let after = BTreeMap::from([(4, 7), (5, 8)]);
        let mut output = BTreeMap::new();

        assert_eq!(remap(&mut output, &before, after), Err(8));
        assert_eq!(
            output,
            BTreeMap::from([(1, 4)]),
            "the value matched before the mismatch keeps its pairs"
        );

        let vanished = BTreeMap::from([(1, 7)]);
        assert_eq!(
            remap(&mut BTreeMap::new(), &vanished, BTreeMap::new()),
            Err(7),
            "a value missing after has zero occurrences there"
        );
    }

    #[test]
    fn an_empty_before_succeeds_without_adding_pairs() {
        let mut output = BTreeMap::from([(3, 3)]);

        assert_eq!(
            remap(&mut output, &BTreeMap::new(), BTreeMap::from([(1, 7)])),
            Ok(())
        );
        assert_eq!(output, BTreeMap::from([(3, 3)]));
    }

    #[test]
    fn grouping_moves_owned_keys_in_encounter_order() {
        let grouped = group_keys_by_value([
            (Token("c"), 1),
            (Token("a"), 2),
            (Token("b"), 1),
            (Token("d"), 2),
        ]);
        assert_eq!(
            grouped,
            BTreeMap::from([
                (1, vec![Token("c"), Token("b")]),
                (2, vec![Token("a"), Token("d")]),
            ]),
            "buckets keep the order keys arrived in, not key order"
        );

        let mut output = BTreeMap::new();
        let after = group_keys_by_value([
            (Token("y"), 2),
            (Token("x"), 1),
            (Token("w"), 2),
            (Token("z"), 1),
        ]);
        assert_eq!(
            extend_occurrence_map(&mut output, grouped, after, |&value| value),
            Ok(())
        );
        assert_eq!(
            output,
            BTreeMap::from([
                (Token("a"), Token("y")),
                (Token("b"), Token("z")),
                (Token("c"), Token("x")),
                (Token("d"), Token("w")),
            ]),
            "owned keys pair by encounter position within each value"
        );
    }
}
