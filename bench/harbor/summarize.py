#!/usr/bin/env python3
"""Compares Harbor job directories run under different marsh configurations.

    summarize.py baseline=jobs/tb-plain sandbox=jobs/tb-sandbox audit=jobs/tb-audit ...

The first LABEL=JOB_DIR is the baseline. For every configuration it reports the solve rate,
median agent time, and policy denials (from each trial's agent/marsh-policy.jsonl). Against the
baseline it reports tasks solved there but not here, and how many of those trials had at least
one denial: the candidates for false denials worth reading by hand. Under `audit` a "denial" is
a would-be denial; the run itself was not affected by it.

Standard library only; reads result.json as Harbor writes it (verifier_result.rewards).
"""

from __future__ import annotations

import json
import re
import statistics
import sys
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path


@dataclass
class Trial:
    task: str
    reward: float | None
    agent_seconds: float | None
    error: str | None
    denials: list[dict] = field(default_factory=list)

    @property
    def solved(self) -> bool:
        return self.reward is not None and self.reward >= 1


def seconds(timing: dict | None) -> float | None:
    if not timing or not timing.get("started_at") or not timing.get("finished_at"):
        return None
    start = timestamp(timing["started_at"])
    end = timestamp(timing["finished_at"])
    return (end - start).total_seconds()


def timestamp(text: str) -> datetime:
    """An ISO 8601 time. Before Python 3.11, fromisoformat rejects a "Z" suffix and fractions of
    other than 3 or 6 digits."""
    text = re.sub(r"Z$", "+00:00", text)
    text = re.sub(r"\.(\d+)", lambda fraction: "." + fraction.group(1)[:6].ljust(6, "0"), text)
    return datetime.fromisoformat(text)


def load_trial(result_path: Path) -> Trial:
    result = json.loads(result_path.read_text())
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    reward = rewards.get("reward", next(iter(rewards.values()), None))
    exception = result.get("exception_info") or {}
    denials = []
    log = result_path.parent / "agent" / "marsh-policy.jsonl"
    if log.exists():
        for line in log.read_text().splitlines():
            if line.strip():
                denials.append(json.loads(line))
    return Trial(
        task=result["task_name"],
        reward=reward,
        agent_seconds=seconds(result.get("agent_execution")),
        error=exception.get("exception_type"),
        denials=denials,
    )


def load_job(job_dir: Path) -> dict[str, list[Trial]]:
    """Trials by task name; a task with several attempts keeps all of them."""
    trials: dict[str, list[Trial]] = {}
    for result_path in sorted(job_dir.glob("*/result.json")):
        trial = load_trial(result_path)
        trials.setdefault(trial.task, []).append(trial)
    if not trials:
        raise SystemExit(f"{job_dir}: no */result.json trial results")
    return trials


def solve_rate(trials: dict[str, list[Trial]]) -> float:
    attempts = [trial for runs in trials.values() for trial in runs]
    return sum(trial.solved for trial in attempts) / len(attempts)


def solved_any(runs: list[Trial]) -> bool:
    return any(trial.solved for trial in runs)


def summarize(jobs: list[tuple[str, dict[str, list[Trial]]]]) -> str:
    baseline_label, baseline = jobs[0]
    lines = [
        "| config | trials | solved | median agent s | trials w/ denial | denials | "
        f"lost vs {baseline_label} | lost w/ denial | gained |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    rules: dict[str, dict[str, int]] = {}
    for label, trials in jobs:
        attempts = [trial for runs in trials.values() for trial in runs]
        times = [t.agent_seconds for t in attempts if t.agent_seconds is not None]
        denied = [t for t in attempts if t.denials]
        common = sorted(set(trials) & set(baseline))
        lost = [task for task in common if solved_any(baseline[task]) and not solved_any(trials[task])]
        gained = [task for task in common if solved_any(trials[task]) and not solved_any(baseline[task])]
        lost_denied = [task for task in lost if any(t.denials for t in trials[task])]
        for trial in attempts:
            for denial in trial.denials:
                key = f"{denial.get('policy', 'agent')}: {denial.get('action')} {denial.get('resource')}"
                rules.setdefault(key, {}).setdefault(label, 0)
                rules[key][label] += 1
        lines.append(
            f"| {label} | {len(attempts)} | {solve_rate(trials):.1%} | "
            f"{statistics.median(times) if times else float('nan'):.0f} | {len(denied)} | "
            f"{sum(len(t.denials) for t in attempts)} | "
            + (f"{len(lost)} | {len(lost_denied)} | {len(gained)} |" if label != baseline_label else "- | - | - |")
        )
        if label != baseline_label and lost_denied:
            lines.append(f"|  ↳ lost with a denial: {', '.join(lost_denied)} |||||||||")
    if rules:
        labels = [label for label, _ in jobs]
        lines += ["", "| denied capability | " + " | ".join(labels) + " |", "|---|" + "---:|" * len(labels)]
        for key in sorted(rules, key=lambda k: -sum(rules[k].values())):
            lines.append(f"| {key} | " + " | ".join(str(rules[key].get(label, 0)) for label in labels) + " |")
    return "\n".join(lines)


def main(argv: list[str]) -> None:
    if len(argv) < 2 or any("=" not in arg for arg in argv[1:]):
        raise SystemExit(__doc__)
    jobs = []
    for arg in argv[1:]:
        label, path = arg.split("=", 1)
        jobs.append((label, load_job(Path(path))))
    print(summarize(jobs))


if __name__ == "__main__":
    main(sys.argv)
