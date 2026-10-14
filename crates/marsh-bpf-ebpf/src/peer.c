/*
 * Only these kernel fields are described. preserve_access_index emits real
 * .BTF.ext CO-RE relocations, applied by Aya against the running kernel BTF.
 * No generated host-specific vmlinux.h or guessed kernel offsets are used.
 */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;

#define CORE __attribute__((preserve_access_index))
struct in6_addr { u8 bytes[16]; } CORE;
struct sock_common {
    u32 skc_daddr;
    u16 skc_dport;
    u16 skc_family;
    int skc_bound_dev_if;
    struct in6_addr skc_v6_daddr;
} CORE;
struct sock { struct sock_common __sk_common; } CORE;
struct socket { struct sock *sk; } CORE;
struct sockaddr_un { u16 sun_family; u8 sun_path[108]; };
struct unix_address { int len; struct sockaddr_un name[1]; } CORE;
struct unix_sock {
    struct sock sk;
    struct unix_address *addr;
    struct sock *peer;
} CORE;
struct msghdr { void *msg_name; int msg_namelen; } CORE;

/* ABI mirrored from marsh-peer-class; the Rust side asserts its size. */
struct endpoint {
    u16 family;
    u16 port;
    u32 scope;
    u8 address[16];
    u32 unix_len;
    u8 unix_path[108];
};

static long (*const probe_read)(void *, u32, const void *) = (void *)113;
#define READ(dst, field) probe_read(&(dst), sizeof(dst), __builtin_preserve_access_index(&(field)))

static u16 host_port(u16 port)
{
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    return __builtin_bswap16(port);
#else
    return port;
#endif
}

int marsh_disconnect(const void *name, int len)
{
    u16 family = 0;
    return name && len >= 2 && !probe_read(&family, 2, name) && family == 0;
}

/* sockaddr is a stable userspace ABI. All pointers here are kernel copies. */
int marsh_sockaddr(const void *name, int len, struct endpoint *out)
{
    const u8 *bytes = name;
    u16 family = 0;
    if (!name || len < 2 || probe_read(&family, 2, bytes))
        return -1;
    out->family = family;
    if (family == 1) {
        if (len > 110)
            return -1;
        u32 n = (u32)(len - 2);
        if (n > 108)
            return -1;
        out->unix_len = n;
        if (n && probe_read(out->unix_path, n, bytes + 2))
            return -1;
        return 0;
    }
    if (family == 16)
        return len >= 12 ? 0 : -1;
    if (family == 2) {
        if (len < 16 || probe_read(&out->port, 2, bytes + 2) ||
            probe_read(out->address, 4, bytes + 4))
            return -1;
        out->port = host_port(out->port);
        return 0;
    }
    if (family == 10) {
        if (len < 24 || probe_read(&out->port, 2, bytes + 2) ||
            probe_read(out->address, 16, bytes + 8) ||
            (len >= 28 && probe_read(&out->scope, 4, bytes + 24)))
            return -1;
        out->port = host_port(out->port);
        return 0;
    }
    return 0; /* Successfully read, unsupported family: ordinary UNKNOWN class. */
}

int marsh_message_name(const struct msghdr *message, const void **name, int *len)
{
    if (!message || READ(*name, message->msg_name) || READ(*len, message->msg_namelen))
        return -1;
    return 0;
}

int marsh_socket_peer(const struct socket *socket, struct endpoint *out)
{
    struct sock *sk = 0;
    u16 family = 0;
    if (!socket || READ(sk, socket->sk) || !sk ||
        READ(family, sk->__sk_common.skc_family))
        return -1;
    out->family = family;
    if (family == 16)
        return 0;
    if (family == 2 || family == 10) {
        if (READ(out->port, sk->__sk_common.skc_dport))
            return -1;
        out->port = host_port(out->port);
        if (family == 2)
            return READ(*(u32 *)out->address, sk->__sk_common.skc_daddr) ? -1 : 0;
        if (probe_read(out->address, 16,
            __builtin_preserve_access_index(&sk->__sk_common.skc_v6_daddr)))
            return -1;
        /* Match ipv6_iface_scope_id(): link-local unicast or node/link multicast. */
        if ((out->address[0] == 0xfe && (out->address[1] & 0xc0) == 0x80) ||
            (out->address[0] == 0xff &&
             ((out->address[1] & 0x0f) == 1 || (out->address[1] & 0x0f) == 2))) {
            if (READ(out->scope, sk->__sk_common.skc_bound_dev_if))
                return -1;
        }
        return 0;
    }
    if (family == 1) {
        struct sock *peer = 0;
        struct unix_address *address = 0;
        int len = 0;
        if (READ(peer, ((struct unix_sock *)sk)->peer) || !peer ||
            READ(address, ((struct unix_sock *)peer)->addr))
            return -1;
        /* An unnamed socketpair has a real UNIX peer, but no bound name. */
        if (!address)
            return 0;
        if (READ(len, address->len))
            return -1;
        return marsh_sockaddr(__builtin_preserve_access_index(&address->name[0]), len, out);
    }
    return 0; /* Known socket family, but no class-specific endpoint ABI. */
}
