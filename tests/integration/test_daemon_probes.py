"""The daemon's probes (section 13.9 of the specification, issue 622):
`muructl probe` over the admin socket, and `/livez`, `/readyz` and
`/startupz` on the metrics address."""

from __future__ import annotations

import http.client
import json
import os
import subprocess
import time
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


def test_the_daemon_answers_the_probes_over_its_socket_and_its_metrics_address(
    tmp_path: Path,
) -> None:
    port = support.free_port()
    daemon = start_daemon(tmp_path, arguments=["--metrics", f"127.0.0.1:{port}"])

    def get(path: str) -> tuple[int, str]:
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return response.status, response.read().decode()
        finally:
            connection.close()

    try:
        # With nothing deployed the daemon is live, started and ready: an
        # application without a serving release holds nothing back.

        for probe in ("live", "ready", "startup"):
            answered = daemon.muructl("probe", probe, check=False)
            assert answered.returncode == 0, answered.stdout + answered.stderr
            assert answered.stdout.strip() == "ok"

        assert get("/readyz") == (200, "ok\n")

        daemon.deploy("shop", SHOP, "--processes", "1")
        assert daemon.muructl("probe", "ready", "--app", "shop", check=False).returncode == 0

        # More workers asked for than the group has: not ready, saying why
        # on standard error, and the same over HTTP for the whole node.

        short = daemon.muructl("probe", "ready", "--min-ready", "2", check=False)
        assert short.returncode == 1
        assert "group default has 1 of 2 workers ready" in short.stderr, short.stderr

        missing = daemon.muructl("probe", "startup", "--app", "nope", check=False)
        assert missing.returncode == 1
        assert "there is no application nope" in missing.stderr, missing.stderr

        assert get("/livez") == (200, "ok\n")
        assert get("/startupz") == (200, "ok\n")
    finally:
        daemon.stop()


@pytest.mark.requires_root
@pytest.mark.requires_linux
def test_the_daemons_guard_scales_a_group_in_when_its_cgroup_runs_short(tmp_path: Path) -> None:
    """Sections 7.7 and 13.11: a daemon in a cgroup with a memory.max, as
    in a container or under systemd with MemoryMax=, guards it across its
    releases: short of memory, a group with a policy above min_workers is
    scaled in by its largest worker. Needs a cgroup v2 root can write, as
    in the Linux virtual machine; not in CI."""

    root = Path("/sys/fs/cgroup")
    assert (root / "cgroup.controllers").is_file(), "cgroup v2 is needed"
    cgroup = root / f"muru-test-{os.getpid()}-{tmp_path.name}"
    subprocess.run(["sudo", "-n", "mkdir", str(cgroup)], check=True)
    project = tmp_path / "shop"
    project.mkdir()
    (project / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    (project / "pyproject.toml").write_text(
        """[project]
name = "shop"
version = "0.0.0"

[tool.muru]
entrypoint = "app:application"
processes = 2
threads = 1

[tool.muru.memory]
check_interval = "1s"

[tool.muru.scaling]
mode = "spare"
min_workers = 1
max_workers = 2
"""
    )

    def write(path: Path, text: str) -> None:
        subprocess.run(
            ["sudo", "-n", "tee", str(path)],
            input=text,
            text=True,
            check=True,
            stdout=subprocess.DEVNULL,
        )

    def into_cgroup(command: list[str]) -> list[str]:
        joined = " ".join(f"'{part}'" for part in command)
        return [
            "sh",
            "-c",
            f"echo $$ | sudo -n tee {cgroup}/cgroup.procs >/dev/null && exec {joined}",
        ]

    daemon: Daemon | None = None

    def limits() -> dict[str, Any]:
        assert daemon is not None
        status = json.loads(daemon.muructl("status", "--json").stdout)
        return dict(status["limits"] or {})

    try:
        write(cgroup / "memory.max", str(1 << 30))
        (tmp_path / "daemon").mkdir()
        daemon = start_daemon(tmp_path / "daemon", wrap=into_cgroup)
        daemon.deploy("shop", project)
        deadline = time.monotonic() + 15

        while limits().get("memory_max") != 1 << 30:
            assert time.monotonic() < deadline, limits()
            time.sleep(0.2)

        # Issue 384: the budget's memory is the cgroup's, less the headroom.

        status = daemon.muructl("status").stdout
        assert "(memory from memory.max 1.0GB less 128MB of headroom" in status, status

        # One worker takes 120MB; then less than the headroom is left.

        assert daemon.get("/grow?mb=120").status == 200

        while limits()["workers_memory"] < 120 << 20:
            assert time.monotonic() < deadline + 15, limits()
            time.sleep(0.2)

        write(cgroup / "memory.max", str(limits()["memory_used"] + (60 << 20)))
        deadline = time.monotonic() + 30

        while "scale in from 2 to 1 workers: for memory" not in daemon.muructl("events").stdout:
            assert time.monotonic() < deadline, daemon.muructl("events").stdout + daemon.log()
            time.sleep(0.5)

        assert daemon.muructl("probe", "ready", check=False).returncode == 0
    finally:
        if daemon is not None:
            daemon.stop()

        subprocess.run(["sudo", "-n", "rmdir", str(cgroup)], check=False)
