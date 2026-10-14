# Privileged enforcement: agent-policy socket checks in the kernel

Status: **proposed, not implemented.** Written 2026-10-14 on `feat/agent-policy`, after
`a76f7c2` (per-class verdict cache, `untraced_local`, declassification).

marsh will offer two enforcement modes for agent-policy socket checks. The **unprivileged** mode is
today's ptrace gate and stays the default. The **privileged** mode moves the check itself into BPF
LSM hooks, which see the address the kernel actually uses. That closes the TOCTOU address rewrite,
the last entry in `crates/marsh-core/tests/leak-corpus/known-gaps.toml` that has no policy fix.
Both modes share all of the policy code: classes, rules, taint and declassification.

## 1. The unprivileged gate today

At the entry stop of every call that may reach a socket peer, the tracer asks the shell's agent
policy whether the task may proceed. A refused call is skipped and fails with `EPERM`.

| Piece | Where | What it does |
|---|---|---|
| Which calls are gated | `reaches_peer`, `written_descriptor`, `read_descriptor` in `crates/marsh-instrument/src/observation.rs` | `connect`, `sendto`, `sendmsg`, `sendmmsg`, writes to a socket, `sendfile`/`splice`, `io_uring_enter` |
| The peer a call names | `read_sockaddr`, `read_peers`, `interpret_peer`, `sent_peers` in `capture.rs` | Reads the `sockaddr` (or each `msghdr`'s `msg_name`) out of tracee memory at entry |
| The peer of a connected socket | `crates/marsh-instrument/src/socket.rs` | Copies the descriptor out with `pidfd_getfd` and asks `getpeername`, cached by socket inode |
| io_uring | `contacted` in `observation.rs` | Any submitted opcode outside `EFFECTLESS_IO_URING_OPS`, or an unreadable ring, is judged as an `unknown` peer |
| Local receivers | `local.rs`, `Tracing::receivers` | Parses `/proc/self/net/*` and scans traced tasks' `/proc/<tid>/fd` for the bound socket |
| The decision | `Snapshot::permits_peer` and `Snapshot::verdict` in `crates/marsh-core/src/shell/snapshot.rs` | Agent rules against committed history, the command's own observations and received taint, cached per peer class (`Verdicts`, `Basis`) |
| Untraced local peers | `untraced_denial` in `snapshot.rs`; `AgentPolicy::untraced_local` | With `Deny`, a principal holding a propagated class may not reach a local socket no traced command of its source holds |

**The gap.** The tracer reads the address from tracee memory, and the kernel copies it later.
Another thread of the tracee, or a `MAP_SHARED` mapper, can rewrite it in between. Python can
do this with `ctypes`, and a lost race only costs a retry. An io_uring submission's opcode can be
swapped the same way after the tracer reads it. Ptrace cannot close this gap: its view of the
arguments is always a copy taken before the kernel's.

## 2. Goals and non-goals

Goals:
* Enforce socket contacts against the address and socket the kernel uses, so a rewrite cannot
  change which peer is judged.
* Keep one policy engine. Classes, rules, `untraced_local`, taint and declassification behave
  identically in both modes, and the leak corpus runs unchanged in both.
* Keep the unprivileged mode fully working. Nothing in it is removed.
* Never fall back silently. A deployment that relies on the privileged mode must fail closed when
  it is unavailable.

Non-goals:
* Filesystem enforcement. Path reads have the same race (an `openat` path can be swapped after the
  tracer reads it, so a secret read could go unrecorded). Closing it needs an LSM `file_open` hook
  and a rebuilt filesystem observer, which is separate work. §9 lists it as a follow-up.
* Replacing ptrace. It still observes every filesystem effect, runs the exec hooks, restricts
  io_uring's filesystem operations (`access.rs`), and feeds the taint table.

## 3. Why the kernel can enforce marsh's rules

BPF programs cannot run junco-policy, and they cannot pause a syscall to ask userspace. They can
look up a precomputed answer in a map. That is enough here, because the rules never see a contact's
address, only its class:

* A contact is labelled `Edit ["net", class]` (`AgentPolicy::label`). The class is one of
  `unix`, `netlink`, `loopback`, `external`, `unknown` or a declared network's label
  (`AgentPolicy::peer_class`). That is a small set, fixed per policy.
* So for a given root (one shell's commands) at a given moment, "may it contact a peer of class
  C?" has one answer per class. The answer depends only on that root's inputs: committed history,
  received taint and the command's observations. `Verdicts` already caches exactly these answers,
  keyed by `Basis`.

The kernel therefore needs only:
* a map from task to root;
* a map from root to its answers per class;
* a classifier from address to class that matches `peer_class` exactly.

## 4. Architecture

```
            ptrace (unchanged role)                      BPF LSM (new)
  ┌──────────────────────────────────────┐   ┌──────────────────────────────────────┐
  │ observes calls; at each gated entry  │   │ socket_connect / socket_sendmsg:     │
  │ stop refreshes the root's answers    │──▶│  task → root → class → answer        │
  │ (Verdicts) for every class           │map│  deny ⇒ -EPERM                       │
  │ fills task→root at fork/clone stops  │──▶│ reports every judged contact         │
  └──────────────────────────────────────┘   └──────────────────────────────────────┘
                     ▲                                          │ ring buffer
                     └──────────── contacts, refusals ──────────┘
```

### 4.1 Modes

```rust
pub enum Enforcement { Unprivileged, Privileged, Auto }
```

* `Unprivileged`: today's gate.
* `Privileged`: load the BPF programs, or fail tracer start-up with the reason (for example
  "BPF LSM not enabled: add `bpf` to the `lsm=` boot parameter").
* `Auto`: try `Privileged` and fall back to `Unprivileged`. Fallback is reported, never silent:
  `Shell::enforcement()` returns the mode in force, and the first agent-policy shell reports the
  fallback through its `PolicyObserver`.

`Tracing::shared()` is process-wide, and so is the BPF state, so the mode is chosen once per
process when the tracer starts. A shell asking for a different mode than the one in force is
refused rather than silently given the other.

### 4.2 Components

* **`crates/marsh-bpf`** (new): the loader and map management, using `aya`. It is built only with a
  `privileged` cargo feature, so the default build is unchanged.
* **`crates/marsh-bpf-ebpf`** (new): the BPF programs (`aya-ebpf`, `no_std`), compiled with the
  pinned nightly's BPF target.
* **`crates/marsh-peer-class`** (new, `no_std`): the address classifier, shared by the BPF programs
  and by `AgentPolicy::peer_class`, so the two cannot drift (§4.5).
* `marsh-instrument`: an `Enforcer` seam behind `PeerGate`. Ptrace mode keeps today's path.
  Privileged mode refreshes answers instead of judging, and maintains the task map.

### 4.3 Maps

| Map | Type | Key → value | Written by |
|---|---|---|---|
| `TASK_ROOT` | hash | tgid → root id | the tracer, at every fork/clone stop before the child runs; removed at exit |
| `ANSWERS` | hash | root id → `{ generation, deny: bitset over class ids, audit: bool }` | userspace refresh (§4.4) |
| `CLASS_NET4`, `CLASS_NET6` | LPM trie | prefix → class id | userspace, from the policy's declared networks (§4.5) |
| `LOCAL_ALLOWED` | hash | (root id, endpoint) → () | userspace, for `untraced_local(Deny)` (§4.7) |
| `EVENTS` | ring buffer | contact and refusal records | the BPF programs |

The task map is keyed by tgid, not tid, because all threads of a process share a root. io_uring's
`io-wq` workers are threads of the submitting process, so work they do on its behalf is attributed
correctly. Exec keeps the tgid. A pid table filled from ptrace stops is race-free because the
child cannot run before the tracer has seen it. It also needs no cgroup delegation, which this
development host's v1/v2-hybrid cgroups would complicate.

### 4.4 Keeping answers current

This is the hard part, and it differs from the sketch discussed before implementation.
`Verdicts` fills **lazily**: an answer is decided the first time a class is contacted, then
reused while `Basis` holds. A kernel table needs answers before the contact, and the obvious
alternative is unaffordable. Recomputing every class after every observed call would be
quadratic in the command's length, because each decision walks the whole history.

The design is split into two phases.

**Phase A: refresh at the entry stop, enforce in the kernel.** Privileged mode keeps the ptrace
entry stops on network calls it has today (the same `reaches_peer` set), but changes what they
do. At each stop the tracer no longer reads the address. It checks `Basis` for the task's root,
and if the answers are stale it recomputes **every** class, not just one. It then publishes them
to `ANSWERS` with a new generation and resumes the task. The kernel hook then classifies the real
address and applies the answer. Because the answers cover every class, a rewritten address gets
the answer for its real class. Contacts with no syscall of their own, which are io_uring
submissions, refresh at the `io_uring_enter` stop. SQPOLL rings are already refused
(`access.rs`), so every contact is preceded by some stop.

Cost: a stale refresh decides every judged class once, instead of only the one contacted. That is
typically 3 to 6 classes per change, and the `Basis` check keeps repeated contacts free, as
measured for `a76f7c2` (3,000 datagrams: 1.17s against 1.15s with no rules).

**Phase B: eager answers, no network stops.** This needs incremental decisions in junco-policy:
keep each rule's derivative state across appended events, so one new event costs one step per
rule instead of a walk over the whole history. With that, every observed call that records
something can update the answers before its exit stop resumes the task. The network calls then
no longer need ptrace stops at all. This is an upstream change to junco-policy (`TracePolicy`
exposes only `decide(history, candidate)` today) and is out of scope until Phase A is proven.

**The stale window.** In Phase A, a recording call on thread B (say, a secret read) can change the
root's answers after thread A passed its entry stop but before A's hook runs. Today's gate has the
same window, since it judges A at entry. The privileged mode can do better: the tracer marks the
root stale in `ANSWERS` at B's exit stop. A hook that finds its root stale denies every class
any rule could forbid, and reports the refusal as `stale`. Whether that produces false denials in
practice is to be measured with audit mode on the Harbor benchmark (§8).

### 4.5 Classifying addresses in the kernel

The classifier mirrors `peer_class`:

1. `AF_UNIX` → `unix`. `AF_NETLINK` → `netlink`. Any other family → `unknown`.
2. IPv4, or IPv6 after unwrapping a v4-mapped address: loopback or unspecified → `loopback`.
3. Otherwise, the declared network that matches first in declaration order, or else `external`.

Step 3 needs care. An LPM trie returns the longest matching prefix, but `peer_class` wants the
first declared one. The two agree once each inserted prefix Q carries the label of the first
declared network N with N ⊇ Q. The networks containing an address form a chain of nested
prefixes. The longest of them, Q, is the one the trie returns, and every other one contains Q.
So the first declared network containing the address is the first declared network containing Q.
Userspace computes these labels when it loads the trie, and a unit test in `marsh-peer-class`
checks the result against first-match search on random overlapping network sets.

### 4.6 Hooks and coverage

| Hook | Covers | Address used |
|---|---|---|
| `lsm/socket_connect` | `connect`, io_uring `CONNECT` | the kernel's copy of the `sockaddr` |
| `lsm/socket_sendmsg` | `send*`, `write`/`writev` on sockets (via `sock_write_iter`), io_uring `SEND`/`SENDMSG`/`WRITE` to a socket, and, on kernels whose `splice` reaches sockets through `sock_sendmsg`, `sendfile` and `splice` | `msg->msg_name` when set (kernel copy). Otherwise the socket's peer: `skc_daddr`/`skc_dport` (v4), `skc_v6_daddr` (v6), or the unix peer's bound name via BTF (`unix_sk(sk)->peer`) |

These replace, in privileged mode only, the address capture in `capture.rs`, the descriptor peer
lookup in `socket.rs`, and the io_uring `unknown`-peer rule in `contacted`. The io_uring rule
over-blocks today, for example Node/libuv after reading a dotenv file. The ptrace side still
refuses SQPOLL rings and io_uring filesystem operations.

`sendfile` and `splice` still need their source counted as a read before the contact. In Phase A
the entry stop does this exactly as `permits_peer` does now (`TaintState::sending` with the
source's effects), and the refresh includes it.

### 4.7 Local peers, taint and `untraced_local`

**Phase A** keeps the userspace logic and feeds it the real address:

* **Handing taint.** The hook reports each loopback or unix contact with its real endpoint. At the
  sender's exit stop, the tracer drains the event and calls the existing `hand_taint` on holders
  of that endpoint. The entry-stop handoff, based on the address the call names, stays too, so
  the common case taints the receiver before delivery. A receiver that races the sender's exit
  stop after a rewritten address is a residual gap, recorded in `known-gaps.toml` for this mode.
* **`untraced_local(Deny)`.** When a principal holds a propagated class, its refresh also writes
  `LOCAL_ALLOWED`: the local endpoints held by traced commands of its source, from
  `Tracing::receivers`. With the `deny` bit set for `loopback`/`unix` and an allowed-endpoint set
  present, the hook permits a local contact only to an allowed endpoint. A local address nothing
  listens on is not refused today; the kernel cannot see listeners, so this mode refuses it. That
  differs only in the error a connection that would fail anyway returns.

**Phase C: taint in the kernel.** The hooks tag the receiving socket: the unix peer, or the
loopback listener. For the listener, that means `bpf_sk_lookup_tcp`/`_udp` where the program type
allows it, and otherwise a socket-cookie map filled by a `sockops` program at passive establish.
An `lsm/socket_recvmsg` hook then marks the reading root tainted when it reads a tagged socket.
For that to take effect at once, the refresh publishes a second, contingent answer set per
propagated class: the root's answers as if it had just received that class. The receive hook
switches to it atomically and reports the taint, so the userspace table catches up. This removes
`/proc/net` parsing, the shared-network-namespace assumption and the taint given to holders that
never read. The receiving socket's owner also answers `untraced_local` exactly: an owning tgid
absent from `TASK_ROOT` means a process marsh doesn't trace.

### 4.8 Events back to userspace

Every judged contact is reported as `{ tgid, tid, root, family, endpoint, class, decision }`:

* **Contacts become observations.** Today `effects.peers` comes from the tracer's capture. In
  privileged mode it comes from the event stream, drained at the call's exit stop before the
  effect is pushed, so its order matches the other effects. Rules that depend on past contacts
  then see the real peer. io_uring contacts complete asynchronously and are drained at the
  process's next stop.
* **Refusals go to the `PolicyObserver`** with the same text as today (`Denial` from the cached
  answer), deduplicated per run by `Snapshot::first_report`.
* **Audit mode.** `ANSWERS.audit` makes the hook permit and report `would block`, matching the
  unprivileged audit.

## 5. Changes to existing code

* `Snapshot::verdict` gains `refresh_all`: decide every judged class under one `Basis`, used by
  privileged entry stops. The lazy path is unchanged for unprivileged mode.
* `Basis` gains a generation number. `TaintState::revision` and the validator revision already
  exist.
* `PeerGate` becomes an `Enforcer` trait with a ptrace implementation (today's code) and a BPF one.
* The tracer adds a task-map update at fork/clone/exit stops; it already stops there.
* `AgentPolicy::peer_class` calls `marsh-peer-class`.
* Nothing in the unprivileged path is deleted.

## 6. Requirements

These need confirming on the target kernels.

* `CONFIG_BPF_LSM=y` **and** `bpf` in the active LSM list (`/sys/kernel/security/lsm`), which
  usually means an `lsm=…,bpf` boot parameter.
* BTF: `CONFIG_DEBUG_INFO_BTF=y`.
* BPF ring buffer: kernel 5.8 or later. BPF LSM: 5.7 or later.
* `sendfile`/`splice` reaching `security_socket_sendmsg` depends on the kernel routing socket
  splices through `sock_sendmsg` (believed 6.5 or later). On older kernels these calls keep being
  judged at the ptrace entry stop, as today.
* Privileges to load and attach LSM programs. I believe this is CAP_BPF and CAP_PERFMON,
  possibly also CAP_MAC_ADMIN; in practice it means root, or a privileged helper that loads the
  programs and passes map descriptors to marsh.

The current development host does **not** qualify: kernel 5.15, LSM list
`lockdown,capability,landlock,yama,apparmor`, `unprivileged_bpf_disabled=2`, no root. Its kernel
is built with BPF LSM and BTF, so a reboot with `lsm=` and root would be enough.

## 7. Security notes

* Enforcement moves into the kernel, but the answers are still computed in userspace from what
  ptrace observed. A secret read that ptrace misses is still missed. That is the filesystem-side
  race in §2's non-goals.
* `TASK_ROOT` must be filled before a child runs, or a fresh process could contact peers before
  it has a root. Its default must be **deny judged classes**, not allow. Stopping the child before
  it runs is how marsh already traces forks.
* The maps must not be writable by traced processes. They are not pinned in a filesystem that
  traced processes can reach, and their descriptors are not inherited (`O_CLOEXEC`).
* Programs are detached and maps freed when the tracer exits. A crash leaves no stale policy
  because nothing is pinned.

## 8. Testing

Runnable on the development host:
* `marsh-peer-class` against `peer_class`: the first-match versus longest-prefix property (§4.5),
  v4-mapped addresses, unspecified addresses and unix names.
* `refresh_all` against the lazy `verdict`: after any sequence of observations, every class's
  answer equals the lazy one.
* The task-map bookkeeping with a fake `Enforcer`.

Needs a privileged runner (kernel 6.x, `lsm=…,bpf`, root), for example a CI VM:
* The full leak corpus in both modes. Its harness gains a mode switch.
* A TOCTOU case: a thread rewrites the `sockaddr` from loopback to the external sink in a loop
  while another connects. It must be blocked in privileged mode, and it stays in
  `known-gaps.toml` for unprivileged mode.
* An io_uring `CONNECT` and `SEND` after a secret read must be blocked. Node/libuv traffic before
  any secret read must be delivered.
* Audit mode on the Harbor benchmark in both modes, counting would-be denials (including `stale`)
  to measure false denials.

## 9. Open questions

1. **Stale-window denials.** Is deny-on-stale (§4.4) acceptable, or should a stale hook permit and
   rely on the next refresh? Decide after the Harbor audit numbers.
2. **Loading and privileges.** Should marsh load the programs itself (needs root), or should a
   small privileged helper own them and hand map descriptors over a socket? The helper keeps
   marsh unprivileged, but adds a component to deploy.
3. **Phase B.** Incremental decisions in junco-policy: an API sketch to agree with its owner before
   Phase B is scheduled.
4. **Filesystem race.** Should an LSM `file_open` hook record secret opens in privileged mode, so
   the answers' inputs are race-free too? It's the natural next step, but it needs a filesystem
   observer that trusts the kernel's path rather than the tracer's.

## 10. Order of work

1. `marsh-peer-class`, with `peer_class` switched over to it and the property test. Runs here.
2. `refresh_all` and the `Enforcer` seam, with the ptrace implementation as today. Runs here.
3. `marsh-bpf`/`marsh-bpf-ebpf` behind the `privileged` feature: the task map, answers, the two
   hooks and the event drain (Phase A without local peers).
4. Phase A local peers: real-endpoint taint handoff and `LOCAL_ALLOWED`.
5. The privileged CI runner, the corpus in both modes, and the TOCTOU case.
6. Phase C (taint in the kernel), then Phase B once junco-policy supports incremental decisions.
