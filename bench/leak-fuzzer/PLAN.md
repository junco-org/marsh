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

1. If `MARSH_LEAK_NS` is not set, re-run the test binary as
   `unshare --user --map-root-user --net -- <current_exe> leak_corpus --exact --ignored
   --nocapture --test-threads 1` with `MARSH_LEAK_NS=1`, passing `LD_LIBRARY_PATH` through, and
   assert that the child succeeded.
   * If `unshare` fails (no user namespaces), print a skip notice and return.
   * Re-execution is required: `unshare(CLONE_NEWUSER)` refuses a multithreaded process, and the
     test harness has threads.
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
* Mutations that need no attack knowledge:
  * sample variants;
  * reorder steps marked `optional`;
  * insert neutral filler steps from a fixed list (`true`, `ls`, writing `src/x`);
  * split a multi-command step across commands or shells.

  These exercise ordering, the policy's de-duplication of repeated observations, and principal
  sharing across shells.

## Runner (`mod.rs`)

For each case and each sampled variant:

1. Create a `Fixture::new()`, write the files, and start the `Sink`.
2. Build N shells with `Fixture::observed_agent(policy.principal(…), observer)`. The observer
   collects every agent denial, as the existing agent-policy tests do.
3. Run the steps in order with per-step timeouts (`tokio::time::timeout`). Record each step's
   outcome (accepted, refused, error, timeout); fail on it only when `expect` says so.
4. Close the shells, then drain the sink.
5. Call the oracle and record a `Verdict`.

Settings:

* `MARSH_LEAK_SEED`: default random, always printed.
* `MARSH_LEAK_ITERATIONS`: variants per case.
* `MARSH_LEAK_CORPUS`: corpus directory override.
* `MARSH_LEAK_FILTER`: case-name substring.

At the end it prints a per-case summary table. The test fails if any attack case leaked or any
control case was denied.

## Oracle (`oracle.rs`)

* **Attack case:** it leaks if any configured encoding of the canary appears in the external
  sinks' bytes. Encodings: raw, base64 (with and without padding), hex, and any per-case encodings
  named in the TOML. A leak is a failure even when the step that sent it was later refused,
  because sent bytes cannot be recalled.
* **Control case:** the canary must reach the sink. If it does not, the case fails as a false
  denial, with the observer's matching denials attached.
* **Audit-mode run:** run the same attack corpus with `AgentPolicy::audit()`. Every leak must then
  come with at least one "would block" report, which checks that audit sees what enforcement
  blocks.
* **Report:** the case name, seed, variant, the expanded commands, each step's outcome, the
  observer's denial log, and the sink hit (listener and offset).

## Shrinking (`shrink.rs`)

When a case fails, apply each reduction below and keep it whenever the case still fails, until
nothing more can be removed:

1. drop `optional` steps one at a time;
2. revert variants to their first value;
3. merge shells into shell 0;
4. drop filler steps.

Print the minimal case as TOML so it can be pasted into the corpus as a regression test.

## Known gaps

`leak-corpus/known-gaps.toml` lists case names with a reason. They run but report `known gap`
instead of failing, so only new leaks fail the build. Two remain: the TOCTOU peer rewrite, which
has no deterministic case, and `untraced-local-daemon-allowed`, the trade-off `recommended` makes by
letting a principal holding a secret reach a local daemon marsh does not trace.

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
* CI: a separate job on a Linux runner with user namespaces enabled. Run nightly with a random seed,
  and per pull request with a fixed seed and a small iteration count.

## Order of work

1. Namespace bootstrap and the sink, plus a smoke test: one control case delivers and one trivial
   attack case is blocked.
2. The schema, loader and placeholders, plus the oracle for raw, base64 and hex.
3. The runner, report, seed and filters.
4. Neutral mutations, then shrinking, then the known-gaps file.
5. Corpus import from hand-written cases or the SABER and RedCode exfiltration tasks.
6. Document the suite in the README's agent-policy section, and point to it from
   `bench/harbor/README.md` as the policy's leak test, separate from the benchmark's measure of
   how much real work the policy blocks.
