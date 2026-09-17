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
