//! The write-ahead log: a job's frozen snapshot into the seed.
//!
//! One append-only log, at `meta/wal.jsonl`, holds every transaction. The order is the protocol:
//! the whole transaction is logged and fsynced first, then each record is applied to the seed, then
//! `End`. A crash before `End` leaves a log a replay can finish; a crash after it leaves nothing to
//! do in the seed.
//!
//! Everything that can be decided before the intent is written is decided there — by [`prepare`],
//! which appends nothing: whether each operation is representable, whether the tree it lands in is
//! the seed's own filesystem, mount and subvolume, whether the seed's directories can take it,
//! and what exactly every write will carry. Only [`PreparedTransaction::apply`] writes, and its
//! first write is the intent: any failure from it may leave a durable intent behind, and the
//! seed must then be recovered before anything else is published into it.
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

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::ops::Bound;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Access, AtFlags, CWD, FileType, Mode as RawMode, OFlags, Uid};
use rustix::io::Errno;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::diff::CommitOp;
use crate::error::{ABSENT, Error, Tolerate};
use crate::log::{self, JsonLog, Removal};
use crate::tree::{Kind, Lookup, Tree, by_depth, io_error, normal, split};
use crate::types::{ContentHash, EntryKind, Mode, Seq, SourceUid, Staging};

/// Log file name under a session's `meta/` directory.
pub const LOG_FILE: &str = "wal.jsonl";

/// One line of the log.
///
/// `M` is the caller's per-transaction metadata, flattened into the `BEGIN` line.
/// Framing fields and each move's kind are required. Unknown operation fields are refused rather
/// than interpreted as another record format; directories use their explicit operations.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "UPPERCASE", deny_unknown_fields)]
pub enum WalRecord<M> {
    /// Opens a transaction: the snapshot its content comes from, plus the caller's metadata.
    Begin {
        /// Sequence number this transaction will occupy.
        seq: Seq,
        /// The snapshot that holds the content, so a replay can find it.
        uid: SourceUid,
        /// Number of operation records in the durable intent.
        op_count: usize,
        /// The directory inside the seed this transaction stages its writes in.
        staging: Staging,
        /// Whatever the caller wants recorded with this transaction.
        #[serde(flatten)]
        meta: M,
    },
    /// Replace the seed's non-directory at a path with the snapshot's file or symlink there.
    Move {
        /// Source, relative to the snapshot root.
        from: PathBuf,
        /// Destination, relative to the seed.
        to: PathBuf,
        /// Hash of the content being moved, so a replay can verify a source before copying it,
        /// and tell an applied record from an interrupted one after the source was swept.
        sha1: ContentHash,
        /// What is moved: `file` with its `mode`, or `symlink` without one.
        kind: EntryKind,
        /// A file's permission bits; absent for a symlink.
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
    },
    /// Delete the non-directory at a path of the seed.
    Delete {
        /// Seed-relative path.
        path: PathBuf,
    },
    /// Create a directory in the seed, owner-only until the transaction's end gives it `mode`.
    Mkdir {
        /// Seed-relative path.
        path: PathBuf,
        /// The directory's final permission bits.
        mode: Mode,
    },
    /// Give an existing directory of the seed its final mode at the transaction's end. Also logged
    /// for a directory whose mode does not change but that the transaction must open up to the
    /// owner while it runs: this record is what restores it.
    Chmoddir {
        /// Seed-relative path.
        path: PathBuf,
        /// The directory's final permission bits.
        mode: Mode,
    },
    /// Delete an empty directory of the seed — never its contents.
    Rmdir {
        /// Seed-relative path.
        path: PathBuf,
    },
    /// Every preceding record of this transaction has been applied.
    End {
        /// The sequence number opened by `Begin`.
        seq: Seq,
    },
}

impl<M> WalRecord<M> {
    /// The commit operation this record performs, or `None` for the framing records.
    ///
    /// Takes the record by value: the path each operation names is moved out rather than cloned.
    /// What is left behind — a source path, a content hash — owns nothing whose destructor
    /// releases a resource beyond its own allocation, so dropping it here reorders nothing
    /// observable.
    fn into_operation(self) -> Option<CommitOp> {
        Some(match self {
            Self::Move { to, .. } => CommitOp::Write(to),
            Self::Delete { path } => CommitOp::Remove(path),
            Self::Mkdir { path, mode } => CommitOp::CreateDirectory { path, mode },
            Self::Chmoddir { path, mode } => CommitOp::SetDirectoryMode { path, mode },
            Self::Rmdir { path } => CommitOp::RemoveDirectory(path),
            Self::Begin { .. } | Self::End { .. } => return None,
        })
    }

    /// The directory this record gives a final mode at its transaction's end.
    fn final_mode(&self) -> Option<(&Path, Mode)> {
        match self {
            Self::Mkdir { path, mode } | Self::Chmoddir { path, mode } => Some((path, *mode)),
            _ => None,
        }
    }

    /// The seed path this record puts a non-directory at or takes one from — a move's
    /// destination, a removal's path: what reconciliation checks the seed for.
    fn leaf(&self) -> Option<&Path> {
        match self {
            Self::Move { to: path, .. } | Self::Delete { path } => Some(path),
            _ => None,
        }
    }

    /// Whether this record copies content through the transaction's staging directory.
    const fn stages(&self) -> bool {
        matches!(self, Self::Move { .. })
    }
}

/// What a `MOVE` record's kind and mode say it publishes.
#[derive(Clone, Copy, Debug)]
enum Moved {
    /// A regular file with these permission bits.
    File(Mode),
    /// A symbolic link.
    Symlink,
}

impl Moved {
    /// The meaning of a record's `kind` and `mode`, or `None` for a combination no writer
    /// produces.
    const fn of(kind: EntryKind, mode: Option<Mode>) -> Option<Self> {
        match (kind, mode) {
            (EntryKind::File, Some(mode)) => Some(Self::File(mode)),
            (EntryKind::Symlink, None) => Some(Self::Symlink),
            (EntryKind::File, None) | (EntryKind::Symlink, Some(_)) => None,
        }
    }

    /// The meaning of the `kind` and `mode` a move to `to` logged; a combination no writer
    /// produces is refused.
    fn logged(to: &Path, kind: EntryKind, mode: Option<Mode>) -> Result<Self, Error> {
        Self::of(kind, mode)
            .ok_or_else(|| Error::Wal(format!("{} has an impossible record", to.display())))
    }

    /// The entry kind a move of this meaning expects at its source.
    const fn kind(self) -> Kind {
        match self {
            Self::File(_) => Kind::File,
            Self::Symlink => Kind::Symlink,
        }
    }
}

/// One logged transaction, as [`recover`] reads it back.
#[derive(Debug)]
pub struct Transaction<M> {
    /// Sequence number the transaction occupies.
    pub seq: Seq,
    /// The snapshot its content came from.
    pub uid: SourceUid,
    /// The metadata [`prepare`] recorded with it.
    pub meta: M,
    /// Its operations, in log order — including the final mode of every directory it had to open
    /// up to the owner while it ran, as a [`CommitOp::SetDirectoryMode`] to the mode it kept.
    pub ops: Vec<CommitOp>,
}

/// One transaction as the log's raw lines describe it, before it is validated or replayed.
struct Frame<M> {
    /// Sequence number the transaction occupies.
    seq: Seq,
    /// The snapshot its content comes from.
    uid: SourceUid,
    /// The caller's metadata.
    meta: M,
    /// Declared operation count.
    op_count: usize,
    /// Declared staging directory.
    staging: Staging,
    /// Byte offset of its `BEGIN` line.
    offset: u64,
    /// Its operation records, in log order.
    records: Vec<WalRecord<M>>,
    /// Whether the log carries this transaction's `End`.
    finished: bool,
}

impl<M> Frame<M> {
    /// Whether this is a counted intent whose operations did not all reach the log: nothing about
    /// it was ever durable, so it is neither replayed nor reported.
    const fn abandoned(&self) -> bool {
        !self.finished && self.records.len() < self.op_count
    }

    /// Fails unless the operations present are exactly the count declared.
    fn check_count(&self) -> Result<(), Error> {
        if self.op_count != self.records.len() {
            return Err(Error::Wal(format!(
                "transaction {} declares {} operations but contains {}",
                self.seq,
                self.op_count,
                self.records.len()
            )));
        }
        Ok(())
    }

    /// Fails unless every record of this frame is one a writer of this log produces: plain
    /// relative paths, a consistent kind and mode for every move, and the staging directory its
    /// sequence number and uid name.
    fn validate(&self) -> Result<(), Error> {
        if self.staging != Staging::of(self.seq, &self.uid) {
            return Err(Error::Wal(format!(
                "transaction {} declares staging directory {}, which is not its own",
                self.seq, self.staging
            )));
        }
        for record in &self.records {
            let paths: [&Path; 2] = match record {
                WalRecord::Move {
                    from,
                    to,
                    kind,
                    mode,
                    ..
                } => {
                    if Moved::of(*kind, *mode).is_none() {
                        return Err(Error::Wal(format!(
                            "transaction {} moves {} with kind {kind:?} and mode {mode:?}, \
                             which no writer produces",
                            self.seq,
                            to.display()
                        )));
                    }
                    [from, to]
                }
                WalRecord::Delete { path }
                | WalRecord::Rmdir { path }
                | WalRecord::Mkdir { path, .. }
                | WalRecord::Chmoddir { path, .. } => [path, path],
                WalRecord::Begin { .. } | WalRecord::End { .. } => {
                    return Err(Error::Wal(format!(
                        "transaction {} holds a framing record among its operations",
                        self.seq
                    )));
                }
            };
            if let Some(path) = paths.into_iter().find(|path| !normal(path)) {
                return Err(Error::Wal(format!(
                    "transaction {} names {}, which is not a relative path of plain components",
                    self.seq,
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Writes this transaction back as the log frames it: its `BEGIN`, its operations and its
    /// `END`.
    fn write(&self, out: &mut dyn std::io::Write) -> Result<(), Error>
    where
        M: Serialize,
    {
        write_intent(&mut *out, self.seq, &self.uid, &self.meta, &self.records)?;
        log::write_record(out, &WalRecord::<()>::End { seq: self.seq })
    }

    /// The transaction this frame describes, as [`recover`] returns it.
    fn into_transaction(self) -> Transaction<M> {
        Transaction {
            seq: self.seq,
            uid: self.uid,
            meta: self.meta,
            ops: self
                .records
                .into_iter()
                .filter_map(WalRecord::into_operation)
                .collect(),
        }
    }
}

/// A transaction checked, fingerprinted and serialized against the seed, with nothing written
/// yet: the product of [`prepare`], published by [`PreparedTransaction::apply`].
#[must_use = "a prepared transaction publishes nothing until it is applied"]
pub struct PreparedTransaction<'a> {
    /// The seed, opened.
    seed: Tree<'a>,
    /// The tree the writes copy from, opened; absent for a transaction without operations.
    work: Option<Tree<'a>>,
    /// The directory the writes land through.
    staging: Staging,
    /// The operation records, in the order they apply.
    records: Vec<WalRecord<()>>,
    /// The encoded frame: `BEGIN`, every operation and `END`.
    frame: Vec<u8>,
    /// Where the frame's `END` starts: everything before it is the intent.
    intent: usize,
    /// The log, opened for appending.
    log: JsonLog<WalRecord<()>>,
    /// The buffer every file is read through.
    buffer: Vec<u8>,
}

/// Writes a transaction's intent onto `writer`: its `BEGIN`, carrying `meta`, declaring `records`
/// and naming the staging directory `seq` and `uid` name — the only one a valid frame has — then
/// `records` in order. The header and operation metadata are independent: a fresh intent's
/// operations carry none, a recovered frame's carry the caller's. `END` and durability are the
/// caller's.
///
/// # Errors
///
/// Fails as [`log::write_record`] does, with the first record that cannot be written.
fn write_intent<W: std::io::Write + ?Sized, H: Serialize, R: Serialize>(
    writer: &mut W,
    seq: Seq,
    uid: &SourceUid,
    meta: &H,
    records: &[WalRecord<R>],
) -> Result<(), Error> {
    log::write_record(
        &mut *writer,
        &WalRecord::Begin {
            seq,
            uid: uid.clone(),
            op_count: records.len(),
            staging: Staging::of(seq, uid),
            meta,
        },
    )?;
    records
        .iter()
        .try_for_each(|record| log::write_record(&mut *writer, record))
}

/// Checks one transaction against the seed and serializes it, appending nothing.
///
/// `seed` is the tree the transaction publishes into, `work` the frozen tree every
/// [`CommitOp::Write`] reads its content from, and `log` the path of the append-only log. Callers
/// pass a `work` of `snap.join(uid)` — the same shape [`recover`] resolves a logged transaction's
/// source with. A transaction without operations reads nothing from `work`, which need not exist.
///
/// Everything predictable is refused here, before any intent exists and without touching the
/// seed: a path that is not relative and plain, or that names the transaction's staging
/// directory; a staging directory name already taken; operations that contradict each other or
/// the seed — a removal of a directory as a leaf, a directory removal of anything but an emptied
/// directory, a creation over an existing entry or beneath a non-directory; a source that is a
/// fifo, socket or device, or a regular file with more than one link, whose links a copy would
/// break; and any entry that lies across a nested subvolume, a mount or a bind mount from the tree
/// it belongs to. A directory whose contents change but that this user cannot write gets a
/// logged final mode, so it can be opened up to its owner while the transaction runs and restored
/// after; one the user does not own is refused.
///
/// The operations are logged in the order they can apply: removals deepest first, directory
/// creations shallowest first, writes shallowest first, final directory modes deepest first.
///
/// # Errors
///
/// Fails with [`Error::Wal`] for every refusal above and for a record that cannot be serialized —
/// a path no JSON string can carry fails here — and with [`Error::Io`] when either tree cannot be
/// read or the log cannot be opened.
pub fn prepare<'a, M: Serialize>(
    seed: &'a Path,
    work: &'a Path,
    log: &'a Path,
    uid: &SourceUid,
    seq: Seq,
    meta: &M,
    ops: &[CommitOp],
) -> Result<PreparedTransaction<'a>, Error> {
    let staging = Staging::of(seq, uid);
    let seed = Tree::open(seed)?;
    let mut buffer = vec![0; log::CHUNK];
    let (work, records) = if ops.is_empty() {
        (None, Vec::new())
    } else {
        let work = Tree::open(work)?;
        let records = Planner {
            seed: &seed,
            work: &work,
            staging: &staging,
            planned: BTreeMap::new(),
            changed: BTreeSet::new(),
            buffer: &mut buffer,
        }
        .plan(ops)?;
        (Some(work), records)
    };

    let mut frame = Vec::new();
    write_intent(&mut frame, seq, uid, meta, &records)?;
    let intent = frame.len();
    log::write_record(&mut frame, &WalRecord::<()>::End { seq })?;
    Ok(PreparedTransaction {
        log: JsonLog::open(log)?,
        seed,
        work,
        staging,
        records,
        frame,
        intent,
        buffer,
    })
}

impl PreparedTransaction<'_> {
    /// Publishes the transaction: the intent, fsynced; then every operation; then `END`.
    ///
    /// The staging directory is created only once the intent is durable, and it is gone again —
    /// and every directory the transaction changed, the seed's root included, fsynced — before
    /// `END` is written. A transaction without operations is its intent and its `END` in one
    /// write.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Io`] or [`Error::Wal`] for any failure at all — and since the intent
    /// is the first thing written, any failure may leave it durable with the seed partly changed.
    /// Nothing may be published into the seed after that until [`recover`] has run over it.
    pub fn apply(mut self) -> Result<(), Error> {
        if self.records.is_empty() {
            return self.log.write_durably(&self.frame);
        }
        let (intent, end) = self.frame.split_at(self.intent);
        self.log.write_durably(intent)?;
        crash_point(Crash::Intent);
        Engine {
            seed: &self.seed,
            work: self.work.as_ref(),
            ops: &self.records,
            done: &[],
            dirty: BTreeSet::new(),
            buffer: &mut self.buffer,
        }
        .run(&self.staging)?;
        crash_point(Crash::BeforeEnd);
        self.log.write_durably(end)?;
        crash_point(Crash::End);
        Ok(())
    }
}

/// Finishes the unfinished transaction, if any, and returns every transaction the log describes
/// whole.
///
/// Called once at startup, before anything reads the seed and *before* any snapshot sweep: an
/// unfinished transaction's content lives in `snap/<uid>`. A finished transaction is returned too,
/// because the protocol writes the seed and then whatever the caller records afterwards, and a
/// crash between the two leaves this log as the only evidence of what was published.
///
/// A log holding a complete line that cannot be decoded — a record of a schema no current writer
/// produces, or metadata `M` does not accept — is discarded whole before anything else is looked
/// at: it is replaced, atomically and durably, by an empty log, whatever valid records surround
/// that line. Nothing is replayed, neither tree is touched, and no transaction is returned.
///
/// The log is validated whole before anything is replayed: every frame's framing, every path and
/// every record's shape; then every source of the transaction to finish is checked against the
/// kind, hash and mode its record logged, and a source that is gone is accepted only when the
/// seed already carries exactly what it would have written. A counted intent whose operations did
/// not all reach the log is abandoned wherever it appears — neither replayed nor returned — and one
/// at the end of the log is truncated away. A staging directory a frame names is cleaned — only
/// its own temporaries, and only that directory — and never anything else in the seed.
///
/// Replay is idempotent, including across a change of kind that already happened: a removal that
/// finds its path — or an ancestor — already turned into what a later operation of the same
/// transaction makes it is done, and is skipped without following anything.
///
/// Then the log is reconciled with the seed. Every path a move publishes, a removal deletes or
/// `metadata_paths` names for a transaction's metadata is expected to exist, unless a later
/// operation of the log explains its absence: a removal of it or of a directory above it, or a
/// move of a non-directory over a directory above it — until a move at or beneath it makes it
/// exist again. A path expected to exist that the seed does not have was deleted outside this log,
/// and its whole history goes: every move to it and removal of it, from every transaction, and
/// whatever `prune_metadata` removes from each transaction's metadata given every such path. A
/// transaction left with no operation is kept only when `prune_metadata` says its metadata still
/// needs it. The log is then replaced, whole and atomically, by the transactions that remain —
/// before any is returned. An absence the log itself explains, and a log with nothing missing,
/// changes nothing. Content is not compared: this reconciles deletions, nothing else.
///
/// `M` is the caller's metadata type; recovery never looks inside it itself. `metadata_paths`
/// appends the seed-relative paths one transaction's metadata refers to onto an empty vector — a
/// path that is not relative and plain is ignored — and `prune_metadata` removes every reference
/// to the paths it is given, keeping the rest in order, and says whether what is left still needs
/// its transaction kept.
///
/// # Errors
///
/// Fails with [`Error::Wal`] — before any seed mutation — when a complete record cannot be
/// decoded as `M`'s log (an incompatible or corrupt log, which is left exactly as it was), when an
/// operation lies outside any transaction, an `END` matches no open transaction, a count does not
/// match, a transaction never finished yet another follows it, a record names what no writer
/// produces, a source differs from its record, or a staging directory holds anything but its own
/// temporaries; and during replay when a record can neither be applied nor recognized as already
/// applied. Fails with [`Error::Io`] when the log, the seed or a source cannot be read or written,
/// and when a path the log expects cannot be looked up for any reason but its absence, which
/// leaves the log as it was.
pub fn recover<M: Serialize + DeserializeOwned>(
    seed: &Path,
    snap: &Path,
    log: &Path,
    mut metadata_paths: impl FnMut(&M, &mut Vec<PathBuf>),
    mut prune_metadata: impl FnMut(&mut M, &BTreeSet<PathBuf>) -> bool,
) -> Result<Vec<Transaction<M>>, Error> {
    let records = log::read_records::<WalRecord<M>>(log)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let (mut frames, abandoned_tail) = frames(records)?;
    for frame in &frames {
        frame.validate()?;
    }
    if let Some(offset) = abandoned_tail {
        log::truncate(log, offset)?;
    }
    if frames.is_empty() {
        return Ok(Vec::new());
    }

    let seed = Tree::open(seed)?;
    for frame in frames.iter().filter(|frame| frame.finished) {
        clear_residue(&seed, &frame.staging, &frame.records)?;
    }
    if let Some(frame) = frames.last_mut().filter(|frame| !frame.finished) {
        let mut buffer = vec![0; log::CHUNK];
        let source = snap.join(&frame.uid);
        let work = Tree::open(&source).tolerate(&[std::io::ErrorKind::NotFound])?;
        let done = settle(&seed, work.as_ref(), &frame.records, &mut buffer)?;
        Engine {
            seed: &seed,
            work: work.as_ref(),
            ops: &frame.records,
            done: &done,
            dirty: BTreeSet::new(),
            buffer: &mut buffer,
        }
        .run(&frame.staging)?;
        JsonLog::<WalRecord<()>>::open(log)?.append(&[WalRecord::End { seq: frame.seq }])?;
        frame.finished = true;
    }

    let missing = missing(seed.path(), &frames, &mut metadata_paths)?;
    if !missing.is_empty() {
        frames.retain_mut(|frame| {
            frame
                .records
                .retain(|record| record.leaf().is_none_or(|path| !missing.contains(path)));
            prune_metadata(&mut frame.meta, &missing) || !frame.records.is_empty()
        });
        log::replace_log(log, |writer| {
            frames.iter().try_for_each(|frame| frame.write(writer))
        })?;
    }

    Ok(frames.into_iter().map(Frame::into_transaction).collect())
}

/// Every path the log expects the seed to have that `seed` does not.
///
/// Each path a move publishes, a removal deletes or a transaction's metadata names starts out
/// expected; then the operations of `frames`, in log order, say which of them the log itself
/// explains the absence of. What stays expected is looked up once, without following a final
/// symlink.
fn missing<M>(
    seed: &Path,
    frames: &[Frame<M>],
    metadata_paths: &mut impl FnMut(&M, &mut Vec<PathBuf>),
) -> Result<BTreeSet<PathBuf>, Error> {
    let mut expected = BTreeMap::<PathBuf, bool>::new();
    let mut named = Vec::new();
    for frame in frames {
        for path in frame.records.iter().filter_map(WalRecord::leaf) {
            if !expected.contains_key(path) {
                expected.insert(path.to_path_buf(), true);
            }
        }
        metadata_paths(&frame.meta, &mut named);
        // Popping moves each path out and keeps the scratch vector's capacity for the next frame;
        // the order they arrive in does not matter, every one of them is expected.
        while let Some(path) = named.pop() {
            if normal(&path) {
                expected.entry(path).or_insert(true);
            }
        }
    }

    for record in frames.iter().flat_map(|frame| &frame.records) {
        match record {
            WalRecord::Move { to, .. } => {
                // A write requires every directory above it to exist.
                for path in to.ancestors() {
                    if let Some(present) = expected.get_mut(path) {
                        *present = true;
                    }
                }
                absent_beneath(&mut expected, to);
            }
            WalRecord::Delete { path } => {
                if let Some(present) = expected.get_mut(path) {
                    *present = false;
                }
                absent_beneath(&mut expected, path);
            }
            _ => {}
        }
    }

    let mut missing = BTreeSet::new();
    for (path, _) in expected.into_iter().filter(|(_, present)| *present) {
        if seed
            .join(&path)
            .symlink_metadata()
            .tolerate(ABSENT)?
            .is_none()
        {
            missing.insert(path);
        }
    }
    Ok(missing)
}

/// Marks every path of `expected` strictly beneath `directory` as absent: whatever replaced or
/// removed `directory` took them with it.
fn absent_beneath(expected: &mut BTreeMap<PathBuf, bool>, directory: &Path) {
    // Paths order by component, so everything beneath `directory` follows it contiguously.
    for (_, present) in expected
        .range_mut::<Path, _>((Bound::Excluded(directory), Bound::Unbounded))
        .take_while(|(path, _)| path.starts_with(directory))
    {
        *present = false;
    }
}

/// Groups the log's records into transactions, refusing any framing a writer cannot produce.
///
/// Returns the finished transactions and the one unfinished but complete transaction a crash can
/// leave — only ever the last — in log order, plus the offset of a final abandoned intent to
/// truncate the log back to.
fn frames<M>(records: Vec<(u64, WalRecord<M>)>) -> Result<(Vec<Frame<M>>, Option<u64>), Error> {
    let mut frames = Vec::new();
    let mut open: Option<Frame<M>> = None;
    for (offset, record) in records {
        match record {
            WalRecord::Begin {
                seq,
                uid,
                op_count,
                staging,
                meta,
            } => {
                if let Some(previous) = open.take()
                    && !previous.abandoned()
                {
                    previous.check_count()?;
                    return Err(Error::Wal(format!(
                        "transaction {} never finished, yet transaction {seq} follows it",
                        previous.seq
                    )));
                }
                open = Some(Frame {
                    seq,
                    uid,
                    meta,
                    op_count,
                    staging,
                    offset,
                    records: Vec::new(),
                    finished: false,
                });
            }
            WalRecord::End { seq } => {
                let Some(mut frame) = open.take().filter(|frame| frame.seq == seq) else {
                    return Err(Error::Wal(format!(
                        "the END of transaction {seq} at byte {offset} matches no open transaction"
                    )));
                };
                frame.check_count()?;
                frame.finished = true;
                frames.push(frame);
            }
            operation => {
                let Some(frame) = open.as_mut() else {
                    return Err(Error::Wal(format!(
                        "the operation at byte {offset} belongs to no transaction"
                    )));
                };
                frame.records.push(operation);
                if frame.records.len() > frame.op_count {
                    frame.check_count()?;
                }
            }
        }
    }
    let mut abandoned_tail = None;
    if let Some(frame) = open {
        if frame.abandoned() {
            abandoned_tail = Some(frame.offset);
        } else {
            frames.push(frame);
        }
    }
    Ok((frames, abandoned_tail))
}

/// Checks every move of an unfinished transaction against the log before anything is replayed,
/// and says which of them are already in place.
///
/// A source that is present must be exactly what its record logged; one that is gone is accepted
/// only when the seed already carries that content at the destination, which is then marked done
/// so the replay neither needs the source nor undoes the destination.
fn settle<M>(
    seed: &Tree<'_>,
    work: Option<&Tree<'_>>,
    ops: &[WalRecord<M>],
    buffer: &mut [u8],
) -> Result<Vec<bool>, Error> {
    let mut done = Vec::with_capacity(ops.len());
    for op in ops {
        let WalRecord::Move {
            from,
            to,
            sha1,
            kind,
            mode,
        } = op
        else {
            done.push(false);
            continue;
        };
        let moved = Moved::logged(to, *kind, *mode)?;
        let source_path = || work.map_or_else(|| Path::new(""), Tree::path).join(from);
        let source = match work {
            Some(work) => Found::at(work, from, buffer)?,
            None => None,
        };
        if let Some(source) = source {
            if !source.carries(moved, sha1) {
                return Err(Error::Wal(format!(
                    "source {} differs from what the log recorded for it; recovery sources \
                     retained",
                    source_path().display()
                )));
            }
            done.push(false);
        } else if Found::at(seed, to, buffer)?.is_some_and(|target| target.carries(moved, sha1)) {
            done.push(true);
        } else {
            return Err(Error::Wal(format!(
                "source {} is gone and {} does not carry its content; the transaction can \
                 neither be completed nor undone",
                source_path().display(),
                seed.path().join(to).display()
            )));
        }
    }
    Ok(done)
}

/// What an existing file or symlink carries, as a record would describe it.
struct Found {
    /// Its kind.
    kind: Kind,
    /// Its content's hash, for a file or a symlink.
    hash: Option<ContentHash>,
    /// Its permission bits.
    mode: Mode,
}

impl Found {
    /// What is at `relative` in `tree`; `None` when nothing is, or a non-directory is in the way.
    fn at(tree: &Tree<'_>, relative: &Path, buffer: &mut [u8]) -> Result<Option<Self>, Error> {
        let Some((directory, node)) = tree.lookup(relative)? else {
            return Ok(None);
        };
        let (_, name) = split(relative)?;
        let hash = match node.kind {
            Kind::File => Some(EntryKind::File),
            Kind::Symlink => Some(EntryKind::Symlink),
            Kind::Directory | Kind::Special => None,
        }
        .map(|kind| hash_content(directory.as_fd(), name, kind, buffer))
        .transpose()?;
        Ok(Some(Self {
            kind: node.kind,
            hash,
            mode: node.mode,
        }))
    }

    /// Whether this is what a move of meaning `moved` and hash `hash` puts in place.
    fn carries(&self, moved: Moved, hash: &ContentHash) -> bool {
        let same = self.hash.as_ref() == Some(hash);
        match moved {
            Moved::File(mode) => self.kind == Kind::File && same && self.mode == mode,
            Moved::Symlink => self.kind == Kind::Symlink && same,
        }
    }
}

/// The hash of the content of the `kind` entry `name` of `directory`, read without following it:
/// a file's bytes, through `buffer`, or a symlink's raw target.
///
/// # Errors
///
/// Fails with [`Error::Io`] when the entry cannot be opened, read or resolved as `kind`.
fn hash_content(
    directory: BorrowedFd<'_>,
    name: &OsStr,
    kind: EntryKind,
    buffer: &mut [u8],
) -> Result<ContentHash, Error> {
    match kind {
        EntryKind::File => ContentHash::of_stream(
            File::from(log::open_regular(directory, name)?),
            None,
            buffer,
        ),
        EntryKind::Symlink => Ok(ContentHash::of(
            rustix::fs::readlinkat(directory, name, Vec::new())?.as_bytes(),
        )),
    }
}

/// What a path will be once the operations planned so far have applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Planned {
    /// Removed.
    Absent,
    /// A directory this transaction creates, and so owns and starts empty.
    Created,
    /// A file or symlink this transaction writes.
    Written,
}

/// What is at a path, planned or found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Nothing.
    Absent,
    /// A directory.
    Directory,
    /// A non-directory.
    Other,
}

/// The preflight of [`prepare`]: every check that can refuse a transaction before its intent.
struct Planner<'e, 'p> {
    /// The seed.
    seed: &'e Tree<'p>,
    /// The tree the writes copy from.
    work: &'e Tree<'p>,
    /// The transaction's staging directory.
    staging: &'e Staging,
    /// What the operations planned so far make of each path they name; any other path is as the
    /// seed has it.
    planned: BTreeMap<PathBuf, Planned>,
    /// Existing seed directories whose contents the plan changes.
    changed: BTreeSet<PathBuf>,
    /// The buffer every source is hashed through.
    buffer: &'e mut [u8],
}

/// A transaction's operations grouped by kind, each group in the order it applies, with every
/// path checked to be loggable and named consistently.
#[derive(Default)]
struct Ordered<'o> {
    /// Leaf removals (`false`) and directory removals (`true`), deepest first.
    removals: Vec<(&'o Path, bool)>,
    /// Directory creations, shallowest first.
    directories: Vec<(&'o Path, Mode)>,
    /// File and symlink writes, shallowest first.
    writes: Vec<&'o Path>,
    /// Final modes of existing directories.
    modes: BTreeMap<&'o Path, Mode>,
}

impl<'o> Ordered<'o> {
    /// Groups and orders `ops`, the operations of the transaction staging through `staging`.
    ///
    /// A path is removed at most once, created at most once, and given a final mode only when it
    /// is neither; a write replaces a non-directory by itself, so it is never also removed.
    fn of(ops: &'o [CommitOp], staging: &Staging) -> Result<Self, Error> {
        let mut ordered = Self::default();
        for op in ops {
            let path = op.path();
            if !normal(path) {
                return Err(Error::Wal(format!(
                    "{} is not a relative path of plain components",
                    path.display()
                )));
            }
            if path
                .components()
                .next()
                .is_some_and(|first| first.as_os_str() == OsStr::new(staging.as_str()))
            {
                return Err(Error::Wal(format!(
                    "{} lies in the transaction's own staging directory",
                    path.display()
                )));
            }
            match op {
                CommitOp::Remove(path) => ordered.removals.push((path, false)),
                CommitOp::RemoveDirectory(path) => ordered.removals.push((path, true)),
                CommitOp::CreateDirectory { path, mode } => {
                    ordered.directories.push((path, *mode));
                }
                CommitOp::Write(path) => ordered.writes.push(path),
                CommitOp::SetDirectoryMode { path, mode } => {
                    if ordered.modes.insert(path, *mode).is_some() {
                        return Err(twice(path));
                    }
                }
            }
        }

        let mut removed = BTreeSet::new();
        for (path, _) in &ordered.removals {
            if !removed.insert(*path) {
                return Err(twice(path));
            }
        }
        let mut created = BTreeSet::new();
        let creations = ordered.directories.iter().map(|(path, _)| *path);
        for path in creations.chain(ordered.writes.iter().copied()) {
            if !created.insert(path) {
                return Err(twice(path));
            }
        }
        if let Some(path) = ordered
            .writes
            .iter()
            .find(|path| ordered.removals.contains(&(**path, false)))
            .or_else(|| {
                ordered
                    .modes
                    .keys()
                    .find(|path| removed.contains(*path) || created.contains(*path))
            })
        {
            return Err(twice(path));
        }
        let Self {
            removals,
            directories,
            writes,
            ..
        } = &mut ordered;
        removals.sort_by(|(left, _), (right, _)| by_depth(left, right, true));
        directories.sort_by(|(left, _), (right, _)| by_depth(left, right, false));
        writes.sort_by(|left, right| by_depth(left, right, false));
        Ok(ordered)
    }
}

impl Planner<'_, '_> {
    /// Checks `ops` against both trees and returns their records, in the order they apply.
    fn plan(mut self, ops: &[CommitOp]) -> Result<Vec<WalRecord<()>>, Error> {
        let ordered = Ordered::of(ops, self.staging)?;
        let staging = OsStr::new(self.staging.as_str());
        if self
            .seed
            .entry(self.seed.root(), staging, Path::new(staging))?
            .is_some()
        {
            return Err(Error::Wal(format!(
                "{} already holds {}, the transaction's staging directory",
                self.seed.path().display(),
                self.staging
            )));
        }

        let mut records = Vec::with_capacity(ops.len());
        for (path, directory) in ordered.removals {
            self.remove(path, directory)?;
            let path = path.to_path_buf();
            records.push(if directory {
                WalRecord::Rmdir { path }
            } else {
                WalRecord::Delete { path }
            });
        }
        for (path, mode) in ordered.directories {
            self.create_directory(path)?;
            records.push(WalRecord::Mkdir {
                path: path.to_path_buf(),
                mode,
            });
        }
        for path in ordered.writes {
            records.push(self.write(path)?);
        }
        let owner = rustix::process::geteuid();
        for path in ordered.modes.keys() {
            self.set_mode(path, owner)?;
        }
        let mut finals = self.widenings(&ordered.modes, owner)?;
        finals.extend(
            ordered
                .modes
                .into_iter()
                .map(|(path, mode)| (path.to_path_buf(), mode)),
        );
        finals.sort_by(|(left, _), (right, _)| by_depth(left, right, true));
        records.extend(
            finals
                .into_iter()
                .map(|(path, mode)| WalRecord::Chmoddir { path, mode }),
        );
        if !records.is_empty() {
            self.root_writable()?;
        }
        Ok(records)
    }

    /// What is at `path` once the operations planned so far have applied.
    fn state(&self, path: &Path) -> Result<State, Error> {
        if path.as_os_str().is_empty() {
            return Ok(State::Directory);
        }
        if let Some(planned) = self.planned.get(path) {
            return Ok(match planned {
                Planned::Absent => State::Absent,
                Planned::Created => State::Directory,
                Planned::Written => State::Other,
            });
        }
        // A created directory starts empty, and nothing is left beneath a removed or replaced one.
        if path
            .ancestors()
            .skip(1)
            .any(|ancestor| self.planned.contains_key(ancestor))
        {
            return Ok(State::Absent);
        }
        Ok(match self.seed.lookup(path)? {
            None => State::Absent,
            Some((_, node)) if node.kind == Kind::Directory => State::Directory,
            Some(_) => State::Other,
        })
    }

    /// Fails unless the parent of `path` will be a directory when its operation applies.
    fn parent_is_directory(&self, path: &Path) -> Result<(), Error> {
        let (parent, _) = split(path)?;
        if self.state(parent)? == State::Directory {
            Ok(())
        } else {
            Err(Error::Wal(format!(
                "{} cannot change: {} is not a directory in {}",
                path.display(),
                parent.display(),
                self.seed.path().display()
            )))
        }
    }

    /// Records that the plan changes the contents of the existing seed directory `directory`.
    fn touch(&mut self, directory: &Path) {
        if !directory.as_os_str().is_empty()
            && self.planned.get(directory) != Some(&Planned::Created)
        {
            self.changed.insert(directory.to_path_buf());
        }
    }

    /// Plans the removal of the leaf — or, for `directory`, the emptied directory — at `path`.
    fn remove(&mut self, path: &Path, directory: bool) -> Result<(), Error> {
        self.parent_is_directory(path)?;
        let (parent, _) = split(path)?;
        match (self.state(path)?, directory) {
            // Nothing there is a removal already done.
            (State::Absent, _) => {}
            (State::Other, false) => self.touch(parent),
            (State::Directory, true) => {
                self.check_emptied(path)?;
                self.touch(parent);
            }
            (State::Directory, false) => {
                return Err(Error::Wal(format!(
                    "{} is a directory, which only a directory removal removes",
                    path.display()
                )));
            }
            (State::Other, true) => {
                return Err(Error::Wal(format!(
                    "{} is not a directory, so a directory removal cannot remove it",
                    path.display()
                )));
            }
        }
        self.planned.insert(path.to_path_buf(), Planned::Absent);
        Ok(())
    }

    /// Fails unless every entry of the seed directory `path` is removed by an earlier operation:
    /// a directory removal takes an empty directory, never contents.
    fn check_emptied(&self, path: &Path) -> Result<(), Error> {
        let Lookup::Found(directory) = self.seed.directory(path, true)? else {
            return Err(Error::Wal(format!("{} vanished", path.display())));
        };
        for entry in rustix::fs::Dir::read_from(&directory)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let child = path.join(OsStr::from_bytes(name));
            if self.planned.get(&child) != Some(&Planned::Absent) {
                return Err(Error::Wal(format!(
                    "{} cannot be removed: {} is still in it",
                    path.display(),
                    child.display()
                )));
            }
        }
        Ok(())
    }

    /// Plans the creation of the directory at `path`.
    fn create_directory(&mut self, path: &Path) -> Result<(), Error> {
        self.parent_is_directory(path)?;
        if self.state(path)? != State::Absent {
            return Err(Error::Wal(format!(
                "{} cannot be created: something is already there",
                path.display()
            )));
        }
        self.work_directory(path)?;
        let (parent, _) = split(path)?;
        self.touch(parent);
        self.planned.insert(path.to_path_buf(), Planned::Created);
        Ok(())
    }

    /// Plans the write of the work tree's file or symlink at `path`, returning its record.
    fn write(&mut self, path: &Path) -> Result<WalRecord<()>, Error> {
        self.parent_is_directory(path)?;
        if self.state(path)? == State::Directory {
            return Err(Error::Wal(format!(
                "{} is a directory, which has to be removed before a file can replace it",
                path.display()
            )));
        }
        let work = self.work;
        let (parent, name) = split(path)?;
        let Some((directory, node)) = work.lookup(path)? else {
            return Err(Error::Wal(format!(
                "{} is not in {}",
                path.display(),
                work.path().display()
            )));
        };
        let (kind, mode) = match node.kind {
            Kind::File if node.nlink > 1 => {
                return Err(Error::Wal(format!(
                    "{} has {} hard links, which publishing a copy of it would break",
                    path.display(),
                    node.nlink
                )));
            }
            Kind::File => (EntryKind::File, Some(node.mode)),
            Kind::Symlink => (EntryKind::Symlink, None),
            Kind::Directory => {
                return Err(Error::Wal(format!(
                    "{} is a directory in {}, not a file",
                    path.display(),
                    work.path().display()
                )));
            }
            Kind::Special => {
                return Err(Error::Wal(format!(
                    "{} is a fifo, socket or device node, which cannot be published",
                    path.display()
                )));
            }
        };
        let sha1 = hash_content(directory.as_fd(), name, kind, self.buffer)?;
        self.touch(parent);
        self.planned.insert(path.to_path_buf(), Planned::Written);
        Ok(WalRecord::Move {
            from: path.to_path_buf(),
            to: path.to_path_buf(),
            sha1,
            kind,
            mode,
        })
    }

    /// Checks that the existing directory at `path` can be given a final mode by `owner`.
    fn set_mode(&self, path: &Path, owner: Uid) -> Result<(), Error> {
        let found = match self.state(path)? {
            State::Directory => self.seed.lookup(path)?,
            State::Absent | State::Other => None,
        };
        let Some((_, node)) = found else {
            return Err(Error::Wal(format!(
                "{} is not a directory of {}",
                path.display(),
                self.seed.path().display()
            )));
        };
        if node.owner != owner && !owner.is_root() {
            return Err(Error::Wal(format!(
                "the mode of {} cannot be set by a user who does not own it",
                path.display()
            )));
        }
        self.work_directory(path)
    }

    /// Fails unless the work tree has a directory at `path`.
    fn work_directory(&self, path: &Path) -> Result<(), Error> {
        match self.work.lookup(path)? {
            Some((_, node)) if node.kind == Kind::Directory => Ok(()),
            _ => Err(Error::Wal(format!(
                "{} is not a directory in {}",
                path.display(),
                self.work.path().display()
            ))),
        }
    }

    /// The directories whose contents change but that this user cannot read, write and search,
    /// each with the mode it keeps: the final modes to log so they can be opened up to their
    /// owner while the transaction runs. One already given a final mode, or removed, needs none.
    fn widenings(
        &self,
        modes: &BTreeMap<&Path, Mode>,
        owner: Uid,
    ) -> Result<Vec<(PathBuf, Mode)>, Error> {
        let mut widened = Vec::new();
        for directory in &self.changed {
            let (_, name) = split(directory)?;
            let Some((parent, node)) = self.seed.lookup(directory)? else {
                return Err(Error::Wal(format!("{} vanished", directory.display())));
            };
            match rustix::fs::accessat(&parent, name, FULL_ACCESS, AtFlags::EACCESS) {
                Ok(()) => continue,
                Err(Errno::ACCESS) => {}
                Err(error) => return Err(io_error(&self.seed.path().join(directory), error)),
            }
            if node.owner != owner {
                return Err(Error::Wal(format!(
                    "{} cannot be changed: this user may not write it and does not own it",
                    directory.display()
                )));
            }
            if !modes.contains_key(directory.as_path()) && !self.planned.contains_key(directory) {
                widened.push((directory.clone(), node.mode));
            }
        }
        Ok(widened)
    }

    /// Fails unless this user can read, write and search the seed's root, which holds the staging
    /// directory and every top-level change.
    fn root_writable(&self) -> Result<(), Error> {
        match rustix::fs::accessat(CWD, self.seed.path(), FULL_ACCESS, AtFlags::EACCESS) {
            Ok(()) => Ok(()),
            Err(Errno::ACCESS) => Err(Error::Wal(format!(
                "{} is not writable by this user",
                self.seed.path().display()
            ))),
            Err(error) => Err(io_error(self.seed.path(), error)),
        }
    }
}

/// Read, write and search: what this user needs of every directory a transaction changes.
const FULL_ACCESS: Access = Access::READ_OK
    .union(Access::WRITE_OK)
    .union(Access::EXEC_OK);

/// The refusal of two operations on one path that no single change of the tree produces.
fn twice(path: &Path) -> Error {
    Error::Wal(format!(
        "{} is named by operations that contradict each other",
        path.display()
    ))
}

/// Applies one transaction's operations to the seed: the first time, and every replay after.
struct Engine<'e, 'p, M> {
    /// The seed.
    seed: &'e Tree<'p>,
    /// The tree the writes copy from, when it still exists.
    work: Option<&'e Tree<'p>>,
    /// The transaction's operation records.
    ops: &'e [WalRecord<M>],
    /// Which moves are already in place, by index; an index past the end is not.
    done: &'e [bool],
    /// Seed directories whose entries changed, to fsync before `END`; the root always is.
    dirty: BTreeSet<PathBuf>,
    /// The buffer every file is read through.
    buffer: &'e mut [u8],
}

impl<M> Engine<'_, '_, M> {
    /// Applies every operation, then gives every directory its final mode and makes the whole
    /// transaction durable, staging directory gone.
    fn run(mut self, staging: &Staging) -> Result<(), Error> {
        let ops = self.ops;
        // Owner access first, shallowest first, to every existing directory the transaction may
        // have to change beneath: a replay can find one already closed to its final mode.
        let mut widen: Vec<&Path> = ops
            .iter()
            .filter_map(|op| match op {
                WalRecord::Rmdir { path } => Some(path.as_path()),
                other => other.final_mode().map(|(path, _)| path),
            })
            .collect();
        widen.sort_by(|left, right| by_depth(left, right, false));
        for path in widen {
            if let Some((parent, node)) = self.seed.lookup(path)?
                && node.kind == Kind::Directory
            {
                log::widen(parent.as_fd(), split(path)?.1, node.mode)?;
            }
        }

        let needed = ops
            .iter()
            .enumerate()
            .any(|(index, op)| op.stages() && !self.is_done(index));
        let staging_dir = open_staging(self.seed, staging, ops, needed)?;
        for (index, op) in ops.iter().enumerate() {
            match op {
                WalRecord::Delete { path } => self.remove(index, path, false)?,
                WalRecord::Rmdir { path } => self.remove(index, path, true)?,
                WalRecord::Mkdir { path, .. } => self.make_directory(path)?,
                WalRecord::Move {
                    from,
                    to,
                    sha1,
                    kind,
                    mode,
                } => {
                    let moved = Moved::logged(to, *kind, *mode)?;
                    if !self.is_done(index) {
                        let staging_dir = staging_dir
                            .as_ref()
                            .ok_or_else(|| Error::Wal(format!("{staging} was never opened")))?;
                        self.write(staging_dir.as_fd(), index, from, to, sha1, moved)?;
                    }
                }
                WalRecord::Chmoddir { .. } => {}
                WalRecord::Begin { .. } | WalRecord::End { .. } => {
                    return Err(Error::Wal(
                        "a framing record among a transaction's operations".to_string(),
                    ));
                }
            }
            if index == 0 {
                crash_point(Crash::FirstOperation);
            }
        }
        self.finish(staging, staging_dir)
    }

    /// Whether the move at `index` is already in place.
    fn is_done(&self, index: usize) -> bool {
        self.done.get(index).copied().unwrap_or(false)
    }

    /// Records that the entries of the seed directory `directory` changed.
    fn touch(&mut self, directory: &Path) {
        if !directory.as_os_str().is_empty() {
            self.dirty.insert(directory.to_path_buf());
        }
    }

    /// Removes the leaf — or, for `directory`, the empty directory — at `path`, the operation at
    /// `index`.
    fn remove(&mut self, index: usize, path: &Path, directory: bool) -> Result<(), Error> {
        let (parent, name) = split(path)?;
        let dir = match self.seed.directory(parent, false)? {
            Lookup::Found(dir) => dir,
            // Nothing beneath a missing ancestor: the removal is done.
            Lookup::Missing => return Ok(()),
            Lookup::Blocked(ancestor) => {
                if self.replaced_later(index, &ancestor)? {
                    return Ok(());
                }
                return Err(Error::Wal(format!(
                    "{} cannot be removed: {} is not a directory",
                    path.display(),
                    ancestor.display()
                )));
            }
        };
        if !directory && self.written_later(index, path) {
            return Ok(());
        }
        let removal = log::apply_remove(dir.as_fd(), name, directory)?;
        drop(dir);
        match removal {
            Removal::Removed => {
                self.touch(parent);
                if directory {
                    self.dirty.retain(|dirty| !dirty.starts_with(path));
                }
                Ok(())
            }
            Removal::Absent => Ok(()),
            Removal::WrongKind if self.replaced_later(index, path)? => Ok(()),
            Removal::WrongKind if directory => Err(Error::Wal(format!(
                "{} cannot be removed as a directory: it is not one",
                path.display()
            ))),
            Removal::WrongKind => Err(Error::Wal(format!(
                "{} cannot be removed as a leaf: it is a directory",
                path.display()
            ))),
        }
    }

    /// Whether a later operation than `index` already made `path` what it ends as: the directory
    /// a later creation makes, or exactly the content a later move writes.
    fn replaced_later(&mut self, index: usize, path: &Path) -> Result<bool, Error> {
        for op in self.ops.iter().skip(index + 1) {
            match op {
                WalRecord::Mkdir { path: created, .. } if created == path => {
                    return Ok(self
                        .seed
                        .lookup(path)?
                        .is_some_and(|(_, node)| node.kind == Kind::Directory));
                }
                WalRecord::Move {
                    to,
                    sha1,
                    kind,
                    mode,
                    ..
                } if to == path => {
                    let Some(moved) = Moved::of(*kind, *mode) else {
                        return Ok(false);
                    };
                    return Ok(Found::at(self.seed, path, self.buffer)?
                        .is_some_and(|found| found.carries(moved, sha1)));
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Whether a later operation than `index` writes `path` and is already in place, so removing
    /// what is there would undo it.
    fn written_later(&self, index: usize, path: &Path) -> bool {
        self.ops
            .iter()
            .enumerate()
            .skip(index + 1)
            .any(|(later, op)| {
                matches!(op, WalRecord::Move { to, .. } if to == path) && self.is_done(later)
            })
    }

    /// Makes `path` a directory the owner can change, creating it `0700` when absent.
    fn make_directory(&mut self, path: &Path) -> Result<(), Error> {
        let (parent, name) = split(path)?;
        let Lookup::Found(dir) = self.seed.directory(parent, false)? else {
            return Err(Error::Wal(format!(
                "{} cannot be created: {} is not a directory",
                path.display(),
                parent.display()
            )));
        };
        log::apply_mkdir(dir.as_fd(), name)?;
        match self.seed.entry(&dir, name, path)? {
            Some(node) if node.kind == Kind::Directory => {
                log::widen(dir.as_fd(), name, node.mode)?;
            }
            _ => {
                return Err(Error::Wal(format!(
                    "{} exists and is not a directory",
                    path.display()
                )));
            }
        }
        drop(dir);
        self.touch(parent);
        Ok(())
    }

    /// Copies the source at `from` onto `to` through `staging`, the move at `index`.
    fn write(
        &mut self,
        staging: BorrowedFd<'_>,
        index: usize,
        from: &Path,
        to: &Path,
        sha1: &ContentHash,
        moved: Moved,
    ) -> Result<(), Error> {
        let missing = || Error::Wal(format!("source {} is gone", from.display()));
        let work = self.work.ok_or_else(missing)?;
        let (_, source_name) = split(from)?;
        let (target_parent, target_name) = split(to)?;
        let (source, node) = work.lookup(from)?.ok_or_else(missing)?;
        let kind = moved.kind();
        if node.kind != kind {
            return Err(Error::Wal(format!(
                "source {} is no longer the kind of entry its record logged",
                from.display()
            )));
        }
        let mode = match moved {
            Moved::File(mode) => mode,
            Moved::Symlink => node.mode,
        };
        let Lookup::Found(target) = self.seed.directory(target_parent, false)? else {
            return Err(Error::Wal(format!(
                "{} cannot be written: {} is not a directory",
                to.display(),
                target_parent.display()
            )));
        };
        log::apply_write(
            source.as_fd(),
            source_name,
            kind,
            mode,
            staging,
            index,
            target.as_fd(),
            target_name,
            sha1,
            self.buffer,
        )?;
        drop(target);
        self.touch(target_parent);
        Ok(())
    }

    /// Gives every directory its final mode, deepest first so each is closed only after
    /// everything beneath it is in place; fsyncs every changed directory; removes the staging
    /// directory; and fsyncs the seed's root last.
    fn finish(self, staging: &Staging, staging_dir: Option<OwnedFd>) -> Result<(), Error> {
        let mut targets: BTreeMap<PathBuf, Option<Mode>> =
            self.dirty.into_iter().map(|path| (path, None)).collect();
        for (path, mode) in self.ops.iter().filter_map(WalRecord::final_mode) {
            targets.insert(path.to_path_buf(), Some(mode));
        }
        let mut order: Vec<(PathBuf, Option<Mode>)> = targets.into_iter().collect();
        order.sort_by(|(left, _), (right, _)| by_depth(left, right, true));
        for (path, mode) in order {
            let Lookup::Found(directory) = self.seed.directory(&path, true)? else {
                return Err(Error::Wal(format!(
                    "{} vanished before its transaction finished",
                    path.display()
                )));
            };
            log::sync_directory(&directory, mode)?;
        }
        if let Some(staging_dir) = staging_dir {
            drop(staging_dir);
            rustix::fs::unlinkat(self.seed.root(), staging.as_str(), AtFlags::REMOVEDIR)
                .map_err(|error| io_error(&self.seed.path().join(staging.as_str()), error))?;
        }
        rustix::fs::fsync(self.seed.root())?;
        Ok(())
    }
}

/// Opens the transaction's staging directory: an existing one — which only a replay can meet —
/// after checking and clearing its temporaries, else a fresh one when `needed`.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when the staging name is taken by something that is not a directory,
/// and with [`Error::Io`] when it cannot be opened for any other reason than being absent.
fn open_staging<M>(
    seed: &Tree<'_>,
    staging: &Staging,
    ops: &[WalRecord<M>],
    needed: bool,
) -> Result<Option<OwnedFd>, Error> {
    let open = || {
        rustix::fs::openat(
            seed.root(),
            staging.as_str(),
            STAGING_FLAGS,
            RawMode::empty(),
        )
    };
    match open() {
        Ok(directory) => {
            clear_parts(staging, &directory, ops)?;
            return Ok(Some(directory));
        }
        Err(Errno::NOENT) if needed => {}
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::NOTDIR | Errno::LOOP) => {
            return Err(Error::Wal(format!(
                "{} is occupied by something that is not a staging directory",
                seed.path().join(staging.as_str()).display()
            )));
        }
        Err(error) => return Err(io_error(&seed.path().join(staging.as_str()), error)),
    }
    rustix::fs::mkdirat(seed.root(), staging.as_str(), RawMode::RWXU)?;
    let directory = open()?;
    rustix::fs::fchmod(&directory, RawMode::RWXU)?;
    Ok(Some(directory))
}

/// How a staging directory is opened: read-only, as a directory, never through a symlink.
const STAGING_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Removes the staging directory a finished transaction left behind, if it did.
fn clear_residue<M>(seed: &Tree<'_>, staging: &Staging, ops: &[WalRecord<M>]) -> Result<(), Error> {
    // The directory is closed again before it is removed: the condition is its own scope.
    if open_staging(seed, staging, ops, false)?.is_some() {
        rustix::fs::unlinkat(seed.root(), staging.as_str(), AtFlags::REMOVEDIR)?;
        rustix::fs::fsync(seed.root())?;
    }
    Ok(())
}

/// Deletes every temporary in `directory`, the staging directory of the transaction whose
/// operations are `ops` — after checking that each one is: a file or symlink named for one of its
/// moves. Anything else means the directory is not only this transaction's, and nothing is
/// deleted.
fn clear_parts<M>(
    staging: &Staging,
    directory: &OwnedFd,
    ops: &[WalRecord<M>],
) -> Result<(), Error> {
    let mut parts: Vec<CString> = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let known = Staging::part_index(name.to_bytes())
            .is_some_and(|index| ops.get(index).is_some_and(WalRecord::stages));
        let kind = FileType::from_raw_mode(
            rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode,
        );
        if !known || !matches!(kind, FileType::RegularFile | FileType::Symlink) {
            return Err(Error::Wal(format!(
                "{staging}/{} is not a temporary of the transaction that owns the directory; \
                 recovery refuses to guess what it is",
                String::from_utf8_lossy(name.to_bytes())
            )));
        }
        parts.push(name.to_owned());
    }
    for part in parts {
        rustix::fs::unlinkat(directory, part.as_c_str(), AtFlags::empty())?;
    }
    Ok(())
}

/// A point in [`PreparedTransaction::apply`] a crash test can end its process at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    /// The intent is durable and nothing has been applied.
    Intent,
    /// The first operation has been applied.
    FirstOperation,
    /// Every operation has been applied and made durable, and `END` is not yet written.
    BeforeEnd,
    /// `END` is durable.
    End,
}

/// Ends the process at `point` when the crash test running it asked for that point.
#[cfg(test)]
fn crash_point(point: Crash) {
    if std::env::var(tests::CRASH_AT).is_ok_and(|wanted| wanted == format!("{point:?}")) {
        // SAFETY: `_exit` takes no pointer and never returns; ending the process at once, with
        // nothing unwound and nothing flushed, is exactly the crash being simulated.
        unsafe { libc::_exit(tests::CRASH_STATUS) }
    }
}

/// Crash points exist only in this crate's own tests.
#[cfg(not(test))]
const fn crash_point(_: Crash) {}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
pub(crate) mod tests {
    use std::fmt::Debug;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use serde_json::json;

    use super::*;
    use crate::diff::diff_trees;
    use crate::testing::{directory, mode_of, put};

    /// Environment variable naming the [`Crash`] point a crash-test child ends its process at.
    pub(crate) const CRASH_AT: &str = "MARSH_WAL_CRASH_AT";

    /// Exit status of a crash-test child that reached its crash point.
    pub(crate) const CRASH_STATUS: i32 = 86;

    /// Environment variable naming the scratch directory a crash-test child publishes in.
    const CRASH_SCRATCH: &str = "MARSH_WAL_CRASH_SCRATCH";

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

    fn uid(text: &str) -> SourceUid {
        SourceUid::new(text).expect("a valid uid")
    }

    /// The three paths a transaction needs, over plain directories — these tests only copy
    /// files, so no subvolume is needed — and the scratch directory holding them, opened up and
    /// removed on drop even where a test left directories unwritable.
    struct Scratch {
        /// The tree transactions publish into.
        seed: PathBuf,
        /// The directory holding one snapshot per job uid.
        snap: PathBuf,
        /// The write-ahead log.
        log: PathBuf,
        /// The scratch directory itself.
        root: tempfile::TempDir,
    }

    impl Scratch {
        /// A fresh layout in its own temporary directory.
        fn new() -> Self {
            let root = tempfile::tempdir().expect("scratch directory");
            let layout = Self {
                seed: root.path().join("seed"),
                snap: root.path().join("snap"),
                log: root.path().join("meta").join(LOG_FILE),
                root,
            };
            std::fs::create_dir_all(&layout.seed).expect("seed");
            std::fs::create_dir_all(&layout.snap).expect("snap");
            layout
        }

        /// A job's snapshot, `snap/<uid>`, created on first ask — what [`prepare`] takes as
        /// `work`.
        fn work(&self, uid: &str) -> PathBuf {
            let work = self.snap.join(uid);
            std::fs::create_dir_all(&work).expect("snapshot");
            work
        }

        /// Prepares `ops` from `work` as transaction `seq` of snapshot `uid_text`, recording
        /// `cmd`.
        fn prepare<'a>(
            &'a self,
            work: &'a Path,
            uid_text: &str,
            seq: u64,
            cmd: &str,
            ops: &[CommitOp],
        ) -> Result<PreparedTransaction<'a>, Error> {
            prepare(
                &self.seed,
                work,
                &self.log,
                &uid(uid_text),
                Seq::new(seq),
                &meta(cmd),
                ops,
            )
        }

        /// Prepares and applies `ops` from snapshot `uid` as transaction `seq`.
        fn publish(&self, uid_text: &str, seq: u64, ops: &[CommitOp]) -> Result<(), Error> {
            self.prepare(&self.work(uid_text), uid_text, seq, "cmd", ops)?
                .apply()
        }

        /// Publishes whatever snapshot `uid` changed relative to the seed, returning the
        /// operations the diff found.
        fn publish_diff(&self, uid_text: &str, seq: u64) -> Result<Vec<CommitOp>, Error> {
            let ops = diff_trees(&self.seed, &self.work(uid_text))?;
            self.publish(uid_text, seq, &ops)?;
            Ok(ops)
        }

        /// Recovers with this crate's test metadata, which names no path and needs no frame kept.
        fn recover(&self) -> Result<Vec<Transaction<Meta>>, Error> {
            recover(&self.seed, &self.snap, &self.log, |_, _| {}, |_, _| false)
        }

        fn records(&self) -> Vec<WalRecord<Meta>> {
            JsonLog::<WalRecord<Meta>>::read(&self.log).expect("read the log")
        }

        /// The log's bytes.
        fn log_bytes(&self) -> Vec<u8> {
            std::fs::read(&self.log).expect("read the log")
        }

        /// The bytes of the seed's file at `path`.
        fn read(&self, path: &str) -> Vec<u8> {
            std::fs::read(self.seed.join(path)).expect("read the seed")
        }

        fn append(&self, records: &[WalRecord<Meta>]) {
            JsonLog::open(&self.log)
                .expect("open log")
                .append(records)
                .expect("append");
        }

        /// Whether any staging directory exists in the seed.
        fn staging_left(&self) -> bool {
            std::fs::read_dir(&self.seed)
                .expect("read the seed")
                .any(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .as_encoded_bytes()
                        .starts_with(b".marsh-wal-")
                })
        }

        /// Fails unless publishing `ops` from snapshot `job0` as transaction 1 is refused with a
        /// log failure naming each of `needles`, before any intent is logged.
        #[track_caller]
        fn refuse(&self, ops: &[CommitOp], needles: &[&str]) {
            assert_wal(self.publish("job0", 1, ops), needles);
            assert!(self.records().is_empty(), "no intent was logged");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            open_up(self.root.path());
        }
    }

    /// Fails unless `result` is an [`Error::Wal`] whose message contains every one of `needles`.
    #[track_caller]
    fn assert_wal<T: Debug>(result: Result<T, Error>, needles: &[&str]) {
        match result {
            Err(Error::Wal(message)) if needles.iter().all(|&needle| message.contains(needle)) => {}
            other => panic!("expected a log failure naming {needles:?}, got {other:?}"),
        }
    }

    /// Gives the owner full access to every directory beneath `root`, so it can be removed.
    fn open_up(root: &Path) {
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
        let _ = marsh_lib::walk_directory::<_, std::io::Error>(root, (), |entry, ()| {
            if entry.file_type()?.is_dir() {
                std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700))?;
                return Ok(Some(()));
            }
            Ok(None)
        });
    }

    /// Everything a tree is: every entry's kind, permission bits and content — a file's bytes, a
    /// symlink's target — by relative path. Two trees with the same shape are the same tree.
    fn shape(root: &Path) -> BTreeMap<PathBuf, (char, u32, Vec<u8>)> {
        let mut entries = BTreeMap::new();
        marsh_lib::walk_directory::<_, std::io::Error>(root, PathBuf::new(), |entry, prefix| {
            let relative = prefix.join(entry.file_name());
            let path = entry.path();
            let kind = entry.file_type()?;
            let described = if kind.is_dir() {
                ('d', mode_of(&path), Vec::new())
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&path)?;
                ('l', 0, target.into_os_string().into_encoded_bytes())
            } else {
                ('f', mode_of(&path), std::fs::read(&path)?)
            };
            entries.insert(relative.clone(), described);
            Ok(kind.is_dir().then_some(relative))
        })
        .expect("walk");
        entries
    }

    /// A `BEGIN` of transaction `seq` from snapshot `uid_text`, declaring `count` operations.
    fn begin(seq: u64, uid_text: &str, count: usize) -> WalRecord<Meta> {
        let uid = uid(uid_text);
        WalRecord::Begin {
            seq: Seq::new(seq),
            staging: Staging::of(Seq::new(seq), &uid),
            uid,
            op_count: count,
            meta: meta("cmd"),
        }
    }

    /// A `MOVE` of the file `path` carrying `contents` with `mode`.
    fn file_move(path: &str, contents: &[u8], mode: u32) -> WalRecord<Meta> {
        WalRecord::Move {
            from: PathBuf::from(path),
            to: PathBuf::from(path),
            sha1: ContentHash::of(contents),
            kind: EntryKind::File,
            mode: Some(Mode::new(mode)),
        }
    }

    /// A `DELETE` of `path`.
    fn delete(path: &str) -> WalRecord<Meta> {
        WalRecord::Delete {
            path: PathBuf::from(path),
        }
    }

    /// The `END` of transaction `seq`.
    const fn end(seq: u64) -> WalRecord<Meta> {
        WalRecord::End { seq: Seq::new(seq) }
    }

    /// Transaction `seq`, from snapshot `job<seq>`, carrying `records` and finished.
    fn finished(seq: u64, records: Vec<WalRecord<Meta>>) -> Vec<WalRecord<Meta>> {
        let mut frame = vec![begin(seq, &format!("job{seq}"), records.len())];
        frame.extend(records);
        frame.push(end(seq));
        frame
    }

    /// The operation writing the work tree's entry at `path`.
    fn write(path: &str) -> CommitOp {
        CommitOp::Write(PathBuf::from(path))
    }

    /// The operation removing the seed's non-directory at `path`.
    fn remove(path: &str) -> CommitOp {
        CommitOp::Remove(PathBuf::from(path))
    }

    /// What each record of a log is and names, in order: enough to compare two histories.
    fn outline(records: &[WalRecord<Meta>]) -> Vec<String> {
        records
            .iter()
            .map(|record| match record {
                WalRecord::Begin { seq, op_count, .. } => format!("BEGIN {seq} {op_count}"),
                WalRecord::Move { to, .. } => format!("MOVE {}", to.display()),
                WalRecord::Delete { path } => format!("DELETE {}", path.display()),
                WalRecord::Mkdir { path, .. } => format!("MKDIR {}", path.display()),
                WalRecord::Chmoddir { path, .. } => format!("CHMODDIR {}", path.display()),
                WalRecord::Rmdir { path } => format!("RMDIR {}", path.display()),
                WalRecord::End { seq } => format!("END {seq}"),
            })
            .collect()
    }

    /// A published write logs its paths as plain strings, what it moves and with which mode, and
    /// the directories it needs as operations of their own.
    #[test]
    fn a_publication_logs_every_entry_explicitly() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        put(&work.join("src/new/deep.txt"), b"fresh\n", 0o640);
        directory(&work.join("src/new"), 0o750);

        let ops = layout.publish_diff("job0", 1).expect("publish");
        assert_eq!(
            ops,
            vec![
                CommitOp::CreateDirectory {
                    path: PathBuf::from("src"),
                    mode: Mode::new(mode_of(&work.join("src"))),
                },
                CommitOp::CreateDirectory {
                    path: PathBuf::from("src/new"),
                    mode: Mode::new(0o750),
                },
                write("src/new/deep.txt"),
            ]
        );

        let text = std::fs::read_to_string(&layout.log).expect("read log");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse line"))
            .collect();
        let moved = lines
            .iter()
            .find(|line| line["op"] == json!("MOVE"))
            .expect("a MOVE line");
        assert_eq!(moved["from"], json!("src/new/deep.txt"));
        assert_eq!(moved["to"], json!("src/new/deep.txt"));
        assert_eq!(moved["kind"], json!("file"));
        assert_eq!(moved["mode"], json!(0o640));
        assert!(lines.contains(&json!({"op": "MKDIR", "path": "src/new", "mode": 0o750})));
        assert_eq!(shape(&layout.seed), shape(&work));
        assert!(!layout.staging_left(), "the staging directory is gone");
    }

    /// A path JSON cannot carry fails at serialization, which is *before* the seed is touched.
    #[test]
    fn a_non_utf8_path_fails_before_the_seed_is_touched() {
        let layout = Scratch::new();
        let name = OsStr::from_bytes(b"bad\xff");
        put(&layout.work("job0").join(name), b"x\n", 0o644);

        let error = layout
            .publish("job0", 1, &[CommitOp::Write(PathBuf::from(name))])
            .expect_err("an unencodable path cannot be logged");
        assert!(
            matches!(&error, Error::Wal(message) if message.starts_with("serialize record:")),
            "got {error:?}"
        );
        assert!(!layout.seed.join(name).exists());
        assert!(!layout.log.exists(), "nothing was logged");
    }

    /// The log's whole reason to exist: the record is durable, the seed is not yet whole, and the
    /// next startup finishes it — and hands the caller back what it may still owe a history entry.
    #[test]
    fn an_unfinished_transaction_is_replayed_on_recover() {
        let layout = Scratch::new();
        put(&layout.work("job0").join("a.txt"), b"recovered\n", 0o644);
        layout.append(&[
            begin(1, "job0", 1),
            file_move("a.txt", b"recovered\n", 0o644),
        ]);

        let recovered = layout.recover().expect("recover");
        assert_eq!(layout.read("a.txt"), b"recovered\n");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, Seq::new(1));
        assert_eq!(recovered[0].uid, uid("job0"));
        assert_eq!(recovered[0].meta, meta("cmd"));
        assert_eq!(recovered[0].ops, [write("a.txt")]);
        assert!(matches!(
            layout.records().last(),
            Some(WalRecord::End { seq }) if *seq == Seq::new(1)
        ));
        assert!(!layout.staging_left());

        let again = layout.recover().expect("recover again");
        assert_eq!(
            again.len(),
            1,
            "a finished transaction is still reported; only the caller knows what it remembers"
        );
    }

    /// The case the recorded hash exists for: the transaction did reach the seed, the crash beat
    /// its `End`, and the snapshot it came from has since been swept. The content is proof enough.
    #[test]
    fn a_replayed_move_whose_snapshot_is_gone_is_a_no_op() {
        let layout = Scratch::new();
        layout.append(&[
            begin(1, "job0", 1),
            file_move("a.txt", b"recovered\n", 0o644),
        ]);
        put(&layout.seed.join("a.txt"), b"recovered\n", 0o644);
        std::fs::remove_dir_all(layout.work("job0")).expect("sweep the snapshot");

        let recovered = layout.recover().expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(layout.read("a.txt"), b"recovered\n");
    }

    /// A present source must be exactly what its record logged: a different one is refused before
    /// anything is replayed, rather than published under a digest it does not match.
    #[test]
    fn a_source_that_differs_from_its_record_is_refused_before_replay() {
        let layout = Scratch::new();
        put(&layout.work("job0").join("a.txt"), b"tampered\n", 0o644);
        put(&layout.work("job0").join("b.txt"), b"b\n", 0o644);
        layout.append(&[
            begin(1, "job0", 2),
            file_move("a.txt", b"logged\n", 0o644),
            file_move("b.txt", b"b\n", 0o644),
        ]);

        assert_wal(layout.recover(), &["differs"]);
        assert!(!layout.seed.join("a.txt").exists());
        assert!(!layout.seed.join("b.txt").exists(), "nothing was replayed");

        // The same content with another mode differs too.
        let layout = Scratch::new();
        put(&layout.work("job0").join("a.txt"), b"logged\n", 0o600);
        layout.append(&[begin(1, "job0", 1), file_move("a.txt", b"logged\n", 0o644)]);
        assert_wal(layout.recover(), &[]);
    }

    /// The one state a replay cannot resolve: the source is swept and the seed does not carry the
    /// content either. Failing loudly is the only honest answer — the seed is neither before nor
    /// after the transaction.
    #[test]
    fn a_move_with_neither_source_nor_published_content_fails() {
        let layout = Scratch::new();
        layout.append(&[
            begin(1, "job0", 1),
            file_move("a.txt", b"recovered\n", 0o644),
        ]);
        put(&layout.seed.join("a.txt"), b"something else\n", 0o644);
        std::fs::remove_dir_all(layout.work("job0")).expect("sweep the snapshot");

        assert_wal(
            layout.recover(),
            &["is gone and", "does not carry its content"],
        );
        assert!(
            !matches!(layout.records().last(), Some(WalRecord::End { .. })),
            "no END is appended on a mismatch"
        );
    }

    /// A tree's full shape — empty directories, every directory's mode, and a path that changes
    /// kind in each direction — survives a diff and a publication: diffing again finds nothing,
    /// and a sibling no operation named is left exactly as it was.
    #[test]
    fn every_kind_of_change_round_trips() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        let seed = &layout.seed;
        for tree in [seed, &work] {
            directory(&tree.join("keep"), 0o755);
            put(&tree.join("sibling.txt"), b"sibling\n", 0o644);
            directory(&tree.join("moded"), 0o755);
            put(&tree.join("pointee.txt"), b"pointee\n", 0o644);
        }
        directory(&seed.join("gone"), 0o755);
        put(&seed.join("emptied/x.txt"), b"x\n", 0o644);
        directory(&work.join("emptied"), 0o755);
        put(&seed.join("to_dir"), b"file\n", 0o644);
        directory(&work.join("to_dir/inner"), 0o711);
        put(&seed.join("to_file/inner/leaf"), b"old\n", 0o644);
        put(&work.join("to_file"), b"file\n", 0o600);
        put(&seed.join("to_link/inner.txt"), b"dir-side\n", 0o644);
        std::os::unix::fs::symlink("pointee.txt", work.join("to_link")).expect("symlink");
        directory(&work.join("repo/.git/objects"), 0o755);
        put(
            &work.join("repo/.git/HEAD"),
            b"ref: refs/heads/main\n",
            0o644,
        );
        directory(&work.join("private"), 0o700);
        directory(&work.join("moded"), 0o750);

        layout.publish_diff("job0", 1).expect("publish");
        assert_eq!(
            diff_trees(seed, &work).expect("diff again"),
            Vec::new(),
            "the seed now has the snapshot's shape"
        );
        assert_eq!(shape(seed), shape(&work));
    }

    /// Directories are published as themselves: `mkdir -p` publishes empty directories, a
    /// directory's mode survives a restart, deleting a directory's last file leaves the directory,
    /// and only an explicit directory removal takes it.
    #[test]
    fn directories_are_published_explicitly_and_never_pruned() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        directory(&work.join("empty/nested"), 0o750);
        directory(&work.join("empty"), 0o711);
        put(&work.join("parent/only.txt"), b"x\n", 0o644);
        layout.publish_diff("job0", 1).expect("publish mkdir -p");
        assert_eq!(mode_of(&layout.seed.join("empty")), 0o711);
        assert_eq!(mode_of(&layout.seed.join("empty/nested")), 0o750);

        directory(&work.join("empty"), 0o700);
        std::fs::remove_file(work.join("parent/only.txt")).expect("rm the last file");
        let ops = layout.publish_diff("job0", 2).expect("publish");
        assert_eq!(
            ops,
            vec![
                remove("parent/only.txt"),
                CommitOp::SetDirectoryMode {
                    path: PathBuf::from("empty"),
                    mode: Mode::new(0o700),
                },
            ]
        );
        assert!(
            layout.seed.join("parent").is_dir(),
            "the emptied parent stays"
        );
        layout.recover().expect("reopen");
        assert_eq!(
            mode_of(&layout.seed.join("empty")),
            0o700,
            "the mode survives"
        );

        std::fs::remove_dir(work.join("parent")).expect("rmdir");
        layout.publish_diff("job0", 3).expect("publish rmdir");
        assert!(!layout.seed.join("parent").exists());
        assert_eq!(shape(&layout.seed), shape(&work));
    }

    /// A directory whose contents change but that nobody may write is opened up to its owner
    /// only for as long as the transaction runs, and ends with the mode it had — which the log
    /// records, so a replay restores it too. A new directory that ends unwritable gets its
    /// contents first and its mode last.
    #[test]
    fn an_unwritable_directory_is_opened_up_and_closed_again() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        for tree in [&layout.seed, &work] {
            put(&tree.join("ro/a.txt"), b"a\n", 0o644);
            directory(&tree.join("ro"), 0o555);
        }
        directory(&work.join("ro"), 0o755);
        put(&work.join("ro/b.txt"), b"b\n", 0o644);
        directory(&work.join("ro"), 0o555);
        put(&work.join("sealed/inside.txt"), b"in\n", 0o444);
        directory(&work.join("sealed"), 0o500);

        layout.publish_diff("job0", 1).expect("publish");
        assert_eq!(shape(&layout.seed), shape(&work));
        assert!(
            layout.records().iter().any(|record| matches!(
                record,
                WalRecord::Chmoddir { path, mode }
                    if path == Path::new("ro") && *mode == Mode::new(0o555)
            )),
            "the mode the directory keeps is logged"
        );
    }

    /// A seed directory this user cannot write and does not own cannot be opened up, so the
    /// transaction is refused before its intent — nothing logged, nothing changed.
    #[test]
    fn an_unwritable_root_is_refused_before_the_intent() {
        let layout = Scratch::new();
        put(&layout.work("job0").join("a.txt"), b"a\n", 0o644);
        directory(&layout.seed, 0o555);
        layout.refuse(&[write("a.txt")], &[]);
        assert!(!layout.log.exists());
    }

    /// A fifo, a socket or a device has no content a copy could carry — and opening a fifo with
    /// no writer would block forever. It is refused before the intent, and the seed keeps
    /// accepting transactions afterwards.
    #[test]
    fn a_special_entry_is_refused_before_the_intent() {
        use std::os::unix::net::UnixListener;

        let layout = Scratch::new();
        let work = layout.work("job0");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            work.join("fifo"),
            FileType::Fifo,
            RawMode::RUSR | RawMode::WUSR,
            0,
        )
        .expect("mkfifo");
        drop(UnixListener::bind(work.join("sock")).expect("socket"));
        put(&work.join("fine.txt"), b"fine\n", 0o644);

        for name in ["fifo", "sock"] {
            layout.refuse(&[write(name)], &["fifo, socket or device"]);
        }
        layout
            .publish("job0", 1, &[write("fine.txt")])
            .expect("the seed is not poisoned");
        assert_eq!(layout.read("fine.txt"), b"fine\n");
    }

    /// A changed regular file with more than one link cannot be published: a copy would break the
    /// links the snapshot has, and the seed would no longer be the snapshot.
    #[test]
    fn a_hard_linked_file_is_refused_before_the_intent() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        put(&work.join("a.txt"), b"shared\n", 0o644);
        std::fs::hard_link(work.join("a.txt"), work.join("b.txt")).expect("link");

        let ops = diff_trees(&layout.seed, &work).expect("diff");
        layout.refuse(&ops, &["hard links"]);
        assert!(!layout.seed.join("a.txt").exists());
    }

    /// A directory removal takes only a directory every earlier removal emptied: a member the
    /// transaction does not remove refuses it before the intent, never deletes it.
    #[test]
    fn a_directory_removal_that_would_not_empty_is_refused() {
        let layout = Scratch::new();
        put(&layout.seed.join("d/a.txt"), b"a\n", 0o644);
        put(&layout.seed.join("d/new.txt"), b"new\n", 0o644);
        let rmdir = CommitOp::RemoveDirectory(PathBuf::from("d"));
        layout.refuse(&[remove("d/a.txt"), rmdir], &["is still in it"]);
        assert!(layout.seed.join("d/a.txt").exists() && layout.seed.join("d/new.txt").exists());
    }

    /// Operations that contradict each other or the seed are refused before the intent: a path
    /// both removed and written, a leaf removal of a directory, a write beneath a symlink — whose
    /// target outside the seed is never touched — and a path that is not plain.
    #[test]
    fn contradictory_operations_are_refused_before_the_intent() {
        let layout = Scratch::new();
        let outside = layout.root.path().join("outside");
        directory(&outside, 0o755);
        std::os::unix::fs::symlink(&outside, layout.seed.join("link")).expect("symlink out");
        directory(&layout.seed.join("dir"), 0o755);
        put(&layout.work("job0").join("link/x"), b"x\n", 0o644);
        put(&layout.work("job0").join("a.txt"), b"a\n", 0o644);
        put(&layout.seed.join("a.txt"), b"old\n", 0o644);

        for ops in [
            vec![remove("a.txt"), write("a.txt")],
            vec![remove("dir")],
            vec![write("link/x")],
            vec![write("./a.txt")],
            vec![write(".marsh-wal-1-job0/x")],
        ] {
            layout.refuse(&ops, &[]);
        }
        assert!(!outside.join("x").exists(), "nothing followed the link");
        assert_eq!(layout.read("a.txt"), b"old\n");
    }

    /// Writes land through a staging directory the transaction owns, named for it: a user's own
    /// file whose name happens to end the way temporaries once did is published and kept like
    /// any other, and an existing entry under the staging name refuses the transaction before its
    /// intent rather than being swept.
    #[test]
    fn staging_is_owned_by_the_transaction_and_never_swept_by_name() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        put(&layout.seed.join("a.txt.tmp-wal"), b"mine\n", 0o644);
        put(&work.join("a.txt.tmp-wal"), b"mine\n", 0o644);
        put(&work.join("a.txt"), b"a\n", 0o644);
        put(&work.join("b.tmp-wal"), b"also mine\n", 0o644);

        layout.publish_diff("job0", 1).expect("publish");
        layout.recover().expect("reopen");
        assert_eq!(layout.read("a.txt.tmp-wal"), b"mine\n");
        assert_eq!(layout.read("b.tmp-wal"), b"also mine\n");
        assert!(!layout.staging_left());

        directory(&layout.seed.join(".marsh-wal-2-job0"), 0o755);
        put(&work.join("c.txt"), b"c\n", 0o644);
        let before = layout.log_bytes();
        assert_wal(layout.publish("job0", 2, &[write("c.txt")]), &[]);
        assert_eq!(layout.log_bytes(), before);
        assert!(layout.seed.join(".marsh-wal-2-job0").is_dir());
    }

    /// A replay reuses its frame's staging directory only when everything in it is one of that
    /// frame's temporaries; anything else fails closed and is left exactly where it is.
    #[test]
    fn a_replay_refuses_a_staging_directory_it_does_not_recognize() {
        let layout = Scratch::new();
        put(&layout.work("job0").join("a.txt"), b"a\n", 0o644);
        layout.append(&[begin(1, "job0", 1), file_move("a.txt", b"a\n", 0o644)]);
        let staging = layout.seed.join(".marsh-wal-1-job0");
        put(&staging.join("stray"), b"not ours\n", 0o644);

        assert_wal(layout.recover(), &[]);
        assert!(staging.join("stray").exists());
        assert!(!layout.seed.join("a.txt").exists());

        std::fs::remove_file(staging.join("stray")).expect("remove the stray");
        put(&staging.join("part-0"), b"half a copy", 0o600);
        layout.recover().expect("a known temporary is replaced");
        assert_eq!(layout.read("a.txt"), b"a\n");
        assert!(!layout.staging_left());
    }

    /// A finished frame's staging directory, should one survive, is cleaned — its temporaries
    /// only, and only that directory.
    #[test]
    fn a_finished_frames_staging_residue_is_cleaned() {
        let layout = Scratch::new();
        put(&layout.seed.join("a.txt"), b"a\n", 0o644);
        layout.append(&[
            begin(1, "job0", 1),
            file_move("a.txt", b"a\n", 0o644),
            end(1),
        ]);
        put(
            &layout.seed.join(".marsh-wal-1-job0/part-0"),
            b"residue",
            0o600,
        );
        layout.recover().expect("recover");
        assert!(!layout.staging_left());
        assert_eq!(layout.read("a.txt"), b"a\n");
    }

    /// A counted prefix that lacks operations is abandoned without touching the seed, is not
    /// reported, and — being the log's last frame — is truncated away so the next append starts
    /// clean.
    #[test]
    fn a_final_incomplete_counted_intent_is_abandoned_and_truncated() {
        let layout = Scratch::new();
        put(&layout.seed.join("done.txt"), b"done\n", 0o644);
        layout.append(&[
            begin(1, "job0", 1),
            file_move("done.txt", b"done\n", 0o644),
            end(1),
        ]);
        let complete = layout.log_bytes();
        put(&layout.work("job1").join("a.txt"), b"not durable\n", 0o644);
        layout.append(&[
            begin(2, "job1", 2),
            file_move("a.txt", b"not durable\n", 0o644),
        ]);

        let recovered = layout.recover().expect("abandon the incomplete intent");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, Seq::new(1));
        assert!(!layout.seed.join("a.txt").exists());
        assert_eq!(
            layout.log_bytes(),
            complete,
            "the abandoned frame is truncated back to its BEGIN"
        );
    }

    /// An abandoned intent can sit anywhere — a later transaction reused its sequence number after
    /// a torn append — and it is skipped there, without touching the seed or the log.
    #[test]
    fn an_interior_abandoned_intent_is_skipped() {
        let layout = Scratch::new();
        put(&layout.seed.join("b.txt"), b"b\n", 0o644);
        layout.append(&[
            begin(1, "job0", 2),
            file_move("a.txt", b"torn\n", 0o644),
            begin(1, "job1", 1),
            file_move("b.txt", b"b\n", 0o644),
            end(1),
        ]);
        let before = layout.log_bytes();

        let recovered = layout.recover().expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].uid, uid("job1"));
        assert_eq!(recovered[0].ops, [write("b.txt")]);
        assert!(!layout.seed.join("a.txt").exists());
        assert_eq!(layout.log_bytes(), before);
    }

    /// Framing no writer produces is corruption, refused before any replay: an operation outside
    /// any transaction, an `END` matching no open one, more operations than declared, a declared
    /// count an `END` contradicts, a transaction that never finished followed by another, and a
    /// staging directory that is not the frame's own.
    #[test]
    fn impossible_framing_is_refused() {
        let move_a = || file_move("a.txt", b"a\n", 0o644);
        let mut foreign = begin(1, "job0", 1);
        if let WalRecord::Begin { staging, .. } = &mut foreign {
            *staging = Staging::of(Seq::new(9), &uid("job0"));
        }
        let cases: Vec<Vec<WalRecord<Meta>>> = vec![
            vec![move_a(), begin(1, "job0", 1), move_a(), end(1)],
            vec![begin(1, "job0", 1), move_a(), end(1), end(1)],
            vec![begin(1, "job0", 1), move_a(), end(2)],
            vec![begin(1, "job0", 1), move_a(), move_a()],
            vec![begin(1, "job0", 1), end(1)],
            vec![begin(1, "job0", 1), move_a(), begin(2, "job1", 0), end(2)],
            vec![foreign, move_a(), end(1)],
        ];
        for records in cases {
            let layout = Scratch::new();
            put(&layout.work("job0").join("a.txt"), b"a\n", 0o644);
            layout.append(&records);
            let before = layout.log_bytes();
            assert_wal(layout.recover(), &[]);
            assert!(!layout.seed.join("a.txt").exists());
            assert_eq!(layout.log_bytes(), before);
        }
    }

    /// A durable but undecodable line refuses recovery before replay or cleanup: the valid
    /// unfinished transaction before it is not replayed, and the log, the seed and every source
    /// keep every byte.
    #[test]
    fn an_undecodable_record_is_refused_without_replay() {
        for bad in [
            "{not-json}",
            r#"{"op":"NOPE"}"#,
            r#"{"op":"MKDIR","path":"d","mode":16877}"#,
        ] {
            let layout = Scratch::new();
            put(&layout.seed.join("a.txt"), b"seed\n", 0o644);
            put(&layout.work("job0").join("a.txt"), b"pending\n", 0o644);
            layout.append(&[begin(1, "job0", 1), file_move("a.txt", b"pending\n", 0o644)]);
            let mut raw = layout.log_bytes();
            raw.extend_from_slice(bad.as_bytes());
            raw.push(b'\n');
            log::write_record(&mut raw, &end(1)).expect("encode");
            std::fs::write(&layout.log, &raw).expect("write the corrupt log");
            let seed_before = shape(&layout.seed);
            let sources_before = shape(&layout.snap);

            let error = layout
                .recover()
                .expect_err("the undecodable log is refused");
            assert!(matches!(error, Error::Wal(_)), "{bad}: got {error:?}");
            assert_eq!(layout.log_bytes(), raw, "{bad}: the log is intact");
            assert_eq!(shape(&layout.seed), seed_before, "{bad}: seed untouched");
            assert_eq!(
                shape(&layout.snap),
                sources_before,
                "{bad}: sources untouched"
            );
        }
    }

    /// A record that decodes but names what no writer produces — a file move without its mode, a
    /// symlink move with one, a removal outside the seed — is refused before replay: nothing is
    /// applied and the log keeps every byte.
    #[test]
    fn a_decoded_but_invalid_record_is_refused_before_replay() {
        for bad in [
            r#"{"op":"MOVE","from":"a.txt","to":"a.txt","sha1":"00","kind":"file"}"#,
            r#"{"op":"MOVE","from":"a.txt","to":"a.txt","sha1":"00","kind":"symlink","mode":420}"#,
            r#"{"op":"DELETE","path":"../outside"}"#,
        ] {
            let layout = Scratch::new();
            put(&layout.work("job0").join("a.txt"), b"a\n", 0o644);
            layout.append(&[begin(1, "job0", 2), file_move("a.txt", b"a\n", 0o644)]);
            let mut raw = layout.log_bytes();
            raw.extend_from_slice(bad.as_bytes());
            raw.push(b'\n');
            std::fs::write(&layout.log, &raw).expect("write");

            assert_wal(layout.recover(), &[]);
            assert!(
                !layout.seed.join("a.txt").exists(),
                "{bad}: nothing applied"
            );
            assert_eq!(layout.log_bytes(), raw);
        }
    }

    /// Obsolete complete records fail to decode even inside finished frames, and recovery is
    /// refused whole: no frame is replayed or returned, no missing path is reconciled, no earlier
    /// staging residue is cleared, and the log keeps every byte.
    #[test]
    fn obsolete_records_are_refused_and_preserve_the_log_and_both_trees() {
        for (index, removed, added) in [
            (0, vec!["op_count"], json!({})),
            (0, vec!["staging"], json!({})),
            (0, vec!["op_count", "staging"], json!({})),
            (0, vec![], json!({"op_count": null})),
            (0, vec![], json!({"staging": null})),
            (1, vec!["kind"], json!({})),
            (1, vec!["kind", "mode"], json!({})),
            (1, vec![], json!({"kind": null})),
            (1, vec!["kind", "mode"], json!({"directory_mode": 488})),
            (1, vec!["mode"], json!({"directory_mode": 420})),
            (1, vec![], json!({"directory_mode": 420})),
            (
                1,
                vec!["mode"],
                json!({"kind": "symlink", "directory_mode": 420}),
            ),
        ] {
            let layout = Scratch::new();
            put(&layout.seed.join("a.txt"), b"seed\n", 0o644);
            put(&layout.seed.join("b.txt"), b"suffix\n", 0o600);
            put(&layout.work("job2").join("a.txt"), b"pending\n", 0o640);
            put(
                &layout.seed.join(".marsh-wal-1-job1/part-0"),
                b"residue",
                0o600,
            );
            layout.append(&finished(
                1,
                vec![
                    file_move("a.txt", b"seed\n", 0o644),
                    file_move("missing", b"deleted outside the log\n", 0o644),
                ],
            ));
            let mut raw = layout.log_bytes();
            let mut obsolete: Vec<serde_json::Value> =
                finished(2, vec![file_move("a.txt", b"pending\n", 0o640)])
                    .iter()
                    .map(|record| serde_json::to_value(record).expect("encode"))
                    .collect();
            let record = obsolete[index].as_object_mut().expect("record");
            for field in &removed {
                record.remove(*field);
            }
            record.extend(added.as_object().expect("fields").clone());
            for record in &obsolete {
                log::write_record(&mut raw, record).expect("encode obsolete frame");
            }
            for record in finished(3, vec![file_move("b.txt", b"suffix\n", 0o600)]) {
                log::write_record(&mut raw, &record).expect("encode valid suffix");
            }
            std::fs::write(&layout.log, &raw).expect("write the obsolete log");
            let seed_before = shape(&layout.seed);
            let sources_before = shape(&layout.snap);

            let error = layout.recover().expect_err("the obsolete log is refused");
            assert!(
                matches!(error, Error::Wal(_)),
                "{index} {removed:?} {added}: got {error:?}"
            );
            assert_eq!(
                layout.log_bytes(),
                raw,
                "{index} {removed:?} {added}: the log is intact"
            );
            assert_eq!(shape(&layout.seed), seed_before);
            assert_eq!(shape(&layout.snap), sources_before);
        }
    }

    /// A session that never published anything — no log at all, or a log that exists and is empty
    /// — has no history to hand back, and recovery is not an error there.
    #[test]
    fn an_absent_or_empty_log_recovers_nothing() {
        let layout = Scratch::new();
        assert!(layout.recover().expect("no log at all").is_empty());
        std::fs::create_dir_all(layout.log.parent().expect("meta")).expect("meta");
        std::fs::write(&layout.log, b"").expect("empty log");
        assert!(layout.recover().expect("an empty log").is_empty());
    }

    /// A transaction the log describes as finished is reported but never re-applied: its snapshot
    /// is long swept, and replaying it would fail on a source that is legitimately gone. What the
    /// seed carries at its path now is not the log's to judge — only whether it is there.
    #[test]
    fn a_finished_transaction_is_reported_without_being_reapplied() {
        let layout = Scratch::new();
        put(&layout.seed.join("a.txt"), b"edited since\n", 0o644);
        layout.append(&[
            begin(4, "swept", 1),
            file_move("a.txt", b"published\n", 0o644),
            end(4),
        ]);
        let before = layout.log_bytes();
        let recovered = layout
            .recover()
            .expect("a finished transaction needs no source");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, Seq::new(4));
        assert_eq!(
            layout.read("a.txt"),
            b"edited since\n",
            "nothing was replayed"
        );
        assert_eq!(layout.log_bytes(), before);
    }

    /// A path deleted from the seed outside the log loses its whole history — every write to it
    /// and removal of it, in every transaction, not only the latest — while every other path
    /// keeps its own, and a transaction left with nothing is gone. The rewritten log is a valid
    /// one that a second recovery reads back unchanged.
    #[test]
    fn an_externally_deleted_path_loses_its_whole_history() {
        let layout = Scratch::new();
        put(&layout.work("job1").join("gone"), b"first\n", 0o644);
        put(&layout.work("job1").join("kept"), b"kept\n", 0o644);
        layout
            .publish("job1", 1, &[write("gone"), write("kept")])
            .expect("publish both");
        layout
            .publish("job2", 2, &[remove("gone")])
            .expect("remove through the log");
        put(&layout.work("job3").join("gone"), b"again\n", 0o644);
        layout
            .publish("job3", 3, &[write("gone")])
            .expect("recreate through the log");
        std::fs::remove_file(layout.seed.join("gone")).expect("delete outside the log");

        let recovered = layout.recover().expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, Seq::new(1));
        assert_eq!(recovered[0].ops, [write("kept")]);
        assert_eq!(
            outline(&layout.records()),
            ["BEGIN 1 1", "MOVE kept", "END 1"]
        );
        assert_eq!(layout.read("kept"), b"kept\n");

        let compacted = layout.log_bytes();
        let again = layout.recover().expect("recover the rewritten log");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].ops, recovered[0].ops);
        assert_eq!(layout.log_bytes(), compacted);
    }

    /// An absence the log itself explains is no deletion from outside: a removal of a directory's
    /// path, or a file moved over it, took what the log had put beneath it — but only beneath it,
    /// component by component. A later write beneath it makes that path expected again.
    #[test]
    fn a_logged_replacement_of_a_directory_explains_what_was_beneath_it() {
        let layout = Scratch::new();
        put(&layout.seed.join("e"), b"file\n", 0o644);
        let mut records = finished(
            1,
            vec![
                file_move("d/f", b"f\n", 0o644),
                file_move("d2/x", b"x\n", 0o644),
                file_move("e/f", b"f\n", 0o644),
            ],
        );
        records.extend(finished(
            2,
            vec![delete("d"), file_move("e", b"file\n", 0o644)],
        ));
        layout.append(&records);

        layout.recover().expect("recover");
        assert_eq!(
            outline(&layout.records()),
            [
                "BEGIN 1 2",
                "MOVE d/f",
                "MOVE e/f",
                "END 1",
                "BEGIN 2 2",
                "DELETE d",
                "MOVE e",
                "END 2",
            ],
            "only d2/x, which no logged removal explains, is forgotten"
        );

        std::fs::create_dir(layout.seed.join("d")).expect("d");
        layout.append(&finished(3, vec![file_move("d/f", b"f\n", 0o644)]));
        layout.recover().expect("recover");
        assert_eq!(
            outline(&layout.records()),
            [
                "BEGIN 1 1",
                "MOVE e/f",
                "END 1",
                "BEGIN 2 2",
                "DELETE d",
                "MOVE e",
                "END 2",
            ],
            "rewritten beneath d, d/f is expected again, and it is gone"
        );
    }

    /// A directory lost outside the log — removed, or replaced by a file so that what was beneath
    /// it is not even a directory's entry — takes the history of everything beneath it along; a
    /// sibling whose name merely extends it keeps its own, and so does a dangling symlink, which
    /// is there.
    #[test]
    fn a_directory_lost_outside_the_log_takes_its_history_along() {
        for replaced_by_a_file in [false, true] {
            let layout = Scratch::new();
            let work = layout.work("job0");
            put(&work.join("dir/a"), b"a\n", 0o644);
            put(&work.join("dir/b/c"), b"c\n", 0o644);
            put(&work.join("dir2/x"), b"x\n", 0o644);
            std::os::unix::fs::symlink("nowhere", work.join("link")).expect("dangling symlink");
            layout.publish_diff("job0", 1).expect("publish");
            std::fs::remove_dir_all(layout.seed.join("dir")).expect("remove outside the log");
            if replaced_by_a_file {
                put(&layout.seed.join("dir"), b"a file now\n", 0o644);
            }

            let recovered = layout.recover().expect("recover");
            let written: BTreeSet<&Path> = recovered[0]
                .ops
                .iter()
                .filter_map(|op| match op {
                    CommitOp::Write(path) => Some(path.as_path()),
                    _ => None,
                })
                .collect();
            let expected = BTreeSet::from([Path::new("dir2/x"), Path::new("link")]);
            assert_eq!(
                written, expected,
                "replaced by a file: {replaced_by_a_file}"
            );
            let records = layout.records();
            let logged: BTreeSet<&Path> = records
                .iter()
                .filter_map(|record| match record {
                    WalRecord::Move { to, .. } => Some(to.as_path()),
                    _ => None,
                })
                .collect();
            assert_eq!(logged, expected, "replaced by a file: {replaced_by_a_file}");
            layout.recover().expect("the rewritten log still reads");
        }
    }

    /// A path that cannot be looked up for any reason but its absence fails recovery, and the log
    /// is left as it was — the plainly missing path beside it included.
    #[test]
    fn an_unreadable_path_fails_recovery_before_anything_is_rewritten() {
        let layout = Scratch::new();
        std::os::unix::fs::symlink("loop", layout.seed.join("loop")).expect("a looping symlink");
        layout.append(&finished(
            1,
            vec![
                file_move("loop/f", b"f\n", 0o644),
                file_move("plain", b"p\n", 0o644),
            ],
        ));
        let before = layout.log_bytes();

        let error = layout
            .recover()
            .expect_err("a path that cannot be looked up");
        assert!(matches!(error, Error::Io(_)), "got {error:?}");
        assert_eq!(layout.log_bytes(), before);
    }

    /// A removal is replayed like a move is — and like a move it is exactly what was logged: the
    /// directory the removal empties stays.
    #[test]
    fn an_unfinished_delete_is_replayed() {
        let layout = Scratch::new();
        put(&layout.seed.join("src/gone.txt"), b"bye\n", 0o644);
        layout.append(&[begin(1, "job0", 1), delete("src/gone.txt")]);

        let recovered = layout.recover().expect("recover");
        assert!(!layout.seed.join("src/gone.txt").exists());
        assert!(layout.seed.join("src").is_dir(), "nothing is pruned");
        assert_eq!(recovered[0].ops, [remove("src/gone.txt")]);
    }

    /// A command that changed nothing still frames a transaction — in one write — so its sequence
    /// number is consumed and the caller's metadata recorded, while the seed is left as it was.
    #[test]
    fn a_transaction_with_no_operations_is_framed_and_changes_nothing() {
        let layout = Scratch::new();
        put(&layout.seed.join("a.txt"), b"seed\n", 0o644);
        let nowhere = layout.root.path().join("no-such-snapshot");
        layout
            .prepare(&nowhere, "job0", 1, "true", &[])
            .expect("prepare")
            .apply()
            .expect("apply an empty transaction");

        assert!(
            matches!(
                layout.records().as_slice(),
                [WalRecord::Begin { op_count: 0, .. }, WalRecord::End { .. }]
            ),
            "got {:?}",
            layout.records()
        );
        let recovered = layout.recover().expect("recover");
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].ops.is_empty());
        assert_eq!(recovered[0].meta, meta("true"));
        assert!(!layout.staging_left());
    }

    /// A symlink is published as a link, and the hash the log records for it is the hash of its
    /// target — the only content a link has.
    #[test]
    fn a_symlink_is_published_and_hashed_by_its_target() {
        let layout = Scratch::new();
        std::os::unix::fs::symlink("pointee.txt", layout.work("job0").join("link"))
            .expect("snapshot symlink");
        layout
            .publish("job0", 1, &[write("link")])
            .expect("publish");
        assert_eq!(
            std::fs::read_link(layout.seed.join("link")).expect("read the published link"),
            Path::new("pointee.txt")
        );
        assert!(layout.records().iter().any(|record| matches!(
            record,
            WalRecord::Move { sha1, kind: EntryKind::Symlink, mode: None, .. }
                if *sha1 == ContentHash::of(b"pointee.txt")
        )));
    }

    /// A half-applied change of kind replays to the end whichever way it went: a directory that
    /// already became a symlink, and a file that already became a directory, are recognized as
    /// the final kind a later operation of the same transaction makes them — and with the
    /// snapshot swept, their content alone proves it.
    #[test]
    fn a_half_applied_change_of_kind_replays() {
        let layout = Scratch::new();
        let work = layout.work("job0");
        put(&layout.seed.join("pointee.txt"), b"p\n", 0o644);
        put(&layout.seed.join("to_link/inner.txt"), b"dir-side\n", 0o644);
        put(&layout.seed.join("to_dir"), b"file-side\n", 0o644);
        put(&work.join("pointee.txt"), b"p\n", 0o644);
        std::os::unix::fs::symlink("pointee.txt", work.join("to_link")).expect("symlink");
        put(&work.join("to_dir/child.txt"), b"child\n", 0o644);
        directory(&work.join("to_dir"), 0o755);
        let ops = diff_trees(&layout.seed, &work).expect("diff");
        let prepared = layout
            .prepare(&work, "job0", 1, "reshape", &ops)
            .expect("prepare");
        // Everything but the END: the seed already has the final shape.
        let intent = prepared.frame[..prepared.intent].to_vec();
        prepared.apply().expect("apply");
        let expected = shape(&work);
        std::fs::write(&layout.log, &intent).expect("forget the END");
        std::fs::remove_dir_all(&work).expect("sweep the snapshot");

        for _ in 0..2 {
            layout.recover().expect("an idempotent replay");
            assert_eq!(shape(&layout.seed), expected);
        }
    }

    /// The child half of the crash tests: publishes its scratch layout's snapshot, and ends its
    /// own process at the crash point its parent named.
    fn crash_child(root: &Path) {
        let work = root.join("snap/job0");
        let seed = root.join("seed");
        let ops = diff_trees(&seed, &work).expect("diff");
        prepare(
            &seed,
            &work,
            &root.join("meta").join(LOG_FILE),
            &uid("job0"),
            Seq::new(1),
            &meta("crash"),
            &ops,
        )
        .expect("prepare")
        .apply()
        .expect("apply");
    }

    /// A seed and a snapshot that differ in every way a transaction can: removals of files and
    /// of a directory, a directory that becomes a symlink and one that becomes a file, a file that
    /// becomes a directory, new nested directories one of which ends unwritable, a directory
    /// whose mode changes, one that must be opened up to take a new file, a changed file, a
    /// symlink, and a file named like an old temporary.
    fn crash_layout() -> Scratch {
        let layout = Scratch::new();
        let seed = &layout.seed;
        let work = layout.work("job0");
        for tree in [seed, &work] {
            put(&tree.join("keep.txt"), b"keep\n", 0o644);
            put(&tree.join("ro/a.txt"), b"a\n", 0o644);
            directory(&tree.join("moded"), 0o755);
        }
        put(&seed.join("gone.txt"), b"gone\n", 0o644);
        put(&seed.join("old/x.txt"), b"x\n", 0o644);
        put(&seed.join("swap/inner.txt"), b"inner\n", 0o644);
        directory(&seed.join("to_file"), 0o755);
        put(&seed.join("to_dir"), b"file\n", 0o644);
        put(&seed.join("changed.txt"), b"before\n", 0o644);

        std::os::unix::fs::symlink("keep.txt", work.join("swap")).expect("symlink");
        put(&work.join("to_file"), b"now a file\n", 0o600);
        put(&work.join("to_dir/child.txt"), b"child\n", 0o644);
        put(&work.join("changed.txt"), b"after, and longer\n", 0o600);
        put(&work.join("new/deeper/run.sh"), b"#!/bin/sh\n", 0o755);
        directory(&work.join("new/deeper"), 0o500);
        directory(&work.join("new"), 0o750);
        directory(&work.join("moded"), 0o700);
        put(&work.join("ro/b.txt"), b"b\n", 0o644);
        std::os::unix::fs::symlink("changed.txt", work.join("link")).expect("symlink");
        put(&work.join("notes.txt.tmp-wal"), b"mine\n", 0o644);
        for tree in [seed, &work] {
            directory(&tree.join("ro"), 0o555);
        }
        layout
    }

    /// Runs `test` again as a crash child at `point` over a fresh [`crash_layout`], then recovers
    /// twice, requiring the seed to end exactly as the snapshot is — contents, modes, directory
    /// shape — with the transaction reported and closed. With `sweep`, the snapshot is gone before
    /// recovery, so only the content already in the seed can prove the transaction applied.
    ///
    /// In the re-executed child, which its parent marks with a scratch directory, the call
    /// publishes that scratch instead and must die at its crash point.
    fn crash_at(test: &str, point: Crash, sweep: bool) {
        if let Some(root) = std::env::var_os(CRASH_SCRATCH) {
            crash_child(Path::new(&root));
            panic!("the crash child outlived its crash point {point:?}");
        }
        let layout = crash_layout();
        let original = shape(&layout.seed);
        let expected = shape(&layout.work("job0"));
        let child = Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", test, "--test-threads=1"])
            .env(CRASH_AT, format!("{point:?}"))
            .env(CRASH_SCRATCH, layout.root.path())
            .output()
            .expect("run the crash child");
        assert_eq!(
            child.status.code(),
            Some(CRASH_STATUS),
            "the child dies at {point:?} and nowhere else: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        match point {
            Crash::Intent => assert_eq!(shape(&layout.seed), original, "nothing applied yet"),
            Crash::FirstOperation => {
                let partial = shape(&layout.seed);
                assert!(
                    partial != original && partial != expected,
                    "the child died between the first operation and the last"
                );
            }
            Crash::BeforeEnd | Crash::End => {
                assert_eq!(shape(&layout.seed), expected, "everything applied");
            }
        }
        if sweep {
            open_up(&layout.work("job0"));
            std::fs::remove_dir_all(layout.work("job0")).expect("sweep the snapshot");
        }
        for _ in 0..2 {
            let recovered = layout.recover().expect("recover");
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].meta, meta("crash"));
            assert_eq!(shape(&layout.seed), expected);
            assert!(!layout.staging_left());
            assert!(matches!(
                layout.records().last(),
                Some(WalRecord::End { seq }) if *seq == Seq::new(1)
            ));
        }
    }

    #[test]
    fn a_crash_after_the_durable_intent_replays_everything() {
        crash_at(
            "commit::tests::a_crash_after_the_durable_intent_replays_everything",
            Crash::Intent,
            false,
        );
    }

    #[test]
    fn a_crash_after_the_first_operation_replays_the_rest() {
        crash_at(
            "commit::tests::a_crash_after_the_first_operation_replays_the_rest",
            Crash::FirstOperation,
            false,
        );
    }

    #[test]
    fn a_crash_before_the_end_replays_idempotently() {
        crash_at(
            "commit::tests::a_crash_before_the_end_replays_idempotently",
            Crash::BeforeEnd,
            false,
        );
    }

    #[test]
    fn a_crash_before_the_end_recovers_after_the_snapshot_is_swept() {
        crash_at(
            "commit::tests::a_crash_before_the_end_recovers_after_the_snapshot_is_swept",
            Crash::BeforeEnd,
            true,
        );
    }

    #[test]
    fn a_crash_after_the_durable_end_changes_nothing() {
        crash_at(
            "commit::tests::a_crash_after_the_durable_end_changes_nothing",
            Crash::End,
            true,
        );
    }
}
