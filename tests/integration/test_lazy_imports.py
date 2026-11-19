"""Deferring imports with unwrapt (section 6.17), end to end through muructl run.

unwrapt comes from the development environment, which the host runs in.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import pytest
from support import server as support

pytest.importorskip("unwrapt")

APP = """
import sys

import heavy


def page(environ, start_response):
    # Whether the worker had loaded heavy before this request used it.

    before = "heavy" in sys.modules
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [f"{before} {heavy.VALUE}".encode()]
"""


def test_an_import_unwrapt_defers_loads_in_the_worker_on_first_use(tmp_path: Path) -> None:
    (tmp_path / "lazy_app.py").write_text(APP)
    (tmp_path / "heavy.py").write_text("VALUE = 42\n")
    (tmp_path / "pyproject.toml").write_text(
        '[tool.muru]\nentrypoint = "lazy_app:page"\nlazy_imports = true\n\n'
        '[tool.unwrapt.lazy_modules]\n"lazy_app" = ["heavy"]\n'
    )
    server = support.start(None, chdir=tmp_path, workers=1)

    try:
        # The host preloaded the application without heavy, and said so
        # as it forked; the worker loads it when the request uses it.

        assert server.get("/").body == b"False 42"
        assert server.get("/").body == b"True 42"
        assert "lazy_imports: 1 import deferred, none of them used before the fork" in server.log()
    finally:
        server.stop()


VIEWS = """
import report_slow  # noqa: F401
from report_pkg import part  # noqa: F401


def page(environ, start_response):
    # A module loaded on first use, after the fork.

    import csv  # noqa: F401

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""


def test_the_import_report_reaches_debug_imports_and_muructl_check(tmp_path: Path) -> None:
    runtime = support.short_dir("muru-rt-")
    (tmp_path / "report_app.py").write_text(VIEWS)
    (tmp_path / "report_slow.py").write_text("import time\n\ntime.sleep(0.005)\n")
    (tmp_path / "report_pkg").mkdir()
    (tmp_path / "report_pkg" / "__init__.py").write_text("")
    (tmp_path / "report_pkg" / "part.py").write_text("import time\n\ntime.sleep(0.005)\n")
    (tmp_path / "pyproject.toml").write_text(
        '[tool.muru]\nentrypoint = "report_app:page"\nimport_report = true\n'
    )
    server = support.start(None, chdir=tmp_path, workers=1, env={"MURU_RUN_DIR": str(runtime)})

    # Every row, since report_pkg's from-import saves nothing unwrapt can
    # count and is below the time a rewrite is advised at (issue 784).

    muructl = [
        str(support.muructl_path()),
        "debug",
        "imports",
        "--all",
        "--json",
        "--socket",
        f"{runtime}/runs/{server.port}/admin.sock",
    ]

    try:
        assert server.wait_for_log("admin socket"), server.log()
        assert server.get("/").body == b"ok"

        # The host sends its report once the run connects to it, so it is
        # waited for; the worker counts the module the request loaded.

        deadline = time.monotonic() + 30.0
        found: dict[str, Any] = {}

        while time.monotonic() < deadline:
            answered = subprocess.run(muructl, capture_output=True, text=True, timeout=60)
            found = json.loads(answered.stdout)

            if found.get("report"):
                break

            time.sleep(0.1)

        report = found["report"]
        assert report["labels"]["role"] == "host", found
        assert report["labels"]["outcome"] == "fork"
        targets = {row["imported"]: row["target"] for row in report["candidates"]}
        assert targets["report_slow"] == "entrypoint", report["candidates"]
        modules = {row["module"]: row for row in found["workers"]["modules"]}
        assert modules["csv"]["loaded_by"] == 1, found["workers"]

        # Issue 788: a package loaded by a from-import is in the report,
        # and --module finds the statement that loaded it.

        assert targets["report_pkg"] == "entrypoint", report["candidates"]
        chain = subprocess.run(
            [*muructl[:-3], "--module", "report_pkg", *muructl[-2:]],
            capture_output=True,
            text=True,
            timeout=60,
        ).stdout
        assert "report_app -> report_pkg" in chain and "report_app.py:3" in chain, chain

        # Issue 753: the load is an event with the report's figures, its
        # time a metric, and the worker's loads counted once it reports.

        socket = ["--socket", f"{runtime}/runs/{server.port}/admin.sock"]
        events = subprocess.run(
            [str(support.muructl_path()), "events", *socket],
            capture_output=True,
            text=True,
            timeout=60,
        ).stdout
        assert "release.loaded" in events and "importing took" in events, events

        deadline = time.monotonic() + 30.0
        metrics = ""

        while time.monotonic() < deadline:
            metrics = subprocess.run(
                [str(support.muructl_path()), "metrics", *socket],
                capture_output=True,
                text=True,
                timeout=60,
            ).stdout
            counted = [
                line
                for line in metrics.splitlines()
                if line.startswith("muru_worker_modules_loaded_total{")
            ]

            if counted and not counted[0].endswith(" 0"):
                break

            time.sleep(0.1)

        assert "muru_release_load_seconds{" in metrics, metrics
        assert counted and not counted[0].endswith(" 0"), metrics
    finally:
        server.stop()

    # muructl check --imports prints the report of one load.

    checked = subprocess.run(
        [
            str(support.muructl_path()),
            "check",
            str(tmp_path),
            "--imports",
            "--python",
            sys.executable,
        ],
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, "MURU_OFFLINE": "1"},
    )

    assert checked.returncode == 0, checked.stdout + checked.stderr
    assert "Import report of the host's load of report_app@dev: outcome check" in checked.stdout


DEFERRING_APP = """
import heavy
import unused_heavy  # noqa: F401


def page(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [str(heavy.VALUE).encode()]
"""


def test_the_workers_section_says_which_deferrals_pay_off(tmp_path: Path) -> None:
    """Issue 793: of two imports the host deferred, the one the view uses
    is advised for preload_modules once every worker loaded it, and the
    one nothing uses is paying off."""

    runtime = support.short_dir("muru-rt-")
    (tmp_path / "deferring_app.py").write_text(DEFERRING_APP)
    (tmp_path / "heavy.py").write_text("VALUE = 42\n")
    (tmp_path / "unused_heavy.py").write_text("")
    (tmp_path / "pyproject.toml").write_text(
        '[tool.muru]\nentrypoint = "deferring_app:page"\nlazy_imports = true\n'
        "import_report = true\n\n"
        '[tool.unwrapt.lazy_modules]\n"deferring_app" = ["heavy", "unused_heavy"]\n'
    )
    server = support.start(None, chdir=tmp_path, workers=1, env={"MURU_RUN_DIR": str(runtime)})
    muructl = [
        str(support.muructl_path()),
        "debug",
        "imports",
        "--json",
        "--socket",
        f"{runtime}/runs/{server.port}/admin.sock",
    ]

    try:
        assert server.wait_for_log("admin socket"), server.log()
        assert server.get("/").body == b"42"

        # The host sends its report once the run connects to it.

        deadline = time.monotonic() + 30.0
        found: dict[str, Any] = {}

        while time.monotonic() < deadline:
            answered = subprocess.run(muructl, capture_output=True, text=True, timeout=60)
            found = json.loads(answered.stdout)

            if found.get("report"):
                break

            time.sleep(0.1)

        workers = found["workers"]
        modules = {row["module"]: row["advice"] for row in workers["modules"]}
        assert modules["heavy"] == (
            "deferred in the host; loaded by 1 of 1 worker: "
            'load it before the fork with preload_modules = ["heavy"]'
        ), workers
        never = {row["module"]: row["advice"] for row in workers["never_used"]}
        assert never["unused_heavy"] == (
            "deferred in the host; loaded by 0 of 1 worker: the deferral is paying off"
        ), workers
    finally:
        server.stop()
