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
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use brush_btrfs::{PersistenceLayer, Subvolumes, short_id};
use brush_instrument::{CommandRecord, CommandRecorder, RecordingHook, dump_records};

use crate::MarshError;

/// What every publication records in its `BEGIN` line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishMeta {
    /// The command line whose completion triggered the publication: its argv joined by single
    /// spaces.
    pub cmd: String,
    /// Ids of the [`CommandRecord`]s whose effects the publication may contain.
    pub commands: Vec<u64>,
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
    /// Ids of dispatched commands whose effects no publication has covered yet.
    pending: Vec<u64>,
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
    /// The builtin hook an instrumented builtin map reports to.
    pub(crate) hook: Arc<RecordingHook>,
    /// Every simple command the executor dispatched.
    pub(crate) commands: CommandRecorder,
    /// Sequence number and deferred command ids.
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

        let log = persistence.meta().join(brush_wal::LOG_FILE);
        // Before the sweep: an unfinished transaction's content is in the snapshot it reclaims.
        let recovered =
            brush_wal::recover::<PublishMeta>(&persistence.seed, &persistence.snap(), &log)?;
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
            commands: CommandRecorder::default(),
            state: Mutex::new(PublishState {
                seq,
                pending: Vec::new(),
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

    /// Records that `id`'s effects are not yet covered by any publication.
    pub(crate) fn defer(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .push(id);
    }

    /// Whether any dispatched command's effects are still uncovered.
    pub(crate) fn has_pending(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .is_empty()
    }

    /// Publishes everything the snapshot changed since the last publication.
    ///
    /// `trigger` is the command whose completion prompted it, if there is one; the transaction's
    /// metadata names it and every command deferred before it. A publication that finds no
    /// difference writes no transaction — it still refreshes the record dumps, which is the only
    /// way a command that changed nothing shows up on disk at all.
    ///
    /// # Errors
    ///
    /// Fails when the trees cannot be compared, when the transaction cannot be logged or applied,
    /// or when a record stream cannot be written.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the lock is held for the whole transaction on purpose: taking the pending set, \
                  diffing, logging and bumping the sequence number are one atomic publication"
    )]
    pub(crate) fn publish(&self, trigger: Option<u64>) -> Result<Publication, MarshError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut covered = std::mem::take(&mut state.pending);
        if let Some(id) = trigger {
            covered.push(id);
        }

        let ops = brush_wal::diff_trees(&self.persistence.seed, &self.snapshot)?;
        if ops.is_empty() {
            self.dump()?;
            return Ok(Publication {
                seq: state.seq,
                ops: 0,
            });
        }

        let seq = state.seq + 1;
        let cmd = self.command_line(trigger.or_else(|| covered.last().copied()));
        brush_wal::apply(
            &self.persistence.seed,
            &self.snapshot,
            &self.log,
            &self.uid,
            seq,
            &PublishMeta {
                cmd,
                commands: covered,
            },
            &ops,
        )?;
        state.seq = seq;
        self.dump()?;
        Ok(Publication {
            seq,
            ops: ops.len(),
        })
    }

    /// The argv of the command `id` names, joined by single spaces; empty when there is none.
    fn command_line(&self, id: Option<u64>) -> String {
        let Some(id) = id else {
            return String::new();
        };
        self.commands
            .records()
            .iter()
            .find_map(|record| match record {
                CommandRecord::Begin {
                    id: recorded, argv, ..
                } if *recorded == id => Some(argv.join(" ")),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Writes both record streams into `meta/runs/<uid>`, whole.
    fn dump(&self) -> Result<(), MarshError> {
        std::fs::write(
            self.run_dir.join("commands.json"),
            dump_records(&self.commands.records())?,
        )?;
        std::fs::write(
            self.run_dir.join("builtins.json"),
            dump_records(&self.hook.records())?,
        )?;
        Ok(())
    }
}

impl Drop for Session {
    /// Publishes whatever the shell left behind, then reclaims the snapshot.
    ///
    /// A snapshot whose final publication failed is deliberately kept: it is the source the next
    /// session's recovery replays from, and deleting it would strand the log.
    fn drop(&mut self) {
        if self.publish(None).is_ok() {
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
                    .ends_with(brush_wal::TEMPORARY_SUFFIX.as_bytes())
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
