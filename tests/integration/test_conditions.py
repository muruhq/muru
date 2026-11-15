"""Named conditions (design specification section 9.10, issue 123): the
daemon evaluates a condition over its own figures every second, fires it
on an error rate an application is made to produce and resolves it once
the errors stop, with an event for each."""

from __future__ import annotations

import json
import textwrap
import time
from collections.abc import Callable
from pathlib import Path

from support.daemon import start_daemon

PYPROJECT = """
[project]
name = "flaky"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1
"""

APP = """
def application(environ, start_response):
    failing = environ["PATH_INFO"] == "/fail"
    status = "500 Internal Server Error" if failing else "200 OK"
    start_response(status, [("Content-Type", "text/plain")])
    return [b"no" if failing else b"ok"]
"""

CONDITIONS = """
[condition.flaky-errors]
when = "error_rate{app='flaky'}[5s] > 0.5"
for = "2s"
clear_when = "error_rate{app='flaky'}[5s] < 0.1"
clear_for = "2s"
min_requests = 5
"""


def test_a_condition_fires_on_an_error_rate_and_resolves_when_it_stops(tmp_path: Path) -> None:
    project = tmp_path / "flaky"
    project.mkdir()
    (project / "pyproject.toml").write_text(textwrap.dedent(PYPROJECT))
    (project / "app.py").write_text(textwrap.dedent(APP))
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    def shown() -> dict:
        return json.loads(daemon.muructl("condition", "show", "flaky-errors", "--json").stdout)

    def drive(path: str, until: Callable[[], bool], timeout: float = 30) -> None:
        deadline = time.monotonic() + timeout

        while not until():
            assert time.monotonic() < deadline, (shown(), daemon.muructl("events").stdout)

            for _ in range(10):
                daemon.get(path)

            time.sleep(0.2)

    try:
        assert daemon.deploy("flaky", project) == "muru: release flaky@r1 is ready and serving"
        applied = tmp_path / "conditions.toml"
        applied.write_text(textwrap.dedent(CONDITIONS))
        daemon.muructl("apply", "-f", str(applied))

        # With no requests yet the condition has no figure.

        assert shown()["state"] == "unknown"

        # Every request fails: the condition fires, naming the figure.

        drive("/fail", lambda: shown()["state"] == "firing")
        events = daemon.muructl("events").stdout
        assert "condition.firing" in events and "flaky-errors" in events, events
        assert shown()["terms"][0]["known"]

        # The errors stop: it resolves.

        drive("/", lambda: shown()["state"] == "resolved")
        assert "condition.resolved" in daemon.muructl("events").stdout
        listed = daemon.muructl("condition", "list").stdout
        assert "flaky-errors" in listed and "resolved" in listed, listed
    finally:
        daemon.stop()
