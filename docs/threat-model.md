# Threat model and complete mediation for privileged enforcement

Status: **phases 1 and 2 implemented and verified in the BPF LSM VM** (§9 steps 1–2); the rest is
design. It defines what the privileged mode must guarantee, enumerates every kernel channel
against that guarantee, and lays out how to enforce it comprehensively. The current
implementation is in [`privileged-enforcement.md`](privileged-enforcement.md); §7 below places
its known holes in this model.

Two bodies of prior work set the method:

* **Syscall interposition fails structurally.** Garfinkel's account of interposition tools
  ([NDSS 2003](https://www.ndss-symposium.org/ndss2003/traps-and-pitfalls-practical-problems-system-call-interposition-based-security-tools))
  lists the failure classes we kept rediscovering: incorrectly replicating OS semantics,
  overlooking indirect paths to resources, race conditions, incorrectly subsetting a complex
  interface. Watson ([WOOT 2007](https://www.usenix.org/event/woot07/tech/full_papers/watson/watson.pdf))
  shows that checking arguments the kernel has not yet copied is exploitable by construction. So
  nothing this model relies on may come from ptrace: ptrace stays for marsh's own filesystem
  bookkeeping and the unprivileged mode, which remains best effort.
* **Channels are found by enumeration over shared resources, not by attack.** Kemmerer's shared
  resource matrix ([TOCS 1983](http://sites.cs.ucsb.edu/~sherwood/cs290/papers/covert-kemmerer.pdf))
  derives every storage channel from the resources two subjects can modify and observe. DIFC
  operating systems (Flume, [SOSP 2007](https://pdos.csail.mit.edu/~yipal/papers/flume-sosp07.pdf);
  HiStar, OSDI 2006; Laminar, [PLDI 2009](https://mdbond.github.io/laminar-pldi-2009.pdf), on a
  Linux security module) make the enumeration finite by labelling every subject and object and
  checking every access in one reference monitor, refusing whatever has no rule. On Linux the
  enumeration of mediated accesses already exists: the LSM hook inventory
  (`include/linux/lsm_hook_defs.h`, 273 hooks in v7.0). Appendix A gives every hook a disposition.

## 1. Scope

* **Mode:** privileged enforcement only. It is the mode that can be complete; the unprivileged
  mode is documented as best effort against a non-adversarial agent.
* **Property:** confidentiality of *propagated classes* (`secret` in `recommended`): data of such a
  class never reaches a sink the policy forbids to a principal holding it, and never leaves
  marsh's governance at all.
* **Out of scope here:** the integrity rules decided at publication (`requires_approval`,
  `locked_after`, `deletion_budget`, `tainted_by_exec`), except where a write lets governed code
  escape governance (G4). Availability: every rule below fails closed, which can refuse work.

## 2. System model

| Entity | Meaning |
|---|---|
| **Domain** | One root: a shell's governed process tree, every thread and descendant of its commands, including io_uring workers. The unit of labelling: flows inside a domain are not tracked (as in Flume, a process is the smallest subject). |
| **Principal** | The policy identity; one or more domains. A domain's label is its principal's label. |
| **Label** | A set of propagated classes. Subjects (domains) and objects (inodes, sockets, ...) carry labels. Labels only grow, except by operator declassification. |
| **Private view** | The domain's btrfs view of the source. Writes inside it reach other principals only through publication, which marsh (the TCB) performs. |
| **Untraced world U** | Every task outside all domains and outside the TCB: the operator's own tools, system daemons, and anything started on a governed domain's behalf by a service it contacted. |
| **TCB** | The kernel, the marsh daemon and tracer, the BPF programs and maps, the policy, the operator, and services the operator explicitly allowlists. |
| **Sinks** | Network peers by class (`external`, `unknown`, declared networks, ...); U; any other domain whose principal may not hold the class. |

## 3. Adversary

The agent controls arbitrary code inside its domains:

* **A1.** Any unprivileged syscall, including io_uring, `sendmmsg`/`recvmmsg`, splice, `mmap`,
  SysV IPC, keyrings and netlink, with any number of threads and processes.
* **A2.** Races against any check that reads its memory: threads, `MAP_SHARED` memory, shared
  io_uring queues, other processes it controls.
* **A3.** State prepared before it reads anything secret: mappings, sockets, descriptors passed to
  others, files, bound names and ports.
* **A4.** Collusion with processes outside its domain that it caused to exist: services started
  through a service manager it contacted (`systemd-run --user`, D-Bus activation, Docker),
  scripts it wrote that untraced programs later execute, other principals' domains.
* **A5.** Covert behaviour: encoding data in names, metadata, process attributes, port numbers,
  timing.

It does not have: kernel compromise, hardware side channels, physical access, any capability
(stripped at launch), the ability to change the policy or the TCB, or the operator's
cooperation.

## 4. Goals and non-goals

* **G1, egress.** A domain holding class *c* never contacts a peer class the policy forbids after
  *c*, judged on the kernel's own copy of the destination, for every packet that leaves.
* **G2, propagation.** Every explicit flow of data between domains through a kernel object
  transfers the source's label to the destination before the destination can use the data.
* **G3, untraced boundary.** Under `untraced_local(Deny)`, no flow transfers a nonempty label to U.
* **G4, no escape.** Governed code cannot cause code it influences to run outside governance:
  no privilege, no new identity or namespace, no control of other processes, no writes that
  untraced programs later execute, no service that runs commands on its behalf unless the
  operator allowlisted that service.
* **G5, completeness and fail-closed.** Every LSM hook has a disposition; an operation on an
  object kind without a rule is refused; a kernel with hooks the matrix does not know refuses
  privileged startup; loss of state poisons rather than permits.

Non-goals: timing and resource-contention channels, hardware side channels, kernel bugs, the
operator reading output (the terminal is a declassification to a trusted human), denial of
service.

## 5. Label model

`L(x)` is a label; `S` a governed domain; `O` an object; `U` the untraced world.

* **R1, classification.** An inode whose name and location match a propagated class carries that
  class. The kernel evaluates it when the name is created, renamed or linked; a TCB scan labels
  existing files when a session starts. Hard links share the inode's label.
* **R2, read.** When `S` reads `O`'s content or metadata, maps it, or receives data from it,
  `L(S) ∪= L(O)` *before* the data reaches `S`'s memory.
* **R3, write and delivery.** When `S` writes `O`, `L(O) ∪= L(S)`, and every *other* domain
  currently holding `O` for reading (open, mapped, a socket or pipe it can read) acquires `L(S)`
  at once. Labelling holders at delivery, not at their later read, removes every window in which
  another thread of the reader could use bytes before its label changes.
* **R4, standing writes.** A shared writable mapping is a write no hook sees. `S` may hold one of
  `O` only while no other domain and no untraced task holds `O`; while it does, other opens and
  mappings of `O` are refused.
* **R5, egress.** A contact is judged by the domain's answers including labels acquired in the
  kernel but not yet folded in (the existing contingent answers).
* **R6, untraced boundary.** Under Deny, any operation through which U would acquire a nonempty
  label is refused, on whichever side the kernel can see. Under Allow, U is a trusted sink.
* **R7, escape.** Governed tasks write only beneath their own writable roots, never change
  identity, privileges, mounts or namespaces, never inspect or control other domains' processes,
  never inject terminal input, and contact a local service outside all domains only when it is
  traced or allowlisted, carrying taint or not.
* **R8, default deny.** An operation by a governed task on an object kind without a rule is
  refused; every hook absent from the matrix fails privileged startup.

R2 and R3 make every label acquisition a kernel *receipt*: the mechanism already used for socket
taint (counters per class, `contingent` answers until userspace folds the receipt in)
generalizes to files, pipes, mappings and descriptors.

## 6. Channel matrix

Families of shared resources, the rule that governs each, and where the implementation stands.
Appendix A assigns every hook to one of these rows.

| # | Family | Objects | Rule and mechanism | Today | Gap |
|---|---|---|---|---|---|
| 1 | Taint sources | files of a propagated class | R1, R2: inode labels; `file_permission` (`read`, io_uring, splice, `sendfile`, `copy_file_range`, reflink), `mmap_file` | phase 2: kernel classification (§8.3) and read receipts | classes whose patterns do not compile refuse the shell |
| 2 | File content between domains | regular files, directories, symlinks | R2, R3 on inode storage | userspace carriers (path, inode) from ptrace | the same races |
| 3 | Shared memory | `MAP_SHARED` files, memfd, `/dev/shm`, SysV shm | R4; per-domain `/dev/shm` and IPC namespace | mapping counted once, at `mmap` | stores after a secret read are unseen |
| 4 | Pipes and FIFOs | pipefs and FIFO inodes | R3 at delivery to holders | ptrace inode carriers | reader blocked before data arrives |
| 5 | Local sockets | UNIX, IP loopback | R3 at delivery (`unix_may_send`, `socket_sendmsg`, `socket_sock_rcv_skb`) | kernel tags, receipts on read return | window between the copy and the receipt |
| 6 | Netlink | user ports and groups | R6, R7: user destinations are local services | allowed, untagged | relay to U or other principals |
| 7 | Network egress | IP, VSOCK, SCTP, MPTCP | R5 at connect and send; per packet for datagrams; MPTCP subflows and SCTP addresses judged | kernel answers at connect and send | connected-UDP reconnect, MPTCP and SCTP extra addresses, host's own addresses, `sendmmsg` batches |
| 8 | Descriptor passing | `SCM_RIGHTS`, `pidfd_getfd`, binder | `file_receive` makes the receiver a holder (R3) | none | a passed pipe or memfd skips socket tags |
| 9 | Process to process | `ptrace`, `/proc/<pid>/*`, signals, `sigqueue` payloads, `prlimit`, nice, `PR_SET_PTRACER` | R6, R7: no inspection or control across domains; `/proc` inodes labelled with their task's domain | phase 1: attributes of another cgroup's tasks and `PR_SET_PTRACER`/`PR_SET_MM` refused; signals scoped by Landlock; Yama | storage channels to U (`/proc/<pid>/cmdline`, `comm`, `oom_score_adj`) |
| 10 | Names and metadata in shared directories | `/tmp` names, xattrs, timestamps, sizes | R7 write confinement (Landlock) and per-domain `/tmp`; labels within writable roots | none | file names in shared directories are a high-bandwidth channel to U |
| 11 | Escape to untraced execution | uid-0 writes, rc files, autostart, systemd units, service managers, `TIOCSTI` | R7 | phase 1: non-root identity, Landlock write confinement, private namespaces, BPF refusals | service managers under Allow |
| 12 | Keyrings | user, session keyrings | per-domain session keyring; user keyring read-only | phase 1: own session keyring; writing, linking or changing keys the root did not allocate refused | reading the user keyring is not labelled |
| 13 | SysV IPC, POSIX queues, watch queues | message queues, semaphores | per-domain IPC namespace; B across domains | phase 1: private IPC namespace | POSIX queues and watch queues outside the IPC namespace |
| 14 | Shared kernel tables | `/proc/net/*` ports and abstract names, PID allocation, locks | decision: per-domain network namespace, or accept with bandwidth bound | none | port numbers and abstract names visible to U |
| 15 | io_uring | rings, personalities, passthrough | operations reach rows 1–8; refuse SQPOLL, personalities, `uring_cmd` | phase 1: SQPOLL, personalities and `uring_cmd` refused; opcodes checked by ptrace | operations reaching unimplemented rows 2–4 and 8 |
| 16 | TCB-mediated | rmux panes, terminal escape sequences | rmux treats pane output as the pane principal's data; filters OSC 52 clipboard writes | not considered | clipboard to U |
| 17 | Timing and contention | CPU, caches, locks, allocation | non-goal | — | — |
| 18 | The interpreter's own accesses | redirections, `source`, tests, globbing, `PATH` search, marsh builtins: run in the daemon | R7: each access runs with the commands' identity, Landlock domain and private paths; the direct route is refused | phase 1 | — |

## 7. What the earlier holes were

Every hole found so far is an instance of a row above, which is why they kept appearing.

| Hole | Row | Rule that closes it |
|---|---|---|
| `openat` path swapped after the tracer reads it | 1 | R1, R2: the label is on the inode the kernel opens |
| io_uring submission queue rewritten after inspection | 1, 15 | R2: io_uring reads pass `file_permission` like any read |
| thread sends between a secret read and its exit stop | 1 | R2: the read is a receipt before the data is copied |
| shared writable mapping set up before the read | 3 | R4 |
| `NETLINK_USERSOCK` relay | 6 | R6, R7 |
| host's own LAN address reaches local listeners | 7 | R5 with the namespace's own addresses classified local |
| DNS through the local resolver under `recommended` | 11 | R7: an untraced service is a sink unless allowlisted |
| connected-UDP peer reconnect, `sendmmsg` batches | 7 | per-packet judgement for datagrams |
| uid 0 retained: owner writes to `/etc/cron.d`, `/etc/systemd/system` | 11 | R7: governed tasks run as a non-root uid (phase 1) |
| builtins and redirections run as the root daemon, outside Landlock | 18 | R7: interpreter accesses take the commands' authority (phase 1) |
| `~/` secret classes expand the daemon's home, not the governed account's | 1 | classes follow the governed identity's home (phase 1) |
| `toctou-peer-rewrite`, `local-handoff-rewrite` | 5, 7 | already closed in privileged mode by kernel hooks |

## 8. Enforcement architecture

### 8.1 Domains in the kernel
Each root runs in its own cgroup v2 (`marsh-<daemon pid>/root-<id>`, owned by root). BPF
identifies a task's domain with `bpf_get_current_cgroup_id` in the `DOMAINS` map. Membership is
inherited by `fork` and io_uring workers inside the kernel, survives PID namespaces, and cannot be
left by a non-root task; it replaced the tgid-keyed `TASK_ROOT` and its fork-stop races (phase 1).
The same cgroup can attach `cgroup_skb/egress` for per-packet judgement (row 7).

### 8.2 Launch confinement (in `restrict_child`)
* Switch to the governed identity (the source's owner, or a configured non-root user) before
  exec, in a session keyring of its own; keep `NO_NEW_PRIVS` and the empty capability sets.
* Apply a Landlock ruleset (inherited, not removable): writes beneath the private view, the
  domain's private `/tmp` and `/dev/shm`, the source's shared scratch, an operator allowlist, and
  `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/tty` only; `LANDLOCK_SCOPE_SIGNAL` and
  `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`. Separate Landlock domains per root also refuse ptrace and
  ptrace-gated `/proc` access across roots, and every mount.
* Unshare a mount namespace with private `/tmp` and `/dev/shm` and the shared scratch at
  `/tmp/shared`, and an IPC namespace.
* BPF refusals for the D rows of Appendix A: user namespaces, BPF, perf, `PR_SET_PTRACER` and
  `PR_SET_MM`, cross-domain process attributes, terminal injection, io_uring personalities and
  passthrough, writes and links to keys the domain did not create.
* The interpreter's own filesystem accesses for the root run on a short-lived thread with the same
  identity and Landlock domain, on the paths its commands see (row 18); the direct route, whose
  commands would run as the daemon, is refused.
* Daemon-side writes into a view or the seed (publication) take their directory's owner, so the
  governed identity can still change what marsh wrote for it.

### 8.3 Kernel classification (R1)
Class patterns compile to anchored forms the kernel can evaluate: name rules (a literal, or one
`*` between literal parts of at most 32 bytes) that apply in directories holding a scope bit, and
the directories that start with those scopes. A leading `**` gives a deep scope, which
subdirectories inherit; any other `**` must end the pattern and gives the matched directory's
subtree the class. Relative patterns anchor at each view's root; absolute and `~/` ones at their
deepest existing directory, so a rule still names what is replaced beneath it. Scope bits are
shared by every policy (64 at most), and every root's rules are joined in one table of 32.

Labels live in inode storage, which dies with the in-memory inode, so the kernel recomputes them
in `d_instantiate`, which every creation and every lookup passes: an inode carries what its name
and its parent's label give it. Anchors are held open by the controller, and a dentry pins its
parent, so every loaded inode beneath an anchor is reached from labelled parents. When the root
is classified, userspace labels what was already loaded beneath each anchor. `inode_rename` and
`inode_link` add the new name's label at once; whatever the inode carries that its new name would
not give back is retained by `(device, inode number)` and restored at every load, until
`inode_free_security` frees it with no links left. A failure to label poisons enforcement.

Only propagated classes are classified; a propagated class's pattern that does not compile refuses
the shell. The kernel adds every matching class where the policy takes the first matching
pattern, which can only over-taint. `file_permission` (`MAY_READ`), `mmap_file`
(`PROT_READ`/`PROT_EXEC`) and `bprm_check_security` make a governed task's read of a classified
inode a receipt with sender `CLASSIFIED` before the data reaches it. Residuals: a directory moved
into a scope keeps its loaded entries' old labels until they are evicted, which only matters for
files the agent placed there itself; btrfs snapshots share inode numbers, so a retained label
can over-taint the same file in another view.

### 8.4 Label stores
Inode storage for files, directories, pipes, socket inodes and `/proc` inodes (via
`task_to_inode`); socket storage for sockets (owner domain, traced status, taint, holders); cgroup
storage for domain labels and receipt counters. Holders are recorded at `file_open`,
`file_receive` and `mmap_file`, and released at `file_release`.

### 8.5 Receipts everywhere (R2, R3)
Each label acquisition increments the domain's per-class counter, exactly as socket receipts do
now; contacts compare counters with acknowledged ones and use `contingent` answers in the
meantime. Userspace folds receipts into the observation log and republishes. The class limit is
16 today; widening the counters to 64 classes removes the fail-closed refusal for most policies.

### 8.6 What stays in userspace
Policy decisions and answers, folding receipts into history, and the filesystem bookkeeping marsh
needs for publication. ptrace observation remains for that bookkeeping, but nothing in this model
depends on it.

### 8.7 Inventory gate (G5)
A machine-readable copy of Appendix A lives next to the programs. At load, the controller lists
the running kernel's `bpf_lsm_*` BTF functions and refuses privileged startup if any hook is
missing from the matrix. A CI test fails when the matrix and a supported kernel's hook list
diverge.

## 9. Plan

Each phase ends with its hooks in Appendix A enforced, an `lsm` test per rule, and a leak-corpus
case per hole it closes.

1. **Escape and identity (row 11, 9, 12, 13, 15).** Non-root uid, Landlock confinement, private
   `/tmp`, `/dev/shm` and IPC namespace, cgroup domains, the D refusals. Closes the uid-0 escape,
   persistence writes, terminal injection and process-to-process channels.
2. **Kernel taint sources (row 1).** Classification, `file_permission`, `mmap_file` and exec
   receipts (implemented). Closes the path swap, the io_uring queue race and the thread window;
   io_uring needs no ptrace check for confidentiality.
3. **Object labels and delivery (rows 2–5, 8).** Inode labels, holders, R3 delivery for files,
   pipes and sockets (sockets move from read-return receipts to delivery), R4.
4. **Untraced boundary (rows 6, 11).** R6 on every side the kernel sees, netlink user ports,
   service allowlist.
5. **Network completion (row 7).** Per-packet egress for datagrams, MPTCP subflows, SCTP
   addresses, the namespace's own addresses as local.
6. **Gate and residuals (rows 14, 16, 17).** The inventory gate, rmux clipboard filtering, the
   row 14 decision, and bandwidth notes for what is accepted.

## 10. Decisions

1. **Deny is the complete mode.** Under `untraced_local(Allow)`, U is trusted and a service
   manager can run governed code ungoverned; G3 and G4 hold only under Deny with an allowlist.
   `recommended` should say so, or default to Deny in privileged mode.
2. **Service allowlist.** Which untraced services (database, Docker, SSH agent, D-Bus) a domain
   may reach, and whether they are sinks for tainted data.
3. **Shared kernel tables (row 14).** A per-domain network namespace removes port numbers and
   abstract names from U's view but needs marsh to route egress; otherwise they are accepted
   storage channels (a bind carries 16 bits, so the rate needs a bound: TCSEC treats storage
   channels above 100 bit/s as high bandwidth).
4. **Shared caches.** Writable shared caches (`~/.cache/pip`, `~/.npm`) carry labels to U; either
   per-domain overlays or labelled entries that U is refused under Deny.
5. **Delivery-time socket taint.** R3 taints every holder of a receiving socket, not only readers:
   race-free, at the cost of over-tainting holders that never read.

## 11. Accepted residual risks

Timing and contention channels; hardware side channels; kernel and TCB bugs; storage channels
through shared kernel tables if decision 3 accepts them; output shown to the operator.

## Appendix A: every LSM hook (Linux v7.0)

Dispositions: **T** label flow (R2, R3); **W** write confinement (R7); **B** boundary between
domains and U (R6, R7); **E** egress judgement (R5); **D** refused for governed tasks; **C**
accepted covert channel; **Label** maintains labels or domain identity; **N** carries no data across
domains. `*` marks hooks BPF LSM cannot attach to (`bpf_lsm_disabled_hooks`); all are N.

### Domain identity, credentials, exec

| Hooks | Disposition |
|---|---|
| `task_alloc` | Label: a child inherits its parent's domain (cgroup) before it runs |
| `task_free`, `cred_alloc_blank`, `cred_free`, `cred_prepare`, `cred_transfer`, `cred_getsecid`, `cred_getlsmprop`, `current_getlsmprop_subj`, `task_getlsmprop_obj`, `kernel_act_as`, `kernel_create_files_as` | N |
| `bprm_creds_for_exec`, `bprm_creds_from_file`, `bprm_check_security`, `bprm_committing_creds`, `bprm_committed_creds` | N: exec keeps the domain; `NO_NEW_PRIVS` forbids gaining privilege |
| `task_fix_setuid`, `task_fix_setgid`, `task_fix_setgroups` | D: governed tasks never change identity |
| `capget`, `capset`, `capable` | D: governed tasks hold no capabilities (defence in depth) |
| `userns_create` | D: a user namespace would return capabilities inside it |

### Process to process

| Hooks | Disposition |
|---|---|
| `ptrace_access_check`, `ptrace_traceme` | B: only the tracer may inspect a governed task; governed tasks may not inspect another domain (`ptrace`, `/proc/<pid>/mem`, `process_vm_*`, `pidfd_getfd`, `kcmp`) |
| `task_kill`, `file_send_sigiotask` | B: no signal (and no `sigqueue` payload, no `F_SETOWN` delivery) from a governed domain to another domain |
| `task_prctl` | B: refuse `PR_SET_PTRACER`, `PR_SET_MM`; `PR_SET_NAME` is covered by the `/proc` rule |
| `task_to_inode` | Label: tags a task's `/proc` inodes with its domain, so the file rules apply to them |
| `task_setpgid`, `task_setnice`, `task_setioprio`, `task_setscheduler`, `task_prlimit`, `task_setrlimit`, `task_movememory` | B: a governed domain may not change another domain's process attributes (each is a storage channel) |
| `task_getpgid`, `task_getsid`, `task_getioprio`, `task_getscheduler` | N: reading another process's attributes flows inward only |

### File content and descriptors

| Hooks | Disposition |
|---|---|
| `file_permission` | T: read labels the reader; write labels the file and, at once, every domain holding it open or mapped for reading (covers `read`, `write`, io_uring, `splice`, `sendfile`, `copy_file_range`, reflink) |
| `file_open`, `file_post_open` | T: records readers/holders; refuses another domain's or untraced task's open of an object whose label it may not hold |
| `mmap_file`, `file_mprotect` | T: a mapping is a standing read (and, shared and writable, a standing write); cross-domain standing writes are refused |
| `mmap_addr` | N |
| `file_truncate` | T: a write |
| `file_receive` | T: a descriptor crossing domains adds the receiver as a holder of the object |
| `file_ioctl`, `file_ioctl_compat` | D: refuse `TIOCSTI`/`TIOCLINUX` (terminal injection); reflink `FICLONE*` is a write via `file_permission` |
| `file_fcntl`, `file_set_fowner` | N: `F_SETOWN` delivery is judged at `file_send_sigiotask` |
| `file_lock` | C: lock state is a low-bandwidth storage channel on shared files; private namespaces leave few shared writable files |
| `file_release`, `file_free_security`, `file_alloc_security`\* | N: ends a holder or mapping record |

### Filesystem names and metadata

| Hooks | Disposition |
|---|---|
| `path_unlink`, `path_mkdir`, `path_rmdir`, `path_mknod`, `path_post_mknod`, `path_truncate`, `path_symlink`, `path_link`, `path_rename`, `path_chmod`, `path_chown`, `path_chroot` | W: write confinement: a governed domain changes names and metadata only beneath its own writable roots (Landlock); within them, a change labels the directory and inode |
| `path_notify` | N: watches observe only names inside the observer's view of the namespace; governed writes land in private namespaces |
| `inode_create`, `inode_post_create_tmpfile`, `inode_link`, `inode_unlink`, `inode_symlink`, `inode_mkdir`, `inode_rmdir`, `inode_mknod`, `inode_rename` | W, Label: as `path_*`; `inode_link` and `inode_rename` add the new name's classification and retain what it would not give back (§8.3); created inodes are classified at `d_instantiate` |
| `inode_setattr`, `inode_post_setattr`, `inode_setxattr`, `inode_post_setxattr`, `inode_removexattr`, `inode_post_removexattr`, `inode_set_acl`, `inode_post_set_acl`, `inode_remove_acl`, `inode_post_remove_acl`, `inode_file_setattr`, `inode_killpriv`, `inode_setsecurity`, `inode_setintegrity` | W: metadata writes are writes |
| `inode_getattr`, `inode_getxattr`, `inode_listxattr`, `inode_get_acl`, `inode_file_getattr`, `inode_readlink`, `inode_follow_link` | T: metadata reads are reads of the inode's label (names and sizes carry data) |
| `inode_alloc_security`, `inode_free_security`, `inode_free_security_rcu`, `inode_init_security`, `inode_init_security_anon`, `dentry_init_security`, `dentry_create_files_as`, `d_instantiate`, `kernfs_init_security`, `inode_getlsmprop`, `inode_copy_up`, `inode_copy_up_xattr`\*, `inode_xattr_skipcap`, `inode_need_killpriv`\*, `inode_getsecurity`\*, `inode_listsecurity`\*, `inode_invalidate_secctx`, `inode_notifysecctx`, `inode_setsecctx`, `inode_getsecctx` | N: label storage lifecycle and LSM-attribute plumbing |
| `inode_permission` | N: lookup permission; names in shared directories are excluded by write confinement |

### Mounts and superblocks

| Hooks | Disposition |
|---|---|
| `sb_mount`, `sb_umount`, `sb_remount`, `sb_pivotroot`, `move_mount`, `fs_context_submount`, `fs_context_dup`, `fs_context_parse_param`, `sb_kern_mount`, `sb_set_mnt_opts`, `sb_clone_mnt_opts` | D: governed tasks never change mounts (also needs capabilities) |
| `sb_alloc_security`, `sb_delete`, `sb_free_security`, `sb_free_mnt_opts`, `sb_eat_lsm_opts`, `sb_mnt_opts_compat`, `sb_show_options`, `sb_statfs` | N |
| `quotactl`, `quota_on` | D |

### Sockets and network

| Hooks | Disposition |
|---|---|
| `socket_connect`, `socket_sendmsg`, `unix_may_send` | E, T: judged against the domain's answers on the kernel's address; local receivers labelled at delivery |
| `socket_sock_rcv_skb` | T: delivery-time labelling of every domain holding an IP receiving socket (loopback tags) |
| `socket_recvmsg` | T: read-time labelling for data already queued; delivery-time labelling covers data that arrives later |
| `socket_create`, `socket_post_create`, `socket_socketpair`, `unix_stream_connect`, `socket_accept`, `sock_graft`, `sk_alloc_security`, `sk_clone_security`, `inet_csk_clone` | Label: a socket's owning domain and traced status; accepted and cloned sockets inherit |
| `socket_bind` | B, C: abstract UNIX names and IP ports are visible to every task in the network namespace (`/proc/net/*`) |
| `socket_listen`, `socket_getsockname`, `socket_getpeername`, `socket_getsockopt`, `socket_shutdown`, `sk_getsecid`, `sk_free_security`, `inet_conn_request`, `inet_conn_established`, `socket_getpeersec_stream`, `socket_getpeersec_dgram`, `secmark_relabel_packet`, `secmark_refcount_inc`, `secmark_refcount_dec`, `req_classify_flow` | N: no payload; `sk_free_security` ends tags |
| `mptcp_add_subflow` | E: a subflow the kernel opens to an address the peer advertised is a contact; judge it |
| `sctp_assoc_request`, `sctp_bind_connect`, `sctp_sk_clone`, `sctp_assoc_established` | E: every address of `sctp_connectx`/`sendmsg` is a contact; refuse SCTP until judged per address |
| `netlink_send` | E, B: netlink to the kernel (port 0) is allowed; to a userspace port or group it is a local contact with an untraced or other-domain receiver |
| `tun_dev_alloc_security`, `tun_dev_create`, `tun_dev_attach_queue`, `tun_dev_attach`, `tun_dev_open`, `ib_pkey_access`, `ib_endport_manage_subnet`, `ib_alloc_security`, `xfrm_policy_alloc_security`, `xfrm_policy_clone_security`, `xfrm_policy_free_security`, `xfrm_policy_delete_security`, `xfrm_state_alloc`, `xfrm_state_alloc_acquire`, `xfrm_state_free_security`, `xfrm_state_delete_security`, `xfrm_policy_lookup`, `xfrm_state_pol_flow_match`, `xfrm_decode_session` | D: need capabilities; refuse explicitly |
| `socket_setsockopt` | N: options act on the domain's own socket; steering and filters that need capabilities are unreachable |

### SysV IPC and notifications

| Hooks | Disposition |
|---|---|
| `ipc_permission`, `msg_queue_associate`, `shm_associate`, `sem_associate`, `msg_queue_msgctl`, `shm_shmctl`, `sem_semctl` | B: SysV IPC objects are private to a domain (a per-root IPC namespace); no association across domains |
| `msg_queue_msgsnd`, `msg_queue_msgrcv`, `shm_shmat`, `sem_semop` | T within a domain only (no label needed); B across domains |
| `msg_msg_alloc_security`, `msg_msg_free_security`, `msg_queue_alloc_security`, `msg_queue_free_security`, `shm_alloc_security`, `shm_free_security`, `sem_alloc_security`, `sem_free_security`, `ipc_getlsmprop` | N |
| `post_notification`, `watch_key` | B: watch queues deliver to another domain's pipe; treat as a pipe write |

### Keyrings

| Hooks | Disposition |
|---|---|
| `key_alloc`, `key_permission`, `key_post_create_or_update` | B: governed tasks use a per-root session keyring; writes or links into user or other keyrings are refused |
| `key_getsecurity`\* | N |

### io_uring

| Hooks | Disposition |
|---|---|
| `uring_allowed` | N: rings are allowed; every operation reaches the same file and socket hooks |
| `uring_sqpoll`, `uring_override_creds`, `uring_cmd` | D: kernel-polled rings, personalities and passthrough commands |

### Kernel facilities

| Hooks | Disposition |
|---|---|
| `bpf`, `bpf_map`, `bpf_prog`, `bpf_map_create`, `bpf_prog_load`, `bpf_token_create`, `bpf_token_cmd`, `bpf_token_capable` | D: governed tasks never use BPF |
| `bpf_map_free`, `bpf_prog_free`, `bpf_token_free` | N |
| `perf_event_open`, `perf_event_read`, `perf_event_write` | D |
| `perf_event_alloc` | N |
| `syslog`, `settime`, `kernel_load_data`, `kernel_read_file`, `locked_down` | D: needs capabilities; refuse explicitly |
| `kernel_post_load_data`, `kernel_post_read_file`, `vm_enough_memory`\*, `initramfs_populated`, `bdev_alloc_security`, `bdev_free_security`, `bdev_setintegrity`, `audit_rule_init`, `audit_rule_known`, `audit_rule_match`\*, `audit_rule_free` | N |
| `binder_set_context_mgr`, `binder_transaction`, `binder_transfer_binder`, `binder_transfer_file` | D: binder is absent on servers; refuse if present |
| `getselfattr`\*, `setselfattr`, `getprocattr`\*, `setprocattr`\*, `ismaclabel`\*, `secid_to_secctx`, `lsmprop_to_secctx`, `secctx_to_secid`, `release_secctx` | N: LSM-attribute plumbing |
| `kernel_module_request` | N: autoloading a protocol module carries no payload |

## Appendix B: completeness check

The appendix was generated from `include/linux/lsm_hook_defs.h` at v7.0 and checked to name each
of its 273 hooks exactly once. §8.7 makes the same check part of privileged startup and CI.
