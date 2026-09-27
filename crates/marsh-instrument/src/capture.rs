//! Small filesystem-role overlay on lurk's architecture table, not a second syscall table.

use std::fmt::Display;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use lurk_cli::arch::{SYSCALLS, SyscallArgType, get_arg_value};
use lurk_cli::syscall_info::{RetCode, SyscallArg, SyscallArgs, SyscallInfo};
use nix::sys::ptrace;
use nix::unistd::Pid;
use syscalls::Sysno;

use crate::{FileTarget, Syscall};

pub(crate) const PATH_LIMIT: usize = 4096;
const WORD: u64 = std::mem::size_of::<libc::c_long>() as u64;

/// Only added role information: the native value and scalar/pointer vocabulary stay upstream.
struct Roles {
    paths: u8,
    descriptors: u8,
    dirfds: u8,
}

#[allow(
    clippy::enum_glob_use,
    reason = "the role table names about a hundred syscalls; qualifying each would bury the table"
)]
const fn roles(syscall: Sysno) -> Roles {
    use Sysno::*;
    let (paths, descriptors, dirfds) = match syscall {
        openat | openat2 | mkdirat | mknodat | fchownat | newfstatat | unlinkat | readlinkat
        | fchmodat | fchmodat2 | faccessat | faccessat2 | utimensat | name_to_handle_at
        | execveat | statx | open_tree | fspick | mount_setattr => (2, 1, 1),
        renameat | renameat2 | linkat | move_mount => (10, 5, 5),
        symlinkat => (5, 2, 2),
        fanotify_mark => (16, 9, 8),
        inotify_add_watch => (2, 1, 0),
        chdir | mkdir | mknod | truncate | chroot | acct | umount2 | swapon | swapoff | statfs
        | setxattr | lsetxattr | getxattr | lgetxattr | listxattr | llistxattr | removexattr
        | lremovexattr | execve => (1, 0, 0),
        pivot_root => (3, 0, 0),
        mount => (2, 0, 0),
        read | write | close | lseek | ioctl | pread64 | pwrite64 | readv | writev | dup
        | fcntl | flock | fsync | fdatasync | ftruncate | fchdir | fchmod | fchown | fstat
        | fstatfs | readahead | fsetxattr | fgetxattr | flistxattr | fremovexattr | getdents64
        | fadvise64 | sync_file_range | vmsplice | fallocate | preadv | pwritev | preadv2
        | pwritev2 | open_by_handle_at | syncfs | io_uring_enter | io_uring_register => (0, 1, 0),
        mmap => (0, 16, 0),
        sendfile | tee | dup3 => (0, 3, 0),
        splice | copy_file_range => (0, 5, 0),
        #[cfg(target_arch = "x86_64")]
        open | creat | stat | lstat | access | rmdir | unlink | readlink | chmod | chown
        | lchown | utime | utimes | uselib => (1, 0, 0),
        #[cfg(target_arch = "x86_64")]
        rename | link | symlink => (3, 0, 0),
        #[cfg(target_arch = "x86_64")]
        futimesat => (2, 1, 1),
        #[cfg(target_arch = "x86_64")]
        getdents => (0, 1, 0),
        #[cfg(target_arch = "x86_64")]
        dup2 => (0, 3, 0),
        _ => (0, 0, 0),
    };
    Roles {
        paths,
        descriptors,
        dirfds,
    }
}

/// Entry capture moves directly into the completed wrapper; upstream args are not cloned.
pub(crate) struct Captured {
    args: SyscallArgs,
    paths: Vec<(usize, Vec<u8>)>,
    descriptors: Vec<(usize, Option<FileTarget>)>,
    cwd: Option<FileTarget>,
    flags: Option<u64>,
    failure: Option<io::Error>,
}

impl Captured {
    pub(crate) fn at_entry(
        tid: Pid,
        syscall: Sysno,
        registers: libc::user_regs_struct,
    ) -> io::Result<Self> {
        let types = argument_types(syscall)?;
        let roles = roles(syscall);
        let mut captured = Self {
            args: SyscallArgs(Vec::with_capacity(types.iter().flatten().count())),
            paths: Vec::with_capacity(roles.paths.count_ones() as usize),
            descriptors: Vec::with_capacity(roles.descriptors.count_ones() as usize),
            cwd: None,
            flags: None,
            failure: None,
        };
        for (index, kind) in types.into_iter().enumerate() {
            let Some(kind) = kind else {
                break;
            };
            let value = get_arg_value(registers, index);
            let bit = 1 << index;
            let arg = if roles.paths & bit != 0 {
                let path = if value == 0 && syscall == Sysno::utimensat {
                    Ok(Vec::new())
                } else {
                    read_path(tid, value)
                };
                if let Some(path) = captured.keep(path) {
                    captured.paths.push((index, path));
                }
                SyscallArg::Addr(native_usize(value))
            } else if roles.descriptors & bit != 0 {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the kernel reads an int argument from the low 32 bits of its register"
                )]
                let fd = value as i32;
                let target = if roles.dirfds & bit != 0 && fd == libc::AT_FDCWD {
                    resolve_cwd(tid)
                } else {
                    resolve_fd(&tid, fd, None)
                };
                captured.descriptors.push((index, target));
                SyscallArg::Int(i64::from(fd))
            } else {
                match kind {
                    SyscallArgType::Int => SyscallArg::Int(value.cast_signed()),
                    SyscallArgType::Str | SyscallArgType::StrArray | SyscallArgType::Addr => {
                        SyscallArg::Addr(native_usize(value))
                    }
                }
            };
            captured.args.0.push(arg);
        }
        if captured
            .paths
            .iter()
            .any(|(index, path)| !path.is_empty() && path[0] != b'/' && bare_path(syscall, *index))
        {
            captured.cwd = captured.keep(
                resolve_cwd(tid).ok_or_else(|| io::Error::other("cannot resolve entry-time cwd")),
            );
        }
        if let Some((pointer, length, minimum)) = match syscall {
            Sysno::openat2 => Some((2, 3, 24)),
            Sysno::clone3 => Some((0, 1, 8)),
            _ => None,
        } {
            let result = if get_arg_value(registers, length) < minimum {
                Err(io::Error::other(format!(
                    "{syscall} structure is shorter than {minimum} bytes"
                )))
            } else {
                read_flags(tid, get_arg_value(registers, pointer))
            };
            captured.flags = captured.keep(result);
        }
        Ok(captured)
    }

    /// Retains only the first capture failure; it matters only if the call succeeds.
    fn keep<T>(&mut self, result: io::Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.failure.get_or_insert(error);
                None
            }
        }
    }

    pub(crate) fn complete(
        self,
        tid: Pid,
        syscall: Sysno,
        result: RetCode,
        entry_order: u64,
        duration: Duration,
    ) -> io::Result<Syscall> {
        if !matches!(result, RetCode::Err(_))
            && let Some(error) = self.failure
        {
            return Err(io::Error::other(format!(
                "capturing successful {syscall} of {tid}: {error}"
            )));
        }
        let returned = match result {
            _ if !returns_fd(syscall, &self.args) => None,
            RetCode::Ok(fd) => Some(fd),
            RetCode::Address(value) => Some(i32::try_from(value).map_err(io::Error::other)?),
            RetCode::Err(_) => None,
        };
        let return_fd = returned
            .map(|fd| {
                resolve_fd(&tid, fd, None).ok_or_else(|| {
                    io::Error::other(format!(
                        "cannot resolve descriptor returned by {syscall} of {tid}"
                    ))
                })
            })
            .transpose()?;
        Ok(Syscall {
            info: SyscallInfo {
                typ: "SYSCALL",
                pid: tid,
                syscall,
                args: self.args,
                result,
                duration,
            },
            entry_order,
            cwd: self.cwd,
            return_fd,
            paths: self.paths,
            descriptors: self.descriptors,
            flags: self.flags,
        })
    }
}

/// Missing or incorrect argument slots in 0.3.14, expressed in its existing scalar vocabulary.
fn argument_types(syscall: Sysno) -> io::Result<[Option<SyscallArgType>; 6]> {
    use SyscallArgType::{Addr, Int};
    let mut types = if syscall == Sysno::fchmodat2 {
        [Some(Int), Some(Addr), Some(Int), Some(Int), None, None]
    } else {
        usize::try_from(syscall.id())
            .ok()
            .and_then(|index| SYSCALLS.get(index))
            .and_then(Option::as_ref)
            .filter(|(number, _)| *number == syscall)
            .map(|(_, args)| *args)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "upstream argument table lacks selected syscall {syscall}"
                ))
            })?
    };
    match syscall {
        Sysno::renameat2 => types[4] = Some(Int),
        Sysno::statx => types[4] = Some(Addr),
        Sysno::clone => {
            types = [
                Some(Int),
                Some(Addr),
                Some(Addr),
                Some(Addr),
                Some(Addr),
                None,
            ];
        }
        #[cfg(target_arch = "x86_64")]
        Sysno::fork | Sysno::vfork => types = [None; 6],
        _ => {}
    }
    Ok(types)
}

const fn bare_path(syscall: Sysno, index: usize) -> bool {
    match syscall {
        Sysno::symlinkat => false,
        #[cfg(target_arch = "x86_64")]
        Sysno::symlink => index != 0,
        _ => roles(syscall).dirfds == 0,
    }
}

fn returns_fd(syscall: Sysno, args: &SyscallArgs) -> bool {
    match syscall {
        Sysno::openat | Sysno::openat2 | Sysno::dup | Sysno::dup3 | Sysno::open_by_handle_at => {
            true
        }
        #[cfg(target_arch = "x86_64")]
        Sysno::open | Sysno::creat | Sysno::dup2 => true,
        Sysno::fcntl => {
            matches!(args.0.get(1), Some(SyscallArg::Int(command)) if *command == i64::from(libc::F_DUPFD) || *command == i64::from(libc::F_DUPFD_CLOEXEC))
        }
        _ => false,
    }
}

/// Feeds tracee bytes from `address` to `take` until it declines one more. Aligned peeks do not
/// touch the next page when the final wanted byte lies at the end of this one.
fn read_bytes(
    tid: Pid,
    address: u64,
    wraps: &str,
    mut take: impl FnMut(u8) -> io::Result<bool>,
) -> io::Result<()> {
    let mut aligned = address - address % WORD;
    let mut skip = native_usize(address - aligned);
    loop {
        let word = ptrace::read(tid, aligned as ptrace::AddressType)
            .map(libc::c_long::to_ne_bytes)
            .map_err(|error| {
                io::Error::other(format!(
                    "cannot read tracee {tid} memory at {aligned:#x}: {error}"
                ))
            })?;
        for &byte in &word[skip..] {
            if !take(byte)? {
                return Ok(());
            }
        }
        skip = 0;
        aligned = aligned
            .checked_add(WORD)
            .ok_or_else(|| io::Error::other(wraps))?;
    }
}

/// A register or pointer value as `usize`: lossless, because marsh-trace builds only for the
/// 64-bit ABIs `NATIVE_ARCH` names.
#[expect(
    clippy::cast_possible_truncation,
    reason = "every supported ABI has 64-bit registers and pointers"
)]
pub(crate) const fn native_usize(value: u64) -> usize {
    value as usize
}

pub(crate) fn read_path(tid: Pid, address: u64) -> io::Result<Vec<u8>> {
    if address == 0 {
        return Err(io::Error::other("null pathname pointer"));
    }
    let mut path = Vec::new();
    read_bytes(tid, address, "pathname wraps address space", |byte| {
        if byte == 0 {
            return Ok(false);
        }
        path.push(byte);
        if path.len() >= PATH_LIMIT {
            Err(io::Error::other(
                "pathname is not terminated within PATH_MAX",
            ))
        } else {
            Ok(true)
        }
    })?;
    Ok(path)
}

pub(crate) fn read_flags(tid: Pid, address: u64) -> io::Result<u64> {
    if address == 0 {
        return Err(io::Error::other("null flags pointer"));
    }
    let mut flags = [0_u8; 8];
    let mut copied = 0;
    read_bytes(tid, address, "flags wrap address space", |byte| {
        flags[copied] = byte;
        copied += 1;
        Ok(copied < flags.len())
    })?;
    Ok(u64::from_ne_bytes(flags))
}

/// Identifies `/proc/<process>/fd/<fd>` from its link bytes, metadata and fdinfo mount, in that
/// order. Metadata comes from `pinned` when supplied, otherwise by following the link.
fn resolve_fd(
    process: &dyn Display,
    fd: i32,
    pinned: Option<&std::fs::File>,
) -> Option<FileTarget> {
    let link = format!("/proc/{process}/fd/{fd}");
    let path = std::fs::read_link(&link).ok()?.into_os_string().into_vec();
    // stat follows the proc magic link but never opens a FIFO/device for I/O.
    let metadata = pinned
        .map_or_else(|| std::fs::metadata(&link), std::fs::File::metadata)
        .ok()?;
    let mount_id = proc_field(format!("/proc/{process}/fdinfo/{fd}"), "mnt_id:")
        .ok()
        .flatten()?;
    Some(FileTarget {
        path,
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        mount_id,
    })
}

fn resolve_cwd(tid: Pid) -> Option<FileTarget> {
    let pinned = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(format!("/proc/{tid}/cwd"))
        .ok()?;
    resolve_fd(&"self", pinned.as_raw_fd(), Some(&pinned))
}

/// Parses the first `key` line of a procfs text file such as `status` or `fdinfo`.
pub(crate) fn proc_field<T: FromStr>(path: impl AsRef<Path>, key: &str) -> io::Result<Option<T>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.trim().parse().ok()))
}
