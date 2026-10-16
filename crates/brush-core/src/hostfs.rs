//! Filesystem accesses the interpreter performs itself on a host thread.
//!
//! Every helper here performs exactly one access on the path [`ExecutionObserver::host_path`]
//! chooses, through [`ExecutionObserver::host_fs`], which may run it with the authority of the
//! shell's commands, and then reports it with that path through
//! [`ExecutionObserver::host_access`], so that an embedder sees the interpreter's own accesses
//! (which no tracer of spawned commands can observe). Callers pass absolute paths; an empty path
//! (which fails with `ENOENT` without any lookup) is not reported.

use std::path::Path;

use crate::extensions::{ExecutionObserver, HostAccess};

/// The errno carried by an I/O error; `EIO` when it carries none.
pub(crate) fn errno(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn report(observer: &impl ExecutionObserver, access: HostAccess<'_>) {
    let reported = match &access {
        HostAccess::Open { path, .. }
        | HostAccess::Metadata { path, .. }
        | HostAccess::ReadDir { path, .. } => !path.as_os_str().is_empty(),
        HostAccess::Descriptor { .. } => true,
    };
    if reported {
        observer.host_access(access);
    }
}

/// Retrieves `path`'s metadata (`stat` when `follow`, `lstat` otherwise).
pub(crate) fn metadata(
    observer: &impl ExecutionObserver,
    path: &Path,
    follow: bool,
) -> std::io::Result<std::fs::Metadata> {
    let used = observer.host_path(path);
    let result = observer
        .host_fs(|| {
            if follow {
                std::fs::metadata(&used)
            } else {
                std::fs::symlink_metadata(&used)
            }
        })
        .and_then(|result| result);
    report(
        observer,
        HostAccess::Metadata {
            path: &used,
            follow,
            errno: result.as_ref().err().map(errno),
        },
    );
    result
}

/// Whether `path` exists, following symbolic links; same as [`Path::exists`].
pub(crate) fn exists(observer: &impl ExecutionObserver, path: &Path) -> bool {
    metadata(observer, path, true).is_ok()
}

/// Whether `path` is a directory, following symbolic links; same as [`Path::is_dir`].
pub(crate) fn is_dir(observer: &impl ExecutionObserver, path: &Path) -> bool {
    metadata(observer, path, true).is_ok_and(|m| m.is_dir())
}

/// Whether `path` is a regular file, following symbolic links; same as [`Path::is_file`].
pub(crate) fn is_file(observer: &impl ExecutionObserver, path: &Path) -> bool {
    metadata(observer, path, true).is_ok_and(|m| m.is_file())
}

/// Checks `path` with `access(2)`, which follows symbolic links; reported as a
/// [`HostAccess::Metadata`] probe with `follow: true`.
pub(crate) fn access(
    observer: &impl ExecutionObserver,
    path: &Path,
    mode: nix::unistd::AccessFlags,
) -> bool {
    let used = observer.host_path(path);
    let result = observer
        .host_fs(|| nix::unistd::access(used.as_ref(), mode).map_err(std::io::Error::from))
        .and_then(|result| result);
    report(
        observer,
        HostAccess::Metadata {
            path: &used,
            follow: true,
            errno: result.as_ref().err().map(errno),
        },
    );
    result.is_ok()
}

/// Whether `path` is executable by the current user; same as `PathExt::executable`.
pub(crate) fn executable(observer: &impl ExecutionObserver, path: &Path) -> bool {
    access(observer, path, nix::unistd::AccessFlags::X_OK)
}

/// Opens `path` for enumeration.
pub(crate) fn read_dir(
    observer: &impl ExecutionObserver,
    path: &Path,
) -> std::io::Result<std::fs::ReadDir> {
    let used = observer.host_path(path);
    let result = observer
        .host_fs(|| std::fs::read_dir(&used))
        .and_then(|result| result);
    report(
        observer,
        HostAccess::ReadDir {
            path: &used,
            errno: result.as_ref().err().map(errno),
        },
    );
    result
}

/// Opens `path` with `options`.
pub(crate) fn open(
    observer: &impl ExecutionObserver,
    options: &std::fs::OpenOptions,
    path: &Path,
) -> std::io::Result<std::fs::File> {
    let used = observer.host_path(path);
    let result = observer
        .host_fs(|| options.open(&used))
        .and_then(|result| result);
    report(
        observer,
        HostAccess::Open {
            result: result
                .as_ref()
                .map(std::os::fd::AsRawFd::as_raw_fd)
                .map_err(errno),
            path: &used,
        },
    );
    result
}
