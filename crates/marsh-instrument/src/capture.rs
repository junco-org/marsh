//! Small filesystem-role overlay on lurk's architecture table, not a second syscall table.

use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use lurk_cli::arch::{SYSCALLS, SyscallArgType, get_arg_value};
use lurk_cli::syscall_info::{RetCode, SyscallArg, SyscallArgs, SyscallInfo};
use nix::sys::ptrace;
use nix::unistd::Pid;
use syscalls::Sysno;

use crate::{FileTarget, Syscall};

pub(crate) const PATH_LIMIT: usize = 4096;
/// The most a selected exec's argument and environment arrays may occupy, pointer slots and
/// terminating NULs included.
const EXEC_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
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

/// What a selected exec runs, read from the old image while it was stopped at the call's entry.
pub(crate) struct ExecImage {
    /// The executable, resolved against the entry-time cwd or directory descriptor.
    pub program: PathBuf,
    /// The argument vector, byte for byte.
    pub argv: Vec<OsString>,
    /// The environment entries, byte for byte.
    pub environment: Vec<OsString>,
    /// The entry-time working directory.
    pub cwd: PathBuf,
}

/// Entry capture moves directly into the completed wrapper; upstream args are not cloned.
pub(crate) struct Captured {
    args: SyscallArgs,
    paths: Vec<(usize, Vec<u8>)>,
    descriptors: Vec<(usize, Option<FileTarget>)>,
    cwd: Option<FileTarget>,
    flags: Option<u64>,
    submissions: Option<Vec<u8>>,
    failure: Option<io::Error>,
    /// For a selected exec: what it runs, or why that could not be read. Only an exec that
    /// succeeds ever needs it.
    exec: Option<io::Result<ExecImage>>,
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
            submissions: None,
            failure: None,
            exec: None,
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

    /// The entry-time identity of a descriptor argument.
    pub(crate) fn descriptor(&self, index: usize) -> Option<&FileTarget> {
        self.descriptors
            .iter()
            .find(|(position, _)| *position == index)
            .and_then(|(_, target)| target.as_ref())
    }

    /// Records the opcodes an `io_uring_enter` found pending at its entry.
    pub(crate) fn submit(&mut self, submissions: Option<Vec<u8>>) {
        self.submissions = submissions;
    }

    /// The executable an `execve` or `execveat` names, resolved against its entry-time cwd or
    /// directory descriptor; for `AT_EMPTY_PATH`, the descriptor's own file. `None` for any
    /// other call, or a path that was not readable.
    pub(crate) fn exec_path(&self, syscall: Sysno) -> Option<PathBuf> {
        let os = |bytes: &[u8]| PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        let (path, base) = match syscall {
            Sysno::execve => (self.path(0)?, self.cwd.as_ref()),
            Sysno::execveat => {
                let directory = self.descriptor(0);
                let path = self.path(1)?;
                let flags = match self.args.0.get(4) {
                    Some(SyscallArg::Int(flags)) => *flags,
                    _ => 0,
                };
                if path.is_empty() && flags & i64::from(libc::AT_EMPTY_PATH) != 0 {
                    return directory.map(|target| os(&target.path));
                }
                (path, directory)
            }
            _ => return None,
        };
        if path.first() == Some(&b'/') {
            return Some(os(path));
        }
        Some(base.map_or_else(|| os(path), |base| os(&base.path).join(os(path))))
    }

    /// Reads a selected exec's argument and environment arrays and its working directory, while
    /// the old image is still stopped at the call's entry.
    pub(crate) fn capture_exec(
        &mut self,
        tid: Pid,
        syscall: Sysno,
        registers: libc::user_regs_struct,
        program: PathBuf,
    ) {
        let (argv, environment) = if syscall == Sysno::execveat { (2, 3) } else { (1, 2) };
        let mut budget = EXEC_CAPTURE_LIMIT;
        let image = (|| {
            let argv = read_strings(tid, get_arg_value(registers, argv), &mut budget)?;
            let environment =
                read_strings(tid, get_arg_value(registers, environment), &mut budget)?;
            let cwd = resolve_cwd(tid)
                .ok_or_else(|| io::Error::other("cannot resolve a selected exec's cwd"))?;
            Ok(ExecImage {
                program,
                argv,
                environment,
                cwd: PathBuf::from(OsString::from_vec(cwd.path)),
            })
        })();
        self.exec = Some(image);
    }

    /// What a selected exec runs, once the kernel has replaced the image.
    pub(crate) const fn take_exec(&mut self) -> Option<io::Result<ExecImage>> {
        self.exec.take()
    }

    /// The captured path at a native argument position.
    fn path(&self, index: usize) -> Option<&[u8]> {
        self.paths
            .iter()
            .find(|(position, _)| *position == index)
            .map(|(_, path)| path.as_slice())
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
            submissions: self.submissions,
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
        Sysno::openat
        | Sysno::openat2
        | Sysno::dup
        | Sysno::dup3
        | Sysno::open_by_handle_at
        | Sysno::io_uring_setup => true,
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
pub(crate) fn read_bytes(
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

/// A register or pointer value as `usize`: lossless, because observation builds only for the
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

/// A NULL-terminated array of NUL-terminated strings, as `execve` takes argv and envp. A null
/// array is empty, as the kernel reads it. Every pointer slot and byte, terminators included,
/// is charged to `budget`; exceeding it is an error rather than a truncation.
pub(crate) fn read_strings(tid: Pid, array: u64, budget: &mut usize) -> io::Result<Vec<OsString>> {
    fn charge(budget: &mut usize, bytes: usize) -> io::Result<()> {
        *budget = budget
            .checked_sub(bytes)
            .ok_or_else(|| io::Error::other("exec arguments exceed the capture limit"))?;
        Ok(())
    }
    let mut strings = Vec::new();
    if array == 0 {
        return Ok(strings);
    }
    let mut slot = array;
    loop {
        charge(budget, native_usize(WORD))?;
        let pointer = ptrace::read(tid, slot as ptrace::AddressType)
            .map_err(|error| {
                io::Error::other(format!("cannot read tracee {tid} exec array at {slot:#x}: {error}"))
            })?
            .cast_unsigned();
        if pointer == 0 {
            return Ok(strings);
        }
        let mut bytes = Vec::new();
        read_bytes(tid, pointer, "exec string wraps address space", |byte| {
            charge(budget, 1)?;
            if byte == 0 {
                return Ok(false);
            }
            bytes.push(byte);
            Ok(true)
        })?;
        strings.push(OsString::from_vec(bytes));
        slot = slot
            .checked_add(WORD)
            .ok_or_else(|| io::Error::other("exec array wraps address space"))?;
    }
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
pub(crate) fn resolve_fd(
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
