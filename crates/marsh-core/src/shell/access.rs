//! Filesystem capabilities and freshness dependencies from native, stopped-task evidence.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use lurk_cli::syscall_info::{RetCode, SyscallArg};
use marsh_instrument::{FileTarget, Syscall};
use marsh_lib::{CheckedAdvance, RecoverPoison as _};
use nix_observer::unistd::Pid;
use syscalls::Sysno;

/// `IORING_SETUP_SQPOLL`: a kernel thread consumes submissions without any syscall to observe.
const IORING_SETUP_SQPOLL: u64 = 1 << 1;
/// `io_uring` operations with no filesystem effect: `NOP`, `POLL_ADD`, `POLL_REMOVE`, `TIMEOUT`,
/// `TIMEOUT_REMOVE`, `ASYNC_CANCEL`, `LINK_TIMEOUT` and `EPOLL_CTL` (event-loop batching, as
/// libuv uses).
const EFFECTLESS_IO_URING_OPS: [u8; 8] = [0, 6, 7, 11, 12, 14, 15, 29];

/// A single native call's footprint. Metadata observations never manufacture content claims.
#[derive(Debug, Default)]
pub(super) struct Effects {
    pub reads: Vec<PathBuf>,
    pub writes: Vec<PathBuf>,
    pub dependencies: Vec<PathBuf>,
    pub recursive_reads: Vec<PathBuf>,
    pub recursive_writes: Vec<PathBuf>,
    pub outside_writes: Vec<PathBuf>,
    /// A successful unlink/rmdir: `writes` names the removed entry, whose claims it clears.
    pub removed: bool,
    /// The one entry an explicit `release` relinquishes; nothing is read or written.
    pub release: Option<PathBuf>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct WorkGeneration(u64);
impl CheckedAdvance for WorkGeneration {
    type Output = Self;
    type Error = std::io::Error;
    fn value(&self) -> u64 {
        self.0
    }
    fn advance(self, value: u64) -> Self {
        Self(value)
    }
    fn exhausted() -> std::io::Error {
        std::io::Error::other("work generation exhausted")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Inode(u64, u64);

#[derive(Clone)]
struct Mapping {
    start: u64,
    end: u64,
    target: FileTarget,
    shared: bool,
    generation: WorkGeneration,
    /// The name `target` was captured at has since been removed.
    // ponytail: removing the captured name retires the binding even if a hard link survives;
    // rebind to a verified surviving alias if that ever matters.
    binding_removed: bool,
}

#[derive(Clone)]
struct Descriptor {
    inode: Inode,
    mount: u64,
    generation: WorkGeneration,
}

/// Clone flags independently share descriptor and VM state; one task record keeps both.
#[derive(Default)]
struct TaskState {
    descriptors: Arc<Mutex<HashMap<i32, Descriptor>>>,
    mappings: Arc<Mutex<Vec<Mapping>>>,
}

/// Retained descriptor and address-space origins survive command boundaries, not work retakes.
#[derive(Default)]
pub(super) struct Access {
    generation: WorkGeneration,
    host: TaskState,
    tasks: HashMap<Pid, TaskState>,
    aliases: HashMap<Inode, Vec<PathBuf>>,
    links: HashMap<PathBuf, PathBuf>,
}

impl Access {
    /// Builds immutable lookup evidence before interpretation; never probes a resumed task's fd.
    pub fn prepare(&mut self, root: &Path, retaken: bool) -> std::io::Result<()> {
        if retaken {
            self.generation = self.generation.next()?;
        }
        self.aliases.clear();
        self.links.clear();
        marsh_lib::walk_directory::<(), std::io::Error>(root, (), |entry, ()| {
            let meta = entry.metadata()?;
            if meta.is_dir() {
                return Ok(Some(()));
            }
            if meta.is_symlink() {
                let path = entry.path();
                let target = std::fs::read_link(&path)?;
                self.links.insert(path, target);
            } else if meta.is_file() && meta.nlink() > 1 {
                self.aliases
                    .entry(Inode(meta.dev(), meta.ino()))
                    .or_default()
                    .push(entry.path());
            }
            Ok(None)
        })
    }

    /// Resolves and classifies one completed call. An unrepresentable protected effect is refused.
    #[expect(
        clippy::too_many_lines,
        reason = "one flat dispatch over every syscall the classifier understands"
    )]
    pub fn observe(&mut self, info: &Syscall, root: &Path) -> Result<Effects, String> {
        let mut effects = Effects::default();
        let success = matches!(info.info.result, RetCode::Ok(_) | RetCode::Address(_));
        let tid = info.info.pid;
        if matches!(
            info.info.syscall,
            Sysno::clone | Sysno::clone3 | Sysno::fork | Sysno::vfork
        ) && success
        {
            let flags = if info.info.syscall == Sysno::clone3 {
                captured_flags(info)?
            } else if info.info.syscall == Sysno::clone {
                integer(info, 0)?
            } else {
                0
            };
            if let Some(child) = returned(info)
                .and_then(|value| i32::try_from(value).ok())
                .map(Pid::from_raw)
            {
                let parent = self.task(tid);
                let descriptors = inherit(
                    &parent.descriptors,
                    flags & u64::from(libc::CLONE_FILES.cast_unsigned()) != 0,
                );
                let mappings = inherit(
                    &parent.mappings,
                    flags & u64::from(libc::CLONE_VM.cast_unsigned()) != 0,
                );
                self.tasks.insert(
                    child,
                    TaskState {
                        descriptors,
                        mappings,
                    },
                );
            }
            return Ok(effects);
        }
        match info.info.syscall {
            Sysno::open | Sysno::openat | Sysno::openat2 | Sysno::creat => {
                let (at, index, flags) = match info.info.syscall {
                    Sysno::open => (None, 0, integer(info, 1)?),
                    Sysno::creat => (
                        None,
                        0,
                        (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)
                            .cast_unsigned()
                            .into(),
                    ),
                    Sysno::openat2 => (Some(0), 1, captured_flags(info)?),
                    _ => (Some(0), 1, integer(info, 2)?),
                };
                self.path(info, at, index, root, &mut effects)?;
                if success {
                    let target = info.return_fd.as_ref().ok_or_else(|| {
                        "successful open has no stopped-task identity".to_string()
                    })?;
                    self.opened(info, target, root)?;
                    let writes = flags
                        & u64::from(
                            (libc::O_WRONLY
                                | libc::O_RDWR
                                | libc::O_CREAT
                                | libc::O_TRUNC
                                | libc::O_APPEND)
                                .cast_unsigned(),
                        )
                        != 0;
                    if writes {
                        self.target_effect(&mut effects, target, root, false, true)?;
                    }
                }
            }
            Sysno::read | Sysno::pread64 | Sysno::readv | Sysno::preadv | Sysno::preadv2 => {
                if success {
                    self.fd_effect(&mut effects, info, 0, root, true, false)?;
                }
            }
            Sysno::write
            | Sysno::pwrite64
            | Sysno::writev
            | Sysno::pwritev
            | Sysno::pwritev2
            | Sysno::fallocate => {
                if success {
                    self.fd_effect(&mut effects, info, 0, root, false, true)?;
                }
            }
            Sysno::sendfile => {
                if success {
                    self.fd_effect(&mut effects, info, 1, root, true, false)?;
                    self.fd_effect(&mut effects, info, 0, root, false, true)?;
                }
            }
            Sysno::copy_file_range | Sysno::splice | Sysno::tee => {
                if success {
                    self.fd_effect(&mut effects, info, 0, root, true, false)?;
                    let output = if info.info.syscall == Sysno::tee {
                        1
                    } else {
                        2
                    };
                    self.fd_effect(&mut effects, info, output, root, false, true)?;
                }
            }
            Sysno::dup | Sysno::dup2 | Sysno::dup3 | Sysno::fcntl => {
                if success && let Some(target) = &info.return_fd {
                    self.check_fd(info, 0, root)?;
                    let generation = self
                        .descriptor_generation(info, 0)?
                        .unwrap_or(self.generation);
                    self.opened(info, target, root)?;
                    if let Some(number) = returned(info).and_then(|value| i32::try_from(value).ok())
                    {
                        if let Some(descriptor) =
                            self.task(tid).descriptors.lock().recover().get_mut(&number)
                        {
                            descriptor.generation = generation;
                        }
                    }
                }
            }
            Sysno::close => {
                if success {
                    let number = descriptor_number(info, 0)?;
                    self.task(tid).descriptors.lock().recover().remove(&number);
                }
            }
            Sysno::close_range => {
                if success {
                    let flags = integer(info, 2)?;
                    if flags & u64::from(libc::CLOSE_RANGE_UNSHARE) != 0 {
                        let task = self.task(tid);
                        let descriptors = inherit(&task.descriptors, false);
                        let mappings = Arc::clone(&task.mappings);
                        self.tasks.insert(
                            tid,
                            TaskState {
                                descriptors,
                                mappings,
                            },
                        );
                    }
                    // CLOEXEC marks descriptors; it does not close them. A subsequent dup must
                    // retain the original generation even before an exec actually closes anything.
                    if flags & u64::from(libc::CLOSE_RANGE_CLOEXEC) != 0 {
                        return Ok(effects);
                    }
                    let first = integer(info, 0)?;
                    let last = integer(info, 1)?;
                    self.task(tid)
                        .descriptors
                        .lock()
                        .recover()
                        .retain(|number, _| {
                            let number = u64::from(number.cast_unsigned());
                            number < first || number > last
                        });
                }
            }
            Sysno::ftruncate | Sysno::fchmod => {
                if success {
                    self.fd_effect(&mut effects, info, 0, root, false, true)?;
                }
            }
            Sysno::fstat | Sysno::fstatfs | Sysno::lseek | Sysno::fchdir => {
                if success {
                    self.fd_effect(&mut effects, info, 0, root, false, false)?;
                }
            }
            Sysno::getdents | Sysno::getdents64 => {
                if success {
                    if let Some(target) = info.fd(0)? {
                        self.check_fd(info, 0, root)?;
                        if let Some(path) = protected(root, &target_path(target))? {
                            effects.recursive_reads.push(path);
                        }
                    } else {
                        return Err("directory enumeration has no stopped-task identity".into());
                    }
                }
            }
            Sysno::mmap => {
                if success
                    && integer(info, 3)? & u64::from(libc::MAP_ANONYMOUS.cast_unsigned()) == 0
                {
                    let target = info
                        .fd(4)?
                        .ok_or_else(|| "file mapping has no stopped-task identity".to_string())?;
                    self.check_fd(info, 4, root)?;
                    let protection = integer(info, 2)?;
                    let shared =
                        integer(info, 3)? & u64::from(libc::MAP_SHARED.cast_unsigned()) != 0;
                    self.target_effect(
                        &mut effects,
                        target,
                        root,
                        protection & u64::from((libc::PROT_READ | libc::PROT_EXEC).cast_unsigned())
                            != 0,
                        shared && protection & u64::from(libc::PROT_WRITE.cast_unsigned()) != 0,
                    )?;
                    let start =
                        returned(info).ok_or_else(|| "mapping has no address".to_string())?;
                    let end = start
                        .checked_add(integer(info, 1)?)
                        .ok_or_else(|| "mapping range overflow".to_string())?;
                    self.task(tid).mappings.lock().recover().push(Mapping {
                        start,
                        end,
                        target: target.clone(),
                        shared,
                        generation: self.generation,
                        binding_removed: false,
                    });
                }
            }
            Sysno::mprotect
            | Sysno::pkey_mprotect
            | Sysno::munmap
            | Sysno::mremap
            | Sysno::msync => {
                if success {
                    self.mapping_effects(info, root, &mut effects)?;
                }
            }
            Sysno::stat
            | Sysno::lstat
            | Sysno::access
            | Sysno::statfs
            | Sysno::readlink
            | Sysno::chdir
            | Sysno::truncate
            | Sysno::chmod
            | Sysno::utime
            | Sysno::utimes => {
                let path = self.path(info, None, 0, root, &mut effects)?;
                if success
                    && matches!(
                        info.info.syscall,
                        Sysno::truncate | Sysno::chmod | Sysno::utime | Sysno::utimes
                    )
                {
                    write_path(&mut effects, root, path)?;
                }
            }
            Sysno::newfstatat
            | Sysno::statx
            | Sysno::faccessat
            | Sysno::faccessat2
            | Sysno::readlinkat
            | Sysno::fchmodat
            | Sysno::fchmodat2
            | Sysno::utimensat
            | Sysno::futimesat => {
                let path = self.path(info, Some(0), 1, root, &mut effects)?;
                if success
                    && matches!(
                        info.info.syscall,
                        Sysno::fchmodat | Sysno::fchmodat2 | Sysno::utimensat | Sysno::futimesat
                    )
                {
                    write_path(&mut effects, root, path)?;
                }
            }
            Sysno::mkdir | Sysno::mkdirat | Sysno::rmdir | Sysno::unlink | Sysno::unlinkat => {
                let at = matches!(info.info.syscall, Sysno::mkdirat | Sysno::unlinkat);
                let path = self.path(info, at.then_some(0), usize::from(at), root, &mut effects)?;
                if success {
                    if let Some(relative) = protected(root, &path)? {
                        if info.info.syscall == Sysno::rmdir
                            || (info.info.syscall == Sysno::unlinkat
                                && integer(info, 2)?
                                    & u64::from(libc::AT_REMOVEDIR.cast_unsigned())
                                    != 0)
                        {
                            effects.recursive_reads.push(relative.clone());
                            effects.recursive_writes.push(relative);
                        }
                    }
                    self.links.remove(&path);
                    if !matches!(info.info.syscall, Sysno::mkdir | Sysno::mkdirat) {
                        effects.removed = true;
                        self.forget(&path);
                    }
                    write_path(&mut effects, root, path)?;
                }
            }
            Sysno::rename | Sysno::renameat | Sysno::renameat2 | Sysno::link | Sysno::linkat => {
                let at = matches!(
                    info.info.syscall,
                    Sysno::renameat | Sysno::renameat2 | Sysno::linkat
                );
                let from = self.path(info, at.then_some(0), usize::from(at), root, &mut effects)?;
                let to = self.path(
                    info,
                    at.then_some(2),
                    if at { 3 } else { 1 },
                    root,
                    &mut effects,
                )?;
                if success {
                    // Temporary aliases (including Git's object installation) may disappear in
                    // the same run. Publication checks the final changed payload's link count.
                    for path in [&from, &to] {
                        if let Some(relative) = protected(root, path)? {
                            effects.recursive_reads.push(relative.clone());
                            effects.recursive_writes.push(relative);
                        }
                        write_path(&mut effects, root, path.clone())?;
                    }
                    if matches!(
                        info.info.syscall,
                        Sysno::rename | Sysno::renameat | Sysno::renameat2
                    ) {
                        let exchange =
                            info.info.syscall == Sysno::renameat2 && integer(info, 4)? & 2 != 0;
                        let mut moved = Vec::new();
                        for (path, target) in &self.links {
                            if let Ok(relative) = path.strip_prefix(&from) {
                                moved.push((to.join(relative), target.clone()));
                            } else if exchange && let Ok(relative) = path.strip_prefix(&to) {
                                moved.push((from.join(relative), target.clone()));
                            }
                        }
                        self.links
                            .retain(|path, _| !path.starts_with(&from) && !path.starts_with(&to));
                        self.links.extend(moved);
                    }
                }
            }
            Sysno::symlink | Sysno::symlinkat => {
                let at = info.info.syscall == Sysno::symlinkat;
                let path = self.path(
                    info,
                    at.then_some(1),
                    if at { 2 } else { 1 },
                    root,
                    &mut effects,
                )?;
                if success {
                    let target = info.path(0).ok_or_else(|| {
                        format!("{} lacks captured symlink target", info.info.syscall)
                    })?;
                    self.links.insert(
                        path.clone(),
                        Path::new(std::ffi::OsStr::from_bytes(target)).to_path_buf(),
                    );
                    write_path(&mut effects, root, path)?;
                }
            }
            Sysno::chown
            | Sysno::lchown
            | Sysno::fchown
            | Sysno::fchownat
            | Sysno::setxattr
            | Sysno::lsetxattr
            | Sysno::fsetxattr
            | Sysno::removexattr
            | Sysno::lremovexattr
            | Sysno::fremovexattr
            | Sysno::mknod
            | Sysno::mknodat => {
                if success {
                    let path = if matches!(
                        info.info.syscall,
                        Sysno::fchown | Sysno::fsetxattr | Sysno::fremovexattr
                    ) {
                        info.fd(0)?
                            .map(target_path)
                            .ok_or_else(|| "mutation has no descriptor identity".to_string())?
                    } else {
                        let at = matches!(info.info.syscall, Sysno::fchownat | Sysno::mknodat);
                        self.path(info, at.then_some(0), usize::from(at), root, &mut effects)?
                    };
                    if protected(root, &path)?.is_some() {
                        return Err(format!(
                            "unsupported protected effect: {}",
                            info.info.syscall
                        ));
                    }
                }
            }
            // A ring is observable while every submission is visible at an `io_uring_enter` stop:
            // no kernel submission thread, and only operations with no filesystem effect.
            Sysno::io_uring_setup => {
                if success && captured_flags(info)? & IORING_SETUP_SQPOLL != 0 {
                    return Err("kernel-polled io_uring submissions are unobservable".into());
                }
            }
            Sysno::io_uring_enter => {
                if integer(info, 1)? != 0 {
                    let submissions = info
                        .submissions
                        .as_ref()
                        .ok_or_else(|| "unobserved io_uring submissions".to_string())?;
                    if let Some(opcode) = submissions
                        .iter()
                        .find(|opcode| !EFFECTLESS_IO_URING_OPS.contains(opcode))
                    {
                        return Err(format!("unsupported io_uring operation {opcode}"));
                    }
                }
            }
            Sysno::io_uring_register
            | Sysno::io_submit
            | Sysno::mount
            | Sysno::umount2
            | Sysno::pivot_root
            | Sysno::chroot
            | Sysno::mount_setattr
            | Sysno::move_mount
            | Sysno::setxattrat
            | Sysno::removexattrat
            | Sysno::file_setattr => {
                if success {
                    return Err(format!(
                        "unsupported workload effect: {}",
                        info.info.syscall
                    ));
                }
            }
            Sysno::ioctl => {
                if success && let Some(target) = info.fd(0)? {
                    if protected(root, &target_path(target))?.is_some() {
                        return Err("protected ioctl effect is unsupported".into());
                    }
                }
            }
            Sysno::execve | Sysno::execveat => {
                if success && let Some(task) = self.tasks.get_mut(&tid) {
                    task.mappings = Arc::default();
                }
            }
            _ => {}
        }
        Ok(effects)
    }

    /// Forgets a removed entry's prepared names: a surviving hard-link alias no longer reaches
    /// it, and a mapping bound through it can no longer claim it.
    fn forget(&mut self, removed: &Path) {
        for names in self.aliases.values_mut() {
            names.retain(|name| name != removed);
        }
        for task in std::iter::once(&self.host).chain(self.tasks.values()) {
            for mapping in &mut *task.mappings.lock().recover() {
                if target_path(&mapping.target) == removed {
                    mapping.binding_removed = true;
                }
            }
        }
    }

    /// Resolves an explicit release of the physical absolute `path`: ancestor links are followed,
    /// the final component is not, and the resolved entry must be a protected, nonroot resource.
    pub fn release(&self, path: &Path, root: &Path) -> Result<Effects, String> {
        let mut effects = Effects::default();
        let mut resolved = PathBuf::new();
        self.walk(
            path.components(),
            false,
            &mut resolved,
            root,
            &mut effects,
            &mut 0,
        )?;
        match protected(root, &resolved)? {
            Some(relative) if !relative.as_os_str().is_empty() => {
                effects.dependencies.push(relative.clone());
                effects.release = Some(relative);
                Ok(effects)
            }
            Some(_) => Err("release names the source root, not a file".into()),
            None => Err(format!("{} is outside the source", resolved.display())),
        }
    }

    fn task(&self, tid: Pid) -> &TaskState {
        self.tasks.get(&tid).unwrap_or(&self.host)
    }

    fn opened(&self, info: &Syscall, target: &FileTarget, root: &Path) -> Result<(), String> {
        let Some(number) = returned(info).and_then(|value| i32::try_from(value).ok()) else {
            return Err("invalid returned descriptor".into());
        };
        let mut table = self.task(info.info.pid).descriptors.lock().recover();
        if protected(root, &target_path(target))?.is_some() {
            table.insert(
                number,
                Descriptor {
                    inode: Inode(target.device, target.inode),
                    mount: target.mount_id,
                    generation: self.generation,
                },
            );
        } else {
            table.remove(&number);
        }
        drop(table);
        Ok(())
    }

    fn descriptor_generation(
        &self,
        info: &Syscall,
        index: usize,
    ) -> Result<Option<WorkGeneration>, String> {
        let Some(target) = info.fd(index)? else {
            return Ok(None);
        };
        let number = descriptor_number(info, index)?;
        Ok(self
            .task(info.info.pid)
            .descriptors
            .lock()
            .recover()
            .get(&number)
            .filter(|origin| {
                origin.inode == Inode(target.device, target.inode)
                    && origin.mount == target.mount_id
            })
            .map(|origin| origin.generation))
    }

    fn check_fd(&self, info: &Syscall, index: usize, root: &Path) -> Result<(), String> {
        if self
            .descriptor_generation(info, index)?
            .is_some_and(|generation| generation != self.generation)
        {
            return Err("descriptor belongs to a retired work generation".into());
        }
        if let Some(target) = info.fd(index)? {
            Self::check_origin(target, root)?;
        }
        Ok(())
    }

    fn check_origin(target: &FileTarget, root: &Path) -> Result<(), String> {
        if target.links == 0 && protected(root, &target_path(target))?.is_some() {
            return Err("protected descriptor refers to an unlinked file".into());
        }
        Ok(())
    }

    fn target_effect(
        &self,
        effects: &mut Effects,
        target: &FileTarget,
        root: &Path,
        read: bool,
        write: bool,
    ) -> Result<(), String> {
        Self::check_origin(target, root)?;
        let path = target_path(target);
        let paths = self.aliases.get(&Inode(target.device, target.inode));
        for path in paths.map_or_else(|| std::slice::from_ref(&path), Vec::as_slice) {
            if let Some(relative) = protected(root, path)? {
                if target.mode & libc::S_IFMT != libc::S_IFREG
                    && target.mode & libc::S_IFMT != libc::S_IFDIR
                {
                    return Err("unsupported protected descriptor kind".into());
                }
                effects.dependencies.push(relative.clone());
                if read {
                    effects.reads.push(relative.clone());
                }
                if write {
                    effects.writes.push(relative);
                }
            } else if write {
                effects.outside_writes.push(path.clone());
            }
        }
        Ok(())
    }

    fn fd_effect(
        &self,
        effects: &mut Effects,
        info: &Syscall,
        index: usize,
        root: &Path,
        read: bool,
        write: bool,
    ) -> Result<(), String> {
        self.check_fd(info, index, root)?;
        let target = info
            .fd(index)?
            .ok_or_else(|| format!("successful {} lacks descriptor identity", info.info.syscall))?;
        self.target_effect(effects, target, root, read, write)
    }

    fn path(
        &self,
        info: &Syscall,
        dirfd: Option<usize>,
        index: usize,
        root: &Path,
        effects: &mut Effects,
    ) -> Result<PathBuf, String> {
        let bytes = match info.path(index) {
            Some(bytes) => bytes,
            None if matches!(info.info.args.0.get(index), Some(SyscallArg::Addr(0))) => b"",
            None => return Err(format!("{} lacks captured path bytes", info.info.syscall)),
        };
        let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base = match dirfd {
                Some(index) => info.fd(index)?,
                None => info.cwd.as_ref(),
            }
            .ok_or_else(|| {
                format!(
                    "{} lacks stopped-task cwd/dirfd identity",
                    info.info.syscall
                )
            })?;
            target_path(base).join(path)
        };
        let follow = match info.info.syscall {
            Sysno::readlink
            | Sysno::readlinkat
            | Sysno::lstat
            | Sysno::lchown
            | Sysno::mkdir
            | Sysno::mkdirat
            | Sysno::rmdir
            | Sysno::unlink
            | Sysno::unlinkat
            | Sysno::rename
            | Sysno::renameat
            | Sysno::renameat2
            | Sysno::link
            | Sysno::linkat
            | Sysno::symlink
            | Sysno::symlinkat
            | Sysno::mknod
            | Sysno::mknodat => false,
            Sysno::newfstatat | Sysno::utimensat | Sysno::fchmodat2 => {
                integer(info, 3)? & u64::from(libc::AT_SYMLINK_NOFOLLOW.cast_unsigned()) == 0
            }
            Sysno::statx => {
                integer(info, 2)? & u64::from(libc::AT_SYMLINK_NOFOLLOW.cast_unsigned()) == 0
            }
            Sysno::fchownat => {
                integer(info, 4)? & u64::from(libc::AT_SYMLINK_NOFOLLOW.cast_unsigned()) == 0
            }
            Sysno::open => integer(info, 1)? & u64::from(libc::O_NOFOLLOW.cast_unsigned()) == 0,
            Sysno::openat => integer(info, 2)? & u64::from(libc::O_NOFOLLOW.cast_unsigned()) == 0,
            Sysno::openat2 => {
                captured_flags(info)? & u64::from(libc::O_NOFOLLOW.cast_unsigned()) == 0
            }
            _ => true,
        };
        let mut resolved = PathBuf::new();
        self.walk(
            absolute.components(),
            follow,
            &mut resolved,
            root,
            effects,
            &mut 0,
        )?;
        dependency(effects, root, &resolved)?;
        Ok(resolved)
    }

    /// Walks the immutable preparation index plus observed namespace changes, preserving lookup
    /// dependencies. Recursion follows at most forty links and does not allocate a component queue.
    fn walk(
        &self,
        mut parts: std::path::Components<'_>,
        follow_leaf: bool,
        resolved: &mut PathBuf,
        root: &Path,
        effects: &mut Effects,
        links: &mut usize,
    ) -> Result<(), String> {
        while let Some(part) = parts.next() {
            match part {
                Component::CurDir => continue,
                Component::ParentDir => {
                    resolved.pop();
                    continue;
                }
                Component::RootDir => {
                    resolved.clear();
                    resolved.push(part.as_os_str());
                }
                _ => resolved.push(part.as_os_str()),
            }
            if (follow_leaf || parts.clone().next().is_some())
                && let Some(target) = self.links.get(resolved)
            {
                dependency(effects, root, resolved)?;
                *links += 1;
                if *links > 40 {
                    return Err("symbolic-link lookup exceeds the kernel limit".into());
                }
                resolved.pop();
                self.walk(target.components(), true, resolved, root, effects, links)?;
            }
        }
        Ok(())
    }

    fn mapping_effects(
        &self,
        info: &Syscall,
        root: &Path,
        effects: &mut Effects,
    ) -> Result<(), String> {
        let start = integer(info, 0)?;
        let end = start
            .checked_add(integer(info, 1)?)
            .ok_or_else(|| "mapping range overflow".to_string())?;
        let group = Arc::clone(&self.task(info.info.pid).mappings);
        let mut mappings = group.lock().recover();
        let mut after = Vec::new();
        for mapping in &*mappings {
            if start >= mapping.end || end <= mapping.start {
                continue;
            }
            if mapping.generation != self.generation
                && protected(root, &target_path(&mapping.target))?.is_some()
            {
                return Err("mapping belongs to a retired work generation".into());
            }
            if mapping.binding_removed && info.info.syscall != Sysno::munmap {
                return Err("mapped file's name was removed".into());
            }
            Self::check_origin(&mapping.target, root)?;
            if matches!(info.info.syscall, Sysno::mprotect | Sysno::pkey_mprotect) {
                let protection = integer(info, 2)?;
                self.target_effect(
                    effects,
                    &mapping.target,
                    root,
                    protection & u64::from((libc::PROT_READ | libc::PROT_EXEC).cast_unsigned())
                        != 0,
                    mapping.shared && protection & u64::from(libc::PROT_WRITE.cast_unsigned()) != 0,
                )?;
            }
            if info.info.syscall == Sysno::munmap {
                if mapping.start < start {
                    after.push(Mapping {
                        end: start,
                        ..mapping.clone()
                    });
                }
                if end < mapping.end {
                    after.push(Mapping {
                        start: end,
                        ..mapping.clone()
                    });
                }
            }
            if info.info.syscall == Sysno::mremap {
                let address =
                    returned(info).ok_or_else(|| "mremap lacks returned address".to_string())?;
                after.push(Mapping {
                    start: address,
                    end: address
                        .checked_add(integer(info, 2)?)
                        .ok_or_else(|| "mremap overflow".to_string())?,
                    ..mapping.clone()
                });
            }
        }
        if matches!(info.info.syscall, Sysno::munmap | Sysno::mremap) {
            mappings.retain(|mapping| start >= mapping.end || end <= mapping.start);
            mappings.extend(after);
        }
        drop(mappings);
        Ok(())
    }
}

/// A child's view of one kernel state table: the parent's own table when shared, else a copy.
fn inherit<T: Clone>(parent: &Arc<Mutex<T>>, shared: bool) -> Arc<Mutex<T>> {
    if shared {
        return Arc::clone(parent);
    }
    Arc::new(Mutex::new(parent.lock().recover().clone()))
}
fn target_path(target: &FileTarget) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(&target.path))
}
fn descriptor_number(info: &Syscall, index: usize) -> Result<i32, String> {
    match info.info.args.0.get(index) {
        Some(SyscallArg::Int(value)) => i32::try_from(*value).map_err(|error| error.to_string()),
        _ => Err(format!(
            "{} has no descriptor number {index}",
            info.info.syscall
        )),
    }
}
fn integer(info: &Syscall, index: usize) -> Result<u64, String> {
    match info.info.args.0.get(index) {
        Some(SyscallArg::Int(value)) => Ok(value.cast_unsigned()),
        Some(SyscallArg::Addr(value)) => u64::try_from(*value).map_err(|error| error.to_string()),
        _ => Err(format!(
            "{} has no integer argument {index}",
            info.info.syscall
        )),
    }
}
fn captured_flags(info: &Syscall) -> Result<u64, String> {
    info.flags
        .ok_or_else(|| format!("{} has no captured flags", info.info.syscall))
}
fn returned(info: &Syscall) -> Option<u64> {
    match info.info.result {
        RetCode::Ok(value) => u64::try_from(value).ok(),
        RetCode::Address(value) => u64::try_from(value).ok(),
        RetCode::Err(_) => None,
    }
}
fn protected(root: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|part| part.as_os_str().to_str().is_none())
    {
        return Err("protected path is not representable as a UTF-8 policy resource".into());
    }
    Ok(Some(relative.to_path_buf()))
}
fn dependency(effects: &mut Effects, root: &Path, path: &Path) -> Result<(), String> {
    if let Some(relative) = protected(root, path)? {
        effects.dependencies.push(relative);
    }
    Ok(())
}
fn write_path(effects: &mut Effects, root: &Path, path: PathBuf) -> Result<(), String> {
    if let Some(relative) = protected(root, &path)? {
        effects.writes.push(relative);
    } else {
        effects.outside_writes.push(path);
    }
    Ok(())
}

#[cfg(test)]
#[path = "access/tests.rs"]
mod tests;
