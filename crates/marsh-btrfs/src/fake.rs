//! A [`Subvolumes`] over plain directories, for tests on filesystems that have no btrfs.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::Error;
use crate::snapshot::{Subvolumes, existing};

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
        let path = canonical(subvolume);
        self.roots().insert(path);
    }

    /// The registered roots, even after a thread panicked holding them: every update is a single
    /// insert or remove, so the set is never left half-changed.
    fn roots(&self) -> MutexGuard<'_, BTreeSet<PathBuf>> {
        self.roots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `path` canonicalized, or as given when it cannot be.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl Subvolumes for CopyTree {
    fn is_subvolume(&self, path: &Path) -> bool {
        path.canonicalize()
            .is_ok_and(|path| self.roots().contains(&path))
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

    /// The same independent copy as [`Self::snapshot`]: a directory tree has no read-only flag,
    /// and making the copy's entries unwritable would change the modes it exists to preserve.
    /// Nothing here writes into it, so it stays what it was when taken.
    fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<(), Error> {
        self.snapshot(src, dest)
    }

    /// Removes the tree — however unwritable its directories were left, as a subvolume deletion
    /// would — and then its registration.
    fn delete_subvolume(&self, path: &Path) -> Result<(), Error> {
        let key = canonical(path);
        match existing(path)? {
            Some(metadata) if metadata.is_dir() => {
                open_up(path)?;
                std::fs::remove_dir_all(path)?;
            }
            Some(_) => std::fs::remove_file(path)?,
            None => {}
        }
        self.roots().remove(&key);
        Ok(())
    }
}

/// Copies `src` onto `dest` recursively, as a btrfs snapshot would present it: contents,
/// permission bits of files and directories alike, symlinks as symlinks, and hard links as links
/// among the copies.
///
/// The walk state carried by [`marsh_lib::walk_directory`] is the destination directory the
/// current source directory copies into, so each entry only has to join its own file name. Every
/// directory is created owner-writable and gets its real mode only once everything beneath it is
/// copied, deepest first, so a directory that ends unwritable can still be filled. Files sharing
/// an inode in `src` share one in `dest`: the first is copied, the rest are linked to it, so a
/// caller that refuses to break hard links sees the same link counts it would on btrfs.
///
/// A fifo, socket or device node is refused rather than copied: `std::fs::copy` would open a
/// fifo and block until a writer came, and no copy of such an entry is the entry anyway.
fn copy_tree(src: &Path, dest: &Path) -> Result<(), Error> {
    let root_mode = std::fs::metadata(src)?.mode();
    std::fs::create_dir_all(dest)?;
    let mut directories: Vec<(PathBuf, u32)> = vec![(dest.to_path_buf(), root_mode)];
    let mut linked: BTreeMap<(u64, u64), PathBuf> = BTreeMap::new();
    marsh_lib::walk_directory::<_, Error>(src, dest.to_path_buf(), |entry, to| {
        let target = to.join(entry.file_name());
        let metadata = entry.metadata()?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            std::fs::create_dir(&target)?;
            chmod(&target, 0o700)?;
            directories.push((target.clone(), metadata.mode()));
            return Ok(Some(target));
        }
        if file_type.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else if file_type.is_file() {
            if metadata.nlink() > 1 {
                match linked.entry((metadata.dev(), metadata.ino())) {
                    Entry::Occupied(first) => {
                        std::fs::hard_link(first.get(), &target)?;
                        return Ok(None);
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(target.clone());
                    }
                }
            }
            // After the bytes: a write by an unprivileged process clears setuid and setgid, so
            // the mode `std::fs::copy` set up front does not survive its own copy.
            std::fs::copy(entry.path(), &target)?;
            chmod(&target, metadata.mode())?;
        } else {
            return Err(Error::Snapshot(format!(
                "{} is a fifo, socket or device node, which a copy cannot carry",
                entry.path().display()
            )));
        }
        Ok(None)
    })?;
    for (directory, mode) in directories.into_iter().rev() {
        chmod(&directory, mode)?;
    }
    Ok(())
}

/// Grants the owner full access to `root` and every directory beneath it, so it can be removed.
fn open_up(root: &Path) -> Result<(), Error> {
    chmod(root, std::fs::symlink_metadata(root)?.mode() | 0o700)?;
    marsh_lib::walk_directory::<_, Error>(root, (), |entry, ()| {
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            chmod(&entry.path(), metadata.mode() | 0o700)?;
        }
        Ok(metadata.is_dir().then_some(()))
    })
}

/// Sets `path`'s permission bits — setuid, setgid and sticky included — to those of `mode`.
fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).expect("stat").mode() & 0o7777
    }

    /// A scratch directory and an instance with nothing registered.
    fn fixture() -> (tempfile::TempDir, CopyTree) {
        (
            tempfile::tempdir().expect("scratch directory"),
            CopyTree::new(),
        )
    }

    /// One of the two ways of taking a snapshot, as [`Subvolumes`] declares them.
    type Snapshot = fn(&CopyTree, &Path, &Path) -> Result<(), Error>;

    /// Only what a caller declared — or what this implementation created — is a subvolume root, so
    /// a seed walk over a plain tree stops exactly where the test said it should.
    #[test]
    fn only_registered_or_created_roots_are_subvolumes() {
        let (scratch, fs) = fixture();
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

    /// A snapshot is a copy that a diff can compare against the source, as a btrfs snapshot
    /// would be: contents, the permission bits of files and directories — one that ends
    /// unwritable included — symlinks as symlinks, and hard links as links among the copies,
    /// independent of the source's; and the result is itself a subvolume root. A read-only
    /// snapshot is the same copy.
    #[test]
    fn a_snapshot_copies_contents_modes_links_and_symlinks() {
        let (scratch, fs) = fixture();
        let seed = scratch.path().join("seed");
        std::fs::create_dir_all(seed.join("sub")).expect("seed tree");
        std::fs::write(seed.join("sub/a.txt"), b"seed\n").expect("a regular file");
        std::fs::hard_link(seed.join("sub/a.txt"), seed.join("alias.txt")).expect("a link");
        let script = seed.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").expect("an executable file");
        chmod(&script, 0o4755).expect("set mode");
        std::os::unix::fs::symlink("sub/a.txt", seed.join("link")).expect("a symlink");
        std::fs::create_dir(seed.join("sealed")).expect("a directory");
        std::fs::write(seed.join("sealed/in.txt"), b"in\n").expect("its file");
        chmod(&seed.join("sealed"), 0o500).expect("seal it");
        chmod(&seed.join("sub"), 0o710).expect("set mode");
        fs.register(&seed);

        let takes: [(&str, Snapshot); 2] = [
            ("snap", CopyTree::snapshot),
            ("frozen", CopyTree::snapshot_readonly),
        ];
        for (name, take) in takes {
            let snap = scratch.path().join(name);
            take(&fs, &seed, &snap).expect("snapshot");
            assert_eq!(
                std::fs::read(snap.join("sub/a.txt")).expect("copied file"),
                b"seed\n"
            );
            assert_eq!(mode_of(&snap.join("run.sh")), 0o4755);
            assert_eq!(mode_of(&snap.join("sub")), 0o710);
            assert_eq!(mode_of(&snap.join("sealed")), 0o500);
            assert_eq!(mode_of(&snap), mode_of(&seed));
            assert_eq!(
                std::fs::read(snap.join("sealed/in.txt")).expect("sealed file"),
                b"in\n"
            );
            assert_eq!(
                std::fs::read_link(snap.join("link")).expect("the link is still a link"),
                Path::new("sub/a.txt")
            );
            let copy = std::fs::metadata(snap.join("sub/a.txt")).expect("stat copy");
            let alias = std::fs::metadata(snap.join("alias.txt")).expect("stat alias");
            assert_eq!(copy.ino(), alias.ino(), "the copies are still one file");
            assert_eq!(copy.nlink(), 2);
            assert_ne!(
                copy.ino(),
                std::fs::metadata(seed.join("sub/a.txt"))
                    .expect("stat")
                    .ino(),
                "and not the source's file"
            );
            assert!(fs.is_subvolume(&snap), "a snapshot is a subvolume root");
        }

        let snap = scratch.path().join("snap");
        std::fs::write(snap.join("sub/a.txt"), b"work\n").expect("snapshots are writable");
        assert_eq!(
            std::fs::read(seed.join("sub/a.txt")).expect("read seed"),
            b"seed\n",
            "writing the copy does not touch the source"
        );
        assert_eq!(
            std::fs::read(snap.join("alias.txt")).expect("read alias"),
            b"work\n",
            "but does reach the copy's own alias"
        );
        for tree in [&seed, &snap, &scratch.path().join("frozen")] {
            fs.delete_subvolume(tree).expect("delete");
        }
    }

    /// A fifo — which `std::fs::copy` would open and block on until a writer came — is refused
    /// rather than copied, and so is a socket.
    #[test]
    fn a_special_entry_is_refused_instead_of_copied() {
        let (scratch, fs) = fixture();
        let seed = scratch.path().join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let fifo = std::ffi::CString::new(seed.join("fifo").into_os_string().into_encoded_bytes())
            .expect("a C path");
        // SAFETY: `fifo` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo");

        let error = fs
            .snapshot(&seed, &scratch.path().join("snap"))
            .expect_err("a fifo cannot be copied");
        assert_error!(error, Error::Snapshot(message) if message.contains("fifo"));

        std::fs::remove_file(seed.join("fifo")).expect("remove the fifo");
        drop(std::os::unix::net::UnixListener::bind(seed.join("sock")).expect("a socket"));
        assert!(matches!(
            fs.snapshot_readonly(&seed, &scratch.path().join("frozen")),
            Err(Error::Snapshot(_))
        ));
    }

    /// Snapshotting a source that is not there fails as I/O rather than producing an empty tree a
    /// caller would mistake for an empty seed.
    #[test]
    fn snapshotting_an_absent_source_fails() {
        let (scratch, fs) = fixture();
        let error = fs
            .snapshot(&scratch.path().join("absent"), &scratch.path().join("snap"))
            .expect_err("no source");
        assert_error!(error, Error::Io(_));
    }

    /// Deleting reclaims the tree — even one with directories left unwritable, which a subvolume
    /// deletion would not mind — and retracts the registration, so a later walk over the same path
    /// does not find a subvolume that is gone.
    #[test]
    fn deleting_removes_the_tree_and_the_registration() {
        let (scratch, fs) = fixture();
        let snap = scratch.path().join("snap/nested");
        fs.create_subvolume(&snap).expect("create");
        std::fs::create_dir(snap.join("sealed")).expect("a directory");
        std::fs::write(snap.join("sealed/a.txt"), b"x").expect("a file inside");
        chmod(&snap.join("sealed"), 0o500).expect("seal it");
        chmod(&snap, 0o555).expect("seal the root");

        fs.delete_subvolume(&snap).expect("delete");
        assert!(!snap.exists());
        assert!(!fs.is_subvolume(&snap));

        fs.delete_subvolume(&snap)
            .expect("deleting an absent path is harmless");
        assert!(!snap.exists());
    }

    /// The two btrfs assertions describe properties a directory tree cannot have, so they pass,
    /// and nothing here is a mount root — a caller testing against this is testing its own logic.
    #[test]
    fn the_btrfs_conditions_are_not_modelled() {
        let (scratch, fs) = fixture();
        fs.assert_btrfs(scratch.path()).expect("always btrfs");
        fs.assert_user_subvol_rm_allowed(scratch.path())
            .expect("always reclaimable");
        assert!(
            !fs.is_mount_root(scratch.path()).expect("never fails"),
            "no directory is reported as a mount root, so discovery never stops early"
        );
    }
}
