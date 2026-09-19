//! One attached session: the seed, the snapshot the shell runs in, and the log between them.
//!
//! A session is opened once and shared by every clone of the executor that owns it. It holds the
//! seed's exclusive lease for its whole life, so a second session over the same seed is refused
//! rather than allowed to publish concurrently.
//!
//! The order at startup is the one a crash makes load-bearing: materialize the state directory,
//! recover the log, *then* sweep leftover snapshots — an unfinished transaction's content lives in
//! the snapshot the sweep would delete.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use marsh_btrfs::{PersistenceLayer, Subvolumes, short_id};
use marsh_instrument::{BuiltinRecord, RecordingHook, SpawnRecord, SpawnRecorder, dump_records};
use marsh_wal::CommitOp;

use super::MarshError;

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
}

/// The outcome of one publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    /// The seed version after it: unchanged when nothing was published.
    pub seq: u64,
    /// How many filesystem operations the transaction carried.
    pub ops: usize,
}

/// The mutable half of a session, behind one lock.
struct PublishState {
    /// The highest transaction sequence number the log carries.
    seq: u64,
    /// How many spawn records previous publications already accounted for.
    spawns_seen: usize,
    /// How many builtin records previous publications already accounted for.
    builtins_seen: usize,
}

/// One attached session.
pub(crate) struct Session {
    /// The btrfs operations, real or faked.
    fs: Arc<dyn Subvolumes>,
    /// This session's snapshot id, and the name of its directory under `snap/`.
    uid: String,
    /// The canonicalized snapshot the shell runs inside.
    snapshot: PathBuf,
    /// `meta/runs/<uid>`: where the record streams are dumped.
    run_dir: PathBuf,
    /// `meta/wal.jsonl`.
    log: PathBuf,
    /// The builtin hook the instrumented registrations report to.
    pub(crate) hook: Arc<RecordingHook>,
    /// Every external command the spawner was asked to start.
    pub(crate) spawns: SpawnRecorder,
    /// Sequence number and record attribution.
    state: Mutex<PublishState>,
    /// The seed, the state directory, and the lease over both.
    ///
    /// Declared last so it is dropped last: [`Drop for Session`](Self::drop) deletes the snapshot,
    /// and the lease must still be held while it does.
    pub(crate) persistence: PersistenceLayer,
}

impl Session {
    /// Attaches to the seed containing `seed` and takes a snapshot of it.
    ///
    /// # Errors
    ///
    /// Fails when no btrfs subvolume contains `seed`, when another process already holds the
    /// seed's lease, when the log cannot be recovered, or when the snapshot cannot be taken.
    pub(crate) fn open(seed: &Path, fs: Arc<dyn Subvolumes>) -> Result<Arc<Self>, MarshError> {
        let mut persistence = PersistenceLayer::discover(seed, fs.as_ref())?;
        persistence.materialize(fs.as_ref())?;
        persistence.acquire()?;

        let log = persistence.meta().join(marsh_wal::LOG_FILE);
        // Before the sweep: an unfinished transaction's content is in the snapshot it reclaims.
        let recovered =
            marsh_wal::recover::<PublishMeta>(&persistence.seed, &persistence.snap(), &log)?;
        let seq = recovered
            .iter()
            .map(|transaction| transaction.seq)
            .max()
            .unwrap_or(0);

        sweep_snapshots(&persistence.snap(), fs.as_ref())?;
        sweep_temporaries(&persistence.seed)?;

        let uid = short_id(&format!(
            "{}:{}:{}",
            persistence.seed.display(),
            std::process::id(),
            nanos_since_epoch()
        ));
        let snapshot = persistence.work(&uid);
        fs.snapshot(&persistence.seed, &snapshot)?;
        let snapshot = snapshot.canonicalize()?;
        let run_dir = persistence.run_dir(&uid)?;
        std::fs::create_dir_all(&run_dir)?;

        Ok(Arc::new(Self {
            fs,
            uid,
            snapshot,
            run_dir,
            log,
            hook: Arc::new(RecordingHook::default()),
            spawns: SpawnRecorder::default(),
            state: Mutex::new(PublishState {
                seq,
                spawns_seen: 0,
                builtins_seen: 0,
            }),
            persistence,
        }))
    }

    /// The snapshot the shell runs inside.
    pub(crate) fn snapshot(&self) -> &Path {
        &self.snapshot
    }

    /// This session's snapshot id.
    pub(crate) fn uid(&self) -> &str {
        &self.uid
    }

    /// Takes stock at a command-line boundary: the records since the previous boundary (which it
    /// marks as seen) and the difference between the seed and the snapshot.
    ///
    /// `cmd` is the command line whose completion prompted it, or empty when nothing named it.
    /// Records that precede a boundary belong to it whether the caller then publishes or
    /// discards — the attempt happened either way. The session's lock is held by the returned
    /// value until one of those two endings, so no second boundary can interleave with this one.
    ///
    /// # Errors
    ///
    /// Fails when the trees cannot be compared.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard travels in the returned `Pending` on purpose: attributing the records, \
                  diffing, and the ending that publishes or discards are one atomic boundary"
    )]
    pub(crate) fn pending(&self, cmd: &str) -> Result<Pending<'_>, MarshError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let spawns = self.spawns.records();
        let builtins = self.hook.records();
        let new_spawns: Vec<u64> = spawns[state.spawns_seen..]
            .iter()
            .map(SpawnRecord::id)
            .collect();
        let first_builtin = state.builtins_seen;
        let new_builtins: Vec<u64> = builtins[first_builtin..]
            .iter()
            .filter_map(|record| match record {
                BuiltinRecord::Begin { id, .. } => Some(*id),
                BuiltinRecord::End { .. } => None,
            })
            .collect();
        state.spawns_seen = spawns.len();
        state.builtins_seen = builtins.len();

        let ops = marsh_wal::diff_trees(&self.persistence.seed, &self.snapshot)?;
        Ok(Pending {
            session: self,
            state,
            cmd: cmd.to_string(),
            spawns,
            builtins,
            first_builtin,
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

/// What a command line left since the last boundary, held under the session lock until it is
/// published or discarded.
pub(crate) struct Pending<'a> {
    /// The session this boundary belongs to.
    session: &'a Session,
    /// The session's state, held for the whole boundary so no second one interleaves.
    state: MutexGuard<'a, PublishState>,
    /// The command line that prompted the boundary; empty when nothing named it.
    cmd: String,
    /// Every spawn record, whole: the dumps are always whole.
    spawns: Vec<SpawnRecord>,
    /// Every builtin record, whole, for the same reason.
    builtins: Vec<BuiltinRecord>,
    /// Where this boundary's builtin records start in `builtins`.
    first_builtin: usize,
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
        &self.builtins[self.first_builtin..]
    }

    /// Applies the difference to the seed through the log.
    ///
    /// A boundary that found no difference writes no transaction; it still refreshes the record
    /// dumps, which is the only way a command that changed nothing shows up on disk at all.
    ///
    /// # Errors
    ///
    /// Fails when the transaction cannot be logged or applied, or when a record stream cannot be
    /// written.
    pub(crate) fn publish(mut self) -> Result<Publication, MarshError> {
        if self.ops.is_empty() {
            self.session.dump(&self.spawns, &self.builtins)?;
            return Ok(Publication {
                seq: self.state.seq,
                ops: 0,
            });
        }

        let seq = self.state.seq + 1;
        let meta = PublishMeta {
            cmd: self.cmd,
            spawns: self.new_spawns,
            builtins: self.new_builtins,
        };
        marsh_wal::apply(
            &self.session.persistence.seed,
            &self.session.snapshot,
            &self.session.log,
            &self.session.uid,
            seq,
            &meta,
            &self.ops,
        )?;
        self.state.seq = seq;
        self.session.dump(&self.spawns, &self.builtins)?;
        Ok(Publication {
            seq,
            ops: self.ops.len(),
        })
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
    pub(crate) fn discard(self) -> Result<(), MarshError> {
        self.session.dump(&self.spawns, &self.builtins)?;
        if !self.ops.is_empty() {
            self.session.fs.delete_subvolume(&self.session.snapshot);
            self.session
                .fs
                .snapshot(&self.session.persistence.seed, &self.session.snapshot)?;
        }
        Ok(())
    }
}

impl Drop for Session {
    /// Publishes whatever is still in the snapshot, then reclaims it.
    ///
    /// Normally nothing is: the [`Shell`](super::Shell) that ran in it concluded the last boundary
    /// in its own drop. What remains is what a failed final boundary left, and a snapshot whose
    /// publication fails again is deliberately kept: it is the source the next session's recovery
    /// replays from, and deleting it would strand the log.
    fn drop(&mut self) {
        if self.pending("").and_then(Pending::publish).is_ok() {
            self.fs.delete_subvolume(&self.snapshot);
        }
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
/// One of three inputs to a snapshot id; a degenerate clock costs uniqueness, never correctness,
/// because the pid and the seed path are in the digest too.
fn nanos_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}
