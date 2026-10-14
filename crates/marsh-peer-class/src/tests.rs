#![allow(clippy::unwrap_used)]

use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::vec::Vec;

fn ip(endpoint: &Endpoint) -> Option<IpAddr> {
    match endpoint.family {
        AF_INET => Some(IpAddr::V4(Ipv4Addr::new(
            endpoint.address[0],
            endpoint.address[1],
            endpoint.address[2],
            endpoint.address[3],
        ))),
        AF_INET6 => {
            let address = Ipv6Addr::from(endpoint.address);
            Some(
                address
                    .to_ipv4_mapped()
                    .map_or(IpAddr::V6(address), IpAddr::V4),
            )
        }
        _ => None,
    }
}

// Independent integer-mask oracle, matching the original userspace policy implementation.
fn reference_contains(network: &Network, endpoint: &Endpoint) -> bool {
    let (candidate, family, width) = match ip(endpoint) {
        Some(IpAddr::V4(address)) => (u128::from(u32::from(address)) << 96, AF_INET, 32),
        Some(IpAddr::V6(address)) => (u128::from(address), AF_INET6, 128),
        None => return false,
    };
    if network.family != family || network.prefix > width {
        return false;
    }
    let mask = if network.prefix == 0 {
        0
    } else {
        u128::MAX << (128 - network.prefix)
    };
    u128::from_be_bytes(network.address) & mask == candidate & mask
}

fn reference_builtin(endpoint: &Endpoint) -> Option<u32> {
    match endpoint.family {
        AF_UNIX => Some(UNIX),
        AF_NETLINK => Some(NETLINK),
        _ => match ip(endpoint) {
            Some(address) if address.is_unspecified() || address.is_loopback() => Some(LOOPBACK),
            Some(_) => None,
            None => Some(UNKNOWN),
        },
    }
}

fn check(networks: &[Network], effective: &[Network], endpoint: &Endpoint) {
    assert_eq!(
        builtin_class(endpoint),
        reference_builtin(endpoint),
        "{endpoint:?}"
    );
    for network in networks {
        assert_eq!(
            network_contains(network, endpoint),
            reference_contains(network, endpoint)
        );
    }
    let declared = networks
        .iter()
        .find(|network| reference_contains(network, endpoint))
        .map_or(EXTERNAL, |network| u32::from(network.class));
    let trie = effective
        .iter()
        .filter(|network| network_contains(network, endpoint))
        .max_by_key(|network| network.prefix)
        .map_or(EXTERNAL, |network| u32::from(network.class));
    assert_eq!(
        declared, trie,
        "{endpoint:?}; declarations={networks:?}; trie={effective:?}"
    );
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn random_address(state: &mut u64) -> [u8; 16] {
    let mut address = [0; 16];
    address[..8].copy_from_slice(&next(state).to_be_bytes());
    address[8..].copy_from_slice(&next(state).to_be_bytes());
    address
}

#[test]
fn overlapping_tries_preserve_first_declaration_at_address_edges() {
    let mut state = 0x8317_482a_cdeb_906fu64;
    for trial in 0..128 {
        let mut networks: Vec<Network> = Vec::new();
        for index in 0..48 {
            let family = if index % 2 == 0 { AF_INET } else { AF_INET6 };
            let width = if family == AF_INET { 32 } else { 128 };
            let mut address = random_address(&mut state);
            let mut prefix = (next(&mut state) % (width + 1)) as u32;
            // Guaranteed nested, reversed, disjoint, noncanonical and duplicate prefixes.
            if index >= 2 && index % 3 != 0 {
                let parent = networks[index - 2];
                address = parent.address;
                if index % 7 == 0 {
                    prefix = parent.prefix;
                }
            }
            if index < 2 && trial % 4 == 0 {
                prefix = 0;
            }
            networks.push(Network {
                family,
                class: index as u16 + 5,
                prefix,
                address,
            });
        }
        let effective = effective_networks(&networks);
        for (index, network) in effective.iter().enumerate() {
            assert!(!effective[..index].iter().any(|other| {
                other.family == network.family
                    && other.prefix == network.prefix
                    && other.address == network.address
            }));
            let mut canonical = *network;
            canonical.class = 0;
            assert_eq!(effective_networks(&[canonical]), std::vec![canonical]);
        }
        for network in &networks {
            let base = u128::from_be_bytes(network.address);
            let mask = if network.prefix == 0 {
                0
            } else {
                u128::MAX << (128 - network.prefix)
            };
            let start = base & mask;
            let end = start | !mask;
            let neighbor = if network.prefix == 0 {
                start
            } else {
                start ^ (1 << (128 - network.prefix))
            };
            for address in [
                base,
                start,
                end,
                neighbor,
                0,
                u128::MAX,
                u128::from_be_bytes(random_address(&mut state)),
            ] {
                let endpoint = Endpoint {
                    family: network.family,
                    address: address.to_be_bytes(),
                    ..Endpoint::default()
                };
                check(&networks, &effective, &endpoint);
                if network.family == AF_INET {
                    let mut mapped = Endpoint {
                        family: AF_INET6,
                        ..Endpoint::default()
                    };
                    mapped.address[10..12].copy_from_slice(&[0xff; 2]);
                    mapped.address[12..].copy_from_slice(&endpoint.address[..4]);
                    check(&networks, &effective, &mapped);
                }
            }
        }
        for address in [
            "0.0.0.0",
            "127.0.0.0",
            "127.255.255.255",
            "128.0.0.0",
            "255.255.255.255",
            "::",
            "::1",
            "::2",
            "::127.0.0.1",
            "::ffff:0.0.0.0",
            "::ffff:127.0.0.1",
            "::ffff:255.255.255.255",
            "2001:db8::",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            let endpoint = Endpoint::from(SocketAddr::new(address.parse().unwrap(), 1));
            check(&networks, &effective, &endpoint);
        }
    }
    // Every mask boundary, including /0 and the exact full-width host prefix.
    for family in [AF_INET, AF_INET6] {
        let width = if family == AF_INET { 32 } else { 128 };
        for prefix in 0..=width {
            let network = Network {
                family,
                class: 5,
                prefix,
                address: [0xff; 16],
            };
            let effective = effective_networks(&[network]);
            for address in [[0; 16], [0xff; 16], random_address(&mut state)] {
                check(
                    &[network],
                    &effective,
                    &Endpoint {
                        family,
                        address,
                        ..Endpoint::default()
                    },
                );
            }
        }
    }
    let endpoint = Endpoint::from("1.2.3.4:80".parse::<SocketAddr>().unwrap());
    assert!(!network_contains(
        &Network {
            family: AF_INET,
            prefix: 33,
            ..Network::default()
        },
        &endpoint
    ));
    assert!(!network_contains(
        &Network {
            family: AF_UNIX,
            ..Network::default()
        },
        &endpoint
    ));
}

#[test]
fn sockaddrs_and_local_keys_are_canonical() {
    for address in [
        "127.0.0.1:8080",
        "0.0.0.0:0",
        "[::1]:443",
        "[fe80::1234%17]:99",
        "[::ffff:10.0.0.1]:42",
    ] {
        let endpoint = Endpoint::from(address.parse::<SocketAddr>().unwrap());
        assert_eq!(Endpoint::from_sockaddr(&endpoint.sockaddr()), endpoint);
        let normalized = endpoint.normalized();
        let mut in_place = endpoint;
        in_place.unix_len = 108;
        in_place.unix.fill(0xff);
        if in_place.family == AF_INET {
            in_place.address[4..].fill(0xff);
            in_place.scope = u32::MAX;
        }
        in_place.normalize();
        assert_eq!(in_place, normalized);
        assert_eq!(normalized.normalized(), normalized);
        assert_eq!(Endpoint::from_sockaddr(&normalized.sockaddr()), normalized);
    }
    let v4 = Endpoint::from("10.0.0.1:42".parse::<SocketAddr>().unwrap());
    let mapped = Endpoint::from("[::ffff:10.0.0.1]:42".parse::<SocketAddr>().unwrap());
    assert_eq!(mapped.normalized(), v4);
    let v6 = Endpoint::from("[fe80::1%17]:99".parse::<SocketAddr>().unwrap()).sockaddr();
    assert_eq!(Endpoint::from_sockaddr(&v6[..24]).scope, 0);
    assert_eq!(
        builtin_class(&Endpoint::from_sockaddr(&v6[..23])),
        Some(UNKNOWN)
    );
    assert_eq!(
        builtin_class(&Endpoint::from_sockaddr(&v4.sockaddr()[..15])),
        Some(UNKNOWN)
    );
    assert_eq!(builtin_class(&Endpoint::from_sockaddr(&[])), Some(UNKNOWN));
    assert_eq!(
        builtin_class(&Endpoint {
            family: 65535,
            ..Endpoint::default()
        }),
        Some(UNKNOWN)
    );
    for (path, expected) in [
        (&b"/run/x.sock\0padding"[..], &b"/run/x.sock"[..]),
        (&b"\0abstract\0\0"[..], &b"\0abstract\0\0"[..]),
        (&b""[..], &b""[..]),
    ] {
        let mut bytes = AF_UNIX.to_ne_bytes().to_vec();
        bytes.extend_from_slice(path);
        let endpoint = Endpoint::from_sockaddr(&bytes);
        assert_eq!(&endpoint.unix[..endpoint.unix_len as usize], expected);
        assert_eq!(Endpoint::from_sockaddr(&endpoint.sockaddr()), endpoint);
        assert_eq!(endpoint.normalized(), endpoint);
        let mut in_place = endpoint;
        in_place.port = u16::MAX;
        in_place.scope = u32::MAX;
        in_place.address.fill(0xff);
        in_place.unix[expected.len()..].fill(0xff);
        in_place.normalize();
        assert_eq!(in_place, endpoint);
        assert_eq!(builtin_class(&endpoint), Some(UNIX));
    }
    let mut full = AF_UNIX.to_ne_bytes().to_vec();
    full.extend_from_slice(&[b'x'; 108]);
    let endpoint = Endpoint::from_sockaddr(&full);
    assert_eq!(endpoint.unix_len, 108);
    assert_eq!(endpoint.sockaddr(), full);
    assert_eq!(
        builtin_class(&Endpoint::from_sockaddr(&AF_NETLINK.to_ne_bytes())),
        Some(NETLINK)
    );
    for family in [AF_UNSPEC, AF_UNIX, AF_NETLINK, 65535] {
        let mut endpoint = Endpoint {
            family,
            port: u16::MAX,
            scope: u32::MAX,
            address: [0xff; 16],
            unix_len: 109,
            unix: [0xff; 108],
        };
        endpoint.normalize();
        assert_eq!(
            endpoint,
            Endpoint {
                family: if family == AF_NETLINK { AF_NETLINK } else { AF_UNSPEC },
                ..Endpoint::default()
            }
        );
    }
}

#[test]
fn shared_abi_layout_is_fixed() {
    use core::mem::{offset_of, size_of};
    assert_eq!(
        [UNIX, NETLINK, LOOPBACK, EXTERNAL, UNKNOWN],
        [0, 1, 2, 3, 4]
    );
    assert_eq!(MAX_CLASSES, 64);
    assert_eq!(size_of::<Endpoint>(), 136);
    assert_eq!(offset_of!(Endpoint, family), 0);
    assert_eq!(offset_of!(Endpoint, port), 2);
    assert_eq!(offset_of!(Endpoint, scope), 4);
    assert_eq!(offset_of!(Endpoint, address), 8);
    assert_eq!(offset_of!(Endpoint, unix_len), 24);
    assert_eq!(offset_of!(Endpoint, unix), 28);
    assert_eq!(size_of::<Network>(), 24);
    assert_eq!(offset_of!(Network, class), 2);
    assert_eq!(offset_of!(Network, prefix), 4);
    assert_eq!(offset_of!(Network, address), 8);
    assert_eq!(size_of::<Answers>(), 40);
    assert_eq!(offset_of!(Answers, deny), 8);
    assert_eq!(offset_of!(Answers, guarded), 16);
    assert_eq!(offset_of!(Answers, local), 24);
    assert_eq!(offset_of!(Answers, stale), 32);
    assert_eq!(offset_of!(Answers, audit), 36);
    assert_eq!(size_of::<Contact>(), 168);
    assert_eq!(offset_of!(Contact, tid), 4);
    assert_eq!(offset_of!(Contact, root), 8);
    assert_eq!(offset_of!(Contact, generation), 16);
    assert_eq!(offset_of!(Contact, endpoint), 24);
    assert_eq!(offset_of!(Contact, class), 160);
    assert_eq!(offset_of!(Contact, decision), 164);
    assert_eq!(size_of::<LocalKey>(), 144);
    assert_eq!(offset_of!(LocalKey, endpoint), 8);
}
