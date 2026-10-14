//! The peers of the sockets traced tasks write to.
//!
//! A socket connected before a policy forbade its peer can still carry data afterwards, with no
//! call naming that peer. Writes to a socket are therefore judged by the socket's own peer, read
//! with `getpeername` on a duplicate of the task's descriptor.

use std::collections::HashMap;
use std::io;

use crate::Endpoint;
use nix::unistd::Pid;
use rustix::fd::OwnedFd;
use rustix::net::{getpeername, getsockname, sockopt};
use rustix::process::{PidfdFlags, PidfdGetfdFlags, pidfd_getfd, pidfd_open};

/// Cached peers of connected sockets, by socket inode.
///
/// Only a connected peer is cached: a socket that is unconnected, or still connecting, is asked
/// again at its next write. Any `connect` may change a cached peer, so it forgets them all.
#[derive(Default)]
pub(crate) struct Sockets(HashMap<u64, Vec<u8>>);

/// The most connected sockets remembered before the cache starts over.
const CACHE_LIMIT: usize = 4096;

impl Sockets {
    /// The peer `sockaddr` of the socket `fd` names in `tid`. `None` when the descriptor is not a
    /// socket or the socket has no peer; empty when it is a socket whose peer cannot be read.
    pub(crate) fn peer(&mut self, tid: Pid, fd: u64) -> Option<Vec<u8>> {
        // Descriptors are `unsigned int`; one no `int` holds is never open.
        let fd = i32::try_from(fd & u64::from(u32::MAX)).ok()?;
        let link = std::fs::read_link(format!("/proc/{tid}/fd/{fd}")).ok()?;
        let inode = link
            .to_str()?
            .strip_prefix("socket:[")?
            .strip_suffix(']')?
            .parse()
            .ok()?;
        if let Some(peer) = self.0.get(&inode) {
            return Some(peer.clone());
        }
        match connected_peer(tid, fd) {
            Ok(Some(peer)) => {
                if self.0.len() >= CACHE_LIMIT {
                    self.0.clear();
                }
                self.0.insert(inode, peer.clone());
                Some(peer)
            }
            Ok(None) => None,
            Err(_) => Some(Vec::new()),
        }
    }

    /// Forgets every cached peer.
    pub(crate) fn forget(&mut self) {
        self.0.clear();
    }
}

/// The peer of `tid`'s socket `fd`, or `None` while it has none.
fn connected_peer(tid: Pid, fd: i32) -> io::Result<Option<Vec<u8>>> {
    let socket = copy_socket(tid, fd)?;
    let peer = match getpeername(&socket) {
        Ok(peer) => peer,
        Err(rustix::io::Errno::NOTCONN) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(peer.map(|peer| {
        // SAFETY: a `SocketAddrAny` holds at least `addr_len` initialised bytes behind `as_ptr`.
        let bytes = unsafe {
            std::slice::from_raw_parts(peer.as_ptr().cast::<u8>(), peer.addr_len() as usize)
        };
        bytes.to_vec()
    }))
}

/// Kernel-owned bound names, including a IPv4 wildcard for a dual-stack IPv6 socket.
pub(crate) fn bound_endpoints(tid: Pid, fd: i32) -> io::Result<(Endpoint, Option<Endpoint>)> {
    let socket = copy_socket(tid, fd)?;
    let address = getsockname(&socket)?;
    // SAFETY: SocketAddrAny owns addr_len initialized bytes behind as_ptr.
    let bytes = unsafe {
        std::slice::from_raw_parts(address.as_ptr().cast::<u8>(), address.addr_len() as usize)
    };
    let mut endpoint = Endpoint::from_sockaddr(bytes);
    endpoint.normalize();
    let v4 = if endpoint.family == marsh_peer_class::AF_INET6
        && endpoint.address == [0; 16]
        && !sockopt::ipv6_v6only(&socket)?
    {
        Some(Endpoint {
            family: marsh_peer_class::AF_INET,
            port: endpoint.port,
            ..Endpoint::default()
        })
    } else {
        None
    };
    Ok((endpoint, v4))
}

fn copy_socket(tid: Pid, fd: i32) -> io::Result<OwnedFd> {
    let process = thread_group(tid)?;
    let process = rustix::process::Pid::from_raw(process)
        .ok_or_else(|| io::Error::other("invalid thread group"))?;
    let handle = pidfd_open(process, PidfdFlags::empty())?;
    pidfd_getfd(&handle, fd, PidfdGetfdFlags::empty()).map_err(Into::into)
}

/// The process `tid` is a thread of: `pidfd_open` accepts only a thread-group leader.
fn thread_group(tid: Pid) -> io::Result<i32> {
    std::fs::read_to_string(format!("/proc/{tid}/status"))?
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))
        .and_then(|tgid| tgid.trim().parse().ok())
        .ok_or_else(|| io::Error::other(format!("no thread group for {tid}")))
}
