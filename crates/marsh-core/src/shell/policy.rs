//! Stage 3: complete baseline diff, freshness, and an atomic ordered capability batch.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::builtins::gitcmd::GitAction;
use super::completion::{Completion, Finalize};
use super::execution::ExecutedCommand;
use super::session::{Authority, Session};
use super::{ShellError, ShellErrorKind};
pub(super) use junco_policy::{Action, Principal, Resource};
pub use junco_policy::{Bump, Event, PolicyDecision};
use marsh_lib::RecoverPoison as _;
use marsh_wal::CommitOp;

/// Receives every capability decision `Authority::check` makes, in order, before its effect.
/// A grant is tentative: a later denial in the same batch or a failed publication rolls it back.
/// Called under the authority's write lock, so it must only enqueue, never block.
pub struct PolicyObserver(Box<dyn Fn(&Event, &PolicyDecision) + Send + Sync>);
impl PolicyObserver {
    /// Wraps the callback each decision is passed to.
    pub fn new(observe: impl Fn(&Event, &PolicyDecision) + Send + Sync + 'static) -> Self {
        Self(Box::new(observe))
    }
    fn notify(&self, event: &Event, decision: &PolicyDecision) {
        (self.0)(event, decision);
    }
}

/// One refused capability, with the policy's explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// The rejected request.
    pub event: junco_policy::Event,
    /// The precondition that failed.
    pub failed_precondition: String,
    /// Policy-provided ways to make the request legal.
    pub allowed_fixes: Vec<String>,
}
impl std::fmt::Display for Denial {
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

/// Immutable authorization configuration selected per shell. Marsh calls [`Policy::evaluator`]
/// once per command batch and owns everything around it: observer notification, append-on-grant,
/// denial collection, rollback, freshness, and publication. The evaluator only decides.
pub trait Policy: Send + Sync + 'static {
    /// Fresh decision state for one batch; `arena` outlives it.
    fn evaluator<'batch>(
        &'batch self,
        arena: &'batch Bump,
    ) -> Box<dyn FnMut(&[Event], &Event) -> PolicyDecision + 'batch>;
}

/// The Git capability policy: a shell's default.
#[derive(Debug, Default)]
pub struct GitPolicy;
impl Policy for GitPolicy {
    fn evaluator<'batch>(
        &'batch self,
        arena: &'batch Bump,
    ) -> Box<dyn FnMut(&[Event], &Event) -> PolicyDecision + 'batch> {
        let mut evaluator = junco_policy::GitPolicy::new(arena);
        Box::new(move |history, event| evaluator.decide(history, event))
    }
}

/// Grants every capability. Snapshots, freshness, history, and publication still apply.
#[derive(Debug, Default)]
pub struct EmptyPolicy;
impl Policy for EmptyPolicy {
    fn evaluator<'batch>(
        &'batch self,
        _arena: &'batch Bump,
    ) -> Box<dyn FnMut(&[Event], &Event) -> PolicyDecision + 'batch> {
        Box::new(|_, _| PolicyDecision::Grant)
    }
}

/// One source's policy authority: its committed capability history and ledger ordering, shared by
/// every shell on that source and by routing predicates that query it.
pub struct PolicyValidator {
    authority: RwLock<Authority>,
    revision: AtomicU64,
}
impl PolicyValidator {
    /// An empty authority at the start of both orderings.
    pub(super) fn new() -> Self {
        Self {
            authority: RwLock::new(Authority::default()),
            revision: AtomicU64::new(0),
        }
    }
    /// What the Git policy would decide for `event` against the shared committed history, without
    /// adopting it. Resources name seed-relative paths, exactly as managed authorization does.
    /// This is a Git-policy query, not the selected shell's publication policy.
    pub fn decide(&self, event: &Event) -> Result<PolicyDecision, ShellError> {
        let authority = self.read();
        if authority.recovery_required {
            return Err(ShellError::infrastructure(
                "source requires recovery before new commands",
            ));
        }
        let arena = Bump::new();
        let mut policy = junco_policy::GitPolicy::new(&arena);
        let decision = policy.decide(&authority.history, event);
        drop(authority);
        Ok(decision)
    }
    pub(super) fn read(&self) -> RwLockReadGuard<'_, Authority> {
        self.authority.read().recover()
    }
    /// Exclusive authority for one mutation. The revision advances before the guard is returned,
    /// so a concurrent routing decision either sees the change or waits for its final history.
    pub(super) fn write(&self) -> Result<RwLockWriteGuard<'_, Authority>, ShellError> {
        let guard = self.authority.write().recover();
        self.revision
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |revision| {
                revision.checked_add(1)
            })
            .map_err(|_| ShellError::infrastructure("policy revision exhaustion"))?;
        Ok(guard)
    }
    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}
impl Authority {
    fn check(
        &mut self,
        shell_policy: &dyn Policy,
        events: &[Event],
        observer: Option<&PolicyObserver>,
    ) -> Result<(), Vec<Denial>> {
        let arena = Bump::new();
        let mut decide = shell_policy.evaluator(&arena);
        let checkpoint = self.history.len();
        let mut denials = Vec::new();
        for event in events {
            let decision = decide(&self.history, event);
            if let Some(observer) = observer {
                observer.notify(event, &decision);
            }
            match decision {
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
            self.history.truncate(checkpoint);
            Err(denials)
        }
    }
}

/// Only this payload can reach publication, holding the authority through intent and application;
/// its finalizer rolls back tentative policy history unless the command completed.
pub(super) struct AuthorizedCommand<'a> {
    pub guard: Option<RwLockWriteGuard<'a, Authority>>,
    pub executed: Option<ExecutedCommand>,
    pub operations: Vec<CommitOp>,
    pub events: Vec<Event>,
    checkpoint: usize,
}
impl Completion<AuthorizedCommand<'_>> {
    /// Finalizes now and releases the authority; the later drop finds no guard to roll back.
    pub fn release_authority(&mut self) {
        self.payload.finalize(self.completed);
        drop(self.payload.guard.take());
    }
}
impl Finalize for AuthorizedCommand<'_> {
    fn finalize(&mut self, completed: bool) {
        if !completed && let Some(guard) = &mut self.guard {
            guard.history.truncate(self.checkpoint);
        }
    }
}

pub(super) fn authorize<'session>(
    session: &'session Session,
    mut executed: ExecutedCommand,
    shell_policy: &dyn Policy,
    observer: Option<&PolicyObserver>,
) -> Result<Completion<AuthorizedCommand<'session>>, ShellError> {
    // Every refusal carries the native status the command already produced.
    let admitted = (|| -> Result<_, ShellError> {
        if let Some(failure) = executed.failure.take() {
            return Err(failure);
        }
        if executed.prepared.run.is_cancelled() {
            return Err(ShellError::new(ShellErrorKind::Interrupted));
        }
        if let Some(failure) = executed.evidence.failure.take() {
            return Err(ShellError::unsupported(failure));
        }
        let scope = session.tracing.internal_scope()?;
        let _scope = scope.enter();
        // The diff never depends on observed writes. Missing write evidence must fail coverage, not
        // disappear from the candidate transaction.
        let operations = marsh_wal::diff_trees(
            &executed.prepared.baseline,
            executed.prepared.snapshot.path(),
        )?;
        preflight(&executed, &operations)?;
        let mut guard = session.validator.write()?;
        if guard.recovery_required {
            return Err(ShellError::infrastructure("source requires recovery"));
        }
        let stale = stale_paths(&executed, &guard);
        if !stale.is_empty() {
            return Err(ShellError::new(ShellErrorKind::Stale {
                paths: stale
                    .into_iter()
                    .map(|path| session.persistence.seed.join(path))
                    .collect(),
            }));
        }
        let events = translate(&executed)?;
        let checkpoint = guard.history.len();
        guard
            .check(shell_policy, &events, observer)
            .map_err(|denials| ShellError::new(ShellErrorKind::Denied { denials }))?;
        Ok((guard, operations, events, checkpoint))
    })();
    match admitted {
        Ok((guard, operations, events, checkpoint)) => Ok(Completion::new(AuthorizedCommand {
            guard: Some(guard),
            executed: Some(executed),
            operations,
            events,
            checkpoint,
        })),
        Err(error) => Err(error.with_result(executed.result.take())),
    }
}

fn preflight(executed: &ExecutedCommand, operations: &[CommitOp]) -> Result<(), ShellError> {
    use std::os::unix::fs::MetadataExt;
    for operation in operations {
        let covered = executed.evidence.effects.iter().any(|(_, _, effect)| {
            effect.writes.iter().any(|path| path == operation.path())
                || effect
                    .recursive_writes
                    .iter()
                    .any(|path| operation.path().starts_with(path))
        });
        if !covered {
            return Err(ShellError::unsupported(format!(
                "unobserved filesystem change: {}",
                operation.path().display()
            )));
        }
        if let CommitOp::Write(path) = operation {
            let metadata = std::fs::symlink_metadata(executed.prepared.snapshot.path().join(path))?;
            if !metadata.is_file() && !metadata.is_symlink() {
                return Err(ShellError::unsupported(
                    "special-file publication is unsupported",
                ));
            }
            if metadata.is_file() && metadata.nlink() > 1 {
                return Err(ShellError::unsupported(
                    "changed hard-link payload is unsupported",
                ));
            }
        }
    }
    Ok(())
}

fn stale_paths(executed: &ExecutedCommand, authority: &Authority) -> Vec<PathBuf> {
    let mut stale = std::collections::BTreeSet::new();
    for (_, _, effects) in &executed.evidence.effects {
        for (changed, sequence) in &authority.versions {
            if *sequence <= executed.prepared.tree_seq {
                continue;
            }
            let exact = effects
                .dependencies
                .iter()
                .chain(&effects.reads)
                .chain(&effects.writes)
                .any(|path| path == changed || path.starts_with(changed));
            let recursive = effects
                .recursive_reads
                .iter()
                .chain(&effects.recursive_writes)
                .any(|path| changed.starts_with(path) || path.starts_with(changed));
            if exact || recursive {
                stale.insert(changed.clone());
            }
        }
    }
    stale.into_iter().collect()
}

fn translate(executed: &ExecutedCommand) -> Result<Vec<Event>, ShellError> {
    let principal = &executed.prepared.snapshot.uid;
    let mut ordered = Vec::new();
    for (order, builtin, effects) in &executed.evidence.effects {
        let semantic = builtin.and_then(|id| {
            executed
                .evidence
                .git
                .iter()
                .find(|git| git.invocation == id)
        });
        for path in &effects.reads {
            ordered.push((*order, Action::Read, path.as_path()));
        }
        for path in &effects.writes {
            let metadata = path
                .components()
                .any(|component| component.as_os_str() == ".git");
            if metadata {
                if semantic
                    .is_none_or(|git| !git.metadata.iter().any(|root| path.starts_with(root)))
                {
                    return Err(ShellError::unsupported(
                        "repository metadata changed outside a successful managed git invocation",
                    ));
                }
                continue;
            }
            if semantic.is_some_and(|git| {
                git.requests.iter().any(|(action, resource)| {
                    resource == path && capability_of(action.clone()).is_write()
                })
            }) {
                continue;
            }
            ordered.push((*order, Action::Edit, path.as_path()));
        }
    }
    for git in &executed.evidence.git {
        // An ordinary concurrent write inside an invocation's native interval has no causal
        // ordering relative to its semantic action, so it cannot be granted by inventing one.
        for (order, builtin, effects) in &executed.evidence.effects {
            if *order > git.started_order
                && *order < git.finished_order
                && *builtin != Some(git.invocation)
                && effects
                    .writes
                    .iter()
                    .any(|path| git.requests.iter().any(|(_, requested)| requested == path))
            {
                return Err(ShellError::unsupported(
                    "unordered git and ordinary writes overlap",
                ));
            }
        }
        for (action, path) in &git.requests {
            ordered.push((git.finished_order, capability_of(action.clone()), path));
        }
    }
    ordered.sort_by_key(|(order, _, _)| *order);
    let mut last = HashMap::new();
    let mut events = Vec::new();
    for (_, action, path) in ordered {
        let Some(resource) = resource_of(path)? else {
            continue;
        };
        if last.get(&resource) == Some(&action) {
            continue;
        }
        last.insert(resource.clone(), action.clone());
        events.push(Event::new(principal.clone(), action, resource));
    }
    Ok(events)
}

pub(super) fn capability_of(action: GitAction) -> Action {
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
pub(super) fn resource_of(path: &Path) -> Result<Option<Resource>, ShellError> {
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    let mut segments = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(ShellError::unsupported("non-relative policy resource"));
        };
        let component = component
            .to_str()
            .ok_or_else(|| ShellError::unsupported("non-UTF-8 policy resource"))?;
        if component == ".git" {
            return Ok(None);
        }
        segments.push(component.to_owned());
    }
    Ok(Some(Resource::from(segments)))
}
