//! A [`Subvolumes`] over plain directories, for tests on filesystems that have no btrfs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::error::Error;
use crate::snapshot::Subvolumes;

/// A [`Subvolumes`] over plain directories: a snapshot is a recursive copy, a subvolume is a root
/// registered with [`CopyTree::register`] or created here.
///
/// It answers the two assertions affirmatively and reports nothing as a mount root, because the
/// conditions they describe are properties of btrfs that a directory tree cannot have — a caller
/// testing against this one is testing its own logic, not the kernel's.
#[derive(Debug, Default)]
pub struct CopyTree {
    /// Canonicalized paths this instance treats as subvolume roots.
    roots: Mutex<BTreeSet<PathBuf>>,
}

impl CopyTree {
    /// An instance with no registered roots.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Treats `subvolume` as a subvolume root from now on.
    ///
    /// The path is canonicalized, so a caller may register a path spelled through a symlink — a
    /// temporary directory under `/tmp` on macOS, say — and still have the seed walk recognize it.
    pub fn register(&self, subvolume: &Path) {
        let path = subvolume
            .canonicalize()
            .unwrap_or_else(|_| subvolume.to_path_buf());
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path);
    }

    /// Stops treating `subvolume` as a subvolume root.
    fn unregister(&self, subvolume: &Path) {
        let path = subvolume
            .canonicalize()
            .unwrap_or_else(|_| subvolume.to_path_buf());
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&path);
    }
}

impl Subvolumes for CopyTree {
    fn is_subvolume(&self, path: &Path) -> bool {
        let Ok(path) = path.canonicalize() else {
            return false;
        };
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&path)
    }

    fn is_mount_root(&self, _path: &Path) -> Result<bool, Error> {
        Ok(false)
    }

    fn assert_btrfs(&self, _path: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn assert_user_subvol_rm_allowed(&self, _path: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn create_subvolume(&self, path: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(path)?;
        self.register(path);
        Ok(())
    }

    fn snapshot(&self, src: &Path, dest: &Path) -> Result<(), Error> {
        copy_tree(src, dest)?;
        self.register(dest);
        Ok(())
    }

    fn delete_subvolume(&self, path: &Path) {
        self.unregister(path);
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Copies `src` onto `dest` recursively, recreating symlinks rather than following them.
///
/// `std::fs::copy` carries the permission bits over, which is what makes the copy comparable to a
/// btrfs snapshot for [`crate`]'s callers: the diff they run afterwards treats mode as part of an
/// entry.
fn copy_tree(src: &Path, dest: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(dest)?;
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                std::fs::create_dir_all(&target)?;
                stack.push((entry.path(), target));
            } else if file_type.is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Only what a caller declared — or what this implementation created — is a subvolume root, so
    /// a seed walk over a plain tree stops exactly where the test said it should.
    #[test]
    fn only_registered_or_created_roots_are_subvolumes() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let fs = CopyTree::new();
        let seed = scratch.path().join("seed");
        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&seed).expect("seed");
        std::fs::create_dir_all(&plain).expect("plain");

        assert!(!fs.is_subvolume(&seed), "nothing is registered yet");
        fs.register(&seed);
        assert!(fs.is_subvolume(&seed));
        assert!(!fs.is_subvolume(&plain));
        assert!(
            !fs.is_subvolume(&scratch.path().join("absent")),
            "a path that cannot be canonicalized is not a subvolume"
        );

        let created = scratch.path().join("created/nested");
        fs.create_subvolume(&created).expect("create");
        assert!(created.is_dir(), "creating makes the directory");
        assert!(fs.is_subvolume(&created), "and registers it");
    }

    /// A snapshot is a copy that a diff can compare against the source: contents, permission bits
    /// and symlinks-as-symlinks all survive, and the result is itself a subvolume root.
    #[test]
    fn a_snapshot_copies_contents_modes_and_symlinks() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let fs = CopyTree::new();
        let seed = scratch.path().join("seed");
        std::fs::create_dir_all(seed.join("sub")).expect("seed tree");
        std::fs::write(seed.join("sub/a.txt"), b"seed\n").expect("a regular file");
        let script = seed.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").expect("an executable file");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("set mode");
        std::os::unix::fs::symlink("sub/a.txt", seed.join("link")).expect("a symlink");
        fs.register(&seed);

        let snap = scratch.path().join("snap");
        fs.snapshot(&seed, &snap).expect("snapshot");

        assert_eq!(
            std::fs::read(snap.join("sub/a.txt")).expect("copied file"),
            b"seed\n"
        );
        assert_eq!(
            std::fs::metadata(snap.join("run.sh"))
                .expect("copied mode")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read_link(snap.join("link")).expect("the link is still a link"),
            Path::new("sub/a.txt")
        );
        assert!(fs.is_subvolume(&snap), "a snapshot is a subvolume root");

        std::fs::write(snap.join("sub/a.txt"), b"work\n").expect("snapshots are writable");
        assert_eq!(
            std::fs::read(seed.join("sub/a.txt")).expect("read seed"),
            b"seed\n",
            "writing the copy does not touch the source"
        );
    }

    /// Snapshotting a source that is not there fails as I/O rather than producing an empty tree a
    /// caller would mistake for an empty seed.
    #[test]
    fn snapshotting_an_absent_source_fails() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let fs = CopyTree::new();
        let error = fs
            .snapshot(&scratch.path().join("absent"), &scratch.path().join("snap"))
            .expect_err("no source");
        assert!(matches!(&error, Error::Io(_)), "got {error:?}");
    }

    /// Deleting reclaims the tree and retracts the registration, so a later walk over the same
    /// path does not find a subvolume that is gone.
    #[test]
    fn deleting_removes_the_tree_and_the_registration() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let fs = CopyTree::new();
        let snap = scratch.path().join("snap/nested");
        fs.create_subvolume(&snap).expect("create");
        std::fs::write(snap.join("a.txt"), b"x").expect("a file inside");

        fs.delete_subvolume(&snap);
        assert!(!snap.exists());
        assert!(!fs.is_subvolume(&snap));

        fs.delete_subvolume(&snap);
        assert!(!snap.exists(), "deleting an absent path is harmless");
    }

    /// The two btrfs assertions describe properties a directory tree cannot have, so they pass,
    /// and nothing here is a mount root — a caller testing against this is testing its own logic.
    #[test]
    fn the_btrfs_conditions_are_not_modelled() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let fs = CopyTree::new();
        fs.assert_btrfs(scratch.path()).expect("always btrfs");
        fs.assert_user_subvol_rm_allowed(scratch.path())
            .expect("always reclaimable");
        assert!(
            !fs.is_mount_root(scratch.path()).expect("never fails"),
            "no directory is reported as a mount root, so discovery never stops early"
        );
    }
}
