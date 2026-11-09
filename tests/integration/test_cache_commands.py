"""The muructl cache commands on the user cache and the daemon's
re-creatable data (design specification section 13.8), through a running
daemon and with none running."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, start_daemon


def muructl(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run muructl with no daemon of its own to find."""

    binary = support.muructl_path()
    assert binary is not None
    completed = subprocess.run(
        [str(binary), *arguments],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )

    if check and completed.returncode != 0:
        raise AssertionError(f"{' '.join(arguments)} failed: {completed.stdout}{completed.stderr}")

    return completed


def write(path: Path, content: bytes = b"x" * 8192) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)


def by_name(report: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {item["name"]: item for item in report["entries"]}


def seed_user_cache(cache: Path) -> None:
    write(cache / "runtime/1.0.0/cp313/muru/__init__.py")
    write(cache / "runtime/1.1.0/cp314t/muru/__init__.py")
    write(cache / "certs/dev.pem")


def seed_state(state: Path) -> None:
    write(state / "runtime/1.0.0/cp313/muru/__init__.py")
    write(state / "uv-cache/alice/wheel")
    write(state / "python/cpython-3.13/bin/python3.13")


def test_cache_list_through_a_daemon_and_without_one(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    seed_user_cache(cache)
    state = tmp_path / "state"
    run = support.short_dir("muru-run-")
    where = ["--cache-dir", str(cache), "--run-dir", str(run), "--state-dir", str(state)]

    # With no daemon both areas come from the files.

    seed_state(state)
    report = json.loads(muructl("cache", "list", *where, "--json").stdout)
    user = by_name(report["user"])
    assert user["1.0.0/cp313"]["kind"] == "runtime" and user["1.0.0/cp313"]["size"] > 0
    assert user["1.1.0/cp314t"]["tag"] == "cp314t"
    assert user["certs"]["kind"] == "certs"
    assert report["user"]["total"] > 0
    daemon_area = by_name(report["daemon"])
    assert daemon_area["uv-cache/alice"]["kind"] == "uv_cache"
    assert daemon_area["python/cpython-3.13"]["kind"] == "python"

    text = muructl("cache", "list", *where).stdout
    assert "USER CACHE" in text and "DAEMON DATA" in text and "1.0.0/cp313" in text

    only = json.loads(muructl("cache", "list", "user", *where, "--json").stdout)
    assert "daemon" not in only and "user" in only

    # Through a daemon the daemon's area comes from it.

    daemon = start_daemon(tmp_path, run_dir=run, listeners=[(support.free_port(), False)])

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        report = json.loads(muructl("cache", "list", *where, "--json").stdout)
        assert by_name(report["daemon"])["python/cpython-3.13"]["size"] > 0

        # A --state-dir that is not the daemon's is refused.

        refused = muructl(
            "cache",
            "list",
            "daemon",
            "--run-dir",
            str(run),
            "--state-dir",
            str(tmp_path),
            check=False,
        )
        assert refused.returncode != 0 and "keeps its state in" in refused.stderr
    finally:
        daemon.stop()
