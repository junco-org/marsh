// SPDX-License-Identifier: GPL-2.0
/*
 * marsh's BPF LSM programs: agent-policy socket answers enforced against the kernel's own
 * address and socket, secret taint carried by local sockets to the processes that read them, and
 * refusal of the operations by which governed code could escape governance or reach another
 * domain's processes.
 *
 * A domain is a cgroup v2 that userspace maps to a root in DOMAINS. A task is governed while its
 * cgroup is a domain: membership is inherited by fork and io_uring workers inside the kernel,
 * survives PID namespaces, and cannot be left by a non-root task.
 *
 * Kernel structs below describe only the fields used. preserve_access_index makes every access a
 * CO-RE relocation, applied by the loader against the running kernel's BTF; no generated
 * vmlinux.h or guessed offsets. Pointers that BPF helpers must see as BTF-typed (sockets passed to
 * socket storage) are loaded directly; everything else goes through bpf_probe_read_kernel.
 */
#include "marsh.h"

#define __always_inline inline __attribute__((always_inline))
#define SEC(name) __attribute__((section(name), used))
#define CORE __attribute__((preserve_access_index))
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name

#define EPERM 1
#define SOCK_STREAM 1
#define SOCK_DGRAM 2
#define TCP_CLOSE 7

/* include/uapi/linux/prctl.h */
#define PR_SET_MM 35
#define PR_SET_PTRACER 0x59616d61
/* include/uapi/asm-generic/ioctls.h, also powerpc's; alpha, mips and sparc number them otherwise. */
#define TIOCSTI 0x5412
#define TIOCLINUX 0x541C
/* include/linux/key.h */
#define KEY_ALLOC_UID_KEYRING 0x10
#define KEY_NEED_WRITE 3
#define KEY_NEED_LINK 5
#define KEY_NEED_SETATTR 6

#define MAP_HASH 1
#define MAP_ARRAY 2
#define MAP_LRU_HASH 9
#define MAP_LPM_TRIE 11
#define MAP_SK_STORAGE 24
#define MAP_RINGBUF 27
#define NO_PREALLOC 1
#define NOEXIST 1
#define STORAGE_CREATE 1

struct bpf_pidns_info {
    __u32 pid;
    __u32 tgid;
};

static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value, __u64 flags) = (void *)2;
static long (*bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static __u64 (*bpf_ktime_get_ns)(void) = (void *)5;
static __u64 (*bpf_get_current_cgroup_id)(void) = (void *)80;
static void *(*bpf_sk_storage_get)(void *map, void *sk, void *value, __u64 flags) = (void *)107;
static long (*bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;
static long (*bpf_get_ns_current_pid_tgid)(__u64 dev, __u64 ino, struct bpf_pidns_info *ids, __u32 size) = (void *)120;
static void *(*bpf_ringbuf_reserve)(void *ring, __u64 size, __u64 flags) = (void *)131;
static void (*bpf_ringbuf_submit)(void *data, __u64 flags) = (void *)132;

struct in6_addr { __u8 bytes[16]; } CORE;
struct net { __u64 net_cookie; } CORE;
typedef struct { struct net *net; } CORE possible_net_t;
struct sock_common {
    __u32 skc_daddr;
    __u32 skc_rcv_saddr;
    __u16 skc_dport;
    __u16 skc_num;
    __u16 skc_family;
    unsigned char skc_state;
    unsigned char skc_reuse:4;
    unsigned char skc_reuseport:1;
    unsigned char skc_ipv6only:1;
    int skc_bound_dev_if;
    possible_net_t skc_net;
    struct in6_addr skc_v6_daddr;
    struct in6_addr skc_v6_rcv_saddr;
} CORE;
struct sock {
    struct sock_common __sk_common;
    __u16 sk_type;
} CORE;
struct socket { struct sock *sk; } CORE;
struct file;
struct sockaddr_un { __u16 sun_family; __u8 sun_path[108]; };
struct unix_address { int len; struct sockaddr_un name[1]; } CORE;
struct unix_sock {
    struct sock sk;
    struct unix_address *addr;
    struct sock *peer;
} CORE;
struct msghdr { void *msg_name; int msg_namelen; } CORE;

/* A task's cgroup v2 id, as bpf_get_current_cgroup_id() computes it for the current task. */
struct kernfs_node { __u64 id; } CORE;
struct cgroup { struct kernfs_node *kn; } CORE;
struct css_set { struct cgroup *dfl_cgrp; } CORE;
struct task_struct { struct css_set *cgroups; } CORE;

static struct socket *(*bpf_sock_from_file)(struct file *file) = (void *)162;
static struct unix_sock *(*bpf_skc_to_unix_sock)(void *sk) = (void *)178;

#define READ(dst, field) bpf_probe_read_kernel(&(dst), sizeof(dst), __builtin_preserve_access_index(&(field)))

/* A CO-RE bitfield through a raw read, as libbpf's BPF_CORE_READ_BITFIELD_PROBED (unsigned). */
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
#define BITFIELD_INTO(size) 0
#else
#define BITFIELD_INTO(size) (8 - (size))
#endif
#define BITFIELD(base, field) ({                                                                 \
    __u64 bits = 0;                                                                              \
    __u32 size = __builtin_preserve_field_info((base)->field, 1);                                \
    bpf_probe_read_kernel((__u8 *)&bits + BITFIELD_INTO(size), size,                             \
                          (const __u8 *)(base) + __builtin_preserve_field_info((base)->field, 0)); \
    bits <<= __builtin_preserve_field_info((base)->field, 4);                                    \
    bits >> __builtin_preserve_field_info((base)->field, 5);                                     \
})

/*
 * Domains: a cgroup v2 id (its kernfs node id, the directory's inode number on 64-bit kernels)
 * and the root it belongs to. Every task in the cgroup is governed as that root.
 */
struct {
    __uint(type, MAP_HASH);
    __uint(max_entries, 4096);
    __uint(map_flags, NO_PREALLOC);
    __type(key, __u64);
    __type(value, __u64);
} DOMAINS SEC(".maps");

/*
 * Keys a domain allocated, by kernel address: their root. A key's serial is assigned only after
 * the key_alloc hook, so the address is the identity. No LSM hook sees a key freed: a stale entry
 * stays until key_alloc reuses its address, rewriting the entry before the new key is usable, or
 * the LRU evicts it. Evicting a live domain key only fails closed: its domain can no longer
 * modify it.
 */
struct {
    __uint(type, MAP_LRU_HASH);
    __uint(max_entries, 16384);
    __type(key, __u64);
    __type(value, __u64);
} KEYS SEC(".maps");

/* Each root's published answers; replaced whole by userspace, RCU-safe with NO_PREALLOC. */
struct {
    __uint(type, MAP_HASH);
    __uint(max_entries, 4096);
    __uint(map_flags, NO_PREALLOC);
    __type(key, __u64);
    __type(value, struct marsh_answers);
} ANSWERS SEC(".maps");

/* Each root's taint receipts, counted by the kernel only. */
struct {
    __uint(type, MAP_HASH);
    __uint(max_entries, 4096);
    __uint(map_flags, NO_PREALLOC);
    __type(key, __u64);
    __type(value, struct marsh_receipts);
} RECEIVED SEC(".maps");

struct net4_key { __u32 prefix; __u8 data[12]; };
struct net6_key { __u32 prefix; __u8 data[24]; };

/* Root-prefixed declared networks; userspace stores first-declaration classes per prefix. */
struct {
    __uint(type, MAP_LPM_TRIE);
    __uint(max_entries, 65536);
    __uint(map_flags, NO_PREALLOC);
    __type(key, struct net4_key);
    __type(value, __u32);
} CLASS_NET4 SEC(".maps");

struct {
    __uint(type, MAP_LPM_TRIE);
    __uint(max_entries, 65536);
    __uint(map_flags, NO_PREALLOC);
    __type(key, struct net6_key);
    __type(value, __u32);
} CLASS_NET6 SEC(".maps");

/* IP endpoints held by traced processes; wildcard binds have a zero address. */
struct {
    __uint(type, MAP_HASH);
    __uint(max_entries, 65536);
    __uint(map_flags, NO_PREALLOC);
    __type(key, struct marsh_endpoint);
    __type(value, __u32);
} LOCAL_ALLOWED SEC(".maps");

/* Per-socket state: whether a governed process created it, and taint sent into it. */
struct sock_state {
    __u64 taint;
    __u64 sender;
    __u32 traced;
    __u32 unused;
};

struct {
    __uint(type, MAP_SK_STORAGE);
    __uint(map_flags, NO_PREALLOC);
    __type(key, int);
    __type(value, struct sock_state);
} SOCKS SEC(".maps");

/*
 * Taint sent to an IP receiver, keyed as the receiving socket sees itself: a TCP connection by
 * its peer's address and port, anything else by its local port and the destination's family
 * (zero for a connection). An IPv4 destination reaches IPv4 and dual-stack IPv6 sockets of the
 * port; an IPv6 one only IPv6 sockets.
 */
struct flow {
    __u64 net;
    __u16 type;
    __u16 port;
    __u16 peer_port;
    __u16 family;
    __u8 peer[16];
};

struct tag {
    __u64 taint;
    __u64 sender;
};

struct {
    __uint(type, MAP_HASH);
    __uint(max_entries, 65536);
    __uint(map_flags, NO_PREALLOC);
    __type(key, struct flow);
    __type(value, struct tag);
} FLOWS SEC(".maps");

struct {
    __uint(type, MAP_RINGBUF);
    __uint(max_entries, 1 << 22);
} EVENTS SEC(".maps");

/*
 * Sticky: event loss or a userspace map failure refuses every governed contact. Ungoverned
 * processes are checked before this switch.
 */
struct {
    __uint(type, MAP_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u64);
} HEALTH SEC(".maps");

struct pid_namespace { __u64 dev; __u64 ino; };

struct {
    __uint(type, MAP_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct pid_namespace);
} PID_NAMESPACE SEC(".maps");

char LICENSE[] SEC("license") = "GPL";

/*
 * The current task's ids in the loader's PID namespace, for reporting only: zero for a task whose
 * active PID namespace is another one. Governance never depends on them.
 */
static __always_inline void current_ids(struct bpf_pidns_info *ids)
{
    __u32 zero = 0;
    struct pid_namespace *namespace = bpf_map_lookup_elem(&PID_NAMESPACE, &zero);
    if (!namespace || bpf_get_ns_current_pid_tgid(namespace->dev, namespace->ino, ids, sizeof(*ids))) {
        ids->pid = 0;
        ids->tgid = 0;
    }
}

/* The current task's root, when its cgroup is a domain. */
static __always_inline __u64 *governed(void)
{
    __u64 cgroup = bpf_get_current_cgroup_id();
    return bpf_map_lookup_elem(&DOMAINS, &cgroup);
}

static __always_inline volatile __u64 *health(void)
{
    __u32 zero = 0;
    return bpf_map_lookup_elem(&HEALTH, &zero);
}

static __always_inline int poisoned(void)
{
    volatile __u64 *state = health();
    return !state || *state;
}

static __always_inline void poison(void)
{
    volatile __u64 *state = health();
    if (state)
        *state = 1;
}

static __always_inline struct marsh_event *reserve(__u32 kind, __u64 root)
{
    struct marsh_event *event = bpf_ringbuf_reserve(&EVENTS, sizeof(*event), 0);
    if (!event) {
        poison();
        return 0;
    }
    struct bpf_pidns_info ids;
    current_ids(&ids);
    __builtin_memset(event, 0, sizeof(*event));
    event->kind = kind;
    event->tgid = ids.tgid;
    event->tid = ids.pid;
    event->root = root;
    event->time = bpf_ktime_get_ns();
    return event;
}

static __always_inline __u16 host_port(__u16 port)
{
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    return __builtin_bswap16(port);
#else
    return port;
#endif
}

/* A kernel copy of a sockaddr. A readable unsupported family is the ordinary UNKNOWN class. */
static __always_inline int read_sockaddr(const void *name, int len, struct marsh_endpoint *out)
{
    const __u8 *bytes = name;
    __u16 family = 0;
    if (!name || len < 2 || bpf_probe_read_kernel(&family, 2, bytes))
        return -1;
    out->family = family;
    if (family == AF_UNIX) {
        __u32 n = (__u32)len - 2;
        if (len > 110)
            return -1;
        out->unix_len = n;
        if (n && bpf_probe_read_kernel(out->unix_path, n & 127, bytes + 2))
            return -1;
        return 0;
    }
    if (family == AF_NETLINK)
        return len >= 12 ? 0 : -1;
    if (family == AF_INET) {
        if (len < 16 || bpf_probe_read_kernel(&out->port, 2, bytes + 2) ||
            bpf_probe_read_kernel(out->address, 4, bytes + 4))
            return -1;
        out->port = host_port(out->port);
        return 0;
    }
    if (family == AF_INET6) {
        if (len < 24 || bpf_probe_read_kernel(&out->port, 2, bytes + 2) ||
            bpf_probe_read_kernel(out->address, 16, bytes + 8) ||
            (len >= 28 && bpf_probe_read_kernel(&out->scope, 4, bytes + 24)))
            return -1;
        out->port = host_port(out->port);
        return 0;
    }
    return 0;
}

/*
 * The bound name of a UNIX socket, read through a raw (probe-read) pointer: its unix_sock fields
 * lie beyond struct sock. An unnamed one (a socketpair end) reads as empty.
 */
static __always_inline int unix_name(void *sk, struct marsh_endpoint *out)
{
    struct unix_address *address = 0;
    int len = 0;
    out->family = AF_UNIX;
    if (READ(address, ((struct unix_sock *)sk)->addr))
        return -1;
    if (!address)
        return 0;
    if (READ(len, address->len))
        return -1;
    return read_sockaddr(__builtin_preserve_access_index(&address->name[0]), len, out);
}

/* An IP socket's peer or local address, normalized like Endpoint addresses. */
static __always_inline int inet_address(struct sock *sk, int local, __u8 out[16])
{
    __u16 family = 0;
    if (READ(family, sk->__sk_common.skc_family))
        return -1;
    if (family == AF_INET) {
        __u32 address = 0;
        if (local ? READ(address, sk->__sk_common.skc_rcv_saddr) : READ(address, sk->__sk_common.skc_daddr))
            return -1;
        __builtin_memcpy(out, &address, 4);
        return 0;
    }
    if (family == AF_INET6) {
        struct marsh_endpoint e = {};
        e.family = AF_INET6;
        if (bpf_probe_read_kernel(e.address, 16, local ?
                __builtin_preserve_access_index(&sk->__sk_common.skc_v6_rcv_saddr) :
                __builtin_preserve_access_index(&sk->__sk_common.skc_v6_daddr)))
            return -1;
        __u8 v4[4];
        if (marsh_ipv4(&e, v4)) {
            __builtin_memcpy(out, v4, 4);
            return 0;
        }
        __builtin_memcpy(out, e.address, 16);
        return 0;
    }
    return -1;
}

/* A connected socket's peer: the destination of a send that names none. `peer` is raw. */
static __always_inline int socket_peer(struct sock *sk, void *peer, struct marsh_endpoint *out)
{
    __u16 family = 0;
    if (READ(family, sk->__sk_common.skc_family))
        return -1;
    out->family = family;
    if (family == AF_INET || family == AF_INET6) {
        if (READ(out->port, sk->__sk_common.skc_dport))
            return -1;
        out->port = host_port(out->port);
        if (family == AF_INET)
            return READ(*(__u32 *)out->address, sk->__sk_common.skc_daddr) ? -1 : 0;
        if (bpf_probe_read_kernel(out->address, 16, __builtin_preserve_access_index(&sk->__sk_common.skc_v6_daddr)))
            return -1;
        /* Match ipv6_iface_scope_id(): link-local unicast or node/link multicast. */
        if ((out->address[0] == 0xfe && (out->address[1] & 0xc0) == 0x80) ||
            (out->address[0] == 0xff && ((out->address[1] & 0x0f) == 1 || (out->address[1] & 0x0f) == 2)))
            return READ(out->scope, sk->__sk_common.skc_bound_dev_if) ? -1 : 0;
        return 0;
    }
    if (family == AF_UNIX)
        return peer ? unix_name(peer, out) : -1;
    return 0;
}

static __always_inline __u32 classify(__u64 root, const struct marsh_endpoint *endpoint)
{
    __s32 builtin = marsh_builtin_class(endpoint);
    if (builtin >= 0)
        return builtin;
    if (endpoint->family == AF_INET) {
        struct net4_key key = { .prefix = 96 };
        __builtin_memcpy(key.data, &root, 8);
        __builtin_memcpy(key.data + 8, endpoint->address, 4);
        __u32 *class = bpf_map_lookup_elem(&CLASS_NET4, &key);
        return class ? *class : CLASS_EXTERNAL;
    }
    struct net6_key key = { .prefix = 192 };
    __builtin_memcpy(key.data, &root, 8);
    __builtin_memcpy(key.data + 8, endpoint->address, 16);
    __u32 *class = bpf_map_lookup_elem(&CLASS_NET6, &key);
    return class ? *class : CLASS_EXTERNAL;
}

/*
 * Taint classes received in the kernel that the published answers do not include yet. Branchless:
 * a constant mask built from branches would make the verifier walk every combination.
 */
static __always_inline __u64 pending(const struct marsh_answers *answers, __u64 root)
{
    struct marsh_receipts *receipts = bpf_map_lookup_elem(&RECEIVED, &root);
    __u64 classes = 0;
    if (!receipts)
        return 0;
    for (int i = 0; i < MAX_TAINTS; i++) {
        __u64 difference = receipts->count[i] ^ answers->acknowledged[i];
        /* Hide the 32-bit range, or the compiler turns this back into a branch. */
        asm volatile("" : "+r"(difference));
        classes |= ((0 - difference) >> 63) << i;
    }
    return classes & ~answers->carried;
}

/* Whether a traced process holds an IP endpoint; a wildcard bind holds every local address. */
static __always_inline int traced_endpoint(struct marsh_endpoint *endpoint)
{
    if (bpf_map_lookup_elem(&LOCAL_ALLOWED, endpoint))
        return 1;
    /* Never cross families: an IPv6 wildcard may be IPV6_V6ONLY. */
    struct marsh_endpoint wildcard = *endpoint;
    __builtin_memset(wildcard.address, 0, 16);
    wildcard.scope = 0;
    return bpf_map_lookup_elem(&LOCAL_ALLOWED, &wildcard) != 0;
}

static __always_inline int traced_socket(struct sock *sk)
{
    struct sock_state *state = sk ? bpf_sk_storage_get(&SOCKS, sk, 0, 0) : 0;
    return state && state->traced;
}

/* A UNIX stream's receiver exists only once connected: its connect defers the receiver check. */
#define RECEIVER_LATER 1

/*
 * Judges a contact already described in `event`, returning the hook's verdict. `unreadable` means
 * the address could not be read, so the contact may be of any class. `receiver` is the receiving
 * UNIX socket, null when unknown.
 */
static __always_inline int judge(struct marsh_event *event, int unreadable, struct sock *receiver, int flags)
{
    struct marsh_answers *answers = bpf_map_lookup_elem(&ANSWERS, &event->root);
    if (!answers || event->class >= MAX_CLASSES) {
        event->decision = DECISION_DENY;
        event->reason = REASON_UNANSWERED;
        return -EPERM;
    }
    event->generation = answers->generation;
    __u64 received = pending(answers, event->root);
    event->taint = answers->carried | received;
    __u64 deny = answers->deny;
    event->reason = REASON_RULE;
    if (answers->stale || (received & (received - 1))) {
        /* Undecidable until userspace republishes: refuse whatever any rule could. */
        deny = answers->guarded;
        event->reason = REASON_STALE;
    } else if (received) {
        /* One class: the index of its bit, bounded where the verifier can see it. */
        __u64 index = __builtin_popcountll(received - 1);
        asm volatile("%0 &= 15" : "+r"(index));
        deny = answers->contingent[index];
        event->reason = REASON_CONTINGENT;
    }
    __u64 bit = 1ULL << event->class;
    int denied = unreadable ? deny != 0 : (deny & bit) != 0;
    if (!denied && event->taint && (unreadable ? answers->untraced : answers->untraced & bit) &&
        !(event->class == CLASS_UNIX && (flags & RECEIVER_LATER))) {
        int traced = 0;
        if (!unreadable)
            traced = event->class == CLASS_UNIX ? traced_socket(receiver) : traced_endpoint(&event->endpoint);
        if (!traced) {
            denied = 1;
            event->reason = REASON_UNTRACED;
        }
    }
    if (!denied) {
        event->decision = DECISION_ALLOW;
        return 0;
    }
    event->decision = answers->audit ? DECISION_AUDIT : DECISION_DENY;
    return answers->audit ? 0 : -EPERM;
}

/* Refuses a permitted contact whose taint could not be recorded at its receiver. */
static __always_inline int untagged(struct marsh_event *event)
{
    event->decision = DECISION_DENY;
    event->reason = REASON_UNTAGGED;
    return -EPERM;
}

/* Records taint a permitted local contact sends into its receiving socket. */
static __always_inline int tag_unix(struct sock *receiver, __u64 taint, __u64 root)
{
    struct sock_state *state = receiver ? bpf_sk_storage_get(&SOCKS, receiver, 0, STORAGE_CREATE) : 0;
    if (!state)
        return -1;
    __sync_fetch_and_or(&state->taint, taint);
    state->sender = root;
    return 0;
}

static __always_inline __u64 net_cookie(struct sock *sk)
{
    struct net *net = 0;
    __u64 cookie = 0;
    if (READ(net, sk->__sk_common.skc_net.net) || !net || READ(cookie, net->net_cookie))
        return 0;
    return cookie;
}

/* Full: refuse the contact, since an untagged receiver would read untainted. */
static __always_inline int tag_flow(struct flow *key, __u64 taint, __u64 root)
{
    struct tag *tag = bpf_map_lookup_elem(&FLOWS, key);
    if (!tag) {
        struct tag fresh = { .taint = taint, .sender = root };
        if (!bpf_map_update_elem(&FLOWS, key, &fresh, NOEXIST))
            return 0;
        tag = bpf_map_lookup_elem(&FLOWS, key);
        if (!tag)
            return -1;
    }
    __sync_fetch_and_or(&tag->taint, taint);
    tag->sender = root;
    return 0;
}

/* A loopback send: tag the receiver's connection, or its whole port for datagrams. */
static __always_inline int tag_inet(struct sock *sk, const struct marsh_endpoint *destination, __u64 taint, __u64 root)
{
    struct flow key = {};
    __u16 type = 0, port = 0;
    if (READ(type, sk->sk_type) || READ(port, sk->__sk_common.skc_num))
        return -1;
    key.net = net_cookie(sk);
    key.type = type;
    key.port = destination->port;
    if (type == SOCK_STREAM && port && !inet_address(sk, 1, key.peer)) {
        key.peer_port = port;
    } else {
        __builtin_memset(key.peer, 0, 16);
        key.family = destination->family;
    }
    return tag_flow(&key, taint, root);
}

static __always_inline void submit(struct marsh_event *event)
{
    if (poisoned()) {
        event->decision = DECISION_DENY;
        event->reason = REASON_STALE;
    }
    bpf_ringbuf_submit(event, 0);
}

SEC("lsm/socket_connect")
int socket_connect(unsigned long long *ctx)
{
    const void *address = (void *)ctx[1];
    int len = ctx[2], previous = ctx[3];
    __u16 family = 0;
    if (previous)
        return previous;
    /* AF_UNSPEC disconnects a socket: no peer contact or observation exists. */
    if (address && len >= 2 && !bpf_probe_read_kernel(&family, 2, address) && family == AF_UNSPEC)
        return 0;
    __u64 *root = governed();
    if (!root)
        return 0;
    if (poisoned())
        return -EPERM;
    struct marsh_event *event = reserve(EVENT_CONTACT, *root);
    if (!event)
        return -EPERM;
    int unreadable = read_sockaddr(address, len, &event->endpoint) != 0;
    if (unreadable)
        __builtin_memset(&event->endpoint, 0, sizeof(event->endpoint));
    marsh_normalize_ip(&event->endpoint);
    event->class = classify(event->root, &event->endpoint);
    int result = judge(event, unreadable, 0, RECEIVER_LATER);
    submit(event);
    return poisoned() ? -EPERM : result;
}

/*
 * Whether a send's named address is where its bytes go: 1 yes, 0 no, -1 unknown. A connected TCP
 * socket ignores a name and a UNIX stream or seqpacket socket refuses or drops it, so naming
 * another address must not change the contact's class; only an unconnected TCP fast-open send
 * connects to the name it carries.
 */
static __always_inline int names_destination(struct sock *sk, __u16 family, __u16 type)
{
    if (family == AF_UNIX)
        return 0;
    if (type != SOCK_STREAM)
        return 1;
    __u8 state = 0;
    if (READ(state, sk->__sk_common.skc_state))
        return -1;
    return state == TCP_CLOSE;
}

SEC("lsm/socket_sendmsg")
int socket_sendmsg(unsigned long long *ctx)
{
    struct socket *sock = (void *)ctx[0];
    struct msghdr *message = (void *)ctx[1];
    int previous = ctx[3];
    if (previous)
        return previous;
    __u64 *root = governed();
    if (!root)
        return 0;
    if (poisoned())
        return -EPERM;
    struct sock *sk = sock->sk;
    if (!sk)
        return -EPERM;
    __u16 family = sk->__sk_common.skc_family;
    __u16 type = sk->sk_type;
    /* UNIX datagrams are judged at unix_may_send, which knows the receiving socket. */
    if (family == AF_UNIX && type == SOCK_DGRAM)
        return 0;
    struct marsh_event *event = reserve(EVENT_CONTACT, *root);
    if (!event)
        return -EPERM;
    /* The receiving UNIX socket: typed for socket storage, raw for reading its name. */
    struct sock *peer = 0;
    void *peer_name = 0;
    if (family == AF_UNIX) {
        struct unix_sock *unix_socket = bpf_skc_to_unix_sock(sk);
        if (unix_socket) {
            peer = unix_socket->peer;
            READ(peer_name, unix_socket->peer);
        }
    }
    const void *name = 0;
    int len = 0, unreadable;
    int named = names_destination(sk, family, type);
    if (named < 0 || (named && (READ(name, message->msg_name) || READ(len, message->msg_namelen))))
        unreadable = 1;
    else if (name)
        unreadable = read_sockaddr(name, len, &event->endpoint) != 0;
    else
        unreadable = socket_peer(sk, peer_name, &event->endpoint) != 0;
    if (unreadable)
        __builtin_memset(&event->endpoint, 0, sizeof(event->endpoint));
    marsh_normalize_ip(&event->endpoint);
    event->class = classify(event->root, &event->endpoint);
    int result = judge(event, unreadable, peer, 0);
    if (event->decision != DECISION_DENY && event->taint) {
        int failed = 0;
        if (event->class == CLASS_UNIX && peer)
            failed = tag_unix(peer, event->taint, event->root);
        else if (event->class == CLASS_LOOPBACK)
            failed = tag_inet(sk, &event->endpoint, event->taint, event->root);
        if (failed)
            result = untagged(event);
    }
    submit(event);
    return poisoned() ? -EPERM : result;
}

/* Every UNIX datagram send, after the kernel found its receiving socket. */
SEC("lsm/unix_may_send")
int unix_may_send(unsigned long long *ctx)
{
    struct socket *other = (void *)ctx[1];
    int previous = ctx[2];
    if (previous)
        return previous;
    __u64 *root = governed();
    if (!root)
        return 0;
    if (poisoned())
        return -EPERM;
    struct marsh_event *event = reserve(EVENT_CONTACT, *root);
    if (!event)
        return -EPERM;
    struct sock *receiver = other->sk;
    void *receiver_name = 0;
    READ(receiver_name, other->sk);
    if (!receiver_name || unix_name(receiver_name, &event->endpoint))
        __builtin_memset(&event->endpoint, 0, sizeof(event->endpoint));
    event->endpoint.family = AF_UNIX;
    marsh_normalize_ip(&event->endpoint);
    event->class = CLASS_UNIX;
    int result = judge(event, 0, receiver, 0);
    if (event->decision != DECISION_DENY && event->taint && receiver &&
        tag_unix(receiver, event->taint, event->root))
        result = untagged(event);
    submit(event);
    return poisoned() ? -EPERM : result;
}

/* A governed process's UNIX socket is traced; kernel and untraced sockets are not. */
SEC("lsm/socket_post_create")
int socket_post_create(unsigned long long *ctx)
{
    struct socket *sock = (void *)ctx[0];
    int family = ctx[1], kern = ctx[4], previous = ctx[5];
    if (previous)
        return previous;
    if (family != AF_UNIX || kern || !governed())
        return 0;
    struct sock *sk = sock->sk;
    struct sock_state *state = sk ? bpf_sk_storage_get(&SOCKS, sk, 0, STORAGE_CREATE) : 0;
    /* Unmarked, it would count as untraced; refusing the socket keeps the rest running. */
    if (!state)
        return -EPERM;
    state->traced = 1;
    return 0;
}

/* The server end of a UNIX stream connection belongs to its listener's owner. */
SEC("lsm/unix_stream_connect")
int unix_stream_connect(unsigned long long *ctx)
{
    struct sock *other = (void *)ctx[1];
    struct sock *server = (void *)ctx[2];
    int previous = ctx[3];
    if (previous)
        return previous;
    if (!traced_socket(other))
        return 0;
    struct sock_state *state = bpf_sk_storage_get(&SOCKS, server, 0, STORAGE_CREATE);
    if (!state)
        return -EPERM;
    state->traced = 1;
    return 0;
}

/* Ors in a port's datagram (or unbound-sender) tag for destinations of `family`. */
static __always_inline void port_tag(struct flow *key, __u16 family, __u64 *taint, __u64 *sender)
{
    key->family = family;
    struct tag *tag = bpf_map_lookup_elem(&FLOWS, key);
    if (tag) {
        *taint |= tag->taint;
        *sender = tag->sender;
    }
}

/* A governed reader of a tagged socket receives its taint before it can act on the bytes. */
static __always_inline int receive(struct sock *sk)
{
    __u64 *root = governed();
    if (!root || !sk)
        return 0;
    __u16 family = sk->__sk_common.skc_family;
    __u64 taint = 0, sender = 0;
    if (family == AF_UNIX) {
        struct sock_state *state = bpf_sk_storage_get(&SOCKS, sk, 0, 0);
        if (state) {
            taint = state->taint;
            sender = state->sender;
        }
    } else if (family == AF_INET || family == AF_INET6) {
        struct flow key = {};
        key.net = net_cookie(sk);
        key.type = sk->sk_type;
        key.port = sk->__sk_common.skc_num;
        struct tag *tag = 0;
        if (key.type == SOCK_STREAM && !inet_address(sk, 0, key.peer)) {
            key.peer_port = host_port(sk->__sk_common.skc_dport);
            tag = bpf_map_lookup_elem(&FLOWS, &key);
        }
        if (tag) {
            taint = tag->taint;
            sender = tag->sender;
        } else {
            key.peer_port = 0;
            __builtin_memset(key.peer, 0, 16);
            port_tag(&key, family, &taint, &sender);
            /* A dual-stack socket also receives what was sent to the port's IPv4 address. */
            if (family == AF_INET6 && !BITFIELD(sk, __sk_common.skc_ipv6only))
                port_tag(&key, AF_INET, &taint, &sender);
        }
    }
    if (!taint)
        return 0;
    __u64 key = *root;
    struct marsh_answers *answers = bpf_map_lookup_elem(&ANSWERS, &key);
    __u64 known = answers ? answers->carried | pending(answers, key) : 0;
    __u64 fresh = taint & ~known;
    if (!fresh)
        return 0;
    struct marsh_receipts *receipts = bpf_map_lookup_elem(&RECEIVED, &key);
    if (!receipts) {
        struct marsh_receipts zero = {};
        bpf_map_update_elem(&RECEIVED, &key, &zero, NOEXIST);
        receipts = bpf_map_lookup_elem(&RECEIVED, &key);
    }
    struct marsh_event *event = receipts ? reserve(EVENT_RECEIPT, key) : 0;
    if (!event) {
        poison();
        return -EPERM;
    }
    for (int i = 0; i < MAX_TAINTS; i++)
        __sync_fetch_and_add(&receipts->count[i], (fresh >> i) & 1);
    event->taint = fresh;
    event->sender = sender;
    bpf_ringbuf_submit(event, 0);
    return 0;
}

static __always_inline int read_returned(struct socket *sock, long received)
{
    if (received > 0)
        receive(sock->sk);
    return 0;
}

/*
 * After a socket read returns data and before the reader regains control. Judging at the read's
 * entry instead would miss a reader already blocked when the tainted bytes arrive. These are the
 * protocol entries of every read of a taggable socket: recv*, read() (sock_read_iter), io_uring
 * receives, and each message of recvmmsg, whose later messages bypass sock_recvmsg. Splice from a
 * socket has its own path. A seqpacket read also enters unix_dgram_recvmsg unless inlined; a second
 * fire finds the classes already pending.
 */
SEC("fexit/inet_recvmsg")
int inet_recvmsg(unsigned long long *ctx)
{
    return read_returned((void *)ctx[0], (long)(int)ctx[4]);
}

SEC("fexit/inet6_recvmsg")
int inet6_recvmsg(unsigned long long *ctx)
{
    return read_returned((void *)ctx[0], (long)(int)ctx[4]);
}

SEC("fexit/unix_stream_recvmsg")
int unix_stream_recvmsg(unsigned long long *ctx)
{
    return read_returned((void *)ctx[0], (long)(int)ctx[4]);
}

SEC("fexit/unix_dgram_recvmsg")
int unix_dgram_recvmsg(unsigned long long *ctx)
{
    return read_returned((void *)ctx[0], (long)(int)ctx[4]);
}

SEC("fexit/unix_seqpacket_recvmsg")
int unix_seqpacket_recvmsg(unsigned long long *ctx)
{
    return read_returned((void *)ctx[0], (long)(int)ctx[4]);
}

SEC("fexit/sock_splice_read")
int sock_splice_read(unsigned long long *ctx)
{
    struct file *file = (void *)ctx[0];
    long received = (long)ctx[5];
    if (received <= 0)
        return 0;
    struct socket *sock = bpf_sock_from_file(file);
    if (sock)
        receive(sock->sk);
    return 0;
}

/*
 * Whether no other socket of this type in its namespace can hold its port: a wildcard bind on no
 * device without SO_REUSEADDR or SO_REUSEPORT. Distinct addresses, devices or reuse share a port.
 */
static __always_inline int sole_holder(struct sock *sk, __u16 family)
{
    if (BITFIELD(sk, __sk_common.skc_reuse) || BITFIELD(sk, __sk_common.skc_reuseport) ||
        sk->__sk_common.skc_bound_dev_if)
        return 0;
    if (family == AF_INET)
        return !sk->__sk_common.skc_rcv_saddr;
    __u8 local[16];
    if (bpf_probe_read_kernel(local, 16, __builtin_preserve_access_index(&sk->__sk_common.skc_v6_rcv_saddr)))
        return 0;
    __u64 high, low;
    __builtin_memcpy(&high, local, 8);
    __builtin_memcpy(&low, local + 8, 8);
    return !high && !low;
}

/*
 * A freed IP socket's tags must not taint the next socket with its addresses, nor be lost while
 * another socket may still read what they describe: a connection's own tag goes with it, a port's
 * only with its sole holder. Port tags of TCP (unbound fast-open senders) stay, since every
 * connection accepted on the port shares it.
 */
SEC("lsm/sk_free_security")
int sk_free_security(unsigned long long *ctx)
{
    struct sock *sk = (void *)ctx[0];
    __u16 family = sk->__sk_common.skc_family;
    if (family != AF_INET && family != AF_INET6)
        return 0;
    struct flow key = {};
    key.net = net_cookie(sk);
    key.type = sk->sk_type;
    key.port = sk->__sk_common.skc_num;
    if (!key.port)
        return 0;
    if (key.type == SOCK_STREAM) {
        if (!inet_address(sk, 0, key.peer)) {
            key.peer_port = host_port(sk->__sk_common.skc_dport);
            bpf_map_delete_elem(&FLOWS, &key);
        }
        return 0;
    }
    if (!sole_holder(sk, family))
        return 0;
    key.family = family;
    bpf_map_delete_elem(&FLOWS, &key);
    if (family == AF_INET6 && !BITFIELD(sk, __sk_common.skc_ipv6only)) {
        key.family = AF_INET;
        bpf_map_delete_elem(&FLOWS, &key);
    }
    return 0;
}

/* An operation no governed task may perform, unless an earlier program already decided. */
static __always_inline int refused(int previous)
{
    if (previous)
        return previous;
    return governed() ? -EPERM : 0;
}

/* A SQPOLL ring's kernel thread submits without stops; refuse it before it exists. */
SEC("lsm/uring_sqpoll")
int uring_sqpoll(unsigned long long *ctx)
{
    return refused(ctx[0]);
}

/* An io_uring personality runs requests under credentials registered earlier. */
SEC("lsm/uring_override_creds")
int uring_override_creds(unsigned long long *ctx)
{
    return refused(ctx[1]);
}

/* Passthrough commands reach drivers below every file and socket hook. */
SEC("lsm/uring_cmd")
int uring_cmd(unsigned long long *ctx)
{
    return refused(ctx[1]);
}

/* A user namespace would hand governed code capabilities inside it. */
SEC("lsm/userns_create")
int userns_create(unsigned long long *ctx)
{
    return refused(ctx[1]);
}

/*
 * BPF and perf reach kernel state every domain shares. The BPF hooks' `kernel` flag marks the
 * kernel's own calls (syscall programs, light skeletons), which pass. bpf_token_create has no
 * such flag.
 */
SEC("lsm/bpf")
int bpf(unsigned long long *ctx)
{
    int kernel = (__u8)ctx[3], previous = ctx[4];
    return kernel ? previous : refused(previous);
}

SEC("lsm/bpf_map_create")
int bpf_map_create(unsigned long long *ctx)
{
    int kernel = (__u8)ctx[3], previous = ctx[4];
    return kernel ? previous : refused(previous);
}

SEC("lsm/bpf_prog_load")
int bpf_prog_load(unsigned long long *ctx)
{
    int kernel = (__u8)ctx[3], previous = ctx[4];
    return kernel ? previous : refused(previous);
}

SEC("lsm/bpf_token_create")
int bpf_token_create(unsigned long long *ctx)
{
    return refused(ctx[3]);
}

SEC("lsm/perf_event_open")
int perf_event_open(unsigned long long *ctx)
{
    return refused(ctx[1]);
}

/*
 * PR_SET_PTRACER lets another process trace the caller; PR_SET_MM rewrites its memory map
 * description. Any other option passes: when every program returns 0 the trampoline runs the
 * hook's stub, whose -ENOSYS tells prctl that no module handled the option. A module ordered
 * before bpf (Yama) has already applied PR_SET_PTRACER when this refuses it, so tracing across
 * domains must also be refused where the trace is checked.
 */
SEC("lsm/task_prctl")
int task_prctl(unsigned long long *ctx)
{
    int option = ctx[0], previous = ctx[5];
    if (previous)
        return previous;
    if (option != PR_SET_PTRACER && option != PR_SET_MM)
        return 0;
    return governed() ? -EPERM : 0;
}

/* TIOCSTI and TIOCLINUX (selection paste) inject input for whoever reads the terminal. */
static __always_inline int terminal_injection(unsigned long long *ctx)
{
    unsigned int cmd = ctx[1];
    int previous = ctx[3];
    if (previous)
        return previous;
    if (cmd != TIOCSTI && cmd != TIOCLINUX)
        return 0;
    return governed() ? -EPERM : 0;
}

SEC("lsm/file_ioctl")
int file_ioctl(unsigned long long *ctx)
{
    return terminal_injection(ctx);
}

SEC("lsm/file_ioctl_compat")
int file_ioctl_compat(unsigned long long *ctx)
{
    return terminal_injection(ctx);
}

/* A task's cgroup v2 id; zero, which no cgroup has, when unreadable. */
static __always_inline __u64 task_cgroup(struct task_struct *task)
{
    struct css_set *set = 0;
    struct cgroup *cgroup = 0;
    struct kernfs_node *node = 0;
    __u64 id = 0;
    if (READ(set, task->cgroups) || !set || READ(cgroup, set->dfl_cgrp) || !cgroup ||
        READ(node, cgroup->kn) || !node || READ(id, node->id))
        return 0;
    return id;
}

/*
 * Process attributes are storage channels another process can observe: a governed task changes
 * them only for tasks in its own cgroup.
 */
static __always_inline int other_domain(struct task_struct *task, int previous)
{
    if (previous)
        return previous;
    if (!governed())
        return 0;
    return task_cgroup(task) == bpf_get_current_cgroup_id() ? 0 : -EPERM;
}

SEC("lsm/task_setnice")
int task_setnice(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[2]);
}

SEC("lsm/task_setioprio")
int task_setioprio(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[2]);
}

SEC("lsm/task_setscheduler")
int task_setscheduler(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[1]);
}

SEC("lsm/task_movememory")
int task_movememory(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[1]);
}

SEC("lsm/task_setpgid")
int task_setpgid(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[2]);
}

/* prlimit(2) on another process passes here as well as task_prlimit. */
SEC("lsm/task_setrlimit")
int task_setrlimit(unsigned long long *ctx)
{
    return other_domain((void *)ctx[0], ctx[3]);
}

/*
 * Keyrings are shared with untraced processes of the same uid, the user keyring above all. A
 * governed task may write, link or change the attributes of only keys its own root allocated;
 * viewing, reading and searching flow inward.
 */
SEC("lsm/key_alloc")
int key_alloc(unsigned long long *ctx)
{
    __u64 key = ctx[0];
    unsigned long flags = ctx[2];
    int previous = ctx[3];
    if (previous)
        return previous;
    __u64 *root = governed();
    /* The user and user-session keyrings belong to the uid, whichever lookup created them. */
    if (!root || (flags & KEY_ALLOC_UID_KEYRING)) {
        bpf_map_delete_elem(&KEYS, &key);
        return 0;
    }
    __u64 owner = *root;
    /* Unrecorded, a stale entry for the address could hand the key to another root. */
    return bpf_map_update_elem(&KEYS, &key, &owner, 0) ? -EPERM : 0;
}

SEC("lsm/key_permission")
int key_permission(unsigned long long *ctx)
{
    /* A key_ref_t is the key's address with bit 0 marking possession. */
    __u64 key = ctx[0] & ~1ULL;
    __u32 need = ctx[2];
    int previous = ctx[3];
    if (previous)
        return previous;
    if (need != KEY_NEED_WRITE && need != KEY_NEED_LINK && need != KEY_NEED_SETATTR)
        return 0;
    __u64 *root = governed();
    if (!root)
        return 0;
    __u64 *owner = bpf_map_lookup_elem(&KEYS, &key);
    return owner && *owner == *root ? 0 : -EPERM;
}
