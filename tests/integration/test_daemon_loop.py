"""The daemon's loop goes on while a release starts (issue 693, design
specification section 9.1): status and another application's deploy
answer while a release runs its before_fork gate, a drain of a release
that starts cancels its start, and a scale of it is refused until it is
ready."""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from support.daemon import Daemon, start_daemon

APP = """
def application(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""

GATED = """
[project]
name = "ops"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1

[tool.muru.commands.migrate]
run = [
    "python", "-c",
    "import os, time\\nwhile not os.path.exists('go'): time.sleep(0.1)",
]
timeout = "5m"
before_fork = true
"""

PLAIN = """
[project]
name = "{name}"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1
"""


def project(tmp_path: Path, name: str, pyproject: str, app: str = APP) -> Path:
    directory = tmp_path / name
    directory.mkdir()
    (directory / "pyproject.toml").write_text(textwrap.dedent(pyproject))
    (directory / "app.py").write_text(app)
    return directory


def shown(daemon: Daemon, release: str) -> dict:
    """The release as `release show` gives it, or nothing before it is
    recorded."""

    completed = daemon.muructl("release", "show", release, "--json", check=False)
    return json.loads(completed.stdout) if completed.returncode == 0 else {}


def wait_until(daemon: Daemon, release: str, holds, timeout: float = 60) -> dict:
    deadline = time.monotonic() + timeout

    while not holds(state := shown(daemon, release)):
        assert time.monotonic() < deadline, state
        time.sleep(0.2)

    return state


def answers_at_once(daemon: Daemon, name: str, source: Path) -> None:
    """Status, and a deploy of another application, answer at once. The
    deploy holds its release, since with no hosts declared one application
    serves."""

    began = time.monotonic()
    daemon.muructl("status")
    assert time.monotonic() - began < 5
    assert daemon.deploy(name, source, "--hold").startswith(f"muru: release {name}@r1 is ready")


def test_the_daemon_answers_while_a_release_starts(tmp_path: Path) -> None:
    ops = project(tmp_path, "ops", GATED)
    api = project(tmp_path, "api", PLAIN.format(name="api"))
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    def deploy_ops() -> subprocess.CompletedProcess[str]:
        return daemon.muructl("deploy", "ops", str(ops), "--python", sys.executable, check=False)

    def gating(state: dict) -> bool:
        return "before_fork gates" in state.get("message", "")

    try:
        with ThreadPoolExecutor() as pool:
            # While ops@r1 runs its before_fork gate, the daemon answers, and
            # refuses to scale the release that starts.

            first = pool.submit(deploy_ops)
            wait_until(daemon, "ops@r1", gating)
            answers_at_once(daemon, "api", api)
            assert not first.done()

            refused = daemon.muructl("scale", "ops@r1", "--processes", "2", check=False)
            assert refused.returncode != 0
            assert "ops@r1 is starting; try again when it is ready" in refused.stderr, refused

            (ops / "go").touch()
            assert first.result().stdout.strip() == "muru: release ops@r1 is ready and serving"

            # A drain of a release that starts cancels its start, stopping
            # the gate's run and the host.

            (ops / "go").unlink()
            second = pool.submit(deploy_ops)
            wait_until(daemon, "ops@r2", gating)
            daemon.muructl("drain", "ops@r2")
            failed = second.result()
            assert failed.returncode != 0
            assert "its start was cancelled: the release was drained" in failed.stderr, failed

        state = wait_until(daemon, "ops@r2", lambda state: state.get("state") == "stopped")
        assert "its start was cancelled" in state["message"], state
        runs = json.loads(daemon.muructl("command", "runs", "ops", "migrate", "--json").stdout)
        assert sorted(run["ok"] for run in runs["runs"]) == [False, True], runs
    finally:
        daemon.stop()
