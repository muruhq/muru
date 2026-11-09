"""The muructl logs commands on the log files (design specification
section 11.3): list, truncate, rotate and prune, through a running daemon
and with none running."""

from __future__ import annotations

import json
import subprocess
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
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )

    if check and completed.returncode != 0:
        raise AssertionError(f"{' '.join(arguments)} failed: {completed.stdout}{completed.stderr}")

    return completed


def by_name(report: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {log["name"]: log for log in report["logs"]}


def test_logs_commands_through_a_daemon_and_without_one(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs"

    try:
        assert daemon.deploy("shop", SHOP, "--processes", "1") == (
            "muru: release shop@r1 is ready and serving"
        )

        for _ in range(5):
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

        # With the daemon stopped the same listing comes from the files.

        daemon.stop()
        report = json.loads(direct(daemon, "logs", "list", "--json").stdout)
        assert report["orphans_known"] is True
        assert by_name(report)["apps/gone/r9.log"]["orphaned"] is True
        assert by_name(report)["apps/shop/r1.log"]["orphaned"] is False
    finally:
        daemon.stop()
