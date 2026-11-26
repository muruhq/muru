"""`muructl daemon reset` (issue 935): an instance put back to nothing,
with everything of it stopped, after listing what goes and what stays."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon


def test_reset_removes_a_stopped_instances_state_and_nothing_while_it_runs(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    state = tmp_path / "state"
    muructl = support.muructl_path()
    assert muructl is not None

    def reset(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                str(muructl),
                "daemon",
                "reset",
                "--config",
                str(tmp_path / "muru.toml"),
                "--run-dir",
                str(daemon.run_dir),
                "--state-dir",
                str(state),
                "--log-dir",
                str(tmp_path / "logs"),
                *arguments,
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
            stdin=subprocess.DEVNULL,
        )

    try:
        daemon.deploy("shop", SHOP)
        assert daemon.get("/").status == 200

        # While any of it runs nothing is removed, with --yes or without,
        # and the refusal names what runs.

        running = reset("--yes")
        assert running.returncode == 1, running.stdout + running.stderr
        assert "muru: a daemon answers at" in running.stderr, running.stderr
        assert "the instance is running: " in running.stderr and "host (pid" in running.stderr
        assert "nothing was removed: stop it all first with muructl daemon stop --shutdown" in (
            running.stderr
        )
        assert (state / "muru.sqlite").exists() and daemon.get("/").status == 200

        # Detached, its edge and hosts still serve, and it is refused too.

        daemon.stop(shutdown=False)
        detached = reset("--dry-run")
        assert detached.returncode == 1, detached.stdout + detached.stderr
        assert "the instance is detached: " in detached.stderr, detached.stderr

        again = start_daemon(tmp_path, run_dir=daemon.run_dir, listeners=[(daemon.port, False)])
        again.stop()
    finally:
        if daemon.process.poll() is None:
            daemon.stop()

    # Stopped: a dry run lists and removes nothing, and says a reset
    # would go ahead by its exit code.

    listed = reset("--dry-run", "--json")
    assert listed.returncode == 0, listed.stdout + listed.stderr
    plan = json.loads(listed.stdout)
    assert plan["allowed"] is True and plan["running"] == [], plan
    removed = {Path(item["path"]).name: item["what"] for item in plan["remove"]}
    assert removed["muru.sqlite"] == (
        "the state store: 1 application, 1 release, 0 hosts and 0 routes"
    ), removed
    kept = {Path(item["path"]).name for item in plan["keep"]}
    assert {"muru.toml", "data", "logs"} <= kept, kept
    assert (state / "muru.sqlite").exists()

    # With nobody to ask and no --yes the list is all there is.

    unasked = reset()
    assert unasked.returncode == 1, unasked.stdout + unasked.stderr
    assert "give --yes to go ahead without a terminal" in unasked.stderr, unasked.stderr
    assert (state / "muru.sqlite").exists()

    done = reset("--yes")
    assert done.returncode == 0, done.stdout + done.stderr
    assert not (state / "muru.sqlite").exists()
    assert (state / "data").is_dir() and (tmp_path / "muru.toml").exists()
    assert (tmp_path / "logs").is_dir()

    # The next start finds nothing to serve, and --data takes the data.

    fresh = start_daemon(tmp_path, run_dir=daemon.run_dir, listeners=[(daemon.port, False)])

    try:
        assert fresh.muructl("release", "list").stdout.strip() == "no releases"
    finally:
        fresh.stop()

    assert reset("--yes", "--data", "--logs").returncode == 0
    assert not (state / "data").exists()
