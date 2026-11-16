"""The steps of the commands and migrations tutorial, with its own files.

docs/tutorials/commands-and-migrations.md includes the files under
docs/tutorials/code/catalog, and this test runs them through the page's
steps, so a change that breaks one fails here rather than for a reader.
The project starts as the page's first step has it, without the gate,
the second migration and the cleanup, and gains them as the page adds
them. The backup is shortened, and the schedule also runs when the daemon
starts from nothing, so its first run comes at once rather than at the
next minute.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

CATALOG = support.ROOT / "docs" / "tutorials" / "code" / "catalog"


def wait_for(condition: Callable[[], bool], timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout

    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.25)


def text(daemon: Daemon, *arguments: str, check: bool = True) -> str:
    completed = daemon.muructl(*arguments, check=check)
    return completed.stdout + completed.stderr


def started(output: str) -> str:
    """The run a detached command started."""

    return output.split("started run ")[1].split(";")[0]


def test_the_commands_and_migrations_tutorial(tmp_path: Path) -> None:
    project = tmp_path / "catalog"
    shutil.copytree(CATALOG, project, ignore=shutil.ignore_patterns("broken"))
    (project / "migrations" / "0002_stock.sql").unlink()
    pyproject = project / "pyproject.toml"
    full = pyproject.read_text().replace(
        'inputs.seconds = { default = "30"', 'inputs.seconds = { default = "4"'
    )
    assert 'default = "4"' in full
    first = full.replace("before_fork = true\n", "")
    first = first[: first.index("[tool.muru.commands.cleanup]")].rstrip() + "\n"
    pyproject.write_text(first)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    try:
        # Declare a command and run it.

        assert daemon.deploy("catalog", project) == "muru: release catalog@r1 is ready and serving"
        listed = text(daemon, "command", "list", "catalog")
        assert "input price (default 1.00, matching [0-9]+[.][0-9][0-9])" in listed, listed
        assert "timeout 600s, one run at a time" in listed, listed
        migrated = text(daemon, "command", "run", "catalog", "migrate")
        assert migrated == "applied 0001_products.sql\nthe schema is up to date\n", migrated
        added = text(
            daemon,
            "command",
            "run",
            "catalog",
            "add-product",
            "--input",
            "name=teapot",
            "--input",
            "price=12.50",
        )
        assert added == "added teapot at 12.50\n", added
        refused = text(
            daemon,
            "command",
            "run",
            "catalog",
            "add-product",
            "--input",
            "name=cup",
            "--input",
            "price=4",
            check=False,
        )
        assert 'add-product input price: "4" does not match [0-9]+[.][0-9][0-9]' in refused
        assert daemon.get().body == b"teapot  12.50\n"

        # A migration as a gate: it passes with the second migration, and
        # a third that fails refuses the release, the one serving going on.

        pyproject.write_text(full[: full.index("[tool.muru.commands.cleanup]")].rstrip() + "\n")
        shutil.copy(CATALOG / "migrations" / "0002_stock.sql", project / "migrations")
        assert daemon.deploy("catalog", project) == "muru: release catalog@r2 is ready and serving"
        runs = text(daemon, "command", "runs", "catalog")
        assert "3      migrate     r2       gate:before_fork" in runs, runs
        assert "gate migrate before_fork passed" in text(daemon, "release", "show", "catalog@r2")
        shutil.copy(CATALOG / "broken" / "0003_restock.sql", project / "migrations")
        failed = text(daemon, "deploy", "catalog", str(project), check=False)
        assert "Error: the before_fork gate migrate exited with status 1 after" in failed, failed
        releases = text(daemon, "release", "list", "catalog")
        assert "catalog@r3               failed" in releases, releases
        assert daemon.get().body == b"teapot  12.50\n"
        logged = text(daemon, "command", "logs", "4")
        assert "[command migrate] 0003_restock.sql failed: duplicate column name: stock" in logged
        (project / "migrations" / "0003_restock.sql").unlink()

        # One run at a time.

        holder = subprocess.Popen(
            [str(support.muructl_path()), "command", "run", "catalog", "backup"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env={**os.environ, "MURU_SOCKET": str(daemon.run_dir / "admin.sock")},
        )
        wait_for(lambda: "under way" in text(daemon, "command", "runs", "catalog", "backup"))
        second = text(daemon, "command", "run", "catalog", "backup", check=False)
        assert "Error: backup runs one at a time for catalog, and run 5 is under way" in second
        output = holder.communicate(timeout=30)[0]
        assert "copied 4 of 4" in output and "backed up to backups/catalog-" in output, output

        # A long run, detached: followed, left, stopped.

        run = started(text(daemon, "command", "run", "catalog", "backup", "--detach"))
        assert run == "6"
        status = text(daemon, "command", "status", run)
        assert "by " in status and "manual, detached" in status and "under way" in status, status
        wait_for(lambda: "exited with status 0" in text(daemon, "command", "status", run))
        followed = text(daemon, "command", "logs", run)
        assert "[command backup] copied 1 of 4" in followed, followed
        long = started(
            text(
                daemon, "command", "run", "catalog", "backup", "--detach", "--input", "seconds=600"
            )
        )
        assert (
            text(daemon, "command", "stop", long).strip() == f"muru: stopping run {long} of backup"
        )
        wait_for(lambda: "stopped because" in text(daemon, "command", "status", long))
        assert "exit       143, failed" in text(daemon, "command", "status", long)

        # It outlives the daemon.

        outliving = started(text(daemon, "command", "run", "catalog", "backup", "--detach"))
        daemon.stop(shutdown=False)
        daemon = start_daemon(tmp_path / "daemon", run_dir=daemon.run_dir, port=daemon.port)
        assert "under way (detached" in text(daemon, "command", "status", outliving)
        wait_for(lambda: "exit       0" in text(daemon, "command", "status", outliving))

        # On a schedule: listed with its next time, run by the schedule,
        # and skipped while a run is under way.

        pyproject.write_text(
            full.replace(
                'schedule = { cron = "* * * * *" }',
                'schedule = { cron = "* * * * *", at_boot = true }',
            )
        )
        assert daemon.deploy("catalog", project) == "muru: release catalog@r4 is ready and serving"
        assert "schedule: * * * * *" in text(daemon, "command", "list", "catalog")
        daemon.stop(shutdown=True)
        daemon = start_daemon(tmp_path / "daemon", run_dir=daemon.run_dir, port=daemon.port)

        def cleanups() -> list[dict[str, object]]:
            listed = text(daemon, "command", "runs", "catalog", "cleanup", "--json")
            return json.loads(listed).get("runs", [])

        wait_for(lambda: bool(cleanups()) and not cleanups()[0]["under_way"])
        assert cleanups()[0]["trigger"] == "schedule"
        held = started(
            text(daemon, "command", "run", "catalog", "cleanup", "--detach", "--input", "pause=70")
        )
        wait_for(
            lambda: f"since run {held} is under way" in text(daemon, "events"),
            timeout=75,
        )
        text(daemon, "command", "stop", held)
    finally:
        daemon.stop()
