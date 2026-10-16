//! Run explicitly as root in a BPF-LSM-enabled VM, never against arbitrary host tasks.
#![cfg(feature = "privileged")]

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram, UnixStream},
    },
};

use marsh_bpf::{Admission, Controller};
use marsh_peer_class::{
    AF_INET, ALLOW, AUDIT, Answers, CONTACT, CONTINGENT, DENY, Endpoint, Event, LOOPBACK,
    NETLINK, Network, RECEIPT, RULE, STALE, UNANSWERED, UNIX, UNKNOWN, UNTRACED,
};

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
    let mut controller = Controller::load()?;
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
    controller.task(tgid, 1)?;
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
    controller.remove_task(tgid)?;
    Ok(())
}

#[test]
#[ignore = "requires root, active BPF LSM, BTF, and loopback IPv4/IPv6 in a disposable VM"]
fn contacts_are_judged_against_the_kernels_address() -> io::Result<()> {
    let mut controller = Controller::load()?;
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
    controller.task(tgid, 1)?;
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
    stream_pair.1.read(&mut sink)?;
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
    controller.task(tgid, 2)?;
    assert!(refused(sender.send_to(b"root two", "198.51.100.1:9")));
    assert_eq!(controller.drain()?[0].class, 6);
    controller.remove_root(2)?;
    assert!(refused(sender.send_to(b"missing answers", destination)));
    assert_eq!(decisions(&controller.drain()?), [(DENY, UNANSWERED)]);
    controller.remove_task(tgid)?;
    controller.remove_task(tgid)?;
    sender.send_to(b"unmapped again", destination)?;

    // Overflow must poison enforcement and reporting, even for an auditing root.
    controller.task(tgid, 1)?;
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
    controller.remove_task(tgid)?;
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
fn local_receipts_taint_the_reader_in_the_kernel() -> io::Result<()> {
    let mut controller = Controller::load()?;
    let tgid = std::process::id();
    let outside = UdpSocket::bind("127.0.0.1:0")?;
    let mut buffer = [0; 64];
    let mut acknowledged = [0; 16];
    // Root 1 carries `carried`; root 2 refuses loopback once it receives any class it lacks.
    let stage = |controller: &mut Controller, carried: u64, acknowledged: [u32; 16]| {
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
    controller.task(tgid, 1)?;
    udp_sender.send_to(b"secret", receiver.local_addr()?)?;
    udp_sender.send_to(b"again", receiver.local_addr()?)?;
    controller.task(tgid, 2)?;
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
    controller.task(tgid, 2)?;
    let mut quiet = TcpStream::connect(listener.local_addr()?)?;
    let (mut quiet_server, _) = listener.accept()?;
    controller.task(tgid, 1)?;
    let mut loud = TcpStream::connect(listener.local_addr()?)?;
    let (mut loud_server, _) = listener.accept()?;
    loud.write_all(b"secret")?;
    controller.task(tgid, 2)?;
    quiet.write_all(b"plain")?;
    controller.drain()?;
    quiet_server.read_exact(&mut buffer[..5])?;
    assert!(controller.drain()?.is_empty());
    loud_server.read_exact(&mut buffer[..6])?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 4, 1)]);
    acknowledged[4] = 1;

    // A UNIX stream tags its connected peer; read() is a socket read too.
    stage(&mut controller, 1 << 5, acknowledged)?;
    controller.task(tgid, 1)?;
    let (mut unix_sender, mut unix_receiver) = UnixStream::pair()?;
    unix_sender.write_all(b"secret")?;
    controller.task(tgid, 2)?;
    controller.drain()?;
    unix_receiver.read_exact(&mut buffer[..6])?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 5, 1)]);
    acknowledged[5] = 1;

    // A reader already blocked when the tainted bytes arrive is tainted as its read returns.
    stage(&mut controller, 1 << 8, acknowledged)?;
    controller.task(tgid, 1)?;
    let (mut writer, reader) = UnixStream::pair()?;
    let mut child = std::process::Command::new("head")
        .args(["-c", "6"])
        .stdin(std::process::Stdio::from(OwnedFd::from(reader)))
        .stdout(std::process::Stdio::null())
        .spawn()?;
    controller.task(child.id(), 2)?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    controller.drain()?;
    writer.write_all(b"secret")?;
    assert!(child.wait()?.success());
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 8, 1)]);
    controller.remove_task(child.id())?;
    acknowledged[8] = 1;

    // Every message of one recvmmsg is a read: a tainted datagram arriving after a clean one
    // taints the reader, though recvmmsg reads it without passing sock_recvmsg.
    stage(&mut controller, 1 << 9, acknowledged)?;
    controller.remove_task(tgid)?;
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
    controller.task(reader, 2)?;
    // Ungoverned, so untagged; the reader takes it before the tainted one exists.
    udp_sender.send_to(b"clean", batch.local_addr()?)?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    controller.task(tgid, 1)?;
    udp_sender.send_to(b"secret", batch.local_addr()?)?;
    let mut status = 0;
    // SAFETY: waits for the forked child above; status is a live out-parameter.
    if unsafe { libc::waitpid(reader.cast_signed(), &raw mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    controller.remove_task(reader)?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 9, 1)]);
    acknowledged[9] = 1;

    // A port's datagram tag outlives a socket that shared the port at another address.
    stage(&mut controller, 1 << 10, acknowledged)?;
    controller.remove_task(tgid)?;
    let shared = UdpSocket::bind("127.0.0.2:0")?;
    let neighbour = UdpSocket::bind(("127.0.0.1", shared.local_addr()?.port()))?;
    controller.task(tgid, 1)?;
    udp_sender.send_to(b"secret", shared.local_addr()?)?;
    drop(neighbour);
    // UDP sockets are freed after an RCU grace period.
    std::thread::sleep(std::time::Duration::from_secs(1));
    controller.task(tgid, 2)?;
    controller.drain()?;
    shared.recv(&mut buffer)?;
    assert_eq!(receipts(&controller.drain()?), [(2, 1 << 10, 1)]);
    acknowledged[10] = 1;

    // Two classes at once are undecidable until userspace republishes.
    stage(&mut controller, (1 << 6) | (1 << 7), acknowledged)?;
    controller.task(tgid, 1)?;
    let (datagram_sender, datagram_receiver) = UnixDatagram::pair()?;
    datagram_sender.send(b"secrets")?;
    controller.task(tgid, 2)?;
    controller.drain()?;
    datagram_receiver.recv(&mut buffer)?;
    assert!(refused(outside.send_to(b"undecidable", outside.local_addr()?)));
    let events = controller.drain()?;
    assert_eq!(receipts(&events), [(2, (1 << 6) | (1 << 7), 1)]);
    assert_eq!(decisions(&events[1..]), [(DENY, STALE)]);
    controller.remove_task(tgid)?;
    Ok(())
}

/// A child in another PID namespace cannot be attributed: it alone is killed, and enforcement
/// stays healthy for everyone else.
#[test]
#[ignore = "requires active BPF LSM, root PID-namespace privileges, and unshare in a disposable VM"]
fn namespace_changed_child_is_refused_alone() -> io::Result<()> {
    use std::{
        io::BufRead,
        process::{Command, Stdio},
    };

    let mut controller = Controller::load()?;
    // Keep the inherited procfs view: its first NSpid is the tracer-visible PID.
    // The shell reads its own status with builtins, not a subprocess's PID.
    let script = "while IFS= read -r line; do case \"$line\" in NSpid:*) set -- $line; printf '%s\\n' \"$2\"; break ;; esac; done </proc/self/status; sleep 30";
    let mut child = Command::new("unshare")
        .args(["--pid", "--fork", "/bin/sh", "-c", script])
        .stdout(Stdio::piped())
        .spawn()?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing namespace child stdout"))?;
    let mut line = String::new();
    io::BufReader::new(output).read_line(&mut line)?;
    let tgid: u32 = line.trim().parse().map_err(io::Error::other)?;
    let registration = controller.task(tgid, 77)?;
    // The killed shell is its namespace's init, so the whole namespace and unshare end at once.
    let ended = child.wait()?;
    assert_eq!(registration, Admission::Refused);
    assert!(!ended.success());
    controller.publish(77, Answers::default())?;
    assert!(controller.drain()?.is_empty());
    Ok(())
}
