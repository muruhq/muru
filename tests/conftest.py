"""Shared pytest configuration: skip Linux-only tests on other platforms,
give the tests a run directory of their own, and leave nothing behind:
what a test leaves running is ended when it ends, and the short directories
under /tmp, the session's temporary directory and temporary logs go when
the session does. A test that fails with a daemon running, or stopped on
the way out, reports what the daemon saw."""

from __future__ import annotations

import os
import sys
import tempfile
from collections.abc import Iterator

import pytest
from support import daemon as daemons
from support import server


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Skip tests marked requires_linux when not running on Linux."""

    if sys.platform.startswith("linux"):
        return

    skip = pytest.mark.skip(reason="needs Linux-only features")

    for item in items:
        if "requires_linux" in item.keywords:
            item.add_marker(skip)


def pytest_sessionstart(session: pytest.Session) -> None:
    """Keep what the tests' processes write out of the user's directories.

    A test that does not name a run directory would otherwise use the
    default one, and a run record it leaves there is found by commands run
    outside the tests. Temporary files go in a directory of the session's
    too, so that a development run a test kills cannot leave its per-run
    directory behind. pytest's own temporary directory is settled first,
    so that a failed test's files are still there to look at.
    """

    tempfile.gettempdir()
    os.environ.setdefault("MURU_RUN_DIR", str(server.short_dir("muru-rt-")))
    os.environ["TMPDIR"] = str(server.short_dir("muru-tmp-"))


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[None]) -> Iterator[None]:
    """Add each daemon's timeline, state and logs to a failing test's report.

    pytest shows a report's sections only when it fails, so a test that
    passes costs nothing (issue 734). As a test ends, the daemons that
    have stopped are forgotten.
    """

    outcome = yield
    report: pytest.TestReport = outcome.get_result()

    if report.failed:
        for started in daemons.STARTED:
            report.sections.extend(started.failure_report())

    if report.when == "teardown":
        daemons.STARTED[:] = [kept for kept in daemons.STARTED if kept.process.poll() is None]


@pytest.fixture(autouse=True)
def ended_processes() -> Iterator[None]:
    """End what a test left running from the directories it made.

    A directory a module's daemon uses was made before this fixture runs,
    so it is left for the session's end. Fixtures finish before this one,
    so a daemon stopped by its fixture is already gone.
    """

    made = server.short_dirs_made()
    yield
    server.end_processes_since(made)


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """End what still runs and remove what the tests made."""

    server.remove_short_dirs()
