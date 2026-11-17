"""Shared pytest configuration: skip Linux-only tests on other platforms,
give the tests a run directory of their own, and leave nothing behind:
what a test leaves running is ended when it ends, and the short directories
under /tmp, the session's temporary directory and temporary logs go when
the session does. A test that fails with a daemon or `muructl run` server
running, or stopped on the way out, reports what it saw, and under the
"failed" retention policy only the temporary directories of the test files
that had a failure are kept."""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import daemon as daemons
from support import server

# The directories under pytest's base temporary directory that fixtures
# wider than a test made, by the test file that set them up ("" for the
# session's and a package's), and the test files that had a failure.
# Under the "failed" retention policy pytest removes a passing test's own
# directory but keeps these, so the session's end removes those of the
# files with no failure (issue 770), and pytest the links to them. It
# keeps a skipped test's directory too, which the session's end removes.

_wider_dirs: dict[str, set[Path]] = {}
_failed_files: set[str] = set()
_skipped_dirs: set[Path] = set()


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
    """Add each daemon's timeline, state and logs, and each `muructl run`
    server's log, to a failing test's report.

    pytest shows a report's sections only when it fails, so a test that
    passes costs nothing (issues 734 and 770). As a test ends, the daemons
    and servers that have stopped are forgotten.
    """

    outcome = yield
    report: pytest.TestReport = outcome.get_result()

    if report.failed:
        _failed_files.update({item.nodeid.split("::")[0], ""})

        for started in [*daemons.STARTED, *server.STARTED]:
            report.sections.extend(started.failure_report())

    if report.skipped and isinstance(path := getattr(item, "funcargs", {}).get("tmp_path"), Path):
        _skipped_dirs.add(path)

    if report.when == "teardown":
        daemons.STARTED[:] = [kept for kept in daemons.STARTED if kept.process.poll() is None]
        server.STARTED[:] = [kept for kept in server.STARTED if kept.process.poll() is None]


@pytest.hookimpl(hookwrapper=True)
def pytest_fixture_setup(
    fixturedef: pytest.FixtureDef[object], request: pytest.FixtureRequest
) -> Iterator[None]:
    """Note the directories a fixture wider than a test makes under the
    base temporary directory, by the test file it is set up for."""

    if fixturedef.scope == "function" or fixturedef.argname == "tmp_path_factory":
        yield
        return

    basetemp = request.getfixturevalue("tmp_path_factory").getbasetemp()
    before = set(basetemp.iterdir())
    yield
    made = set(basetemp.iterdir()) - before
    test_file = (
        request.node.nodeid.split("::")[0] if fixturedef.scope in ("module", "class") else ""
    )
    _wider_dirs.setdefault(test_file, set()).update(made)


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
    """End what still runs and remove what the tests made, and, under the
    "failed" retention policy, the directories of skipped tests and the
    wider fixtures' directories of the test files that had no failure."""

    server.remove_short_dirs()

    if session.config.getini("tmp_path_retention_policy") != "failed":
        return

    removed = set(_skipped_dirs)

    for test_file, made in _wider_dirs.items():
        if test_file not in _failed_files:
            removed |= made

    for directory in removed:
        if directory.is_dir() and not directory.is_symlink():
            shutil.rmtree(directory, ignore_errors=True)
