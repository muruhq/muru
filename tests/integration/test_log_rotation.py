"""Built-in log rotation (design specification section 11.3): the daemon
renames a log that reaches its size, has the writers reopen their files,
compresses the archive in the background and keeps a bounded number of
archives; external rotation turns it off."""

from __future__ import annotations

import gzip
import json
import sqlite3
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon


def wait_for(
    predicate: Callable[[], bool],
    what: str,
    timeout: float = 30.0,
    describe: Callable[[], str] = lambda: "",
) -> None:
    """Wait for the predicate, failing with what was awaited and `describe()`."""

    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {what}\n{describe()}")

        time.sleep(0.1)


def listing(directory: Path) -> str:
    """Every file under the directory with its size, one a line."""

    return "\n".join(
        f"{path.relative_to(directory)} {path.stat().st_size}"
        for path in sorted(directory.rglob("*"))
        if path.is_file()
    )


def archives(directory: Path, name: str) -> list[Path]:
    return sorted(path for path in directory.glob(f"{name}.*") if path.name != name)


def test_a_period_passed_while_the_daemon_was_stopped_rotates_the_log_once(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs"

    def state() -> str:
        return f"{listing(logs)}\n{daemon.muructl('events').stdout}"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(5):
            assert daemon.get("/catalogue").status == 200

        # The first daemon has checked every file once, so each has a
        # period recorded; stopping it leaves the edge and the host
        # writing. The records are put back a day, standing in for a
        # stop over midnight.

        wait_for(lambda: (logs / "access.log").stat().st_size > 0, "the access log", describe=state)
        time.sleep(1.5)
        daemon.stop(shutdown=False)

        with sqlite3.connect(tmp_path / "state" / "muru.sqlite") as store:
            assert store.execute("SELECT count(*) FROM log_periods").fetchone()[0] > 0
            store.execute("UPDATE log_periods SET period = 'd1'")

        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, listeners=daemon.listeners)
        wait_for(lambda: len(archives(logs, "access.log")) == 1, "one archive", describe=state)

        # The record is updated, so a later check and a further restart
        # leave the file alone.

        assert daemon.get("/catalogue").status == 200
        time.sleep(2.5)
        daemon.stop(shutdown=False)
        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, listeners=daemon.listeners)
        assert daemon.get("/catalogue").status == 200
        time.sleep(2.5)
        assert len(archives(logs, "access.log")) == 1, listing(logs)
    finally:
        daemon.stop()


def test_a_rotation_policy_added_by_reload_takes_effect(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(60):
            assert daemon.get("/catalogue").status == 200

        time.sleep(1.5)
        assert archives(logs, "access.log") == []

        with (daemon.base / "muru.toml").open("a") as handle:
            handle.write("\n[logs]\nsize = '4KB'\nkeep = 2\ncompress = 'off'\nevery = 'off'\n")

        reloaded = daemon.muructl("daemon", "reload").stdout
        assert "changed [logs] rotation (the daemon rotates on the new policy)" in reloaded, (
            reloaded
        )
        wait_for(lambda: len(archives(logs, "access.log")) >= 1, "an access log archive")
    finally:
        daemon.stop()


def test_logs_are_rotated_by_size_compressed_and_pruned(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[logs]\nsize = '4KB'\nkeep = 2\ncompress = 'gzip'\nevery = 'off'\n",
    )
    logs = tmp_path / "logs"

    def state() -> str:
        return f"{listing(logs)}\n{daemon.muructl('events').stdout}"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        # Each request adds an access log line and a release log line;
        # enough of them pass the size and the daemon rotates on its tick.

        def rotated(directory: Path, name: str) -> Callable[[], bool]:
            def check() -> bool:
                for _ in range(60):
                    assert daemon.get("/catalogue").status == 200

                return len(archives(directory, name)) >= 1

            return check

        wait_for(rotated(logs, "access.log"), "an access log archive", describe=state)
        wait_for(
            lambda: any(path.suffix == ".gz" for path in archives(logs, "access.log")),
            "the access log archive to be compressed",
            describe=state,
        )
        events = daemon.muructl("events").stdout
        assert "logs.rotated" in events and "access.log." in events
        archive = next(path for path in archives(logs, "access.log") if path.suffix == ".gz")
        lines = gzip.decompress(archive.read_bytes()).decode().splitlines()
        assert lines and json.loads(lines[0])["app"] == "shop"
        assert archive.name.count(".") == 3, "access.log.YYYYMMDD-HHMMSS.gz"

        # The writers went on into fresh files, which the CLI follows.

        assert daemon.get("/catalogue").status == 200
        wait_for(
            lambda: (logs / "access.log").exists() and (logs / "access.log").stat().st_size > 0,
            "a fresh access log",
            describe=state,
        )
        assert "/catalogue" in daemon.muructl("logs", "--access", "--lines", "5").stdout

        # A release log line is shorter than an access log line, so the
        # release log can still be under the size when the access log
        # first rotates; the requests go on until it passes it too.

        wait_for(rotated(logs / "apps" / "shop", "r1.log"), "a release log archive", describe=state)
        wait_for(
            lambda: "shop: served" in daemon.muructl("logs", "shop", "--lines", "20").stdout,
            "the release log to be followed",
            describe=state,
        )

        # Only the newest archives are kept.

        def pruned() -> bool:
            for _ in range(60):
                daemon.get("/catalogue")

            kept = [path for path in archives(logs, "access.log") if path.suffix == ".gz"]
            return len(kept) >= 2 and len(archives(logs, "access.log")) <= 3

        wait_for(pruned, "the archives to be pruned", timeout=60, describe=state)
        time.sleep(1.0)
        assert len([p for p in archives(logs, "access.log") if p.suffix == ".gz"]) <= 2
    finally:
        daemon.stop()


def test_the_edge_reopens_its_own_log_after_it_is_rotated(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[logs]\nsize = '100B'\nkeep = 20\ncompress = 'off'\nevery = 'off'\n",
        env={"MURU_RENAME_CHECK_MS": "200"},
    )
    logs = tmp_path / "logs"

    def state() -> str:
        return f"{listing(logs)}\n{daemon.muructl('events').stdout}"

    def traffic() -> None:
        for _ in range(30):
            assert daemon.get("/catalogue").status == 200

    def rotated() -> bool:
        traffic()
        return len(archives(logs, "edge.log")) >= 1

    edge_log = logs / "edge.log"

    def reopened() -> bool:
        traffic()
        return f"edge reopened {edge_log}" in edge_log.read_text()

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )
        first = edge_log.stat().st_ino

        # edge.log is renamed like the other logs, and the edge opens it
        # again by name: the line saying so is the first in the new file.

        wait_for(rotated, "an edge log archive", describe=state)
        wait_for(reopened, "the edge to reopen edge.log", describe=state)
        assert edge_log.stat().st_ino != first

        # The rotation's own renames are not taken for someone else's, so
        # the check for renamed files, every 200 ms here, reopens nothing.

        assert "renamed)" not in daemon.muructl("events").stdout
    finally:
        daemon.stop()


def test_external_rotation_leaves_the_files_alone_and_a_rename_is_reopened(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="[logs]\nrotate = 'external'\nsize = '1KB'\n",
        env={"MURU_RENAME_CHECK_MS": "200"},
    )
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(40):
            assert daemon.get("/catalogue").status == 200

        time.sleep(2.5)
        assert archives(logs, "access.log") == []
        assert "logs.rotated" not in daemon.muructl("events").stdout
        assert (logs / "access.log").stat().st_size > 1024

        refused = daemon.muructl("logs", "rotate", "--access", check=False)
        assert refused.returncode != 0 and "rotation is external" in refused.stderr

        # A rename with no signal, as logrotate makes one with no
        # postrotate: each writer opens its file again by name.

        renamed = [logs / "access.log", logs / "edge.log", logs / "apps" / "shop" / "r1.log"]

        for path in renamed:
            path.rename(path.with_name(path.name + ".1"))

        def written_again() -> bool:
            daemon.get("/after-the-rename")
            return all(path.is_file() and path.stat().st_size > 0 for path in renamed)

        wait_for(
            written_again, "every renamed log to be written again", describe=lambda: listing(logs)
        )
        assert "/after-the-rename" in (logs / "access.log").read_text()
        events = daemon.muructl("events").stdout
        assert (
            "reopened the log files (access.log, edge.log, apps/shop/r1.log renamed)" in events
        ), events
    finally:
        daemon.stop()


def test_a_bad_logs_table_is_refused(tmp_path: Path) -> None:
    import pytest

    with pytest.raises(RuntimeError, match="every must be off, hourly, daily or weekly"):
        start_daemon(tmp_path, config_extra="[logs]\nevery = 'fortnightly'\n")
