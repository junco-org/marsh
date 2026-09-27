//! The values a transaction is described in.
//!
//! Each is a type of its own rather than the primitive it is carried as, so a sequence number is
//! never taken for a count, a snapshot's uid for a path or a staging name, and a mode for any
//! `u32`. Every one of them is what the log records, in the log's own spelling: a newtype
//! serializes exactly as the primitive it wraps, and decoding one checks what the type promises,
//! so a record that breaks the promise fails to decode rather than reaching a replay.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::Error;

/// Longest single path component Linux accepts (`NAME_MAX`).
const NAME_MAX: usize = 255;

/// Prefix of every staging directory's name.
const STAGING_PREFIX: &str = ".marsh-wal-";

/// Prefix of every temporary inside a staging directory.
const PART_PREFIX: &str = "part-";

/// Displays each newtype exactly as the value it wraps.
macro_rules! display_as_inner {
    ($($name:ty),*) => {$(
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    )*};
}

display_as_inner!(Seq, SourceUid, Staging, ContentHash);

/// The position a transaction occupies in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// The sequence number `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number itself.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The uid of the snapshot a transaction's content comes from, which is also that snapshot's
/// directory name under the caller's snapshot root.
///
/// Always one plain path component — never empty, `.`, `..` or anything with a `/` or a NUL in
/// it — and short enough that the transaction's [`Staging`] directory, named after it, is a legal
/// name too.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SourceUid(String);

impl SourceUid {
    /// The uid `uid`.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Wal`] when `uid` is not one plain path component, or is too long for
    /// the staging directory named after it.
    pub fn new(uid: impl Into<String>) -> Result<Self, Error> {
        let uid = uid.into();
        let longest = NAME_MAX - STAGING_PREFIX.len() - u64::MAX.to_string().len() - 1;
        if uid.is_empty()
            || uid == "."
            || uid == ".."
            || uid.contains(['/', '\0'])
            || uid.len() > longest
        {
            return Err(Error::Wal(format!(
                "transaction uid {uid:?} is not one plain path component of at most {longest} bytes"
            )));
        }
        Ok(Self(uid))
    }

    /// The uid as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SourceUid {
    type Error = Error;

    fn try_from(uid: String) -> Result<Self, Error> {
        Self::new(uid)
    }
}

impl AsRef<Path> for SourceUid {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

/// The directory directly inside the seed that one transaction lands its writes through:
/// `.marsh-wal-<seq>-<uid>`.
///
/// It is created only after the transaction's intent is durable and removed before its `END`, so
/// every such directory that exists is named by a log frame. Its only entries are `part-<index>`
/// temporaries, one per write, named by the write's position in the transaction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Staging(String);

impl Staging {
    /// The staging directory of transaction `seq`, whose content comes from snapshot `uid`.
    #[must_use]
    pub fn of(seq: Seq, uid: &SourceUid) -> Self {
        Self(format!("{STAGING_PREFIX}{seq}-{uid}"))
    }

    /// The directory's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Name of the temporary the write at `index` lands through.
    pub(crate) fn part(index: usize) -> String {
        format!("{PART_PREFIX}{index}")
    }

    /// The write index a temporary called `name` belongs to, if it is one.
    pub(crate) fn part_index(name: &[u8]) -> Option<usize> {
        let digits = name.strip_prefix(PART_PREFIX.as_bytes())?;
        if digits.is_empty() || digits.first() == Some(&b'0') && digits.len() > 1 {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    }
}

impl TryFrom<String> for Staging {
    type Error = Error;

    fn try_from(name: String) -> Result<Self, Error> {
        if !name.starts_with(STAGING_PREFIX) || name.contains(['/', '\0']) || name.len() > NAME_MAX
        {
            return Err(Error::Wal(format!(
                "{name:?} is not the name of a staging directory"
            )));
        }
        Ok(Self(name))
    }
}

/// The hex-encoded `sha1` of an entry's content: a regular file's bytes, or a symlink's target.
///
/// Full length, never a prefix: this is what tells a replay "already applied" from "interrupted",
/// and it is compared, never typed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// The hash of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha1::digest(bytes)))
    }

    /// The hash of everything `input` reads, streamed through `buffer` and, when given, copied
    /// into `output` on the way.
    pub(crate) fn of_stream(
        mut input: File,
        mut output: Option<&mut File>,
        buffer: &mut [u8],
    ) -> Result<Self, Error> {
        let mut hasher = Sha1::new();
        loop {
            let read = match input.read(buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            let chunk = buffer.get(..read).unwrap_or_default();
            hasher.update(chunk);
            if let Some(output) = output.as_deref_mut() {
                output.write_all(chunk)?;
            }
        }
        Ok(Self(hex::encode(hasher.finalize())))
    }

    /// The hash as hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Permission bits of an entry: `st_mode & 0o7777`, the setuid, setgid and sticky bits included
/// and the file type excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Mode(u32);

impl Mode {
    /// The permission bits of `raw`; anything outside `0o7777` is dropped.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw & 0o7777)
    }

    /// The permission bits of an entry whose unfollowed metadata is `metadata`.
    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self::new(std::os::unix::fs::MetadataExt::mode(metadata))
    }

    /// The bits themselves.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether the owner may read, write and search.
    pub(crate) const fn owner_has_all(self) -> bool {
        self.0 & 0o700 == 0o700
    }

    /// These bits with the owner's read, write and search added.
    pub(crate) const fn with_owner_all(self) -> Self {
        Self(self.0 | 0o700)
    }
}

impl TryFrom<u32> for Mode {
    type Error = Error;

    fn try_from(raw: u32) -> Result<Self, Error> {
        if raw & !0o7777 != 0 {
            return Err(Error::Wal(format!(
                "mode {raw:#o} carries bits outside the permission mask"
            )));
        }
        Ok(Self(raw))
    }
}

impl From<Mode> for u32 {
    fn from(mode: Mode) -> Self {
        mode.0
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#o}", self.0)
    }
}

/// What a `MOVE` record publishes: the kind of entry at its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A regular file, published with its bytes and mode.
    File,
    /// A symbolic link, published with its target.
    Symlink,
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A uid is one directory name: anything a path join would read as more — or less — than
    /// one component is refused, and so is one too long to leave room for its staging name.
    #[test]
    fn a_uid_is_one_plain_component() {
        for good in ["job0", "a.b", "..x", "ümlaut"] {
            let uid = SourceUid::new(good).expect(good);
            let staging = Staging::of(Seq::new(u64::MAX), &uid);
            assert!(staging.as_str().len() <= NAME_MAX);
        }
        for bad in ["", ".", "..", "a/b", "/", "nul\0"] {
            assert!(SourceUid::new(bad).is_err(), "{bad:?}");
        }
        assert!(SourceUid::new("x".repeat(NAME_MAX)).is_err());
        assert!(serde_json::from_str::<SourceUid>("\"../escape\"").is_err());
        assert_eq!(
            serde_json::to_string(&SourceUid::new("job0").expect("uid")).expect("encode"),
            "\"job0\""
        );
    }

    /// Only `part-<index>` in its canonical spelling names a temporary.
    #[test]
    fn only_canonical_part_names_have_an_index() {
        assert_eq!(Staging::part_index(Staging::part(12).as_bytes()), Some(12));
        assert_eq!(Staging::part_index(b"part-0"), Some(0));
        for bad in [
            &b"part-"[..],
            b"part-01",
            b"part--1",
            b"part-x",
            b"parts-1",
            b"x",
        ] {
            assert_eq!(Staging::part_index(bad), None, "{bad:?}");
        }
    }

    /// A mode is masked when made and refused when decoded with bits it cannot have, so a log
    /// line carrying a file-type bit never reaches a replay.
    #[test]
    fn a_mode_is_masked_when_made_and_checked_when_decoded() {
        assert_eq!(Mode::new(0o104_755).bits(), 0o4755);
        assert_eq!(
            serde_json::to_string(&Mode::new(0o750)).expect("encode"),
            "488"
        );
        assert_eq!(
            serde_json::from_str::<Mode>("488").expect("decode"),
            Mode::new(0o750)
        );
        assert!(serde_json::from_str::<Mode>("16877").is_err());
    }
}
