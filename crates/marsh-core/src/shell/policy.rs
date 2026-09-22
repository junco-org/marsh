//! The capability gate: what a command line requested, and whether the committed history allows it.
//!
//! junco-policy's `GitPolicy` borrows a `Bump` arena and `Bump` is `!Sync`, so a compiled policy
//! cannot live behind a lock shared across threads. [`PolicyValidator`] therefore owns only the
//! committed history — plain `Event`s — and compiles the static rule table into a local arena on
//! every check; compilation is microseconds against a command line. That is what lets the
//! process-wide instance be an `Arc<Mutex<PolicyValidator>>`.
//!
//! The history is memory, and the tree it governs is not. A process that began with an empty one
//! would judge every resource unowned, so the first line after any restart would win against work
//! another principal published and never settled. `PolicyValidator::adopt` closes that: a seed's
//! own write-ahead log records the capabilities each transaction was granted, and the session
//! installs them here — once, at reopen, before any shell over that seed can reach
//! [`PolicyValidator::check`].

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use marsh_instrument::BuiltinRecord;
use marsh_wal::CommitOp;
pub use rust_validator::{Action, Event, Principal, Resource};
use rust_validator::{Bump, GitPolicy, PolicyDecision};

use super::builtins::gitcmd::{self, GitAction};

// Keep caller-selected durable identities disjoint from reusable names and snapshot uids.
const PRINCIPAL_NAMESPACE: &str = "@marsh/";

pub(crate) fn durable_principal(name: &str) -> Principal {
    Principal::from(format!("{PRINCIPAL_NAMESPACE}durable/{name}"))
}

pub(crate) fn escaped_live_principal(name: &Principal) -> Option<Principal> {
    name.as_str()
        .starts_with(PRINCIPAL_NAMESPACE)
        .then(|| Principal::from(format!("{PRINCIPAL_NAMESPACE}live/{name}")))
}

/// One refused capability, with the policy's explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// The request that was refused.
    pub event: Event,
    /// The precondition it failed, in the policy's words.
    pub failed_precondition: String,
    /// What would make it legal, in the policy's words; may be empty.
    pub allowed_fixes: Vec<String>,
}

impl std::fmt::Display for Denial {
    /// `"{principal} {action} {resource}: {failed_precondition}"`, then `"\n    fix: {fixes}"`
    /// with the fixes joined by `"; "` when there are any.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {}: {}",
            self.event.principal, self.event.action, self.event.resource, self.failed_precondition
        )?;
        if !self.allowed_fixes.is_empty() {
            write!(f, "\n    fix: {}", self.allowed_fixes.join("; "))?;
        }
        Ok(())
    }
}

/// The committed capability history, and the git policy that judges candidates against it.
#[derive(Debug, Default)]
pub struct PolicyValidator {
    /// Every granted event, in grant order. This is the policy's only input besides the candidate.
    history: Vec<Event>,
}

/// The process-wide validator, built on first use.
static GLOBAL: LazyLock<Arc<Mutex<PolicyValidator>>> = LazyLock::new(Arc::default);

impl PolicyValidator {
    /// An empty history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    /// The process-wide instance: every shell built with it shares one history, which is what makes
    /// one principal's unstaged edit visible to another's request.
    #[must_use]
    pub fn global() -> Arc<Mutex<Self>> {
        GLOBAL.clone()
    }

    /// Installs the capability history a seed's durable log recovered.
    ///
    /// Appended where it lands, which in the ordinary topology — one session, opened before any
    /// shell over it exists — is in front of every event this process will grant. The row
    /// languages hinge on a resource's *last* state change, so a seed adopted into a history that
    /// already carries the same facts decides exactly as one adopted into an empty one.
    ///
    /// Adopting is not a decision. Nothing is judged, nothing is appended to the log, and no
    /// transaction is opened: every event here was granted — and published — by the process that
    /// wrote it, and this only restores what that grant means to the next one.
    ///
    /// Crate-private on purpose. The history is the policy's only input besides the candidate, so
    /// a caller that could write into it could grant itself anything; the only producer is the
    /// seed's own log, read by `Session::open`.
    pub(crate) fn adopt(&mut self, history: impl IntoIterator<Item = Event>) {
        self.history.extend(history);
    }

    /// Decides `events` in order against the committed history, all or nothing.
    ///
    /// A grant is appended before the next event is judged, so a line's later requests see its
    /// earlier ones. Every denial is collected; when there is any, the history is restored to what
    /// it was on entry and the denials are returned — a partially granted line leaves no trace.
    ///
    /// # Errors
    ///
    /// Returns every refused request when the policy denied at least one of them.
    pub fn check(&mut self, events: &[Event]) -> Result<(), Vec<Denial>> {
        let arena = Bump::new();
        let mut policy = GitPolicy::new(&arena);
        let committed = self.history.len();
        let mut denials = Vec::new();
        for event in events {
            match policy.decide(&self.history, event) {
                PolicyDecision::Grant => self.history.push(event.clone()),
                PolicyDecision::Deny {
                    failed_precondition,
                    allowed_fixes,
                } => denials.push(Denial {
                    event: event.clone(),
                    failed_precondition,
                    allowed_fixes,
                }),
            }
        }
        if denials.is_empty() {
            Ok(())
        } else {
            self.history.truncate(committed);
            Err(denials)
        }
    }

    /// The granted events, in grant order.
    #[must_use]
    pub fn history(&self) -> &[Event] {
        &self.history
    }
}

/// The capabilities one command line requested, in the order it requested them.
///
/// Edits come from the tree diff, stamped with the written file's mtime (a removal with its parent
/// directory's); git requests come from the `git` builtin's records, stamped with the builtin's own
/// clock. Merging the two by timestamp is what makes `printf x > p; git add -- p` arrive as an edit
/// followed by a stage, which is what the policy's rows need: they hinge on the *last* state change
/// of a resource.
///
/// A tree change at a path this line asked git to write into — a `Checkout`, `Stash`, `Delete` or
/// `Clean` — is git's doing, not a user edit, and produces nothing. That attribution is by request
/// and never by timestamp: a filesystem stamps mtimes from the kernel's coarse clock, one tick
/// wide, so a file libgit2 wrote can carry a time milliseconds *before* the builtin that wrote it
/// began.
///
/// Paths under `.git/` are published but never requested, and exact duplicates collapse to their
/// first occurrence.
pub(crate) fn translate(
    principal: &Principal,
    snapshot: &Path,
    ops: &[CommitOp],
    builtins: &[BuiltinRecord],
) -> Vec<Event> {
    let mut exits: HashMap<u64, u8> = HashMap::new();
    for record in builtins {
        if let BuiltinRecord::End { id, exit, .. } = record {
            exits.insert(*id, *exit);
        }
    }

    let mut stamped: Vec<(u64, Event)> = Vec::new();
    let mut git_writes: HashSet<PathBuf> = HashSet::new();
    for record in builtins {
        let BuiltinRecord::Begin {
            id,
            ts,
            builtin,
            argv,
            cwd,
            ..
        } = record
        else {
            continue;
        };
        if builtin != "git" || exits.get(id).copied() != Some(0) {
            continue;
        }
        // Unreachable for an exit-0 record: the builtin refused whatever it could not parse.
        let Ok(invocation) = gitcmd::parse(argv) else {
            continue;
        };
        let action = capability_of(invocation.action);
        let writes_the_tree = matches!(
            action,
            Action::Delete | Action::Clean | Action::Checkout | Action::Stash
        );
        for pathspec in &invocation.pathspecs {
            let absolute = gitcmd::resolve(cwd, pathspec);
            // Likewise unreachable at exit 0: the builtin bounds the repository by the snapshot.
            let Some(segments) = gitcmd::relative_segments(snapshot, &absolute) else {
                continue;
            };
            if writes_the_tree {
                git_writes.insert(segments.iter().collect());
            }
            let Some(resource) = resource_from(segments) else {
                continue;
            };
            stamped.push((*ts, Event::new(principal.clone(), action.clone(), resource)));
        }
    }

    for op in ops {
        if git_writes.contains(op.path()) {
            continue;
        }
        let Some(resource) = resource_from(segments_of(op.path())) else {
            continue;
        };
        let stamp = match op {
            CommitOp::Write(relative) => mtime_micros(&snapshot.join(relative)),
            CommitOp::Remove(relative) => snapshot.join(relative).parent().map_or(0, mtime_micros),
        };
        stamped.push((stamp, Event::new(principal.clone(), Action::Edit, resource)));
    }

    stamped.sort_by_key(|(stamp, _)| *stamp);
    let mut events: Vec<Event> = Vec::new();
    for (_, event) in stamped {
        if !events.contains(&event) {
            events.push(event);
        }
    }
    events
}

/// The policy vocabulary for a git operation the builtin performed.
fn capability_of(action: GitAction) -> Action {
    match action {
        GitAction::Stage => Action::Stage,
        GitAction::Delete => Action::Delete,
        GitAction::Commit { message } => Action::Commit { message },
        GitAction::Unstage => Action::Unstage,
        GitAction::Checkout => Action::Checkout,
        GitAction::Stash => Action::Stash,
        GitAction::Clean => Action::Clean,
        GitAction::Diff => Action::Diff,
        GitAction::History => Action::History,
    }
}

/// The normal path components of a seed-relative path.
fn segments_of(relative: &Path) -> Vec<String> {
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The resource `segments` name, or `None` when they name nothing the policy governs: the tree
/// root itself, or anything inside a repository's `.git/`.
fn resource_from(segments: Vec<String>) -> Option<Resource> {
    if segments.is_empty() || segments.iter().any(|segment| segment == ".git") {
        return None;
    }
    Some(Resource::from(segments))
}

/// `path`'s modification time in `CLOCK_REALTIME` microseconds, or 0 when it has none.
fn mtime_micros(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_denied_line_leaves_no_trace() {
        let mut validator = PolicyValidator::new();
        let stage = Event::new("p", Action::Stage, ["src", "x"]);

        let denials = validator
            .check(std::slice::from_ref(&stage))
            .expect_err("staging a path nobody edited is illegal");

        assert_eq!(
            denials,
            vec![Denial {
                event: stage,
                failed_precondition:
                    "stage requires an unstaged resource owned by the acting principal".to_string(),
                allowed_fixes: vec!["p edit src/x before staging".to_string()],
            }]
        );
        assert!(
            validator.history().is_empty(),
            "a denied line commits nothing: {:?}",
            validator.history()
        );
    }

    #[test]
    fn a_line_sees_its_own_grants() {
        let mut validator = PolicyValidator::new();
        let edit = Event::new("p", Action::Edit, ["src", "x"]);
        let stage = Event::new("p", Action::Stage, ["src", "x"]);

        validator
            .check(&[edit.clone(), stage.clone()])
            .expect("the stage sees the edit that precedes it");

        assert_eq!(validator.history(), [edit, stage]);
    }
}
