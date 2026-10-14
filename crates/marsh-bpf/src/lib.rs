//! Privileged-only BPF LSM loader and root-scoped policy maps.
//!
//! Enable `privileged` to build and embed the real BPF object. Without that feature,
//! this crate builds without BPF tooling and [`Controller::load`] returns an error.
//! Install `nightly-2025-12-01` with `rust-src`, clang 21, and `bpf-linker =0.9.15`
//! (LLVM 21), then build with `cargo build -p marsh-bpf --features privileged`.
//! The nested BPF build uses the pinned nightly and never host kernel headers.
//!
//! Publish networks and local endpoints before answers: [`Controller::configure`]
//! and [`Controller::local`] invalidate old answers while replacing their maps.
//! Keep newly governed children stopped until [`Controller::task`] succeeds.
//! Every map/reporting failure permanently fails closed; create a new controller
//! only after terminating the old controller's governed tasks. Maps and links are
//! never pinned; kernel-created descriptors are close-on-exec. Dropping the
//! controller detaches its programs and releases every descriptor.
//!
//! PID attribution uses the loader's actual PID namespace. Its procfs must expose
//! that namespace (including PID 1); children entering another PID namespace are
//! rejected before resume. Nested tracer namespaces with a matching procfs work.

use std::{
    collections::BTreeMap,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
};

#[cfg(feature = "privileged")]
use aya::{Btf, maps::Map, programs::Lsm};
use aya::{
    Ebpf, Pod,
    maps::{Array, HashMap, LpmTrie, MapData, MapError, RingBuf, lpm_trie::Key},
};
use marsh_peer_class::{
    AF_INET, AF_INET6, AF_UNIX, Answers, Contact, Endpoint, LocalKey, MAX_CLASSES, Network,
    effective_networks,
};

#[repr(transparent)]
#[derive(Clone, Copy)]
struct WireAnswers(Answers);
// SAFETY: Answers contains only fixed-width integer fields and has no padding.
unsafe impl Pod for WireAnswers {}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct WireLocal(LocalKey);
// SAFETY: LocalKey contains only fixed-width integers/byte arrays and no padding.
unsafe impl Pod for WireLocal {}

/// Owns the attached LSM programs and all their unpinned policy/reporting maps.
///
/// Share this controller behind the tracer service's mutex. Loss is sticky: any
/// failed operation poisons enforcement for all mapped tasks, including audit roots.
/// Tasks not in the task map are always left unaffected.
pub struct Controller {
    // Drop links before maps. Aya owns the links and detaches them on Ebpf::drop.
    programs: Option<Ebpf>,
    task_root: HashMap<MapData, u32, u64>,
    answers: HashMap<MapData, u64, WireAnswers>,
    net4: LpmTrie<MapData, [u8; 12], u32>,
    net6: LpmTrie<MapData, [u8; 24], u32>,
    local: HashMap<MapData, WireLocal, u32>,
    events: RingBuf<MapData>,
    health: Array<MapData, u64>,
    // pidfds avoid killing a recycled PID if poisoning the HEALTH map itself fails.
    tasks: BTreeMap<u32, OwnedFd>,
    pid_namespace: [u64; 2],
    _pid_namespace: OwnedFd,
}

impl Controller {
    /// Load and attach both socket hooks and the mandatory SQPOLL refusal hook.
    ///
    /// Requires active BPF LSM, kernel BTF, ring buffers, and sufficient privileges.
    /// A kernel lacking any requirement is never given an unprivileged fallback.
    /// Successful loading makes the tracer nondumpable, preventing capless traced
    /// processes from stealing its BPF descriptors. Failed loading leaves the
    /// dumpability flag unchanged, so an explicit Auto fallback remains usable.
    pub fn load() -> io::Result<Self> {
        #[cfg(not(feature = "privileged"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "privileged enforcement was not compiled; enable the privileged feature",
            ))
        }
        #[cfg(feature = "privileged")]
        {
            let btf = prerequisites()?;
            let (pid_namespace_fd, pid_namespace) = own_pid_namespace()?;
            let mut programs = Ebpf::load(aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/marsh-bpf.o")))
                .map_err(|error| io::Error::other(format!("loading BPF LSM maps/CO-RE relocations failed (BPF privileges and ring-buffer support required): {error}")))?;
            let task_root =
                HashMap::try_from(take(&mut programs, "TASK_ROOT")?).map_err(io::Error::other)?;
            let answers =
                HashMap::try_from(take(&mut programs, "ANSWERS")?).map_err(io::Error::other)?;
            let net4 =
                LpmTrie::try_from(take(&mut programs, "CLASS_NET4")?).map_err(io::Error::other)?;
            let net6 =
                LpmTrie::try_from(take(&mut programs, "CLASS_NET6")?).map_err(io::Error::other)?;
            let local = HashMap::try_from(take(&mut programs, "LOCAL_ALLOWED")?)
                .map_err(io::Error::other)?;
            let events =
                RingBuf::try_from(take(&mut programs, "EVENTS")?).map_err(io::Error::other)?;
            let health =
                Array::try_from(take(&mut programs, "HEALTH")?).map_err(io::Error::other)?;
            let mut namespace_map: Array<_, [u64; 2]> = Array::try_from(
                programs
                    .map_mut("PID_NAMESPACE")
                    .ok_or_else(|| io::Error::other("missing BPF PID namespace configuration"))?,
            )
            .map_err(io::Error::other)?;
            namespace_map
                .set(0, pid_namespace, 0)
                .map_err(io::Error::other)?;
            for name in ["socket_connect", "socket_sendmsg", "uring_sqpoll"] {
                let program: &mut Lsm = programs
                    .program_mut(name)
                    .ok_or_else(|| io::Error::other(format!("missing BPF program {name}")))?
                    .try_into()
                    .map_err(io::Error::other)?;
                program.load(name, &btf).map_err(|error| {
                    io::Error::other(format!("loading BPF LSM {name} failed: {error}"))
                })?;
                program.attach().map_err(|error| {
                    io::Error::other(format!("attaching BPF LSM {name} failed: {error}"))
                })?;
            }
            // Do this last: no governed child exists yet, and a failed load must
            // not make Auto's unprivileged fallback inherit nondumpable children.
            // SAFETY: PR_SET_DUMPABLE accepts scalar arguments and no pointers.
            if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
                let error = io::Error::last_os_error();
                return Err(io::Error::new(error.kind(), format!("protecting BPF descriptors with PR_SET_DUMPABLE failed: {error}")));
            }
            Ok(Self {
                programs: Some(programs),
                task_root,
                answers,
                net4,
                net6,
                local,
                events,
                health,
                tasks: BTreeMap::new(),
                pid_namespace,
                _pid_namespace: pid_namespace_fd,
            })
        }
    }

    /// Replace this root's networks, preserving first-declaration policy precedence.
    ///
    /// Every trie key starts with all 64 root bits: even `/0` cannot match another
    /// root. Existing answers are invalidated; publish new answers afterwards.
    pub fn configure(&mut self, root: u64, networks: &[Network]) -> io::Result<()> {
        self.update(|this| {
            validate_networks(networks)?;
            absent_ok(this.answers.remove(&root))?;
            this.clear_networks(root)?;
            for network in effective_networks(networks) {
                let class = u32::from(network.class);
                if network.family == AF_INET {
                    this.net4
                        .insert(
                            &Key::new(64 + network.prefix, net4_key(root, &network.address)),
                            class,
                            0,
                        )
                        .map_err(io::Error::other)?;
                } else {
                    this.net6
                        .insert(
                            &Key::new(64 + network.prefix, net6_key(root, &network.address)),
                            class,
                            0,
                        )
                        .map_err(io::Error::other)?;
                }
            }
            Ok(())
        })
    }

    /// Atomically publish the complete answer generation after dependent maps are ready.
    pub fn answers(&mut self, root: u64, answers: Answers) -> io::Result<()> {
        self.update(|this| {
            this.answers
                .insert(root, WireAnswers(answers), 0)
                .map_err(io::Error::other)
        })
    }

    /// Mark a root stale, retaining its generation, audit flag, and guarded-class mask.
    /// A root without answers already denies every contact and needs no update.
    pub fn stale(&mut self, root: u64) -> io::Result<()> {
        self.update(|this| {
            let mut answers = match this.answers.get(&root, 0) {
                Ok(answers) => answers,
                Err(MapError::KeyNotFound) => return Ok(()),
                Err(error) => return Err(io::Error::other(error)),
            };
            answers.0.stale = 1;
            this.answers
                .insert(root, answers, 0)
                .map_err(io::Error::other)
        })
    }

    /// Attribute all threads of a stopped process to a root before the child resumes.
    /// Missing root answers deny contacts, rather than leaving the child ungoverned.
    /// The process must remain in the controller's PID namespace; namespace-changing
    /// children are refused before resume, rather than silently losing attribution.
    pub fn task(&mut self, tgid: u32, root: u64) -> io::Result<()> {
        self.update(|this| {
            if let std::collections::btree_map::Entry::Vacant(entry) = this.tasks.entry(tgid) {
                entry.insert(pidfd(tgid)?);
            }
            let namespace = std::fs::metadata(format!("/proc/{tgid}/ns/pid"))?;
            if [namespace.dev(), namespace.ino()] != this.pid_namespace {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "governed child changed PID namespace; privileged attribution cannot resume it",
                ));
            }
            this.task_root
                .insert(tgid, root, 0)
                .map_err(io::Error::other)
        })
    }

    /// Remove an exited process's mapping; repeated cleanup is harmless.
    pub fn remove_task(&mut self, tgid: u32) -> io::Result<()> {
        if let Err(error) = absent_ok(self.task_root.remove(&tgid)) {
            let error = self.poison(error);
            let cleanup = self.terminate();
            return Err(io::Error::other(format!(
                "{error}; task-map cleanup failed; controller termination: {cleanup:?}"
            )));
        }
        self.tasks.remove(&tgid);
        Ok(())
    }

    /// Remove a root's answers, network declarations, and local allowlist.
    /// Any process still mapped to this root remains governed and denied.
    pub fn remove_root(&mut self, root: u64) -> io::Result<()> {
        let result = (|| {
            absent_ok(self.answers.remove(&root))?;
            self.clear_networks(root)?;
            self.clear_local(root)
        })();
        result.map_err(|error| self.poison(error))
    }

    /// Send SIGKILL to every governed pidfd, clean up even if poisoned, and detach.
    /// Call on a shared-service failure. Signal failures retain the hooks; after
    /// successful signaling, map cleanup errors are returned but stale numeric
    /// PID keys cannot affect PID reuse because the programs are detached.
    pub fn terminate(&mut self) -> io::Result<()> {
        let mut failure = self.health.set(0, 1, 0).map_err(io::Error::other).err();
        self.kill_tasks()?;
        for tgid in self.tasks.keys() {
            if let Err(error) = absent_ok(self.task_root.remove(tgid)) {
                failure = Some(error);
            }
        }
        self.tasks.clear();
        self.programs.take();
        failure.map_or(Ok(()), Err)
    }

    /// Replace normalized local receiver endpoints, invalidating old answers first.
    ///
    /// Abstract UNIX name length is significant. Wildcard IP listeners match
    /// loopback contacts of the same family and port in the kernel; other endpoints
    /// must match exactly. Publish the refreshed answers after this method returns.
    pub fn local(&mut self, root: u64, endpoints: &[Endpoint]) -> io::Result<()> {
        self.update(|this| {
            absent_ok(this.answers.remove(&root))?;
            this.clear_local(root)?;
            for endpoint in endpoints {
                let endpoint = endpoint.normalized();
                if !matches!(endpoint.family, AF_UNIX | AF_INET | AF_INET6) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "local allowlist contains an unknown endpoint",
                    ));
                }
                this.local
                    .insert(WireLocal(LocalKey { root, endpoint }), 1, 0)
                    .map_err(io::Error::other)?;
            }
            Ok(())
        })
    }

    /// Drain real contacts. Overflow/malformed records/map errors permanently fail closed.
    /// Reporting loss is fatal even when the root uses audit mode.
    pub fn drain(&mut self) -> io::Result<Vec<Contact>> {
        self.update(|this| {
            let mut contacts = Vec::new();
            while let Some(bytes) = this.events.next() {
                if bytes.len() != size_of::<Contact>() {
                    return Err(io::Error::other(
                        "malformed BPF contact record; enforcement poisoned",
                    ));
                }
                // SAFETY: bytes has exactly Contact's size; Contact consists entirely
                // of integers/byte arrays, and read_unaligned handles ring alignment.
                let contact = unsafe { bytes.as_ptr().cast::<Contact>().read_unaligned() };
                if contact.class as usize >= MAX_CLASSES
                    || contact.decision > 4
                    || contact.endpoint.unix_len > 108
                {
                    return Err(io::Error::other(
                        "invalid BPF contact record; enforcement poisoned",
                    ));
                }
                contacts.push(contact);
            }
            // Loss racing the first check must not be returned as a successful drain.
            this.check_health()?;
            Ok(contacts)
        })
    }

    /// Whether socket `sendfile`/`splice` can skip the ptrace peer gate.
    ///
    /// False conservatively on every kernel: version numbers do not establish
    /// backported `sock_sendmsg` routing. Keep the existing ptrace fallback until
    /// that path is positively probed on the deployment kernel.
    pub const fn sendfile_covered(&self) -> bool {
        false
    }

    fn check_health(&self) -> io::Result<()> {
        if self.health.get(&0, 0).map_err(io::Error::other)? != 0 {
            return Err(io::Error::other(
                "BPF reporting/map loss: privileged enforcement is permanently poisoned",
            ));
        }
        Ok(())
    }

    fn update<T>(&mut self, action: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        let result = self.check_health().and_then(|()| action(self));
        result.map_err(|error| self.poison(error))
    }

    fn poison(&mut self, error: io::Error) -> io::Error {
        if let Err(map_error) = self.health.set(0, 1, 0) {
            // If even the poison switch is unavailable, pidfds are the last
            // fail-closed boundary. Never wait here: ptrace exit stops need the tracer.
            let termination = self.terminate();
            return io::Error::other(format!(
                "{error}; cannot poison BPF maps: {map_error}; termination: {termination:?}"
            ));
        }
        error
    }

    fn kill_tasks(&self) -> io::Result<()> {
        let mut failure = None;
        for process in self.tasks.values() {
            // SAFETY: process is a live owned pidfd; no siginfo is supplied.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    process.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    failure = Some(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn clear_networks(&mut self, root: u64) -> io::Result<()> {
        // ponytail: scan shared maps on replacement; track per-root keys if this becomes hot.
        let keys: Vec<_> = self
            .net4
            .keys()
            .filter(|key| {
                key.as_ref()
                    .map_or(true, |key| key.data()[..8] == root.to_be_bytes())
            })
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for key in keys {
            self.net4.remove(&key).map_err(io::Error::other)?;
        }
        let keys: Vec<_> = self
            .net6
            .keys()
            .filter(|key| {
                key.as_ref()
                    .map_or(true, |key| key.data()[..8] == root.to_be_bytes())
            })
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for key in keys {
            self.net6.remove(&key).map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn clear_local(&mut self, root: u64) -> io::Result<()> {
        let keys: Vec<_> = self
            .local
            .keys()
            .filter(|key| key.as_ref().map_or(true, |key| key.0.root == root))
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for key in keys {
            self.local.remove(&key).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

fn net4_key(root: u64, address: &[u8; 16]) -> [u8; 12] {
    let mut key = [0; 12];
    key[..8].copy_from_slice(&root.to_be_bytes());
    key[8..].copy_from_slice(&address[..4]);
    key
}

fn net6_key(root: u64, address: &[u8; 16]) -> [u8; 24] {
    let mut key = [0; 24];
    key[..8].copy_from_slice(&root.to_be_bytes());
    key[8..].copy_from_slice(address);
    key
}

fn validate_networks(networks: &[Network]) -> io::Result<()> {
    for network in networks {
        let width = match network.family {
            AF_INET => 32,
            AF_INET6 => 128,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported declared network family",
                ));
            }
        };
        if network.prefix > width || network.class < 5 || usize::from(network.class) >= MAX_CLASSES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid network prefix or class id (declared labels must be 5..64)",
            ));
        }
    }
    Ok(())
}

fn absent_ok(result: Result<(), MapError>) -> io::Result<()> {
    match result {
        Ok(()) | Err(MapError::KeyNotFound) => Ok(()),
        Err(MapError::SyscallError(error))
            if error.io_error.raw_os_error() == Some(libc::ENOENT) =>
        {
            Ok(())
        }
        Err(error) => Err(io::Error::other(error)),
    }
}

fn pidfd(tgid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes an integer PID and flags, not pointers; its returned
    // descriptor is fresh, close-on-exec, and owned only by this function.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = i32::try_from(descriptor).map_err(io::Error::other)?;
    // SAFETY: the successful syscall returned a fresh owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(feature = "privileged")]
fn take(programs: &mut Ebpf, name: &str) -> io::Result<Map> {
    programs
        .take_map(name)
        .ok_or_else(|| io::Error::other(format!("missing BPF map {name}")))
}

#[cfg(feature = "privileged")]
fn own_pid_namespace() -> io::Result<(OwnedFd, [u64; 2])> {
    let namespace = std::fs::File::open("/proc/self/ns/pid")?;
    let own = namespace.metadata()?;
    let visible = std::fs::metadata("/proc/1/ns/pid")?;
    let identity = [own.dev(), own.ino()];
    if [visible.dev(), visible.ino()] != identity {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "procfs PID namespace does not match the tracer: mount procfs in the tracer's PID namespace",
        ));
    }
    Ok((namespace.into(), identity))
}

#[cfg(feature = "privileged")]
fn prerequisites() -> io::Result<Btf> {
    let active = std::fs::read_to_string("/sys/kernel/security/lsm").map_err(|error| {
        io::Error::new(error.kind(), format!("cannot read active LSM list /sys/kernel/security/lsm (mount securityfs and enable CONFIG_BPF_LSM): {error}"))
    })?;
    if !active.trim().split(',').any(|name| name.trim() == "bpf") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "BPF LSM is not active: enable CONFIG_BPF_LSM and add bpf to the lsm= boot parameter",
        ));
    }
    Btf::from_sys_fs().map_err(|error| io::Error::other(format!("kernel BTF unavailable at /sys/kernel/btf/vmlinux: enable CONFIG_DEBUG_INFO_BTF: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_and_root_prefixes_are_isolated() {
        assert_eq!(size_of::<Answers>(), 40);
        assert_eq!(size_of::<WireAnswers>(), 40);
        assert_eq!(size_of::<Endpoint>(), 136);
        assert_eq!(size_of::<WireLocal>(), 144);
        assert_eq!(size_of::<Contact>(), 168);
        let address = [0; 16];
        assert_ne!(net4_key(1, &address)[..8], net4_key(2, &address)[..8]);
        assert_ne!(net6_key(1, &address)[..8], net6_key(2, &address)[..8]);
        assert_eq!(net4_key(1, &address)[..8], net6_key(1, &address)[..8]);
        let valid = Network {
            family: AF_INET,
            prefix: 0,
            class: 5,
            address,
        };
        assert!(validate_networks(&[valid]).is_ok());
        assert!(
            validate_networks(&[Network {
                prefix: 33,
                ..valid
            }])
            .is_err()
        );
        assert!(validate_networks(&[Network { class: 64, ..valid }]).is_err());
        assert!(validate_networks(&[Network { class: 4, ..valid }]).is_err());
    }
}
