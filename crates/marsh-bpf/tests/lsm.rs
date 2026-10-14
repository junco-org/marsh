//! Run explicitly as root in a BPF-LSM-enabled VM, never against arbitrary host tasks.
#![cfg(feature = "privileged")]

use std::{
    io::{self, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
};

use marsh_bpf::Controller;
use marsh_peer_class::{AF_INET, Answers, Endpoint, LOOPBACK, Network, UNIX};

#[test]
#[ignore = "requires root, active BPF LSM, BTF, and loopback IPv4/IPv6 in a disposable VM"]
fn actual_lsm_enforcement_and_reporting() -> io::Result<()> {
    let mut controller = Controller::load()?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    let destination = receiver.local_addr()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let listener6 = TcpListener::bind("[::1]:0")?;
    let tgid = std::process::id();
    let mut answers = Answers {
        generation: 7,
        deny: 1 << LOOPBACK,
        guarded: 1 << LOOPBACK,
        ..Answers::default()
    };
    controller.answers(1, answers)?;
    // A populated denial table must not affect an ungoverned task.
    sender.send_to(b"unmapped", destination)?;
    assert!(controller.drain()?.is_empty());
    controller.task(tgid, 1)?;
    assert_eq!(
        sender
            .send_to(b"denied", destination)
            .err()
            .and_then(|error| error.raw_os_error()),
        Some(libc::EPERM)
    );
    assert_eq!(
        TcpStream::connect(listener.local_addr()?)
            .err()
            .and_then(|error| error.raw_os_error()),
        Some(libc::EPERM)
    );
    let events = controller.drain()?;
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.tgid == tgid
        && event.root == 1
        && event.generation == 7
        && event.class == LOOPBACK
        && event.decision == 1));

    answers.audit = 1;
    controller.answers(1, answers)?;
    sender.send_to(b"audit", destination)?;
    assert_eq!(controller.drain()?[0].decision, 2);
    controller.stale(1)?;
    sender.send_to(b"stale audit", destination)?;
    assert_eq!(controller.drain()?[0].decision, 4);
    answers.audit = 0;
    answers.stale = 1;
    answers.deny = 0;
    controller.answers(1, answers)?;
    assert!(sender.send_to(b"stale deny", destination).is_err());
    assert_eq!(controller.drain()?[0].decision, 3);

    answers.stale = 0;
    controller.answers(1, answers)?;
    // Root still has its real capabilities: refusal must come from uring_sqpoll,
    // not child capability stripping or a network-class denial.
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
            .all(|event| event.class == LOOPBACK && event.decision == 0)
    );
    assert_eq!(
        events[1].endpoint,
        Endpoint::from(listener.local_addr()?).normalized()
    );
    assert_eq!(
        events[3].endpoint,
        Endpoint::from(listener6.local_addr()?).normalized()
    );

    let mut wildcard = Endpoint::from(destination);
    wildcard.address = [0; 16];
    controller.local(1, &[wildcard])?;
    answers.local = 1 << LOOPBACK;
    controller.answers(1, answers)?;
    sender.send_to(b"wildcard listener", destination)?;
    assert_eq!(controller.drain()?[0].decision, 0);
    controller.local(1, &[])?;
    controller.answers(1, answers)?;
    assert!(sender.send_to(b"untraced local", destination).is_err());
    assert_eq!(controller.drain()?[0].decision, 1);

    let (unnamed, _unnamed_peer) = UnixDatagram::pair()?;
    answers.local = 0;
    controller.answers(1, answers)?;
    unnamed.send(b"permissive socketpair")?;
    answers.deny = 1 << UNIX;
    controller.answers(1, answers)?;
    assert!(unnamed.send(b"denied socketpair").is_err());
    answers.audit = 1;
    controller.answers(1, answers)?;
    unnamed.send(b"audit denied socketpair")?;
    answers.audit = 0;
    answers.deny = 0;
    answers.local = 1 << UNIX;
    controller.answers(1, answers)?;
    assert!(unnamed.send(b"unidentifiable local receiver").is_err());
    answers.audit = 1;
    controller.answers(1, answers)?;
    unnamed.send(b"audit local socketpair")?;
    answers.local = 0;
    controller.answers(1, answers)?;
    unnamed.send(b"audit normal permissive socketpair")?;
    let events = controller.drain()?;
    assert_eq!(
        events
            .iter()
            .map(|event| event.decision)
            .collect::<Vec<_>>(),
        [0, 1, 2, 1, 2, 0]
    );
    assert!(
        events
            .iter()
            .all(|event| event.class == UNIX && event.endpoint.unix_len == 0)
    );
    answers.audit = 0;

    let name = format!("marsh-bpf-{tgid}").into_bytes();
    let mut longer = name.clone();
    longer.push(0);
    let address = SocketAddr::from_abstract_name(&name)?;
    let other = SocketAddr::from_abstract_name(&longer)?;
    let _unix_receiver = UnixDatagram::bind_addr(&address)?;
    let _other_receiver = UnixDatagram::bind_addr(&other)?;
    let unix_sender = UnixDatagram::unbound()?;
    let mut endpoint = Endpoint {
        family: marsh_peer_class::AF_UNIX,
        unix_len: name.len() as u32 + 1,
        ..Endpoint::default()
    };
    endpoint.unix[1..1 + name.len()].copy_from_slice(&name);
    controller.local(1, &[endpoint])?;
    answers.local = 1 << UNIX;
    controller.answers(1, answers)?;
    unix_sender.send_to_addr(b"exact abstract", &address)?;
    assert!(
        unix_sender
            .send_to_addr(b"different significant length", &other)
            .is_err()
    );
    unix_sender.connect_addr(&address)?;
    unix_sender.send(b"connected unix peer")?;
    let events = controller.drain()?;
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].endpoint, endpoint);
    assert_eq!(events[1].decision, 1);
    assert_eq!(events[3].endpoint, endpoint);

    answers.local = 0;
    controller.answers(1, answers)?;
    // A readable unsupported family is the ordinary UNKNOWN class. Its protocol
    // rejects the address only after LSM, so allow/audit still return EAFNOSUPPORT.
    let mut unknown = [0u8; 20];
    unknown[..2].copy_from_slice(&(libc::AF_PACKET as u16).to_ne_bytes());
    let unknown_contact = || {
        // SAFETY: unknown is initialized and lives across the synchronous connect.
        let result = unsafe {
            libc::connect(
                sender.as_raw_fd(),
                unknown.as_ptr().cast(),
                unknown.len() as libc::socklen_t,
            )
        };
        if result == 0 {
            None
        } else {
            io::Error::last_os_error().raw_os_error()
        }
    };
    let unspecified = libc::sockaddr {
        sa_family: libc::AF_UNSPEC as u16,
        sa_data: [0; 14],
    };
    let disconnect = || {
        // SAFETY: unspecified is a complete initialized sockaddr; AF_UNSPEC uses
        // only sa_family, and sender remains a live UDP descriptor.
        let result = unsafe {
            libc::connect(
                sender.as_raw_fd(),
                std::ptr::from_ref(&unspecified),
                size_of::<libc::sa_family_t>() as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    assert_eq!(unknown_contact(), Some(libc::EAFNOSUPPORT));
    answers.deny = 1 << marsh_peer_class::UNKNOWN;
    controller.answers(1, answers)?;
    assert_eq!(unknown_contact(), Some(libc::EPERM));
    answers.audit = 1;
    controller.answers(1, answers)?;
    assert_eq!(unknown_contact(), Some(libc::EAFNOSUPPORT));
    let events = controller.drain()?;
    assert_eq!(
        events
            .iter()
            .map(|event| event.decision)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(
        events
            .iter()
            .all(|event| event.class == marsh_peer_class::UNKNOWN)
    );
    // AF_UNSPEC is a disconnect, always passes, and creates no Contact even when
    // UNKNOWN would be denied/audited. Prior LSM denial is still respected.
    disconnect()?;
    assert!(controller.drain()?.is_empty());
    answers.audit = 0;
    answers.deny = 0;
    controller.answers(1, answers)?;
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
    address.nl_family = libc::AF_NETLINK as u16;
    // SAFETY: address points to a complete initialized sockaddr_nl.
    let connected = unsafe {
        libc::connect(
            netlink.as_raw_fd(),
            std::ptr::from_ref(&address).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
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
            .all(|event| event.class == marsh_peer_class::NETLINK && event.decision == 0)
    );

    let network = Network {
        family: AF_INET,
        prefix: 24,
        class: 5,
        address: [198, 51, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    controller.configure(1, &[network])?;
    controller.answers(
        1,
        Answers {
            deny: 1 << 5,
            ..Answers::default()
        },
    )?;
    assert!(sender.send_to(b"root one", "198.51.100.1:9").is_err());
    assert_eq!(controller.drain()?[0].class, 5);
    controller.configure(
        2,
        &[Network {
            class: 6,
            ..network
        }],
    )?;
    controller.answers(
        2,
        Answers {
            deny: 1 << 6,
            ..Answers::default()
        },
    )?;
    controller.task(tgid, 2)?;
    assert!(sender.send_to(b"root two", "198.51.100.1:9").is_err());
    assert_eq!(controller.drain()?[0].class, 6);
    controller.remove_root(2)?;
    assert!(sender.send_to(b"missing answers", destination).is_err());
    assert_eq!(controller.drain()?[0].decision, 1);
    controller.remove_task(tgid)?;
    controller.remove_task(tgid)?;
    sender.send_to(b"unmapped again", destination)?;

    // Overflow must poison enforcement/reporting even for an audit root.
    controller.task(tgid, 1)?;
    controller.answers(
        1,
        Answers {
            audit: 1,
            ..Answers::default()
        },
    )?;
    let mut overflow_denied = false;
    for _ in 0..30_000 {
        if sender
            .send_to(b"fill ring", destination)
            .err()
            .is_some_and(|error| error.raw_os_error() == Some(libc::EPERM))
        {
            overflow_denied = true;
            break;
        }
    }
    assert!(overflow_denied);
    assert!(controller.drain().is_err());
    assert!(sender.send_to(b"sticky poison", destination).is_err());
    controller.remove_task(tgid)?;
    controller.remove_root(1)?;
    sender.send_to(b"unmapped despite poison", destination)?;
    drop(controller);
    sender.send_to(b"drop detached", destination)?;
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
