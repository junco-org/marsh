//! What a command changed, recovered by comparing a tree before the command against the tree it
//! left.
//!
//! The two trees are copies of the seed — a baseline taken when the command started, and the
//! snapshot it ran in — so a difference between them is the command's own work. Every path is
//! tree-relative. Differences under `.git/` count like any other, and must be committed for a git
//! history to survive.
//!
//! Every entry is compared as itself, directories included: a directory is an entry with a mode,
//! whether or not anything lives in it, so creating, removing or re-moding one is an operation of
//! its own and nothing is created or pruned implicitly.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::error::{ABSENT, Error, Tolerate};
use crate::tree::by_depth;
use crate::types::Mode;

/// One filesystem change to apply to the seed. Paths are seed-relative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOp {
    /// Replace whatever non-directory is at this path — or nothing — with the work tree's regular
    /// file or symlink there.
    Write(PathBuf),
    /// Delete the non-directory at this path.
    Remove(PathBuf),
    /// Create a directory, which ends with these permission bits.
    CreateDirectory {
        /// Seed-relative path.
        path: PathBuf,
        /// Final permission bits.
        mode: Mode,
    },
    /// Give an existing directory these permission bits.
    SetDirectoryMode {
        /// Seed-relative path.
        path: PathBuf,
        /// Final permission bits.
        mode: Mode,
    },
    /// Delete the directory at this path, which every earlier removal has emptied.
    RemoveDirectory(PathBuf),
}

impl CommitOp {
    /// The path this operation names.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Write(path)
            | Self::Remove(path)
            | Self::RemoveDirectory(path)
            | Self::CreateDirectory { path, .. }
            | Self::SetDirectoryMode { path, .. } => path,
        }
    }
}

/// A tree entry, at the granularity differences are detected.
enum Entry {
    /// A regular file: what it is, plus the identity that says whether it needs reading.
    File(FileMeta),
    /// A symbolic link, compared by target.
    Symlink(PathBuf),
    /// A directory, compared by its permission bits.
    Directory(Mode),
    /// Anything else — fifo, socket, device — compared by mode alone.
    Other(Mode),
}

/// A regular file, and the inode identity that can answer "unchanged" without opening it.
struct FileMeta {
    /// Size in bytes.
    len: u64,
    /// Permission bits.
    mode: Mode,
    /// Inode number, then the modification and change timestamps as `(seconds, nanoseconds)`.
    ///
    /// Both trees are btrfs snapshots of the seed and a snapshot copies inode items verbatim: a
    /// file no command touched carries the same number and the same pair of stamps in both, and
    /// its bytes therefore cannot differ. A path a merge rewrote gets a new inode instead — every
    /// write lands through a staged copy and a rename — so it falls through to the byte comparison
    /// and is reported only when its content really differs. `ctime` is in the tuple because it is
    /// the one stamp userspace cannot restore — `touch -r` puts back an `mtime`, nothing puts back
    /// a `ctime`.
    id: (u64, (i64, i64), (i64, i64)),
}

/// Computes the changes that turn `seed` into `work`.
///
/// A path present only in `seed` is a [`CommitOp::Remove`], or a [`CommitOp::RemoveDirectory`]
/// for a directory; one present only in `work` is a [`CommitOp::Write`], or a
/// [`CommitOp::CreateDirectory`] carrying its mode. A directory in both whose mode differs is a
/// [`CommitOp::SetDirectoryMode`]. A non-directory in both is a write when its kind, mode, target
/// or bytes differ — a write replaces a non-directory of any kind. A change between a directory
/// and anything else is the removal of the old entry and the creation of the new one. A fifo,
/// socket or device that appears or changes is reported as a write, which publication refuses:
/// the diff describes the tree, and what can be published is the log's decision.
///
/// Ordering is total and deterministic, and it is the order the operations can be applied in:
/// removals deepest first (so a directory is emptied before it is removed), then directory
/// creations shallowest first (so parents exist before children), then writes shallowest first,
/// then directory modes deepest first (so a directory that ends unwritable is closed only after
/// everything beneath it is in place).
///
/// # Errors
///
/// Fails with [`Error::Io`] when either tree cannot be walked or a file cannot be read.
pub fn diff_trees(seed: &Path, work: &Path) -> Result<Vec<CommitOp>, Error> {
    diff_paths(seed, work, std::iter::empty(), [Path::new("")])
}

/// [`diff_trees`] of only what lies at one of `paths` or at or beneath one of `trees`.
///
/// Exactly the operations of [`diff_trees`] whose path is one of `paths` or lies at or beneath one
/// of `trees`, in the same order — at the cost of reading only those. A path's own entry is looked
/// up and a tree is walked; nothing else in either tree is visited, so the cost follows the
/// footprint rather than the size of the seed. A path is found exactly when the whole-tree walk
/// would reach it: through real directories, never through a symbolic link. A path that is not
/// relative and normal names nothing in a tree, and neither does an empty one; an empty tree is
/// the whole of it.
///
/// # Errors
///
/// Fails with [`Error::Io`] when an entry in scope cannot be resolved, a tree in scope cannot be
/// walked, or a file in scope cannot be read.
pub fn diff_paths<'a>(
    seed: &Path,
    work: &Path,
    paths: impl IntoIterator<Item = &'a Path>,
    trees: impl IntoIterator<Item = &'a Path>,
) -> Result<Vec<CommitOp>, Error> {
    let scope = Scope::new(paths, trees);
    let seed_entries = scope.collect(seed)?;
    let work_entries = scope.collect(work)?;

    let mut ops = Vec::new();
    for (path, seed_entry) in &seed_entries {
        match (seed_entry, work_entries.get(path)) {
            (Entry::Directory(_), None) => ops.push(CommitOp::RemoveDirectory(path.clone())),
            (_, None) => ops.push(CommitOp::Remove(path.clone())),
            (Entry::Directory(before), Some(Entry::Directory(after))) => {
                if before != after {
                    ops.push(CommitOp::SetDirectoryMode {
                        path: path.clone(),
                        mode: *after,
                    });
                }
            }
            (_, Some(Entry::Directory(after))) => ops.extend([
                CommitOp::Remove(path.clone()),
                CommitOp::CreateDirectory {
                    path: path.clone(),
                    mode: *after,
                },
            ]),
            (Entry::Directory(_), Some(_)) => ops.extend([
                CommitOp::RemoveDirectory(path.clone()),
                CommitOp::Write(path.clone()),
            ]),
            (_, Some(work_entry)) => {
                if changed(seed_entry, work_entry, &seed.join(path), &work.join(path))? {
                    ops.push(CommitOp::Write(path.clone()));
                }
            }
        }
    }
    for (path, work_entry) in &work_entries {
        if !seed_entries.contains_key(path) {
            ops.push(match work_entry {
                Entry::Directory(mode) => CommitOp::CreateDirectory {
                    path: path.clone(),
                    mode: *mode,
                },
                _ => CommitOp::Write(path.clone()),
            });
        }
    }
    ops.sort_by(apply_order);
    Ok(ops)
}

/// Orders two operations the way a transaction applies them: removals deepest first, so a
/// directory is emptied before it is removed; then directory creations shallowest first, so
/// parents exist before children; then writes shallowest first; then directory modes deepest
/// first, so a directory that ends unwritable is closed only after everything beneath it is in
/// place.
fn apply_order(left: &CommitOp, right: &CommitOp) -> Ordering {
    /// An operation's group, by the position the group applies in, and whether the group goes
    /// deepest first.
    const fn group(op: &CommitOp) -> (u8, bool) {
        match op {
            CommitOp::Remove(_) | CommitOp::RemoveDirectory(_) => (0, true),
            CommitOp::CreateDirectory { .. } => (1, false),
            CommitOp::Write(_) => (2, false),
            CommitOp::SetDirectoryMode { .. } => (3, true),
        }
    }
    let (rank, deepest) = group(left);
    rank.cmp(&group(right).0)
        .then_with(|| by_depth(left.path(), right.path(), deepest))
}

/// What a scoped diff reads: single entries, and whole subtrees.
struct Scope {
    /// Entries read by themselves, none of them inside one of [`Self::trees`].
    paths: Vec<PathBuf>,
    /// Subtrees read whole, none of them inside another.
    trees: Vec<PathBuf>,
}

impl Scope {
    /// Keeps the relative, normal names, and drops everything another tree already covers.
    fn new<'a>(
        paths: impl IntoIterator<Item = &'a Path>,
        trees: impl IntoIterator<Item = &'a Path>,
    ) -> Self {
        let normal = |path: &Path| {
            path.components()
                .all(|component| matches!(component, Component::Normal(_)))
        };
        let mut trees: Vec<PathBuf> = trees
            .into_iter()
            .filter(|tree| normal(tree))
            .map(Path::to_path_buf)
            .collect();
        // Component-wise order puts every tree's descendants right after it.
        trees.sort();
        trees.dedup_by(|later, earlier| later.starts_with(earlier));
        let covered = |path: &Path| trees.iter().any(|tree| path.starts_with(tree));
        let paths: BTreeSet<PathBuf> = paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty() && normal(path) && !covered(path))
            .map(Path::to_path_buf)
            .collect();
        Self {
            paths: paths.into_iter().collect(),
            trees,
        }
    }

    /// Indexes what this scope covers of `root`, by `root`-relative path.
    ///
    /// A `root` that is not a directory cannot be diffed however little is in scope: that is an
    /// I/O failure, never an empty tree.
    fn collect(&self, root: &Path) -> Result<BTreeMap<PathBuf, Entry>, Error> {
        if !std::fs::metadata(root)?.is_dir() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory).into());
        }
        let mut entries = BTreeMap::new();
        let mut directories = BTreeSet::new();
        for tree in &self.trees {
            if !tree.as_os_str().is_empty() {
                let Some(entry) = reachable(root, tree, &mut directories)? else {
                    continue;
                };
                let directory = matches!(entry, Entry::Directory(_));
                entries.insert(tree.clone(), entry);
                if !directory {
                    continue;
                }
            }
            index_beneath(root, tree, &mut entries)?;
        }
        for path in &self.paths {
            if let Some(entry) = reachable(root, path, &mut directories)? {
                entries.insert(path.clone(), entry);
            }
        }
        Ok(entries)
    }
}

/// The entry at `relative` under `root`, when a walk of `root` would reach it.
///
/// Every ancestor has to be a real directory: the walk follows no symbolic link, so a path the
/// kernel would resolve through one is not in the tree. `directories` remembers the ancestors
/// already proved, so a footprint of many paths in one directory proves that directory once. A
/// missing entry, or an ancestor that is not a directory, is absent rather than a failure.
fn reachable(
    root: &Path,
    relative: &Path,
    directories: &mut BTreeSet<PathBuf>,
) -> Result<Option<Entry>, Error> {
    let mut proved = Vec::new();
    for ancestor in relative
        .ancestors()
        .skip(1)
        .take_while(|ancestor| !ancestor.as_os_str().is_empty())
    {
        if directories.contains(ancestor) {
            break;
        }
        match std::fs::symlink_metadata(root.join(ancestor)).tolerate(ABSENT)? {
            Some(metadata) if metadata.is_dir() => proved.push(ancestor.to_path_buf()),
            _ => return Ok(None),
        }
    }
    directories.extend(proved);
    let path = root.join(relative);
    std::fs::symlink_metadata(&path)
        .tolerate(ABSENT)?
        .map(|metadata| classify(&path, &metadata))
        .transpose()
}

/// Indexes every entry beneath `root/relative`, by its `root`-relative path.
///
/// The relative path is accumulated as the walk descends rather than recovered by stripping `root`
/// off an absolute path: a relative [`PathBuf`] is what the key is, and building it directly keeps
/// every byte of every component, which a string round-trip would not. That accumulated prefix is
/// exactly the per-directory state [`marsh_lib::walk_directory`] carries, so the traversal itself
/// is the shared walker and only the classification — one `metadata` call per entry, and what it
/// decides — is this crate's. `relative` itself, when it is not the root, is the caller's to have
/// indexed first.
fn index_beneath(
    root: &Path,
    relative: &Path,
    entries: &mut BTreeMap<PathBuf, Entry>,
) -> Result<(), Error> {
    let start = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    marsh_lib::walk_directory::<_, Error>(&start, relative.to_path_buf(), |entry, prefix| {
        let relative = prefix.join(entry.file_name());
        let classified = classify(&entry.path(), &entry.metadata()?)?;
        let descend = matches!(classified, Entry::Directory(_)).then(|| relative.clone());
        entries.insert(relative, classified);
        Ok(descend)
    })
}

/// What the entry at `path`, whose unfollowed metadata is `metadata`, is to a diff.
fn classify(path: &Path, metadata: &std::fs::Metadata) -> Result<Entry, Error> {
    use std::os::unix::fs::MetadataExt;

    let file_type = metadata.file_type();
    Ok(if file_type.is_dir() {
        Entry::Directory(Mode::of(metadata))
    } else if file_type.is_symlink() {
        Entry::Symlink(std::fs::read_link(path)?)
    } else if file_type.is_file() {
        Entry::File(FileMeta {
            len: metadata.len(),
            mode: Mode::of(metadata),
            id: (
                metadata.ino(),
                (metadata.mtime(), metadata.mtime_nsec()),
                (metadata.ctime(), metadata.ctime_nsec()),
            ),
        })
    } else {
        Entry::Other(Mode::of(metadata))
    })
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
    use crate::log::CHUNK;
    use std::io::{BufRead, BufReader};

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
    use crate::testing::{chmod, mode_of};

    /// A scratch directory holding a seed and a work tree, both created empty.
    fn trees() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let (seed, work) = (scratch.path().join("seed"), scratch.path().join("work"));
        for tree in [&seed, &work] {
            std::fs::create_dir(tree).expect("tree");
        }
        (scratch, seed, work)
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().expect("has a parent")).expect("create parents");
        std::fs::write(target, contents).expect("write file");
    }

    #[test]
    fn detects_creations_modifications_and_deletions() {
        let (_scratch, seed, work) = trees();
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
                CommitOp::CreateDirectory {
                    path: PathBuf::from("src/new"),
                    mode: Mode::new(mode_of(&work.join("src/new"))),
                },
                CommitOp::Write(PathBuf::from(".git/index")),
                CommitOp::Write(PathBuf::from("src/change.txt")),
                CommitOp::Write(PathBuf::from("src/new/deep.txt")),
            ],
            "git metadata merges like any other path; unchanged paths are absent"
        );
    }

    #[test]
    fn detects_same_length_content_changes_and_mode_changes() {
        let (_scratch, seed, work) = trees();
        write(&seed, "a.txt", "aaa");
        write(&work, "a.txt", "bbb");
        write(&seed, "b.sh", "x");
        write(&work, "b.sh", "x");
        chmod(&work.join("b.sh"), 0o755);

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

    /// Removals go deepest first, so a directory is empty by the time it is removed; creations
    /// and writes go shallowest first, so every parent exists before its children.
    #[test]
    fn orders_deep_removals_before_shallow_ones_and_parents_before_children() {
        let (_scratch, seed, work) = trees();
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
                CommitOp::RemoveDirectory(PathBuf::from("d/x")),
                CommitOp::Remove(PathBuf::from("d/mid.txt")),
                CommitOp::Remove(PathBuf::from("top.txt")),
                CommitOp::RemoveDirectory(PathBuf::from("d")),
                CommitOp::CreateDirectory {
                    path: PathBuf::from("n"),
                    mode: Mode::new(mode_of(&work.join("n"))),
                },
                CommitOp::CreateDirectory {
                    path: PathBuf::from("n/n2"),
                    mode: Mode::new(mode_of(&work.join("n/n2"))),
                },
                CommitOp::Write(PathBuf::from("root.txt")),
                CommitOp::Write(PathBuf::from("n/one.txt")),
                CommitOp::Write(PathBuf::from("n/n2/leaf.txt")),
            ]
        );
    }

    #[test]
    fn detects_symlink_target_changes_and_type_changes() {
        let (_scratch, seed, work) = trees();
        write(&seed, "target1", "one");
        write(&work, "target1", "one");
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
                CommitOp::RemoveDirectory(PathBuf::from("swap")),
                CommitOp::Write(PathBuf::from("link")),
                CommitOp::Write(PathBuf::from("swap")),
            ],
            "a retargeted symlink is a write; a directory replaced by a file is emptied, removed \
             and written"
        );
    }

    /// A name that is not UTF-8 is a perfectly legal filename, and the seed-relative key has to
    /// carry it verbatim — a lossy string round-trip would name a path that does not exist.
    #[test]
    fn a_non_utf8_name_survives_the_diff() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_scratch, seed, work) = trees();
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
        let (_scratch, seed, work) = trees();
        write(&seed, "src/a.txt", "same");
        write(&work, "src/a.txt", "same");
        assert!(diff_trees(&seed, &work).expect("diff").is_empty());
    }

    /// A tree that is not there cannot be diffed, and that is an I/O failure — never an empty
    /// change set, which a caller would publish as "the command deleted everything".
    #[test]
    fn a_missing_tree_is_an_error_not_an_empty_diff() {
        let (scratch, seed, work) = trees();
        write(&seed, "a.txt", "same");
        std::fs::remove_dir(&work).expect("no work tree");

        assert!(matches!(diff_trees(&seed, &work), Err(Error::Io(_))));
        assert!(matches!(
            diff_trees(&scratch.path().join("absent"), &seed),
            Err(Error::Io(_))
        ));
    }

    /// Entries with no content — a socket here — are compared by mode alone, and a mode change is
    /// still a change. Reading them is not an option, so this is all the comparison there is.
    #[test]
    fn entries_without_content_are_compared_by_mode() {
        use std::os::unix::net::UnixListener;

        let (_scratch, seed, work) = trees();
        for tree in [&seed, &work] {
            for name in ["same.sock", "moded.sock"] {
                drop(UnixListener::bind(tree.join(name)).expect("bind socket"));
            }
        }
        chmod(&work.join("moded.sock"), 0o600);
        chmod(&seed.join("moded.sock"), 0o644);

        assert_eq!(
            diff_trees(&seed, &work).expect("diff"),
            vec![CommitOp::Write(PathBuf::from("moded.sock"))],
            "equal modes are unchanged; a differing mode is a write"
        );
    }

    /// A file whose kind changed is a write whatever the two kinds are: publishing the seed's old
    /// regular file under a name that is now a symlink would leave the trees different.
    #[test]
    fn a_change_of_kind_is_a_write() {
        let (_scratch, seed, work) = trees();
        write(&seed, "entry", "plain file");
        std::os::unix::fs::symlink("elsewhere", work.join("entry")).expect("work symlink");

        assert_eq!(
            diff_trees(&seed, &work).expect("diff"),
            vec![CommitOp::Write(PathBuf::from("entry"))]
        );
    }

    /// The identity short-circuit: a path that is literally the same inode in both trees — which
    /// is what a snapshot of an untouched file is — is unchanged without being read.
    #[test]
    fn the_same_inode_in_both_trees_is_unchanged() {
        let (_scratch, seed, work) = trees();
        write(&seed, "shared.txt", "content");
        std::fs::hard_link(seed.join("shared.txt"), work.join("shared.txt"))
            .expect("share the inode");

        assert!(diff_trees(&seed, &work).expect("diff").is_empty());
    }

    /// Equal length is decided by reading, and the read has to run to the end: a difference past
    /// the first buffer is still a difference.
    #[test]
    fn a_difference_past_the_first_chunk_is_found() {
        let (_scratch, seed, work) = trees();
        let mut bytes = vec![b'a'; 200 * 1024];
        std::fs::write(seed.join("big.bin"), &bytes).expect("seed file");
        let last = bytes.len() - 1;
        bytes[last] = b'b';
        std::fs::write(work.join("big.bin"), &bytes).expect("work file");

        assert_eq!(
            diff_trees(&seed, &work).expect("diff"),
            vec![CommitOp::Write(PathBuf::from("big.bin"))]
        );
    }

    /// Every directory is an entry of its own, empty or not: a new one is created with its
    /// mode, a vanished one removed once emptied, a changed mode set, and a change between a
    /// directory and a file is the old entry's removal and the new one's creation. A directory
    /// that merely filled or emptied is unchanged — its contents carry that.
    #[test]
    fn every_directory_is_an_entry_with_a_mode() {
        let (_scratch, seed, work) = trees();
        for tree in [&seed, &work] {
            std::fs::create_dir_all(tree.join("keep")).expect("an unchanged empty directory");
            std::fs::create_dir_all(tree.join("mode")).expect("a directory whose mode changes");
        }
        std::fs::create_dir_all(seed.join("gone")).expect("an empty directory that vanishes");
        write(&seed, "emptied/x.txt", "x");
        std::fs::create_dir_all(work.join("emptied")).expect("a directory left empty");
        std::fs::create_dir_all(seed.join("filled")).expect("an empty directory that fills");
        write(&work, "filled/y.txt", "y");
        write(&seed, "was_file", "file");
        std::fs::create_dir_all(work.join("was_file")).expect("a file replaced by a directory");
        std::fs::create_dir_all(work.join("new")).expect("a new empty directory");
        write(&work, "newparent/child.txt", "child");
        chmod(&work.join("mode"), 0o700);

        assert_eq!(
            diff_trees(&seed, &work).expect("diff"),
            vec![
                CommitOp::Remove(PathBuf::from("emptied/x.txt")),
                CommitOp::Remove(PathBuf::from("was_file")),
                CommitOp::RemoveDirectory(PathBuf::from("gone")),
                CommitOp::CreateDirectory {
                    path: PathBuf::from("new"),
                    mode: Mode::new(mode_of(&work.join("new"))),
                },
                CommitOp::CreateDirectory {
                    path: PathBuf::from("newparent"),
                    mode: Mode::new(mode_of(&work.join("newparent"))),
                },
                CommitOp::CreateDirectory {
                    path: PathBuf::from("was_file"),
                    mode: Mode::new(mode_of(&work.join("was_file"))),
                },
                CommitOp::Write(PathBuf::from("filled/y.txt")),
                CommitOp::Write(PathBuf::from("newparent/child.txt")),
                CommitOp::SetDirectoryMode {
                    path: PathBuf::from("mode"),
                    mode: Mode::new(0o700),
                },
            ]
        );
    }

    /// A scoped diff is the whole-tree diff cut down to its scope — same operations, same order —
    /// including where the kernel would resolve a path through a symbolic link the walk never
    /// follows, and for directories in scope as single paths or inside a tree.
    #[test]
    fn a_scoped_diff_is_the_whole_diff_restricted_to_its_scope() {
        let (scratch, seed, work) = trees();
        for tree in [&seed, &work] {
            write(tree, "real/x", "same");
            write(tree, "outside.txt", "same");
            std::fs::create_dir_all(tree.join("dir/kept")).expect("a kept directory");
        }
        // In scope as single paths.
        write(&work, "a.txt", "new");
        write(&seed, "gone.txt", "old");
        std::fs::create_dir_all(work.join("fresh")).expect("a new empty directory");
        write(&work, "dir/kept/inside", "fills an existing directory");
        // In scope as a tree.
        write(&seed, "tree/deep/old.txt", "old");
        write(&work, "tree/deep/new.txt", "new");
        std::fs::create_dir_all(work.join("tree/empty")).expect("an empty directory in a tree");
        // Resolvable only through a link the walk does not follow.
        std::os::unix::fs::symlink("real", work.join("link")).expect("a directory symlink");
        // Out of scope.
        write(&work, "outside.txt", "changed");

        let paths = [
            "a.txt",
            "gone.txt",
            "fresh",
            "dir/kept",
            "dir/kept/inside",
            "link/x",
            "tree/deep/new.txt",
            "../seed/outside.txt",
            "",
        ]
        .map(Path::new);
        let trees = ["tree", "tree/deep", "absent"].map(Path::new);
        let in_scope =
            |path: &Path| paths.contains(&path) || trees.iter().any(|tree| path.starts_with(tree));
        let whole: Vec<CommitOp> = diff_trees(&seed, &work)
            .expect("whole diff")
            .into_iter()
            .filter(|op| in_scope(op.path()))
            .collect();

        assert_eq!(
            diff_paths(&seed, &work, paths, trees).expect("scoped diff"),
            whole
        );
        assert_eq!(
            whole,
            vec![
                CommitOp::Remove(PathBuf::from("tree/deep/old.txt")),
                CommitOp::Remove(PathBuf::from("gone.txt")),
                CommitOp::CreateDirectory {
                    path: PathBuf::from("fresh"),
                    mode: Mode::new(mode_of(&work.join("fresh"))),
                },
                CommitOp::CreateDirectory {
                    path: PathBuf::from("tree/empty"),
                    mode: Mode::new(mode_of(&work.join("tree/empty"))),
                },
                CommitOp::Write(PathBuf::from("a.txt")),
                CommitOp::Write(PathBuf::from("dir/kept/inside")),
                CommitOp::Write(PathBuf::from("tree/deep/new.txt")),
            ],
            "the scope reaches every kind of change and none outside it"
        );
        assert!(
            diff_paths(&seed, &scratch.path().join("absent"), paths, trees).is_err(),
            "a tree that is not there is a failure however narrow the scope"
        );
    }
}
