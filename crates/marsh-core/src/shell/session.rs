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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, RwLock, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use marsh_btrfs::{PersistenceLayer, Subvolumes, short_id};
use marsh_instrument::{
    BuiltinRecord, RecordingHook, SpawnRecord, SpawnRecorder, TraceLine, dump_records,
};
use marsh_wal::CommitOp;

use super::MarshError;
use super::access::Access;
use super::builtins::gitcmd::GitAction;
use super::policy::{Action, Event, PolicyValidator, Principal, Resource, durable_principal};

/// The id of one snapshot: its directory under `snap/` and the default durable owner of its
/// published capabilities.
///
/// Reusable job names never inherit a previous snapshot's stake. An embedding caller can
/// explicitly opt a stable [`crate::shellmux::ShellId::durable`] identity into WAL recovery
/// instead; the snapshot uid still identifies its content.
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
/// comes from what the `git` builtin observed its invocations do, which lives only as long as the
/// line it happened in.
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
    /// The snapshot that published this transaction, and its owner unless explicitly overridden.
    ///
    /// Defaulted so a log written before this field existed still parses. Such a transaction
    /// names nobody, and a reopen therefore grants its paths to nobody — which is exactly what
    /// that log says. A log that cannot even parse into records of this shape is not refused
    /// either: reading it resets it to empty, and startup proceeds over the fresh log that leaves.
    #[serde(default)]
    pub principal: SnapshotUid,
    /// The stable agent name explicitly opted into durable ownership, if any.
    ///
    /// Absent in legacy logs and ordinary jobs. Recovery scopes this name separately from
    /// reusable names and snapshot ids; it never infers durable authority from a job label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_principal: Option<String>,
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

/// One seed, opened once per process: its lease, its log, and the authority every snapshot's
/// boundary serializes through.
pub(crate) struct Session {
    /// The btrfs operations, real or faked.
    fs: Arc<dyn Subvolumes>,
    /// `meta/wal.jsonl`.
    log: PathBuf,
    /// The builtin hook every attached shell's builtins report to: instrumentation is
    /// process-global, and every attach re-installs this same hook.
    ///
    /// Supplied at open rather than made here, because a host running shells over several seeds
    /// has to give all of them one recorder — the global installation keeps only the last hook
    /// installed, so per-session recorders would leave every seed but the newest unobserved.
    /// Records are told apart by the snapshot path they were made in, not by which session owns
    /// the recorder.
    pub(crate) hook: Arc<RecordingHook>,
    /// Read = take or retake a snapshot; write = one boundary (diff, check, publish or discard).
    authority: RwLock<Authority>,
    /// The capabilities granted by this seed's log, in log order, stamped with their snapshot
    /// owner or explicitly opted-in durable agent identity.
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
    /// Opens the seed an already-discovered `persistence` names: takes its lease, recovers its
    /// log, rebuilds the capability history that log describes, sweeps what a previous session
    /// left. Takes no snapshot; [`Self::snapshot`] does that, once per shell.
    ///
    /// Discovery is the caller's, because one mux hosts shells over several seeds and each of them
    /// is found from its own shell's starting directory. `hook` is supplied for the same reason:
    /// instrumentation is installed process-wide, so every session a single mux opens must report
    /// to one recorder or the last seed opened would silence the others' builtins.
    ///
    /// The history is rebuilt from the same transactions recovery returns, so it costs no second
    /// read and cannot disagree with what was replayed. Rebuilding it is not new work: no
    /// transaction is opened, nothing is appended, and every capability in it was granted by the
    /// process that published it.
    ///
    /// # Errors
    ///
    /// Fails when the state directory cannot be materialized, when another process already holds
    /// the seed's lease, or when the log cannot be recovered. A seed with no log at all is not
    /// that case: it reads as empty, and an empty log genuinely owes nobody anything.
    pub(crate) fn open(
        mut persistence: PersistenceLayer,
        fs: Arc<dyn Subvolumes>,
        hook: Arc<RecordingHook>,
    ) -> Result<Arc<Self>, MarshError> {
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
                principal,
                durable_principal: stable_name,
                granted,
                ..
            } = transaction.meta;
            // A transaction from a log written before grants were recorded names nobody, so it
            // hands nobody a stake. That is what such a line says; it is not a reason to refuse.
            if granted.is_empty() || (stable_name.is_none() && principal.is_empty()) {
                continue;
            }
            let owner = stable_name
                .as_deref()
                .map_or_else(|| principal.principal(), durable_principal);
            for capability in granted {
                durable.push(Event::new(
                    owner.clone(),
                    Action::from(capability.action),
                    capability.resource,
                ));
            }
            durable_principals.insert(owner.as_str().to_owned());
        }

        sweep_snapshots(&persistence.snap(), fs.as_ref())?;
        sweep_temporaries(&persistence.seed)?;

        Ok(Arc::new(Self {
            fs,
            log,
            hook,
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
    /// `durable_name` is supplied only by an explicitly durable mux identity, whose already
    /// scoped policy principal may resume the same grants after recovery.
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
        durable_name: Option<Principal>,
    ) -> Result<Arc<Snapshot>, MarshError> {
        let uid = SnapshotUid::from(short_id(&format!(
            "{}:{}:{}:{}",
            self.persistence.seed.display(),
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed),
            nanos_since_epoch()
        )));
        let principal = principal.unwrap_or_else(|| uid.principal());
        let principal =
            if durable_name.is_none() && self.durable_principals.contains(principal.as_str()) {
                Principal::from(format!("{principal}@{uid}"))
            } else {
                principal
            };

        let authority = self
            .authority
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let path = self.persistence.work(uid.as_str());
        let run_dir = self.internal(|| {
            self.fs.snapshot(&self.persistence.seed, &path)?;
            let run_dir = self.persistence.run_dir(uid.as_str())?;
            std::fs::create_dir_all(&run_dir)?;
            Ok::<_, MarshError>(run_dir)
        })?;
        let path = path.canonicalize()?;
        let base_seq = authority.seq;
        drop(authority);

        let snapshot = Arc::new(Snapshot {
            session: Arc::clone(self),
            uid,
            principal,
            durable_name,
            path,
            run_dir,
            spawns: SpawnRecorder::default(),
            interrupted: Arc::new(AtomicBool::new(false)),
            resume: Arc::new(tokio::sync::Notify::new()),
            state: Mutex::new(SnapshotState {
                base_seq,
                spawns_seen: 0,
                builtins_seen: 0,
                traces: Vec::new(),
                traces_seen: 0,
                reads: BTreeSet::new(),
                writes: BTreeMap::new(),
                recursive_reads: BTreeSet::new(),
                recursive_writes: BTreeMap::new(),
                access: Access::default(),
                git: GitLine::default(),
                dependency: None,
                recovery_required: false,
            }),
        });

        // After the tree exists and before any shell can run in it: from here on every file
        // access inside this root is classified into the snapshot's own read and write sets. The
        // observer holds a *weak* handle — the hook would otherwise keep every snapshot ever
        // taken alive, and it is the snapshot's drop that unregisters it.
        let observer = Arc::downgrade(&snapshot);
        snapshot.session.hook.register_root(
            snapshot.path(),
            Arc::clone(&snapshot.interrupted),
            Arc::new(move |line: &TraceLine| match observer.upgrade() {
                None => Ok(()),
                Some(snapshot) => snapshot.observe(line),
            }),
        )?;
        Ok(snapshot)
    }

    /// Runs `work` with every syscall it makes marked as the implementation's own.
    ///
    /// Not bookkeeping: a tree diff walks the snapshot by name, a subvolume copy writes back into
    /// it by name, and a record dump opens files beside it. Every one of those is a traced access
    /// to a path inside a registered root, so a boundary that did not say "this is mine" would
    /// observe itself reading the whole tree and writing back everything it restored — and would
    /// then find its own footprint invalidated by the next principal's publication.
    fn internal<T>(&self, work: impl FnOnce() -> T) -> T {
        let scope = self.hook.scope(None);
        let guard = scope.as_ref().map(marsh_instrument::TraceScope::enter);
        let done = work();
        drop(guard);
        done
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
    /// This snapshot's id and the default durable owner of its publications.
    uid: SnapshotUid,
    /// The scoped policy principal used by the running shell. Ordinary names become snapshot
    /// owners in the WAL; explicitly durable identities retain this same principal on recovery.
    principal: Principal,
    /// The stable caller-owned agent name, only when durable ownership was explicitly selected.
    durable_name: Option<Principal>,
    /// Canonical `snap/<uid>`.
    path: PathBuf,
    /// `meta/runs/<uid>`: where the record streams are dumped.
    run_dir: PathBuf,
    /// This shell's own spawner records; the executor writes straight into here.
    pub(crate) spawns: SpawnRecorder,
    /// Set when this evaluation read something another principal has since published.
    ///
    /// Shared with the instrumentation, which is what lets a builtin about to run — and Brush's
    /// own interactive driver — learn that the line has to be evaluated again without this
    /// snapshot's lock being involved at all.
    pub(crate) interrupted: Arc<AtomicBool>,
    /// Notified when [`Self::interrupted`] is set, so the run loop can signal this evaluation's
    /// own processes without polling for it.
    pub(crate) resume: Arc<tokio::sync::Notify>,
    /// Base version, record attribution and this line's observed footprint.
    state: Mutex<SnapshotState>,
}

/// The mutable half of a snapshot, behind one lock.
struct SnapshotState {
    /// Seed seq this snapshot equals; a read dependency is measured against it.
    base_seq: u64,
    /// Own spawn records already attributed to a boundary.
    spawns_seen: usize,
    /// Index into the session hook's shared stream already attributed to a boundary.
    builtins_seen: usize,
    /// Every trace line attributed to this snapshot, as the tracer printed it.
    traces: Vec<String>,
    /// How much of [`Self::traces`] a boundary has already accounted for.
    traces_seen: usize,
    /// Seed-relative paths this evaluation observed the content or existence of.
    reads: BTreeSet<PathBuf>,
    /// Seed-relative paths this evaluation changed, each with the trace timestamps of the calls
    /// that changed it: the clock a git invocation's window is measured on too.
    writes: BTreeMap<PathBuf, Vec<u64>>,
    /// Subtrees whose contents this evaluation depended on.
    recursive_reads: BTreeSet<PathBuf>,
    /// Subtrees this evaluation restructured, with the timestamps of the calls that did.
    recursive_writes: BTreeMap<PathBuf, Vec<u64>>,
    /// Where each traced thread resolves relative paths from, and what it has mapped.
    access: Access,
    /// What this evaluation's managed git invocations did, and which are still running.
    git: GitLine,
    /// The footprint's read dependency as last decided in full, and under which seqs.
    ///
    /// Within one pair of seqs only this evaluation's own new reads can change it: generations
    /// move only with the authority's seq, and a footprint only grows until it is reset. So a
    /// traced call checks only what it added, and the whole footprint is walked again only after a
    /// publication or a rebase — not once per call, which for a command reading thousands of
    /// paths is quadratic and leaves the tracer's drains waiting on this classification.
    dependency: Option<Dependency>,
    /// Set before this snapshot's approved `marsh_wal::apply` and cleared only once the apply,
    /// the state update and the record dumps have all succeeded.
    ///
    /// While it is set this tree is the recovery source for a durable transaction that may be
    /// half applied. Its drop must therefore leave it on disk: deleting it would strand the log
    /// with no content to replay from.
    recovery_required: bool,
}

/// A read dependency decided over a whole footprint.
#[derive(Debug, Clone, Copy)]
struct Dependency {
    /// The authority's seq when it was decided.
    seq: u64,
    /// The snapshot's `base_seq` when it was decided.
    base_seq: u64,
    /// Whether the footprint read something published after `base_seq`.
    newer: bool,
}

/// What one git invocation did to the resources of this snapshot, in the git vocabulary.
///
/// The window is two readings of the trace's own clock, taken around the native process, so a
/// traced write can be placed before, inside or after it. Nothing here is a policy action yet:
/// the gate maps each recorded action when it builds the line's requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitEffectRecord {
    /// `CLOCK_REALTIME` microseconds just before the process started.
    pub(crate) started_at: u64,
    /// The same clock, once the process had been reaped.
    pub(crate) finished_at: u64,
    /// Each observed transition, in the order it is requested, at its snapshot-relative path.
    pub(crate) requests: Vec<(GitAction, PathBuf)>,
    /// Snapshot-relative repository metadata the invocation used — a git directory, a common
    /// directory — whose writes inside its window are git's bookkeeping, never edits.
    pub(crate) metadata: Vec<PathBuf>,
}

/// How a managed git invocation shares the snapshot with the others running in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitCohortKind {
    /// Observes only: any number run together.
    Inspect,
    /// May change state: its effects are attributed by comparing before with after, so nothing
    /// else may run in the snapshot meanwhile.
    Exclusive,
}

/// The managed git invocations of one evaluation.
///
/// Heterogeneous on purpose: the recorded effects, the first failure, and the running members
/// with the processes they own are one lifecycle, reset together at every boundary.
#[derive(Default)]
struct GitLine {
    /// Which evaluation the fields below describe; a guard from an earlier one touches nothing.
    generation: u64,
    /// Id of the next guard.
    next_id: u64,
    /// Guards alive in any generation: a process an earlier boundary cancelled may still be
    /// exiting, and the tree may not be reused under it until it has.
    live: usize,
    /// The kind of the invocations running now, when any are.
    cohort: Option<GitCohortKind>,
    /// How many of this evaluation's invocations are running.
    members: usize,
    /// The native process each running invocation owns, by guard id: `(pid, process group)`.
    children: BTreeMap<u64, (i32, Option<i32>)>,
    /// Completed invocations' effects, in completion order.
    records: Vec<GitEffectRecord>,
    /// The first reason this evaluation's git effects cannot be published.
    failure: Option<String>,
}

impl GitLine {
    /// Latches `message` unless an earlier failure already explains the line.
    fn fail(&mut self, message: String) {
        self.failure.get_or_insert(message);
    }

    /// Forgets this evaluation, leaving every guard still alive unable to touch the next one.
    fn reset(&mut self) {
        self.generation += 1;
        self.cohort = None;
        self.members = 0;
        self.children.clear();
        self.records.clear();
        self.failure = None;
    }

    /// Kills every process a running invocation owns and latches why the line cannot stand.
    ///
    /// A boundary that found git still running cannot know what it will have done, so neither
    /// publishing nor retaking the tree under it is honest; the line fails and the processes are
    /// ended rather than left to write into a tree nobody will look at.
    fn cancel_running(&mut self) {
        if self.members == 0 {
            return;
        }
        for (pid, group) in self.children.values() {
            end_process(*pid, *group);
        }
        self.fail("git: a managed git was still running at the line boundary".to_string());
    }
}

/// Kills a native process this shell started, with the process group it leads, and resumes it
/// in case it was stopped so the kill is delivered at once.
///
/// A process that joined this process's own group is signalled alone: that group is the host's,
/// not the child's.
pub(crate) fn end_process(pid: i32, group: Option<i32>) {
    // SAFETY: `getpgrp` takes no arguments and cannot fail.
    let own_group = unsafe { libc::getpgrp() };
    let target = match group {
        Some(group) if group != own_group => -group,
        _ => pid,
    };
    // SAFETY: `kill` has no memory effects; a target that already exited is ESRCH.
    unsafe { libc::kill(target, libc::SIGKILL) };
    // SAFETY: as above.
    unsafe { libc::kill(target, libc::SIGCONT) };
}

/// A managed git invocation's membership of its snapshot's cohort, released when dropped.
///
/// Its observation ends with [`Self::record`] or [`Self::finish`]. A guard dropped before either
/// — a future cut short between the process and its probes — leaves effects nobody attributed,
/// so the line it belonged to fails rather than publishing them.
pub(crate) struct GitGuard {
    /// The snapshot it runs in.
    snapshot: Arc<Snapshot>,
    /// The evaluation it was admitted to.
    generation: u64,
    /// Its id within the snapshot.
    id: u64,
    /// Whether its observation reached an end.
    completed: bool,
}

impl GitGuard {
    /// Runs `update` over the line state, when this guard's evaluation is still the current one.
    fn current(&self, update: impl FnOnce(&mut GitLine)) {
        let mut state = self
            .snapshot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.git.generation == self.generation {
            update(&mut state.git);
        }
    }

    /// Registers the native process this invocation started, so a boundary that finds it still
    /// running can end it.
    pub(crate) fn spawned(&self, pid: i32, group: Option<i32>) {
        self.current(|line| {
            line.children.insert(self.id, (pid, group));
        });
    }

    /// Records what this invocation did, ending its observation.
    pub(crate) fn record(mut self, record: GitEffectRecord) {
        self.current(|line| line.records.push(record));
        self.completed = true;
    }

    /// Ends an observation that had nothing to record.
    pub(crate) fn finish(mut self) {
        self.completed = true;
    }

    /// Latches why this evaluation's git effects cannot be published.
    pub(crate) fn fail(&self, message: String) {
        self.current(|line| line.fail(message));
    }
}

impl Drop for GitGuard {
    fn drop(&mut self) {
        let mut state = self
            .snapshot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let line = &mut state.git;
        line.live -= 1;
        if line.generation == self.generation {
            if !self.completed {
                line.fail("git: a managed git's observation was cut short".to_string());
            }
            line.members -= 1;
            line.children.remove(&self.id);
            if line.members == 0 {
                line.cohort = None;
            }
        }
        drop(state);
    }
}

impl SnapshotState {
    /// Folds one traced call into this line's footprint, and says whether the footprint now
    /// depends on something another principal published since `base_seq`.
    ///
    /// # Errors
    ///
    /// Fails when the call named a path the decoder cannot read: the command touched something
    /// and the evidence does not say what, which a publication may not be built on.
    fn observe(
        &mut self,
        line: &TraceLine,
        root: &Path,
        authority: &Authority,
    ) -> Result<bool, MarshError> {
        let effects = self
            .access
            .observe(line, root)
            .map_err(|cause| MarshError::Io(std::io::Error::other(cause)))?;
        let known = self
            .dependency
            .filter(|known| known.seq == authority.seq && known.base_seq == self.base_seq);
        let added = known.is_some_and(|known| !known.newer)
            && self.newer(
                &effects.reads,
                &effects.recursive_reads,
                &authority.generations,
            );
        if !effects.is_empty() {
            self.traces.push(line.to_string());
            self.reads.extend(effects.reads);
            for path in effects.writes {
                self.writes.entry(path).or_default().push(line.ts_us);
            }
            self.recursive_reads.extend(effects.recursive_reads);
            for path in effects.recursive_writes {
                self.recursive_writes
                    .entry(path)
                    .or_default()
                    .push(line.ts_us);
            }
        }
        let newer = match known {
            Some(known) => known.newer || added,
            None => self.depends_on_newer(&authority.generations),
        };
        self.dependency = Some(Dependency {
            seq: authority.seq,
            base_seq: self.base_seq,
            newer,
        });
        Ok(newer)
    }

    /// Forgets the footprint of an evaluation that will be run again, keeping its evidence.
    ///
    /// The trace lines stay and the offset moves past them: the attempt happened and its record
    /// is still dumped, but nothing it observed may reach the next evaluation's request.
    fn reset_footprint(&mut self) {
        self.traces_seen = self.traces.len();
        self.reads.clear();
        self.writes.clear();
        self.recursive_reads.clear();
        self.dependency = None;
        self.recursive_writes.clear();
        self.access = Access::default();
        self.git.reset();
    }

    /// Whether `path` is inside what this evaluation was seen to write.
    fn written(&self, path: &Path) -> bool {
        self.writes.contains_key(path)
            || self
                .recursive_writes
                .keys()
                .any(|prefix| path.starts_with(prefix))
    }

    /// When this evaluation wrote `path`, on the trace's clock.
    ///
    /// The calls that wrote the path itself, when any did. Only a path no call named — one that
    /// arrived inside a directory renamed into place — takes the stamps of the restructuring above
    /// it: a `mkdir` that preceded a file's own write says nothing about when that write happened.
    fn write_stamps(&self, path: &Path) -> Vec<u64> {
        if let Some(stamps) = self.writes.get(path) {
            return stamps.clone();
        }
        self.recursive_writes
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix))
            .flat_map(|(_, stamps)| stamps.iter().copied())
            .collect()
    }

    /// Whether another principal published something this evaluation read.
    ///
    /// A plain read names one path. A recursive read — a directory that was renamed or removed
    /// out from under the command — depends on everything beneath it, because that is what moved.
    fn depends_on_newer(&self, generations: &HashMap<PathBuf, u64>) -> bool {
        self.newer(&self.reads, &self.recursive_reads, generations)
    }

    /// Whether any of `reads`, or anything beneath one of `recursive_reads`, was published after
    /// `base_seq`.
    fn newer<'a>(
        &self,
        reads: impl IntoIterator<Item = &'a PathBuf>,
        recursive_reads: impl IntoIterator<Item = &'a PathBuf>,
        generations: &HashMap<PathBuf, u64>,
    ) -> bool {
        let newer = |seq: &u64| *seq > self.base_seq;
        reads
            .into_iter()
            .any(|path| generations.get(path).is_some_and(newer))
            || recursive_reads.into_iter().any(|prefix| {
                generations
                    .iter()
                    .any(|(path, seq)| newer(seq) && path.starts_with(prefix))
            })
    }
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
    /// the records are evidence for the dumps, and requests come from the observed footprint.
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
                BuiltinRecord::Begin { id, .. } | BuiltinRecord::End { id, .. } => {
                    mine.contains(id)
                }
            })
            .cloned()
            .collect()
    }

    /// Every builtin this snapshot's shell ran.
    pub(crate) fn builtin_records(&self) -> Vec<BuiltinRecord> {
        self.attributed(&self.session.hook.records())
    }

    /// Folds one traced call into this line's footprint, and decides whether the line has to be
    /// evaluated again.
    ///
    /// Called by the instrumentation's decoder, outside every lock that layer holds, which is why
    /// it may take this session's. The authority is read first and the snapshot's state second —
    /// the order every boundary uses — and both are released before anything is signalled: a
    /// publication must not wait on a process dying, and a driver must not be woken with the seed
    /// held.
    ///
    /// Only *reads* invalidate. Two shells writing the same path is an ownership question the
    /// capability policy already answers, and re-running the loser would not change its answer.
    ///
    /// # Errors
    ///
    /// Fails when the call named a path the decoder cannot read.
    pub(crate) fn observe(&self, line: &TraceLine) -> std::io::Result<()> {
        let authority = self
            .session
            .authority
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let classified = state.observe(line, &self.path, &authority);
        let depends = matches!(classified, Ok(true));
        drop(state);
        drop(authority);

        if depends && !self.interrupted.swap(true, std::sync::atomic::Ordering::AcqRel) {
            // First observation only: the run loop signals this evaluation's own processes once,
            // and a second notification would signal an evaluation that has already unwound.
            self.resume.notify_waiters();
        }
        classified.map(|_newer| ()).map_err(|error| match error {
            MarshError::Io(error) => error,
            other => std::io::Error::other(other.to_string()),
        })
    }

    /// Whether this evaluation was told to stop and start over.
    pub(crate) fn interrupted(&self) -> bool {
        self.interrupted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Admits a managed git invocation of `kind`, or refuses one that could not be attributed.
    ///
    /// Inspections run together; anything that may change state runs alone, because its effects
    /// are read off the difference between before and after and a second invocation's would be
    /// indistinguishable from its own. A refusal is latched: the line it happened in fails at its
    /// boundary rather than publishing whatever the other invocation left.
    ///
    /// # Errors
    ///
    /// Fails with the refusal's diagnostic when another invocation is running and either of the
    /// two may change state.
    pub(crate) fn begin_git(self: &Arc<Self>, kind: GitCohortKind) -> Result<GitGuard, String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let line = &mut state.git;
        match line.cohort {
            None => line.cohort = Some(kind),
            Some(GitCohortKind::Inspect) if kind == GitCohortKind::Inspect => {}
            Some(_) => {
                let refusal = "git: overlapping managed Git mutations cannot be attributed";
                line.fail(refusal.to_string());
                return Err(refusal.to_string());
            }
        }
        line.members += 1;
        line.live += 1;
        let id = line.next_id;
        line.next_id += 1;
        let generation = line.generation;
        drop(state);
        Ok(GitGuard {
            snapshot: Arc::clone(self),
            generation,
            id,
            completed: false,
        })
    }

    /// Waits until every syscall this shell has issued so far has been decoded into its
    /// footprint.
    ///
    /// Taken outside every lock of this session: the decoder classifies under them.
    ///
    /// # Errors
    ///
    /// Fails when the evidence stream is broken, the decoder cannot keep up, or a call of this
    /// line entered the kernel and never returned.
    pub(crate) fn drain_trace(&self) -> Result<(), MarshError> {
        self.session.hook.drain()?;
        if self.session.hook.unresolved(&self.path) {
            return Err(MarshError::Io(std::io::Error::other(
                "file access tracing failed: a call of this line entered the kernel and never \
                 returned, so its effect is unknown",
            )));
        }
        Ok(())
    }

    /// The snapshot-relative paths this evaluation wrote in `(after, until]` on the trace's clock,
    /// counting a restructured directory as the path it names.
    ///
    /// Only as complete as the decoder: call [`Self::drain_trace`] first.
    pub(crate) fn writes_between(&self, after: u64, until: u64) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .writes
            .iter()
            .chain(&state.recursive_writes)
            .filter(|(_, stamps)| stamps.iter().any(|stamp| *stamp > after && *stamp <= until))
            .map(|(path, _)| path.clone())
            .collect()
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
        if state.git.live > 0 {
            return Err(MarshError::Io(std::io::Error::other(
                "git: a managed git an earlier boundary cancelled is still exiting; the snapshot \
                 cannot be retaken under it",
            )));
        }
        if state.base_seq != authority.seq {
            self.session.internal(|| {
                self.session.fs.delete_subvolume(&self.path);
                self.session
                    .fs
                    .snapshot(&self.session.persistence.seed, &self.path)
            })?;
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
        state.git.cancel_running();

        let spawns = self.spawns.records();
        let all = self.session.hook.records();
        let new_spawns: Vec<u64> = spawns[state.spawns_seen..]
            .iter()
            .map(SpawnRecord::id)
            .collect();
        let new_builtins: Vec<u64> = self
            .attributed(&all[state.builtins_seen..])
            .iter()
            .filter_map(|record| match record {
                BuiltinRecord::Begin { id, .. } => Some(*id),
                BuiltinRecord::End { .. } => None,
            })
            .collect();
        let whole_builtins = self.attributed(&all);
        state.spawns_seen = spawns.len();
        state.builtins_seen = all.len();

        // The read dependency is decided before the diff, and the diff is skipped when it holds:
        // two full tree walks to describe a tree that is about to be thrown away and retaken is
        // the one cost this boundary can simply not pay.
        let needs_sync = state.depends_on_newer(&authority.generations);
        let stale_view = state.base_seq != authority.seq;
        let dirty = !state.writes.is_empty()
            || !state.recursive_writes.is_empty()
            || self.interrupted();
        let (ops, edits) = if needs_sync {
            (Vec::new(), Vec::new())
        } else {
            // Filtered by what this line was *seen* to write. A disjoint file another principal
            // published while this line ran is present in the seed and absent from this older
            // snapshot, and an unfiltered diff would carry it back out as a deletion. The order
            // `diff_trees` produced — removals deepest first, then writes shallowest first — is
            // preserved, because that order is what makes the apply legal.
            let seed = &self.session.persistence.seed;
            let ops: Vec<CommitOp> = self
                .session
                .internal(|| marsh_wal::diff_trees(seed, &self.path))?
                .into_iter()
                .filter(|op| state.written(op.path()))
                .collect();
            let edits = self.session.internal(|| {
                ops.iter()
                    .filter(|op| changes_content(seed, &self.path, op))
                    .map(|op| (op.path().to_path_buf(), state.write_stamps(op.path())))
                    .collect()
            });
            (ops, edits)
        };
        Ok(Pending {
            snapshot: self,
            authority,
            state,
            cmd: cmd.to_string(),
            spawns,
            whole_builtins,
            new_spawns,
            new_builtins,
            ops,
            edits,
            needs_sync,
            stale_view,
            dirty,
        })
    }

    /// Writes every record stream into `meta/runs/<uid>`, whole.
    ///
    /// `trace.log` sits beside the two record dumps because it is the third stream of the same
    /// run: what the shell was asked to do, what it spawned, and what the kernel saw it touch.
    /// An evaluation that was abandoned and run again leaves its evidence here too — the attempt
    /// happened — even though nothing it observed reaches a capability request.
    fn dump(
        &self,
        spawns: &[SpawnRecord],
        builtins: &[BuiltinRecord],
        traces: &[String],
    ) -> Result<(), MarshError> {
        let spawns = dump_records(spawns)?;
        let builtins = dump_records(builtins)?;
        self.session.internal(|| {
            std::fs::write(self.run_dir.join("spawns.json"), spawns)?;
            std::fs::write(self.run_dir.join("builtins.json"), builtins)?;
            std::fs::write(self.run_dir.join("trace.log"), traces.join("\n"))?;
            Ok(())
        })
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
    /// Ids of this boundary's spawn records, for the transaction's metadata.
    new_spawns: Vec<u64>,
    /// Ids of this boundary's builtin invocations, for the transaction's metadata.
    new_builtins: Vec<u64>,
    /// The seed-to-snapshot difference, filtered to this line's own write footprint.
    ops: Vec<CommitOp>,
    /// The operations that change content rather than only the tree's shape, each with the trace
    /// timestamps of the calls that wrote it.
    edits: Vec<(PathBuf, Vec<u64>)>,
    /// Whether this line read something another principal published while it ran.
    ///
    /// Decided before the diff, which is why there is no diff to look at when it is set.
    needs_sync: bool,
    /// Whether the seed moved on since this snapshot was taken.
    ///
    /// Not a verdict on this line — the publication may be entirely disjoint — but the reason a
    /// tree cannot be called current without being retaken.
    stale_view: bool,
    /// Whether this evaluation was seen to write inside the snapshot, or was cut short with its
    /// evidence incomplete.
    dirty: bool,
}

impl Pending<'_> {
    /// The content changes this boundary found, with the timestamps that place them in the line.
    pub(crate) fn edits(&self) -> &[(PathBuf, Vec<u64>)] {
        &self.edits
    }

    /// What this evaluation's git invocations did.
    pub(crate) fn git_records(&self) -> &[GitEffectRecord] {
        &self.state.git.records
    }

    /// Why this evaluation's git effects cannot be published, when something made them so.
    ///
    /// A line with a latched failure is never translated: whatever its tree holds was left by a
    /// git whose effects are unknown or unattributable.
    pub(crate) fn git_failure(&self) -> Option<&str> {
        self.state.git.failure.as_deref()
    }

    /// Whether this line read something another principal has published since its snapshot was
    /// taken.
    ///
    /// The line's own answer may depend on bytes that are no longer current, so it is evaluated
    /// again rather than judged. Writes are deliberately not consulted: two principals writing one
    /// path is an ownership question, and the capability policy is what answers it.
    pub(crate) const fn needs_sync(&self) -> bool {
        self.needs_sync
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
    /// as state-preserving. The snapshot is retaken either way, because an empty publication out
    /// of an older tree leaves that tree older than the seed.
    ///
    /// # Errors
    ///
    /// Fails when the transaction cannot be logged or applied, or when a record stream cannot be
    /// written.
    pub(crate) fn publish(mut self, granted: &[Event]) -> Result<Publication, MarshError> {
        if self.ops.is_empty() {
            self.finish()?;
            // Retaken when the view moved or a write was seen, not merely renumbered: this tree
            // may be older than the seed — another principal published a disjoint path while this
            // line ran — and calling it current without retaking it would make the next line's
            // diff report that path as a deletion.
            let retake = self.stale_view || self.dirty;
            self.resynchronize(retake)?;
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
            cmd: std::mem::take(&mut self.cmd),
            spawns: std::mem::take(&mut self.new_spawns),
            builtins: std::mem::take(&mut self.new_builtins),
            principal: self.snapshot.uid.clone(),
            durable_principal: self.snapshot.durable_name.as_ref().map(ToString::to_string),
            granted: granted.iter().map(GrantedCapability::from).collect(),
        };
        self.snapshot.session.internal(|| {
            marsh_wal::apply(
                &self.snapshot.session.persistence.seed,
                &self.snapshot.path,
                &self.snapshot.session.log,
                self.snapshot.uid.as_str(),
                seq,
                &meta,
                &self.ops,
            )
        })?;
        self.authority.seq = seq;
        for op in &self.ops {
            self.authority
                .generations
                .insert(op.path().to_path_buf(), seq);
        }
        self.state.base_seq = seq;
        self.finish()?;
        // Cleared before the retake: the apply and every record dump have succeeded, so the
        // transaction is complete and this tree is no longer anybody's recovery source.
        self.state.recovery_required = false;
        self.authority.recovery_required = false;
        // The apply made the seed carry this tree's writes. It does not make this tree carry
        // somebody else's, so a tree that was already behind is retaken and a current one is not.
        let retake = self.stale_view;
        self.resynchronize(retake)?;
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
        let traces = self.state.traces.clone();
        self.snapshot
            .dump(&self.spawns, &self.whole_builtins, &traces)
    }

    /// Throws the line away: dumps the records — the attempt happened — then retakes the snapshot
    /// from the seed, so the tree the shell runs in is the seed's again.
    ///
    /// A background job still writing into the old snapshot writes into a deleted tree, which is
    /// what a per-line snapshot refresh means.
    ///
    /// # Errors
    ///
    /// Fails when a record stream cannot be written or the snapshot cannot be retaken.
    pub(crate) fn discard(mut self) -> Result<(), MarshError> {
        // A discard can never prove the tree clean: it is thrown away precisely because the
        // evidence for it was refused, invalidated or cut short. Retake unless nothing at all
        // happened and the view never moved.
        let retake = self.stale_view || self.dirty;
        self.finish()?;
        self.resynchronize(retake)
    }

    /// Dumps this boundary's evidence and forgets the footprint it was decided from.
    ///
    /// # Errors
    ///
    /// Fails when a record stream cannot be written.
    fn finish(&mut self) -> Result<(), MarshError> {
        let traces = self.state.traces.clone();
        self.snapshot
            .dump(&self.spawns, &self.whole_builtins, &traces)?;
        self.state.reset_footprint();
        self.snapshot
            .interrupted
            .store(false, std::sync::atomic::Ordering::Release);
        self.snapshot.session.hook.retire_unresolved(&self.snapshot.path);
        Ok(())
    }

    /// Makes the tree the seed's again, and records that it is.
    ///
    /// `retake` is the caller's judgement of whether this tree can still be *proved* equal to the
    /// seed. "The filtered difference was empty" is not that proof — the filter is exactly what
    /// hides a disjoint publication this tree does not have yet — so every ending decides it from
    /// what it knows: whether the view moved, whether a write was observed, and whether the
    /// evaluation was interrupted with its evidence incomplete.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be retaken.
    fn resynchronize(&mut self, retake: bool) -> Result<(), MarshError> {
        if retake {
            let session = &self.snapshot.session;
            session.internal(|| {
                session.fs.delete_subvolume(&self.snapshot.path);
                session
                    .fs
                    .snapshot(&session.persistence.seed, &self.snapshot.path)
            })?;
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
        // Before anything else: no line of this shell can be decoded after its tree is gone, and
        // the last root going is what stops the host's tracer.
        let _ = self.session.hook.unregister_root(&self.path);
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

/// Whether `op` changes a file's content rather than only the tree's shape.
///
/// An empty directory's creation, mode change or removal is published so the seed keeps its
/// shape — an unborn repository is empty directories and a few files — but it is not a resource
/// anybody edits. A directory that replaced a file is still the end of that file, so it is.
fn changes_content(seed: &Path, work: &Path, op: &CommitOp) -> bool {
    let is_directory = |root: &Path| {
        std::fs::symlink_metadata(root.join(op.path())).is_ok_and(|metadata| metadata.is_dir())
    };
    match op {
        CommitOp::Write(path) => {
            !is_directory(work)
                || std::fs::symlink_metadata(seed.join(path))
                    .is_ok_and(|metadata| !metadata.is_dir())
        }
        CommitOp::Remove(_) => !is_directory(seed),
    }
}

/// Deletes every snapshot left by a previous session.
///
/// Everything under `snap` is a snapshot by construction, so no name filtering. Called only after
/// recovery has run: normally that means every tree's content was already replayed or reported,
/// but a log recovery reset because it could not be parsed discards those trees' content
/// unreplayed instead — this sweep is what actually erases it.
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
    marsh_lib::walk_directory::<_, MarshError>(root, (), |entry, ()| {
        // The entry's own type, never `path.is_dir()`: that follows a symlink, and a link named
        // like a temporary would take the sweep out of the tree it was asked to clean.
        if entry.file_type()?.is_dir() {
            return Ok(Some(()));
        }
        let path = entry.path();
        if path.file_name().is_some_and(|name| {
            name.as_encoded_bytes()
                .ends_with(marsh_wal::TEMPORARY_SUFFIX.as_bytes())
        }) {
            std::fs::remove_file(path)?;
        }
        Ok(None)
    })
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

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep erases the temporaries a crash left behind and nothing else — and it classifies
    /// entries by what they are, not by what following them would reach. A symlink out of the
    /// seed is removed as the link it is; the tree it points at is not swept.
    #[test]
    fn sweep_temporaries_removes_only_temporary_leaves_without_following_links() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let seed = scratch.path().join("seed");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(seed.join("nested")).expect("a nested directory");
        std::fs::create_dir_all(seed.join("dir.tmp-wal")).expect("a directory named like one");
        std::fs::create_dir_all(&outside).expect("a sibling tree");
        std::fs::write(seed.join("a.tmp-wal"), b"a").expect("a temporary");
        std::fs::write(seed.join("nested/b.tmp-wal"), b"b").expect("a nested temporary");
        std::fs::write(seed.join("keep.txt"), b"keep").expect("a real file");
        std::fs::write(seed.join("dir.tmp-wal/keep.txt"), b"keep").expect("a file inside it");
        std::fs::write(outside.join("untouched.tmp-wal"), b"sentinel").expect("a sentinel");
        std::os::unix::fs::symlink(&outside, seed.join("link.tmp-wal")).expect("a matching link");
        std::os::unix::fs::symlink(&outside, seed.join("keep-link")).expect("an unrelated link");

        sweep_temporaries(&seed).expect("the sweep runs");

        for gone in ["a.tmp-wal", "nested/b.tmp-wal", "link.tmp-wal"] {
            assert!(
                seed.join(gone).symlink_metadata().is_err(),
                "{gone} is a temporary and is gone"
            );
        }
        for kept in [
            "keep.txt",
            "dir.tmp-wal",
            "dir.tmp-wal/keep.txt",
            "keep-link",
        ] {
            assert!(
                seed.join(kept).symlink_metadata().is_ok(),
                "{kept} is not a temporary leaf and survives"
            );
        }
        assert!(
            outside.join("untouched.tmp-wal").exists(),
            "the sweep never walked through a symlink out of the seed"
        );
    }

    /// A line's read dependency is decided call by call without walking the whole footprint each
    /// time, yet a publication that lands *after* a read still counts against it from the very
    /// next call — whatever that call touches — and a read made after a publication counts too.
    #[test]
    fn a_read_depends_on_publications_before_and_after_it() {
        let root = Path::new("/work");
        let read = |name: &str| TraceLine {
            tid: 7,
            ts_us: 1,
            call: marsh_instrument::Call::Syscall {
                name: "openat".to_string(),
                args: format!("AT_FDCWD</work>, \"{name}\", O_RDONLY"),
                ret: 3,
                ret_path: Some(format!("/work/{name}")),
            },
        };
        let mut state = SnapshotState {
            base_seq: 0,
            spawns_seen: 0,
            builtins_seen: 0,
            traces: Vec::new(),
            traces_seen: 0,
            reads: BTreeSet::new(),
            writes: BTreeMap::new(),
            recursive_reads: BTreeSet::new(),
            recursive_writes: BTreeMap::new(),
            access: Access::default(),
            git: GitLine::default(),
            dependency: None,
            recovery_required: false,
        };
        let mut authority = Authority {
            seq: 0,
            generations: HashMap::new(),
            recovery_required: false,
        };
        let observe = |state: &mut SnapshotState, authority: &Authority, name: &str| {
            state
                .observe(&read(name), root, authority)
                .expect("a readable line")
        };

        assert!(!observe(&mut state, &authority, "a.txt"));
        authority.seq = 1;
        authority.generations.insert(PathBuf::from("a.txt"), 1);
        assert!(
            observe(&mut state, &authority, "b.txt"),
            "a.txt was read before another principal published it"
        );

        state.reset_footprint();
        state.base_seq = 1;
        assert!(!observe(&mut state, &authority, "a.txt"), "rebased past it");
        authority.seq = 2;
        authority.generations.insert(PathBuf::from("c.txt"), 2);
        assert!(
            !observe(&mut state, &authority, "b.txt"),
            "a publication of an unread file changes nothing"
        );
        assert!(
            observe(&mut state, &authority, "c.txt"),
            "c.txt was read after another principal published it"
        );
    }
}
