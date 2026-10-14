# Privileged enforcement: agent-policy socket checks in the kernel

Status: **Phase A implemented and exercised on a BPF-LSM kernel. Both 31-case leak-corpus modes
pass with seed 20261014; direct hooks and nested PID namespaces pass. Phases B/C are explicitly
deferred.** Updated 2026-10-14 on `feat/agent-policy`.

marsh offers two enforcement modes for agent-policy socket checks. The **unprivileged** mode is
today's ptrace gate and stays the default. The **privileged** mode moves the check itself into BPF
LSM hooks, which see the kernel's copied address. That closes the named-sockaddr TOCTOU rewrite.
It does not freeze the peer of a shared connected datagram socket through protocol transmission;
the connected-UDP peer-reconnect race remains a documented gap (§7).
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
* `marsh-instrument`: one `Enforcer` interface for ptrace decisions, all-class kernel refresh,
  real contact delivery and asynchronous contact observations. The old public `PeerGate` path
  is removed; ptrace enforcement itself remains unchanged.

### 4.3 Maps

| Map | Type | Key → value | Written by |
|---|---|---|---|
| `TASK_ROOT` | hash | tgid → root id | the tracer, at every fork/clone stop before the child runs; removed at exit |
| `ANSWERS` | hash | root id → `{ generation, deny, guarded, local, stale, audit }` | userspace refresh (§4.4) |
| `CLASS_NET4`, `CLASS_NET6` | LPM trie | root-prefixed IP prefix → class id | userspace, from the policy's declared networks (§4.5) |
| `LOCAL_ALLOWED` | hash | (root id, endpoint) → () | userspace, for `untraced_local(Deny)` (§4.7) |
| `EVENTS` | ring buffer | contact and refusal records | the BPF programs |
| `PID_NAMESPACE` | array | 0 → loader PID namespace device/inode | loader before attaching hooks |
| `HEALTH` | array | 0 → sticky reporting/map failure | loader and BPF programs |

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
| `lsm/uring_sqpoll` | admission of a kernel-polled submission ring | governed tasks are refused before a SQPOLL worker can be created |

In privileged mode, kernel contact events replace captured peer observations. Named-address
capture remains only for the conservative early local taint handoff and `sendfile`/`splice`
fallback. The io_uring `unknown`-peer rule is replaced by actual socket hook decisions; socket-only
CONNECT/SEND/SENDMSG operations are accepted by the filesystem observer. Other ring operations
remain unsupported. The extra `uring_sqpoll` hook is mandatory: rejecting a setup only at
publication cannot prevent an untraced kernel submission thread consuming its queue.

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
  `LOCAL_ALLOWED`: exact bound endpoints obtained with `pidfd_getfd`/`getsockname` from traced
  commands, filtered through `Tracing::receivers` so an untraced co-listener is not an exception.
  Kernel names avoid `/proc/net/unix`'s ambiguous rendering of literal `@` and embedded NUL bytes.
  Dual-stack IPv6 wildcards also supply an IPv4 exception only when `IPV6_V6ONLY` is disabled.
  With local restriction bits and an allowed-endpoint set
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
  Draining reserves every batch owner while trace state is locked; callbacks run outside locks.
  Run quiescence waits for in-flight deliveries and a final drain before reclaiming evidence,
  so another tracer cannot retire a root whose events are already being dispatched.
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
* `CONFIG_IO_URING=y` and the `uring_sqpoll` LSM hook: privileged startup requires all three
  admission/socket hooks. A kernel lacking the hook is not silently given weaker enforcement.
* `sendfile`/`splice` coverage by `security_socket_sendmsg` varies with kernel implementation.
  Phase A conservatively retains the ptrace peer gate and its observation evidence on every
  kernel, even when the LSM hook also reports the contact.
* Privileges to load and attach LSM programs. I believe this is CAP_BPF and CAP_PERFMON,
  possibly also CAP_MAC_ADMIN; in practice it means root, or a privileged helper that loads the
  programs and passes map descriptors to marsh.

The current development host has noninteractive sudo/root access, but its active LSM list does
**not** include `bpf`, so privileged attachment is unavailable until the host is booted with BPF
LSM active. An explicit privileged request must report this prerequisite failure; Auto reports
the reason before falling back. Root alone cannot activate an LSM missing from the boot-time list.

The isolated runner has loaded and exercised the real programs on Linux 7.0.0-38 with active
BPF LSM and BTF, including a loader inside a nested PID namespace. No host reboot is required.

### Build tools

Default builds need no BPF toolchain. For the `privileged` feature:

```sh
rustup toolchain install nightly-2025-12-01 --profile minimal --component rust-src
cargo +nightly-2025-12-01 install bpf-linker --version 0.9.15 --locked
cargo build --features privileged
```

Install clang 21 alongside the normal C/libclang/libbtrfsutil prerequisites. The build checks the
clang/linker versions, builds the CO-RE object with the pinned nightly, and embeds it; missing
tools fail the privileged build rather than silently producing a weaker binary.


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
* `socket_sendmsg` sees a kernel-owned destination when `msg_name` is present. Without one,
  it reads the socket's connected peer. Another holder of a connected UDP socket can reconnect
  that mutable peer after the hook and before the protocol selects its transmission destination.
  This residual socket-state race is not closed by the two hooks; closing it requires an
  additional destination-stable protocol/packet enforcement point, not another userspace read.
* Governed tasks use `PTRACE_O_EXITKILL`: a tracer crash must not leave them running with a
  frozen policy table. Shared controller failures signal every governed process through pidfds,
  clear task mappings even after poisoning, and detach only after those fatal signals are sent.
* Task keys and events use the loader's PID namespace via `bpf_get_ns_current_pid_tgid`.
  A loader may run in a nested PID namespace with a matching procfs view; a governed child
  changing PID namespace is refused before resume rather than silently losing attribution.
* Successful loading makes the host nondumpable, protecting its unpinned map/program descriptors
  from same-UID tracees. Privileged managed launches clear effective/permitted/inheritable/ambient
  capabilities and set `NO_NEW_PRIVS`; root children also lock `NOROOT` and clear their bounding
  set. This intentionally prevents managed commands using administrative OS capabilities.
  Their dumpability is restored only after stripping authority, so the parent can still seize
  them. Same-UID signaling remains possible; `EXITKILL` protects against a killed tracer, not DoS.

## 8. Testing

The shared classifier and policy refresh regressions run without an active BPF LSM. Actual
socket decisions, audit/stale outcomes, local exceptions, ring loss and PID-namespace attribution
are tested by the ignored `marsh-bpf` `lsm` binary in a qualifying disposable kernel.

```sh
cargo test -p marsh-core -p marsh-bpf --features privileged --no-run
sudo python3 bench/leak-fuzzer/privileged-vm.py <printed-lsm-test-executable> --ignored --nocapture
sudo python3 bench/leak-fuzzer/privileged-vm.py env MARSH_LEAK_ENFORCEMENT=privileged <printed-core-test-executable> leak_corpus --ignored --nocapture --test-threads=1
```

Run the same core executable again with `MARSH_LEAK_ENFORCEMENT=unprivileged`. The runner needs
QEMU, busybox-static, cpio, zstd, kmod, iproute2 and an installed generic BPF/BTF kernel with 9p
modules; the corpus also needs Python and genuine Node.js/libuv on a capless-accessible path.
It mounts the host runtime read-only, only the repository writable, and creates no external NIC.
System executable directories precede private-home PATH entries. `.github/workflows/privileged.yml`
builds and runs these same checks. Serialize VM runs on small hosts; command deadlines are not
excuses to weaken controls or remove forced io-wq execution.

The named-address rewrite case is a must-block attack only in privileged mode. Forced asynchronous
io_uring SEND/CONNECT attacks and untainted SEND/Node controls exercise real dispatch and delivery.
The privilege-boundary control checks capability stripping, `NO_NEW_PRIVS` and loader-FD isolation;
SQPOLL admission is checked with ordinary traffic still allowed. Harbor audit measurements remain
separate benchmark work; no Harbor false-denial numbers are claimed here.

Observed verification: 31 case runs with zero failures in each mode (including audit cross-checks),
two direct LSM acceptance tests in both initial and nested PID namespaces, 167 core regressions,
17 tracing regressions, three shared-classifier properties, and public mux/env mode checks.
The native Git-history regression was not runnable: this host's Git 2.53 has no `git history`
subcommand. Tests requiring real btrfs used an isolated btrfs HOME; the host LSM configuration was
not changed or rebooted.

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
