"""The instances on one machine (design specification sections 13.8 and
13.10): `muructl instance list` reads the instance registry, says which
instances run, are detached or are stale, and finds the Muru processes
no live instance accounts for."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import start_daemon


def muructl(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run muructl with the test's environment."""

    path = support.muructl_path()
    assert path is not None
    completed = subprocess.run(
        [str(path), *arguments], capture_output=True, text=True, check=False, timeout=60
    )

    if check and completed.returncode not in (0, 1):
        raise RuntimeError(f"muructl {' '.join(arguments)} failed:\n{completed.stderr}")

    return completed


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def listed(label: str, *flags: str) -> dict[str, Any]:
    """`instance list --json` narrowed to one label."""

    answer: dict[str, Any] = json.loads(
        muructl("instance", "list", "--json", "--label", label, *flags).stdout
    )
    return answer


def test_the_list_shows_each_instance_its_state_and_what_is_left_behind(
    tmp_path: Path,
) -> None:
    label = f"test-{uuid.uuid4().hex[:8]}"
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon", env={"MURU_INSTANCE_LABEL": label})
    run = support.start(
        "plain_wsgi:application", workers=1, threads=1, env={"MURU_INSTANCE_LABEL": label}
    )
    stray = None

    try:
        # A daemon and a run, both running, each with what it listens on.

        rows = {row["kind"]: row for row in listed(label)["instances"]}
        assert set(rows) == {"daemon", "run"}, rows
        assert rows["daemon"]["state"] == "running" and rows["daemon"]["location"] == "local"
        assert rows["daemon"]["pid"] == daemon.process.pid
        assert rows["run"]["tag"] == f"run:{run.port}"
        assert rows["run"]["listeners"][0]["bound"] == f"127.0.0.1:{run.port}"

        table = muructl("instance", "list", "--label", label).stdout
        assert table.splitlines()[0].split() == [
            "NAME",
            "KIND",
            "STATE",
            "PID",
            "UPTIME",
            "LISTEN",
            "DASHBOARD",
            "WHERE",
            "LABEL",
        ], table
        assert f"run:{run.port}" in table and label in table, table

        # A process carrying the ID of an instance that is gone is an
        # orphan, and the listing exits 1.

        stray = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            env={
                **os.environ,
                "MURU_INSTANCE_ID": uuid.uuid4().hex,
                "MURU_INSTANCE": "gone",
                "MURU_INSTANCE_LABEL": label,
            },
        )
        wait_for(
            lambda: any(
                orphan["pid"] == stray.pid
                for orphan in listed(label, "--orphans").get("orphans", [])
            )
        )
        orphans = listed(label, "--orphans")["orphans"]
        assert [(o["pid"], o["instance"], o["why"]) for o in orphans] == [
            (stray.pid, "gone", "its instance is gone")
        ], orphans
        assert muructl("instance", "list", "--orphans", "--label", label).returncode == 1
        stray.kill()
        stray.wait()

        # The daemon detaches: its lock is free, its edge and hosts carry
        # its ID, and none of them is an orphan.

        daemon.stop(shutdown=False)
        rows = {row["kind"]: row for row in listed(label)["instances"]}
        assert rows["daemon"]["state"] == "detached", rows
        assert listed(label, "--orphans")["orphans"] == []
    finally:
        if stray is not None and stray.poll() is None:
            stray.kill()

        run.stop()

        if daemon.process.poll() is not None:
            daemon = start_daemon(
                tmp_path / "daemon",
                run_dir=daemon.run_dir,
                port=daemon.port,
                env={"MURU_INSTANCE_LABEL": label},
            )

        daemon.stop()

    # A daemon shut down and a run stopped leave no entries.

    assert listed(label)["instances"] == []


def test_a_port_another_instance_holds_names_that_instance(tmp_path: Path) -> None:
    # Issue 808: a bind refused because an instance of Muru's holds the
    # port says which instance, and the ports list shows what each uses.

    label = f"test-{uuid.uuid4().hex[:8]}"
    run = support.start(
        "plain_wsgi:application", workers=1, threads=1, env={"MURU_INSTANCE_LABEL": label}
    )
    muructl_path = support.muructl_path()
    assert muructl_path is not None

    try:
        second = subprocess.run(
            [
                str(muructl_path),
                "run",
                "plain_wsgi:application",
                "--port",
                str(run.port),
                "--dashboard",
                "off",
            ],
            capture_output=True,
            text=True,
            timeout=120,
            cwd=support.FIXTURES,
            check=False,
        )
        assert second.returncode != 0
        assert (
            f"port {run.port} on 127.0.0.1 is in use by the development run of plain-wsgi in "
            in second.stderr
        ), second.stderr
        assert "choose another with --port, or stop it" in second.stderr

        # A daemon is refused before it starts anything.

        base = tmp_path / "daemon"
        refused = subprocess.run(
            [
                str(muructl_path),
                "daemon",
                "start",
                "--listen",
                f"127.0.0.1:{run.port}",
                "--config",
                str(base / "muru.toml"),
                "--state-dir",
                str(base / "state"),
                "--run-dir",
                str(support.short_dir("muru-run-")),
                "--log-dir",
                str(base / "logs"),
                "--cache-dir",
                str(base / "cache"),
            ],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        assert refused.returncode != 0
        assert "in use by the development run of plain-wsgi" in refused.stderr, refused.stderr
        assert "choose another listener, or stop it" in refused.stderr
        assert not (base / "state").exists()

        ports = json.loads(
            muructl("instance", "list", "--ports", "--json", "--label", label).stdout
        )["ports"]
        assert [(p["port"], p["use"], p["state"]) for p in ports][:1] == [
            (run.port, "http", "bound")
        ], ports
    finally:
        run.stop()
