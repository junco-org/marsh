"""Terminus-2 driving rmux instead of tmux, so every line it types is a sandboxed marsh command.

Terminus-2 (Harbor's reference terminal agent) types into a tmux pane. ``marsh-setup.sh`` moves
the task's working directory onto a btrfs subvolume and installs a ``tmux`` shim that runs
rmux, whose commandless panes are marsh shells. Everything else -- prompts, parsing, timeouts,
the verifier -- is unmodified Terminus-2 and Harbor.

Select it with ``harbor run --agent bench.harbor.marsh_terminus:MarshTerminus2`` and choose the
policy with ``--ae MARSH_AGENT_POLICY=sandbox|audit|recommended`` (default ``recommended``).
"""

import shlex

from harbor.agents.terminus_2 import Terminus2
from harbor.environments.base import BaseEnvironment

SETUP = "/opt/marsh/libexec/marsh-setup.sh"
MODES = ("sandbox", "audit", "recommended")


class MarshTerminus2(Terminus2):
    """Terminus-2 whose terminal multiplexer is rmux."""

    @staticmethod
    def name() -> str:
        return "marsh-terminus-2"

    async def setup(self, environment: BaseEnvironment) -> None:
        mode = self._get_env("MARSH_AGENT_POLICY") or "recommended"
        if mode not in MODES:
            raise ValueError(f"MARSH_AGENT_POLICY must be one of {MODES}, not {mode!r}")
        workdir = await environment.exec(command="pwd", user=environment.default_user)
        if workdir.return_code != 0 or not (workdir.stdout or "").strip():
            raise RuntimeError(f"cannot read the task working directory: {workdir.stderr}")
        user = environment.default_user
        user = "" if user is None else str(user)
        command = " ".join(
            shlex.quote(part)
            for part in ("sh", SETUP, workdir.stdout.strip(), mode, user)
        )
        result = await environment.exec(command=command, user="root")
        if result.return_code != 0:
            raise RuntimeError(
                "marsh setup failed (see bench/harbor/README.md):\n"
                f"{result.stdout or ''}{result.stderr or ''}"
            )
        await super().setup(environment)
