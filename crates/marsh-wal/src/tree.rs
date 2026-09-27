//! Descriptor-relative access beneath one directory tree.
//!
//! Every path this crate reads or writes lies beneath a root it opened once: the seed a
//! transaction publishes into, or the frozen tree its content comes from. A relative path is
//! resolved one component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)` from that root, so a
//! symbolic link anywhere above an entry is never followed — out of the tree or anywhere else —
//! and a path is in the tree exactly when a walk of real directories reaches it.
//!
//! Every entry met on the way is also checked to lie in the root's own filesystem, mount and — on
//! btrfs — subvolume. A transaction is one tree: a nested subvolume or a mount inside it is another
//! one, which a rename cannot reach (`EXDEV`) and whose stand-in in a snapshot is an empty
//! placeholder that must never be published over the real thing.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, FsWord, OFlags, Statx, StatxFlags, Uid};
use rustix::io::Errno;

use crate::error::Error;
use crate::types::Mode;

/// `statfs.f_type` for btrfs (`BTRFS_SUPER_MAGIC`).
const BTRFS_SUPER_MAGIC: FsWord = 0x9123_683E;

/// Inode number of every btrfs subvolume's root directory (`BTRFS_FIRST_FREE_OBJECTID`).
const BTRFS_SUBVOLUME_ROOT: u64 = 256;

/// Inode number of the empty placeholder a btrfs snapshot shows where its source had a nested
/// subvolume (`BTRFS_EMPTY_SUBVOL_DIR_OBJECTID`): it shares the snapshot's device, so only its
/// number tells it apart from an ordinary empty directory.
const BTRFS_EMPTY_SUBVOLUME: u64 = 2;

/// What `statx` has to report for an entry to be classified at all.
const IDENTITY: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::UID)
    .union(StatxFlags::INO)
    .union(StatxFlags::MNT_ID);

/// What an entry is, at the granularity publication distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A directory.
    Directory,
    /// A fifo, socket or device node: nothing a transaction can carry.
    Special,
}

/// The filesystem and mount an entry was reached in: what every entry of one tree shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placement {
    /// Device, as `(major, minor)` — one per btrfs subvolume.
    device: (u32, u32),
    /// Mount the entry was reached through — one per bind mount, even of the same device.
    mount: u64,
}

impl Placement {
    /// Where a `statx` answer says its entry is.
    const fn of(stat: &Statx) -> Self {
        Self {
            device: (stat.stx_dev_major, stat.stx_dev_minor),
            mount: stat.stx_mnt_id,
        }
    }
}

/// One entry, as `statx` reports it without following a link.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Node {
    /// Its kind.
    pub(crate) kind: Kind,
    /// Permission bits.
    pub(crate) mode: Mode,
    /// Hard-link count.
    pub(crate) nlink: u32,
    /// Owning user.
    pub(crate) owner: Uid,
    /// Inode number, which on btrfs tells a subvolume root from an ordinary directory.
    ino: u64,
    /// Where it was reached.
    placement: Placement,
}

/// A directory reached by a walk: the root itself, or a descriptor opened beneath it.
pub(crate) enum Dir<'t> {
    /// The tree's root.
    Root(BorrowedFd<'t>),
    /// A directory beneath it.
    Owned(OwnedFd),
}

impl AsFd for Dir<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Root(fd) => *fd,
            Self::Owned(fd) => fd.as_fd(),
        }
    }
}

/// Where a walk to a directory ended.
pub(crate) enum Lookup<'t> {
    /// Every component is a directory of the tree.
    Found(Dir<'t>),
    /// A component is absent, so nothing at or beneath it exists.
    Missing,
    /// This ancestor exists and is not a directory, so nothing beneath it is in the tree.
    Blocked(PathBuf),
}

/// One opened tree: its root descriptor and the identity every entry beneath it must share.
pub(crate) struct Tree<'p> {
    /// The path it was opened by, for messages only — nothing is resolved through it again.
    path: &'p Path,
    /// The root, opened for reading so it can be fsynced and enumerated.
    root: OwnedFd,
    /// Where the root is, and so where every entry of the tree has to be.
    placement: Placement,
    /// Whether the root is on btrfs, where inode numbers identify subvolume roots.
    btrfs: bool,
}

impl<'p> Tree<'p> {
    /// Opens the tree rooted at `path`.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Io`] when `path` is not a directory that can be opened for reading,
    /// and with [`Error::Wal`] when the kernel reports no mount identity for it.
    pub(crate) fn open(path: &'p Path) -> Result<Self, Error> {
        let root = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| io_error(path, error))?;
        let stat = statx(&root, "", AtFlags::EMPTY_PATH, path)?
            .ok_or_else(|| io_error(path, Errno::NOENT))?;
        let btrfs = rustix::fs::fstatfs(&root)
            .map_err(|error| io_error(path, error))?
            .f_type
            == BTRFS_SUPER_MAGIC;
        Ok(Self {
            path,
            placement: Placement::of(&stat),
            root,
            btrfs,
        })
    }

    /// The path this tree was opened by.
    pub(crate) const fn path(&self) -> &'p Path {
        self.path
    }

    /// The root descriptor.
    pub(crate) fn root(&self) -> BorrowedFd<'_> {
        self.root.as_fd()
    }

    /// Walks to the directory at `relative` — the root when it is empty.
    ///
    /// Intermediate directories are opened `O_PATH`, which needs search permission on the parent
    /// and nothing on the directory itself; the last one is opened for reading when `readable`,
    /// for a caller that has to fsync or enumerate it.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Wal`] when a component leaves the tree's filesystem, mount or
    /// subvolume, and with [`Error::Io`] when a component cannot be opened for any reason other
    /// than being absent or not a directory.
    pub(crate) fn directory(&self, relative: &Path, readable: bool) -> Result<Lookup<'_>, Error> {
        let mut current = Dir::Root(self.root.as_fd());
        let mut walked = PathBuf::new();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let name = component.as_os_str();
            walked.push(name);
            let access = if readable && components.peek().is_none() {
                OFlags::RDONLY
            } else {
                OFlags::PATH
            };
            let flags = access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fd =
                match rustix::fs::openat(current.as_fd(), name, flags, rustix::fs::Mode::empty()) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) => return Ok(Lookup::Missing),
                    // `O_NOFOLLOW` reports a symlink as `ELOOP`, `O_DIRECTORY` anything else as
                    // `ENOTDIR`: either way the entry exists and is no directory.
                    Err(Errno::NOTDIR | Errno::LOOP) => return Ok(Lookup::Blocked(walked)),
                    Err(error) => return Err(io_error(&self.path.join(&walked), error)),
                };
            let location = self.path.join(&walked);
            let stat = statx(&fd, "", AtFlags::EMPTY_PATH, &location)?
                .ok_or_else(|| io_error(&location, Errno::NOENT))?;
            self.contain(&walked, &Node::of(&stat))?;
            current = Dir::Owned(fd);
        }
        Ok(Lookup::Found(current))
    }

    /// The entry `name` of `directory`, which is `relative` in this tree; `None` when absent.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Wal`] when the entry leaves the tree's filesystem, mount or subvolume,
    /// and with [`Error::Io`] when it cannot be examined.
    pub(crate) fn entry(
        &self,
        directory: impl AsFd,
        name: &OsStr,
        relative: &Path,
    ) -> Result<Option<Node>, Error> {
        let Some(stat) = statx(
            directory,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
            &self.path.join(relative),
        )?
        else {
            return Ok(None);
        };
        let node = Node::of(&stat);
        self.contain(relative, &node)?;
        Ok(Some(node))
    }

    /// The entry at `relative`, looked up through its parent; `None` when anything on the way is
    /// absent or no directory.
    ///
    /// # Errors
    ///
    /// As [`Self::directory`] and [`Self::entry`].
    pub(crate) fn lookup(&self, relative: &Path) -> Result<Option<(Dir<'_>, Node)>, Error> {
        let (parent, name) = split(relative)?;
        let Lookup::Found(directory) = self.directory(parent, false)? else {
            return Ok(None);
        };
        Ok(self
            .entry(&directory, name, relative)?
            .map(|node| (directory, node)))
    }

    /// Fails unless `node`, reached as `relative`, lies in this tree's own filesystem, mount and
    /// subvolume.
    fn contain(&self, relative: &Path, node: &Node) -> Result<(), Error> {
        let subvolume = self.btrfs
            && node.kind == Kind::Directory
            && matches!(node.ino, BTRFS_SUBVOLUME_ROOT | BTRFS_EMPTY_SUBVOLUME);
        if node.placement != self.placement || subvolume {
            return Err(Error::Wal(format!(
                "{} crosses a subvolume or mount boundary inside {}",
                relative.display(),
                self.path.display()
            )));
        }
        Ok(())
    }
}

impl Node {
    /// Classifies a `statx` answer.
    fn of(stat: &Statx) -> Self {
        let raw = u32::from(stat.stx_mode);
        Self {
            kind: match FileType::from_raw_mode(raw) {
                FileType::RegularFile => Kind::File,
                FileType::Symlink => Kind::Symlink,
                FileType::Directory => Kind::Directory,
                _ => Kind::Special,
            },
            mode: Mode::new(raw),
            nlink: stat.stx_nlink,
            owner: Uid::from_raw_unchecked(stat.stx_uid),
            ino: stat.stx_ino,
            placement: Placement::of(stat),
        }
    }
}

/// `statx` of `name` in `directory` — which is `path`, for messages — for everything
/// classification needs; `None` when absent.
///
/// An answer without all of it, the mount identity especially, is refused rather than guessed
/// around: without it a bind mount is indistinguishable from its surroundings.
fn statx(
    directory: impl AsFd,
    name: impl rustix::path::Arg,
    flags: AtFlags,
    path: &Path,
) -> Result<Option<Statx>, Error> {
    let stat = match rustix::fs::statx(directory, name, flags, IDENTITY) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(io_error(path, error)),
    };
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(IDENTITY) {
        return Err(Error::Wal(format!(
            "the kernel reports no mount identity for {}",
            path.display()
        )));
    }
    Ok(Some(stat))
}

/// A system call's failure about `path`, as the I/O error it is.
pub(crate) fn io_error(path: &Path, error: Errno) -> Error {
    let error = std::io::Error::from(error);
    Error::Io(std::io::Error::new(
        error.kind(),
        format!("{}: {error}", path.display()),
    ))
}

/// Whether `path` is a nonempty relative path of normal components, written without redundant
/// separators or `.`: the only shape a logged path may have, byte for byte.
pub(crate) fn normal(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    !bytes.is_empty()
        && bytes
            .split(|byte| *byte == b'/')
            .all(|component| !matches!(component, b"" | b"." | b".."))
}

/// A normal path's parent (empty for a top-level entry) and final name.
///
/// # Errors
///
/// Fails with [`Error::Wal`] for a path that names no entry.
pub(crate) fn split(path: &Path) -> Result<(&Path, &OsStr), Error> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(Error::Wal(format!("{} names no entry", path.display()))),
    }
}

/// Orders two paths shallowest first, ties broken by the paths' own component-wise order — or,
/// when `deepest`, exactly the reverse.
pub(crate) fn by_depth(left: &Path, right: &Path, deepest: bool) -> Ordering {
    let order = (left.components().count(), left).cmp(&(right.components().count(), right));
    if deepest { order.reverse() } else { order }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A walk refuses to pass through a symlink however it is spelled: the link is an entry
    /// that is not a directory, not a way into the directory it points at.
    #[test]
    fn a_walk_never_follows_a_symlink_ancestor() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().join("tree");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&root).expect("tree");
        std::fs::create_dir_all(outside.join("inner")).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("link out");
        std::fs::write(root.join("file"), b"x").expect("a file");

        let tree = Tree::open(&root).expect("open");
        assert!(matches!(
            tree.directory(Path::new("link/inner"), false).expect("walk"),
            Lookup::Blocked(at) if at == Path::new("link")
        ));
        assert!(matches!(
            tree.directory(Path::new("file"), true).expect("walk"),
            Lookup::Blocked(at) if at == Path::new("file")
        ));
        assert!(matches!(
            tree.directory(Path::new("absent/deeper"), false)
                .expect("walk"),
            Lookup::Missing
        ));
        let (_, node) = tree
            .lookup(Path::new("link"))
            .expect("lookup")
            .expect("present");
        assert_eq!(node.kind, Kind::Symlink, "the link itself, not its target");
    }

    /// A mount inside a tree is another tree: `/proc` is its own mount on every Linux system, and
    /// a walk from `/` neither enters it nor reports it as an entry of its own.
    #[test]
    fn a_mount_inside_the_tree_is_a_boundary() {
        let tree = Tree::open(Path::new("/")).expect("open /");
        let entered = tree.directory(Path::new("proc/self"), false).map(|_| ());
        let reported = tree.lookup(Path::new("proc")).map(|_| ());
        for result in [entered, reported] {
            assert!(
                matches!(&result, Err(Error::Wal(message)) if message.contains("boundary")),
                "got {:?}",
                result.err()
            );
        }
        assert!(
            tree.lookup(Path::new("etc")).expect("lookup").is_some(),
            "an ordinary directory of the same mount is in the tree"
        );
    }

    /// Only one spelling of a path is loggable: anything a lenient parser would normalize is
    /// refused rather than silently meaning something else.
    #[test]
    fn only_plain_relative_paths_are_normal() {
        for good in ["a", "a/b", "a/b.c", ".hidden/x", "a..b"] {
            assert!(normal(Path::new(good)), "{good}");
        }
        for bad in ["", "/a", "a/", "a//b", "./a", "a/./b", "a/..", "..", "."] {
            assert!(!normal(Path::new(bad)), "{bad}");
        }
    }
}
