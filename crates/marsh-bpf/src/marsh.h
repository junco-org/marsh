/*
 * Records shared with userspace, and the address classifier. Mirrors marsh-peer-class: both
 * sides assert the record sizes, and a host test checks the classifier against the Rust one.
 * Plain C with no kernel or BPF dependencies, so the host test can compile it too.
 */
#ifndef MARSH_H
#define MARSH_H

typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef int __s32;

#ifndef MARSH_INLINE
#define MARSH_INLINE static inline __attribute__((always_inline))
#endif

#define AF_UNSPEC 0
#define AF_UNIX 1
#define AF_INET 2
#define AF_INET6 10
#define AF_NETLINK 16

#define CLASS_UNIX 0
#define CLASS_NETLINK 1
#define CLASS_LOOPBACK 2
#define CLASS_EXTERNAL 3
#define CLASS_UNKNOWN 4
#define MAX_CLASSES 64
#define MAX_TAINTS 16

enum { EVENT_CONTACT, EVENT_RECEIPT };
enum { DECISION_ALLOW, DECISION_DENY, DECISION_AUDIT };
enum { REASON_RULE, REASON_CONTINGENT, REASON_STALE, REASON_UNTRACED, REASON_UNANSWERED };

struct marsh_endpoint {
    __u16 family;
    __u16 port;
    __u32 scope;
    __u8 address[16];
    __u32 unix_len;
    __u8 unix_path[108];
};

struct marsh_answers {
    __u64 generation;
    __u64 deny;
    __u64 guarded;
    __u64 untraced;
    __u64 carried;
    __u64 contingent[MAX_TAINTS];
    __u32 acknowledged[MAX_TAINTS];
    __u32 stale;
    __u32 audit;
};

struct marsh_receipts {
    __u32 count[MAX_TAINTS];
};

struct marsh_event {
    __u32 kind;
    __u32 tgid;
    __u32 tid;
    __u32 class;
    __u64 root;
    __u64 time;
    __u64 generation;
    __u64 taint;
    __u64 sender;
    __u32 decision;
    __u32 reason;
    struct marsh_endpoint endpoint;
};

_Static_assert(sizeof(struct marsh_endpoint) == 136, "Endpoint ABI");
_Static_assert(sizeof(struct marsh_answers) == 240, "Answers ABI");
_Static_assert(sizeof(struct marsh_receipts) == 64, "Receipts ABI");
_Static_assert(sizeof(struct marsh_event) == 200, "Event ABI");

/* Mapped IPv6 unwraps to IPv4, exactly as Endpoint::normalize. */
MARSH_INLINE int marsh_ipv4(const struct marsh_endpoint *e, __u8 out[4])
{
    int from = 0;
    if (e->family == AF_INET) {
        from = 0;
    } else if (e->family == AF_INET6) {
        for (int i = 0; i < 10; i++)
            if (e->address[i])
                return 0;
        if (e->address[10] != 0xff || e->address[11] != 0xff)
            return 0;
        from = 12;
    } else {
        return 0;
    }
    for (int i = 0; i < 4; i++)
        out[i] = e->address[from + i];
    return 1;
}

/*
 * The IP part of Endpoint::normalize: mapped IPv6 becomes IPv4 without a scope. The kernel needs
 * canonical IP endpoints for its trie and allowlist; userspace normalizes every event it reads.
 */
MARSH_INLINE void marsh_normalize_ip(struct marsh_endpoint *e)
{
    __u8 v4[4];
    if (!marsh_ipv4(e, v4))
        return;
    e->family = AF_INET;
    for (int i = 0; i < 16; i++)
        e->address[i] = i < 4 ? v4[i] : 0;
    e->scope = 0;
}

/* Builtin class, or -1 when an IP needs the declared-network trie. */
MARSH_INLINE __s32 marsh_builtin_class(const struct marsh_endpoint *e)
{
    __u8 v4[4];
    switch (e->family) {
    case AF_UNIX:
        return CLASS_UNIX;
    case AF_NETLINK:
        return CLASS_NETLINK;
    case AF_INET:
    case AF_INET6:
        if (marsh_ipv4(e, v4))
            return v4[0] == 127 || (!v4[0] && !v4[1] && !v4[2] && !v4[3]) ? CLASS_LOOPBACK : -1;
        for (int i = 0; i < 15; i++)
            if (e->address[i])
                return -1;
        return e->address[15] <= 1 ? CLASS_LOOPBACK : -1;
    default:
        return CLASS_UNKNOWN;
    }
}

#endif
