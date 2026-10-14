//! Socket classification shared by userspace policy and BPF LSM enforcement.
//!
//! Addresses use network-order octets; ports and all other integers use host order. IPv4
//! occupies the first four address bytes. Builtins are classified before declared networks.
//! The `std` feature adds sockaddr serialization and first-declaration trie normalization.
#![no_std]

#[cfg(any(feature = "std", test))]
extern crate std;

/// Linux unknown/unspecified address family.
pub const AF_UNSPEC: u16 = 0;
/// Linux UNIX-domain address family.
pub const AF_UNIX: u16 = 1;
/// Linux IPv4 address family.
pub const AF_INET: u16 = 2;
/// Linux IPv6 address family.
pub const AF_INET6: u16 = 10;
/// Linux netlink address family.
pub const AF_NETLINK: u16 = 16;

/// Builtin UNIX-domain peer class.
pub const UNIX: u32 = 0;
/// Builtin netlink peer class.
pub const NETLINK: u32 = 1;
/// Builtin loopback or unspecified-address peer class.
pub const LOOPBACK: u32 = 2;
/// Default class for an IP outside all declared networks.
pub const EXTERNAL: u32 = 3;
/// Builtin unreadable or unsupported peer class.
pub const UNKNOWN: u32 = 4;
/// Class bitset width; declared labels start at ID 5.
pub const MAX_CLASSES: usize = 64;

/// Fixed-layout socket endpoint shared with the kernel.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// Linux `AF_*` family number.
    pub family: u16,
    /// Host-endian IP port, or zero for other families.
    pub port: u16,
    /// IPv6 scope ID, or zero for other families.
    pub scope: u32,
    /// Network-order IP octets; IPv4 occupies the first four bytes.
    pub address: [u8; 16],
    /// Number of significant UNIX name bytes, at most 108.
    pub unix_len: u32,
    /// UNIX name, including the initial NUL for abstract sockets; unused bytes are zero.
    pub unix: [u8; 108],
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            family: AF_UNSPEC,
            port: 0,
            scope: 0,
            address: [0; 16],
            unix_len: 0,
            unix: [0; 108],
        }
    }
}

/// One declared or effective IP prefix and its policy-assigned class.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Network {
    /// `AF_INET` or `AF_INET6`; mapped endpoints use the IPv4 trie.
    pub family: u16,
    /// Class bit index, less than `MAX_CLASSES`.
    pub class: u16,
    /// Address prefix width in bits (0..=32 or 0..=128).
    pub prefix: u32,
    /// Network-order address octets; effective prefixes have zero host bits.
    pub address: [u8; 16],
}

/// One root's precomputed answers at a policy generation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answers {
    /// Userspace policy revision represented by these answers.
    pub generation: u64,
    /// Classes currently forbidden by policy rules.
    pub deny: u64,
    /// Classes any rule can forbid; used to fail closed while stale.
    pub guarded: u64,
    /// Local classes requiring membership in `LOCAL_ALLOWED`.
    pub local: u64,
    /// Nonzero when the policy basis changed after this generation was published.
    pub stale: u32,
    /// Nonzero to report refusals without blocking the contact.
    pub audit: u32,
}

/// Kernel contact event, including the actual endpoint and answer generation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    /// Sending process's thread-group ID.
    pub tgid: u32,
    /// Sending thread ID.
    pub tid: u32,
    /// Tracer-assigned root ID.
    pub root: u64,
    /// Answer generation used for this contact.
    pub generation: u64,
    /// Endpoint used by the kernel, not a prior userspace copy.
    pub endpoint: Endpoint,
    /// Classified peer's class bit index.
    pub class: u32,
    /// 0 allow, 1 deny, 2 audit, 3 stale deny, 4 stale audit.
    pub decision: u32,
}

/// Root-isolated key for a permitted traced local endpoint.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LocalKey {
    /// Tracer-assigned root ID.
    pub root: u64,
    /// Canonical endpoint from [`Endpoint::normalized`].
    pub endpoint: Endpoint,
}

impl Endpoint {
    /// Decode a Linux native sockaddr; malformed or unsupported addresses become unknown.
    /// An IPv6 sockaddr may omit its scope field, as accepted by the kernel.
    #[expect(clippy::cast_possible_truncation, reason = "UNIX names contain at most 108 bytes")]
    pub fn from_sockaddr(bytes: &[u8]) -> Self {
        if bytes.len() < 2 || bytes.len() > 128 {
            return Self::default();
        }
        let mut endpoint = Self {
            family: u16::from_ne_bytes([bytes[0], bytes[1]]),
            ..Self::default()
        };
        match endpoint.family {
            AF_UNIX if bytes.len() <= 110 => {
                let path = &bytes[2..];
                let length = if path.first() == Some(&0) {
                    path.len()
                } else {
                    path.iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(path.len())
                };
                endpoint.unix[..length].copy_from_slice(&path[..length]);
                endpoint.unix_len = length as u32;
            }
            AF_NETLINK => {}
            AF_INET if bytes.len() >= 16 => {
                endpoint.port = u16::from_be_bytes([bytes[2], bytes[3]]);
                endpoint.address[..4].copy_from_slice(&bytes[4..8]);
            }
            AF_INET6 if bytes.len() >= 24 => {
                endpoint.port = u16::from_be_bytes([bytes[2], bytes[3]]);
                endpoint.address.copy_from_slice(&bytes[8..24]);
                if let Some(scope) = bytes.get(24..28) {
                    endpoint.scope = u32::from_ne_bytes([scope[0], scope[1], scope[2], scope[3]]);
                }
            }
            _ => return Self::default(),
        }
        endpoint
    }

    /// Canonical map key: unwrap mapped IPv4 and discard irrelevant fields and UNIX padding.
    /// Abstract UNIX names retain every byte counted by their length.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let mut endpoint = *self;
        endpoint.normalize();
        endpoint
    }

    /// Canonicalize in place without a full endpoint temporary, for the BPF stack limit.
    /// Abstract UNIX names retain every byte counted by their length.
    #[cfg_attr(target_arch = "bpf", inline(always))]
    #[cfg_attr(not(target_arch = "bpf"), inline)]
    #[expect(clippy::cast_possible_truncation, reason = "UNIX names contain at most 108 bytes")]
    pub fn normalize(&mut self) {
        match self.family {
            AF_INET | AF_INET6 => {
                if let Some(address) = ipv4(self) {
                    self.family = AF_INET;
                    self.address[..4].copy_from_slice(&address);
                    self.address[4..].fill(0);
                    self.scope = 0;
                }
                self.unix_len = 0;
                self.unix.fill(0);
            }
            AF_UNIX if self.unix_len <= 108 => {
                self.port = 0;
                self.scope = 0;
                self.address.fill(0);
                let path = &self.unix[..self.unix_len as usize];
                let length = if path.first() == Some(&0) {
                    path.len()
                } else {
                    path.iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(path.len())
                };
                self.unix_len = length as u32;
                // BPF must see an absolute 0..108 index bound. A tail-slice memset
                // loses the correlation between its variable pointer and length.
                for (index, byte) in self.unix.iter_mut().enumerate() {
                    if index >= length {
                        // SAFETY: byte is an initialized, uniquely borrowed array
                        // element. Volatile prevents folding back into tail memset.
                        unsafe { core::ptr::write_volatile(byte, 0) };
                    }
                }
            }
            _ => {
                if self.family != AF_NETLINK {
                    self.family = AF_UNSPEC;
                }
                self.port = 0;
                self.scope = 0;
                self.address.fill(0);
                self.unix_len = 0;
                self.unix.fill(0);
            }
        }
    }

    /// Encode a Linux native sockaddr. Unknown/malformed endpoints encode as an empty slice.
    /// IPv6 flowinfo is zero; pathname UNIX names include a terminator when there is room.
    #[cfg(any(feature = "std", test))]
    pub fn sockaddr(&self) -> std::vec::Vec<u8> {
        let mut bytes = match self.family {
            AF_INET => std::vec![0; 16],
            AF_INET6 => std::vec![0; 28],
            AF_NETLINK => std::vec![0; 12],
            AF_UNIX if self.unix_len <= 108 => {
                let length = self.unix_len as usize;
                let terminator = usize::from(length > 0 && self.unix[0] != 0 && length < 108);
                std::vec![0; 2 + length + terminator]
            }
            _ => return std::vec::Vec::new(),
        };
        bytes[..2].copy_from_slice(&self.family.to_ne_bytes());
        match self.family {
            AF_INET => {
                bytes[2..4].copy_from_slice(&self.port.to_be_bytes());
                bytes[4..8].copy_from_slice(&self.address[..4]);
            }
            AF_INET6 => {
                bytes[2..4].copy_from_slice(&self.port.to_be_bytes());
                bytes[8..24].copy_from_slice(&self.address);
                bytes[24..28].copy_from_slice(&self.scope.to_ne_bytes());
            }
            AF_UNIX => bytes[2..2 + self.unix_len as usize]
                .copy_from_slice(&self.unix[..self.unix_len as usize]),
            _ => {}
        }
        bytes
    }
}

#[cfg(any(feature = "std", test))]
impl From<std::net::SocketAddr> for Endpoint {
    fn from(address: std::net::SocketAddr) -> Self {
        let mut endpoint = Self {
            port: address.port(),
            ..Self::default()
        };
        match address {
            std::net::SocketAddr::V4(address) => {
                endpoint.family = AF_INET;
                endpoint.address[..4].copy_from_slice(&address.ip().octets());
            }
            std::net::SocketAddr::V6(address) => {
                endpoint.family = AF_INET6;
                endpoint.address = address.ip().octets();
                endpoint.scope = address.scope_id();
            }
        }
        endpoint
    }
}

fn ipv4(endpoint: &Endpoint) -> Option<[u8; 4]> {
    if endpoint.family == AF_INET {
        return Some([
            endpoint.address[0],
            endpoint.address[1],
            endpoint.address[2],
            endpoint.address[3],
        ]);
    }
    if endpoint.family == AF_INET6
        && endpoint.address[..10] == [0; 10]
        && endpoint.address[10..12] == [0xff; 2]
    {
        return Some([
            endpoint.address[12],
            endpoint.address[13],
            endpoint.address[14],
            endpoint.address[15],
        ]);
    }
    None
}

/// Classify builtins before consulting a network trie. `None` means an external IP needs lookup.
/// Unspecified addresses reach this host and are therefore loopback, including mapped IPv4.
#[inline]
pub fn builtin_class(endpoint: &Endpoint) -> Option<u32> {
    match endpoint.family {
        AF_UNIX => Some(UNIX),
        AF_NETLINK => Some(NETLINK),
        AF_INET | AF_INET6 => {
            let local = if let Some(address) = ipv4(endpoint) {
                address[0] == 127 || address == [0; 4]
            } else {
                endpoint.address == [0; 16]
                    || (endpoint.address[..15] == [0; 15] && endpoint.address[15] == 1)
            };
            if local { Some(LOOPBACK) } else { None }
        }
        _ => Some(UNKNOWN),
    }
}

/// First-match network membership, with exactly the same mapped-IPv4 treatment as builtins.
/// Invalid network families and out-of-range prefixes never match.
#[inline]
pub fn network_contains(network: &Network, endpoint: &Endpoint) -> bool {
    if let Some(address) = ipv4(endpoint) {
        network.family == AF_INET
            && network.prefix <= 32
            && prefix_equal(&network.address, &address, network.prefix)
    } else {
        network.family == AF_INET6
            && endpoint.family == AF_INET6
            && network.prefix <= 128
            && prefix_equal(&network.address, &endpoint.address, network.prefix)
    }
}

fn prefix_equal(left: &[u8], right: &[u8], prefix: u32) -> bool {
    let full = prefix as usize / 8;
    let bits = prefix % 8;
    let mut index = 0;
    while index < full {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    bits == 0 || (left[full] ^ right[full]) & (0xff << (8 - bits)) == 0
}

/// Convert declaration-order matching to longest-prefix matching without changing its answer.
///
/// Each canonical prefix carries the first declared network's class that contains it. Duplicate
/// prefixes are emitted once, in their original order. Callers must validate network families,
/// prefix widths and class IDs before loading a kernel map.
#[cfg(any(feature = "std", test))]
pub fn effective_networks(networks: &[Network]) -> std::vec::Vec<Network> {
    let mut effective = std::vec::Vec::with_capacity(networks.len());
    // ponytail: O(n²) setup for policy-sized network lists; use a prefix tree if setup grows hot.
    for network in networks {
        let mut network = *network;
        let width = match network.family {
            AF_INET => 32,
            AF_INET6 => 128,
            _ => continue,
        };
        if network.prefix > width {
            continue;
        }
        for (index, byte) in (0_u32..).zip(network.address.iter_mut()) {
            let remaining = network.prefix.saturating_sub(index * 8).min(8);
            *byte &= if remaining == 0 {
                0
            } else {
                0xff << (8 - remaining)
            };
        }
        // Do not use network_contains: a declared IPv6 mapped prefix remains an IPv6 prefix,
        // even though actual mapped endpoints are looked up in the IPv4 trie.
        network.class = networks
            .iter()
            .find(|declared| {
                declared.family == network.family
                    && declared.prefix <= network.prefix
                    && prefix_equal(&declared.address, &network.address, declared.prefix)
            })
            .map_or(network.class, |declared| declared.class);
        if !effective.iter().any(|existing: &Network| {
            existing.family == network.family
                && existing.prefix == network.prefix
                && existing.address == network.address
        }) {
            effective.push(network);
        }
    }
    effective
}

#[cfg(test)]
mod tests;
