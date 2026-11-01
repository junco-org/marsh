//! Launch confinement for privileged roots (`docs/threat-model.md` §8.1, §8.2).
//!
//! Every command of a confined root runs in that root's cgroup v2, which the BPF programs use as
//! its domain; in fresh mount and IPC namespaces whose `/tmp` and `/dev/shm` are the root's own
//! directories, with the source's shared scratch directory at `/tmp/shared`; in a session keyring
//! of its own; as a non-root identity; and in a Landlock domain that confines every write beneath
//! the root's writable directories and refuses signals and abstract UNIX sockets outside it.
//!
//! Everything a child needs is prepared in the parent: the post-fork steps neither allocate nor
//! lock.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The identity a confined root's commands run as. Never root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The user commands run as.
    pub uid: u32,
    /// Their primary group.
    pub gid: u32,
    /// Supplementary groups, from the group database.
    pub groups: Vec<u32>,
    /// The account's home directory, from the password database.
    pub home: PathBuf,
}

impl Identity {
    /// The account `uid` names in the password and group databases.
    ///
    /// # Errors
    /// Refuses root and a uid the password database does not know.
    pub fn of_user(uid: u32) -> io::Result<Self> {
        if uid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "privileged enforcement never runs governed commands as root",
            ));
        }
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .map_err(io::Error::from)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("uid {uid} has no password database entry"),
                )
            })?;
        let name = CString::new(user.name.as_bytes()).map_err(io::Error::other)?;
        let groups = nix::unistd::getgrouplist(&name, user.gid)
            .map_err(io::Error::from)?
            .into_iter()
            .map(nix::unistd::Gid::as_raw)
            .collect();
        Ok(Self {
            uid,
            gid: user.gid.as_raw(),
            groups,
            home: user.dir,
        })
    }

    /// The account owning `path`: the identity governed commands of a source run as.
    ///
    /// # Errors
    /// Refuses a root-owned path, and fails as [`Self::of_user`] does.
    pub fn owner_of(path: &Path) -> io::Result<Self> {
        let owner = std::fs::metadata(path)?.uid();
        if owner == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by root; privileged enforcement runs governed commands as the \
                     source's owner, so configure a non-root governed user explicitly",
                    path.display()
                ),
            ));
        }
        Self::of_user(owner)
    }
}

/// How a privileged root's commands are confined.
#[derive(Clone, Debug)]
pub struct Confinement {
    /// Who commands run as.
    pub identity: Identity,
    /// Directories (and files) commands may write: the private view and the operator's allowlist.
    pub writable: Vec<PathBuf>,
    /// The root's own directory, bound onto `/tmp`. It must contain a `shared` directory.
    pub tmp: PathBuf,
    /// The root's own directory, bound onto `/dev/shm` when the host has one.
    pub shm: PathBuf,
    /// The source's scratch directory every root of it shares, bound onto `/tmp/shared`.
    pub shared: PathBuf,
}

/// Device files a confined command may open for writing: discarding output and its terminal.
const DEVICES: [&str; 4] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/tty"];

// Landlock ABI (include/uapi/linux/landlock.h); version 6 adds the scopes used here.
const LANDLOCK_ABI: i64 = 6;
const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
/// Rights that apply to a file itself; the rest name directory entries.
const FILE_WRITES: u64 = ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE | ACCESS_FS_IOCTL_DEV;
/// Every write Landlock can confine. Reads and execution stay unrestricted.
const WRITES: u64 = FILE_WRITES
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_CHAR
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_BLOCK
    | ACCESS_FS_MAKE_SYM
    | ACCESS_FS_REFER;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[cfg_attr(not(feature = "privileged"), allow(dead_code))]
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// One root's cgroup v2: its domain in the kernel programs.
#[cfg_attr(
    not(feature = "privileged"),
    allow(
        dead_code,
        reason = "only the privileged kernel integration creates cgroups"
    )
)]
pub(crate) struct Cgroup {
    directory: PathBuf,
    /// The cgroup id the kernel reports (`bpf_get_current_cgroup_id`): its directory's inode.
    pub(crate) id: u64,
    /// `cgroup.procs`, opened by the daemon: a child writes `0` to join with this fd's authority.
    procs: OwnedFd,
}

#[cfg_attr(
    not(feature = "privileged"),
    allow(
        dead_code,
        reason = "only the privileged kernel integration creates cgroups"
    )
)]
impl Cgroup {
    /// Creates `marsh-<daemon pid>/<name>` under the cgroup v2 mount.
    pub(crate) fn create(name: &str) -> io::Result<Self> {
        let statfs = nix::sys::statfs::statfs(CGROUP_ROOT).map_err(io::Error::from)?;
        if statfs.filesystem_type() != nix::sys::statfs::CGROUP2_SUPER_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("privileged enforcement needs cgroup v2 mounted at {CGROUP_ROOT}"),
            ));
        }
        let base = Path::new(CGROUP_ROOT).join(format!("marsh-{}", std::process::id()));
        match std::fs::create_dir(&base) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = base.join(name);
        std::fs::create_dir(&directory)?;
        let id = std::fs::metadata(&directory)?.ino();
        let procs = std::fs::OpenOptions::new()
            .write(true)
            .open(directory.join("cgroup.procs"))?
            .into();
        Ok(Self {
            directory,
            id,
            procs,
        })
    }

    /// Kills every process left in the cgroup.
    pub(crate) fn kill(&self) -> io::Result<()> {
        match std::fs::write(self.directory.join("cgroup.kill"), b"1") {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Kills what is left, then removes the cgroup once the kernel has emptied it.
    pub(crate) fn remove(&self) -> io::Result<()> {
        self.kill()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::remove_dir(&self.directory) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error)
                    if error.raw_os_error() == Some(libc::EBUSY)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        // The daemon's base directory goes with its last root; another root may still hold it.
        if let Some(base) = self.directory.parent() {
            let _ = std::fs::remove_dir(base);
        }
        Ok(())
    }
}

/// A confinement resolved into what the forked child applies without allocating, and what the
/// interpreter's own accesses for the root apply on a thread of their own.
pub(crate) struct Prepared {
    cgroup: std::sync::Arc<Cgroup>,
    ruleset: OwnedFd,
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
    tmp: CString,
    shm: Option<CString>,
    shared: CString,
    /// The host directories behind `/tmp`, `/dev/shm` (when present) and `/tmp/shared`.
    tmp_dir: PathBuf,
    shm_dir: Option<PathBuf>,
    shared_dir: PathBuf,
}

const ROOT_DIR: &CStr = c"/";
const TMP_DIR: &CStr = c"/tmp";
const SHM_DIR: &CStr = c"/dev/shm";
const SHARED_DIR: &CStr = c"/tmp/shared";
const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;

impl Prepared {
    pub(crate) fn new(
        confinement: &Confinement,
        cgroup: std::sync::Arc<Cgroup>,
    ) -> io::Result<Self> {
        let path =
            |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other);
        if !confinement.tmp.join("shared").is_dir() {
            return Err(io::Error::other(format!(
                "{} has no shared mount point",
                confinement.tmp.display()
            )));
        }
        let mut writable = confinement.writable.clone();
        writable.extend([
            confinement.tmp.clone(),
            confinement.shm.clone(),
            confinement.shared.clone(),
        ]);
        let shm_dir = Path::new("/dev/shm")
            .is_dir()
            .then(|| confinement.shm.clone());
        Ok(Self {
            cgroup,
            ruleset: ruleset(&writable)?,
            uid: confinement.identity.uid,
            gid: confinement.identity.gid,
            groups: confinement.identity.groups.clone(),
            tmp: path(&confinement.tmp)?,
            shm: shm_dir.as_deref().map(path).transpose()?,
            shared: path(&confinement.shared)?,
            tmp_dir: confinement.tmp.clone(),
            shm_dir,
            shared_dir: confinement.shared.clone(),
        })
    }

    /// The host path a confined command sees `path` as: its private `/tmp`, `/tmp/shared` and
    /// `/dev/shm` are the root's own directories.
    pub(crate) fn host_path<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, Path> {
        let mounts = [
            (Path::new("/tmp/shared"), Some(&self.shared_dir)),
            (Path::new("/tmp"), Some(&self.tmp_dir)),
            (Path::new("/dev/shm"), self.shm_dir.as_ref()),
        ];
        for (mount, directory) in mounts {
            if let (Ok(rest), Some(directory)) = (path.strip_prefix(mount), directory) {
                return std::borrow::Cow::Owned(directory.join(rest));
            }
        }
        std::borrow::Cow::Borrowed(path)
    }

    /// Runs `access` on a thread of its own with the confined commands' identity and Landlock
    /// domain, so the interpreter's own filesystem accesses carry no more authority than the
    /// commands it runs. Credentials and Landlock domains are per thread; the thread ends with it.
    pub(crate) fn run<T: Send>(&self, access: impl FnOnce() -> T + Send) -> io::Result<T> {
        std::thread::scope(|scope| {
            let confined = scope.spawn(|| -> io::Result<T> {
                self.identity()?;
                // SAFETY: PR_SET_NO_NEW_PRIVS takes integer arguments only.
                check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
                self.restrict()?;
                Ok(access())
            });
            confined
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    }

    /// Becomes the governed identity on the calling thread. Raw syscalls change only this
    /// thread's credentials; libc's wrappers would change every thread's.
    fn identity(&self) -> io::Result<()> {
        // SAFETY: the group list is a live allocation made before any fork.
        check(unsafe {
            libc::syscall(libc::SYS_setgroups, self.groups.len(), self.groups.as_ptr())
        })?;
        // SAFETY: integer arguments only.
        check(unsafe { libc::syscall(libc::SYS_setresgid, self.gid, self.gid, self.gid) })?;
        // SAFETY: integer arguments only.
        check(unsafe { libc::syscall(libc::SYS_setresuid, self.uid, self.uid, self.uid) })?;
        Ok(())
    }

    /// In the forked child, still root: join the cgroup, then the private namespaces.
    pub(crate) fn enter(&self) -> io::Result<()> {
        check(retry(|| {
            // SAFETY: writes two bytes from a static buffer to a live descriptor.
            unsafe { libc::write(self.cgroup.procs.as_raw_fd(), c"0".as_ptr().cast(), 1) }
        }))?;
        // SAFETY: unshare takes only flags.
        check(unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWIPC) }.into())?;
        // Nothing mounted below may propagate back to the host's namespace.
        mount(None, ROOT_DIR, libc::MS_REC | libc::MS_PRIVATE)?;
        mount(Some(&self.tmp), TMP_DIR, libc::MS_BIND)?;
        mount(Some(&self.shared), SHARED_DIR, libc::MS_BIND)?;
        if let Some(shm) = &self.shm {
            mount(Some(shm), SHM_DIR, libc::MS_BIND)?;
        }
        Ok(())
    }

    /// In the forked child, after the bounding set is dropped: become the governed identity in a
    /// session keyring of its own.
    pub(crate) fn assume(&self) -> io::Result<()> {
        self.identity()?;
        // SAFETY: a null name asks for a fresh anonymous session keyring.
        check(unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_JOIN_SESSION_KEYRING,
                std::ptr::null::<libc::c_char>(),
            )
        })?;
        Ok(())
    }

    /// In the forked child, last before exec: enter the Landlock domain.
    pub(crate) fn restrict(&self) -> io::Result<()> {
        // SAFETY: integer arguments only; the ruleset is a live descriptor.
        check(unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            )
        })
        .map(drop)
    }
}

/// Builds the Landlock ruleset: writes only beneath `writable` and to the shared devices, and
/// no signals or abstract UNIX connections outside the domain.
fn ruleset(writable: &[PathBuf]) -> io::Result<OwnedFd> {
    // SAFETY: a null attribute with the version flag only queries the ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < LANDLOCK_ABI {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "privileged enforcement needs Landlock ABI {LANDLOCK_ABI} or later (found {}): \
                 enable landlock in the boot LSM list",
                if abi < 0 {
                    io::Error::last_os_error().to_string()
                } else {
                    abi.to_string()
                }
            ),
        ));
    }
    let attr = RulesetAttr {
        handled_access_fs: WRITES,
        handled_access_net: 0,
        scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
    };
    // SAFETY: attr is a live, fully initialized ruleset attribute of the given size.
    let fd = check(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            size_of::<RulesetAttr>(),
            0,
        )
    })?;
    // SAFETY: the successful syscall returned a fresh close-on-exec descriptor owned here.
    let ruleset = unsafe { OwnedFd::from_raw_fd(i32::try_from(fd).map_err(io::Error::other)?) };
    for path in writable {
        allow(&ruleset, path, None)?;
    }
    for device in DEVICES {
        if Path::new(device).exists() {
            allow(&ruleset, Path::new(device), Some(FILE_WRITES))?;
        }
    }
    Ok(ruleset)
}

/// Allows `access` beneath `path`: every write for a directory, file writes for anything else.
fn allow(ruleset: &OwnedFd, path: &Path, access: Option<u64>) -> io::Result<()> {
    let directory = std::fs::metadata(path)?.is_dir();
    let parent = std::fs::File::from(
        nix::fcntl::open(
            path,
            nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let attr = PathBeneathAttr {
        allowed_access: access.unwrap_or(if directory { WRITES } else { FILE_WRITES }),
        parent_fd: parent.as_raw_fd(),
    };
    // SAFETY: attr is a live, fully initialized path-beneath rule; both descriptors are live.
    check(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &raw const attr,
            0,
        )
    })
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("allowing writes beneath {}: {error}", path.display()),
        )
    })?;
    Ok(())
}

fn mount(source: Option<&CStr>, target: &CStr, flags: libc::c_ulong) -> io::Result<()> {
    // SAFETY: every pointer is null or a live NUL-terminated string prepared before the fork.
    check(
        unsafe {
            libc::mount(
                source.map_or(std::ptr::null(), CStr::as_ptr),
                target.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            )
        }
        .into(),
    )
    .map(drop)
}

fn retry(mut call: impl FnMut() -> isize) -> i64 {
    loop {
        let result = call();
        if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return result as i64;
        }
    }
}

fn check(result: i64) -> io::Result<i64> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::os::unix::process::CommandExt as _;

    /// A confined command writes beneath its writable directories and to discarded output only,
    /// may move entries between them, and cannot signal outside its Landlock domain.
    #[test]
    fn landlock_confines_writes_and_scopes_signals() {
        // SAFETY: a null attribute with the version flag only queries the ABI.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < LANDLOCK_ABI {
            eprintln!("skipped: this kernel offers Landlock ABI {abi}, below {LANDLOCK_ABI}");
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let (allowed, other) = (scratch.path().join("allowed"), scratch.path().join("other"));
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&other).unwrap();
        let ruleset = ruleset(std::slice::from_ref(&allowed)).unwrap();
        let raw = ruleset.as_raw_fd();
        let script = "set -u; \
            echo ok > allowed/file || exit 10; \
            echo out > /dev/null || exit 11; \
            if echo no > other/file 2>/dev/null; then exit 12; fi; \
            mkdir allowed/a allowed/b && echo x > allowed/a/f || exit 13; \
            mv allowed/a/f allowed/b/f || exit 14; \
            if kill -0 $PPID 2>/dev/null; then exit 15; fi";
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).current_dir(scratch.path());
        let restrict = move || {
            // SAFETY: this prctl operation takes integer arguments, not pointers.
            check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
            // SAFETY: the syscall reads only the ruleset descriptor, which outlives the spawn.
            check(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, raw, 0) }).map(drop)
        };
        // SAFETY: the hook only makes those two async-signal-safe syscalls.
        unsafe {
            command.pre_exec(restrict);
        }
        let status = command.status().unwrap();
        drop(ruleset);
        assert!(status.success(), "confined script failed: {status}");
        assert!(!other.join("file").exists());
        assert!(allowed.join("b/f").exists());
    }
}
