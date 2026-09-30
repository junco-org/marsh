//! Filesystem accesses the host interpreter performs itself, restated in the native vocabulary.
//!
//! A process cannot trace its own threads, so the interpreter reports these accesses right after
//! making them. Each report becomes the records a tracer would have captured for the same call,
//! with identities resolved through this process's own procfs entries, so one classifier serves
//! both sources.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use lurk_cli::syscall_info::{RetCode, SyscallArg, SyscallArgs, SyscallInfo};
use nix::errno::Errno;
use nix::unistd::Pid;
use syscalls::Sysno;

use crate::capture::resolve_fd;
use crate::observation::Sequence;
use crate::{FileTarget, Syscall};

/// One filesystem access the host interpreter performed itself, reported after it happened.
/// Paths are absolute.
#[derive(Clone, Copy, Debug)]
pub enum HostCall<'a> {
    /// An open: `Ok` carries the new descriptor, still open during the report; `Err` an errno.
    Open {
        /// The opened path.
        path: &'a Path,
        /// The descriptor, or the errno of a failed open.
        result: Result<RawFd, i32>,
    },
    /// A metadata or existence probe: stat when `follow`, lstat otherwise.
    Metadata {
        /// The probed path.
        path: &'a Path,
        /// Whether a final symbolic link was followed.
        follow: bool,
        /// The errno of a failed probe.
        errno: Option<i32>,
    },
    /// A directory enumeration.
    ReadDir {
        /// The enumerated directory.
        path: &'a Path,
        /// The errno of a failed open of the directory.
        errno: Option<i32>,
    },
    /// A directory creation (`mkdir`).
    CreateDir {
        /// The created directory.
        path: &'a Path,
        /// The errno of a failed creation.
        errno: Option<i32>,
    },
    /// A file removal (`unlink`).
    Unlink {
        /// The removed path.
        path: &'a Path,
        /// The errno of a failed removal.
        errno: Option<i32>,
    },
    /// A descriptor opened by an earlier command is used again; the reads and writes made
    /// through it are not reported one by one.
    Descriptor {
        /// The descriptor, open during the report.
        fd: RawFd,
    },
}

/// Builds the records of one host access for thread `tid`.
pub(crate) fn records(call: HostCall<'_>, tid: Pid, sequence: &Sequence) -> io::Result<Vec<Syscall>> {
    match call {
        HostCall::Descriptor { fd } => reused(fd, tid, sequence),
        HostCall::Open { path, result } => opened(absolute(path)?, result, tid, sequence, true),
        HostCall::Metadata {
            path,
            follow,
            errno,
        } => {
            let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            Ok(vec![record(
                tid,
                sequence,
                Sysno::newfstatat,
                vec![at_cwd(), SyscallArg::Addr(0), SyscallArg::Addr(0), int(flags)],
                errno.map_or(RetCode::Ok(0), RetCode::Err),
                vec![(1, absolute(path)?)],
                vec![(0, None)],
                None,
            )?])
        }
        HostCall::CreateDir { path, errno } => Ok(vec![record(
            tid,
            sequence,
            Sysno::mkdirat,
            vec![at_cwd(), SyscallArg::Addr(0), int(0o777)],
            errno.map_or(RetCode::Ok(0), RetCode::Err),
            vec![(1, absolute(path)?)],
            vec![(0, None)],
            None,
        )?]),
        HostCall::Unlink { path, errno } => Ok(vec![record(
            tid,
            sequence,
            Sysno::unlinkat,
            vec![at_cwd(), SyscallArg::Addr(0), int(0)],
            errno.map_or(RetCode::Ok(0), RetCode::Err),
            vec![(1, absolute(path)?)],
            vec![(0, None)],
            None,
        )?]),
        HostCall::ReadDir {
            path,
            errno: Some(errno),
        } => opened(absolute(path)?, Err(errno), tid, sequence, false),
        HostCall::ReadDir { path, errno: None } => {
            let bytes = absolute(path)?;
            // The interpreter's own handle is already gone; pin the directory it listed.
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(path)?;
            let fd = directory.as_raw_fd();
            let mut records = opened(bytes, Ok(fd), tid, sequence, false)?;
            let target = identity(fd)?;
            records.push(record(
                tid,
                sequence,
                Sysno::getdents64,
                vec![int(fd), SyscallArg::Addr(0), int(0)],
                RetCode::Ok(0),
                Vec::new(),
                vec![(0, Some(target))],
                None,
            )?);
            Ok(records)
        }
    }
}

/// A reported path's bytes; only absolute paths are meaningful without a working directory.
fn absolute(path: &Path) -> io::Result<Vec<u8>> {
    if path.is_absolute() {
        Ok(path.as_os_str().as_bytes().to_vec())
    } else {
        Err(io::Error::other(format!(
            "host access reported a relative path: {}",
            path.display()
        )))
    }
}

/// A reused descriptor, as the reads and writes its access mode allows. The descriptor keeps the
/// identity and work generation of the open that created it, so a stale one is still refused.
fn reused(fd: RawFd, tid: Pid, sequence: &Sequence) -> io::Result<Vec<Syscall>> {
    // SAFETY: F_GETFL takes no argument and accesses no memory.
    let flags = Errno::result(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let target = identity(fd)?;
    let mode = flags & libc::O_ACCMODE;
    let mut records = Vec::new();
    for (syscall, used) in [
        (Sysno::read, mode != libc::O_WRONLY),
        (Sysno::write, mode != libc::O_RDONLY),
    ] {
        if used {
            records.push(record(
                tid,
                sequence,
                syscall,
                vec![int(fd), SyscallArg::Addr(0), int(0)],
                RetCode::Ok(0),
                Vec::new(),
                vec![(0, Some(target.clone()))],
                None,
            )?);
        }
    }
    Ok(records)
}

/// An `openat` from the working directory, plus a read of a readable descriptor when `reads`:
/// the interpreter's later reads through it are not reported one by one.
fn opened(
    path: Vec<u8>,
    result: Result<RawFd, i32>,
    tid: Pid,
    sequence: &Sequence,
    reads: bool,
) -> io::Result<Vec<Syscall>> {
    let fd = match result {
        Ok(fd) => fd,
        Err(errno) => {
            return Ok(vec![record(
                tid,
                sequence,
                Sysno::openat,
                vec![at_cwd(), SyscallArg::Addr(0), int(libc::O_RDONLY), int(0)],
                RetCode::Err(errno),
                vec![(1, path)],
                vec![(0, None)],
                None,
            )?]);
        }
    };
    // SAFETY: F_GETFL takes no argument and accesses no memory.
    let flags = Errno::result(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let target = identity(fd)?;
    let mut records = vec![record(
        tid,
        sequence,
        Sysno::openat,
        vec![at_cwd(), SyscallArg::Addr(0), int(flags), int(0)],
        RetCode::Ok(fd),
        vec![(1, path)],
        vec![(0, None)],
        Some(target.clone()),
    )?];
    if reads && flags & libc::O_ACCMODE != libc::O_WRONLY {
        records.push(record(
            tid,
            sequence,
            Sysno::read,
            vec![int(fd), SyscallArg::Addr(0), int(0)],
            RetCode::Ok(0),
            Vec::new(),
            vec![(0, Some(target))],
            None,
        )?);
    }
    Ok(records)
}

fn identity(fd: RawFd) -> io::Result<FileTarget> {
    resolve_fd(&"self", fd, None)
        .ok_or_else(|| io::Error::other(format!("cannot resolve host descriptor {fd}")))
}

#[expect(
    clippy::too_many_arguments,
    reason = "one flat constructor over every field of a native record"
)]
fn record(
    tid: Pid,
    sequence: &Sequence,
    syscall: Sysno,
    args: Vec<SyscallArg>,
    result: RetCode,
    paths: Vec<(usize, Vec<u8>)>,
    descriptors: Vec<(usize, Option<FileTarget>)>,
    return_fd: Option<FileTarget>,
) -> io::Result<Syscall> {
    Ok(Syscall {
        info: SyscallInfo {
            typ: "SYSCALL",
            pid: tid,
            syscall,
            args: SyscallArgs(args),
            result,
            duration: Duration::ZERO,
        },
        entry_order: sequence.next()?,
        cwd: None,
        return_fd,
        paths,
        descriptors,
        flags: None,
        submissions: None,
    })
}

const fn at_cwd() -> SyscallArg {
    SyscallArg::Int(libc::AT_FDCWD as i64)
}

const fn int(value: i32) -> SyscallArg {
    SyscallArg::Int(value as i64)
}
