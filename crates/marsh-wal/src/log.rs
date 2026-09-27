//! This crate's durable-log primitives: an append-only JSON Lines log, and the descriptor-relative
//! file operations a transaction replays.
//!
//! A transaction moves content from a frozen snapshot into the seed. It cannot use `rename(2)`
//! between the two — a rename cannot cross a subvolume boundary, and the snapshot is one — so every
//! write is a copy into a *transaction-owned staging directory* inside the seed, an fsync, and a
//! rename into place. That is what makes a crash unable to expose a half-copied file at a real
//! path, and why no temporary is ever named after — or left beside — a user's file.
//!
//! Every operation is idempotent: a write replaces the non-directory at its target, a removal
//! tolerates an absent path. Replaying a log that may have partly run is therefore safe, which is
//! the whole recovery contract. The operations here resolve nothing by path: each takes the
//! directory descriptors the caller walked to, so no symbolic link is ever followed through.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions, Permissions as FilePermissions};
use std::io::{BufWriter, ErrorKind, Write};
use std::marker::PhantomData;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::{Error, Tolerate};
use crate::tree::Kind;
use crate::types::{ContentHash, Mode as Permissions, Staging};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Suffix of the sibling a log's replacement is written through before it is renamed over it.
const REPLACEMENT_SUFFIX: &str = ".tmp-wal";

/// Append-only handle on a JSON Lines log of `R`.
///
/// The record type is on the handle, not on `append`, because a log file is one format: a record of
/// another shape written into `meta/wal.jsonl` would be a line recovery cannot parse, and there is
/// no caller for which that is a legal thing to do.
///
/// `PhantomData<fn(R)>` rather than `PhantomData<R>`: the handle owns no `R`, and this form is
/// `Send + Sync` whatever `R` is — which a caller holding one behind a `Mutex` inside an `Arc<dyn
/// Trait>` needs.
pub struct JsonLog<R> {
    /// The log file, opened for appending.
    file: File,
    /// The record type this log holds.
    _record: PhantomData<fn(R)>,
}

impl<R> JsonLog<R> {
    /// Opens (creating if absent) the log at `path`, durably creating its directory if needed.
    ///
    /// Every directory this creates is fsynced into its parent, and a log file this creates is
    /// fsynced into its directory, before this returns: an intent appended to the log is durable
    /// only if the log itself is reachable after a crash, and seed mutations follow that intent.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Io`] when a directory or the file cannot be created, opened or synced.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let directory = parent_of(path);
        create_directories(directory)?;
        let created = OpenOptions::new().append(true).create_new(true).open(path);
        let file = match created.tolerate(&[ErrorKind::AlreadyExists])? {
            Some(file) => {
                File::open(directory)?.sync_all()?;
                file
            }
            None => OpenOptions::new().append(true).open(path)?,
        };
        Ok(Self {
            file,
            _record: PhantomData,
        })
    }

    /// Appends `records` as one write and forces them to disk before returning.
    ///
    /// One write and one fsync for the whole batch: a log is durable as a unit, and paying an
    /// fsync per line would make a transaction's cost proportional to the files it touched.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Wal`] when a record cannot be serialized — which is what a path no
    /// JSON string can carry produces — and with [`Error::Io`] when the write or the fsync fails.
    pub fn append(&mut self, records: &[R]) -> Result<(), Error>
    where
        R: Serialize,
    {
        let mut bytes = Vec::new();
        for record in records {
            write_record(&mut bytes, record)?;
        }
        self.write_durably(&bytes)
    }

    /// Writes already-encoded records as one write, then fsyncs.
    pub(crate) fn write_durably(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.file.write_all(bytes)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Every complete record of the log at `path`; an absent log reads as empty.
    ///
    /// A torn final line — the only corruption an append-and-fsync log can produce — is truncated
    /// away so the next append starts from a clean record boundary. A newline-terminated record
    /// that fails to parse or to typecheck as `R` is a different kind of corruption, and it is
    /// refused: the log is left byte for byte as it was, every record after the bad one
    /// included, because a prefix of a history is not a history and discarding the rest would
    /// forget whatever it recorded.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Wal`], naming the record's byte offset and the decoding error, when a
    /// complete record cannot be decoded, and with [`Error::Io`] when the log cannot be read or a
    /// torn tail cannot be truncated.
    pub fn read(path: &Path) -> Result<Vec<R>, Error>
    where
        R: DeserializeOwned,
    {
        Ok(read_records(path)?
            .into_iter()
            .map(|(_, record)| record)
            .collect())
    }
}

/// [`JsonLog::read`], each record paired with the byte offset its line starts at.
pub(crate) fn read_records<R: DeserializeOwned>(path: &Path) -> Result<Vec<(u64, R)>, Error> {
    let Some(bytes) = std::fs::read(path).tolerate(&[ErrorKind::NotFound])? else {
        return Ok(Vec::new());
    };

    let mut records = Vec::new();
    let mut offset = 0_u64;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let Some(record) = line.strip_suffix(b"\n") else {
            truncate(path, offset)?;
            break;
        };
        if !record.is_empty() {
            let parsed = serde_json::from_slice::<R>(record).map_err(|error| {
                Error::Wal(format!(
                    "{}: record at byte {offset} cannot be decoded: {error}",
                    path.display()
                ))
            })?;
            records.push((offset, parsed));
        }
        offset += line.len() as u64;
    }
    Ok(records)
}

/// Truncates the log at `path` to `len` bytes and forces the truncation to disk.
pub(crate) fn truncate(path: &Path, len: u64) -> Result<(), Error> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

/// Serializes `record` as one line onto `writer`.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when the record cannot be serialized — which is what a path no JSON
/// string can carry produces — and with [`Error::Io`] when `writer` fails.
pub(crate) fn write_record<W: Write + ?Sized, R: Serialize>(
    writer: &mut W,
    record: &R,
) -> Result<(), Error> {
    serde_json::to_writer(&mut *writer, record).map_err(|error| match error.io_error_kind() {
        Some(kind) => Error::Io(std::io::Error::new(kind, error)),
        None => Error::Wal(format!("serialize record: {error}")),
    })?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Replaces the log at `path`, whole, with what `encode` writes — never truncating it in place.
///
/// The replacement is written through a sibling temporary named after the log, in the log's own
/// directory, which must already exist: a fresh owner-only file, given the log's permission bits
/// when the log exists, fsynced, then renamed over the log, and the directory fsynced. A crash
/// before the rename leaves the old log authoritative and the temporary unread; a crash after it
/// leaves the whole replacement. A temporary an earlier attempt left behind is removed first —
/// unless it is a directory, which fails the replacement and is left alone.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when `path` names no file, with whatever `encode` fails with, and
/// with [`Error::Io`] when the temporary cannot be removed, written, synced or renamed, or the
/// directory cannot be synced — the last after the replacement is already in place.
pub(crate) fn replace_log(
    path: &Path,
    encode: impl FnOnce(&mut dyn Write) -> Result<(), Error>,
) -> Result<(), Error> {
    let Some(name) = path.file_name() else {
        return Err(Error::Wal(format!(
            "log target {} has no name",
            path.display()
        )));
    };
    let mut temporary = name.to_os_string();
    temporary.push(REPLACEMENT_SUFFIX);
    let temporary = path.with_file_name(temporary);
    std::fs::remove_file(&temporary).tolerate(&[ErrorKind::NotFound])?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    if let Err(error) = write_replacement(file, path, encode)
        .and_then(|()| std::fs::rename(&temporary, path).map_err(Error::from))
    {
        // Only this attempt's own file: `create_new` refused anything already there.
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    File::open(parent_of(path))?.sync_all()?;
    Ok(())
}

/// Streams `encode` into `file`, gives it the permission bits of the log at `path` when there is
/// one, and forces it to disk.
fn write_replacement(
    file: File,
    path: &Path,
    encode: impl FnOnce(&mut dyn Write) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut writer = BufWriter::new(file);
    encode(&mut writer)?;
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))?;
    if let Some(metadata) = std::fs::metadata(path).tolerate(&[ErrorKind::NotFound])? {
        let mode = metadata.permissions().mode() & 0o7777;
        file.set_permissions(FilePermissions::from_mode(mode))?;
    }
    file.sync_all()?;
    Ok(())
}

/// The directory a log at `path` lives in; `.` for a bare file name.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Creates `directory` and every missing ancestor, one component at a time, fsyncing each new
/// entry into its parent so none of them can vanish after a crash.
fn create_directories(directory: &Path) -> Result<(), Error> {
    let mut missing = Vec::new();
    let mut current = directory;
    loop {
        match std::fs::metadata(current).tolerate(&[ErrorKind::NotFound])? {
            Some(metadata) if metadata.is_dir() => break,
            Some(_) => return Err(std::io::Error::from(ErrorKind::NotADirectory).into()),
            None => {
                missing.push(current);
                current = parent_of(current);
            }
        }
    }
    for created in missing.into_iter().rev() {
        std::fs::create_dir(created).tolerate(&[ErrorKind::AlreadyExists])?;
        File::open(parent_of(created))?.sync_all()?;
    }
    Ok(())
}

/// Chunk size of the one buffer a transaction reads files through, and of each side's buffer
/// when a diff compares two files.
pub(crate) const CHUNK: usize = 64 * 1024;

/// Opens the regular file `name` of `directory` for reading.
///
/// Never blocks and never follows a link: `O_NONBLOCK` keeps a fifo swapped into its place from
/// stalling the open, `O_NOFOLLOW` refuses a symlink, and the descriptor is checked to really be a
/// regular file before anything reads it.
pub(crate) fn open_regular(directory: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Error> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
    let file = rustix::fs::openat(directory, name, flags | OFlags::CLOEXEC, Mode::empty())?;
    if FileType::from_raw_mode(rustix::fs::fstat(&file)?.st_mode) != FileType::RegularFile {
        return Err(Error::Wal(format!(
            "{} is no longer a regular file",
            Path::new(name).display()
        )));
    }
    Ok(file)
}

/// What a removal found at its target.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The entry was there and is gone now.
    Removed,
    /// Nothing was there.
    Absent,
    /// Something of the other kind is there — a directory for a leaf removal, a non-directory
    /// for a directory's — and was left alone.
    WrongKind,
}

/// Replaces the non-directory at `name` of `target` with a copy of the file or symlink `name` of
/// `source`, landing it through the write's temporary `part-<index>` of `staging`.
///
/// A regular file's bytes are hashed as they are copied, and the copy is renamed into place only
/// if they are the bytes `hash` names: whatever the source tree's mutability, what reaches the seed
/// is what the log describes. The copy is fsynced before the rename; the rename's two directories
/// are the caller's to fsync before it records the transaction finished. A symlink is recreated,
/// never followed, and hashed by its target.
///
/// # Errors
///
/// Fails with [`Error::Wal`] when the source is not a file or symlink, or no longer carries the
/// logged content, and with [`Error::Io`] when any copy, fsync or rename fails.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_write(
    source: BorrowedFd<'_>,
    source_name: &OsStr,
    kind: Kind,
    mode: Permissions,
    staging: BorrowedFd<'_>,
    index: usize,
    target: BorrowedFd<'_>,
    target_name: &OsStr,
    hash: &ContentHash,
    buffer: &mut [u8],
) -> Result<(), Error> {
    let part = Staging::part(index);
    let copied = match kind {
        Kind::File => {
            let input = File::from(open_regular(source, source_name)?);
            let mut output = File::from(rustix::fs::openat(
                staging,
                part.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?);
            let copied = ContentHash::of_stream(input, Some(&mut output), buffer)?;
            rustix::fs::fchmod(&output, Mode::from_raw_mode(mode.bits()))?;
            output.sync_all()?;
            copied
        }
        Kind::Symlink => {
            let link = rustix::fs::readlinkat(source, source_name, Vec::new())?;
            rustix::fs::symlinkat(link.as_c_str(), staging, part.as_str())?;
            ContentHash::of(link.as_bytes())
        }
        Kind::Directory | Kind::Special => {
            return Err(Error::Wal(format!(
                "{} is neither a regular file nor a symlink",
                Path::new(source_name).display()
            )));
        }
    };
    if copied != *hash {
        rustix::fs::unlinkat(staging, part.as_str(), AtFlags::empty())?;
        return Err(Error::Wal(format!(
            "{} no longer carries the content the log records for it",
            Path::new(source_name).display()
        )));
    }
    rustix::fs::renameat(staging, part.as_str(), target, target_name)?;
    Ok(())
}

/// Removes `name` of `parent` without recursing: the non-directory it names, or when `directory`
/// the empty directory it names — never its contents. Its parent is the caller's to fsync.
///
/// # Errors
///
/// Fails with [`Error::Io`] when the removal fails for any reason other than the entry being
/// absent or of the other kind; in particular, a nonempty directory is not removed.
pub(crate) fn apply_remove(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    directory: bool,
) -> Result<Removal, Error> {
    let (flags, wrong_kind) = if directory {
        (AtFlags::REMOVEDIR, Errno::NOTDIR)
    } else {
        (AtFlags::empty(), Errno::ISDIR)
    };
    match rustix::fs::unlinkat(parent, name, flags) {
        Ok(()) => Ok(Removal::Removed),
        Err(Errno::NOENT) => Ok(Removal::Absent),
        Err(error) if error == wrong_kind => Ok(Removal::WrongKind),
        Err(error) => Err(error.into()),
    }
}

/// Creates the directory `name` of `directory`, `0700`; one already there is left as it is. The
/// entry's kind, its final mode and its parent's fsync are the caller's.
///
/// # Errors
///
/// Fails with [`Error::Io`] when the creation fails for any reason other than the name existing.
pub(crate) fn apply_mkdir(directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Error> {
    match rustix::fs::mkdirat(directory, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Grants the owner read, write and search on the directory `name` of `directory`, whose
/// permission bits are `mode`, when it lacks any of them.
pub(crate) fn widen(
    directory: BorrowedFd<'_>,
    name: &OsStr,
    mode: Permissions,
) -> Result<(), Error> {
    if !mode.owner_has_all() {
        rustix::fs::chmodat(
            directory,
            name,
            Mode::from_raw_mode(mode.with_owner_all().bits()),
            AtFlags::empty(),
        )?;
    }
    Ok(())
}

/// Opens `directory` for reading and fsyncs it, first setting its permission bits to `mode`
/// when given: one descriptor, opened while the directory is still readable, carries both.
pub(crate) fn sync_directory(directory: impl AsFd, mode: Option<Permissions>) -> Result<(), Error> {
    if let Some(mode) = mode {
        rustix::fs::fchmod(&directory, Mode::from_raw_mode(mode.bits()))?;
    }
    rustix::fs::fsync(directory)?;
    Ok(())
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::testing::{chmod, mode_of};

    /// A minimal record type: the log primitives are generic, so the shape under test only has to
    /// round-trip.
    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Line {
        /// Sequence number, so a torn tail is identifiable by what survives.
        seq: u64,
    }

    /// A fresh scratch directory, and the path `name` inside it.
    fn scratch(name: &str) -> (tempfile::TempDir, PathBuf) {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let path = scratch.path().join(name);
        (scratch, path)
    }

    /// Appends one record per sequence number of `seqs` to the log at `path`, opening it first.
    fn append(path: &Path, seqs: &[u64]) {
        let records: Vec<Line> = seqs.iter().map(|&seq| Line { seq }).collect();
        JsonLog::open(path)
            .expect("open log")
            .append(&records)
            .expect("append");
    }

    /// The sequence numbers of every record of the log at `path`.
    fn read(path: &Path) -> Vec<u64> {
        let records = JsonLog::<Line>::read(path).expect("read log");
        records.into_iter().map(|line| line.seq).collect()
    }

    /// The length in bytes of the file at `path`.
    fn len(path: &Path) -> u64 {
        std::fs::metadata(path).expect("stat").len()
    }

    /// A directory opened for the descriptor-relative helpers.
    fn open_dir(path: &Path) -> OwnedFd {
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open directory")
    }

    /// [`apply_write`] of the `kind` entry `source` of `root` onto `target` beside it with `mode`,
    /// landing through the temporary `index` of `root/staging` and requiring the content `hash`.
    fn write(
        root: &Path,
        source: &str,
        kind: Kind,
        mode: u32,
        index: usize,
        target: &str,
        hash: &[u8],
    ) -> Result<(), Error> {
        std::fs::create_dir_all(root.join("staging"))?;
        let (dir, staging) = (open_dir(root), open_dir(&root.join("staging")));
        apply_write(
            dir.as_fd(),
            OsStr::new(source),
            kind,
            Permissions::new(mode),
            staging.as_fd(),
            index,
            dir.as_fd(),
            OsStr::new(target),
            &ContentHash::of(hash),
            &mut vec![0; CHUNK],
        )
    }

    /// A crash can only ever tear the *last* line — midway through a record, after a complete
    /// JSON value that lacks its terminating newline, or midway through a UTF-8 code point — and
    /// the log is cut back to its last complete record and has to keep accepting appends
    /// afterwards: otherwise one interrupted write would end the session.
    #[test]
    fn a_torn_tail_is_truncated_and_appends_continue() {
        for (case, seqs, tail) in [
            (
                "a_torn_final_record_is_truncated_away",
                &[1, 2][..],
                &br#"{"seq":3"#[..],
            ),
            (
                "complete_json_without_a_newline_is_truncated",
                &[1][..],
                &br#"{"seq":2}"#[..],
            ),
            (
                "truncated_utf8_tail_is_repaired",
                &[1][..],
                &b"{\"seq\":2,\"text\":\"\xf0\x9f"[..],
            ),
        ] {
            let (_scratch, path) = scratch("log.jsonl");
            append(&path, seqs);
            let complete = len(&path);
            let mut raw = std::fs::read(&path).expect("read log");
            raw.extend_from_slice(tail);
            std::fs::write(&path, raw).expect("simulate a torn append");

            assert_eq!(read(&path), seqs, "{case}: read past the torn tail");
            assert_eq!(
                len(&path),
                complete,
                "{case}: truncated to its last complete record"
            );
            let next = seqs.last().expect("a record") + 1;
            append(&path, &[next]);
            assert_eq!(
                read(&path),
                [seqs, &[next]].concat(),
                "{case}: append after repair"
            );
        }
    }

    /// A newline-terminated record that fails to parse, or parses but does not typecheck, is
    /// refused with its offset — and nothing is repaired: the valid prefix, the bad record, the
    /// valid suffix after it and even a torn tail after that all survive byte for byte, so a
    /// reader that kept only the prefix could not pass.
    #[test]
    fn an_undecodable_record_is_refused_and_every_byte_survives() {
        let (_scratch, path) = scratch("log.jsonl");
        for bad in [&b"{not-json}"[..], b"{\"seq\":\"seven\"}", b"{\"other\":1}"] {
            let mut raw = b"{\"seq\":7}\n".to_vec();
            let offset = raw.len();
            raw.extend_from_slice(bad);
            raw.extend_from_slice(b"\n{\"seq\":8}\n{\"seq\":9");
            std::fs::write(&path, &raw).expect("write corrupt log");

            let error = JsonLog::<Line>::read(&path).expect_err("a corrupt record is refused");
            assert!(
                matches!(&error, Error::Wal(message)
                    if message.contains(&format!("record at byte {offset} cannot be decoded"))),
                "got {error:?}"
            );
            assert_eq!(
                std::fs::read(&path).expect("reread the log"),
                raw,
                "the log is preserved exactly, valid suffix and torn tail included"
            );
        }
    }

    /// A log beneath directories that do not exist yet gets them, and the log itself.
    #[test]
    fn a_log_opens_beneath_missing_directories() {
        let (_scratch, path) = scratch("state/meta/deeper/wal.jsonl");
        append(&path, &[1]);
        assert_eq!(read(&path), [1]);
        JsonLog::<Line>::open(&path).expect("reopening an existing log keeps it");
        assert_eq!(read(&path), [1]);
    }

    /// A log path cannot be opened under a plain file, and the failure is I/O, not corruption.
    #[test]
    fn a_log_under_a_file_cannot_be_opened() {
        let (_scratch, blocker) = scratch("meta");
        std::fs::write(&blocker, b"not a directory\n").expect("blocking file");
        for path in [blocker.join("deeper/wal.jsonl"), blocker.join("wal.jsonl")] {
            assert!(
                matches!(JsonLog::<Line>::open(&path), Err(Error::Io(_))),
                "{path:?}"
            );
        }
    }

    /// A replacement that fails before it is whole — its encoder fails partway, or its temporary's
    /// name is taken by a directory — leaves the log exactly as it was and the directory alone.
    /// One that succeeds replaces the log whole, over whatever a previous attempt left, and keeps
    /// its permission bits; an empty one leaves an empty log that still takes appends.
    #[test]
    fn a_log_is_replaced_whole_or_not_at_all() {
        let (_scratch, path) = scratch("wal.jsonl");
        let temporary = path.with_file_name("wal.jsonl.tmp-wal");
        append(&path, &[1, 2]);
        chmod(&path, 0o640);
        let before = std::fs::read(&path).expect("read log");

        let error = replace_log(&path, |writer| {
            write_record(writer, &Line { seq: 9 })?;
            writer.write_all(br#"{"seq":"#)?;
            Err(Error::Wal("the encoder gave up".to_string()))
        })
        .expect_err("a failed encoder");
        assert!(matches!(error, Error::Wal(_)), "got {error:?}");
        assert_eq!(std::fs::read(&path).expect("read log"), before);
        assert!(!temporary.exists(), "the attempt's own temporary is gone");

        std::fs::create_dir(&temporary).expect("a directory where the temporary goes");
        let error = replace_log(&path, |writer| write_record(writer, &Line { seq: 9 }))
            .expect_err("a directory in the temporary's place");
        assert!(matches!(error, Error::Io(_)), "got {error:?}");
        assert_eq!(std::fs::read(&path).expect("read log"), before);
        assert!(temporary.is_dir(), "the blocking directory is not removed");

        std::fs::remove_dir(&temporary).expect("remove the blocker");
        std::fs::write(&temporary, br#"{"seq":"#).expect("a stale partial temporary");
        replace_log(&path, |writer| {
            write_record(writer, &Line { seq: 3 })?;
            write_record(writer, &Line { seq: 4 })
        })
        .expect("replace");
        assert_eq!(read(&path), [3, 4]);
        assert_eq!(mode_of(&path), 0o640);
        assert!(!temporary.exists());

        replace_log(&path, |_| Ok(())).expect("replace with nothing");
        assert_eq!(len(&path), 0);
        append(&path, &[5]);
        assert_eq!(read(&path), [5]);
    }

    /// A session that never logged anything reads as an empty history, and a log that cannot be
    /// read at all is an I/O failure rather than an empty one.
    #[test]
    fn an_absent_log_reads_empty_and_an_unreadable_one_fails() {
        let (dir, path) = scratch("never-written.jsonl");
        assert!(read(&path).is_empty());

        let directory = dir.path().join("directory.jsonl");
        std::fs::create_dir(&directory).expect("directory in the log's place");
        assert!(matches!(
            JsonLog::<Line>::read(&directory),
            Err(Error::Io(_))
        ));
    }

    /// An empty batch costs neither a write nor an fsync: a transaction that changed nothing must
    /// not grow the log.
    #[test]
    fn an_empty_batch_does_not_touch_the_log() {
        let (_scratch, path) = scratch("log.jsonl");
        let mut log = JsonLog::open(&path).expect("open log");
        log.append(&[Line { seq: 1 }]).expect("append");
        let before = len(&path);

        log.append(&[]).expect("an empty batch is a no-op");
        assert_eq!(len(&path), before, "nothing was appended");
    }

    /// A record that cannot be serialized fails the append, and fails it before anything reaches
    /// the log: a half-written line is exactly what the format cannot survive.
    #[test]
    fn an_unserializable_record_fails_the_append_without_writing() {
        /// A record whose serialization always fails, standing in for the real case — a path no
        /// JSON string can carry.
        struct Unserializable;

        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(<S::Error as serde::ser::Error>::custom("nope"))
            }
        }

        let (_scratch, path) = scratch("log.jsonl");
        let mut log = JsonLog::<Unserializable>::open(&path).expect("open log");

        let error = log
            .append(&[Unserializable])
            .expect_err("an unserializable record cannot be logged");
        assert!(
            matches!(&error, Error::Wal(message) if message.starts_with("serialize record:")),
            "got {error:?}"
        );
        assert_eq!(
            len(&path),
            0,
            "the batch is serialized whole before any of it is written"
        );
    }

    /// A blank line carries no record; it is skipped, and it neither ends the log nor triggers the
    /// torn-tail repair that would throw away everything after it.
    #[test]
    fn a_blank_line_is_skipped_not_treated_as_a_torn_tail() {
        let (_scratch, path) = scratch("log.jsonl");
        std::fs::write(&path, b"{\"seq\":1}\n\n{\"seq\":2}\n")
            .expect("write log with a blank line");
        let before = len(&path);

        assert_eq!(read(&path), [1, 2]);
        assert_eq!(len(&path), before, "nothing was truncated");
    }

    /// A removal is a leaf's: it tolerates an absent entry and leaves a directory alone, and a
    /// directory removal takes only an empty directory — never contents.
    #[test]
    fn removals_are_nonrecursive() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        std::fs::create_dir_all(root.join("full/inner")).expect("nested dirs");
        std::fs::write(root.join("full/inner/leaf"), b"x").expect("leaf");
        std::fs::create_dir(root.join("empty")).expect("empty dir");
        std::fs::write(root.join("file"), b"x").expect("file");
        let dir = open_dir(root);
        let remove = |name: &str, directory| apply_remove(dir.as_fd(), OsStr::new(name), directory);

        for (name, found) in [
            ("full", Removal::WrongKind),
            ("absent", Removal::Absent),
            ("file", Removal::Removed),
        ] {
            assert_eq!(
                remove(name, false).expect(name),
                found,
                "leaf removal of {name}"
            );
        }
        assert!(matches!(
            remove("full", true),
            Err(Error::Io(error)) if error.raw_os_error() == Some(libc::ENOTEMPTY)
        ));
        assert!(
            root.join("full/inner/leaf").exists(),
            "contents are never removed"
        );
        assert_eq!(remove("empty", true).expect("rmdir"), Removal::Removed);
        std::fs::write(root.join("file"), b"x").expect("file again");
        assert_eq!(
            remove("file", true).expect("rmdir of a file"),
            Removal::WrongKind
        );
    }

    /// A write of anything but a file or a symlink is refused before any open: a fifo with no
    /// writer would block an open forever, and no copy of it is the entry anyway.
    #[test]
    fn a_special_source_is_refused_without_opening_it() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            root.join("fifo"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .expect("mkfifo");

        for kind in [Kind::Special, Kind::File] {
            let error = write(root, "fifo", kind, 0o644, 0, "out", b"")
                .expect_err("a fifo is not publishable");
            assert!(matches!(error, Error::Wal(_)), "got {error:?}");
        }
        assert!(!root.join("out").exists());
        assert_eq!(
            std::fs::read_dir(root.join("staging"))
                .expect("staging")
                .count(),
            0,
            "and nothing is left in staging"
        );
    }

    /// The bytes that land are the bytes the log names: a source that changed after its hash
    /// was logged is refused before the rename, leaving the target and staging untouched.
    #[test]
    fn a_write_whose_source_changed_is_refused_before_the_rename() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        std::fs::write(root.join("source"), b"changed").expect("source");
        std::fs::write(root.join("target"), b"old").expect("target");

        let error = write(root, "source", Kind::File, 0o644, 3, "target", b"logged")
            .expect_err("changed content");
        assert!(matches!(error, Error::Wal(_)), "got {error:?}");
        assert_eq!(std::fs::read(root.join("target")).expect("target"), b"old");
        assert!(!root.join("staging/part-3").exists());

        write(root, "source", Kind::File, 0o751, 3, "target", b"changed")
            .expect("matching content lands");
        assert_eq!(
            std::fs::read(root.join("target")).expect("target"),
            b"changed"
        );
        assert_eq!(mode_of(&root.join("target")), 0o751);
    }
}
