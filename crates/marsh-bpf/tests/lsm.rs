//! Run explicitly as root in a BPF-LSM-enabled VM, never against arbitrary host tasks.
#![cfg(feature = "privileged")]

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram, UnixStream},
    },
};

use marsh_bpf::Controller;
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

#[test]
#[ignore = "requires active BPF LSM, root PID-namespace privileges, and unshare in a disposable VM"]
fn namespace_changed_child_cannot_be_registered() -> io::Result<()> {
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
    let registration = controller.task(tgid, 77);
    // Rejected registration retains the pidfd, so fatal cleanup must kill this
    // child too without affecting the unrelated unshare parent or host tasks.
    controller.terminate()?;
    child.wait()?;
    assert_eq!(
        registration.err().map(|error| error.kind()),
        Some(io::ErrorKind::Unsupported)
    );
    Ok(())
}
