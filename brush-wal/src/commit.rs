//! The write-ahead log: a job's snapshot into the seed.
//!
//! One append-only log, at `meta/wal.jsonl`, holds every transaction. The order is the protocol:
//! the whole transaction is logged and fsynced first, then each record is applied to the seed, then
//! `End`. A crash before `End` leaves a log a replay can finish; a crash after it leaves nothing to
//! do in the seed.
//!
//! The log also outlives its own stage. A caller normally follows a transaction with a record of
//! its own — a history entry, an audit line — and a crash between the two leaves this log as the
//! only surviving description of what entered the seed. That is why [`recover`] returns *every*
//! transaction whose intent is complete, finished or not: the caller compares them against whatever
//! it remembers and re-derives the difference.
//!
//! The per-transaction metadata `M` is entirely the caller's. It is serialized into the `BEGIN`
//! record beside the sequence number, flattened, so a caller's own fields sit at the top level of
//! the line; this crate never looks inside it.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::diff::CommitOp;
use crate::error::Error;
use crate::log::{self, JsonLog};

/// Log file name under a session's `meta/` directory.
pub const LOG_FILE: &str = "wal.jsonl";

/// One line of the log.
///
/// `M` is the caller's per-transaction metadata, flattened into the `BEGIN` line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "UPPERCASE")]
pub enum WalRecord<M> {
    /// Opens a transaction: the snapshot its content comes from, plus the caller's metadata.
    Begin {
        /// Sequence number this transaction will occupy.
        seq: u64,
        /// The job whose snapshot holds the content, so a replay can find it.
        uid: String,
        /// Number of operation records in the durable intent. Absent in legacy logs.
        #[serde(default)]
        op_count: Option<usize>,
        /// Whatever the caller wants recorded with this transaction.
        #[serde(flatten)]
        meta: M,
    },
    /// Copy the snapshot's version of a path into the seed.
    Move {
        /// Source, relative to the job's snapshot root.
        from: PathBuf,
        /// Destination, relative to the seed.
        to: PathBuf,
        /// `sha1` of the content being moved, so a replay can tell an applied record from an
        /// interrupted one after the snapshot it came from was swept.
        sha1: String,
    },
    /// Delete a path from the seed.
    Delete {
        /// Seed-relative path.
        path: PathBuf,
    },
    /// Every preceding record of this transaction has been applied.
    End {
        /// The sequence number opened by `Begin`.
        seq: u64,
    },
}

impl<M> WalRecord<M> {
    /// The commit operation this record performs, or `None` for the framing records.
    fn operation(&self) -> Option<CommitOp> {
        match self {
            Self::Move { to, .. } => Some(CommitOp::Write(to.clone())),
            Self::Delete { path } => Some(CommitOp::Remove(path.clone())),
            Self::Begin { .. } | Self::End { .. } => None,
        }
    }
}

/// One logged transaction, as [`recover`] reads it back.
#[derive(Debug)]
pub struct Transaction<M> {
    /// Sequence number the transaction occupies.
    pub seq: u64,
    /// The job whose snapshot its content came from.
    pub uid: String,
    /// The metadata [`apply`] recorded with it.
    pub meta: M,
    /// Its operations, in log order.
    pub ops: Vec<CommitOp>,
}

/// One transaction as the log's raw lines describe it, before it is validated or replayed.
struct Frame<M> {
    /// Sequence number the transaction occupies.
    seq: u64,
    /// The job whose snapshot its content comes from.
    uid: String,
    /// The caller's metadata.
    meta: M,
    /// Declared operation count, absent for legacy transactions.
    op_count: Option<usize>,
    /// Its `Move` and `Delete` records, in log order.
    records: Vec<WalRecord<M>>,
    /// Whether the log carries this transaction's `End`.
    finished: bool,
}

/// Logs one transaction and applies it to the seed.
///
/// Order is the protocol: the whole batch first, fsynced, then the records, then `End`.
///
/// `seed` is the tree the transaction publishes into, `work` the tree every [`CommitOp::Write`]
/// reads its content from, and `log` the path of the append-only log. Callers pass a `work` of
/// `snap.join(uid)` — the same shape [`recover`] resolves a logged transaction's source with.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when a record cannot be serialized — a path no JSON string can carry
/// fails here, before the seed is touched — and when a record cannot be applied to the seed, and
/// with [`Error::Io`] when the log cannot be written.
pub fn apply<M: Serialize>(
    seed: &Path,
    work: &Path,
    log: &Path,
    uid: &str,
    seq: u64,
    meta: &M,
    ops: &[CommitOp],
) -> Result<(), Error> {
    let mut records: Vec<WalRecord<&M>> = Vec::with_capacity(ops.len() + 1);
    records.push(WalRecord::Begin {
        seq,
        uid: uid.to_string(),
        op_count: Some(ops.len()),
        meta,
    });
    for op in ops {
        records.push(match op {
            CommitOp::Remove(path) => WalRecord::Delete { path: path.clone() },
            // `from` and `to` are equal in practice; both are logged so a record reads on its own.
            CommitOp::Write(path) => WalRecord::Move {
                from: path.clone(),
                to: path.clone(),
                sha1: content_hash(&work.join(path))?,
            },
        });
    }

    let mut handle = JsonLog::open(log)?;
    handle.append(&records)?;
    for record in &records {
        apply_record(seed, work, record)?;
    }
    handle.append(&[WalRecord::End { seq }])
}

/// Finishes every unfinished transaction and returns every transaction the log describes whole.
///
/// Called once at startup, before anything reads the seed and *before* any snapshot sweep: an
/// unfinished transaction's content lives in `snap/<uid>`. A finished transaction is returned too,
/// because the protocol writes the seed and then whatever the caller records afterwards, and a
/// crash between the two leaves this log as the only evidence of what was published.
///
/// Transactions come back in log order. A counted intent whose operations did not all reach the log
/// is abandoned — neither replayed nor returned.
///
/// A frame's content is looked for at `snap.join(&frame.uid)` — the `work` tree [`apply`] logged
/// it from — and `log` is the same log path [`apply`] appended to.
///
/// `M` is `Serialize` as well as deserializable because finishing an interrupted transaction
/// appends its `End` line, which is a record of the same enum.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when the log is corrupt, when a transaction's declared operation count
/// does not match its contents, when an unfinished legacy transaction carries no count, or when a
/// record can neither be applied nor recognized as already applied.
pub fn recover<M: Serialize + DeserializeOwned>(
    seed: &Path,
    snap: &Path,
    log: &Path,
) -> Result<Vec<Transaction<M>>, Error> {
    let records = JsonLog::<WalRecord<M>>::read(log)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }

    let mut frames: Vec<Frame<M>> = Vec::new();
    for record in records {
        match record {
            WalRecord::Begin {
                seq,
                uid,
                op_count,
                meta,
            } => frames.push(Frame {
                seq,
                uid,
                meta,
                op_count,
                records: Vec::new(),
                finished: false,
            }),
            WalRecord::End { seq } => {
                if let Some(frame) = frames.iter_mut().rev().find(|frame| frame.seq == seq) {
                    frame.finished = true;
                }
            }
            // A record before any `Begin` belongs to no transaction and describes nothing.
            operation => {
                if let Some(frame) = frames.last_mut() {
                    frame.records.push(operation);
                }
            }
        }
    }

    // Validate every frame before replaying any seed mutation.
    for frame in &frames {
        let actual = frame.records.len();
        match frame.op_count {
            Some(expected) if actual > expected || (frame.finished && actual != expected) => {
                return Err(Error::Wal(format!(
                    "transaction {} declares {expected} operations but contains {actual}",
                    frame.seq
                )));
            }
            None if !frame.finished => {
                return Err(Error::Wal(format!(
                    "unfinished legacy transaction {} has no operation count; recovery sources retained",
                    frame.seq
                )));
            }
            Some(_) | None => {}
        }
    }

    let mut handle: JsonLog<WalRecord<M>> = JsonLog::open(log)?;
    let mut recovered = Vec::with_capacity(frames.len());
    for frame in frames {
        if frame
            .op_count
            .is_some_and(|expected| frame.records.len() < expected)
        {
            continue;
        }
        if !frame.finished {
            let work = snap.join(&frame.uid);
            for record in &frame.records {
                apply_record(seed, &work, record)?;
            }
            handle.append(&[WalRecord::End { seq: frame.seq }])?;
        }
        recovered.push(Transaction {
            seq: frame.seq,
            uid: frame.uid,
            meta: frame.meta,
            ops: frame
                .records
                .iter()
                .filter_map(WalRecord::operation)
                .collect(),
        });
    }
    Ok(recovered)
}

/// Applies one record to the seed.
///
/// Idempotent, which is what lets a replay re-run a transaction that may have partly happened: a
/// write replaces whatever is there and a removal tolerates an absent path. A write whose source is
/// gone is not an error when the destination already carries the content the record named — that is
/// a transaction which completed and whose snapshot was swept.
fn apply_record<M>(seed: &Path, work: &Path, record: &WalRecord<M>) -> Result<(), Error> {
    match record {
        WalRecord::Begin { .. } | WalRecord::End { .. } => Ok(()),
        WalRecord::Delete { path } => log::apply_remove(seed, &seed.join(path)),
        WalRecord::Move { from, to, sha1 } => write(seed, work, from, to, sha1),
    }
}

/// Copies `from` (in the job's snapshot) onto `to` (in the seed).
fn write(seed: &Path, work: &Path, from: &Path, to: &Path, sha1: &str) -> Result<(), Error> {
    let source = work.join(from);
    let target = seed.join(to);
    if source.symlink_metadata().is_ok() {
        return log::apply_write(&source, &target);
    }
    if target.symlink_metadata().is_ok() && content_hash(&target)? == sha1 {
        return Ok(());
    }
    Err(Error::Wal(format!(
        "source {} is gone and {} does not carry its content; the transaction can neither be \
         completed nor undone",
        source.display(),
        target.display()
    )))
}

/// The hex-encoded `sha1` of `bytes`: the content hash the write-ahead log records.
///
/// Full length, never a prefix: this is what tells a replay "already applied" from "interrupted",
/// and it is compared, never typed.
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))
}

/// The `sha1` a record carries for a path: the bytes of a file, or the target of a symlink.
///
/// Anything else — a fifo, a socket, a device node — has no content to read; its mode is the whole
/// entry, and the diff already compared that.
fn content_hash(path: &Path) -> Result<String, Error> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        Ok(digest(
            std::fs::read_link(path)?.as_os_str().as_encoded_bytes(),
        ))
    } else if metadata.is_file() {
        Ok(digest(&std::fs::read(path)?))
    } else {
        Ok(digest(&[]))
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A caller's metadata, of the shape a shell multiplexer records: who asked, and for what.
    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Meta {
        /// The principal the transaction was performed for.
        principal: String,
        /// The command line, for the audit trail.
        cmd: String,
    }

    fn meta(cmd: &str) -> Meta {
        Meta {
            principal: "agent0".to_string(),
            cmd: cmd.to_string(),
        }
    }

    /// The three paths a transaction needs, over plain directories: these tests only copy files,
    /// so no subvolume is needed.
    struct Scratch {
        /// The tree transactions publish into.
        seed: PathBuf,
        /// The directory holding one snapshot per job uid.
        snap: PathBuf,
        /// The write-ahead log.
        log: PathBuf,
    }

    impl Scratch {
        /// A job's snapshot, `snap/<uid>`, created on first ask — what [`apply`] takes as `work`.
        fn work(&self, uid: &str) -> PathBuf {
            let work = self.snap.join(uid);
            std::fs::create_dir_all(&work).expect("snapshot");
            work
        }
    }

    /// The seed, snapshot root and log directory laid out under `root`.
    fn scratch(root: &Path) -> Scratch {
        let layout = Scratch {
            seed: root.join("seed"),
            snap: root.join("snap"),
            log: root.join("meta").join(LOG_FILE),
        };
        std::fs::create_dir_all(&layout.seed).expect("seed");
        std::fs::create_dir_all(&layout.snap).expect("snap");
        std::fs::create_dir_all(root.join("meta")).expect("meta");
        layout
    }

    /// Writes an unfinished transaction — logged, not yet applied — over one file.
    fn unfinished_log(layout: &Scratch, uid: &str, path: &str, contents: &[u8]) {
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 1,
                    uid: uid.to_string(),
                    op_count: Some(1),
                    meta: meta(&format!("printf … > {path}")),
                },
                WalRecord::Move {
                    from: PathBuf::from(path),
                    to: PathBuf::from(path),
                    sha1: digest(contents),
                },
            ])
            .expect("append");
    }

    /// The caller's metadata sits at the top level of the `BEGIN` line, beside the framing fields
    /// and nothing else: this is the on-disk contract a caller's existing logs were written to.
    #[test]
    fn a_begin_record_flattens_the_callers_metadata() {
        let record: WalRecord<Meta> = WalRecord::Begin {
            seq: 3,
            uid: "job0".to_string(),
            op_count: Some(2),
            meta: meta("touch a"),
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["cmd", "op", "op_count", "principal", "seq", "uid"]);
        assert_eq!(object["op"], serde_json::json!("BEGIN"));

        let parsed: WalRecord<Meta> = serde_json::from_str(&json).expect("round-trip");
        let WalRecord::Begin {
            seq,
            uid,
            op_count,
            meta: parsed_meta,
        } = parsed
        else {
            panic!("expected a BEGIN record");
        };
        assert_eq!((seq, uid.as_str(), op_count), (3, "job0", Some(2)));
        assert_eq!(parsed_meta, meta("touch a"));
    }

    /// Paths are plain JSON strings, so a log written before they were typed still reads.
    #[test]
    fn operation_records_carry_paths_as_strings() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        let deep = layout.work("job0").join("src/new");
        std::fs::create_dir_all(&deep).expect("snapshot dirs");
        std::fs::write(deep.join("deep.txt"), b"fresh\n").expect("snapshot file");

        apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            1,
            &meta("printf fresh > src/new/deep.txt"),
            &[CommitOp::Write(PathBuf::from("src/new/deep.txt"))],
        )
        .expect("apply");

        let text = std::fs::read_to_string(&layout.log).expect("read log");
        let moved = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse line"))
            .find(|line| line["op"] == serde_json::json!("MOVE"))
            .expect("a MOVE line");
        assert_eq!(moved["from"], serde_json::json!("src/new/deep.txt"));
        assert_eq!(moved["to"], serde_json::json!("src/new/deep.txt"));
        assert_eq!(
            std::fs::read(layout.seed.join("src/new/deep.txt")).expect("read the seed"),
            b"fresh\n"
        );
    }

    /// A path JSON cannot carry fails at serialization, which is *before* the seed is touched.
    #[test]
    fn a_non_utf8_path_fails_before_the_seed_is_touched() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        let name = OsStr::from_bytes(b"bad\xff");
        std::fs::write(layout.work("job0").join(name), b"x\n").expect("snapshot file");

        let error = apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            1,
            &meta("printf x > ?"),
            &[CommitOp::Write(PathBuf::from(name))],
        )
        .expect_err("an unencodable path cannot be logged");
        assert!(
            matches!(&error, Error::Wal(message) if message.starts_with("serialize record:")),
            "got {error:?}"
        );
        assert!(
            !layout.seed.join(name).exists(),
            "the seed is untouched when the intent could not be made durable"
        );
    }

    /// The log's whole reason to exist: the record is durable, the seed is not yet whole, and the
    /// next startup finishes it — and hands the caller back what it may still owe a history entry.
    #[test]
    fn an_unfinished_transaction_is_replayed_on_recover() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::write(layout.work("job0").join("a.txt"), b"recovered\n")
            .expect("snapshot file");
        unfinished_log(&layout, "job0", "a.txt", b"recovered\n");

        let recovered: Vec<Transaction<Meta>> = recover(&layout.seed, &layout.snap, &layout.log).expect("recover");
        assert_eq!(
            std::fs::read(layout.seed.join("a.txt")).expect("read the seed"),
            b"recovered\n",
            "the interrupted move reached the seed"
        );
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, 1);
        assert_eq!(recovered[0].uid, "job0");
        assert_eq!(recovered[0].meta, meta("printf … > a.txt"));
        assert_eq!(
            recovered[0].ops,
            vec![CommitOp::Write(PathBuf::from("a.txt"))],
            "the caller gets the operations back so it can re-derive what it lost"
        );

        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log)
                .expect("read the log");
        assert!(
            matches!(records.last(), Some(WalRecord::End { seq: 1 })),
            "and the transaction is closed: {records:?}"
        );

        let again: Vec<Transaction<Meta>> = recover(&layout.seed, &layout.snap, &layout.log).expect("recover again");
        assert_eq!(
            again.len(),
            1,
            "a finished transaction is still reported; only the caller knows what it remembers"
        );
    }

    /// The case the recorded `sha1` exists for: the transaction did reach the seed, the crash beat
    /// its `End`, and the snapshot it came from has since been swept. The content is proof enough.
    #[test]
    fn a_replayed_move_whose_snapshot_is_gone_is_a_no_op() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        unfinished_log(&layout, "job0", "a.txt", b"recovered\n");
        std::fs::write(layout.seed.join("a.txt"), b"recovered\n").expect("seed file");
        std::fs::remove_dir_all(layout.work("job0")).expect("sweep the snapshot");

        let recovered: Vec<Transaction<Meta>> = recover(&layout.seed, &layout.snap, &layout.log).expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            std::fs::read(layout.seed.join("a.txt")).expect("read the seed"),
            b"recovered\n",
            "the seed already carried the record's content, so nothing was rewritten"
        );
    }

    /// A counted prefix that lacks operations is abandoned without touching the seed, and is not
    /// reported to the caller either — nothing about it was ever durable.
    #[test]
    fn an_incomplete_counted_intent_is_abandoned() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::write(layout.work("job0").join("a.txt"), b"not durable\n")
            .expect("snapshot file");
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 1,
                    uid: "job0".to_string(),
                    op_count: Some(2),
                    meta: meta("write two files"),
                },
                WalRecord::Move {
                    from: PathBuf::from("a.txt"),
                    to: PathBuf::from("a.txt"),
                    sha1: digest(b"not durable\n"),
                },
            ])
            .expect("append prefix");

        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("abandon incomplete intent");
        assert!(recovered.is_empty());
        assert!(!layout.seed.join("a.txt").exists());
        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read WAL");
        assert!(!matches!(records.last(), Some(WalRecord::End { .. })));
    }

    /// Completed legacy records are still reported, but unfinished ones cannot be guessed.
    #[test]
    fn legacy_recovery_requires_an_end_record() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let root = temp.path();
        let completed = scratch(&root.join("completed"));
        std::fs::write(completed.seed.join("a.txt"), b"applied\n").expect("seed file");
        JsonLog::open(&completed.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 1,
                    uid: "job0".to_string(),
                    op_count: None,
                    meta: meta("legacy complete"),
                },
                WalRecord::Move {
                    from: PathBuf::from("a.txt"),
                    to: PathBuf::from("a.txt"),
                    sha1: digest(b"applied\n"),
                },
                WalRecord::End { seq: 1 },
            ])
            .expect("append completed legacy transaction");
        let recovered: Vec<Transaction<Meta>> =
            recover(&completed.seed, &completed.snap, &completed.log).expect("recover completed legacy transaction");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, 1);

        let unfinished = scratch(&root.join("unfinished"));
        std::fs::write(unfinished.work("job0").join("a.txt"), b"retained\n")
            .expect("snapshot file");
        JsonLog::open(&unfinished.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 1,
                    uid: "job0".to_string(),
                    op_count: None,
                    meta: meta("legacy unfinished"),
                },
                WalRecord::Move {
                    from: PathBuf::from("a.txt"),
                    to: PathBuf::from("a.txt"),
                    sha1: digest(b"retained\n"),
                },
            ])
            .expect("append unfinished legacy transaction");
        let error =
            recover::<Meta>(&unfinished.seed, &unfinished.snap, &unfinished.log).expect_err("unfinished legacy intent must fail closed");
        assert_eq!(
            error.to_string(),
            "write-ahead log failure: unfinished legacy transaction 1 has no operation count; recovery sources retained"
        );
        assert!(unfinished.work("job0").join("a.txt").exists());
        assert!(!unfinished.seed.join("a.txt").exists());
    }

    /// Declared framing mismatches are corruption even when the transaction carries an END.
    #[test]
    fn a_finished_transaction_with_the_wrong_count_is_rejected() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 7,
                    uid: "job0".to_string(),
                    op_count: Some(1),
                    meta: meta("mismatched"),
                },
                WalRecord::End { seq: 7 },
            ])
            .expect("append mismatched transaction");
        let error = recover::<Meta>(&layout.seed, &layout.snap, &layout.log).expect_err("mismatched framing must fail");
        assert_eq!(
            error.to_string(),
            "write-ahead log failure: transaction 7 declares 1 operations but contains 0"
        );
    }

    /// A session that never published anything — no log at all, or a log that exists and is empty
    /// — has no history to hand back, and recovery is not an error there.
    #[test]
    fn an_absent_or_empty_log_recovers_nothing() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("no log at all");
        assert!(recovered.is_empty());

        std::fs::write(&layout.log, b"").expect("empty log");
        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("an empty log");
        assert!(recovered.is_empty());
    }

    /// A line that is durable and unparsable is corruption: recovery stops rather than publishing
    /// a transaction it only half understands.
    #[test]
    fn a_corrupt_log_line_stops_recovery() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::write(&layout.log, b"{\"op\":\"NOPE\"}\n").expect("write a corrupt log");

        let error = recover::<Meta>(&layout.seed, &layout.snap, &layout.log)
            .expect_err("an unparsable record is corruption");
        assert!(
            matches!(&error, Error::Wal(message) if message.starts_with("corrupt record ")),
            "got {error:?}"
        );
    }

    /// A transaction the log describes as finished is reported but never re-applied: its snapshot
    /// is long swept, and replaying it would fail on a source that is legitimately gone.
    #[test]
    fn a_finished_transaction_is_reported_without_being_reapplied() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 4,
                    uid: "swept".to_string(),
                    op_count: Some(1),
                    meta: meta("printf … > a.txt"),
                },
                WalRecord::Move {
                    from: PathBuf::from("a.txt"),
                    to: PathBuf::from("a.txt"),
                    sha1: digest(b"published\n"),
                },
                WalRecord::End { seq: 4 },
            ])
            .expect("append a finished transaction");

        let recovered: Vec<Transaction<Meta>> = recover(&layout.seed, &layout.snap, &layout.log)
            .expect("a finished transaction needs no source");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, 4);
        assert_eq!(
            recovered[0].ops,
            vec![CommitOp::Write(PathBuf::from("a.txt"))]
        );
        assert!(
            !layout.seed.join("a.txt").exists(),
            "nothing was replayed into the seed"
        );
    }

    /// A removal is replayed like a move is: the seed has to reach the state the durable intent
    /// described, whichever kind of operation was interrupted.
    #[test]
    fn an_unfinished_delete_is_replayed() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::create_dir_all(layout.seed.join("src")).expect("seed dirs");
        std::fs::write(layout.seed.join("src/gone.txt"), b"bye\n").expect("seed file");
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Begin {
                    seq: 1,
                    uid: "job0".to_string(),
                    op_count: Some(1),
                    meta: meta("rm src/gone.txt"),
                },
                WalRecord::Delete {
                    path: PathBuf::from("src/gone.txt"),
                },
            ])
            .expect("append an unfinished removal");

        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("recover");
        assert!(!layout.seed.join("src/gone.txt").exists());
        assert_eq!(
            recovered[0].ops,
            vec![CommitOp::Remove(PathBuf::from("src/gone.txt"))]
        );
        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read the log");
        assert!(matches!(records.last(), Some(WalRecord::End { seq: 1 })));
    }

    /// An operation ahead of every `BEGIN` belongs to no transaction. It describes nothing the
    /// caller ever committed to, so it is dropped rather than applied.
    #[test]
    fn operations_before_any_begin_are_ignored() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::write(layout.work("job0").join("real.txt"), b"real\n").expect("snapshot file");
        JsonLog::open(&layout.log)
            .expect("open log")
            .append(&[
                WalRecord::Delete {
                    path: PathBuf::from("orphan.txt"),
                },
                WalRecord::Begin {
                    seq: 1,
                    uid: "job0".to_string(),
                    op_count: Some(1),
                    meta: meta("printf real > real.txt"),
                },
                WalRecord::Move {
                    from: PathBuf::from("real.txt"),
                    to: PathBuf::from("real.txt"),
                    sha1: digest(b"real\n"),
                },
            ])
            .expect("append an orphaned record ahead of a transaction");
        std::fs::write(layout.seed.join("orphan.txt"), b"untouched\n").expect("seed file");

        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].ops,
            vec![CommitOp::Write(PathBuf::from("real.txt"))],
            "the orphan joins no transaction"
        );
        assert!(
            layout.seed.join("orphan.txt").exists(),
            "and it is never applied"
        );
    }

    /// A command that changed nothing still frames a transaction: the sequence number is consumed
    /// and the caller's metadata is recorded, while the seed is left exactly as it was.
    #[test]
    fn a_transaction_with_no_operations_is_framed_and_changes_nothing() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::write(layout.seed.join("a.txt"), b"seed\n").expect("seed file");

        apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            1,
            &meta("true"),
            &[],
        )
        .expect("apply an empty transaction");

        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read the log");
        assert!(
            matches!(
                records.as_slice(),
                [WalRecord::Begin { seq: 1, .. }, WalRecord::End { seq: 1 }]
            ),
            "got {records:?}"
        );
        assert_eq!(
            std::fs::read(layout.seed.join("a.txt")).expect("read the seed"),
            b"seed\n"
        );

        let recovered: Vec<Transaction<Meta>> =
            recover(&layout.seed, &layout.snap, &layout.log).expect("recover");
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].ops.is_empty());
        assert_eq!(recovered[0].meta, meta("true"));
    }

    /// The one state a replay cannot resolve: the source is swept and the seed does not carry the
    /// content either. Failing loudly is the only honest answer — the seed is neither before nor
    /// after the transaction.
    #[test]
    fn a_move_with_neither_source_nor_published_content_fails() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        unfinished_log(&layout, "job0", "a.txt", b"recovered\n");
        std::fs::write(layout.seed.join("a.txt"), b"something else\n").expect("stale seed file");
        std::fs::remove_dir_all(layout.work("job0")).expect("sweep the snapshot");

        let error = recover::<Meta>(&layout.seed, &layout.snap, &layout.log)
            .expect_err("an unresolvable move must fail");
        assert!(
            matches!(&error, Error::Wal(message)
                if message.contains("is gone and")
                    && message.contains("does not carry its content")),
            "got {error:?}"
        );
    }

    /// A symlink is published as a link, and the hash the log records for it is the hash of its
    /// target — the only content a link has.
    #[test]
    fn a_symlink_is_published_and_hashed_by_its_target() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::os::unix::fs::symlink("pointee.txt", layout.work("job0").join("link"))
            .expect("snapshot symlink");

        apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            1,
            &meta("ln -s pointee.txt link"),
            &[CommitOp::Write(PathBuf::from("link"))],
        )
        .expect("apply");

        assert_eq!(
            std::fs::read_link(layout.seed.join("link")).expect("read the published link"),
            Path::new("pointee.txt")
        );
        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read the log");
        let moved = records
            .iter()
            .find_map(|record| match record {
                WalRecord::Move { sha1, .. } => Some(sha1.clone()),
                _ => None,
            })
            .expect("a MOVE record");
        assert_eq!(moved, digest(b"pointee.txt"));
    }

    /// An entry with no content — a socket here — hashes as empty and cannot be copied. The
    /// transaction fails on the copy instead of publishing a file that is not the entry.
    #[test]
    fn an_entry_with_no_content_hashes_empty_and_is_not_published() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        let listener =
            UnixListener::bind(layout.work("job0").join("sock")).expect("snapshot socket");
        drop(listener);

        let outcome = apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            1,
            &meta("nc -lU sock"),
            &[CommitOp::Write(PathBuf::from("sock"))],
        );
        assert!(matches!(outcome, Err(Error::Io(_))), "got {outcome:?}");
        assert!(
            !layout.seed.join("sock").exists(),
            "no stand-in file is left in the seed"
        );

        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read the log");
        let moved = records
            .iter()
            .find_map(|record| match record {
                WalRecord::Move { sha1, .. } => Some(sha1.clone()),
                _ => None,
            })
            .expect("a MOVE record");
        assert_eq!(
            moved,
            digest(&[]),
            "an entry with nothing to read hashes as empty"
        );
    }

    /// A deletion is published like a write is: the log carries a `DELETE` record for it, and the
    /// seed loses the path — including the directories the loss empties.
    #[test]
    fn a_removal_is_logged_and_published() {
        let temp = tempfile::tempdir().expect("scratch directory");
        let layout = scratch(temp.path());
        std::fs::create_dir_all(layout.seed.join("src")).expect("seed dirs");
        std::fs::write(layout.seed.join("src/gone.txt"), b"bye\n").expect("seed file");

        apply(
            &layout.seed,
            &layout.work("job0"),
            &layout.log,
            "job0",
            2,
            &meta("rm src/gone.txt"),
            &[CommitOp::Remove(PathBuf::from("src/gone.txt"))],
        )
        .expect("apply");

        assert!(!layout.seed.join("src").exists(), "the emptied parent goes too");
        let records: Vec<WalRecord<Meta>> =
            JsonLog::<WalRecord<Meta>>::read(&layout.log).expect("read the log");
        assert!(
            records.iter().any(|record| matches!(
                record,
                WalRecord::Delete { path } if path == Path::new("src/gone.txt")
            )),
            "got {records:?}"
        );
    }
}
