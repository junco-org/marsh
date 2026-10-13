//! Cutting a failing case down to the smallest form that still fails.
//!
//! Each reduction is tried once; it is kept only when the case still reproduces the same kind of
//! failure. The predicate re-runs the case, so shrinking costs one run per kept-or-rejected step
//! and only ever runs after a genuine failure, which should be rare.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use super::case::Case;

/// Re-runs a candidate and reports whether it still fails the same way.
pub(super) type Reproduces<'a> =
    dyn Fn(&Case, &BTreeMap<String, String>) -> Pin<Box<dyn Future<Output = bool> + 'a>> + 'a;

/// The smallest case and variant choice that still fails, found by greedily removing structure.
pub(super) async fn minimize(
    case: &Case,
    variants: &BTreeMap<String, String>,
    fails: &Reproduces<'_>,
) -> (Case, BTreeMap<String, String>) {
    let mut case = case.clone();
    let mut variants = variants.clone();

    // 1. Revert each variant slot to the case's first value.
    let first = case.first_variants();
    for (slot, value) in &first {
        if variants.get(slot) == Some(value) {
            continue;
        }
        let mut candidate = variants.clone();
        candidate.insert(slot.clone(), value.clone());
        if fails(&case, &candidate).await {
            variants = candidate;
        }
    }

    // 2. Drop optional and filler steps, one at a time, as long as the failure survives.
    let mut index = 0;
    while index < case.steps.len() {
        if case.steps[index].optional || case.steps[index].filler {
            let mut candidate = case.clone();
            candidate.steps.remove(index);
            if !candidate.steps.is_empty() && fails(&candidate, &variants).await {
                case = candidate;
                continue;
            }
        }
        index += 1;
    }

    // 3. Merge every step onto shell 0, in case the extra shell was not needed.
    if case.steps.iter().any(|step| step.shell != 0) {
        let mut candidate = case.clone();
        for step in &mut candidate.steps {
            step.shell = 0;
        }
        if fails(&candidate, &variants).await {
            case = candidate;
        }
    }

    (case, variants)
}
