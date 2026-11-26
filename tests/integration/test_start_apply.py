"""`--apply` at start (issue 934): files applied once the daemon is up,
at every start and to a daemon that is already running, with readiness
waiting for them and a failed apply failing the start."""

from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

import pytest
from support import server as support
from support.daemon import SHOP, inherited, start_daemon


def release(release_id: str, threads: int) -> str:
    return (
        f"[app.shop.release.{release_id}]\nsource = '{SHOP}'\n"
        f"python = '{sys.executable}'\nthreads = {threads}\n\n"
    )


def test_a_start_applies_its_files_and_fails_when_it_cannot(tmp_path: Path) -> None:
    files = tmp_path / "deploy.d"
    files.mkdir()
    shop = files / "shop.toml"
    shop.write_text(release("r1", 1))
    port = support.free_port()
    muructl = support.muructl_path()
    assert muructl is not None

    daemon = start_daemon(tmp_path, listeners=[(port, False)], arguments=["--apply", str(files)])
    where = [
        "--state-dir",
        str(tmp_path / "state"),
        "--run-dir",
        str(daemon.run_dir),
        "--log-dir",
        str(tmp_path / "logs"),
        "--cache-dir",
        str(tmp_path / "cache"),
        "--config",
        str(tmp_path / "muru.toml"),
    ]

    def ready() -> bool:
        return daemon.muructl("probe", "startup", check=False).returncode == 0

    def serving() -> dict[str, str]:
        return {row["release"]: row["serving"] for row in daemon.releases("shop")}

    try:
        # Ready means applied: the release the files declare serves.

        deadline = time.monotonic() + 30

        while not ready() and time.monotonic() < deadline:
            time.sleep(0.1)

        assert ready(), daemon.log()
        assert serving() == {"shop@r1": "yes"}
        assert daemon.get("/").status == 200

        # What is pending is said before anything of the apply happens.

        events = daemon.events()
        assert events.index("config.apply_pending") < events.index("release.created"), events

        # muructl up with --apply applies to the daemon that already runs.

        shop.write_text("[app.shop]\nserving = 'r2'\n\n" + release("r1", 1) + release("r2", 2))
        up = subprocess.run(
            [str(muructl), "up", *where, "--apply", str(files)],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
            env=inherited(),
        )
        assert up.returncode == 0, up.stdout + up.stderr
        assert "muru: the daemon is already running" in up.stdout, up.stdout
        assert serving() == {"shop@r2": "yes", "shop@r1": ""}

        # Detached, its edge and host serve on. A start whose files cannot
        # be applied fails, and leaves them serving.

        daemon.stop(shutdown=False)
        shop.write_text(shop.read_text() + "[host.shop]\nnames = 7\n")
        failed = subprocess.run(
            [str(muructl), "daemon", "run", *where, "--apply", str(files)],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
            env=inherited(),
        )
        assert failed.returncode != 0, failed.stdout + failed.stderr
        said = failed.stdout + failed.stderr
        assert f"start apply of {files} failed: " in said, said
        assert "what was serving is left serving" in said, said
        assert daemon.get("/").status == 200

        # With the file put right, the next start takes the hosts back,
        # says what is pending, and applies it with --prune.

        shop.write_text("[app.shop]\nserving = 'r2'\n\n" + release("r2", 2) + release("r3", 3))
        again = start_daemon(
            tmp_path,
            run_dir=daemon.run_dir,
            listeners=[(port, False)],
            arguments=["--apply", str(files), "--prune"],
        )

        try:
            deadline = time.monotonic() + 30

            while (
                again.muructl("probe", "startup", check=False).returncode != 0
                and time.monotonic() < deadline
            ):
                time.sleep(0.1)

            left = {row["release"] for row in again.releases("shop")}
            assert left == {"shop@r1", "shop@r2", "shop@r3"}, left
            assert "the files do not declare" not in again.log()
            assert again.get("/").status == 200
        finally:
            again.stop()
    finally:
        if daemon.process.poll() is None:
            daemon.stop()


@pytest.mark.parametrize("flag", ["--prune"])
def test_prune_at_start_needs_files(tmp_path: Path, flag: str) -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    refused = subprocess.run(
        [str(muructl), "daemon", "run", flag],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
        env=inherited(),
    )
    assert refused.returncode != 0 and "--apply" in refused.stderr, refused.stderr
