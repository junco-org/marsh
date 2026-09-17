//! What a command changed, recovered by comparing the seed against the command's work snapshot.
//!
//! The snapshot is a copy of the seed taken when the command started, so a difference between the
//! two is either the command's own work or a transaction that landed while it ran — which is what
//! the caller's staleness check decides between. Every path is seed-relative. Differences under
//! `.git/` count like any other, and must be committed for a git history to survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// One filesystem change to apply to the seed. Paths are seed-relative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOp {
    /// Copy the work snapshot's version of this path over the seed's.
    Write(PathBuf),
    /// Delete this path from the seed.
    Remove(PathBuf),
}

impl CommitOp {
    /// The path this operation names.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Write(path) | Self::Remove(path) => path,
        }
    }
}

/// A non-directory tree entry, at the granularity differences are detected.
enum Entry {
    /// A regular file: what it is, plus the identity that says whether it needs reading.
    File(FileMeta),
    /// A symbolic link, compared by target.
    Symlink(PathBuf),
    /// Anything else — fifo, socket, device — compared by mode alone.
    Other(u32),
}

/// A regular file, and the inode identity that can answer "unchanged" without opening it.
struct FileMeta {
    /// Size in bytes.
    len: u64,
    /// Permission bits.
    mode: u32,
    /// Inode number, then the modification and change timestamps as `(seconds, nanoseconds)`.
    ///
    /// `work` is a btrfs snapshot of the seed and a snapshot copies inode items verbatim: a file no
    /// command touched carries the same number and the same pair of stamps in both trees, and its
    /// bytes therefore cannot differ. A path a later merge rewrote gets a new inode instead —
    /// [`crate::log::apply_write`] lands every record through a temporary and a rename — so it
    /// falls through to the byte comparison and is reported only when its content really differs.
    /// `ctime` is in the tuple because it is the one stamp userspace cannot restore — `touch -r`
    /// puts back an `mtime`, nothing puts back a `ctime`.
    id: (u64, (i64, i64), (i64, i64)),
}

/// Computes the changes that turn `seed` into `work`.
///
/// Directories are implicit: a [`CommitOp::Write`] creates the parents it needs, and directories
/// left empty by a [`CommitOp::Remove`] are pruned. A command that creates an *empty* directory and
/// nothing else therefore merges as a no-op — the one tree shape this representation cannot carry.
///
/// Ordering is total and deterministic: removals first, deepest paths first (so a directory is
/// emptied before it is pruned), then writes shallowest first (so parents exist before children).
///
/// # Errors
///
/// Fails with [`Error::Io`] when either tree cannot be walked or a file cannot be read.
pub fn diff_trees(seed: &Path, work: &Path) -> Result<Vec<CommitOp>, Error> {
    let seed_entries = collect(seed)?;
    let work_entries = collect(work)?;

    let mut removes: Vec<PathBuf> = Vec::new();
    let mut writes: Vec<PathBuf> = Vec::new();

    for (path, seed_entry) in &seed_entries {
        match work_entries.get(path) {
            None => removes.push(path.clone()),
            Some(work_entry) => {
                if changed(seed_entry, work_entry, &seed.join(path), &work.join(path))? {
                    writes.push(path.clone());
                }
            }
        }
    }
    for path in work_entries.keys() {
        if !seed_entries.contains_key(path) {
            writes.push(path.clone());
        }
    }

    removes.sort_by(|left, right| depth_key(right).cmp(&depth_key(left)));
    writes.sort_by(|left, right| depth_key(left).cmp(&depth_key(right)));

    let mut ops = Vec::with_capacity(removes.len() + writes.len());
    ops.extend(removes.into_iter().map(CommitOp::Remove));
    ops.extend(writes.into_iter().map(CommitOp::Write));
    Ok(ops)
}

/// Sort key placing shallower paths first, ties broken by the paths' own component-wise ordering.
fn depth_key(path: &Path) -> (usize, &Path) {
    (path.components().count(), path)
}

/// Indexes every non-directory entry of `root` by its `root`-relative path.
///
/// The relative path is accumulated as the walk descends rather than recovered by stripping `root`
/// off an absolute path: a relative [`PathBuf`] is what the key is, and building it directly keeps
/// every byte of every component, which a string round-trip would not.
fn collect(root: &Path) -> Result<BTreeMap<PathBuf, Entry>, Error> {
    use std::os::unix::fs::MetadataExt;

    let mut entries = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let relative = prefix.join(entry.file_name());
            let metadata = entry.metadata()?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                stack.push((entry.path(), relative));
            } else if file_type.is_symlink() {
                entries.insert(relative, Entry::Symlink(std::fs::read_link(entry.path())?));
            } else if file_type.is_file() {
                entries.insert(
                    relative,
                    Entry::File(FileMeta {
                        len: metadata.len(),
                        mode: mode(&metadata),
                        id: (
                            metadata.ino(),
                            (metadata.mtime(), metadata.mtime_nsec()),
                            (metadata.ctime(), metadata.ctime_nsec()),
                        ),
                    }),
                );
            } else {
                entries.insert(relative, Entry::Other(mode(&metadata)));
            }
        }
    }
    Ok(entries)
}

/// Permission bits of an entry.
fn mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

/// Whether the two entries differ, opening the files only when their inode identities do not
/// already prove they are the same untouched file.
///
/// A mismatched identity is not a change: the diff tests build their trees as two independently
/// written directories, where every counterpart file has a different inode. It only means the
/// question has to be answered by reading.
fn changed(seed: &Entry, work: &Entry, seed_path: &Path, work_path: &Path) -> Result<bool, Error> {
    match (seed, work) {
        (Entry::File(seed_file), Entry::File(work_file)) => {
            if seed_file.mode != work_file.mode {
                return Ok(true);
            }
            if seed_file.id == work_file.id {
                return Ok(false);
            }
            if seed_file.len != work_file.len {
                return Ok(true);
            }
            contents_differ(seed_path, work_path)
        }
        (Entry::Symlink(seed_target), Entry::Symlink(work_target)) => {
            Ok(seed_target != work_target)
        }
        (Entry::Other(seed_mode), Entry::Other(work_mode)) => Ok(seed_mode != work_mode),
        // A path that is a file on one side and a symlink on the other is a change of kind.
        _ => Ok(true),
    }
}

/// Compares two regular files byte for byte, stopping at the first difference.
fn contents_differ(seed: &Path, work: &Path) -> Result<bool, Error> {
    use std::io::{BufRead, BufReader};

    /// Chunk size of each side's buffer.
    const CHUNK: usize = 64 * 1024;

    let mut seed = BufReader::with_capacity(CHUNK, std::fs::File::open(seed)?);
    let mut work = BufReader::with_capacity(CHUNK, std::fs::File::open(work)?);
    loop {
        let left = seed.fill_buf()?;
        let right = work.fill_buf()?;
        if left.is_empty() || right.is_empty() {
            // One side ended: they match only if the other ended too.
            return Ok(left.len() != right.len());
        }
        let len = left.len().min(right.len());
        if left[..len] != right[..len] {
            return Ok(true);
        }
        seed.consume(len);
        work.consume(len);
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().expect("has a parent")).expect("create parents");
        std::fs::write(target, contents).expect("write file");
    }

    #[test]
    fn detects_creations_modifications_and_deletions() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        write(&seed, "src/keep.txt", "same");
        write(&seed, "src/change.txt", "old");
        write(&seed, "src/gone.txt", "bye");
        write(&seed, ".git/index", "old-index");
        write(&work, "src/keep.txt", "same");
        write(&work, "src/change.txt", "new-longer");
        write(&work, "src/new/deep.txt", "fresh");
        write(&work, ".git/index", "new-index");

        let ops = diff_trees(&seed, &work).expect("diff");
        assert_eq!(
            ops,
            vec![
                CommitOp::Remove(PathBuf::from("src/gone.txt")),
                CommitOp::Write(PathBuf::from(".git/index")),
                CommitOp::Write(PathBuf::from("src/change.txt")),
                CommitOp::Write(PathBuf::from("src/new/deep.txt")),
            ],
            "git metadata merges like any other path; unchanged paths are absent"
        );
    }

    #[test]
    fn detects_same_length_content_changes_and_mode_changes() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        write(&seed, "a.txt", "aaa");
        write(&work, "a.txt", "bbb");
        write(&seed, "b.sh", "x");
        write(&work, "b.sh", "x");
        std::fs::set_permissions(
            work.join("b.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");

        let ops = diff_trees(&seed, &work).expect("diff");
        assert_eq!(
            ops,
            vec![
                CommitOp::Write(PathBuf::from("a.txt")),
                CommitOp::Write(PathBuf::from("b.sh")),
            ],
            "equal length is not equal content, and mode is part of the entry"
        );
    }

    #[test]
    fn orders_deep_removals_before_shallow_ones_and_parents_before_children() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        write(&seed, "d/x/deep.txt", "gone");
        write(&seed, "d/mid.txt", "gone");
        write(&seed, "top.txt", "gone");
        write(&work, "n/n2/leaf.txt", "new");
        write(&work, "n/one.txt", "new");
        write(&work, "root.txt", "new");

        let ops = diff_trees(&seed, &work).expect("diff");
        assert_eq!(
            ops,
            vec![
                CommitOp::Remove(PathBuf::from("d/x/deep.txt")),
                CommitOp::Remove(PathBuf::from("d/mid.txt")),
                CommitOp::Remove(PathBuf::from("top.txt")),
                CommitOp::Write(PathBuf::from("root.txt")),
                CommitOp::Write(PathBuf::from("n/one.txt")),
                CommitOp::Write(PathBuf::from("n/n2/leaf.txt")),
            ]
        );
    }

    #[test]
    fn detects_symlink_target_changes_and_type_changes() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        write(&seed, "target1", "one");
        write(&work, "target1", "one");
        std::fs::create_dir_all(&seed).expect("seed dir");
        std::os::unix::fs::symlink("target1", seed.join("link")).expect("seed symlink");
        std::os::unix::fs::symlink("target2", work.join("link")).expect("work symlink");
        // A directory in the seed becomes a plain file in the work snapshot.
        write(&seed, "swap/inner.txt", "dir-side");
        write(&work, "swap", "file-side");

        let ops = diff_trees(&seed, &work).expect("diff");
        assert_eq!(
            ops,
            vec![
                CommitOp::Remove(PathBuf::from("swap/inner.txt")),
                CommitOp::Write(PathBuf::from("link")),
                CommitOp::Write(PathBuf::from("swap")),
            ],
            "a retargeted symlink is a write; a directory replaced by a file empties then writes"
        );
    }

    /// A name that is not UTF-8 is a perfectly legal filename, and the seed-relative key has to
    /// carry it verbatim — a lossy string round-trip would name a path that does not exist.
    #[test]
    fn a_non_utf8_name_survives_the_diff() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        std::fs::create_dir_all(&seed).expect("seed dir");
        std::fs::create_dir_all(&work).expect("work dir");
        let name = OsStr::from_bytes(b"bad\xff");
        std::fs::write(work.join(name), b"new\n").expect("write the odd name");

        let ops = diff_trees(&seed, &work).expect("diff");
        assert_eq!(ops, vec![CommitOp::Write(PathBuf::from(name))]);
        assert!(
            work.join(ops[0].path()).exists(),
            "the recorded path names the file it came from"
        );
    }

    #[test]
    fn identical_trees_produce_no_operations() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path();
        let seed = root.join("seed");
        let work = root.join("work");
        write(&seed, "src/a.txt", "same");
        write(&work, "src/a.txt", "same");
        assert!(diff_trees(&seed, &work).expect("diff").is_empty());
    }
}
