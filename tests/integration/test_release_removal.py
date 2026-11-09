"""Removing releases and applications from the command line (design
specification section 9.1): what goes with a release, and what removal
never takes."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


def as_json(daemon: Daemon, *arguments: str) -> dict[str, Any]:
    """Run a muructl command asking for JSON and return its answer."""

    return json.loads(daemon.muructl(*arguments, "--json").stdout)


def seed_files(base: Path, app: str, release: str) -> None:
    """Give a release an environment and bytecode, as a build would."""

    for directory in ("envs", "pycache"):
        path = base / "state" / directory / app / release
        path.mkdir(parents=True, exist_ok=True)
        (path / "marker").write_bytes(b"x" * 8192)


def removed_ids(answer: dict[str, Any]) -> list[str]:
    return [item["id"] for item in answer["removed"]]


def test_release_prune_takes_only_what_it_may(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    logs = tmp_path / "logs" / "apps" / "shop"

    try:
        for _ in range(4):
            daemon.deploy("shop", SHOP, "--processes", "1")

        failed = daemon.muructl(
            "deploy",
            "shop",
            str(SHOP),
            "--python",
            sys.executable,
            "--entrypoint",
            "shop.missing:application",
            check=False,
        )
        assert failed.returncode != 0, failed.stdout

        for release in ("r1", "r2", "r5"):
            seed_files(tmp_path, "shop", release)

        archive = logs / "r1.log.20260101-000000.zst"
        archive.write_bytes(b"old log")

        # release list says what each takes.

        listed = {
            item["id"]: item for item in as_json(daemon, "release", "list", "shop")["releases"]
        }
        assert listed["r1"]["disk_bytes"] >= 16384

        # Refused without a terminal, and --keep is required.

        refused = daemon.muructl("release", "prune", "shop", "--keep", "0", check=False)
        assert refused.returncode != 0 and "--yes" in refused.stderr
        assert daemon.muructl("release", "prune", "shop", check=False).returncode != 0

        # A dry run lists what would go and why the rest stays. Serving and
        # rollback stay at --keep 0; the failed release is newer than both.

        plan = as_json(daemon, "release", "prune", "shop", "--keep", "0", "--dry-run")
        assert plan["dry_run"] is True
        assert removed_ids(plan) == ["r1", "r2"]
        assert plan["freed"] > 16384
        kept = {item["id"]: item["why"] for item in plan["kept"]}
        assert kept == {"r3": "rollback", "r4": "serving", "r5": "failed"}
        assert (tmp_path / "state/envs/shop/r1").exists(), "a dry run changes nothing"

        # With --keep-logs the release goes and its log stays.

        answer = as_json(daemon, "release", "prune", "shop", "--keep", "1", "--keep-logs", "--yes")
        assert removed_ids(answer) == ["r1"]
        assert answer["kept"][0]["why"] in {"keep", "rollback", "serving", "failed"}
        assert not (tmp_path / "state/envs/shop/r1").exists()
        assert not (tmp_path / "state/pycache/shop/r1").exists()
        assert (logs / "r1.log").exists() and archive.exists()

        ids = [item["id"] for item in as_json(daemon, "release", "list", "shop")["releases"]]
        assert "r1" not in ids
        assert "shop@r1" in daemon.muructl("events").stdout

        # Without --keep-logs the log goes too.

        answer = as_json(daemon, "release", "prune", "shop", "--keep", "0", "--yes")
        assert removed_ids(answer) == ["r2"]
        assert not (logs / "r2.log").exists()

        # The serving and rollback releases are still there and serving.

        assert daemon.get("/").status == 200

        # A release the applied files declare stays unless forced.

        site = tmp_path / "site.toml"
        site.write_text(
            "[app.shop]\nserving = 'r4'\n\n"
            "[app.shop.release.r6]\n"
            f"source = '{SHOP}'\n"
            f"python = '{sys.executable}'\n"
        )
        daemon.muructl("apply", "-f", str(site))

        plan = as_json(daemon, "release", "prune", "shop", "--keep", "0", "--dry-run")
        assert "r6" not in removed_ids(plan)
        assert {item["id"]: item["why"] for item in plan["kept"]}["r6"] == "declared"
        forced = as_json(daemon, "release", "prune", "shop", "--keep", "0", "--force", "--dry-run")
        assert "r6" in removed_ids(forced)

        # Once newer releases serve and stand ready to roll back to, the
        # failed release goes whatever N is.

        daemon.deploy("shop", SHOP, "--processes", "1")
        daemon.deploy("shop", SHOP, "--processes", "1")
        answer = as_json(daemon, "release", "prune", "shop", "--keep", "9", "--yes")
        assert removed_ids(answer) == ["r5"]
        assert answer["removed"][0]["why"] == "failed"
        assert not (tmp_path / "state/envs/shop/r5").exists()

        # release remove refuses what prune never takes, and says why.

        status = as_json(daemon, "release", "show", "shop@r8")
        assert status["serving"] is True
        refused = daemon.muructl("release", "remove", "shop@r8", "--yes", check=False)
        assert refused.returncode != 0 and "serves" in refused.stderr
        refused = daemon.muructl("release", "remove", "shop@r7", "--yes", check=False)
        assert refused.returncode != 0 and "rollback" in refused.stderr
        refused = daemon.muructl("release", "remove", "shop@r3", check=False)
        assert refused.returncode != 0 and "--yes" in refused.stderr
        missing = daemon.muructl("release", "remove", "shop@r99", "--yes", check=False)
        assert missing.returncode != 0

        # A failed release named goes at once, newer than the cutoff or not.

        failed = daemon.muructl(
            "deploy",
            "shop",
            str(SHOP),
            "--python",
            sys.executable,
            "--entrypoint",
            "shop.missing:application",
            check=False,
        )
        assert failed.returncode != 0, failed.stdout
        answer = as_json(daemon, "release", "remove", "shop@r9", "--yes")
        assert removed_ids(answer) == ["r9"]
        assert answer["removed"][0]["why"] == "named"
        assert not (logs / "r9.log").exists()

        answer = as_json(daemon, "release", "remove", "shop@r3", "--keep-logs", "--yes")
        assert removed_ids(answer) == ["r3"]
        assert (logs / "r3.log").exists()
    finally:
        daemon.stop()
