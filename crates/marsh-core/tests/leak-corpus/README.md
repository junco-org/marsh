# Leak fuzzer corpus

Hand-written cases for the leak fuzzer (`bench/leak-fuzzer/PLAN.md`). Each
`*.toml` is one case the harness runs against a managed marsh shell judged by
an agent policy, then judges with the leak/false-denial oracle. `known-gaps.toml`
is not a case; it registers cases whose leaks are expected (see below).

## What each `kind` asserts

* `attack` — a secret's bytes must **not** reach an external sink. The case
  passes when the policy blocks the egress. Most model a real exfiltration
  technique; a few probe a specific enforcement point (a pre-connected socket,
  `writev`, an IPv4-mapped IPv6 peer, `sendfile`/`splice` copying in-kernel, an
  io_uring `WRITE` to a connected socket).
* `control` — legitimate traffic that must **not** be blocked. The canary must
  reach the sink; if it does not, the case fails as a false denial. Controls
  here never read a `secret`-classed path, so egress is allowed and the sink
  receives the canary.

Every attack iteration is replayed under `AgentPolicy::audit()` with the same
variant and mutation. The audit canary must reach a sink; ordinary attacks also
require a network "would block" report. Registered gaps waive only that report.
Foreground timeouts and shell errors are inconclusive. Timeouts from declared
`&` server steps are expected only when the later sink/audit oracle proves the
operation ran. Unsupported attack effects need a network denial plus successful
audit delivery; without both, they are inconclusive.

Run the same corpus with `MARSH_LEAK_ENFORCEMENT=unprivileged` or `privileged`.
The latter requires `--features privileged` and an active BPF-LSM kernel; it
never skips or falls back if unavailable. `bench/leak-fuzzer/privileged-vm.py`
provides an isolated runner when the host's boot-time LSM list lacks `bpf`.
Root runs use a network namespace without dropping initial-user-namespace
BPF authority. See `docs/privileged-enforcement.md` for build tools.

`expect = "accepted-privileged"` requires successful publication in kernel
mode while allowing the unprivileged filesystem observer to refuse unsupported
ring operations. Both modes still require a control's canary to be delivered.
The SEND control forces an io-wq worker; the privilege-boundary control checks
capability stripping and loader descriptor isolation.

Iterations after the first sample declared variants, shuffle optional steps,
and insert one or two neutral `true`/`pwd`/`ls` commands. The attack recipes
remain hand-written; the harness does not generate arbitrary commands.
Pull requests run fixed seed `20261014` once. Nightly CI runs seeds
`20261014`–`20261017` twice each in both enforcement modes.

## Placeholders

The loader substitutes these before running a case. A `{NAME}` with no value
fails loading, so file contents and commands must not contain stray braces.

* `{CANARY}` — a random 24-char token minted per run; the secret's value.
* `{CANARY_FILE}` — the seeded secret's path (these cases read it by its literal
  path instead; both are equivalent).
* `{SINK_HOST}`, `{SINK_PORT}` — the external capture listener
  (`192.0.2.1`, a per-case port).
* `{LOCAL_SINK_PORT}` — a capture listener on `127.0.0.1` standing in for a
  local daemon marsh does not trace. What reaches it counts as leaked too.
* Variant slots: `[variants.<slot>] = [...]` feeds the `{<SLOT>}` placeholder
  (uppercased). `encoded-exfil.toml` uses `enc` -> `{ENC}`.

## Conventions these cases rely on

* The external sink is `192.0.2.1` (TEST-NET-1), labelled `external`.
  `127.0.0.1` and unix sockets are `loopback`/`unix` and permitted by
  `recommended` — a control may deliver to them. The `proxy-via-*` cases relay
  through them; a tainted contact taints the relay's principal instead.
* `policy` is `recommended`, or `untraced-local-deny` for `recommended` with
  `untraced_local(Deny)`, which refuses a principal holding a secret any local
  peer no traced command holds.
* `recommended` gates egress only after a `secret`-classed read, per reading
  principal. A shared `principal` field makes several shells one principal; its
  absence makes each shell its own (used by the cross-principal cases).
* Secrets are read by their literal seeded path. Scripts avoid Python 3.10+
  features and any `{`/`}` that is not a placeholder (no dict/set literals, no
  f-strings, no JSON braces in seeded contents).

## Known gaps

Entries may select an `enforcement` mode. A named-sockaddr rewrite is a known
gap only for ptrace; kernel mode must block it, and must taint a local relay
whatever port a rewritten address named (`local-handoff-rewrite`).
`recommended`'s allowance for untraced daemons remains deliberate in both
modes. The shared connected-UDP peer-state race is a documented residual
without a deterministic case, not a claim of test coverage.

Registered leaks report "known gap" rather than failing. A new, unlisted leak
breaks the build; remove an entry once its case is actually closed.
