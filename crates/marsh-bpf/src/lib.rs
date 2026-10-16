//! Privileged-only BPF LSM loader and the maps its programs read.
//!
//! Enable `privileged` to compile `src/marsh.bpf.c` with clang and embed it. Without that feature
//! this crate builds without BPF tooling and [`Controller::load`] returns an error.
//!
//! Publish a root's networks before its answers: [`Controller::configure`] withdraws old answers
//! while it replaces the trie. A domain is a cgroup v2 whose tasks are all governed as one root,
//! whatever their PID namespace: register it with [`Controller::domain`] before any task enters
//! the cgroup. Its id is the cgroup's kernfs id: the directory's inode number on 64-bit kernels,
//! and in general the id in its `name_to_handle_at` handle. Every map or reporting failure poisons
//! enforcement for good; create a new controller only after killing the old one's domains. Invalid
//! input fails only that call. Maps and links are never pinned, and kernel-created descriptors are
//! close-on-exec. Dropping the controller detaches its programs.
//!
//! Governed tasks may not use bpf(2), so the controller must run outside every domain. Event tgids
//! and tids are in the loader's PID namespace, and zero for a task in any other.

use std::{
    collections::HashSet,
    io,
    os::fd::{AsRawFd, BorrowedFd},
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
    UNTAGGED, effective_networks,
};

/// Every LSM hook the programs attach to; all are required.
#[cfg(feature = "privileged")]
const HOOKS: [&str; 26] = [
    "socket_connect",
    "socket_sendmsg",
    "unix_may_send",
    "socket_post_create",
    "unix_stream_connect",
    "sk_free_security",
    "uring_sqpoll",
    "uring_override_creds",
    "uring_cmd",
    "userns_create",
    "bpf",
    "bpf_map_create",
    "bpf_prog_load",
    "bpf_token_create",
    "perf_event_open",
    "task_prctl",
    "file_ioctl",
    "file_ioctl_compat",
    "task_setnice",
    "task_setioprio",
    "task_setscheduler",
    "task_movememory",
    "task_setpgid",
    "task_setrlimit",
    "key_alloc",
    "key_permission",
];

/// Socket reads, traced on return so a reader blocked before tainted bytes arrived is caught:
/// the protocol entries every read (each recvmmsg message included) passes, and splice.
#[cfg(feature = "privileged")]
const READS: [&str; 6] = [
    "inet_recvmsg",
    "inet6_recvmsg",
    "unix_stream_recvmsg",
    "unix_dgram_recvmsg",
    "unix_seqpacket_recvmsg",
    "sock_splice_read",
];

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
/// Share it behind the tracer's mutex. Loss is sticky: any failed map or reporting operation
/// refuses every governed task's contacts, auditing roots included. Tasks outside every domain
/// are never affected.
pub struct Controller {
    // Drop links before maps: Aya detaches the programs when `Ebpf` drops.
    programs: Option<Ebpf>,
    domains: HashMap<MapData, u64, u64>,
    answers: HashMap<MapData, u64, WireAnswers>,
    received: HashMap<MapData, u64, WireReceipts>,
    net4: LpmTrie<MapData, [u8; 12], u32>,
    net6: LpmTrie<MapData, [u8; 24], u32>,
    local: HashMap<MapData, WireEndpoint, u32>,
    allowed: HashSet<Endpoint>,
    events: RingBuf<MapData>,
    health: Array<MapData, u64>,
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
            let pid_namespace = own_pid_namespace()?;
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
                domains: map(&mut programs, "DOMAINS")?,
                answers: map(&mut programs, "ANSWERS")?,
                received: map(&mut programs, "RECEIVED")?,
                net4: map(&mut programs, "CLASS_NET4")?,
                net6: map(&mut programs, "CLASS_NET6")?,
                local: map(&mut programs, "LOCAL_ALLOWED")?,
                allowed: HashSet::new(),
                events: map(&mut programs, "EVENTS")?,
                health: map(&mut programs, "HEALTH")?,
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
        validate_networks(networks)?;
        self.update(|this| {
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

    /// Governs every task in a cgroup v2, `cgroup` being its kernfs id, as `root`. Register it
    /// before any task enters the cgroup; children inherit it, and only a privileged task can
    /// leave it. A root without answers refuses its contacts.
    pub fn domain(&mut self, cgroup: u64, root: u64) -> io::Result<()> {
        self.update(|this| {
            this.domains
                .insert(cgroup, root, 0)
                .map_err(io::Error::other)
        })
    }

    /// Stops governing a cgroup; repeated cleanup is harmless.
    pub fn remove_domain(&mut self, cgroup: u64) -> io::Result<()> {
        absent_ok(self.domains.remove(&cgroup)).map_err(|error| self.poison(error))
    }

    /// Removes a root's answers, receipts and networks. Domains still mapped to it refuse.
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
        self.update(|this| {
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
                    || event.reason > UNTAGGED
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

    /// Poisons enforcement, removes every domain even when poisoned, then detaches the programs.
    ///
    /// Call on a shared-service failure. It signals nothing: kill every domain through its
    /// cgroup's `cgroup.kill` first, since once this returns no domain is governed any more. Map
    /// errors are returned after detaching.
    pub fn terminate(&mut self) -> io::Result<()> {
        let poisoned = self.health.set(0, 1, 0).map_err(io::Error::other);
        let cleared = self.clear_domains();
        self.programs.take();
        cleared.and(poisoned)
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
            // Detaching would only lift what enforcement remains; killing is the caller's.
            return io::Error::other(format!(
                "{error}; cannot poison BPF maps: {map_error}; kill every domain"
            ));
        }
        error
    }

    fn clear_domains(&mut self) -> io::Result<()> {
        let cgroups: Vec<u64> = self
            .domains
            .keys()
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        for cgroup in cgroups {
            absent_ok(self.domains.remove(&cgroup))?;
        }
        Ok(())
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

/// The loader's own PID namespace, whose ids events report. `/proc/self` names the caller in any
/// procfs that can see it, so the namespace is right whenever it resolves.
#[cfg(feature = "privileged")]
fn own_pid_namespace() -> io::Result<[u64; 2]> {
    use std::os::unix::fs::MetadataExt as _;
    let own = std::fs::metadata("/proc/self/ns/pid")?;
    Ok([own.dev(), own.ino()])
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
