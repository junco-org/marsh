//! The sockets a contact with a local peer may deliver to.
//!
//! A loopback or unix-domain peer is another process on this host. Data sent to it reaches
//! whoever holds a socket bound to that address: the listener, the connections it accepted, or a
//! datagram socket. Those are found in this network namespace's `/proc/net` tables by local
//! address, and their holders by the socket inodes their descriptors name.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::Endpoint;
use std::io;

/// Exact kernel bound endpoints, not the lossy abstract-name rendering in /proc/net/unix.
pub(crate) fn endpoints(tasks: &[nix::unistd::Pid]) -> io::Result<Vec<Endpoint>> {
    let mut seen = HashSet::new();
    let mut endpoints = HashSet::new();
    for tid in tasks {
        let entries = match std::fs::read_dir(format!("/proc/{tid}/fd")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let Some(inode) = std::fs::read_link(entry.path()).ok().and_then(|link| {
                link.to_str()
                    .and_then(|link| {
                        link.strip_prefix("socket:[")
                            .and_then(|link| link.strip_suffix(']'))
                    })
                    .and_then(|inode| inode.parse::<u64>().ok())
            }) else {
                continue;
            };
            if seen.contains(&inode) {
                continue;
            }
            let (endpoint, v4) = match crate::socket::bound_endpoints(*tid, fd) {
                Ok(endpoints) => endpoints,
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EBADF | libc::ESRCH | libc::ENOENT)
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            seen.insert(inode);
            endpoints.insert(endpoint);
            if let Some(v4) = v4 {
                endpoints.insert(v4);
            }
        }
    }
    let mut endpoints: Vec<_> = endpoints.into_iter().collect();
    endpoints.sort_unstable_by_key(|endpoint| {
        (
            endpoint.family,
            endpoint.port,
            endpoint.scope,
            endpoint.address,
            endpoint.unix_len,
            endpoint.unix,
        )
    });
    Ok(endpoints)
}
/// A local address a contact names.
#[derive(Debug, PartialEq, Eq)]
enum Local {
    Ip(IpAddr, u16),
    /// A filesystem path as the caller named it, or an abstract name as `/proc/net/unix` prints
    /// it: `@`, then the name with each NUL as `@`.
    Unix(String),
}

/// The inodes of the sockets bound to the loopback or unix address `peer` (a native `sockaddr`),
/// or `None` when `peer` names no such address.
pub(crate) fn bound_to(peer: &[u8]) -> Option<HashSet<u64>> {
    let local = decode(peer)?;
    let mut inodes = HashSet::new();
    match &local {
        Local::Ip(address, port) => {
            for table in ["tcp", "tcp6", "udp", "udp6"] {
                let Ok(text) = std::fs::read_to_string(format!("/proc/self/net/{table}")) else {
                    continue;
                };
                inodes.extend(text.lines().skip(1).filter_map(|line| {
                    let fields: Vec<&str> = line.split_ascii_whitespace().collect();
                    let (bound, bound_port) = inet(fields.get(1)?)?;
                    let matches = bound_port == *port
                        && (unspecified(bound) || unspecified(*address) || same(bound, *address));
                    matches.then(|| fields.get(9)?.parse::<u64>().ok())?
                }));
            }
        }
        Local::Unix(path) => {
            let text = std::fs::read_to_string("/proc/self/net/unix").ok()?;
            inodes.extend(text.lines().skip(1).filter_map(|line| {
                let fields: Vec<&str> = line.split_ascii_whitespace().collect();
                let bound = fields.get(7)?;
                same_path(bound, path).then(|| fields.get(6)?.parse::<u64>().ok())?
            }));
        }
    }
    Some(inodes)
}

fn decode(peer: &[u8]) -> Option<Local> {
    let family = u16::from_ne_bytes([*peer.first()?, *peer.get(1)?]);
    match i32::from(family) {
        libc::AF_INET if peer.len() >= 8 => {
            let port = u16::from_be_bytes([peer[2], peer[3]]);
            let address = Ipv4Addr::new(peer[4], peer[5], peer[6], peer[7]);
            local_ip(IpAddr::V4(address)).then_some(Local::Ip(IpAddr::V4(address), port))
        }
        libc::AF_INET6 if peer.len() >= 24 => {
            let port = u16::from_be_bytes([peer[2], peer[3]]);
            let octets: [u8; 16] = peer[8..24].try_into().ok()?;
            let address = IpAddr::V6(Ipv6Addr::from(octets));
            local_ip(address).then_some(Local::Ip(address, port))
        }
        libc::AF_UNIX if peer.len() > 2 => {
            let name = &peer[2..];
            if name[0] == 0 {
                let abstract_name: String = name[1..]
                    .iter()
                    .map(|byte| if *byte == 0 { '@' } else { char::from(*byte) })
                    .collect();
                Some(Local::Unix(format!("@{abstract_name}")))
            } else {
                let end = name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(name.len());
                Some(Local::Unix(
                    String::from_utf8_lossy(&name[..end]).into_owned(),
                ))
            }
        }
        _ => None,
    }
}

/// Whether `address` reaches this host: loopback, unspecified, or loopback mapped into IPv6.
fn local_ip(address: IpAddr) -> bool {
    let address = canonical(address);
    address.is_loopback() || address.is_unspecified()
}

fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

fn unspecified(address: IpAddr) -> bool {
    canonical(address).is_unspecified()
}

fn same(left: IpAddr, right: IpAddr) -> bool {
    canonical(left) == canonical(right)
}

/// A `/proc/net/{tcp,udp}{,6}` local address: native-order 32-bit words in hex, then the port.
fn inet(field: &str) -> Option<(IpAddr, u16)> {
    let (address, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |index: usize| {
        u32::from_str_radix(address.get(index * 8..index * 8 + 8)?, 16)
            .ok()
            .map(u32::to_ne_bytes)
    };
    let address = match address.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut octets = [0; 16];
            for index in 0..4 {
                octets[index * 4..index * 4 + 4].copy_from_slice(&word(index)?);
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some((address, port))
}

/// Whether a bound unix path matches the one a contact named. A relative path resolves against a
/// working directory this table does not record, so it matches any path ending in it.
fn same_path(bound: &str, named: &str) -> bool {
    if bound == named {
        return true;
    }
    if bound.starts_with('@') || named.starts_with('@') {
        return false;
    }
    let tail = |full: &str, relative: &str| {
        !relative.starts_with('/') && full.ends_with(&format!("/{relative}"))
    };
    tail(bound, named) || tail(named, bound)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn family(family: i32) -> Vec<u8> {
        u16::try_from(family).unwrap().to_ne_bytes().to_vec()
    }

    #[test]
    fn decodes_local_addresses_only() {
        let mut v4 = vec![0; 16];
        v4[..2].copy_from_slice(&family(libc::AF_INET));
        v4[2..4].copy_from_slice(&8080_u16.to_be_bytes());
        v4[4..8].copy_from_slice(&[127, 0, 0, 1]);
        assert_eq!(
            decode(&v4),
            Some(Local::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
        );
        v4[4..8].copy_from_slice(&[192, 0, 2, 1]);
        assert_eq!(decode(&v4), None);
        let mut unix = family(libc::AF_UNIX);
        unix.extend_from_slice(b"relay.sock\0");
        assert_eq!(decode(&unix), Some(Local::Unix("relay.sock".into())));
        let mut abstract_name = family(libc::AF_UNIX);
        abstract_name.extend_from_slice(b"\0name\0x");
        assert_eq!(decode(&abstract_name), Some(Local::Unix("@name@x".into())));
    }

    #[test]
    fn parses_proc_net_addresses() {
        let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
        assert_eq!(
            inet(&format!("{loopback:08X}:1F90")),
            Some((IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
        );
        let mut words = String::new();
        for chunk in Ipv6Addr::LOCALHOST.octets().chunks(4) {
            write!(
                words,
                "{:08X}",
                u32::from_ne_bytes(chunk.try_into().unwrap())
            )
            .unwrap();
        }
        assert_eq!(
            inet(&format!("{words}:0050")),
            Some((IpAddr::V6(Ipv6Addr::LOCALHOST), 80))
        );
    }

    #[test]
    fn relative_unix_paths_match_by_suffix() {
        assert!(same_path("/work/relay.sock", "relay.sock"));
        assert!(same_path("relay.sock", "/work/relay.sock"));
        assert!(!same_path("/work/other.sock", "relay.sock"));
        assert!(!same_path("@relay.sock", "relay.sock"));
    }

    #[test]
    fn finds_a_listening_loopback_socket() {
        use std::os::unix::fs::MetadataExt;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut peer = vec![0; 16];
        peer[..2].copy_from_slice(&family(libc::AF_INET));
        peer[2..4].copy_from_slice(&port.to_be_bytes());
        peer[4..8].copy_from_slice(&[127, 0, 0, 1]);
        let fd = std::os::fd::AsRawFd::as_raw_fd(&listener);
        let inode = std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .unwrap()
            .ino();
        assert!(bound_to(&peer).unwrap().contains(&inode));
        assert!(
            endpoints(&[nix::unistd::getpid()])
                .unwrap()
                .contains(&Endpoint::from_sockaddr(&peer))
        );
    }

    #[test]
    fn inventory_preserves_unix_names_with_spaces() {
        use std::os::unix::{ffi::OsStrExt as _, net::UnixListener};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local peer socket");
        let _listener = UnixListener::bind(&path).unwrap();
        let mut bytes = family(libc::AF_UNIX);
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(0);
        assert!(
            endpoints(&[nix::unistd::getpid()])
                .unwrap()
                .contains(&Endpoint::from_sockaddr(&bytes))
        );
    }

    #[test]
    fn inventory_distinguishes_abstract_name_bytes() {
        use std::os::{
            linux::net::SocketAddrExt as _,
            unix::net::{SocketAddr, UnixDatagram},
        };
        let name = format!("marsh-inventory-{}@literal", std::process::id());
        let _socket =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        let mut bytes = family(libc::AF_UNIX);
        bytes.push(0);
        bytes.extend_from_slice(name.as_bytes());
        assert!(
            endpoints(&[nix::unistd::getpid()])
                .unwrap()
                .contains(&Endpoint::from_sockaddr(&bytes))
        );
    }
}
