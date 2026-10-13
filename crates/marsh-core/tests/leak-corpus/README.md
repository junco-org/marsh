# Leak fuzzer corpus

Hand-written cases for the leak fuzzer (`bench/leak-fuzzer/PLAN.md`). Each
`*.toml` is one case the harness runs against a managed marsh shell judged by
an agent policy, then judges with the leak/false-denial oracle. `known-gaps.toml`
is not a case; it registers cases whose leaks are expected (see below).

## What each `kind` asserts

* `attack` — a secret's bytes must **not** reach an external sink. The case
  passes when the policy blocks the egress. Most model a real exfiltration
  technique; a few probe a specific enforcement point (a pre-connected socket,
  `sendfile`, `writev`, an IPv4-mapped IPv6 peer).
* `control` — legitimate traffic that must **not** be blocked. The canary must
  reach the sink; if it does not, the case fails as a false denial. Controls
  here never read a `secret`-classed path, so egress is allowed and the sink
  receives the canary.

Every attack also runs under `AgentPolicy::audit()` (done by the harness, not a
separate case): a leak must come with a "would block" report.

## Placeholders

The loader substitutes these before running a case. A `{NAME}` with no value
fails loading, so file contents and commands must not contain stray braces.

* `{CANARY}` — a random 24-char token minted per run; the secret's value.
* `{CANARY_FILE}` — the seeded secret's path (these cases read it by its literal
  path instead; both are equivalent).
* `{SINK_HOST}`, `{SINK_PORT}` — the external capture listener
  (`192.0.2.1`, a per-case port).
* Variant slots: `[variants.<slot>] = [...]` feeds the `{<SLOT>}` placeholder
  (uppercased). `encoded-exfil.toml` uses `enc` -> `{ENC}`.

## Conventions these cases rely on

* The external sink is `192.0.2.1` (TEST-NET-1), labelled `external`.
  `127.0.0.1` and unix sockets are `loopback`/`unix` and permitted by
  `recommended` — a control may deliver to them, and the loopback/unix proxy
  gaps abuse exactly that.
* `recommended` gates egress only after a `secret`-classed read, per reading
  principal. A shared `principal` field makes several shells one principal; its
  absence makes each shell its own (used by the cross-principal gap).
* Secrets are read by their literal seeded path. Scripts avoid Python 3.10+
  features and any `{`/`}` that is not a placeholder (no dict/set literals, no
  f-strings, no JSON braces in seeded contents).

## Known gaps

Cases listed in `known-gaps.toml` leak by design: they exercise paths
`recommended` does not cover (cross-principal staging, loopback/unix proxying,
io_uring socket writes, a TOCTOU peer rewrite). Their leaks report "known gap"
rather than failing, so only a new, unlisted leak breaks the build. Close a gap
in the policy, then delete its entry and the case becomes a must-block attack.
