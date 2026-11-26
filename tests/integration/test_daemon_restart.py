"""Restarting the daemon in place (issue 940): `muructl daemon restart`
re-executes the daemon from the binary it runs, keeping its pid, its
hosts and its edge, so that the settings read only at start take effect;
`--restart` on `muructl daemon config` does so when a change needs it."""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from support.load import Load


def edge_pid_of(daemon: Daemon) -> int:
    status = daemon.muructl("status").stdout
    return int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))


def test_a_restart_keeps_everything_serving_and_reads_the_file_afresh(tmp_path: Path) -> None:
    files = tmp_path / "deploy.d"
    files.mkdir()
    shop = files / "shop.toml"
    release = (
        f"[app.shop.release.{{}}]\nsource = '{SHOP}'\npython = '{sys.executable}'\nprocesses = 1\n"
    )
    shop.write_text(release.format("r1"))
    daemon = start_daemon(
        tmp_path, listeners=[(support.free_port(), False)], arguments=["--apply", str(files)]
    )

    try:
        # Ready means the files of the start are applied.

        deadline = time.monotonic() + 30

        while (
            daemon.muructl("probe", "startup", check=False).returncode != 0
            and time.monotonic() < deadline
        ):
            time.sleep(0.1)

        worker = json.loads(daemon.get("/info").body)["pid"]
        edge = edge_pid_of(daemon)
        pid = daemon.process.pid

        # A change the reload applies needs no restart, and gets none.

        plain = daemon.muructl(
            "daemon", "config", "set", "limits.max_body_size", "20MB", "--restart"
        )
        assert "no setting read at start changed, so the daemon is not restarted" in plain.stdout
        assert "daemon.restarted" not in daemon.events()

        # A setting read at start: the reload says it waits, and the
        # restart follows. The files the start applied are not applied
        # again, though they now declare another release.

        shop.write_text(release.format("r1") + release.format("r2"))
        late = daemon.muructl("daemon", "config", "set", "daemon.detours", "true", "--restart")
        assert "[daemon] detours changed and is read at start" in late.stdout, late.stdout
        assert f"muru: daemon (pid {pid}) re-executing from " in late.stdout, late.stdout
        assert late.stdout.splitlines()[-1] == f"muru: daemon restarted (pid {pid})", late.stdout

        # The same process, the same edge, the same host.

        assert daemon.process.poll() is None
        assert edge_pid_of(daemon) == edge
        assert json.loads(daemon.get("/info").body)["pid"] == worker
        events = daemon.events()
        after = events[events.index("daemon.restarted") :]
        assert "release.adopted" in after, after
        assert not {"daemon.started", "edge.replaced", "release.created"} & set(after), after
        assert {row["release"] for row in daemon.releases("shop")} == {"shop@r1"}

        # The daemon now runs with what the file says, so a reload finds
        # nothing waiting.

        reloaded = daemon.muructl("daemon", "reload").stdout
        assert "changed and is read at start" not in reloaded, reloaded

        # A file the daemon would refuse fails the restart, and the
        # daemon stays as it is.

        config = tmp_path / "muru.toml"
        good = config.read_text()
        config.write_text(good.replace("20MB", "lots"))
        refused = daemon.muructl("daemon", "restart", check=False)
        assert refused.returncode != 0, refused.stdout
        assert "is not a size" in refused.stderr, refused.stderr
        assert "the daemon is not restarted" in refused.stderr, refused.stderr
        assert daemon.events().count("daemon.restarted") == 1
        config.write_text(good)

        # By its own command and under load, no request fails.

        with Load(daemon.port) as load:
            again = daemon.muructl("daemon", "restart")

        load.check("the restart", retries_at_most=4)
        assert again.stdout.splitlines()[-1] == f"muru: daemon restarted (pid {pid})", again.stdout
        assert daemon.events().count("daemon.restarted") == 2
        assert edge_pid_of(daemon) == edge
    finally:
        daemon.stop()
