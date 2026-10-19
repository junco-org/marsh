"""Exercise the CI policy against real, isolated systemd services, never the caller's cgroup."""

import os
from pathlib import Path
import select
import signal
import subprocess
import sys


def own_cgroup():
    path = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
    return Path("/sys/fs/cgroup") / path.lstrip("/")


def probe(mode, unit):
    root = own_cgroup()
    assert root.name == unit, root  # ExecStartPre must configure the service, not .control.
    assert (root / "memory.max").read_text().strip() == str(128 * 1024 * 1024)
    assert (root / "memory.oom.group").read_text().strip() == "1"
    host = root / "host"
    host.mkdir()
    (host / "cgroup.procs").write_text("0")
    (root / "cgroup.subtree_control").write_text("+cpu +memory +pids")
    leaf = root / "pipeline"
    leaf.mkdir()
    (leaf / "memory.max").write_text(str(32 * 1024 * 1024) if mode == "leaf" else "max")
    (leaf / "memory.swap.max").write_text("0")
    (leaf / "memory.oom.group").write_text("1")
    sibling = os.fork()
    if sibling == 0:
        os.execv(sys.executable, [sys.executable, "-c", "import time; time.sleep(60)"])
    print(f"sibling {sibling}", flush=True)
    print(f"ready {mode}", flush=True)
    child = os.fork()
    if child == 0:
        (leaf / "cgroup.procs").write_text("0")
        assert own_cgroup() == leaf
        megabytes = 128 if mode == "leaf" else 256
        os.execv(sys.executable, [sys.executable, "-c", f"bytearray({megabytes} * 1024 * 1024)"])
    _, status = os.waitpid(child, 0)
    assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL, status
    assert own_cgroup() == host
    assert "oom_kill 0\n" in (root / "memory.events.local").read_text()
    os.kill(sibling, signal.SIGTERM)
    os.waitpid(sibling, 0)
    leaf.rmdir()
    print(f"survived {mode}", flush=True)


def check_policy(mode):
    unit = f"marsh-cgroup-policy-{os.getpid()}-{mode}.service"
    command = [
        "sudo", "-n", "systemd-run", f"--uid={os.getuid()}", f"--gid={os.getgid()}",
        "--wait", "--pipe", "--service-type=exec", "--expand-environment=no", f"--unit={unit}",
        "-p", "Delegate=yes", "-p", "MemoryMax=128M", "-p", "MemorySwapMax=0",
        "-p", "MemoryAccounting=yes", "-p", "OOMPolicy=continue",
        "-p", os.environ["CGROUP_OOM_PRESTART"], "-p", "KillMode=control-group",
        "-p", "RuntimeMaxSec=10s", "-p", "TimeoutStopSec=2s",
        sys.executable, "-u", str(Path(__file__).resolve()), "--probe", mode, unit,
    ]
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=20)
        print(result.stdout, result.stderr, sep="", flush=True)
        assert f"ready {mode}\n" in result.stdout, result
        if mode == "leaf":
            assert result.returncode == 0 and "survived leaf\n" in result.stdout, result
        else:
            assert result.returncode != 0 and "survived outer\n" not in result.stdout, result
            state = subprocess.check_output([
                "systemctl", "show", unit, "-p", "ExecMainCode", "-p", "ExecMainStatus",
            ], text=True)
            assert dict(line.split("=", 1) for line in state.splitlines()) == {
                "ExecMainCode": "2", "ExecMainStatus": str(signal.SIGKILL),
            }, state
            sibling = int(next(line.split()[1] for line in result.stdout.splitlines() if line.startswith("sibling ")))
            try:
                fd = os.pidfd_open(sibling)
            except ProcessLookupError:
                pass
            else:
                try:
                    assert select.select([fd], [], [], 0)[0], "service sibling survived group OOM"
                finally:
                    os.close(fd)
    finally:
        subprocess.run(["sudo", "-n", "systemctl", "stop", unit], capture_output=True)
        subprocess.run(["sudo", "-n", "systemctl", "reset-failed", unit], capture_output=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--probe":
        probe(*sys.argv[2:])
    else:
        check_policy("leaf")
        check_policy("outer")
