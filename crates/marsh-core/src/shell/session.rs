//! Source discovery, the process-wide live-shell registry and route admission, and one lazily
//! owned durable store with its shared policy authority per source.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use marsh_btrfs::{LibBtrfs, PersistenceLayer, Subvolumes};
use marsh_instrument::Tracing;
use marsh_lib::{CheckedAdvance, RecoverPoison as _};
use marsh_wal::{CommitOp, Seq};

use super::completion::{Completion, Finalize};
use super::execution::Run;
use super::policy::{Action, Event, PolicyValidator, Principal, Resource, resource_of};
use super::sandbox_policy::{CommandContext, SandboxPolicy};
use super::snapshot::{CommandNumber, Snapshot};
use super::{ShellError, ShellErrorKind};
use crate::shellmux::Sandbox;

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
/// Signalled on every membership, admission or domain-failure change.
static CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();
static FILESYSTEM: LazyLock<Arc<dyn Subvolumes>> = LazyLock::new(|| Arc::new(LibBtrfs));
static IDENTITIES: AtomicU64 = AtomicU64::new(1);

/// Creates an opaque owner unrelated to the shell's user-facing name.
pub(super) fn fresh_principal() -> Result<Principal, ShellError> {
    let next = IDENTITIES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| ShellError::infrastructure("shell identity exhaustion"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ShellError::infrastructure(error.to_string()))?
        .as_nanos();
    Ok(Principal::from(format!(
        "{:x}-{now:x}-{next:x}",
        std::process::id()
    )))
}

/// Serde operates directly on Junco's action, without a second runtime vocabulary.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(remote = "Action", rename_all = "lowercase")]
enum ActionSerde {
    Read,
    Edit,
    Stage,
    Unstage,
    Commit { message: Option<String> },
    Checkout,
    Stash,
    Delete,
    Clean,
    Diff,
    History,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct GrantedCapability {
    #[serde(with = "ActionSerde")]
    pub action: Action,
    #[serde(with = "resource_segments")]
    pub resource: Resource,
}
impl From<&Event> for GrantedCapability {
    fn from(event: &Event) -> Self {
        Self {
            action: event.action.clone(),
            resource: event.resource.clone(),
        }
    }
}
impl GrantedCapability {
    /// The seed-relative path this grant's resource names — only when that path names exactly
    /// this resource again, so a recorded resource is never reconciled as another one.
    fn path(&self) -> Option<PathBuf> {
        let path: PathBuf = self.resource.segments().iter().collect();
        (resource_of(&path).ok().flatten().as_ref() == Some(&self.resource)).then_some(path)
    }
}
mod resource_segments {
    use super::Resource;
    pub fn serialize<S: serde::Serializer>(
        resource: &Resource,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(resource.segments(), serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Resource, D::Error> {
        let segments: Vec<String> = serde::Deserialize::deserialize(deserializer)?;
        if segments.is_empty()
            || segments.iter().any(|segment| {
                let mut parts = std::path::Path::new(segment).components();
                !matches!(parts.next(), Some(std::path::Component::Normal(_)))
                    || parts.next().is_some()
            })
        {
            return Err(serde::de::Error::custom(
                "invalid durable resource components",
            ));
        }
        Ok(Resource::from(segments))
    }
}

mod principal_string {
    use super::Principal;
    pub fn serialize<S: serde::Serializer>(
        principal: &Principal,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if principal.as_str().is_empty() {
            return Err(serde::ser::Error::custom("durable principal is empty"));
        }
        serializer.serialize_str(principal.as_str())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Principal, D::Error> {
        let value: String = serde::Deserialize::deserialize(deserializer)?;
        if value.is_empty() {
            return Err(serde::de::Error::custom("durable principal is empty"));
        }
        Ok(Principal::from(value))
    }
}

/// Grants are required, including an explicitly empty list. Missing ownership never means empty.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublishMeta {
    pub cmd: String,
    #[serde(with = "principal_string")]
    pub principal: Principal,
    pub granted: Vec<GrantedCapability>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TreeVersion(u64);
impl CheckedAdvance for TreeVersion {
    type Output = Self;
    type Error = ShellError;
    fn value(&self) -> u64 {
        self.0
    }
    fn advance(self, value: u64) -> Self {
        Self(value)
    }
    fn exhausted() -> ShellError {
        ShellError::infrastructure("tree sequence exhaustion")
    }
}

pub(super) struct Authority {
    // Ledger ordering and physical-tree ordering are different domains.
    pub seq: Seq,
    pub tree_seq: TreeVersion,
    pub versions: HashMap<PathBuf, TreeVersion>,
    pub history: Vec<Event>,
    pub recovery_required: bool,
}
impl Default for Authority {
    fn default() -> Self {
        Self {
            seq: Seq::new(0),
            tree_seq: TreeVersion::default(),
            versions: HashMap::new(),
            history: Vec::new(),
            recovery_required: false,
        }
    }
}

/// Whether two coverage roots can name the same file.
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// What one admission holds, or would hold, over its coverage root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    Managed,
    Direct,
    Recovery,
}
impl Hold {
    /// Only managed work shares a coverage root with other managed work.
    fn excludes(self, other: Self) -> bool {
        !(self == Self::Managed && other == Self::Managed)
    }
}

struct Ticket {
    coverage: PathBuf,
    /// The validated verdict; none while one is being evaluated, which excludes like Direct.
    hold: Option<Hold>,
    active: bool,
}

/// Every live shell, source domain and admission of this process.
#[derive(Default)]
struct Registry {
    domains: BTreeMap<PathBuf, Weak<SourceDomain>>,
    /// Replaced copy-on-write on membership changes; evaluations keep the list they captured.
    sessions: Arc<Vec<Sandbox>>,
    /// Pending and active admissions, in ticket (arrival) order.
    tickets: BTreeMap<u64, Ticket>,
    next: u64,
}
impl Registry {
    /// Whether an active admission, or an earlier pending one, excludes `hold` over `coverage`.
    fn blocked(&self, ticket: u64, coverage: &Path, hold: Hold) -> bool {
        self.tickets.iter().any(|(other, entry)| {
            *other != ticket
                && (entry.active || *other < ticket)
                && overlaps(&entry.coverage, coverage)
                && entry.hold.unwrap_or(Hold::Direct).excludes(hold)
        })
    }
    fn live_domains(&self) -> Vec<Arc<SourceDomain>> {
        self.domains.values().filter_map(Weak::upgrade).collect()
    }
    /// Records a verdict: a grant activates it, a waiting verdict never downgrades a reservation.
    fn settle(&mut self, ticket: u64, hold: Hold, grant: bool) {
        if let Some(entry) = self.tickets.get_mut(&ticket)
            && (grant || !entry.active)
        {
            entry.hold = Some(hold);
            entry.active |= grant;
        }
    }
}

/// The route an admission selected; it stays fixed for the whole span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Managed,
    Direct,
}

/// One ticket in the admission queue; releasing it wakes every waiter.
pub(super) struct Admission {
    ticket: u64,
}
impl Admission {
    fn enqueue(coverage: &Path) -> Result<Completion<Self>, ShellError> {
        let mut registry = REGISTRY.lock().recover();
        let ticket = registry.next;
        registry.next = ticket
            .checked_add(1)
            .ok_or_else(|| ShellError::infrastructure("admission ticket exhaustion"))?;
        registry.tickets.insert(
            ticket,
            Ticket {
                coverage: coverage.to_path_buf(),
                hold: None,
                active: false,
            },
        );
        drop(registry);
        Ok(Completion::new(Self { ticket }))
    }
}
impl Finalize for Admission {
    fn finalize(&mut self, _completed: bool) {
        REGISTRY.lock().recover().tickets.remove(&self.ticket);
        CHANGED.notify_waiters();
    }
}

/// Where a new shell starts and which source it belongs to.
pub(super) struct Discovered {
    pub domain: Arc<SourceDomain>,
    /// The canonical Git work-tree root, or the canonical initial directory outside one.
    pub root: PathBuf,
    /// The canonical logical initial directory.
    pub cwd: PathBuf,
    /// The shallower of `root` and the storage seed: everything admission must exclude over.
    pub coverage: PathBuf,
}

/// One storage source: its shared authority, and durable storage opened only when needed.
pub(super) struct SourceDomain {
    /// The canonical seed, or the policy root when no storage is available.
    key: PathBuf,
    pub validator: Arc<PolicyValidator>,
    fs: Arc<dyn Subvolumes>,
    /// Seed and state root, or why this source has no managed storage.
    storage: Result<(PathBuf, PathBuf), Arc<ShellError>>,
    session: tokio::sync::OnceCell<Arc<Session>>,
    materialized: tokio::sync::OnceCell<()>,
    /// Advanced before every direct command over this source.
    epoch: Arc<AtomicU64>,
    /// Uncertain producer quiescence, with the coverage it leaves unusable.
    failure: Mutex<Option<(PathBuf, Arc<ShellError>)>>,
}
impl SourceDomain {
    /// Resolves `initial` to its logical directory, policy root and source domain. Creates nothing.
    pub fn discover(
        initial: &Path,
        backend: Option<Arc<dyn Subvolumes>>,
    ) -> Result<Discovered, ShellError> {
        let fs = backend.unwrap_or_else(|| Arc::clone(&FILESYSTEM));
        let initial = if initial.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            initial.to_path_buf()
        };
        let canonical = initial.canonicalize()?;
        if !canonical.is_dir() {
            return Err(ShellError::infrastructure(
                "working directory is not a directory",
            ));
        }
        let cwd = logical_source(&canonical);
        let root = policy_root(&cwd)?;
        let (storage, key) = match PersistenceLayer::discover(&cwd, fs.as_ref()) {
            Ok((layer, _)) => {
                let key = layer.seed.clone();
                (Ok((layer.seed, layer.root)), key)
            }
            Err(error) => (Err(Arc::new(ShellError::from(error))), root.clone()),
        };
        let coverage = match &storage {
            Ok((seed, _)) if seed.components().count() < root.components().count() => seed.clone(),
            _ => root.clone(),
        };
        let mut registry = REGISTRY.lock().recover();
        registry
            .domains
            .retain(|_, domain| domain.strong_count() != 0);
        let existing = registry.domains.get(&key).and_then(Weak::upgrade);
        let domain = match existing {
            Some(domain) if !Arc::ptr_eq(&domain.fs, &fs) => {
                drop(registry);
                return Err(ShellError::infrastructure(
                    "source already belongs to a different storage backend",
                ));
            }
            Some(domain) => domain,
            None => {
                let domain = Arc::new(Self {
                    key: key.clone(),
                    validator: Arc::new(PolicyValidator::new()),
                    fs,
                    storage,
                    session: tokio::sync::OnceCell::new(),
                    materialized: tokio::sync::OnceCell::new(),
                    epoch: Arc::new(AtomicU64::new(0)),
                    failure: Mutex::new(None),
                });
                registry.domains.insert(key, Arc::downgrade(&domain));
                domain
            }
        };
        drop(registry);
        Ok(Discovered {
            domain,
            root,
            cwd,
            coverage,
        })
    }

    /// The durable session, when this source's storage has been opened.
    pub fn session(&self) -> Option<&Arc<Session>> {
        self.session.get()
    }

    /// The durable session prepared for managed work: materialized, leased and recovered.
    pub async fn managed(&self) -> Result<Arc<Session>, ShellError> {
        self.materialized
            .get_or_try_init(|| async {
                let (seed, root) = self.storage()?;
                let tracing = Tracing::shared();
                let scope = tracing.internal_scope()?;
                let _guard = scope.enter();
                PersistenceLayer::new(seed.to_path_buf(), root.to_path_buf())
                    .materialize(self.fs.as_ref())?;
                Ok::<(), ShellError>(())
            })
            .await?;
        self.open().await.map(Arc::clone)
    }

    /// Records a failure that leaves `coverage` unusable until every owner has closed.
    pub fn fail(&self, coverage: &Path, message: impl Into<String>) {
        self.failure.lock().recover().get_or_insert_with(|| {
            (
                coverage.to_path_buf(),
                Arc::new(ShellError::infrastructure(message)),
            )
        });
    }

    fn storage(&self) -> Result<(&Path, &Path), ShellError> {
        match &self.storage {
            Ok((seed, root)) => Ok((seed, root)),
            Err(error) => Err(ShellError::caused(
                ShellErrorKind::Infrastructure,
                Arc::clone(error),
            )),
        }
    }

    /// Whether durable state from an earlier owner must be recovered before anyone may route.
    fn prior_state(&self) -> Result<bool, ShellError> {
        match &self.storage {
            Ok((_, root)) if self.session.get().is_none() => Ok(root.try_exists()?),
            _ => Ok(false),
        }
    }

    /// Refuses admission over `coverage` while recovery is required or producers are uncertain.
    fn check(&self, coverage: &Path) -> Result<(), ShellError> {
        if self.validator.read().recovery_required {
            return Err(ShellError::infrastructure(
                "source requires recovery before new commands",
            ));
        }
        if let Some((failed, error)) = &*self.failure.lock().recover()
            && overlaps(failed, coverage)
        {
            return Err(ShellError::caused(
                ShellErrorKind::Infrastructure,
                Arc::clone(error),
            ));
        }
        Ok(())
    }

    async fn open(&self) -> Result<&Arc<Session>, ShellError> {
        self.session
            .get_or_try_init(|| async { self.recover() })
            .await
    }

    /// Takes the lease and recovers durable authority into this domain's validator.
    fn recover(&self) -> Result<Arc<Session>, ShellError> {
        let (seed, root) = self.storage()?;
        let tracing = Tracing::shared();
        let scope = tracing.internal_scope()?;
        let _guard = scope.enter();
        let mut persistence = PersistenceLayer::new(seed.to_path_buf(), root.to_path_buf());
        persistence.acquire()?;
        let log = persistence.meta().join(marsh_wal::LOG_FILE);
        // A path deleted outside the log takes every grant over it along; the set of resources it
        // was is built once, the first time recovery finds one, and most startups find none.
        let mut gone: Option<BTreeSet<Resource>> = None;
        let recovered = marsh_wal::recover::<PublishMeta>(
            &persistence.seed,
            &persistence.snap(),
            &log,
            |meta, paths| paths.extend(meta.granted.iter().filter_map(GrantedCapability::path)),
            |meta, missing| {
                let gone = gone.get_or_insert_with(|| {
                    missing
                        .iter()
                        .filter_map(|path| resource_of(path).ok().flatten())
                        .collect()
                });
                meta.granted.retain(|grant| !gone.contains(&grant.resource));
                !meta.granted.is_empty()
            },
        )?;
        let mut authority = Authority::default();
        for transaction in recovered {
            authority.seq = authority.seq.max(transaction.seq);
            if !transaction.ops.is_empty() {
                authority.tree_seq = authority.tree_seq.next()?;
            }
            let meta = transaction.meta;
            authority.history.extend(
                meta.granted
                    .into_iter()
                    .map(|grant| Event::new(meta.principal.clone(), grant.action, grant.resource)),
            );
        }
        // Recovery validated every frame, or discarded an undecodable log whole, before any
        // resource can be swept.
        match std::fs::read_dir(persistence.snap()) {
            Ok(entries) => {
                for entry in entries {
                    self.fs.delete_subvolume(&entry?.path())?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        *self.validator.write()? = authority;
        Ok(Arc::new(Session {
            fs: Arc::clone(&self.fs),
            tracing,
            validator: Arc::clone(&self.validator),
            epoch: Arc::clone(&self.epoch),
            log,
            snapshots: Mutex::new(BTreeMap::new()),
            persistence,
        }))
    }
}

/// `canonical` with a live work view's prefix replaced by the source it snapshots.
fn logical_source(canonical: &Path) -> PathBuf {
    let domains = REGISTRY.lock().recover().live_domains();
    for session in domains.iter().filter_map(|domain| domain.session.get()) {
        let snapshots = session.snapshots.lock().recover();
        for (work, snapshot) in &*snapshots {
            if snapshot.strong_count() != 0
                && let Ok(relative) = canonical.strip_prefix(work)
            {
                return session.persistence.seed.join(relative);
            }
        }
    }
    canonical.to_path_buf()
}

/// The canonical work-tree root of the repository containing `directory`, or `directory` itself
/// outside a work tree.
fn policy_root(directory: &Path) -> Result<PathBuf, ShellError> {
    match git2::Repository::discover(directory) {
        Ok(repository) => match repository.workdir() {
            Some(workdir) if !repository.is_bare() => Ok(workdir.canonicalize()?),
            _ => Ok(directory.to_path_buf()),
        },
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(directory.to_path_buf()),
        Err(error) => Err(ShellError::infrastructure(format!(
            "repository discovery failed: {error}"
        ))),
    }
}

/// A shell's persistent execution resources and its live-membership registration.
pub(super) struct ExecutionResources {
    pub domain: Arc<SourceDomain>,
    pub principal: Principal,
    registered: bool,
    pub policy: SandboxPolicy,
    /// The managed view, while this shell is in one.
    pub snapshot: Option<Arc<Snapshot>>,
    pub coverage: PathBuf,
    serial: u64,
}
impl ExecutionResources {
    pub const fn new(
        domain: Arc<SourceDomain>,
        coverage: PathBuf,
        principal: Principal,
        policy: SandboxPolicy,
    ) -> Self {
        Self {
            domain,
            principal,
            registered: false,
            policy,
            snapshot: None,
            coverage,
            serial: 1,
        }
    }

    /// Adds this shell's record to the live set; dropping these resources removes it again.
    pub fn register(&mut self, record: Sandbox) {
        let mut registry = REGISTRY.lock().recover();
        Arc::make_mut(&mut registry.sessions).push(record);
        drop(registry);
        self.registered = true;
        CHANGED.notify_waiters();
    }

    /// The next command number; numbers are never reused, even across recreated views.
    pub fn number(&mut self) -> Result<CommandNumber, ShellError> {
        let number = self.serial;
        self.serial = number
            .checked_add(1)
            .ok_or_else(|| ShellError::infrastructure("command identity exhaustion"))?;
        Ok(CommandNumber(number))
    }

    /// Queues for a route over this shell's coverage, evaluates the policy against consistent
    /// inputs, and waits for overlapping conflicting work. Only the pure policy ever repeats.
    ///
    /// A call made from inside another live command returns Busy instead of waiting, so no command
    /// ever waits on work that waits on it.
    pub async fn admit(
        &self,
        command: &str,
        cancel: &tokio::sync::Notify,
        force: &AtomicBool,
        parent: Option<&Weak<Run>>,
    ) -> Result<(Completion<Admission>, Route), ShellError> {
        let nested = || {
            parent
                .and_then(Weak::upgrade)
                .is_some_and(|run| !run.closed.load(Ordering::Acquire))
        };
        let admission = Admission::enqueue(&self.coverage)?;
        let ticket = admission.ticket;
        let mut reserved = false;
        loop {
            let changed = CHANGED.notified();
            let cancelled = cancel.notified();
            tokio::pin!(changed, cancelled);
            changed.as_mut().enable();
            cancelled.as_mut().enable();
            if force.load(Ordering::Acquire) {
                return Err(ShellError::new(ShellErrorKind::Interrupted));
            }
            let domains = {
                let mut registry = REGISTRY.lock().recover();
                if let Some(entry) = registry.tickets.get_mut(&ticket)
                    && !entry.active
                {
                    entry.hold = None;
                }
                registry.live_domains()
            };
            for domain in &domains {
                if Arc::ptr_eq(domain, &self.domain) || overlaps(&domain.key, &self.coverage) {
                    domain.check(&self.coverage)?;
                }
            }
            drop(domains);
            if !reserved && self.domain.prior_state()? {
                if Self::reserve(ticket, &self.coverage) {
                    reserved = true;
                    self.domain.open().await?;
                    continue;
                }
            } else if let Some(route) = self.decide(ticket, command)? {
                return Ok((admission, route));
            }
            if nested() {
                return Err(ShellError::new(ShellErrorKind::Busy));
            }
            tokio::select! {
                () = &mut changed => {}
                () = &mut cancelled => return Err(ShellError::new(ShellErrorKind::Interrupted)),
            }
        }
    }

    /// Takes an exclusive recovery reservation over `coverage` when nothing excludes it.
    fn reserve(ticket: u64, coverage: &Path) -> bool {
        let mut registry = REGISTRY.lock().recover();
        let granted = !registry.blocked(ticket, coverage, Hold::Recovery);
        if granted {
            registry.settle(ticket, Hold::Recovery, true);
        }
        granted
    }

    /// Evaluates the policy once and grants its route when nothing excludes it. `Ok(None)` means
    /// the inputs changed or the route must wait; only a granted verdict leaves this function.
    fn decide(&self, ticket: u64, command: &str) -> Result<Option<Route>, ShellError> {
        let (sessions, revision) = {
            let registry = REGISTRY.lock().recover();
            (
                Arc::clone(&registry.sessions),
                self.domain.validator.revision(),
            )
        };
        let current = sessions
            .iter()
            .find(|record| record.uid == self.principal)
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        let managed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.policy.eval(&CommandContext {
                command,
                current,
                sessions: &sessions,
                validator: Arc::clone(&self.domain.validator),
            })
        }))
        .map_err(|_| ShellError::infrastructure("sandbox policy panicked"))?;
        let (route, hold) = if managed {
            (Route::Managed, Hold::Managed)
        } else {
            (Route::Direct, Hold::Direct)
        };
        let mut registry = REGISTRY.lock().recover();
        if !Arc::ptr_eq(&registry.sessions, &sessions)
            || self.domain.validator.revision() != revision
        {
            drop(registry);
            return Ok(None);
        }
        if registry.blocked(ticket, &self.coverage, hold) {
            registry.settle(ticket, hold, false);
            drop(registry);
            return Ok(None);
        }
        if route == Route::Direct {
            // Every managed view of this coverage must be retaken before it is used again.
            for domain in registry.live_domains() {
                if overlaps(&domain.key, &self.coverage) {
                    domain
                        .epoch
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                            epoch.checked_add(1)
                        })
                        .map_err(|_| ShellError::infrastructure("direct epoch exhaustion"))?;
                }
            }
        }
        registry.settle(ticket, hold, true);
        drop(registry);
        CHANGED.notify_waiters();
        Ok(Some(route))
    }
}
impl Drop for ExecutionResources {
    fn drop(&mut self) {
        if self.registered {
            let mut registry = REGISTRY.lock().recover();
            Arc::make_mut(&mut registry.sessions).retain(|record| record.uid != self.principal);
            drop(registry);
            CHANGED.notify_waiters();
        }
    }
}

/// Durable storage of one source: lease, log and private work views. It exists only once a
/// command needed it, and never holds live-shell membership.
pub(super) struct Session {
    pub fs: Arc<dyn Subvolumes>,
    pub tracing: Arc<Tracing>,
    pub validator: Arc<PolicyValidator>,
    pub epoch: Arc<AtomicU64>,
    pub log: PathBuf,
    pub snapshots: Mutex<BTreeMap<PathBuf, Weak<Snapshot>>>,
    // Lease is released after every other resource owned by this source.
    pub persistence: PersistenceLayer,
}

impl Session {
    pub fn check(&self) -> Result<(), ShellError> {
        if self.validator.read().recovery_required {
            Err(ShellError::infrastructure(
                "source requires recovery before new commands",
            ))
        } else {
            Ok(())
        }
    }

    /// Foreign writes never become the other shell's evidence. Invalidate its private view and
    /// refuse the originating run without taking an authority or policy lock in the receiver.
    pub fn invalidate_foreign(origin: &Path, paths: &[PathBuf]) -> bool {
        if paths.is_empty() {
            return false;
        }
        let domains = REGISTRY.lock().recover().live_domains();
        let mut foreign = false;
        for session in domains.iter().filter_map(|domain| domain.session.get()) {
            let snapshots = session.snapshots.lock().recover();
            for (root, snapshot) in &*snapshots {
                if root == origin || !paths.iter().any(|path| path.starts_with(root)) {
                    continue;
                }
                if let Some(snapshot) = snapshot.upgrade() {
                    let mut state = snapshot.state.lock().recover();
                    state.dirty = true;
                    if let Some((_, evidence)) = &mut state.evidence {
                        evidence.failure.get_or_insert_with(|| {
                            "work view was modified by another command".into()
                        });
                    }
                    drop(state);
                    foreign = true;
                }
            }
        }
        foreign
    }

    pub fn record_versions(
        authority: &mut Authority,
        operations: &[CommitOp],
    ) -> Result<(), ShellError> {
        if operations.is_empty() {
            return Ok(());
        }
        authority.tree_seq = authority.tree_seq.next()?;
        for operation in operations {
            authority
                .versions
                .insert(operation.path().to_path_buf(), authority.tree_seq);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Hold, Registry, Ticket, overlaps};
    use std::path::{Path, PathBuf};

    fn registry(tickets: &[(u64, &str, Option<Hold>, bool)]) -> Registry {
        let mut registry = Registry::default();
        for (ticket, coverage, hold, active) in tickets {
            registry.tickets.insert(
                *ticket,
                Ticket {
                    coverage: PathBuf::from(coverage),
                    hold: *hold,
                    active: *active,
                },
            );
        }
        registry
    }

    #[test]
    fn coverage_roots_overlap_by_ancestry_only() {
        assert!(overlaps(Path::new("/src"), Path::new("/src")));
        assert!(overlaps(Path::new("/src"), Path::new("/src/repo")));
        assert!(overlaps(Path::new("/src/repo"), Path::new("/src")));
        assert!(!overlaps(Path::new("/src/a"), Path::new("/src/b")));
        assert!(!overlaps(Path::new("/src"), Path::new("/srcx")));
    }

    #[test]
    fn only_managed_work_shares_an_overlapping_coverage() {
        let active = registry(&[(0, "/src", Some(Hold::Managed), true)]);
        assert!(!active.blocked(1, Path::new("/src/sub"), Hold::Managed));
        assert!(active.blocked(1, Path::new("/src/sub"), Hold::Direct));
        assert!(active.blocked(1, Path::new("/src"), Hold::Recovery));
        assert!(!active.blocked(1, Path::new("/other"), Hold::Direct));
        for exclusive in [Hold::Direct, Hold::Recovery] {
            let active = registry(&[(0, "/src", Some(exclusive), true)]);
            assert!(active.blocked(1, Path::new("/src"), Hold::Managed));
            assert!(!active.blocked(1, Path::new("/elsewhere"), Hold::Managed));
        }
    }

    #[test]
    fn earlier_pending_tickets_are_never_bypassed_by_conflicting_later_ones() {
        // An earlier ticket still evaluating excludes like Direct.
        let evaluating = registry(&[(0, "/src", None, false)]);
        assert!(evaluating.blocked(1, Path::new("/src"), Hold::Managed));
        // A later pending ticket never holds back an earlier one.
        let later = registry(&[(5, "/src", None, false)]);
        assert!(!later.blocked(1, Path::new("/src"), Hold::Direct));
        // A validated earlier managed verdict admits later managed work but not later raw work.
        let validated = registry(&[(0, "/src", Some(Hold::Managed), false)]);
        assert!(!validated.blocked(1, Path::new("/src"), Hold::Managed));
        assert!(validated.blocked(1, Path::new("/src"), Hold::Direct));
    }

    #[test]
    fn a_waiting_verdict_never_downgrades_a_held_reservation() {
        let mut held = registry(&[(0, "/src", Some(Hold::Recovery), true)]);
        held.settle(0, Hold::Managed, false);
        assert!(held.blocked(1, Path::new("/src"), Hold::Managed));
        held.settle(0, Hold::Managed, true);
        assert!(!held.blocked(1, Path::new("/src"), Hold::Managed));
    }
}
