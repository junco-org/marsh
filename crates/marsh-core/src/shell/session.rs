//! One seed and the snapshots hanging off it: the lease, the log, and the tree each shell runs in.
//!
//! A [`Session`] is opened once per process and shared by every clone of every executor over that
//! seed. It holds the seed's exclusive lease for its whole life, so a second session over the same
//! seed is refused rather than allowed to publish concurrently. Any number of [`Snapshot`]s hang
//! off it — one per attached shell, one per principal — and each serializes its boundaries through
//! the session's shared authority, so two principals publishing at once cannot interleave.
//!
//! The order at startup is the one a crash makes load-bearing: materialize the state directory,
//! recover the log, *then* sweep leftover snapshots — an unfinished transaction's content lives in
//! the snapshot the sweep would delete.
//!
//! Recovery is also where the capability history comes back. The policy judges a line against what
//! was granted before it, and that memory dies with the process while the tree it protects does
//! not — so the log records each transaction's grants and the snapshot id that earned them, and
//! [`Session::open`] hands them to the validator before any shell over the seed can run a line.
//! Ownership established by one process is therefore still in force in the next one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, RwLock, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use marsh_btrfs::{PersistenceLayer, Subvolumes, short_id};
use marsh_instrument::{BuiltinRecord, RecordingHook, SpawnRecord, SpawnRecorder, dump_records};
use marsh_wal::CommitOp;

use super::MarshError;
use super::policy::{Action, Event, PolicyValidator, Principal, Resource};

/// The id of one snapshot: the directory it lives in under `snap/`, and the identity every
/// capability published out of it is durably recorded against.
///
/// A job's *name* comes back. A pane index is reused, and a restarted daemon numbers its jobs from
/// one again — so a name is a fine identity for a live principal and a ruinous one for a durable
/// stake: the next holder of the name would inherit the last holder's unsettled work. A snapshot
/// id never comes back, which is why it, and not the name, is what a transaction records.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SnapshotUid(String);

impl SnapshotUid {
    /// The id as written on disk.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the empty id a detached executor reports.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// This id as a capability principal: who a recovered transaction's grants belong to.
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal::from(self.0.as_str())
    }
}

impl From<&str> for SnapshotUid {
    fn from(uid: &str) -> Self {
        Self(uid.to_string())
    }
}

impl From<String> for SnapshotUid {
    fn from(uid: String) -> Self {
        Self(uid)
    }
}

impl std::fmt::Display for SnapshotUid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The durable spelling of a policy [`Action`].
///
/// junco's own type carries no serde and is not this crate's to change, and a log line has to keep
/// its meaning whatever that crate does to its enum next, so the durable vocabulary is written out
/// here and converted at the boundary. A variant that stopped converting would fail to compile
/// rather than silently record a capability as something else.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantedAction {
    /// Observe a resource's contents.
    Read,
    /// Change a resource's contents.
    Edit,
    /// Stage a resource — the act that releases it to everyone else.
    Stage,
    /// Take a resource out of the staging area.
    Unstage,
    /// Commit a staged resource.
    Commit {
        /// The message, distinct from an absent one.
        message: Option<String>,
    },
    /// Restore a resource from the repository.
    Checkout,
    /// Stash a resource's local changes.
    Stash,
    /// Remove a resource and stage the removal.
    Delete,
    /// Discard untracked content at a resource.
    Clean,
    /// Observe a resource's differences.
    Diff,
    /// Observe a resource's history.
    History,
}

impl From<&Action> for GrantedAction {
    fn from(action: &Action) -> Self {
        match action {
            Action::Read => Self::Read,
            Action::Edit => Self::Edit,
            Action::Stage => Self::Stage,
            Action::Unstage => Self::Unstage,
            Action::Commit { message } => Self::Commit {
                message: message.clone(),
            },
            Action::Checkout => Self::Checkout,
            Action::Stash => Self::Stash,
            Action::Delete => Self::Delete,
            Action::Clean => Self::Clean,
            Action::Diff => Self::Diff,
            Action::History => Self::History,
        }
    }
}

impl From<GrantedAction> for Action {
    fn from(action: GrantedAction) -> Self {
        match action {
            GrantedAction::Read => Self::Read,
            GrantedAction::Edit => Self::Edit,
            GrantedAction::Stage => Self::Stage,
            GrantedAction::Unstage => Self::Unstage,
            GrantedAction::Commit { message } => Self::Commit { message },
            GrantedAction::Checkout => Self::Checkout,
            GrantedAction::Stash => Self::Stash,
            GrantedAction::Delete => Self::Delete,
            GrantedAction::Clean => Self::Clean,
            GrantedAction::Diff => Self::Diff,
            GrantedAction::History => Self::History,
        }
    }
}

/// One capability the policy granted for a transaction, as its `BEGIN` line records it.
///
/// This is the part of a grant the seed cannot re-derive. A resource is recoverable from the
/// transaction's own operations only when the grant was an [`Action::Edit`]; every git capability
/// is built from the builtin records, and those live beside the snapshot and go when it is swept.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrantedCapability {
    /// What was granted.
    pub action: GrantedAction,
    /// What it was granted over, as seed-relative path segments.
    #[serde(with = "resource_segments")]
    pub resource: Resource,
}

impl From<&Event> for GrantedCapability {
    fn from(event: &Event) -> Self {
        Self {
            action: GrantedAction::from(&event.action),
            resource: event.resource.clone(),
        }
    }
}

/// A [`Resource`] as the path segments it is made of, because junco's type carries no serde.
mod resource_segments {
    use super::Resource;

    /// Writes the segments, which is the whole of a resource's identity.
    pub(super) fn serialize<S: serde::Serializer>(
        resource: &Resource,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(resource.segments(), serializer)
    }

    /// Reads them back in order; segment count, order and text are all that equality compares.
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Resource, D::Error> {
        let segments: Vec<String> = serde::Deserialize::deserialize(deserializer)?;
        Ok(Resource::from(segments))
    }
}

/// What every publication records in its `BEGIN` line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishMeta {
    /// The command line whose boundary this transaction is; empty for a script's end and for the
    /// boundary at drop.
    pub cmd: String,
    /// Ids of the [`SpawnRecord`]s recorded since the previous boundary, published or discarded.
    pub spawns: Vec<u64>,
    /// Ids of the [`BuiltinRecord::Begin`]s recorded since the previous boundary, published or
    /// discarded.
    pub builtins: Vec<u64>,
    /// The durable identity that published it: the id of the snapshot its content came from.
    ///
    /// Defaulted so a log written before this field existed still parses. Such a transaction
    /// names nobody, and a reopen therefore grants its paths to nobody — which is exactly what
    /// that log says. It is not a reason to refuse the seed: an unparseable log is.
    #[serde(default)]
    pub principal: SnapshotUid,
    /// The capabilities the policy granted for it, in grant order.
    ///
    /// Defaulted for the same reason, and empty for the same meaning.
    #[serde(default)]
    pub granted: Vec<GrantedCapability>,
}

/// The outcome of one publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    /// The seed version after it: unchanged when nothing was published.
    pub seq: u64,
    /// How many filesystem operations the transaction carried.
    pub ops: usize,
}

/// A path another principal published after this line's snapshot was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StalePath {
    /// Seed-relative path.
    pub path: PathBuf,
    /// Sequence number of the transaction that won it.
    pub merged_seq: u64,
}

/// One seed, opened once per process: its lease, its log, and the authority every snapshot's
/// boundary serializes through.
pub(crate) struct Session {
    /// The btrfs operations, real or faked.
    fs: Arc<dyn Subvolumes>,
    /// `meta/wal.jsonl`.
    log: PathBuf,
    /// The one builtin hook every attached shell's builtins report to: instrumentation is
    /// process-global, and every attach re-installs this same hook.
    pub(crate) hook: Arc<RecordingHook>,
    /// Read = take or retake a snapshot; write = one boundary (diff, check, publish or discard).
    authority: RwLock<Authority>,
    /// The capabilities this seed's log says were granted before this process existed, in log
    /// order, each stamped with the snapshot id that published it.
    ///
    /// Built once, at open, from the very transactions recovery replays. Empty means the log says
    /// nothing was ever published — a genuinely unowned seed — which is a different thing from a
    /// log that could not be read: that one fails [`Self::open`] and yields no session at all.
    durable: Vec<Event>,
    /// The principals [`Self::durable`] names, for the one check a live name has to pass.
    durable_principals: HashSet<String>,
    /// Guards [`Self::adopt_into`] so one seed's durable history is installed exactly once.
    adopted: Once,
    /// Serial half of a snapshot uid, so two taken in one nanosecond differ.
    counter: AtomicU64,
    /// The seed, the state directory, and the lease over both.
    ///
    /// Declared last so it is dropped last: a [`Snapshot`]'s drop deletes its subvolume, and the
    /// lease must still be held while it does.
    pub(crate) persistence: PersistenceLayer,
}

/// The shared publication authority: what the seed is at, and who last wrote each path.
struct Authority {
    /// Highest transaction sequence number in the log.
    seq: u64,
    /// Seed-relative path → seq of the transaction that last wrote it, this process only.
    generations: HashMap<PathBuf, u64>,
    /// Set while an approved publication is being applied, and left set when it fails.
    ///
    /// A failed approved publication poisons the whole session, not only the snapshot that
    /// attempted it: the seed may be partially applied, and every other principal's next
    /// boundary would be diffing against a state nobody has verified. The flag lives under the
    /// authority lock because that is the lock a boundary already holds, so a gate can read it
    /// without a second acquisition and without a window.
    recovery_required: bool,
}

impl Session {
    /// Attaches to the seed containing `seed`: takes its lease, recovers its log, rebuilds the
    /// capability history that log describes, sweeps what a previous session left. Takes no
    /// snapshot; [`Self::snapshot`] does that, once per shell.
    ///
    /// The history is rebuilt from the same transactions recovery returns, so it costs no second
    /// read and cannot disagree with what was replayed. Rebuilding it is not new work: no
    /// transaction is opened, nothing is appended, and every capability in it was granted by the
    /// process that published it.
    ///
    /// # Errors
    ///
    /// Fails when no btrfs subvolume contains `seed`, when another process already holds the
    /// seed's lease, or when the log cannot be recovered. A seed with no log at all is not that
    /// case: it reads as empty, and an empty log genuinely owes nobody anything.
    pub(crate) fn open(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Arc<Self>, MarshError> {
        let mut persistence = PersistenceLayer::discover(seed, fs.as_ref())?;
        persistence.materialize(fs.as_ref())?;
        persistence.acquire()?;

        let log = persistence.meta().join(marsh_wal::LOG_FILE);
        // Before the sweep: an unfinished transaction's content is in the snapshot it reclaims.
        let recovered =
            marsh_wal::recover::<PublishMeta>(&persistence.seed, &persistence.snap(), &log)?;
        let mut seq = 0_u64;
        let mut durable: Vec<Event> = Vec::new();
        let mut durable_principals: HashSet<String> = HashSet::new();
        for transaction in recovered {
            // Before the skip below: a transaction that hands nobody a stake still occupies its
            // sequence, and a reopen that forgot it would reissue a number the log already holds.
            seq = seq.max(transaction.seq);
            let PublishMeta {
                principal, granted, ..
            } = transaction.meta;
            // A transaction from a log written before grants were recorded names nobody, so it
            // hands nobody a stake. That is what such a line says; it is not a reason to refuse.
            if principal.is_empty() || granted.is_empty() {
                continue;
            }
            for capability in granted {
                durable.push(Event::new(
                    principal.principal(),
                    Action::from(capability.action),
                    capability.resource,
                ));
            }
            durable_principals.insert(principal.0);
        }

        sweep_snapshots(&persistence.snap(), fs.as_ref())?;
        sweep_temporaries(&persistence.seed)?;

        Ok(Arc::new(Self {
            fs,
            log,
            hook: Arc::new(RecordingHook::default()),
            // Empty: every snapshot this process takes has `base_seq >= seq`, so no transaction
            // already in the log can be newer than a snapshot taken after it.
            authority: RwLock::new(Authority {
                seq,
                generations: HashMap::new(),
                // Recovery ran above: whatever the log still owed the seed has been replayed, so
                // a freshly opened session starts clean by construction.
                recovery_required: false,
            }),
            durable,
            durable_principals,
            adopted: Once::new(),
            counter: AtomicU64::new(0),
            persistence,
        }))
    }

    /// Installs this seed's durable capability history into `validator`, once.
    ///
    /// Called where an executor over this seed is bound to the history its lines will be judged
    /// against, which is before any of them can reach a gate. The [`Once`] is what makes "the
    /// history the log describes" mean the same thing to the tenth shell as to the first: a second
    /// installation would count every recovered grant twice.
    pub(crate) fn adopt_into(&self, validator: &Mutex<PolicyValidator>) {
        self.adopted.call_once(|| {
            validator
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .adopt(self.durable.iter().cloned());
        });
    }

    /// Takes a fresh snapshot of the seed for `principal`, or for the snapshot's own uid when the
    /// caller named nobody.
    ///
    /// A live name that happens to spell a recovered snapshot's id is disambiguated by this
    /// snapshot's own: the policy compares principals as strings, a job may be called anything,
    /// and a name chosen to match a durable owner would otherwise inherit that owner's unsettled
    /// stake. No recovered transaction can carry an id minted here, so the disambiguated form is
    /// unownable by anything already in the log.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be taken, canonicalized, or given a run directory.
    pub(crate) fn snapshot(
        self: &Arc<Self>,
        principal: Option<Principal>,
    ) -> Result<Arc<Snapshot>, MarshError> {
        let uid = SnapshotUid::from(short_id(&format!(
            "{}:{}:{}:{}",
            self.persistence.seed.display(),
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed),
            nanos_since_epoch()
        )));
        let principal = principal.unwrap_or_else(|| uid.principal());
        let principal = if self.durable_principals.contains(principal.as_str()) {
            Principal::from(format!("{principal}@{uid}"))
        } else {
            principal
        };

        let authority = self.authority.read().unwrap_or_else(PoisonError::into_inner);
        let path = self.persistence.work(uid.as_str());
        self.fs.snapshot(&self.persistence.seed, &path)?;
        let path = path.canonicalize()?;
        let base_seq = authority.seq;
        drop(authority);

        let run_dir = self.persistence.run_dir(uid.as_str())?;
        std::fs::create_dir_all(&run_dir)?;

        Ok(Arc::new(Snapshot {
            session: Arc::clone(self),
            uid,
            principal,
            path,
            run_dir,
            spawns: SpawnRecorder::default(),
            state: Mutex::new(SnapshotState {
                base_seq,
                spawns_seen: 0,
                builtins_seen: 0,
                recovery_required: false,
            }),
        }))
    }

    /// Whether an approved publication failed and its log still has to be replayed.
    ///
    /// Read under the authority's read lock, which is the same lock a boundary takes for writing:
    /// a gate that observes `false` here cannot be racing a failure that has already happened.
    pub(crate) fn recovery_required(&self) -> bool {
        self.authority
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .recovery_required
    }
}

/// One shell's snapshot of the seed, and the records of what ran in it.
pub(crate) struct Snapshot {
    /// The seed this is a snapshot of.
    session: Arc<Session>,
    /// This snapshot's id: the name of its directory under `snap/`, and the identity every
    /// capability it publishes is durably recorded against.
    uid: SnapshotUid,
    /// Who the shell running in it acts as while this process is alive.
    ///
    /// The name the caller gave, or this snapshot's own id when nobody named one. A denial names
    /// this, because it is what a reader can act on; the log records [`Self::uid`], because that
    /// is what still means the same thing after a restart.
    principal: Principal,
    /// Canonical `snap/<uid>`.
    path: PathBuf,
    /// `meta/runs/<uid>`: where the record streams are dumped.
    run_dir: PathBuf,
    /// This shell's own spawner records; the executor writes straight into here.
    pub(crate) spawns: SpawnRecorder,
    /// Base version and record attribution.
    state: Mutex<SnapshotState>,
}

/// The mutable half of a snapshot, behind one lock.
struct SnapshotState {
    /// Seed seq this snapshot equals; staleness is measured against it.
    base_seq: u64,
    /// Own spawn records already attributed to a boundary.
    spawns_seen: usize,
    /// Index into the session hook's shared stream already attributed to a boundary.
    builtins_seen: usize,
    /// Set before this snapshot's approved `marsh_wal::apply` and cleared only once the apply,
    /// the state update and the record dumps have all succeeded.
    ///
    /// While it is set this tree is the recovery source for a durable transaction that may be
    /// half applied. Its drop must therefore leave it on disk: deleting it would strand the log
    /// with no content to replay from.
    recovery_required: bool,
}

impl Snapshot {
    /// The tree the shell runs inside.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// This snapshot's id.
    pub(crate) const fn uid(&self) -> &SnapshotUid {
        &self.uid
    }

    /// Who the shell running in it acts as.
    pub(crate) const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The seed this is a snapshot of.
    pub(crate) const fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// The subset of `records` this snapshot's shell produced.
    ///
    /// The hook is shared by every shell in the process, so a shell's records are told apart by
    /// its logical working directory lying inside its own snapshot — where
    /// [`MarshExecutor::attach`](super::MarshExecutor::attach) starts it. A builtin run after a
    /// `cd` to an absolute path outside the snapshot is attributed to nobody, which costs nothing:
    /// [`policy::translate`](super::policy::translate) ignores such paths anyway.
    pub(crate) fn attributed(&self, records: &[BuiltinRecord]) -> Vec<BuiltinRecord> {
        let mine: HashSet<u64> = records
            .iter()
            .filter_map(|record| match record {
                BuiltinRecord::Begin { id, cwd, .. } if cwd.starts_with(&self.path) => Some(*id),
                _ => None,
            })
            .collect();
        records
            .iter()
            .filter(|record| match record {
                BuiltinRecord::Begin { id, .. } | BuiltinRecord::End { id, .. } => mine.contains(id),
            })
            .cloned()
            .collect()
    }

    /// Every builtin this snapshot's shell ran.
    pub(crate) fn builtin_records(&self) -> Vec<BuiltinRecord> {
        self.attributed(&self.session.hook.records())
    }

    /// Retakes the snapshot from the seed when another principal has published since it was taken.
    ///
    /// Under the authority's read lock, so no publication interleaves with the retake, and under
    /// this snapshot's own mutex, so two refreshes of one snapshot cannot race. A background job
    /// still writing into the old tree writes into a deleted one — the consequence
    /// [`Pending::discard`] already documents.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be retaken.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "both guards are held across the retake on purpose: the read lock keeps a \
                  publication from interleaving with it, the mutex keeps a second refresh out"
    )]
    pub(crate) fn refresh(&self) -> Result<(), MarshError> {
        let authority = self
            .session
            .authority
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.base_seq != authority.seq {
            self.session.fs.delete_subvolume(&self.path);
            self.session
                .fs
                .snapshot(&self.session.persistence.seed, &self.path)?;
            state.base_seq = authority.seq;
        }
        Ok(())
    }

    /// Takes stock at a command-line boundary: the records since the previous boundary (which it
    /// marks as seen) and the difference between the seed and this snapshot.
    ///
    /// `cmd` is the command line whose completion prompted it, or empty when nothing named it.
    /// Records that precede a boundary belong to it whether the caller then publishes or
    /// discards — the attempt happened either way. The session's authority is held for writing by
    /// the returned value until one of those two endings, so no other principal's boundary can
    /// interleave with this one.
    ///
    /// # Errors
    ///
    /// Fails when the trees cannot be compared.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guards travel in the returned `Pending` on purpose: attributing the \
                  records, diffing, and the ending that publishes or discards are one atomic \
                  boundary"
    )]
    pub(crate) fn pending(&self, cmd: &str) -> Result<Pending<'_>, MarshError> {
        let authority = self
            .session
            .authority
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let spawns = self.spawns.records();
        let all = self.session.hook.records();
        let new_spawns: Vec<u64> = spawns[state.spawns_seen..]
            .iter()
            .map(SpawnRecord::id)
            .collect();
        let boundary_builtins = self.attributed(&all[state.builtins_seen..]);
        let new_builtins: Vec<u64> = boundary_builtins
            .iter()
            .filter_map(|record| match record {
                BuiltinRecord::Begin { id, .. } => Some(*id),
                BuiltinRecord::End { .. } => None,
            })
            .collect();
        let whole_builtins = self.attributed(&all);
        state.spawns_seen = spawns.len();
        state.builtins_seen = all.len();

        let ops = marsh_wal::diff_trees(&self.session.persistence.seed, &self.path)?;
        Ok(Pending {
            snapshot: self,
            authority,
            state,
            cmd: cmd.to_string(),
            spawns,
            whole_builtins,
            boundary_builtins,
            new_spawns,
            new_builtins,
            ops,
        })
    }

    /// Writes both record streams into `meta/runs/<uid>`, whole.
    fn dump(&self, spawns: &[SpawnRecord], builtins: &[BuiltinRecord]) -> Result<(), MarshError> {
        std::fs::write(self.run_dir.join("spawns.json"), dump_records(spawns)?)?;
        std::fs::write(self.run_dir.join("builtins.json"), dump_records(builtins)?)?;
        Ok(())
    }
}

/// What a command line left since the last boundary, held under the session's authority until it
/// is published or discarded.
pub(crate) struct Pending<'a> {
    /// The snapshot this boundary belongs to.
    snapshot: &'a Snapshot,
    /// The seed's authority, held for the whole boundary so no other principal's interleaves.
    authority: RwLockWriteGuard<'a, Authority>,
    /// This snapshot's state, held for the same span.
    state: MutexGuard<'a, SnapshotState>,
    /// The command line that prompted the boundary; empty when nothing named it.
    cmd: String,
    /// Every spawn record of this shell, whole: the dumps are always whole.
    spawns: Vec<SpawnRecord>,
    /// Every builtin record of this shell, whole, for the same reason.
    whole_builtins: Vec<BuiltinRecord>,
    /// This boundary's builtin records only, which is what the translation reads.
    boundary_builtins: Vec<BuiltinRecord>,
    /// Ids of this boundary's spawn records, for the transaction's metadata.
    new_spawns: Vec<u64>,
    /// Ids of this boundary's builtin invocations, for the transaction's metadata.
    new_builtins: Vec<u64>,
    /// The seed-to-snapshot difference.
    ops: Vec<CommitOp>,
}

impl Pending<'_> {
    /// The difference this boundary found.
    pub(crate) fn ops(&self) -> &[CommitOp] {
        &self.ops
    }

    /// This boundary's builtin records only.
    pub(crate) fn builtins(&self) -> &[BuiltinRecord] {
        &self.boundary_builtins
    }

    /// The paths this boundary touches or requests that another principal published first.
    ///
    /// Empty means the line raced nobody and may be judged on its own merits.
    pub(crate) fn stale(&self, requested: &[Event]) -> Vec<StalePath> {
        let mut paths: Vec<PathBuf> = self
            .ops
            .iter()
            .map(|op| op.path().to_path_buf())
            .chain(
                requested
                    .iter()
                    .map(|event| event.resource.segments().iter().collect::<PathBuf>()),
            )
            .collect();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter_map(|path| {
                let merged_seq = *self.authority.generations.get(&path)?;
                (merged_seq > self.state.base_seq).then_some(StalePath { path, merged_seq })
            })
            .collect()
    }

    /// Applies the difference to the seed through the log, recording `granted` — the capabilities
    /// the policy allowed this boundary — in the transaction's `BEGIN` line.
    ///
    /// Recording them is what makes a grant outlive the process that earned it. The transaction's
    /// operations say which paths moved; they do not say what capability moved them, which
    /// principal holds the result, or that a `git add` released a path it never rewrote. Only the
    /// validator knows that, and only at this moment, so it is written down here.
    ///
    /// A boundary that found no difference writes no transaction; it still refreshes the record
    /// dumps, which is the only way a command that changed nothing shows up on disk at all. Its
    /// grants are not recorded either, and cannot matter: an action that changes a resource's
    /// state — an edit, a stage, an unstage, a delete, a commit, a checkout, a stash — moves bytes
    /// in the tree or in `.git/`, so it is never what an empty difference was granted for. What is
    /// left is `read`, `diff`, `history` and `clean`, and the rule table treats every one of them
    /// as state-preserving. Either way the snapshot equals the seed afterwards, so the next line
    /// starts unstale.
    ///
    /// # Errors
    ///
    /// Fails when the transaction cannot be logged or applied, or when a record stream cannot be
    /// written.
    pub(crate) fn publish(mut self, granted: &[Event]) -> Result<Publication, MarshError> {
        if self.ops.is_empty() {
            self.snapshot.dump(&self.spawns, &self.whole_builtins)?;
            self.state.base_seq = self.authority.seq;
            return Ok(Publication {
                seq: self.authority.seq,
                ops: 0,
            });
        }
        // Armed *before* the durable apply and cleared only once every step of it succeeded. The
        // window this covers is the one that matters: `marsh_wal::apply` writes the log and then
        // moves content into the seed, so a failure partway through leaves a seed that is neither
        // the old one nor the new one. Nothing may be published on top of that, and this
        // snapshot's tree must survive as the content the log replays from.
        self.state.recovery_required = true;
        self.authority.recovery_required = true;

        let seq = self.authority.seq + 1;
        let meta = PublishMeta {
            cmd: self.cmd,
            spawns: self.new_spawns,
            builtins: self.new_builtins,
            principal: self.snapshot.uid.clone(),
            granted: granted.iter().map(GrantedCapability::from).collect(),
        };
        marsh_wal::apply(
            &self.snapshot.session.persistence.seed,
            &self.snapshot.path,
            &self.snapshot.session.log,
            self.snapshot.uid.as_str(),
            seq,
            &meta,
            &self.ops,
        )?;
        self.authority.seq = seq;
        for op in &self.ops {
            self.authority
                .generations
                .insert(op.path().to_path_buf(), seq);
        }
        self.state.base_seq = seq;
        self.snapshot.dump(&self.spawns, &self.whole_builtins)?;
        self.state.recovery_required = false;
        self.authority.recovery_required = false;
        Ok(Publication {
            seq,
            ops: self.ops.len(),
        })
    }

    /// Abandons the boundary without publishing and without retaking the snapshot.
    ///
    /// The records are still dumped — the attempt happened — but nothing is translated, checked or
    /// applied, and the tree is left exactly as it is. For the one caller that is about to delete
    /// the tree anyway: retaking a snapshot immediately before reclaiming it is two subvolume
    /// operations to produce a directory nobody will ever look at.
    ///
    /// # Errors
    ///
    /// Fails when a record stream cannot be written.
    pub(crate) fn abandon(self) -> Result<(), MarshError> {
        self.snapshot.dump(&self.spawns, &self.whole_builtins)
    }

    /// Throws the line away: dumps the records — the attempt happened — then, when anything
    /// differed, deletes the snapshot and retakes it from the seed at the same path, so the tree
    /// the shell runs in is the seed's again.
    ///
    /// A background job still writing into the old snapshot writes into a deleted tree, which is
    /// what a per-line snapshot refresh means.
    ///
    /// # Errors
    ///
    /// Fails when a record stream cannot be written or the snapshot cannot be retaken.
    pub(crate) fn discard(mut self) -> Result<(), MarshError> {
        self.snapshot.dump(&self.spawns, &self.whole_builtins)?;
        if !self.ops.is_empty() {
            self.snapshot
                .session
                .fs
                .delete_subvolume(&self.snapshot.path);
            self.snapshot
                .session
                .fs
                .snapshot(&self.snapshot.session.persistence.seed, &self.snapshot.path)?;
        }
        self.state.base_seq = self.authority.seq;
        Ok(())
    }
}

impl Drop for Snapshot {
    /// Reclaims the snapshot without publishing anything.
    ///
    /// Destruction is not a boundary. Whatever is still staged here was never translated, never
    /// checked against the policy and never granted, and publishing it on the way out would be a
    /// grant nobody made — reachable from an abort, a failed launch or a panic, none of which a
    /// capability decision may depend on. [`Shell::gate`](super::Shell) after
    /// [`PolicyValidator::check`](super::PolicyValidator) is the only path to a publication.
    ///
    /// The records are still dumped, because the attempt happened.
    ///
    /// The one tree that survives is the one whose *approved* publication failed: it is the
    /// content the next session's recovery replays from, and deleting it would strand the log.
    fn drop(&mut self) {
        let retained = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recovery_required;
        if retained {
            return;
        }
        let _ = self.pending("").and_then(Pending::abandon);
        self.session.fs.delete_subvolume(&self.path);
    }
}

/// Deletes every snapshot left by a previous session.
///
/// Everything under `snap` is a snapshot by construction, so no name filtering. Called only after
/// recovery has consumed whatever content those trees carried.
fn sweep_snapshots(snap: &Path, fs: &dyn Subvolumes) -> Result<(), MarshError> {
    if !snap.exists() {
        std::fs::create_dir_all(snap)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(snap)? {
        let path = entry?.path();
        if path.is_dir() {
            fs.delete_subvolume(&path);
        }
    }
    Ok(())
}

/// Removes the temporaries a crash may have left mid-transaction.
///
/// They are unreferenced by definition: a transaction either renamed its temporary into place or
/// never completed.
fn sweep_temporaries(root: &Path) -> Result<(), MarshError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| {
                name.as_encoded_bytes()
                    .ends_with(marsh_wal::TEMPORARY_SUFFIX.as_bytes())
            }) {
                std::fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

/// Nanoseconds since the epoch, or 0 on a clock that predates it.
///
/// One of four inputs to a snapshot id; a degenerate clock costs uniqueness, never correctness,
/// because the pid, the serial and the seed path are in the digest too.
fn nanos_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}
