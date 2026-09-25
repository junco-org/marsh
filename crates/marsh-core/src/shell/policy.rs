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

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

pub use rust_validator::{Action, Event, Principal, Resource};
use rust_validator::{Bump, GitPolicy, PolicyDecision};

use super::MarshError;
use super::builtins::gitcmd::{self, GitAction};
use super::session::GitEffectRecord;

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
/// Two streams meet here. `edits` are the line's own changes to content — every path its
/// publication carries that is a file, a symlink, or a file a directory replaced — each with the
/// trace timestamps of the calls that wrote it. `git` is what each observed git invocation did,
/// already in the git vocabulary and still unmapped. The trace and the invocation windows share
/// one clock, so the line splits into segments: before the first invocation, each invocation's
/// own window, between invocations, and after the last. That is what makes
/// `printf a > p; git add -- p; printf b > p; git add -- p` arrive as edit, stage, edit, stage —
/// the order the policy's rows need, since they hinge on the *last* state change of a resource.
///
/// Within an invocation's window, a path it wrote is its own doing when one of its recorded
/// actions maps onto a state change of that path; otherwise the write is an ordinary edit,
/// requested before the invocation's own actions. Writes under the repository metadata an
/// invocation named are never edits. A recorded action that changes no state — a read, a diff,
/// a history — requests nothing at all: observing a resource takes no claim on it.
///
/// Paths under `.git/` are published but never requested, and only a consecutive repetition of
/// the same action on one resource collapses: an edit, a stage and an edit again are three
/// transitions and all three are requested.
///
/// # Errors
///
/// Fails when a path cannot be named losslessly as a policy resource — a component that is not
/// a plain UTF-8 name — because two different files would otherwise be requested as one.
pub(crate) fn translate(
    principal: &Principal,
    edits: &[(PathBuf, Vec<u64>)],
    git: &[GitEffectRecord],
) -> Result<Vec<Event>, MarshError> {
    let mut requests = Requests {
        principal,
        events: Vec::new(),
        last: HashMap::new(),
    };
    let mut records: Vec<&GitEffectRecord> = git.iter().collect();
    records.sort_by_key(|record| record.started_at);

    let mut since = 0;
    for record in records {
        for path in written_between(edits, since, record.started_at) {
            requests.push(Action::Edit, path)?;
        }
        let mapped: Vec<(Action, &Path)> = record
            .requests
            .iter()
            .map(|(action, path)| (capability_of(action.clone()), path.as_path()))
            .filter(|(action, _)| action.is_write())
            .collect();
        for path in written_between(edits, record.started_at, record.finished_at) {
            let accounted = mapped.iter().any(|(_, requested)| *requested == path)
                || record.metadata.iter().any(|root| path.starts_with(root));
            if !accounted {
                requests.push(Action::Edit, path)?;
            }
        }
        for (action, path) in mapped {
            requests.push(action, path)?;
        }
        since = record.finished_at;
    }
    for path in written_between(edits, since, u64::MAX) {
        requests.push(Action::Edit, path)?;
    }
    Ok(requests.events)
}

/// The requests of one line as they accumulate, with the last action each resource received.
struct Requests<'a> {
    /// Who is asking.
    principal: &'a Principal,
    /// Everything requested so far, in order.
    events: Vec<Event>,
    /// The action each resource was last requested for, so a repetition collapses.
    last: HashMap<Resource, Action>,
}

impl Requests<'_> {
    /// Requests `action` over `path`, unless it names nothing the policy governs or repeats the
    /// resource's previous request.
    fn push(&mut self, action: Action, path: &Path) -> Result<(), MarshError> {
        let Some(resource) = resource_of(path)? else {
            return Ok(());
        };
        if self.last.get(&resource) == Some(&action) {
            return Ok(());
        }
        self.last.insert(resource.clone(), action.clone());
        self.events
            .push(Event::new(self.principal.clone(), action, resource));
        Ok(())
    }
}

/// The edited paths with a write in `(after, until]`, ordered by the first such write.
fn written_between(edits: &[(PathBuf, Vec<u64>)], after: u64, until: u64) -> Vec<&Path> {
    let mut written: Vec<(u64, &Path)> = edits
        .iter()
        .filter_map(|(path, stamps)| {
            stamps
                .iter()
                .copied()
                .filter(|stamp| *stamp > after && *stamp <= until)
                .min()
                .map(|first| (first, path.as_path()))
        })
        .collect();
    written.sort_by_key(|(first, _)| *first);
    written.into_iter().map(|(_, path)| path).collect()
}

/// The policy vocabulary for a git operation.
///
/// The one mapping between the two. Whether an action reads or changes a resource is the
/// policy's own answer — [`Action::is_read`], [`Action::is_write`] — asked of what this returns.
pub(crate) fn capability_of(action: GitAction) -> Action {
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
        GitAction::Read => Action::Read,
        GitAction::Edit => Action::Edit,
    }
}

/// The resource a snapshot-relative path names, or `None` when it names nothing the policy
/// governs: the tree root itself, or anything inside a repository's `.git/`.
///
/// # Errors
///
/// Fails when a component is not a plain UTF-8 name.
fn resource_of(relative: &Path) -> Result<Option<Resource>, MarshError> {
    let lossless = relative
        .components()
        .all(|component| matches!(component, Component::Normal(part) if part.to_str().is_some()));
    if !lossless {
        return Err(MarshError::Io(std::io::Error::other(format!(
            "{} cannot be requested as a policy resource: its path is not a plain UTF-8 name",
            relative.display()
        ))));
    }
    Ok(gitcmd::relative_segments(Path::new(""), relative).and_then(resource_from))
}

/// The resource `segments` name, or `None` when they name nothing the policy governs: the tree
/// root itself, or anything inside a repository's `.git/`.
fn resource_from(segments: Vec<String>) -> Option<Resource> {
    if segments.is_empty() || segments.iter().any(|segment| segment == ".git") {
        return None;
    }
    Some(Resource::from(segments))
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

    /// One invocation's window with `requests` in it.
    fn invocation(started_at: u64, requests: Vec<(GitAction, &str)>) -> GitEffectRecord {
        GitEffectRecord {
            started_at,
            finished_at: started_at + 10,
            requests: requests
                .into_iter()
                .map(|(action, path)| (action, PathBuf::from(path)))
                .collect(),
            metadata: Vec::new(),
        }
    }

    /// Recorded git actions reach the real validator through the one mapping, so the policy's
    /// staging and settlement rules hold for them: nobody stages what nobody edited, nobody
    /// restores another principal's unstaged edit, and a committed resource is free again.
    #[test]
    fn mapped_git_actions_enforce_staging_and_settlement() {
        let mut validator = PolicyValidator::new();
        let (a, b) = (Principal::from("a"), Principal::from("b"));
        let actions = |events: &[Event]| -> Vec<Action> {
            events.iter().map(|event| event.action.clone()).collect()
        };

        let stage = translate(
            &a,
            &[],
            &[invocation(10, vec![(GitAction::Stage, "src/p")])],
        )
        .expect("translate");
        assert_eq!(
            stage,
            vec![Event::new(a.clone(), Action::Stage, ["src", "p"])]
        );
        validator
            .check(&stage)
            .expect_err("a stage of a clean resource");

        let edit = translate(&a, &[(PathBuf::from("src/p"), vec![5])], &[]).expect("translate");
        validator.check(&edit).expect("a's edit");

        let checkout = translate(
            &b,
            &[],
            &[invocation(20, vec![(GitAction::Checkout, "src/p")])],
        )
        .expect("translate");
        assert_eq!(actions(&checkout), [Action::Checkout]);
        validator
            .check(&checkout)
            .expect_err("b may not restore a's unstaged edit");

        let settle = translate(
            &a,
            &[],
            &[
                invocation(30, vec![(GitAction::Stage, "src/p")]),
                invocation(
                    50,
                    vec![(
                        GitAction::Commit {
                            message: Some("saved".to_string()),
                        },
                        "src/p",
                    )],
                ),
            ],
        )
        .expect("translate");
        assert_eq!(actions(&settle), [Action::Stage, Action::commit("saved")]);
        validator.check(&settle).expect("a stages and commits");

        let after = translate(&b, &[(PathBuf::from("src/p"), vec![70])], &[]).expect("translate");
        validator
            .check(&after)
            .expect("the committed resource is b's to edit");

        let observed = translate(
            &a,
            &[],
            &[invocation(
                80,
                vec![
                    (GitAction::Read, "src/p"),
                    (GitAction::Diff, "src/p"),
                    (GitAction::History, "src/p"),
                ],
            )],
        )
        .expect("translate");
        assert!(
            observed.is_empty(),
            "observing claims nothing: {observed:?}"
        );
    }

    /// The trace's clock splits a line around its git invocations, so repeated transitions of
    /// one resource survive in order; a write inside an invocation's window is its own when it
    /// recorded an action there, and an ordinary edit otherwise.
    #[test]
    fn edits_and_git_actions_interleave_by_window() {
        let edits = [
            (PathBuf::from("p"), vec![5, 25]),
            (PathBuf::from("hook.log"), vec![55]),
            (PathBuf::from("q"), vec![15]),
            (PathBuf::from("repo.git/HEAD"), vec![75]),
        ];
        let git = [
            invocation(10, vec![(GitAction::Stage, "p")]),
            invocation(30, vec![(GitAction::Stage, "p")]),
            invocation(50, vec![(GitAction::Checkout, "hook.log")]),
            GitEffectRecord {
                metadata: vec![PathBuf::from("repo.git")],
                ..invocation(70, Vec::new())
            },
        ];
        let events = translate(&Principal::from("a"), &edits, &git).expect("translate");
        let rendered: Vec<String> = events
            .iter()
            .map(|event| format!("{} {}", event.action, event.resource))
            .collect();
        assert_eq!(
            rendered,
            [
                "edit p",
                "edit q",
                "stage p",
                "edit p",
                "stage p",
                "checkout hook.log"
            ],
            "q was written inside the first window with no action of its own there, so it is an \
             edit requested before that invocation's own stage"
        );
    }

    /// A path that is not a plain UTF-8 name cannot be requested without aliasing another one.
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"bad\xff"));
        assert!(translate(&Principal::from("a"), &[(path, vec![1])], &[]).is_err());
    }
}
