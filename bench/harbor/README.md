# Benchmarking marsh with Harbor

[Harbor](https://github.com/laude-institute/harbor) runs agent benchmarks (Terminal-Bench 2.0,
SWE-bench and SWE-bench Pro, CooperBench, ...) in Docker containers. Its reference terminal
agent, Terminus-2, types into a `tmux` pane. `MarshTerminus2` is Terminus-2 with rmux in place of
tmux, so every line it types runs as a managed marsh command and is judged by an agent policy.
Prompts, models, parsing, timeouts and every task's verifier are unchanged, so results compare
directly with a plain Terminus-2 run on the same tasks.

| Configuration | `MARSH_AGENT_POLICY` | Measures |
| --- | --- | --- |
| plain Terminus-2 | (agent `terminus-2`) | baseline solve rate and time |
| `sandbox` | `sandbox` | cost of the managed route alone: snapshot, trace, WAL |
| `audit` | `audit` | which capabilities the recommended policy *would* deny, with no effect on the run |
| `recommended` | `recommended` | solve rate and time under enforcement |

`audit` vs. plain gives the policy's would-be denials on unaffected trajectories. `recommended`
vs. plain gives the real utility cost. The tasks lost in `recommended` that had a denial are
the false-denial candidates to read by hand.

## Requirements

* Docker, Harbor, and a glibc-based task image. Musl/Alpine images are not supported by the
  bundle.
* Marsh traces with ptrace. Docker's default seccomp profile allows this on Linux 4.8+, and
  `compose/marsh.yaml` adds `SYS_PTRACE` as well.
* A btrfs store, one of:
  * **Bind mount (preferred, unprivileged):** a host directory on btrfs mounted with
    `user_subvol_rm_allowed`, passed as `MARSH_STORE` to `compose/marsh.yaml`. Each trial creates
    `trial.*/<workdir>` subvolumes in it, which you remove after the job, e.g.
    `btrfs subvolume delete "$MARSH_STORE"/trial.*/*`.
  * **Loop image:** `compose/marsh-loop.yaml` runs the container privileged and formats a sparse
    image (`MARSH_LOOP_SIZE`, default 16G) inside it.

## Running

```sh
bench/harbor/build-bundle.sh "$HOME/marsh-bundle"
export MARSH_BUNDLE="$HOME/marsh-bundle" MARSH_STORE=/mnt/btrfs/marsh-store
export PYTHONPATH="$PWD"   # so Harbor can import bench.harbor.marsh_terminus

MODEL=anthropic/claude-sonnet-5-5
harbor run --dataset terminal-bench@2.0 --agent terminus-2 --model "$MODEL" \
    --jobs-dir jobs/plain
for mode in sandbox audit recommended; do
    harbor run --dataset terminal-bench@2.0 --model "$MODEL" \
        --agent bench.harbor.marsh_terminus:MarshTerminus2 --ae MARSH_AGENT_POLICY=$mode \
        --extra-docker-compose bench/harbor/compose/marsh.yaml --jobs-dir jobs/$mode
done
python3 bench/harbor/summarize.py plain=jobs/plain/<job> sandbox=jobs/sandbox/<job> \
    audit=jobs/audit/<job> recommended=jobs/recommended/<job>
```

Each `harbor run` writes one job directory (`<job>`) under its `--jobs-dir`. Repeat each
configuration (`--n-attempts`) before reading small solve-rate differences: agent runs are noisy.

`summarize.py` reports per configuration the solve rate, median agent time, and denials (each
trial's `agent/marsh-policy.jsonl`). Against the first (baseline) configuration it reports tasks
lost, lost with a denial, and gained, followed by a table of denied capabilities.

## What happens in a trial

1. `MarshTerminus2.setup` runs `marsh-setup.sh` as root. It copies the task's working
   directory into a new btrfs subvolume, leaves a symlink at the original path (so the agent and
   the verifier use the same path), writes `/etc/marsh.env`, and installs the `tmux` shim in
   `/usr/local/bin`.
2. Terminus-2 runs `tmux new-session ... 'bash --login'`. The shim drops `'bash --login'`, because
   in rmux it would run the whole session as one marsh command. A commandless rmux pane is
   instead a marsh shell that runs, judges and publishes each line separately. The rmux daemon
   reads `/etc/marsh.env`:
   * `MARSH_AGENT_POLICY` sends every pane through the managed route and selects the policy;
   * `MARSH_POLICY_LOG` (`/logs/agent/marsh-policy.jsonl`, collected by Harbor) receives each
     denial as one JSON line.
3. The verifier runs as usual against the published working directory.

## Limits

* **Effects outside the working directory.** Only the working directory is a marsh source.
  Writes elsewhere (for example `pip install` into `/usr`) are traced and visible to policies,
  but they are not rolled back.
* **Network rules block at the syscall, file rules at publication.** A refused
  `connect`/`send*` never reaches the kernel; the agent's program sees `EPERM`. File-class
  rules refuse a command's working-directory effects when it is published. Effects outside the
  working directory are not rolled back.
* **Unsupported effects are refused, not approximated.** For example FIFOs, special files, and
  some `io_uring` uses. A task that needs them fails under every marsh configuration, and the
  `sandbox` row shows this cost separately from the policy's.
* **Untested.** This adapter has not yet been run end to end against a live Harbor job (the
  environment it was written in had no Docker access). Expect to adjust it on a first run.

## Beyond Terminal-Bench

`MarshTerminus2` runs on any Harbor dataset whose tasks Terminus-2 can attempt. For a larger
utility sample, use the existing `swebench`/`swebenchpro` adapters. `cooperbench` covers
multi-agent work.

Harbor measures how much real work the policy blocks. It does not measure whether the policy
actually prevents a leak; the leak fuzzer does that, separately. It runs the
`crates/marsh-core/tests/leak-corpus/` cases against policy-judged shells in a network namespace
and fails if a secret reaches a stand-in external host — see
[`bench/leak-fuzzer/PLAN.md`](../leak-fuzzer/PLAN.md).

Security efficacy against a live agent still needs attack tasks. The candidates below have no
Harbor adapters yet, and porting them is the next step:
* SABER (arXiv 2606.01317): destructive changes and exfiltration in stateful workspaces.
* RedCode-Exec (arXiv 2411.07781): risky shell and Python execution.
* "Your AI, My Shell" (arXiv 2509.22040): prompt-injection payloads.
* ImpossibleBench (arXiv 2510.20270): test tampering.
