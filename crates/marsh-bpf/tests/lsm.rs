//! Run explicitly as root in a BPF-LSM-enabled VM, never against arbitrary host tasks, one test at
//! a time: each test moves its whole process between cgroups to govern itself.
#![cfg(feature = "privileged")]
#![allow(
    clippy::tests_outside_test_module,
    clippy::panic_in_result_fn,
    reason = "an integration test file is the test module, and its tests assert and propagate I/O"
)]

use std::{
    ffi::CStr,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            net::{SocketAddr, UnixDatagram, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::Command,
};

use marsh_bpf::Controller;
use marsh_peer_class::{
    AF_INET, ALLOW, AUDIT, Answers, CLASSIFIED, CONTACT, CONTINGENT, DENY, Endpoint, Event,
    InodeLabel, LOOPBACK, NETLINK, NameRule, Network, RECEIPT, RULE, STALE, UNANSWERED, UNIX,
    UNKNOWN, UNTRACED,
};

const CGROUPS: &str = "/sys/fs/cgroup";

/// The cgroup v2 directory a process is in.
fn cgroup_of(pid: u32) -> io::Result<PathBuf> {
    let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let path = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| io::Error::other("no cgroup v2 membership"))?;
    Ok(Path::new(CGROUPS).join(path.trim_start_matches('/')))
}

/// Moves every thread of a process into a cgroup.
fn move_to(cgroup: &Path, pid: u32) -> io::Result<()> {
    std::fs::write(cgroup.join("cgroup.procs"), pid.to_string())
}

/// This test's cgroups, `/sys/fs/cgroup/marsh-lsm-test-<pid>/<name>`. The test process starts in
/// the parent, which is no domain, and returns to its own cgroup on drop.
struct Cgroups {
    parent: PathBuf,
    home: PathBuf,
}

impl Cgroups {
    fn new() -> io::Result<Self> {
        let pid = std::process::id();
        let home = cgroup_of(pid)?;
        let parent = Path::new(CGROUPS).join(format!("marsh-lsm-test-{pid}"));
        std::fs::create_dir(&parent)?;
        let cgroups = Self { parent, home };
        cgroups.leave(pid)?;
        Ok(cgroups)
    }

    /// A new child cgroup, governed as `root`.
    fn domain(&self, controller: &mut Kernel<'_>, name: &str, root: u64) -> io::Result<Cgroup> {
        let path = self.parent.join(name);
        std::fs::create_dir(&path)?;
        // On 64-bit kernels a cgroup's id is its directory's inode number.
        let cgroup = Cgroup {
            id: std::fs::metadata(&path)?.ino(),
            path,
            parent: self.parent.clone(),
        };
        controller.domain(cgroup.id, root)?;
        Ok(cgroup)
    }

    /// Moves a process to the parent cgroup, where it is ungoverned.
    fn leave(&self, pid: u32) -> io::Result<()> {
        move_to(&self.parent, pid)
    }
}

impl Drop for Cgroups {
    fn drop(&mut self) {
        let _ = move_to(&self.home, std::process::id());
        let _ = std::fs::remove_dir(&self.parent);
    }
}

/// A domain cgroup, removed on drop.
struct Cgroup {
    path: PathBuf,
    parent: PathBuf,
    id: u64,
}

impl Cgroup {
    /// Moves a process here: it is governed as this domain's root.
    fn enter(&self, pid: u32) -> io::Result<()> {
        move_to(&self.path, pid)
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let pid = std::process::id();
        if cgroup_of(pid).is_ok_and(|current| current == self.path) {
            let _ = move_to(&self.parent, pid);
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// The controller, called from the parent cgroup only: governed tasks may not use bpf(2), and
/// this test process governs itself by entering domains.
struct Kernel<'a> {
    controller: Controller,
    cgroups: &'a Cgroups,
}

impl<'a> Kernel<'a> {
    fn load(cgroups: &'a Cgroups) -> io::Result<Self> {
        Ok(Self {
            controller: Controller::load()?,
            cgroups,
        })
    }

    fn call<T>(&mut self, action: impl FnOnce(&mut Controller) -> io::Result<T>) -> io::Result<T> {
        let pid = std::process::id();
        let here = cgroup_of(pid)?;
        self.cgroups.leave(pid)?;
        let result = action(&mut self.controller);
        move_to(&here, pid)?;
        result
    }

    fn configure(&mut self, root: u64, networks: &[Network]) -> io::Result<()> {
        self.call(|controller| controller.configure(root, networks))
    }

    fn publish(&mut self, root: u64, answers: Answers) -> io::Result<()> {
        self.call(|controller| controller.publish(root, answers))
    }

    fn stale(&mut self, root: u64) -> io::Result<()> {
        self.call(|controller| controller.stale(root))
    }

    fn domain(&mut self, cgroup: u64, root: u64) -> io::Result<()> {
        self.call(|controller| controller.domain(cgroup, root))
    }

    fn remove_domain(&mut self, cgroup: u64) -> io::Result<()> {
        self.call(|controller| controller.remove_domain(cgroup))
    }

    fn remove_root(&mut self, root: u64) -> io::Result<()> {
        self.call(|controller| controller.remove_root(root))
    }

    fn local(&mut self, endpoints: &[Endpoint]) -> io::Result<()> {
        self.call(|controller| controller.local(endpoints))
    }

    fn drain(&mut self) -> io::Result<Vec<Event>> {
        self.call(Controller::drain)
    }
}

/// Runs `action` in a forked child and returns its exit status, -1 when it did not exit. The
/// action must be async-signal-safe: the test process has other threads.
fn in_child(action: impl FnOnce() -> i32) -> io::Result<i32> {
    // SAFETY: the child runs only the async-signal-safe action, then _exit.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let status = action();
        // SAFETY: _exit ends the forked child without running the parent's destructors.
        unsafe { libc::_exit(status) };
    }
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status = 0;
    // SAFETY: waits for the child forked above; status is a live out-parameter.
    if unsafe { libc::waitpid(child, &raw mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    })
}

/// A system call's errno, 0 when it succeeded.
fn errno(result: impl Into<i64>) -> i32 {
    if result.into() >= 0 {
        0
    } else {
        io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    }
}

/// A new pty's master and its terminal end, which is not this process's controlling terminal.
fn pty() -> io::Result<(OwnedFd, std::fs::File)> {
    // SAFETY: posix_openpt takes flags only and returns a fresh descriptor.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: posix_openpt returned this fresh descriptor, owned nowhere else.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: grantpt takes the live master only.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: unlockpt takes the live master only.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0; 64];
    // SAFETY: ptsname_r writes at most name.len() bytes, NUL-terminated on success.
    let error = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: ptsname_r succeeded, so name holds a NUL-terminated path.
    let path = unsafe { CStr::from_ptr(name.as_ptr()) };
    let terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path.to_str().map_err(io::Error::other)?)?;
    Ok((master, terminal))
}

fn refused(result: io::Result<impl Sized>) -> bool {
    result
        .err()
        .is_some_and(|error| error.raw_os_error() == Some(libc::EPERM))
}

fn decisions(events: &[Event]) -> Vec<(u32, u32)> {
    events
        .iter()
        .map(|event| (event.decision, event.reason))
        .collect()
}

/// `sendto` with an explicit name of any sockaddr type.
fn send_named<T>(socket: RawFd, bytes: &[u8], name: &T) -> io::Result<usize> {
    // SAFETY: bytes and name are initialized and outlive this synchronous call; the length is
    // exactly the size of T.
    let sent = unsafe {
        libc::sendto(
            socket,
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
            std::ptr::from_ref(name).cast(),
            libc::socklen_t::try_from(size_of::<T>()).map_err(io::Error::other)?,
        )
    };
    usize::try_from(sent).map_err(|_| io::Error::last_os_error())
}

/// A `sockaddr_in` for an IPv4 address and port.
fn ipv4(address: [u8; 4], port: u16) -> io::Result<libc::sockaddr_in> {
    // SAFETY: the zeroed sockaddr_in is an integer-only C struct.
    let mut name: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    name.sin_family = u16::try_from(libc::AF_INET).map_err(io::Error::other)?;
    name.sin_port = port.to_be();
    name.sin_addr.s_addr = u32::from_ne_bytes(address);
    Ok(name)
}

/// Sends on connected sockets go to their peer whatever name the call carries, so the kernel must
/// judge the peer: TCP ignores the name and UNIX seqpacket drops it.
#[test]
#[ignore = "requires root, active BPF LSM, BTF, and loopback IPv4 in a disposable VM"]
fn connected_sends_are_judged_by_their_peer() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let tgid = std::process::id();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let stream = TcpStream::connect(listener.local_addr()?)?;
    let (_server, _) = listener.accept()?;
    let mut pair = [0; 2];
    // SAFETY: pair has room for exactly the two descriptors socketpair writes.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned these two fresh descriptors, owned nowhere else.
    let seqpacket = unsafe { OwnedFd::from_raw_fd(pair[0]) };
    // SAFETY: as above, for the other end.
    let _seqpacket_peer = unsafe { OwnedFd::from_raw_fd(pair[1]) };
    controller.publish(
        1,
        Answers {
            deny: (1 << LOOPBACK) | (1 << UNIX),
            guarded: (1 << LOOPBACK) | (1 << UNIX),
            ..Answers::default()
        },
    )?;
    let one = cgroups.domain(&mut controller, "one", 1)?;
    one.enter(tgid)?;
    // SAFETY: the zeroed sockaddr_nl is an integer-only C struct.
    let mut netlink: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    netlink.nl_family = u16::try_from(libc::AF_NETLINK).map_err(io::Error::other)?;
    assert!(refused(send_named(stream.as_raw_fd(), b"to loopback", &netlink)));
    let external = ipv4([198, 51, 100, 1], 9)?;
    assert!(refused(send_named(seqpacket.as_raw_fd(), b"to peer", &external)));
    let events = controller.drain()?;
    assert_eq!(
        events
            .iter()
            .map(|event| (event.class, event.decision))
            .collect::<Vec<_>>(),
        [(LOOPBACK, DENY), (UNIX, DENY)]
    );
    assert_eq!(
        events[0].endpoint,
        Endpoint::from(listener.local_addr()?).normalized()
    );
    cgroups.leave(tgid)?;
    controller.remove_domain(one.id)?;
    Ok(())
}

#[test]
#[ignore = "requires root, active BPF LSM, BTF, and loopback IPv4/IPv6 in a disposable VM"]
#[expect(
    clippy::too_many_lines,
    reason = "every decision and audit outcome is checked against one loaded program set in turn"
)]
fn contacts_are_judged_against_the_kernels_address() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    let destination = receiver.local_addr()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let listener6 = TcpListener::bind("[::1]:0")?;
    let tgid = std::process::id();
    // Created before this process is governed: no traced process created it.
    let (untraced, _untraced_peer) = UnixDatagram::pair()?;
    let mut answers = Answers {
        generation: 7,
        deny: 1 << LOOPBACK,
        guarded: 1 << LOOPBACK,
        ..Answers::default()
    };
    controller.publish(1, answers)?;
    // A populated denial table must not affect an ungoverned task.
    sender.send_to(b"unmapped", destination)?;
    assert!(controller.drain()?.is_empty());
    let one = cgroups.domain(&mut controller, "one", 1)?;
    one.enter(tgid)?;
    assert!(refused(sender.send_to(b"denied", destination)));
    assert!(refused(TcpStream::connect(listener.local_addr()?)));
    let events = controller.drain()?;
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.kind == CONTACT
        && event.tgid == tgid
        && event.root == 1
        && event.generation == 7
        && event.class == LOOPBACK
        && (event.decision, event.reason) == (DENY, RULE)));

    answers.audit = 1;
    controller.publish(1, answers)?;
    sender.send_to(b"audit", destination)?;
    controller.stale(1)?;
    sender.send_to(b"stale audit", destination)?;
    answers = Answers {
        stale: 1,
        deny: 0,
        audit: 0,
        ..answers
    };
    controller.publish(1, answers)?;
    assert!(refused(sender.send_to(b"stale deny", destination)));
    assert_eq!(
        decisions(&controller.drain()?),
        [(AUDIT, RULE), (AUDIT, STALE), (DENY, STALE)]
    );

    answers.stale = 0;
    controller.publish(1, answers)?;
    // Root still has its real capabilities: refusal must come from uring_sqpoll, not child
    // capability stripping or a network-class denial.
    let mut params = [0u32; 30];
    params[2] = 2; // io_uring_params.flags = IORING_SETUP_SQPOLL.
    // SAFETY: params is the initialized, aligned 120-byte io_uring_params ABI.
    let setup = unsafe { libc::syscall(libc::SYS_io_uring_setup, 8u32, params.as_mut_ptr()) };
    let setup_error = io::Error::last_os_error();
    if setup >= 0 {
        let descriptor = i32::try_from(setup).map_err(io::Error::other)?;
        // SAFETY: successful io_uring_setup returned this fresh owned descriptor.
        drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
    }
    assert_eq!(setup, -1);
    assert_eq!(setup_error.raw_os_error(), Some(libc::EPERM));
    assert!(controller.drain()?.is_empty());
    let mut stream = TcpStream::connect(listener.local_addr()?)?;
    stream.write_all(b"connected v4")?;
    let mut stream6 = TcpStream::connect(listener6.local_addr()?)?;
    stream6.write_all(b"connected v6")?;
    let events = controller.drain()?;
    assert_eq!(events.len(), 4);
    assert!(
        events
            .iter()
            .all(|event| event.class == LOOPBACK && event.decision == ALLOW)
    );
    assert_eq!(
        events[1].endpoint,
        Endpoint::from(listener.local_addr()?).normalized()
    );
    assert_eq!(
        events[3].endpoint,
        Endpoint::from(listener6.local_addr()?).normalized()
    );

    // A taint-carrying root may reach only IP endpoints traced processes hold.
    answers.untraced = (1 << LOOPBACK) | (1 << UNIX);
    answers.carried = 1;
    controller.publish(1, answers)?;
    let mut wildcard = Endpoint::from(destination);
    wildcard.address = [0; 16];
    controller.local(&[wildcard])?;
    sender.send_to(b"wildcard listener", destination)?;
    controller.local(&[])?;
    assert!(refused(sender.send_to(b"untraced local", destination)));
    assert_eq!(
        decisions(&controller.drain()?),
        [(ALLOW, RULE), (DENY, UNTRACED)]
    );

    // A UNIX socket a governed process created is traced; one created before is not.
    let (traced, _traced_peer) = UnixDatagram::pair()?;
    assert!(refused(untraced.send(b"untraced socketpair")));
    traced.send(b"traced socketpair")?;
    let mut stream_pair = UnixStream::pair()?;
    stream_pair.0.write_all(b"traced stream")?;
    answers.carried = 0;
    controller.publish(1, answers)?;
    untraced.send(b"no taint, no restriction")?;
    let events = controller.drain()?;
    assert_eq!(
        decisions(&events),
        [(DENY, UNTRACED), (ALLOW, RULE), (ALLOW, RULE), (ALLOW, RULE)]
    );
    assert!(
        events
            .iter()
            .all(|event| event.class == UNIX && event.endpoint.unix_len == 0)
    );
    // Reading back what this root sent while carrying class 0 is a receipt of class 0, which
    // the kernel counts until published answers acknowledge it.
    let mut sink = [0; 64];
    let read = stream_pair.1.read(&mut sink)?;
    assert_ne!(read, 0);
    let events = controller.drain()?;
    assert_eq!(
        events
            .iter()
            .map(|event| (event.kind, event.taint))
            .collect::<Vec<_>>(),
        [(RECEIPT, 1)]
    );
    answers.acknowledged[0] = 1;

    let name = format!("marsh-bpf-{tgid}").into_bytes();
    let mut longer = name.clone();
    longer.push(0);
    let address = SocketAddr::from_abstract_name(&name)?;
    let other = SocketAddr::from_abstract_name(&longer)?;
    let _unix_receiver = UnixDatagram::bind_addr(&address)?;
    let _other_receiver = UnixDatagram::bind_addr(&other)?;
    let unix_sender = UnixDatagram::unbound()?;
    answers.deny = 1 << UNIX;
    controller.publish(1, answers)?;
    assert!(refused(unix_sender.send_to_addr(b"denied abstract", &address)));
    answers.deny = 0;
    controller.publish(1, answers)?;
    unix_sender.send_to_addr(b"exact abstract", &address)?;
    unix_sender.send_to_addr(b"other abstract", &other)?;
    let events = controller.drain()?;
    let mut endpoint = Endpoint {
        family: marsh_peer_class::AF_UNIX,
        unix_len: u32::try_from(name.len() + 1).map_err(io::Error::other)?,
        ..Endpoint::default()
    };
    endpoint.unix[1..=name.len()].copy_from_slice(&name);
    assert_eq!(decisions(&events), [(DENY, RULE), (ALLOW, RULE), (ALLOW, RULE)]);
    assert_eq!(events[1].endpoint, endpoint);
    assert_ne!(events[2].endpoint, endpoint);

    // A readable unsupported family is the ordinary UNKNOWN class. Its protocol rejects the
    // address only after LSM, so allow and audit still return EAFNOSUPPORT.
    let mut unknown = [0u8; 20];
    unknown[..2].copy_from_slice(&u16::try_from(libc::AF_PACKET).map_err(io::Error::other)?.to_ne_bytes());
    let unknown_contact = || {
        // SAFETY: unknown is initialized and lives across the synchronous connect.
        let result = unsafe {
            libc::connect(
                sender.as_raw_fd(),
                unknown.as_ptr().cast(),
                libc::socklen_t::try_from(unknown.len()).unwrap_or_default(),
            )
        };
        if result == 0 {
            None
        } else {
            io::Error::last_os_error().raw_os_error()
        }
    };
    assert_eq!(unknown_contact(), Some(libc::EAFNOSUPPORT));
    answers.deny = 1 << UNKNOWN;
    controller.publish(1, answers)?;
    assert_eq!(unknown_contact(), Some(libc::EPERM));
    let events = controller.drain()?;
    assert_eq!(decisions(&events), [(ALLOW, RULE), (DENY, RULE)]);
    assert!(events.iter().all(|event| event.class == UNKNOWN));
    answers.deny = 0;
    controller.publish(1, answers)?;
    // AF_UNSPEC is a disconnect: it always passes and creates no contact.
    let unspecified = libc::sockaddr {
        sa_family: u16::try_from(libc::AF_UNSPEC).map_err(io::Error::other)?,
        sa_data: [0; 14],
    };
    // SAFETY: unspecified is a complete initialized sockaddr; AF_UNSPEC uses only sa_family.
    let disconnected = unsafe {
        libc::connect(
            sender.as_raw_fd(),
            std::ptr::from_ref(&unspecified),
            libc::socklen_t::try_from(size_of::<libc::sa_family_t>()).unwrap_or_default(),
        )
    };
    assert_eq!(disconnected, 0);
    assert!(controller.drain()?.is_empty());

    // NETLINK's class needs only its family, including the connected-send path.
    // SAFETY: socket has no pointer arguments and returns a fresh owned descriptor.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_USERSOCK,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is the fresh descriptor returned above, owned nowhere else.
    let netlink = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: the zeroed sockaddr_nl is an integer-only C struct.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = u16::try_from(libc::AF_NETLINK).map_err(io::Error::other)?;
    // SAFETY: address points to a complete initialized sockaddr_nl.
    let connected = unsafe {
        libc::connect(
            netlink.as_raw_fd(),
            std::ptr::from_ref(&address).cast(),
            libc::socklen_t::try_from(size_of::<libc::sockaddr_nl>()).unwrap_or_default(),
        )
    };
    if connected != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the buffer is initialized and lives across this synchronous send.
    let _ = unsafe { libc::send(netlink.as_raw_fd(), b"probe".as_ptr().cast(), 5, 0) };
    let events = controller.drain()?;
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|event| event.class == NETLINK && event.decision == ALLOW)
    );

    // Declared networks are root-isolated.
    let network = Network {
        family: AF_INET,
        prefix: 24,
        class: 5,
        address: [198, 51, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    controller.configure(1, &[network])?;
    controller.publish(
        1,
        Answers {
            deny: 1 << 5,
            acknowledged: answers.acknowledged,
            ..Answers::default()
        },
    )?;
    assert!(refused(sender.send_to(b"root one", "198.51.100.1:9")));
    assert_eq!(controller.drain()?[0].class, 5);
    controller.configure(2, &[Network { class: 6, ..network }])?;
    controller.publish(
        2,
        Answers {
            deny: 1 << 6,
            ..Answers::default()
        },
    )?;
    let two = cgroups.domain(&mut controller, "two", 2)?;
    two.enter(tgid)?;
    assert!(refused(sender.send_to(b"root two", "198.51.100.1:9")));
    assert_eq!(controller.drain()?[0].class, 6);
    controller.remove_root(2)?;
    assert!(refused(sender.send_to(b"missing answers", destination)));
    assert_eq!(decisions(&controller.drain()?), [(DENY, UNANSWERED)]);
    // Its cgroup no longer a domain, the process is ungoverned where it stands.
    controller.remove_domain(two.id)?;
    controller.remove_domain(two.id)?;
    sender.send_to(b"unmapped again", destination)?;

    // Overflow must poison enforcement and reporting, even for an auditing root.
    one.enter(tgid)?;
    controller.publish(
        1,
        Answers {
            audit: 1,
            ..Answers::default()
        },
    )?;
    let overflowed = (0..30_000).any(|_| refused(sender.send_to(b"fill ring", destination)));
    assert!(overflowed);
    assert!(controller.drain().is_err());
    assert!(refused(sender.send_to(b"sticky poison", destination)));
    controller.remove_domain(one.id)?;
    controller.remove_root(1)?;
    sender.send_to(b"unmapped despite poison", destination)?;
    drop(controller);
    sender.send_to(b"drop detached", destination)?;
    Ok(())
}

/// Taint a carrying root sends into a local socket reaches whichever root reads it, before the
/// reader's next contact: the kernel switches the reader to its contingent answers at once.
#[test]
#[ignore = "requires root, active BPF LSM, BTF, and loopback IPv4 in a disposable VM"]
#[expect(
    clippy::too_many_lines,
    reason = "each transport's receipt is checked in turn against the same two governed roots"
)]
fn local_receipts_taint_the_reader_in_the_kernel() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let one = cgroups.domain(&mut controller, "one", 1)?;
    let two = cgroups.domain(&mut controller, "two", 2)?;
    let tgid = std::process::id();
    let outside = UdpSocket::bind("127.0.0.1:0")?;
    let mut buffer = [0; 64];
    let mut acknowledged = [0; 16];
    // Root 1 carries `carried`; root 2 refuses loopback once it receives any class it lacks.
    let stage = |controller: &mut Kernel<'_>, carried: u64, acknowledged: [u32; 16]| {
        controller.publish(
            1,
            Answers {
                carried,
                ..Answers::default()
            },
        )?;
        controller.publish(
            2,
            Answers {
                guarded: 1 << LOOPBACK,
                contingent: [1 << LOOPBACK; 16],
                acknowledged,
                ..Answers::default()
            },
        )?;
        controller.drain().map(drop)
    };
    let receipts = |events: &[Event]| -> Vec<(u64, u64, u64)> {
        events
            .iter()
            .filter(|event| event.kind == RECEIPT)
            .map(|event| (event.root, event.taint, event.sender))
            .collect()
    };

    // A datagram tags its receiver's whole port; a second read is no news.
    stage(&mut controller, 1 << 3, acknowledged)?;
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    let udp_sender = UdpSocket::bind("127.0.0.1:0")?;
    one.enter(tgid)?;
    udp_sender.send_to(b"secret", receiver.local_addr()?)?;
    udp_sender.send_to(b"again", receiver.local_addr()?)?;
    two.enter(tgid)?;
    controller.drain()?;
    receiver.recv(&mut buffer)?;
    receiver.recv(&mut buffer)?;
    assert!(refused(outside.send_to(b"after", outside.local_addr()?)));
    let events = controller.drain()?;
    assert_eq!(receipts(&events), [(2, 1 << 3, 1)]);
    assert_eq!(decisions(&events[1..]), [(DENY, CONTINGENT)]);
    // Answers that fold the receipt in replace the contingent ones.
    acknowledged[3] = 1;
    controller.publish(
        2,
        Answers {
            guarded: 1 << LOOPBACK,
            carried: 1 << 3,
            acknowledged,
            ..Answers::default()
        },
    )?;
    outside.send_to(b"folded", outside.local_addr()?)?;
    assert_eq!(decisions(&controller.drain()?), [(ALLOW, RULE)]);

    // A TCP connection is tagged by its peer: another connection to the same port is not.
    stage(&mut controller, 1 << 4, acknowledged)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    two.enter(tgid)?;
    let mut quiet = TcpStream::connect(listener.local_addr()?)?;
    let (mut quiet_server, _) = listener.accept()?;
    one.enter(tgid)?;
    let mut loud = TcpStream::connect(listener.local_addr()?)?;
    let (mut loud_server, _) = listener.accept()?;
    loud.write_all(b"secret")?;
    two.enter(tgid)?;
    quiet.write_all(b"plain")?;
    controller.drain()?;
    quiet_server.read_exact(&mut buffer[..5])?;
    assert!(controller.drain()?.is_empty());
    loud_server.read_exact(&mut buffer[..6])?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 4, 1)]);
    acknowledged[4] = 1;

    // A UNIX stream tags its connected peer; read() is a socket read too.
    stage(&mut controller, 1 << 5, acknowledged)?;
    one.enter(tgid)?;
    let (mut unix_sender, mut unix_receiver) = UnixStream::pair()?;
    unix_sender.write_all(b"secret")?;
    two.enter(tgid)?;
    controller.drain()?;
    unix_receiver.read_exact(&mut buffer[..6])?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 5, 1)]);
    acknowledged[5] = 1;

    // A reader already blocked when the tainted bytes arrive is tainted as its read returns.
    stage(&mut controller, 1 << 8, acknowledged)?;
    one.enter(tgid)?;
    let (mut writer, reader) = UnixStream::pair()?;
    let mut child = std::process::Command::new("head")
        .args(["-c", "6"])
        .stdin(std::process::Stdio::from(OwnedFd::from(reader)))
        .stdout(std::process::Stdio::null())
        .spawn()?;
    // Spawned into this process's domain; the reader belongs to another.
    two.enter(child.id())?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    controller.drain()?;
    writer.write_all(b"secret")?;
    assert!(child.wait()?.success());
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 8, 1)]);
    acknowledged[8] = 1;

    // Every message of one recvmmsg is a read: a tainted datagram arriving after a clean one
    // taints the reader, though recvmmsg reads it without passing sock_recvmsg.
    stage(&mut controller, 1 << 9, acknowledged)?;
    cgroups.leave(tgid)?;
    let batch = UdpSocket::bind("127.0.0.1:0")?;
    // SAFETY: the child runs only the async-signal-safe recvmmsg and _exit on its own stack.
    let reader = unsafe { libc::fork() };
    if reader == 0 {
        let mut first = [0u8; 16];
        let mut second = [0u8; 16];
        let mut vectors = [
            libc::iovec {
                iov_base: first.as_mut_ptr().cast(),
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_mut_ptr().cast(),
                iov_len: second.len(),
            },
        ];
        // SAFETY: zeroed mmsghdrs are valid empty headers.
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        for (header, vector) in headers.iter_mut().zip(vectors.iter_mut()) {
            header.msg_hdr.msg_iov = vector;
            header.msg_hdr.msg_iovlen = 1;
        }
        // SAFETY: each header names one live iovec over a live buffer; recvmmsg blocks for
        // both messages and returns before they go out of scope.
        let count = unsafe {
            libc::recvmmsg(
                batch.as_raw_fd(),
                headers.as_mut_ptr(),
                2,
                0,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: _exit ends the forked child without running the parent's destructors.
        unsafe { libc::_exit(i32::from(count != 2)) };
    }
    let reader = u32::try_from(reader).map_err(|_| io::Error::last_os_error())?;
    two.enter(reader)?;
    // Ungoverned, so untagged; the reader takes it before the tainted one exists.
    udp_sender.send_to(b"clean", batch.local_addr()?)?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    one.enter(tgid)?;
    udp_sender.send_to(b"secret", batch.local_addr()?)?;
    let mut status = 0;
    // SAFETY: waits for the forked child above; status is a live out-parameter.
    if unsafe { libc::waitpid(reader.cast_signed(), &raw mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 9, 1)]);
    acknowledged[9] = 1;

    // A port's datagram tag outlives a socket that shared the port at another address.
    stage(&mut controller, 1 << 10, acknowledged)?;
    cgroups.leave(tgid)?;
    let shared = UdpSocket::bind("127.0.0.2:0")?;
    let neighbour = UdpSocket::bind(("127.0.0.1", shared.local_addr()?.port()))?;
    one.enter(tgid)?;
    udp_sender.send_to(b"secret", shared.local_addr()?)?;
    drop(neighbour);
    // UDP sockets are freed after an RCU grace period.
    std::thread::sleep(std::time::Duration::from_secs(1));
    two.enter(tgid)?;
    controller.drain()?;
    shared.recv(&mut buffer)?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 10, 1)]);
    acknowledged[10] = 1;

    // Two classes at once are undecidable until userspace republishes.
    stage(&mut controller, (1 << 6) | (1 << 7), acknowledged)?;
    one.enter(tgid)?;
    let (datagram_sender, datagram_receiver) = UnixDatagram::pair()?;
    datagram_sender.send(b"secrets")?;
    two.enter(tgid)?;
    controller.drain()?;
    datagram_receiver.recv(&mut buffer)?;
    assert!(refused(outside.send_to(b"undecidable", outside.local_addr()?)));
    let events = controller.drain()?;
    assert_eq!(receipts(&events), [(2, (1 << 6) | (1 << 7), 1)]);
    assert_eq!(decisions(&events[1..]), [(DENY, STALE)]);
    cgroups.leave(tgid)?;
    Ok(())
}

/// A child in another PID namespace stays in its parent's cgroup, so it stays governed: its contact
/// is judged, though its ids, invisible in the loader's PID namespace, are reported as zero.
#[test]
#[ignore = "requires root, active BPF LSM, BTF, loopback IPv4 and PID namespaces in a disposable VM"]
fn pid_namespace_child_stays_governed() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let tgid = std::process::id();
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    let destination = ipv4([127, 0, 0, 1], receiver.local_addr()?.port())?;
    controller.publish(
        77,
        Answers {
            deny: 1 << LOOPBACK,
            guarded: 1 << LOOPBACK,
            ..Answers::default()
        },
    )?;
    let one = cgroups.domain(&mut controller, "one", 77)?;
    one.enter(tgid)?;
    let socket = sender.as_raw_fd();
    let status = in_child(|| {
        // SAFETY: unshare takes flags only and changes only this single-threaded child.
        if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
            return 100;
        }
        // The new namespace's init: PID 1 there, absent from the loader's namespace.
        in_child(|| i32::from(!refused(send_named(socket, b"namespaced", &destination))))
            .unwrap_or(101)
    })?;
    cgroups.leave(tgid)?;
    assert_eq!(status, 0);
    let events = controller.drain()?;
    assert_eq!(decisions(&events), [(DENY, RULE)]);
    assert_eq!((events[0].root, events[0].tgid, events[0].tid), (77, 0, 0));
    controller.remove_domain(one.id)?;
    Ok(())
}

const KEY_SPEC_SESSION_KEYRING: libc::c_long = -3;
const KEY_SPEC_USER_KEYRING: libc::c_long = -4;
const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
const KEYCTL_UNLINK: libc::c_long = 9;

/// Links a new user key into `keyring`, then unlinks it so that no run leaves one behind.
fn add_key(keyring: libc::c_long) -> i32 {
    // SAFETY: the pointers are NUL-terminated literals; the payload is their first byte.
    let serial = unsafe {
        libc::syscall(
            libc::SYS_add_key,
            c"user".as_ptr(),
            c"marsh-lsm-test".as_ptr(),
            c"x".as_ptr(),
            1_usize,
            keyring,
        )
    };
    if serial >= 0 {
        // SAFETY: KEYCTL_UNLINK takes integers only.
        unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_UNLINK, serial, keyring) };
    }
    errno(serial)
}

/// Each escape's errno, 0 when it succeeded: a user namespace, terminal injection, a ptrace
/// exception, `other`'s priority, bpf(2), and a key linked into the user keyring.
fn escape_attempts(terminal: RawFd, other: u32) -> io::Result<[i32; 6]> {
    let none: libc::c_ulong = 0;
    let attr = [0_u64; 9];
    Ok([
        // SAFETY: unshare takes flags only and changes only the forked child.
        in_child(|| errno(unsafe { libc::unshare(libc::CLONE_NEWUSER) }))?,
        // SAFETY: TIOCSTI reads one byte of the NUL-terminated literal.
        errno(unsafe { libc::ioctl(terminal, libc::TIOCSTI, c"x".as_ptr()) }),
        // SAFETY: PR_SET_PTRACER 0 clears this process's exception; it takes no pointers.
        errno(unsafe { libc::prctl(libc::PR_SET_PTRACER, none, none, none, none) }),
        // SAFETY: setpriority takes integers only.
        errno(unsafe { libc::setpriority(libc::PRIO_PROCESS, other, 5) }),
        // SAFETY: BPF_MAP_CREATE (0) only reads the zeroed attr, which outlives the call.
        errno(unsafe { libc::syscall(libc::SYS_bpf, 0, attr.as_ptr(), size_of_val(&attr)) }),
        add_key(KEY_SPEC_USER_KEYRING),
    ])
}

/// A governed task cannot leave governance or act on another domain's processes, while outside
/// every domain the same operations are not refused: a user namespace, terminal injection, a
/// ptrace exception, another cgroup's process priority, bpf(2), and a write to the user keyring.
#[test]
#[ignore = "requires root, active BPF LSM, BTF, keyrings, ptys and user namespaces in a disposable VM"]
fn escapes_are_refused_in_a_domain() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let tgid = std::process::id();
    let one = cgroups.domain(&mut controller, "one", 1)?;
    let (_master, terminal) = pty()?;
    // In the parent cgroup with this process, which is no domain.
    let mut other = Command::new("sleep").arg("60").spawn()?;
    let outside = escape_attempts(terminal.as_raw_fd(), other.id())?;
    one.enter(tgid)?;
    let inside = escape_attempts(terminal.as_raw_fd(), other.id())?;
    // What is the domain's own stays usable: its processes' attributes and the keys it allocated.
    one.enter(other.id())?;
    // SAFETY: setpriority takes integers only.
    let own_process = errno(unsafe { libc::setpriority(libc::PRIO_PROCESS, other.id(), 6) });
    // SAFETY: a null name asks for a new anonymous session keyring; no other argument is read.
    let joined = errno(unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_JOIN_SESSION_KEYRING,
            std::ptr::null::<libc::c_char>(),
        )
    });
    let own_key = add_key(KEY_SPEC_SESSION_KEYRING);
    cgroups.leave(tgid)?;
    other.kill()?;
    other.wait()?;
    assert!(outside.iter().all(|&error| error != libc::EPERM), "{outside:?}");
    assert_eq!(inside, [libc::EPERM; 6]);
    assert_eq!((own_process, joined, own_key), (0, 0, 0));
    controller.remove_domain(one.id)?;
    Ok(())
}

/// Drops every unused dentry and inode, so the next lookup loads them again.
fn evict() -> io::Result<()> {
    std::fs::write("/proc/sys/vm/drop_caches", "2")
}

/// What a governed read of each path received, by class bits: a classified file's classes.
fn read_receipts(
    controller: &mut Kernel<'_>,
    domain: &Cgroup,
    paths: &[&Path],
) -> io::Result<Vec<u64>> {
    let tgid = std::process::id();
    let mut received = Vec::new();
    for path in paths {
        controller.drain()?;
        domain.enter(tgid)?;
        let read = std::fs::read(path);
        controller.cgroups.leave(tgid)?;
        read?;
        let events = controller.drain()?;
        assert!(
            events
                .iter()
                .all(|event| event.kind == RECEIPT && event.sender == CLASSIFIED),
            "{events:?}"
        );
        received.push(events.iter().fold(0, |taint, event| taint | event.taint));
    }
    Ok(received)
}

/// Reading a classified file is a receipt of its classes before the bytes arrive: whether the
/// entry was loaded before classification, created after it, inherited its class from a
/// directory, or lost its classifying name, and after the kernel evicted and reloaded it.
#[test]
#[ignore = "requires root, active BPF LSM, BTF and an evictable filesystem in a disposable VM"]
fn classified_reads_are_receipts() -> io::Result<()> {
    let cgroups = Cgroups::new()?;
    let mut controller = Kernel::load(&cgroups)?;
    let one = cgroups.domain(&mut controller, "one", 1)?;
    // tmpfs never evicts; the VM's target directory is on 9p.
    let directory =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("lsm-classify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(directory.join("keys"))?;
    // Present and loaded before any classification exists.
    std::fs::write(directory.join(".env"), "TOKEN=1")?;
    std::fs::write(directory.join("keys/id"), "key")?;
    std::fs::write(directory.join("README"), "public")?;
    let rule = |glob: &str| NameRule::new(glob, 1).ok_or_else(|| io::Error::other("bad glob"));
    let rules = [
        NameRule {
            classes: 1 << 3,
            ..rule(".env")?
        },
        NameRule {
            subtree: 1 << 4,
            ..rule("keys")?
        },
    ];
    let anchor = std::fs::File::open(&directory)?;
    controller.call(|controller| {
        controller.name_rules(&rules)?;
        controller.label(
            std::os::fd::AsFd::as_fd(&anchor),
            InodeLabel {
                direct: 1,
                ..InodeLabel::default()
            },
        )
    })?;
    evict()?;
    let paths = [".env", "keys/id", "README"].map(|name| directory.join(name));
    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    assert_eq!(read_receipts(&mut controller, &one, &paths)?, [1 << 3, 1 << 4, 0]);

    // Created after classification, beneath a classified directory.
    std::fs::write(directory.join("keys/new"), "key")?;
    assert_eq!(
        read_receipts(&mut controller, &one, &[&directory.join("keys/new")])?,
        [1 << 4]
    );
    // Renamed away from its classifying name, then evicted: the inode keeps its class.
    std::fs::rename(directory.join(".env"), directory.join("notes"))?;
    evict()?;
    assert_eq!(
        read_receipts(&mut controller, &one, &[&directory.join("notes")])?,
        [1 << 3]
    );
    // A governed mapping is a read too; an ungoverned read is no receipt.
    let file = std::fs::File::open(directory.join("keys/id"))?;
    controller.drain()?;
    one.enter(std::process::id())?;
    // SAFETY: maps one page of a live file read-only; the mapping is unmapped below.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            3,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    cgroups.leave(std::process::id())?;
    assert_ne!(mapping, libc::MAP_FAILED);
    // SAFETY: unmaps exactly the mapping made above.
    unsafe { libc::munmap(mapping, 3) };
    let mapped = controller.drain()?;
    std::fs::read(directory.join(".env").with_file_name("notes"))?;
    let ungoverned = controller.drain()?;
    assert_eq!(
        mapped.iter().map(|event| event.taint).collect::<Vec<_>>(),
        [1 << 4]
    );
    assert!(ungoverned.is_empty(), "{ungoverned:?}");
    drop(anchor);
    std::fs::remove_dir_all(&directory)?;
    controller.remove_domain(one.id)?;
    Ok(())
}
