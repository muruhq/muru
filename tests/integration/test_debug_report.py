"""muructl debug report (design specification section 18.6): the summary
to attach to a bug report, against a daemon that runs and one that has
stopped, a development run and muructl serve, with what identifies the
system replaced by placeholders in every format."""

from __future__ import annotations

import json
import os
import platform
import socket
import subprocess
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from test_run_admin import muructl as run_muructl
from test_run_admin import start as start_run
from test_serve import start as start_serve


def report(
    daemon: Daemon, *arguments: str, state: Path | None = None
) -> subprocess.CompletedProcess[str]:
    """Run muructl debug report with the locations of a test's daemon."""

    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [
            str(muructl),
            "debug",
            "report",
            *arguments,
            "--run-dir",
            str(daemon.run_dir),
            "--state-dir",
            str(state or daemon.base / "state"),
            "--log-dir",
            str(daemon.base / "logs"),
            "--config",
            str(daemon.base / "muru.toml"),
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    return completed


def identifying(daemon: Daemon) -> list[str]:
    """What names this test's system: none of it may be in a report."""

    return [
        "shop",
        str(daemon.base),
        str(daemon.run_dir),
        str(Path.home()),
        platform.node().split(".")[0],
        os.environ.get("USER", "nobody-at-all"),
    ]


def setting(found: dict[str, Any], key: str) -> dict[str, str]:
    return next(setting for setting in found["settings"] if setting["key"] == key)


def test_report_of_a_daemon_running_and_stopped(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra="[limits]\nmax_body_size = '2MB'\n",
    )

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "2") == (
            "muru: release shop@r1 is ready and serving"
        )

        found = json.loads(report(daemon, "--json").stdout)
        assert found["runs"] == "a user daemon"
        assert found["running"] is True
        assert found["platform"]["arch"]
        assert found["not_collected"] == []

        (release,) = found["releases"]
        assert release["release"] == "app1@r1"
        assert release["state"] == "ready" and release["serving"] is True
        assert release["interface"] == "wsgi"
        assert release["python"] == platform.python_version()
        assert release["build"] in ("regular", "free-threaded")
        assert [worker["state"] for worker in release["workers"]] == ["ready", "ready"]

        assert setting(found, "processes") == {
            "scope": "app1@r1",
            "key": "processes",
            "value": "2",
            "source": "deploy flags",
        }
        assert setting(found, "entrypoint")["value"] == "<module1>:application"
        assert {"key": "limits.max_body_size", "value": "2MB"} in found["daemon_settings"]
        assert {"type": "release.ready", "level": "info", "count": 1} in found["events"]["counts"]
        assert found["replaced"]["applications"] == 1

        # A command that names only the socket finds the daemon's state
        # from what the daemon says, and reports the same.

        by_socket = json.loads(daemon.muructl("debug", "report", "--json").stdout)
        assert by_socket["releases"] == found["releases"]
        assert by_socket["settings"] == found["settings"]
        assert by_socket["daemon_settings"] == found["daemon_settings"]

        # Nothing that names the system is in any format, and the
        # Markdown is headed for an issue.

        text = report(daemon).stdout
        markdown = report(daemon, "--format", "markdown").stdout
        assert "Runs as a user daemon, running" in text
        assert "app1@r1" in text and "processes = 2" in text
        assert markdown.startswith("### Muru diagnostic report\n")
        assert "| `app1@r1` | `ready` | yes | `wsgi` |" in markdown

        for output in (text, markdown, json.dumps(found)):
            for name in identifying(daemon):
                assert name not in output, f"{name} in:\n{output}"
    finally:
        daemon.stop()

    # With the daemon stopped the report is read from its files, which
    # are left exactly as they were.

    state = tmp_path / "state"
    before = {path.name: path.stat().st_mtime_ns for path in state.iterdir()}
    stored = (state / "muru.sqlite").read_bytes()

    found = json.loads(report(daemon, "--json").stdout)
    assert found["running"] is False
    (release,) = found["releases"]
    assert release["release"] == "app1@r1"
    assert release["state"] == "stopped" and release["workers"] == []
    assert setting(found, "processes")["value"] == "2"
    assert {"key": "limits.max_body_size", "value": "2MB"} in found["daemon_settings"]
    assert any(count["type"] == "daemon.stopped" for count in found["events"]["counts"])

    assert {path.name: path.stat().st_mtime_ns for path in state.iterdir()} == before
    assert (state / "muru.sqlite").read_bytes() == stored

    # A socket that listens and gives no answer, as a daemon that hangs
    # would leave it, is said to be so, and the files are read all the same.

    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(daemon.run_dir / "admin.sock"))
        listener.listen(64)
        found = json.loads(report(daemon, "--json").stdout)

    (daemon.run_dir / "admin.sock").unlink()
    assert found["running"] is False
    assert found["releases"][0]["release"] == "app1@r1"
    (note,) = found["not_collected"]
    assert note["part"] == "live state"
    assert note["why"] == "what listens at <run-dir>/admin.sock gave no answer within 5 seconds"

    # Where there is nothing to read, the report says so and succeeds.

    empty = tmp_path / "nothing"
    empty.mkdir()
    found = json.loads(report(daemon, "--json", state=empty).stdout)
    assert found["releases"] is None and found["events"] is None
    assert [note["part"] for note in found["not_collected"]] == ["releases", "settings", "events"]
    assert "there is no state store at" in found["not_collected"][0]["why"]
    text = report(daemon, state=empty).stdout
    assert "NOT COLLECTED\n  releases: no daemon is running, and there is no state store" in text


def test_report_of_a_development_run_and_of_a_serve(tmp_path: Path) -> None:
    runtime = support.short_dir("muru-rt-")
    server = start_run(runtime)

    try:
        found = json.loads(
            run_muructl(runtime, "debug", "report", "--json", cwd=support.FIXTURES).stdout
        )
        assert found["runs"] == "muructl run" and found["running"] is True
        assert found["daemon_settings"] is None
        assert found["not_collected"] == []

        (release,) = found["releases"]
        assert release["release"] == "app1@r1" and release["state"] == "ready"
        assert release["interface"] == "wsgi"
        assert release["python"] == platform.python_version()
        assert len(release["workers"]) == 2

        assert setting(found, "processes")["source"] == "command line"
        assert setting(found, "entrypoint")["value"] == "<module1>:application"
        assert setting(found, "run.port")["value"] == str(server.port)

        text = run_muructl(runtime, "debug", "report", cwd=support.FIXTURES).stdout
        assert "Runs as muructl run, running" in text
        assert "plain" not in text and str(support.FIXTURES) not in text
    finally:
        server.stop()

    serve = start_serve(tmp_path)

    try:
        completed = subprocess.run(
            [str(support.muructl_path()), "debug", "report", "--json"],
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
            env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
        )
        found = json.loads(completed.stdout)
        assert found["runs"] == "muructl serve" and found["running"] is True
        (release,) = found["releases"]
        assert release["release"] == "app1@r1" and release["interface"] == "wsgi"
        assert setting(found, "processes")["value"] == "1"
        assert setting(found, "serve.listen")["value"] == f"127.0.0.1:{serve.port}"
        assert str(serve.run_dir) not in completed.stdout
    finally:
        serve.stop()
