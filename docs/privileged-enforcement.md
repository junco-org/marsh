# Privileged enforcement: agent-policy socket checks in the kernel

Status: **Phases A, B and C implemented** on `feat/agent-policy`. Phase B depends on junco-policy's
incremental `TraceCursor`, which the workspace takes from the upstream `feat/incremental-trace`
branch. Verification is recorded in §8.

marsh offers two enforcement modes for agent-policy socket checks. The **unprivileged** mode is the
ptrace gate and stays the default. The **privileged** mode moves the check into BPF LSM hooks,
which see the kernel's copied address and socket, keeps every class's answer current as the
command runs, and carries secret taint through local sockets inside the kernel. Both modes share
the policy code: classes, rules, taint and declassification.

## 1. The unprivileged gate

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
Another thread of the tracee, or a `MAP_SHARED` mapper, can rewrite it in between, and the same
rewrite misdirects the local taint handoff (`local-handoff-rewrite`). Ptrace cannot close this gap:
its view of the arguments is always a copy taken before the kernel's.

## 2. Goals and non-goals

Goals:
* Enforce socket contacts against the address and socket the kernel uses.
* Keep one policy engine; the leak corpus runs unchanged in both modes.
* Keep the unprivileged mode fully working.
* Never fall back silently: a deployment relying on the privileged mode fails closed without it.

Non-goals:
* Filesystem enforcement. An `openat` path can be swapped after the tracer reads it, so a secret
  read could go unrecorded. Closing it needs an LSM `file_open` hook and a filesystem observer
  that trusts the kernel's path (§9).
* Replacing ptrace. It still observes every filesystem effect, runs the exec hooks, restricts
  io_uring's filesystem operations (`access.rs`), and feeds the taint table.

## 3. Why the kernel can enforce marsh's rules

BPF programs cannot run junco-policy or pause a syscall to ask userspace, but they can look up a
precomputed answer, and the rules never see a contact's address, only its class:

* A contact is labelled `Edit ["net", class]`. The class is `unix`, `netlink`, `loopback`,
  `external`, `unknown` or a declared network's label: a small set, fixed per policy.
* For a given root (one shell's commands) at a given moment, "may it contact a peer of class C?"
  has one answer per class, decided from committed history, the command's observations and
  received taint.

The kernel therefore needs a map from task to root, a map from root to its answers per class, and a
classifier from address to class that matches `AgentPolicy::peer_class` exactly.

## 4. Architecture

```
          ptrace + policy engine (userspace)                 BPF LSM (kernel)
  ┌───────────────────────────────────────────┐   ┌──────────────────────────────────────────┐
  │ every recording exit stop advances each   │   │ socket_connect / socket_sendmsg /        │
  │ class's junco cursor and publishes the    │──▶│ unix_may_send: task → root → class →     │
  │ root's answers (Phase B)                  │map│ answer, tag the local receiver (Phase C) │
  │ fork/exec/exit stops fill task → root     │──▶│ socket reads (fexit): taint the reader,  │
  │ an event thread folds contacts and        │   │ which switches to contingent answers     │
  │ receipts into evidence and taint          │◀──│ every contact and receipt → ring buffer  │
  └───────────────────────────────────────────┘   └──────────────────────────────────────────┘
```

### 4.1 Modes

`Enforcement::{Unprivileged, Privileged, Auto}`. `Privileged` loads the programs or fails tracer
start-up with the reason; `Auto` tries and falls back to `Unprivileged`, reporting the reason
through `Tracing::enforcement_fallback`, `Shell::enforcement()` and the first agent shell's
`PolicyObserver`. The mode is chosen once per process (`select` in `tracing_kernel.rs`); a later
explicit request for the other mode is refused.

### 4.2 Components

* **`crates/marsh-bpf/src/marsh.bpf.c`**: the programs, in C. `cargo build --features privileged`
  compiles it with clang 15+ (`-mcpu=v3`) and embeds it; no nightly toolchain or BPF linker. Kernel
  structs are declared field by field with `preserve_access_index`, so every access is a CO-RE
  relocation against the running kernel's BTF; no generated `vmlinux.h`.
* **`crates/marsh-bpf/src/marsh.h`**: the records shared with userspace and the C classifier,
  mirrored by **`crates/marsh-peer-class`**, which `AgentPolicy::peer_class` also uses. Both sides
  assert the record sizes, and a host test compiles the C classifier and checks it against the
  Rust one on random endpoints.
* **`crates/marsh-bpf/src/lib.rs`**: `Controller`, the loader and map management (Aya).
* **`crates/marsh-instrument/src/tracing_kernel.rs`**: the tracer side: mode selection, publishing,
  task attribution, the entry-stop work and event delivery. Without the `privileged` feature its
  `Kernel` is uninhabited, so every privileged path is statically dead.
* **`crates/marsh-core/src/shell/answers.rs`**: the incremental answer engine (Phase B).
* `Enforcer` (`enforcement.rs`): the seam between tracer and policy engine: `permits` (the ptrace
  gate), `refresh` (a root's answers) and `event` (a kernel contact or receipt).

### 4.3 Maps

| Map | Type | Key → value | Written by |
|---|---|---|---|
| `TASK_ROOT` | hash | tgid → root | the tracer, at every fork/clone stop before the child runs; removed at exit |
| `ANSWERS` | hash | root → `Answers` | userspace only, replaced whole |
| `RECEIVED` | hash | root → per-taint-class receipt counters | the kernel only (atomic adds); userspace removes a retired root's entry |
| `CLASS_NET4`, `CLASS_NET6` | LPM trie | root-prefixed IP prefix → class | userspace, from declared networks (§4.5) |
| `LOCAL_ALLOWED` | hash | IP endpoint → () | userspace: endpoints traced processes hold (§4.7) |
| `SOCKS` | socket storage | socket → `{ traced, taint, sender }` | the kernel; freed with the socket |
| `FLOWS` | hash | `{ netns, type, port, peer port, peer address, destination family }` → `{ taint, sender }` | the kernel; a connection's tag goes with its socket, a port's only with its sole holder (§4.7) |
| `EVENTS` | ring buffer | contacts and receipts | the kernel |
| `HEALTH` | array | sticky poison flag: event loss or a map failure | loader and kernel |
| `PID_NAMESPACE` | array | the loader's PID namespace | loader |

`Answers` holds `generation`, `deny` (classes refused now), `guarded` (classes any rule can
refuse), `untraced` (local classes whose receiver must be traced while the root carries taint),
`carried` (taint classes its sends carry), `contingent[16]` (`deny` as if it had just received
taint class *i*), `acknowledged[16]` (receipts folded in), `stale` and `audit`.

Tasks are keyed by tgid: io_uring's `io-wq` workers are threads of the submitting process, and
exec keeps the tgid. A table filled from ptrace stops is race-free because the child cannot run
before the tracer has seen it.

### 4.4 Keeping answers current (Phase B)

`Verdicts` in the unprivileged path fills lazily: each class is decided the first time it is
contacted, walking the whole history. The kernel needs answers before the contact, and recomputing
every class by a full walk after every observed call would be quadratic.

Phase B makes the answers incremental. junco-policy's `TraceCursor` holds one candidate's
derivative state per rule; `TracePolicy::advance` consumes one more event with at most one step per
rule, and `verdict` equals `decide` over everything consumed (junco's differential test checks this
on every prefix of random histories). `KernelAnswers` compiles the agent policy once into an arena
it owns (`self_cell`), keeps one cursor per class after the committed history (extended in place
while that history only grows), and one per class after the command's own observations.

The own observations are extended in place whenever that equals what `agent::observations` would
produce from scratch: each recording effect ordered after the last, and no write changing whether
a path an earlier write named exists (write classifications are re-read from the work view on
every decision). Otherwise they are rebuilt from the committed cursors. Taint received from other
principals, which the lazy path appends after everything else, is replayed per decision: a handful
of events. A call that records nothing reuses the published answers.

`Snapshot::observe` republishes before each exit stop resumes its task, so a secret read is in the
kernel's answers before the reader can contact anything. Taint changes republish every root of the
source; a command's start marks its root stale and publishes its answers. A test compares the
incremental answers with answers decided from scratch, and with the lazy gate, after every step of
a command (`kernel_answers_stay_equal_to_fresh_and_lazy_decisions_as_the_command_runs`).

**Network stops.** Ptrace stops at every syscall, so a socket call still stops, but it does no
policy work: no address capture, no refresh. The tracer refreshes at a socket call's entry only
when the root's answers are stale (a committed-history change marks them so until republished), or
**contact-sensitive**: one more contact could change a guarded decision, which no built-in rule
does but a custom rule over `net` events can. Then it first delivers pending contacts, so the
answers include them. In-kernel copies (`sendfile`, `splice`) keep the Phase A refresh with their
source counted as read, plus the ptrace gate, because `security_socket_sendmsg` coverage of them
varies by kernel.

**The stale window.** A secret read by thread B reaches the answers at B's exit stop. Between the
read completing and that stop, another thread A of the same root can contact a peer with the old
answers; the bytes are in the process's memory already, so ptrace-based observation cannot close
this (§7).

### 4.5 Classifying addresses in the kernel

The classifier mirrors `peer_class`: `AF_UNIX` → `unix`, `AF_NETLINK` → `netlink`, any other family
→ `unknown`; IPv4 (or mapped IPv6) loopback or unspecified → `loopback`; otherwise the first
declared network that contains it, or `external`. An LPM trie returns the longest prefix, so
userspace labels each inserted prefix Q with the first declared network containing Q
(`effective_networks`); a property test checks this against first-match search.

### 4.6 Hooks

| Hook | Covers |
|---|---|
| `socket_connect` | `connect` and io_uring `CONNECT`, judged on the kernel's copy of the address |
| `socket_sendmsg` | `send*`, writes to sockets, io_uring `SEND`/`SENDMSG` (and their zero-copy forms), judged on `msg_name` where the protocol sends to it, else the connected peer; tags the receiver |
| `unix_may_send` | every UNIX datagram send, after the kernel found the receiving socket; tags it |
| `socket_post_create`, `unix_stream_connect` | mark UNIX sockets a governed process created, and the server end of their stream connections, as traced |
| `fexit` of `inet_recvmsg`, `inet6_recvmsg`, `unix_stream_recvmsg`, `unix_dgram_recvmsg`, `unix_seqpacket_recvmsg`, `sock_splice_read` | a governed read that returned data from a tagged socket (`recv*`, every message of `recvmmsg`, `read`, `splice`), judged on return so a reader already blocked when the bytes arrived is caught |
| `sk_free_security` | drops a freed IP socket's connection tag, and its port's tags when no other socket can hold that port |
| `uring_sqpoll` | refuses kernel-polled rings for governed tasks before their worker exists |

A send judges `msg_name` only where it is the destination: datagram and raw sockets, and an
unconnected TCP socket's fast-open send. A connected TCP socket ignores a name and a UNIX stream or
seqpacket socket refuses or drops one, so those are judged on their connected peer whatever name the
call carries. The read hooks are the protocol entries rather than `sock_recvmsg`, which `recvmmsg`
bypasses for every message after its first.

In privileged mode kernel contact events replace captured peer observations; only `sendfile` and
`splice` keep ptrace-observed peers as evidence. Of io_uring's socket operations the filesystem
observer accepts `CONNECT`, `SEND`, `SENDMSG`, `SEND_ZC` and `SENDMSG_ZC`; `WRITE`, the receives
(`READ`, `RECV`, `RECVMSG`), `ACCEPT`, `SOCKET` and every other operation outside the effectless set
remain unsupported, though the hooks above would judge them.

### 4.7 Local peers, taint and `untraced_local` (Phase C)

**Taint in the kernel.** A permitted local contact by a root whose `carried` taint (or a receipt it
has not yet folded in) is nonempty tags the receiving socket:

* UNIX: the receiver itself. A stream's is the sender's connected peer; a datagram's is the socket
  `unix_may_send` names. Its `SOCKS` entry ORs in the classes and records the sender.
* IP loopback: the kernel cannot reach the receiving socket, so `FLOWS` keys the tag as the
  receiver sees itself: a TCP connection by network namespace, local port, peer port and peer
  address (exactly one connection); anything else by its local port and the destination's family
  (every datagram reader of that port, an over-approximation). An IPv4 destination's tag is read by
  IPv4 and dual-stack IPv6 sockets of the port, an IPv6 one's by IPv6 sockets only.
* A full `FLOWS` map, or socket storage that cannot be created, refuses that contact (`untagged`)
  rather than poisoning every root: the receiver would otherwise read untainted.

A freed socket removes its connection's tag. A port's tag is removed only when the freed socket was
the port's sole possible holder: a wildcard bind on no device without `SO_REUSEADDR` or
`SO_REUSEPORT`. Sockets bound to distinct addresses, devices or with reuse share a port, and another
may still hold tagged datagrams; their port's tag stays until such a sole holder is freed.

When a governed process's socket read returns data from a tagged socket, and the tag has classes
its root neither carries nor already has pending, the hook increments the root's `RECEIVED` counter for each and reports a
receipt. From then on, each contact of that root compares `RECEIVED` with `acknowledged`: one
pending class selects `contingent[class]`, several are undecidable and refuse every guarded class
(reported `stale`). Userspace folds the receipt into the taint table (`TaintState::hand`, so it
enters the reader's log and received taint) and republishes with the receipt acknowledged. Because
`RECEIVED` is written only by the kernel and `ANSWERS` only by userspace, a publish racing a receipt
cannot lose it. A test checks that the contingent answers equal the answers after the receipt is
folded in.

This replaces the Phase A send-time handoff: a relay is tainted when it reads, whatever address the
sender named, and holders that never read are not tainted. Taint class IDs are process-wide (at most
16 propagated classes), so a receipt taints readers in any source; the ptrace path hands taint only
within a source.

**`untraced_local(Deny)`.** While a root carries taint, a contact of an `untraced` class needs a
traced receiver. For UNIX sockets the kernel knows it exactly: the receiving socket must have been
created by a governed process (or be the server end of a stream to one). This includes unnamed
socketpairs, which Phase A refused because a name could not identify them. IP loopback receivers
are not reachable from the hook, so the tracer keeps `LOCAL_ALLOWED`: the IP endpoints traced
tasks hold (`pidfd_getfd`/`getsockname`), excluding any an untraced process also listens on,
refreshed at the root's socket-call entry stops. A wildcard bind matches every local address of
its family and port; IPv6 wildcards supply an IPv4 entry only without `IPV6_V6ONLY`.

### 4.8 Events back to userspace

Every judged contact and every receipt is a fixed `Event` record with the hook's
`CLOCK_MONOTONIC` time. The tracer's order sequence never runs behind that clock, so a contact
made before a stop is processed orders before that stop's call: an event thread
(`marsh-bpf-events`) delivers events as they arrive, without waiting for a stop or overflowing the
ring while tasks only send.

* **Contacts become observations** ordered at their hook time; an out-of-order arrival rebuilds the
  incremental own observations. Draining reserves each event's run while trace state is locked,
  and quiescence waits for those deliveries and a final drain, so no root is retired with events
  in flight.
* **Refusals go to the `PolicyObserver`** with the ptrace gate's wording (the cached denial for
  the class, or the stale, untraced, unanswered or untagged explanation), once per run.
* **Audit mode** permits and reports `would block`.

## 5. Requirements

* `CONFIG_BPF_LSM=y` with `bpf` in the active LSM list (`/sys/kernel/security/lsm`), BTF
  (`CONFIG_DEBUG_INFO_BTF=y`), and a kernel with the `uring_sqpoll` hook (5.19 or later); socket
  storage, `bpf_sock_from_file`, `bpf_skc_to_unix_sock` and BPF atomics are older than that.
  The read hooks are `fexit` programs, which need BPF trampolines (x86-64 or arm64), on functions
  of built-in IPv4, IPv6 and UNIX sockets (`inet_recvmsg`, `inet6_recvmsg`, `unix_*_recvmsg`).
  Privileged startup requires every hook; a kernel lacking one is not given weaker enforcement.
* Privileges to load and attach LSM programs: root in practice.
* Build: clang 15 or later (`MARSH_BPF_CLANG`, default `clang`).

This development host's boot-time LSM list lacks `bpf`; `bench/leak-fuzzer/privileged-vm.py` boots
an installed generic kernel in a disposable QEMU guest with BPF LSM active instead.

## 6. Code map

| Concern | Where |
|---|---|
| Mode selection, publishing, entry stops, event delivery | `tracing_kernel.rs` |
| Ordering kernel and ptrace records | `Sequence` in `observation.rs` |
| Incremental answers, taint class IDs | `answers.rs` |
| Republishing, kernel events into evidence and taint | `Snapshot::{republish, kernel_event}`, `Session::republish` |
| Capability stripping for privileged children | `restrict_child` in `enforcement.rs` |

## 7. Security notes

The threat model these notes are measured against, every kernel channel it implies and the plan to
mediate all of them are in [`threat-model.md`](threat-model.md). The notes below are the current
implementation's residuals; that document's §7 places each in its channel matrix.

* The answers are computed from what ptrace observed. A secret read ptrace misses is still missed,
  and a read reaches the answers only at its exit stop (§4.4): the filesystem-side race of §2.
* `TASK_ROOT` is filled before a child runs. A governed task without published answers is refused.
* The maps are not pinned and their descriptors are close-on-exec; loading makes the tracer
  nondumpable. Privileged managed launches clear all capabilities and set `NO_NEW_PRIVS`; root
  children also lock `NOROOT` and clear their bounding set. Their dumpability is restored only
  after stripping authority, so the parent can still seize them.
* Governed tasks use `PTRACE_O_EXITKILL`. A shared tracer failure kills every governed process
  through pidfds, clears task mappings even after poisoning, then detaches.
* Event loss or a map failure poisons enforcement for good: every governed contact is refused,
  auditing roots included. Failures one root causes stay with it: a full tag map refuses the
  contact that needed the tag, a root carrying a taint class beyond the 16 kernel IDs has every
  contact refused, and an unreadable socket or task drops out of `LOCAL_ALLOWED`.
* Task keys and events use the loader's PID namespace. A child in another PID namespace cannot be
  attributed: it alone is killed through its pidfd before it resumes, and enforcement stays healthy.
* `socket_sendmsg` sees a kernel-owned destination when it judges `msg_name`; otherwise it reads
  the connected peer, which another holder of a connected UDP socket can reconnect between the
  hook and transmission (`connected-udp-peer-reconnect`). Closing that needs a destination-stable
  protocol or packet enforcement point.
* Kernel taint follows reads on their return from the protocol `recvmsg` entries and
  `sock_splice_read`, before the reading thread regains control. Another thread of the reader could
  see the bytes in shared memory before that return. A TCP zero-copy receive
  (`getsockopt(TCP_ZEROCOPY_RECEIVE)`) maps data without either and is not tagged.
* Loopback datagram tags cover a whole port, and stay while any socket may share it, so readers
  can be over-tainted; never under-tainted.
* A UNIX socket counts as traced because a governed process created it, even after its descriptor
  is passed to an untraced process.

## 8. Testing

Without an active BPF LSM: the classifier's properties and C/Rust parity (`marsh-peer-class`,
`marsh-bpf`), junco's cursor differential test, the incremental-answer and kernel-event tests in
`crates/marsh-core/src/shell/tests/agent_policy.rs`, and the tracer's delivery test. With one, in
the disposable guest:

```sh
cargo test -p marsh-core -p marsh-bpf --features marsh-core/privileged,marsh-bpf/privileged --no-run
sudo python3 bench/leak-fuzzer/privileged-vm.py <lsm-test> --ignored --nocapture --test-threads=1
sudo python3 bench/leak-fuzzer/privileged-vm.py env MARSH_LEAK_ENFORCEMENT=privileged <core-test> leak_corpus --ignored --nocapture --test-threads=1
```

The `lsm` tests drive the real hooks: decisions, audit and stale outcomes, untraced receivers,
declared networks per root, ring overflow, a PID-namespace child refused alone, connected TCP and
UNIX seqpacket sends judged by their peer whatever name they carry, and taint receipts over UDP,
TCP (only the tainted connection), UNIX streams read with `read()`, a reader already blocked when
the bytes arrive, the second message of one `recvmmsg`, a datagram port another socket shared at a
different address, and several classes at once.
`.github/workflows/privileged.yml` runs them and the corpus in both modes: pull requests with
seed `20261014`, nightly with four seeds and two iterations each. The runner needs QEMU,
busybox-static, cpio, zstd, kmod, iproute2 and an installed generic BPF/BTF kernel with 9p modules;
the corpus also needs Python and Node.js. Serialize VM runs on small hosts.

Observed verification is recorded in §8.1.

### 8.1 Verification

Observed on this host (2 vCPUs, QEMU TCG without KVM, guest kernel 7.0.0-38-generic with BPF LSM):

* All nine programs pass the host kernel's verifier (`bpftool prog loadall`), and the three `lsm`
  tests pass in the guest in both the initial and a nested PID namespace.
* Leak corpus, seed `20261014`, one iteration: 32 case runs with zero failures in each mode, audit
  cross-checks included. `local-handoff-rewrite` and `toctou-peer-rewrite` are blocked in
  privileged mode.
* Seed `20261015`, two iterations with mutations: 64 case runs with zero failures in each mode.
  Unprivileged mode leaked `local-handoff-rewrite` in one iteration (its registered gap);
  privileged mode blocked it and `toctou-peer-rewrite` in every iteration.
* Unit suites: junco-policy 41 tests; `marsh-core` 171 of 173, the two failures needing a
  `git history` subcommand and a btrfs `HOME` this host lacks; `marsh-instrument` 16 of 17, with
  `native_observer_regressions` timing out in the full suite as it does on the previous commit
  (it passes alone).

An earlier corpus run found the reader race §4.7's return-time hooks close: a relay already
blocked in `recv` when tainted bytes arrived was not tainted by an entry-time hook.

## 9. Open questions

Questions about completeness (untraced services, shared kernel tables, shared caches,
delivery-time taint) are decisions in [`threat-model.md`](threat-model.md) §10.

1. **Stale-window denials.** Committed-history changes and multi-class receipts refuse every
   guarded class until republished. Measure false denials with audit mode on the Harbor benchmark.
2. **Loading and privileges.** Should a small privileged helper own the programs and hand map
   descriptors to an unprivileged marsh?
3. **Filesystem race.** An LSM `file_open` hook could record secret opens, so the answers' inputs
   are race-free too; it needs a filesystem observer that trusts the kernel's path.
4. **IP receiver ownership.** A `cgroup/post_bind` program could record which root owns each bound
   port, replacing `LOCAL_ALLOWED`'s `/proc` scan for `untraced_local(Deny)`.
5. **junco-policy.** Merge `feat/incremental-trace` upstream and repin the workspace to the merged
   branch.
