//! Privileged-only BPF LSM loader and the maps its programs read.
//!
//! Enable `privileged` to compile `src/marsh.bpf.c` with clang and embed it. Without that feature
//! this crate builds without BPF tooling and [`Controller::load`] returns an error.
//!
//! Publish a root's networks before its answers: [`Controller::configure`] withdraws old answers
//! while it replaces the trie. Keep a newly governed child stopped until [`Controller::task`]
//! succeeds. Every map or reporting failure poisons enforcement for good; create a new controller
//! only after terminating the old one's governed tasks. Maps and links are never pinned, and
//! kernel-created descriptors are close-on-exec. Dropping the controller detaches its programs.
//!
//! PID attribution uses the loader's actual PID namespace. Its procfs must expose that namespace
//! (including PID 1); children entering another PID namespace are refused before they resume.

use std::{
    collections::{BTreeMap, HashSet},
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
};

#[cfg(feature = "privileged")]
use aya::{
    Btf,
    maps::Map,
    programs::{FExit, Lsm},
};
use aya::{
    Ebpf, Pod,
    maps::{Array, HashMap, LpmTrie, MapData, MapError, RingBuf, lpm_trie::Key},
};
use marsh_peer_class::{
    AF_INET, AF_INET6, Answers, Endpoint, Event, MAX_CLASSES, Network, RECEIPT, Receipts,
    UNANSWERED, effective_networks,
};

/// Every LSM hook the programs attach to; all are required.
#[cfg(feature = "privileged")]
const HOOKS: [&str; 7] = [
    "socket_connect",
    "socket_sendmsg",
    "unix_may_send",
    "socket_post_create",
    "unix_stream_connect",
    "sk_free_security",
    "uring_sqpoll",
];

/// Socket reads, traced on return so a reader blocked before tainted bytes arrived is caught.
#[cfg(feature = "privileged")]
const READS: [&str; 2] = ["sock_recvmsg", "sock_splice_read"];

macro_rules! pod {
    ($wire:ident, $inner:ty) => {
        #[repr(transparent)]
        #[derive(Clone, Copy)]
        struct $wire($inner);
        // SAFETY: the record holds only fixed-width integers and byte arrays, with no padding;
        // marsh-peer-class asserts its size against marsh.h.
        unsafe impl Pod for $wire {}
    };
}
pod!(WireAnswers, Answers);
pod!(WireReceipts, Receipts);
pod!(WireEndpoint, Endpoint);

/// Owns the attached LSM programs and all their unpinned maps.
///
/// Share it behind the tracer's mutex. Loss is sticky: any failed operation refuses every mapped
/// task's contacts, auditing roots included. Tasks outside the task map are never affected.
pub struct Controller {
    // Drop links before maps: Aya detaches the programs when `Ebpf` drops.
    programs: Option<Ebpf>,
    task_root: HashMap<MapData, u32, u64>,
    answers: HashMap<MapData, u64, WireAnswers>,
    received: HashMap<MapData, u64, WireReceipts>,
    net4: LpmTrie<MapData, [u8; 12], u32>,
    net6: LpmTrie<MapData, [u8; 24], u32>,
    local: HashMap<MapData, WireEndpoint, u32>,
    allowed: HashSet<Endpoint>,
    events: RingBuf<MapData>,
    health: Array<MapData, u64>,
    // pidfds: killing a recycled PID is impossible even when poisoning HEALTH itself fails.
    tasks: BTreeMap<u32, OwnedFd>,
    pid_namespace: [u64; 2],
    _pid_namespace: OwnedFd,
}

impl Controller {
    /// Loads and attaches every hook.
    ///
    /// Requires active BPF LSM, kernel BTF, ring buffers and privileges. A kernel lacking any of
    /// them gets no weaker enforcement. Success makes the tracer nondumpable, so capless traced
    /// processes cannot steal its BPF descriptors; failure leaves dumpability unchanged, so an
    /// `Auto` fallback stays usable.
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
            // Socket storage is a map type Aya has no wrapper for; the kernel owns it entirely.
            let mut programs = aya::EbpfLoader::new()
                .allow_unsupported_maps()
                .load(aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/marsh.bpf.o")))
                .map_err(|error| io::Error::other(format!("loading BPF LSM maps/CO-RE relocations failed (BPF privileges, ring buffers and socket storage required): {error}")))?;
            let mut namespace: Array<_, [u64; 2]> = Array::try_from(
                programs
                    .map_mut("PID_NAMESPACE")
                    .ok_or_else(|| io::Error::other("missing BPF map PID_NAMESPACE"))?,
            )
            .map_err(io::Error::other)?;
            namespace.set(0, pid_namespace, 0).map_err(io::Error::other)?;
            let controller = Self {
                task_root: map(&mut programs, "TASK_ROOT")?,
                answers: map(&mut programs, "ANSWERS")?,
                received: map(&mut programs, "RECEIVED")?,
                net4: map(&mut programs, "CLASS_NET4")?,
                net6: map(&mut programs, "CLASS_NET6")?,
                local: map(&mut programs, "LOCAL_ALLOWED")?,
                allowed: HashSet::new(),
                events: map(&mut programs, "EVENTS")?,
                health: map(&mut programs, "HEALTH")?,
                tasks: BTreeMap::new(),
                pid_namespace,
                _pid_namespace: pid_namespace_fd,
                programs: None,
            };
            for hook in HOOKS {
                let program: &mut Lsm = programs
                    .program_mut(hook)
                    .ok_or_else(|| io::Error::other(format!("missing BPF program {hook}")))?
                    .try_into()
                    .map_err(io::Error::other)?;
                program.load(hook, &btf).map_err(|error| {
                    io::Error::other(format!("loading BPF LSM {hook} failed: {error}"))
                })?;
                program.attach().map_err(|error| {
                    io::Error::other(format!("attaching BPF LSM {hook} failed: {error}"))
                })?;
            }
            for read in READS {
                let program: &mut FExit = programs
                    .program_mut(read)
                    .ok_or_else(|| io::Error::other(format!("missing BPF program {read}")))?
                    .try_into()
                    .map_err(io::Error::other)?;
                program.load(read, &btf).map_err(|error| {
                    io::Error::other(format!("loading BPF fexit {read} failed: {error}"))
                })?;
                program.attach().map_err(|error| {
                    io::Error::other(format!("attaching BPF fexit {read} failed: {error}"))
                })?;
            }
            // Last: no governed child exists yet, and a failed load must not leave an Auto
            // fallback's children nondumpable.
            // SAFETY: PR_SET_DUMPABLE takes scalar arguments only.
            if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
                let error = io::Error::last_os_error();
                return Err(io::Error::new(
                    error.kind(),
                    format!("protecting BPF descriptors with PR_SET_DUMPABLE failed: {error}"),
                ));
            }
            Ok(Self {
                programs: Some(programs),
                ..controller
            })
        }
    }

    /// Replaces this root's declared networks, preserving first-declaration precedence.
    ///
    /// Every trie key starts with the 64 root bits, so even `/0` cannot match another root. The
    /// root's answers are withdrawn; publish new ones afterwards.
    pub fn configure(&mut self, root: u64, networks: &[Network]) -> io::Result<()> {
        self.update(|this| {
            validate_networks(networks)?;
            absent_ok(this.answers.remove(&root))?;
            this.clear_networks(root)?;
            for network in effective_networks(networks) {
                let class = u32::from(network.class);
                let prefix = 64 + network.prefix;
                let inserted = if network.family == AF_INET {
                    this.net4
                        .insert(&Key::new(prefix, net4_key(root, &network.address)), class, 0)
                } else {
                    this.net6
                        .insert(&Key::new(prefix, net6_key(root, &network.address)), class, 0)
                };
                inserted.map_err(io::Error::other)?;
            }
            Ok(())
        })
    }

    /// Atomically replaces a root's answers.
    pub fn publish(&mut self, root: u64, answers: Answers) -> io::Result<()> {
        self.update(|this| {
            this.answers
                .insert(root, WireAnswers(answers), 0)
                .map_err(io::Error::other)
        })
    }

    /// Marks a root's answers stale, keeping the rest. A root without answers already refuses.
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

    /// Attributes every thread of a stopped process to a root before it resumes. A root without
    /// answers refuses its contacts. A child that changed PID namespace is refused rather than
    /// resumed unattributed.
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

    /// Removes an exited process's mapping; repeated cleanup is harmless.
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

    /// Removes a root's answers, receipts and networks. Processes still mapped to it refuse.
    pub fn remove_root(&mut self, root: u64) -> io::Result<()> {
        let result = (|| {
            absent_ok(self.answers.remove(&root))?;
            absent_ok(self.received.remove(&root))?;
            self.clear_networks(root)
        })();
        result.map_err(|error| self.poison(error))
    }

    /// Replaces the IP endpoints traced processes hold, which a taint-carrying root may reach
    /// under `untraced_local(Deny)`. Wildcard binds have a zero address and match every local
    /// address of their family and port. Unchanged entries stay in place throughout.
    pub fn local(&mut self, endpoints: &[Endpoint]) -> io::Result<()> {
        self.update(|this| {
            let wanted: HashSet<Endpoint> = endpoints.iter().map(Endpoint::normalized).collect();
            if let Some(invalid) = wanted
                .iter()
                .find(|endpoint| !matches!(endpoint.family, AF_INET | AF_INET6))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("local allowlist entry is not an IP endpoint: {invalid:?}"),
                ));
            }
            for endpoint in this.allowed.difference(&wanted) {
                absent_ok(this.local.remove(&WireEndpoint(*endpoint)))?;
            }
            for endpoint in wanted.difference(&this.allowed) {
                this.local
                    .insert(WireEndpoint(*endpoint), 1, 0)
                    .map_err(io::Error::other)?;
            }
            this.allowed = wanted;
            Ok(())
        })
    }

    /// Readable when events are waiting; poll it, then [`Self::drain`].
    pub fn events(&self) -> BorrowedFd<'_> {
        // SAFETY: the ring's descriptor lives exactly as long as `self`, which this borrows.
        unsafe { BorrowedFd::borrow_raw(self.events.as_raw_fd()) }
    }

    /// Drains every waiting event. Overflow, malformed records and map errors poison for good,
    /// auditing roots included.
    pub fn drain(&mut self) -> io::Result<Vec<Event>> {
        self.update(|this| {
            let mut events = Vec::new();
            while let Some(bytes) = this.events.next() {
                if bytes.len() != size_of::<Event>() {
                    return Err(io::Error::other(
                        "malformed BPF event record; enforcement poisoned",
                    ));
                }
                // SAFETY: bytes holds exactly one Event, which consists of integers and byte
                // arrays only; read_unaligned tolerates the ring's alignment.
                let mut event = unsafe { bytes.as_ptr().cast::<Event>().read_unaligned() };
                if event.kind > RECEIPT
                    || event.class as usize >= MAX_CLASSES
                    || event.decision > 2
                    || event.reason > UNANSWERED
                    || event.endpoint.unix_len > 108
                {
                    return Err(io::Error::other(
                        "invalid BPF event record; enforcement poisoned",
                    ));
                }
                event.endpoint.normalize();
                events.push(event);
            }
            // Loss racing the first check must not pass as a complete drain.
            this.check_health()?;
            Ok(events)
        })
    }

    /// Sends SIGKILL to every governed pidfd, cleans up even when poisoned, then detaches.
    ///
    /// Call on a shared-service failure. A signalling failure keeps the hooks attached; after
    /// successful signalling, map cleanup errors are returned, but stale numeric PID keys cannot
    /// match a reused PID because the programs are detached.
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

    fn check_health(&self) -> io::Result<()> {
        if self.health.get(&0, 0).map_err(io::Error::other)? != 0 {
            return Err(io::Error::other(
                "BPF reporting or map loss: privileged enforcement is permanently poisoned",
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
            // Without the switch, pidfds are the last fail-closed boundary. Never wait here:
            // ptrace exit stops need the tracer.
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
        // ponytail: scans the shared tries on replacement; track per-root keys if this gets hot.
        let prefix = root.to_ne_bytes();
        let keys: Vec<_> = self
            .net4
            .keys()
            .filter(|key| key.as_ref().map_or(true, |key| key.data()[..8] == prefix))
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for key in keys {
            self.net4.remove(&key).map_err(io::Error::other)?;
        }
        let keys: Vec<_> = self
            .net6
            .keys()
            .filter(|key| key.as_ref().map_or(true, |key| key.data()[..8] == prefix))
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for key in keys {
            self.net6.remove(&key).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

/// Trie keys begin with the root in native byte order, as the kernel program copies it.
fn net4_key(root: u64, address: &[u8; 16]) -> [u8; 12] {
    let mut key = [0; 12];
    key[..8].copy_from_slice(&root.to_ne_bytes());
    key[8..].copy_from_slice(&address[..4]);
    key
}

fn net6_key(root: u64, address: &[u8; 16]) -> [u8; 24] {
    let mut key = [0; 24];
    key[..8].copy_from_slice(&root.to_ne_bytes());
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
        Err(MapError::SyscallError(error)) if error.io_error.raw_os_error() == Some(libc::ENOENT) => {
            Ok(())
        }
        Err(error) => Err(io::Error::other(error)),
    }
}

fn pidfd(tgid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes an integer PID and flags; it returns a fresh close-on-exec
    // descriptor owned only here.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = i32::try_from(descriptor).map_err(io::Error::other)?;
    // SAFETY: the successful syscall returned a fresh owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(feature = "privileged")]
fn map<T: TryFrom<Map>>(programs: &mut Ebpf, name: &str) -> io::Result<T>
where
    T::Error: std::fmt::Display,
{
    let map = programs
        .take_map(name)
        .ok_or_else(|| io::Error::other(format!("missing BPF map {name}")))?;
    T::try_from(map).map_err(|error| io::Error::other(format!("BPF map {name}: {error}")))
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

#[cfg(all(test, feature = "privileged"))]
mod tests {
    use super::*;
    use marsh_peer_class::builtin_class;

    #[link(name = "marsh_classify", kind = "static")]
    unsafe extern "C" {
        fn marsh_host_builtin_class(endpoint: *const Endpoint) -> i32;
        fn marsh_host_normalize_ip(endpoint: *mut Endpoint);
    }

    /// The kernel's classifier is C; it must agree with the policy's Rust one on every IP.
    #[test]
    fn kernel_classifier_matches_policy_classifier() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let interesting: [[u8; 16]; 6] = [
            [0; 16],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0],
            [127, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
        ];
        for round in 0..20_000 {
            let mut endpoint = Endpoint {
                family: [AF_INET, AF_INET6, 1, 16, 0, 17][usize::try_from(next() % 6).unwrap()],
                port: 80,
                scope: u32::try_from(next() % 3).unwrap(),
                ..Endpoint::default()
            };
            endpoint.address = if round % 2 == 0 {
                interesting[usize::try_from(next() % 6).unwrap()]
            } else {
                let mut address = [0; 16];
                for byte in &mut address {
                    *byte = u8::try_from(next() % 4).unwrap() * 85;
                }
                address
            };
            // SAFETY: endpoint is a live, initialized Endpoint, the C struct's exact layout.
            let kernel = unsafe { marsh_host_builtin_class(&endpoint) };
            assert_eq!(
                builtin_class(&endpoint).map_or(-1, |class| i32::try_from(class).unwrap()),
                kernel,
                "{endpoint:?}"
            );
            if matches!(endpoint.family, AF_INET | AF_INET6) {
                let mut kernel = endpoint;
                // SAFETY: as above, uniquely borrowed for the call.
                unsafe { marsh_host_normalize_ip(&mut kernel) };
                assert_eq!(kernel, endpoint.normalized(), "{endpoint:?}");
            }
        }
    }
}
