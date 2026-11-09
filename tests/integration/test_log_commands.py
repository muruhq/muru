"""The muructl logs commands on the log files (design specification
section 11.3): list, truncate, rotate and prune, through a running daemon
and with none running."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


def direct(daemon: Daemon, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run a muructl logs command with no daemon running, on its files."""

    muructl = support.muructl_path()
    assert muructl is not None
    completed = subprocess.run(
        [
            str(muructl),
            *arguments,
            "--run-dir",
            str(daemon.run_dir),
            "--state-dir",
            str(daemon.base / "state"),
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

    if check and completed.returncode != 0:
        raise AssertionError(f"{' '.join(arguments)} failed: {completed.stdout}{completed.stderr}")

    return completed


def wait_for(predicate: Callable[[], bool], what: str, timeout: float = 20.0) -> None:
    """Wait for the predicate, failing with what was awaited."""

    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, f"timed out waiting for {what}"
        time.sleep(0.1)


def by_name(report: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {log["name"]: log for log in report["logs"]}


def test_logs_commands_through_a_daemon_and_without_one(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(30):
            assert daemon.get("/catalogue").status == 200

        # A log whose release no longer exists, with an archive, and a
        # file that is no log at all.

        orphan = logs / "apps" / "gone" / "r9.log"
        orphan.parent.mkdir(parents=True)
        orphan.write_text("old\n")
        (orphan.parent / "r9.log.20260101-000000").write_text("older\n")
        (logs / "notes.txt").write_text("not a log\n")

        # logs list, through the daemon.

        report = json.loads(daemon.muructl("logs", "list", "--json").stdout)
        assert report["orphans_known"] is True
        found = by_name(report)
        assert found["access.log"]["kind"] == "access"
        assert found["access.log"]["size"] > 0
        assert found["apps/shop/r1.log"]["app"] == "shop"
        assert found["apps/shop/r1.log"]["orphaned"] is False
        assert found["apps/gone/r9.log"]["orphaned"] is True
        assert found["apps/gone/r9.log"]["archives"][0]["size"] == 6
        assert "notes.txt" not in found
        assert report["total"] == sum(log["total"] for log in report["logs"])

        text = daemon.muructl("logs", "list").stdout
        assert "apps/gone/r9.log" in text and "orphaned" in text and "total " in text

        # A --log-dir that is not the daemon's is refused rather than
        # working on another directory behind its back.

        refused = daemon.muructl("logs", "list", "--log-dir", str(tmp_path), check=False)
        assert refused.returncode != 0 and "writes its logs to" in refused.stderr

        # truncate needs a target and --keep, and a --yes with no terminal.

        access = logs / "access.log"
        assert (
            "name what to truncate"
            in daemon.muructl("logs", "truncate", "--keep", "0", check=False).stderr
        )
        refused = daemon.muructl("logs", "truncate", "--access", "--keep", "1KB", check=False)
        assert refused.returncode != 0 and "--yes" in refused.stderr
        before = access.stat().st_size
        assert before > 4096
        archive = logs / "access.log.20260103-000000.zst"
        archive.write_bytes(b"kept")

        # Keeping some cuts at a line and leaves valid JSON lines, and the
        # edge goes on into the file that replaced it.

        answer = json.loads(
            daemon.muructl(
                "logs", "truncate", "--access", "--keep", "1KB", "--yes", "--json"
            ).stdout
        )
        assert answer["freed"] > 0 and answer["logs"][0]["error"] == ""
        assert 0 < access.stat().st_size <= 1024 + 2048
        assert archive.read_bytes() == b"kept", "truncate leaves archives alone"
        kept = access.read_text()
        assert kept.endswith("\n") and all(json.loads(line) for line in kept.splitlines())
        assert daemon.get("/truncated").status in (200, 404)
        wait_for(lambda: "/truncated" in access.read_text(), "the edge to write to the new file")
        assert "logs.truncated" in daemon.muructl("events").stdout

        # Keeping none empties the file in place, and the host writes on.

        release_log = logs / "apps" / "shop" / "r1.log"
        answer = json.loads(
            daemon.muructl("logs", "truncate", "shop@r1", "--keep", "0", "--yes", "--json").stdout
        )
        assert answer["logs"][0]["kept"] == 0
        assert release_log.stat().st_size == 0
        assert daemon.get("/catalogue").status == 200
        wait_for(lambda: release_log.stat().st_size > 0, "the host to write on")

        # logs rotate rotates through the daemon's policy, here zstd and
        # 14 archives by default: the edge moves to a fresh file.

        assert daemon.get("/catalogue").status == 200
        wait_for(lambda: access.stat().st_size > 0, "a line in the access log")
        answer = json.loads(daemon.muructl("logs", "rotate", "--access", "--json").stdout)
        assert answer["logs"][0]["name"] == "access.log"
        assert answer["logs"][0]["archive"].startswith("access.log.")
        assert answer["logs"][0]["error"] == ""
        assert "logs.rotated" in daemon.muructl("events").stdout

        wait_for(lambda: access.stat().st_size == 0, "the access log to be moved aside")
        assert daemon.get("/rotated").status in (200, 404)
        wait_for(lambda: "/rotated" in access.read_text(), "the edge to write to a new file")
        wait_for(
            lambda: any(path.suffix == ".zst" for path in logs.glob("access.log.*")),
            "the archive to be compressed",
        )

        # logs prune lists first, then removes by count, by age and the
        # orphans, asking unless told not to.

        wait_for(
            lambda: not list(logs.glob("access.log.*[0-9]")),
            "the archive to be compressed so it is settled",
        )
        (logs / "access.log.20260101-000000.zst").write_bytes(b"old")
        (logs / "access.log.20260102-000000.zst").write_bytes(b"old")
        older = time.time() - 100 * 86400
        os.utime(logs / "access.log.20260101-000000.zst", (older, older))

        assert "name what to prune" in daemon.muructl("logs", "prune", check=False).stderr
        refused = daemon.muructl("logs", "prune", "--orphaned", check=False)
        assert refused.returncode != 0 and "--yes" in refused.stderr

        plan = json.loads(
            daemon.muructl("logs", "prune", "--orphaned", "--dry-run", "--json").stdout
        )
        assert plan["dry_run"] is True
        assert {item["name"] for item in plan["removed"]} == {
            "apps/gone/r9.log",
            "apps/gone/r9.log.20260101-000000",
        }
        assert orphan.exists(), "a dry run changes nothing"

        answer = json.loads(
            daemon.muructl("logs", "prune", "--older-than", "30d", "--yes", "--json").stdout
        )
        assert [item["why"] for item in answer["removed"]] == ["older_than"]
        assert not (logs / "access.log.20260101-000000.zst").exists()

        answer = json.loads(
            daemon.muructl("logs", "prune", "--keep", "1", "--orphaned", "--yes", "--json").stdout
        )
        whys = {item["why"] for item in answer["removed"]}
        assert whys == {"keep", "orphaned"}, answer
        assert answer["freed"] > 0
        assert not orphan.parent.exists(), "the orphan's directory goes with it"
        assert len(list(logs.glob("access.log.*"))) == 1, "the newest archive stays"
        assert "logs.pruned" in daemon.muructl("events").stdout

        # With the daemon stopped the same commands work on the files.

        daemon.stop()
        orphan.parent.mkdir(parents=True)
        orphan.write_text("old\n")

        # The stopped daemon's store is read and never migrated, however
        # far behind this muructl it is.

        store_path = tmp_path / "state" / "muru.sqlite"

        with sqlite3.connect(store_path) as store:
            store.execute("DROP TABLE log_periods")
            store.execute("UPDATE schema SET version = 12")

        report = json.loads(direct(daemon, "logs", "list", "--json").stdout)

        with sqlite3.connect(store_path) as store:
            assert store.execute("SELECT version FROM schema").fetchone() == (12,)
            assert (
                store.execute(
                    "SELECT count(*) FROM sqlite_master WHERE name = 'log_periods'"
                ).fetchone()[0]
                == 0
            )

        assert report["orphans_known"] is True
        assert by_name(report)["apps/gone/r9.log"]["orphaned"] is True
        assert by_name(report)["apps/shop/r1.log"]["orphaned"] is False

        answer = json.loads(
            direct(daemon, "logs", "truncate", "--access", "--keep", "0", "--yes", "--json").stdout
        )
        assert access.stat().st_size == 0 and answer["freed"] > 0

        access.write_text('{"line": 1}\n')
        answer = json.loads(direct(daemon, "logs", "rotate", "--access", "--json").stdout)
        assert answer["logs"][0]["archive"].endswith(".zst")
        assert access.stat().st_size == 0

        answer = json.loads(
            direct(daemon, "logs", "prune", "--keep", "0", "--yes", "--json").stdout
        )
        assert answer["removed"] and not list(logs.glob("access.log.*"))
    finally:
        daemon.stop()
