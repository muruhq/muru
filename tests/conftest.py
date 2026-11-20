"""Shared pytest configuration: skip Linux-only tests on other platforms, give
the tests a run directory of their own, and leave nothing behind: every
instance the session starts carries its label, what a test leaves running is
found through the instance registry, ended and failed as that test's, and
the short directories under /tmp, the session's temporary directory and
temporary logs go when the session does. A test that fails with a daemon or
`muructl run` server running, or stopped on the way out, reports what it
saw, and under the "failed" retention policy only the temporary directories
of the test files that had a failure are kept."""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import daemon as daemons
from support import instances, server

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

    # Issue 812: every instance the session starts is labelled, so what
    # it leaves behind is found by the label.

    os.environ.setdefault(instances.LABEL_VARIABLE, f"pytest-{os.getpid()}")


def pytest_configure(config: pytest.Config) -> None:
    """Name the marker of a test that leaves an instance detached on
    purpose."""

    config.addinivalue_line(
        "markers",
        "leaves_detached: the test leaves a detached instance for the session's end to stop",
    )


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
def nothing_left_running(request: pytest.FixtureRequest) -> Iterator[None]:
    """Fail a test that leaves an instance running, or a process of one.

    The session's instances in the registry before the test, a module's
    or the session's fixtures' among them, are left alone; fixtures of the
    test's own finish before this one, so a daemon its fixture stopped is
    gone. With nothing started and no new entry the check is a directory
    listing. What was left is stopped and pruned, then named in the
    test's error.
    """

    label = instances.label()
    before = instances.labelled_ids(label)
    known = {id(started) for started in [*daemons.STARTED, *server.STARTED]}
    yield

    new = instances.labelled_ids(label) - before
    started_now = any(id(started) not in known for started in [*daemons.STARTED, *server.STARTED])

    if not new and not started_now:
        return

    allow_detached = request.node.get_closest_marker("leaves_detached") is not None
    rows, orphans = instances.leftovers(label, new, allow_detached)

    if rows or orphans:
        instances.end(rows, label)
        pytest.fail(f"the test left Muru running:\n{instances.describe(rows, orphans)}")


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """End what still runs and remove what the tests made, and, under the
    "failed" retention policy, the directories of skipped tests and the
    wider fixtures' directories of the test files that had no failure."""

    # Whatever is still labelled for the session, detached instances
    # left on purpose among them, is stopped and its entries removed.

    swept = instances.sweep(instances.label())

    if swept and (swept.get("stopped") or swept.get("removed")):
        reporter = session.config.pluginmanager.get_plugin("terminalreporter")

        if reporter is not None:
            reporter.write_line(
                f"muru: the session's end stopped {len(swept['stopped'])} processes and "
                f"removed {len(swept['removed'])} registry entries"
            )

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
