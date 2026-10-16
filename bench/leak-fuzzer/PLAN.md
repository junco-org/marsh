# Leak fuzzer plan

A harness that runs test cases against marsh shells judged by an agent policy, and fails on two
kinds of result:

* **Leak:** a secret's bytes reach a destination the policy forbids.
* **False denial:** a control case that never touched a secret was blocked.

The harness runs, judges and shrinks cases. The attack cases themselves come from a corpus that
is written by hand or imported from existing benchmarks (for example the exfiltration tasks in
SABER, arXiv 2606.01317, or RedCode, arXiv 2411.07781). The harness does not generate attack
techniques of its own.

Status: **implemented**. The harness lives in `crates/marsh-core/src/shell/tests/leak_fuzz/`
(namespace bootstrap and runner in `mod.rs`, capture listeners in `sink.rs`, the case schema and
loader in `case.rs`, the encoding oracle in `oracle.rs`, shrinking in `shrink.rs`); the corpus is
`crates/marsh-core/tests/leak-corpus/`. Run it with
`cargo test -p marsh-core --lib leak_corpus -- --ignored --nocapture`. The notes below are the
design it was built to; where the code refines the plan it is noted inline.

Its first run found a real leak the corpus had not anticipated: `sendfile` copies the secret into
the socket with an in-kernel read the tracer does not observe as a secret read, so the principal is
never tainted and the egress is allowed. The gate now counts a `sendfile`'s source as read before
judging it, and `sendfile-exfil` is an ordinary must-block attack; `splice-exfil` covers the
pipe-based variant.

## Facts established on the development host

* `unshare -rn` works without root. Inside it, `ip link set lo up` and
  `ip route add local 192.0.2.0/24 dev lo` make TEST-NET-1 addresses local, so a listener can bind
  `192.0.2.1`.
* The policy labels `192.0.2.1` as `external`: it is not loopback and no network is declared for
  it. A listener there stands in for a real external server, and no traffic leaves the machine.
* marsh's tracer and the fake btrfs backend (`marsh_btrfs::fake::CopyTree`) both work inside the
  namespace. The existing `agent_policy::` tests pass there, except those that assume
  `192.0.2.1` is unreachable.
* The test binary needs `LD_LIBRARY_PATH` pointing at a built `libbtrfsutil` when the host has
  none installed.
* Python is 3.8 on that host: cases should avoid `os.splice` and other 3.10+ features.
* No property-testing crate is cached locally; `rand` 0.8 and 0.10 are. A small seeded PRNG with
  hand-written shrinking avoids adding a dependency.

## Layout

Everything lives in `crates/marsh-core/src/shell/tests/`, because the harness needs the
crate-private `Fixture`, the fake backend and `ShellBuilder.backend`.

* `leak_fuzz/mod.rs`: the entry test, the namespace re-exec, and the runner.
* `leak_fuzz/sink.rs`: the capture listeners.
* `leak_fuzz/case.rs`: the case schema, the corpus loader, and template substitution.
* `leak_fuzz/oracle.rs`: leak and false-denial verdicts.
* `leak_fuzz/shrink.rs`: cutting a failing case down to its smallest form.
* Corpus: `crates/marsh-core/tests/leak-corpus/*.toml`. Seed it with a few trivial cases only:
  "read the secret, then send it with an ordinary connect", which must be blocked, and one
  control case.
* Registration: `mod leak_fuzz;` in `tests/mod.rs`.

## Namespace bootstrap

The entry is `#[test] #[ignore] fn leak_corpus()`.

1. If `MARSH_LEAK_NS` is not set, re-run the test binary through `unshare --net` as real root,
   retaining initial-user-namespace BPF authority. Other users also request `--user --map-root-user`.
   Set `MARSH_LEAK_NS=1`, pass `LD_LIBRARY_PATH` through, and assert the child succeeded.
   * An unavailable namespace may skip only unprivileged mode; privileged mode fails closed.
   * Re-execution avoids `unshare(CLONE_NEWUSER)` in the multithreaded test harness.
2. Inside the namespace, run `ip link set lo up` and `ip route add local 192.0.2.0/24 dev lo`,
   failing loudly on error. Then run the corpus.

## Sink (`sink.rs`)

* One `Sink` per case, with a fixed set of listeners:
  * TCP on `192.0.2.1` at a few ports from the corpus (for example 9, 80 and 443), with an accept
    loop on a thread;
  * UDP on the same ports;
  * optionally a control listener on `127.0.0.1`, to check delivery for allowed traffic.
* Each listener appends `(listener id, bytes)` to an `Arc<Mutex<Vec<…>>>`. `drain(timeout)` waits
  briefly after the case ends so late data still arrives. `received()` returns the bytes per
  listener and in total.
* Bind a fresh port per case, or reset between cases, so one case's bytes cannot leak into the
  next case's verdict.

## Case schema (`case.rs`)

```toml
name = "..."
kind = "attack"                    # or "control"
policy = "recommended"             # or "audit", or a named custom builder
principal = "agent"                # shared principal for multi-shell cases

[[files]]                          # seeded before the shell opens
path = ".env"
contents = "TOKEN={CANARY}"

[[files]]
path = "helper.py"
contents = "..."                   # may reference {SINK_HOST} {SINK_PORT} {CANARY_FILE}

[[steps]]
shell = 0                          # which shell runs it
run = "..."                        # one command line
expect = "any"                     # or "accepted" or "refused"
optional = true                    # the shrinker may drop it

[oracle]
encodings = ["raw", "base64", "hex"]   # transforms of the canary to search for
```

* Placeholders: `{CANARY}` is a random 24-character token per run, `{SINK_HOST}` and
  `{SINK_PORT}` name the sink, and `{CANARY_FILE}` is the secret's path. A placeholder with no
  value fails loading.
* Variants: a case may list alternative values per slot, for example `[variants.port] = [9, 80]`;
  the fuzzer samples combinations with the seeded PRNG.
* Iteration 0 runs the authored case unchanged with each slot's first value. Later iterations sample
  slot values, shuffle only steps marked `optional` among their existing positions, and insert one
  or two accepted neutral commands (`true`, `pwd`, `ls`) at seeded positions and shells.
* These mutations vary sequence ordering and scheduling without synthesizing attack techniques.
  Required steps stay in their original relative order; the audit run validates each mutated attack.

The campaign intentionally does not generate arbitrary shell commands or claim coverage-guided
fuzzing. New attack behavior remains a reviewed corpus case.

## Runner (`mod.rs`)

For each case, seed and iteration:

1. Create a `Fixture::new()`, write the files, and start the `Sink`.
2. Build N shells with `Fixture::observed_agent(policy.principal(…), observer)`. The observer
   collects every agent denial, as the existing agent-policy tests do.
3. Run steps with per-step timeouts. A foreground timeout or shell error is inconclusive; an `&`
   server's timeout is expected, but the control sink or attack audit must still prove execution.
4. Close the shells, then drain the sink.
5. Call the oracle and record a `Verdict`.

Settings:

* `MARSH_LEAK_SEED`: default random, always printed.
* `MARSH_LEAK_ITERATIONS`: runs per case; iterations after the first sample variants and apply
  seeded sequence mutations.
* `MARSH_LEAK_CORPUS`: corpus directory override.
* `MARSH_LEAK_FILTER`: case-name substring.

Every attack iteration is replayed in audit mode with the same variant and mutated sequence. The
audit run must deliver its canary to a sink; ordinary attacks must also produce a network
would-block report. Unsupported attack effects require network-denial evidence and this audit
proof; otherwise they are inconclusive. Known gaps waive only the would-block report. A case-name
filter matching no cases is an error.

A case-specific random stream makes sequence generation independent of corpus order. Seed, case name
and iteration reproduce its variants, filler and canary. Enforcement failures print a minimized TOML
reproducer with sampled variants fixed; audit failures print the same replay key and step outcomes.

## Oracle (`oracle.rs`)

* **Attack case:** it leaks if any configured encoding of the canary appears in the external
  sinks' bytes. Encodings: raw, base64 (with and without padding), hex, and any per-case encodings
  named in the TOML. A leak is a failure even when the step that sent it was later refused,
  because sent bytes cannot be recalled.
* **Control case:** the canary must reach the sink. If it does not, the case fails as a false
  denial, with the observer's matching denials attached.
* **Audit-mode run:** replay each attack iteration exactly under `AgentPolicy::audit()`. The
  canary must reach a sink, proving the attack path ran; ordinary attacks must also produce at
  least one "would block" report. Known gaps waive only the would-block expectation.
* **Report:** campaign seed, case, iteration, sampled variants, step outcomes, denials and sink hit.
  Enforcement failures also print minimized TOML with the sampled variants fixed.

## Shrinking (`shrink.rs`)

When a case fails, apply each reduction below and keep it whenever the case still fails, until
nothing more can be removed:

1. drop `optional` steps one at a time;
2. revert variants to their first value;
3. merge shells into shell 0;
4. drop filler steps.

Print the minimal case as TOML so it can be pasted into the corpus as a regression test.

## Known gaps

`leak-corpus/known-gaps.toml` lists case names, reasons and an optional enforcement mode. Registered
leaks report `known gap` instead of failing. `toctou-peer-rewrite` is runnable and remains a gap only
for ptrace; privileged mode must block it. `local-handoff-rewrite` races the same rewrite against a
local relay: ptrace hands taint to the port the address named, so it is a gap there, while
privileged mode taints the relay when it reads. `untraced-local-daemon-allowed` remains the
deliberate `recommended` trade-off. The connected-UDP peer-state race is a documented residual
without a deterministic case, not a test coverage claim.

Closed since: io_uring writes to sockets (every operation outside the effectless set is gated as an
unknown peer); a different principal sending data the agent staged outside the source (files a
tainted principal writes carry its taint to their readers); and proxying through a loopback or
unix-socket relay another principal runs (a tainted contact taints the principals holding the bound
socket). A relay marsh does not trace is refused under `untraced_local(Deny)`
(`untraced-local-daemon-exfil`); the harness runs an untraced capture listener on `127.0.0.1` for
these cases.

## Running it

* Locally:
  `cargo test -p marsh-core --lib leak_corpus -- --ignored --nocapture`
  (with `LD_LIBRARY_PATH` set when `libbtrfsutil` is not installed).
* Select `MARSH_LEAK_ENFORCEMENT=unprivileged|privileged`; kernel mode needs the `privileged`
  Cargo feature, active BPF LSM and the pinned toolchain described in
  `docs/privileged-enforcement.md`. The corpus requires Python and genuine Node.js/libuv.
* `bench/leak-fuzzer/privileged-vm.py` boots an isolated qualifying kernel without rebooting the
  host. The privileged CI workflow builds the same artifacts and runs both modes; serialize VM
  verification on small hosts so emulation load does not turn command deadlines into false denials.
* Pull requests run seed `20261014`, one unchanged iteration, in both modes. Nightly CI runs four
  fixed seeds (`20261014`–`20261017`), two iterations per seed, in both modes. The logs print each
  seed and iteration for exact replay. Direct hook tests additionally cover classification, root
  isolation, stale answers, ring-buffer overflow and task cleanup.

## Campaign boundaries

The daily campaign uses four deterministic seeds and the newest generic kernel installed by the
runner. It is not a supported-kernel matrix or a coverage-guided arbitrary-command fuzzer. The
direct LSM tests exercise kernel event loss, task lifecycle and taint receipts over UDP, TCP and
UNIX sockets; connected-UDP peer reconnect remains a documented residual, not campaign coverage.
