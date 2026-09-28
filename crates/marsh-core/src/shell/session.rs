//! Canonical source ownership, exclusive recovery lease, and one private policy authority.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};

use marsh_btrfs::{LibBtrfs, PersistenceLayer, Subvolumes};
use marsh_instrument::Tracing;
use marsh_lib::{CheckedAdvance, RecoverPoison as _};
use marsh_wal::{CommitOp, Seq};

use super::ShellError;
use super::policy::{Action, Event, PolicyValidator, Principal, Resource, resource_of};

static SESSIONS: Mutex<BTreeMap<PathBuf, Weak<Session>>> = Mutex::new(BTreeMap::new());
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
    pub policy: PolicyValidator,
    pub recovery_required: bool,
}

pub(super) struct Session {
    pub fs: Arc<dyn Subvolumes>,
    pub tracing: Arc<Tracing>,
    pub authority: RwLock<Authority>,
    pub log: PathBuf,
    pub snapshots: Mutex<BTreeMap<PathBuf, Weak<super::snapshot::Snapshot>>>,
    // Lease is released after every other resource owned by this source.
    pub persistence: PersistenceLayer,
}

impl Session {
    /// Discovers logical cwd and atomically reuses or recovers the source's sole authority.
    pub fn open(
        initial: &Path,
        backend: Option<Arc<dyn Subvolumes>>,
    ) -> Result<(Arc<Self>, PathBuf), ShellError> {
        let fs = backend.unwrap_or_else(|| Arc::clone(&FILESYSTEM));
        let initial = if initial.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            initial.to_path_buf()
        };
        let mut registry = SESSIONS.lock().recover();
        registry.retain(|_, session| session.strong_count() != 0);
        let canonical = initial.canonicalize()?;
        if !canonical.is_dir() {
            return Err(ShellError::infrastructure(
                "working directory is not a directory",
            ));
        }
        let mut logical = canonical.clone();
        for session in registry.values().filter_map(Weak::upgrade) {
            let snapshots = session.snapshots.lock().recover();
            for (work, snapshot) in &*snapshots {
                if snapshot.strong_count() != 0
                    && let Ok(relative) = canonical.strip_prefix(work)
                {
                    logical = session.persistence.seed.join(relative);
                }
            }
        }
        let (mut persistence, cwd) = PersistenceLayer::discover(&logical, fs.as_ref())?;
        if let Some(session) = registry.get(&persistence.seed).and_then(Weak::upgrade) {
            if !Arc::ptr_eq(&session.fs, &fs) {
                return Err(ShellError::infrastructure(
                    "source already belongs to a different storage backend",
                ));
            }
            drop(registry);
            session.check()?;
            return Ok((session, cwd));
        }
        persistence.materialize(fs.as_ref())?;
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
        let mut seq = Seq::new(0);
        let mut tree_seq = TreeVersion::default();
        let mut policy = PolicyValidator::default();
        for transaction in recovered {
            seq = seq.max(transaction.seq);
            if !transaction.ops.is_empty() {
                tree_seq = tree_seq.next()?;
            }
            let meta = transaction.meta;
            policy.adopt(
                meta.granted
                    .into_iter()
                    .map(|grant| Event::new(meta.principal.clone(), grant.action, grant.resource)),
            );
        }
        // Recovery validated every frame, or discarded an undecodable log whole, before any
        // resource can be swept.
        for entry in std::fs::read_dir(persistence.snap())? {
            fs.delete_subvolume(&entry?.path())?;
        }
        let session = Arc::new(Self {
            fs,
            tracing: Tracing::shared(),
            log,
            authority: RwLock::new(Authority {
                seq,
                tree_seq,
                versions: HashMap::new(),
                policy,
                recovery_required: false,
            }),
            snapshots: Mutex::new(BTreeMap::new()),
            persistence,
        });
        registry.insert(session.persistence.seed.clone(), Arc::downgrade(&session));
        drop(registry);
        Ok((session, cwd))
    }

    pub fn check(&self) -> Result<(), ShellError> {
        if self.authority.read().recover().recovery_required {
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
        let registry = SESSIONS.lock().recover();
        let mut foreign = false;
        for session in registry.values().filter_map(Weak::upgrade) {
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
        drop(registry);
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
