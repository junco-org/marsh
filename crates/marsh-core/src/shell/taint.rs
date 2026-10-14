//! Taint that crosses principals.
//!
//! Agent rules follow the principal that read a secret. A principal can hand the secret to
//! another one through a file it writes or a local socket it sends to, so each source keeps one
//! live table of which principals hold data of a propagated class (see [`crate::AgentRuleSet`])
//! and which files carry it. A principal that reads a carrier acquires its classes, and so does
//! one whose commands hold a socket bound to a loopback or unix address a holder contacts. The
//! acquisition enters the observation log as
//! [`Observed::Taint`](super::agent::Observed::Taint), which the rules read like a read of the
//! class itself.
//!
//! The table is updated as each call is observed, while its task is still stopped, so a carrier
//! is known before its writer resumes and a reader's taint before it can reach any peer. Taint is
//! never rolled back: what a principal read stays read even when its command is refused.
//!
//! Files inside the source are carriers by source-relative path, the name every shell's private
//! view shares. Files outside the source are carriers by inode, which renames and hard links keep.
//! A reused inode, or another view's unpublished file of the same name, over-taints; neither
//! under-taints.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;

use junco_policy::Principal;

use super::access::Effects;
use super::agent::AgentPolicy;

/// How a principal came to hold data of `class`: from `from`, through `via`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Taint {
    pub class: String,
    pub from: Principal,
    pub via: String,
}
impl fmt::Display for Taint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} data from {} via {}", self.class, self.from, self.via)
    }
}

/// A file that holds data of a propagated class.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Carrier {
    /// A source-relative path.
    Inside(PathBuf),
    /// A file outside the source, by device and inode.
    Outside(u64, u64),
}

/// One source's live taint, shared by all of its shells.
#[derive(Default)]
pub(super) struct TaintState {
    /// The classes each principal holds, each with how it first came to hold it.
    tainted: HashMap<Principal, BTreeMap<String, Taint>>,
    /// The classes each carrier holds, each with the taint of the principal that wrote it.
    carriers: HashMap<Carrier, BTreeMap<String, Taint>>,
    /// Taint handed to a principal by another one's call, not yet in the receiver's log.
    pending: HashMap<Principal, Vec<Taint>>,
}

impl TaintState {
    /// Folds one observed call of `principal`, classified by `agent`, into the table. Returns the
    /// classes the call handed `principal` through a carrier for the first time; a principal's own
    /// read of a class is already in the log as that read.
    pub(super) fn observe(
        &mut self,
        principal: &Principal,
        agent: &AgentPolicy,
        effects: &Effects,
    ) -> Vec<Taint> {
        let mut acquired = self.pending.remove(principal).unwrap_or_default();
        let own = effects
            .reads
            .iter()
            .map(|path| (agent.inside_class(path), path))
            .chain(effects.outside_reads.iter().filter_map(|path| {
                agent.outside_class(path).map(|class| (class, path))
            }));
        for (class, path) in own {
            if agent.propagates(class) {
                self.hold(
                    principal,
                    Taint {
                        class: class.to_owned(),
                        from: principal.clone(),
                        via: path.display().to_string(),
                    },
                );
            }
        }
        for taint in self.carried(effects) {
            if self.hold(principal, taint.clone()) {
                acquired.push(taint);
            }
        }
        let Some(held) = self.tainted.get(principal).filter(|held| !held.is_empty()) else {
            return acquired;
        };
        let held: Vec<Taint> = held.values().cloned().collect();
        let written = effects
            .writes
            .iter()
            .map(|path| (Carrier::Inside(path.clone()), path.display().to_string()))
            .chain(
                effects
                    .outside_contents
                    .iter()
                    .filter(|content| content.write)
                    .map(|content| {
                        (
                            Carrier::Outside(content.device, content.inode),
                            content.path.display().to_string(),
                        )
                    }),
            );
        for (carrier, via) in written {
            let classes = self.carriers.entry(carrier).or_default();
            for taint in &held {
                classes.entry(taint.class.clone()).or_insert_with(|| Taint {
                    class: taint.class.clone(),
                    from: principal.clone(),
                    via: via.clone(),
                });
            }
        }
        acquired
    }

    /// The classes the carriers `effects` read hold, without recording that anyone read them.
    pub(super) fn carried(&self, effects: &Effects) -> Vec<Taint> {
        effects
            .reads
            .iter()
            .map(|path| Carrier::Inside(path.clone()))
            .chain(
                effects
                    .outside_contents
                    .iter()
                    .filter(|content| content.read)
                    .map(|content| Carrier::Outside(content.device, content.inode)),
            )
            .filter_map(|carrier| self.carriers.get(&carrier))
            .flat_map(BTreeMap::values)
            .cloned()
            .collect()
    }

    /// Every class `principal` holds, however it came to.
    pub(super) fn held(&self, principal: &Principal) -> Vec<Taint> {
        self.tainted
            .get(principal)
            .into_iter()
            .flat_map(BTreeMap::values)
            .cloned()
            .collect()
    }

    /// Hands `principal` the class of `taint` from another principal's call, before that call
    /// delivers anything to it. It enters `principal`'s log at its next observed call.
    pub(super) fn hand(&mut self, principal: &Principal, taint: Taint) {
        if self.hold(principal, taint.clone()) {
            self.pending.entry(principal.clone()).or_default().push(taint);
        }
    }

    /// Every class `principal` holds through another principal's carrier or socket.
    pub(super) fn received(&self, principal: &Principal) -> Vec<Taint> {
        self.tainted
            .get(principal)
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter(|taint| taint.from != *principal)
            .cloned()
            .collect()
    }

    /// Records that `principal` holds `taint`'s class; true when it did not already.
    fn hold(&mut self, principal: &Principal, taint: Taint) -> bool {
        let held = self.tainted.entry(principal.clone()).or_default();
        if held.contains_key(&taint.class) {
            return false;
        }
        held.insert(taint.class.clone(), taint);
        true
    }
}

#[cfg(test)]
#[path = "taint/tests.rs"]
mod tests;
