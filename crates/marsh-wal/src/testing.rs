//! Filesystem fixtures the unit tests of every module share.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Gives the entry at `path` exactly the permission bits `mode`.
pub(crate) fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The permission bits of the entry at `path`, without following a final symlink.
pub(crate) fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Writes `contents` at `path` with exactly `mode`, creating missing parents.
pub(crate) fn put(path: &Path, contents: &[u8], mode: u32) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("parents");
    std::fs::write(path, contents).expect("write");
    chmod(path, mode);
}

/// Makes `path` a directory with exactly `mode`, creating missing parents.
pub(crate) fn directory(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).expect("directory");
    chmod(path, mode);
}
