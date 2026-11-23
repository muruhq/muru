"""The instances on one machine (design specification sections 13.8 and
13.10): `muructl instance list` reads the instance registry, says which
instances run, are detached or are stale, and finds the Muru processes
no live instance accounts for."""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, start_daemon


def muructl(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run muructl with the test's environment."""

    path = support.muructl_path()
    assert path is not None
    completed = subprocess.run(
        [str(path), *arguments], capture_output=True, text=True, check=False, timeout=60
    )

    if check and completed.returncode not in (0, 1):
        raise RuntimeError(f"muructl {' '.join(arguments)} failed:\n{completed.stderr}")

    return completed


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def listed(label: str, *flags: str) -> dict[str, Any]:
    """`instance list --json` narrowed to one label."""

    answer: dict[str, Any] = json.loads(
        muructl("instance", "list", "--json", "--label", label, *flags).stdout
    )
    return answer


def test_the_list_shows_each_instance_its_state_and_what_is_left_behind(
    tmp_path: Path,
) -> None:
    label = f"test-{uuid.uuid4().hex[:8]}"
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon", env={"MURU_INSTANCE_LABEL": label})
    run = support.start(
        "plain_wsgi:application", workers=1, threads=1, env={"MURU_INSTANCE_LABEL": label}
    )
    stray = None

    try:
        # A daemon and a run, both running, each with what it listens on.

        rows = {row["kind"]: row for row in listed(label)["instances"]}
        assert set(rows) == {"daemon", "run"}, rows
        assert rows["daemon"]["state"] == "running" and rows["daemon"]["location"] == "local"
        assert rows["daemon"]["pid"] == daemon.process.pid
        assert rows["run"]["tag"] == f"run:{run.port}"
        assert rows["run"]["listeners"][0]["bound"] == f"127.0.0.1:{run.port}"

        table = muructl("instance", "list", "--label", label).stdout
        assert table.splitlines()[0].split() == [
            "NAME",
            "KIND",
            "STATE",
            "PID",
            "UPTIME",
            "LISTEN",
            "DASHBOARD",
            "WHERE",
            "LABEL",
        ], table
        assert f"run:{run.port}" in table and label in table, table

        # A process carrying the ID of an instance that is gone is an
        # orphan, and the listing exits 1.

        stray = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            env={
                **os.environ,
                "MURU_INSTANCE_ID": uuid.uuid4().hex,
                "MURU_INSTANCE_TAG": "gone",
                "MURU_INSTANCE_LABEL": label,
            },
        )
        wait_for(
            lambda: any(
                orphan["pid"] == stray.pid
                for orphan in listed(label, "--orphans").get("orphans", [])
            )
        )
        orphans = listed(label, "--orphans")["orphans"]
        assert [(o["pid"], o["instance"], o["why"]) for o in orphans] == [
            (stray.pid, "gone", "its instance is gone")
        ], orphans
        assert muructl("instance", "list", "--orphans", "--label", label).returncode == 1
        stray.kill()
        stray.wait()

        # The daemon detaches: its lock is free, its edge and hosts carry
        # its ID, and none of them is an orphan.

        daemon.stop(shutdown=False)
        rows = {row["kind"]: row for row in listed(label)["instances"]}
        assert rows["daemon"]["state"] == "detached", rows
        assert listed(label, "--orphans")["orphans"] == []
    finally:
        if stray is not None and stray.poll() is None:
            stray.kill()

        run.stop()

        if daemon.process.poll() is not None:
            daemon = start_daemon(
                tmp_path / "daemon",
                run_dir=daemon.run_dir,
                port=daemon.port,
                env={"MURU_INSTANCE_LABEL": label},
            )

        daemon.stop()

    # A daemon shut down and a run stopped leave no entries.

    assert listed(label)["instances"] == []


def test_a_port_another_instance_holds_names_that_instance(tmp_path: Path) -> None:
    # Issue 808: a bind refused because an instance of Muru's holds the
    # port says which instance, and the ports list shows what each uses.

    label = f"test-{uuid.uuid4().hex[:8]}"
    run = support.start(
        "plain_wsgi:application", workers=1, threads=1, env={"MURU_INSTANCE_LABEL": label}
    )
    muructl_path = support.muructl_path()
    assert muructl_path is not None

    try:
        second = subprocess.run(
            [
                str(muructl_path),
                "run",
                "plain_wsgi:application",
                "--port",
                str(run.port),
                "--dashboard",
                "off",
            ],
            capture_output=True,
            text=True,
            timeout=120,
            cwd=support.FIXTURES,
            check=False,
        )
        assert second.returncode != 0
        assert (
            f"port {run.port} on 127.0.0.1 is in use by the development run of plain-wsgi in "
            in second.stderr
        ), second.stderr
        assert "choose another with --port, or stop it" in second.stderr

        # A daemon is refused before it starts anything.

        base = tmp_path / "daemon"
        refused = subprocess.run(
            [
                str(muructl_path),
                "daemon",
                "start",
                "--listen",
                f"127.0.0.1:{run.port}",
                "--config",
                str(base / "muru.toml"),
                "--state-dir",
                str(base / "state"),
                "--run-dir",
                str(support.short_dir("muru-run-")),
                "--log-dir",
                str(base / "logs"),
                "--cache-dir",
                str(base / "cache"),
            ],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        assert refused.returncode != 0
        assert "in use by the development run of plain-wsgi" in refused.stderr, refused.stderr
        assert "choose another listener, or stop it" in refused.stderr
        assert not (base / "state").exists()

        ports = json.loads(
            muructl("instance", "list", "--ports", "--json", "--label", label).stdout
        )["ports"]
        assert (run.port, "http", "bound") in [(p["port"], p["use"], p["state"]) for p in ports], (
            ports
        )
    finally:
        run.stop()


def test_prune_removes_what_is_stale_and_stops_a_detached_daemon_only_when_asked(
    tmp_path: Path,
) -> None:
    # Issue 809: a run killed outright leaves a stale entry, a daemon that
    # detaches leaves its edge and host serving; prune removes the first
    # and keeps the second until --detached.

    label = f"test-{uuid.uuid4().hex[:8]}"
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon", env={"MURU_INSTANCE_LABEL": label})
    run = support.start(
        "plain_wsgi:application", workers=1, threads=1, env={"MURU_INSTANCE_LABEL": label}
    )

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        run.process.send_signal(signal.SIGKILL)
        run.process.wait(timeout=10)
        daemon.stop(shutdown=False)

        def states() -> dict[str, str]:
            return {row["kind"]: row["state"] for row in listed(label)["instances"]}

        wait_for(lambda: states() == {"daemon": "detached", "run": "stale"}, timeout=20)
        assert daemon.get("/").status == 200

        # Issue 868: selecting either says why it cannot be reached.

        tags = {row["kind"]: row["tag"] for row in listed(label)["instances"]}
        stale = muructl("status", "--instance", tags["run"], check=False).stderr
        assert "which is not running: its entry was left behind" in stale, stale
        detached = muructl("status", "--instance", tags["daemon"], check=False).stderr
        assert "which is not running: its daemon has gone" in detached, detached

        pruned = json.loads(
            muructl("instance", "prune", "--label", label, "--yes", "--json").stdout
        )
        assert [entry["kind"] for entry in pruned["removed"]] == ["run"], pruned
        assert {kept["why"] for kept in pruned["kept"]} >= {
            "it is detached and its processes keep serving; --detached stops them"
        }, pruned
        assert states() == {"daemon": "detached"}
        assert daemon.get("/").status == 200

        pruned = json.loads(
            muructl("instance", "prune", "--label", label, "--detached", "--yes", "--json").stdout
        )
        assert {stopped["role"] for stopped in pruned["stopped"]} >= {"edge", "host"}, pruned
        assert [entry["kind"] for entry in pruned["removed"]] == ["daemon"], pruned
        assert listed(label)["instances"] == []
        assert listed(label, "--orphans")["orphans"] == []
    finally:
        run.stop()

        if daemon.process.poll() is None:
            daemon.stop()


def test_named_instances_take_their_locations_from_their_names(tmp_path: Path) -> None:
    # Issue 810: two daemons on one machine, each a named instance whose
    # locations follow from its name, in a home directory of the test's.

    label = f"test-{uuid.uuid4().hex[:8]}"
    suffix = uuid.uuid4().hex[:6]
    dev, staging = f"dev-{suffix}", f"staging-{suffix}"
    home = tmp_path / "home"
    home.mkdir()
    env = {
        name: value
        for name, value in os.environ.items()
        if name
        not in (
            "MURU_RUN_DIR",
            "MURU_STATE_DIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        )
    }
    env.update({"HOME": str(home), "MURU_INSTANCE_LABEL": label})
    path = support.muructl_path()
    assert path is not None

    def named(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(path), *arguments], capture_output=True, text=True, env=env, timeout=60
        )

    first, second = support.free_port(), support.free_port()
    started: list[str] = []

    try:
        assert named("instance", "create", dev, "--listen", f"127.0.0.1:{first}").returncode == 0
        clash = named("instance", "create", staging, "--listen", f"127.0.0.1:{first}")
        assert f"is configured for instance {dev} already" in clash.stderr, clash.stderr
        assert (
            named("instance", "create", staging, "--listen", f"127.0.0.1:{second}").returncode == 0
        )
        missing = named("daemon", "start", "--instance", "nowhere")
        assert "define it with muructl instance create nowhere" in missing.stderr, missing.stderr

        rows = {
            row["name"]: row["state"]
            for row in json.loads(named("instance", "list", "--json").stdout)["instances"]
            if row.get("name") in (dev, staging)
        }
        assert rows == {dev: "stopped", staging: "stopped"}, rows

        for name in (dev, staging):
            assert named("daemon", "start", "--instance", name).returncode == 0
            started.append(name)

        status = named("daemon", "status", "--instance", dev).stdout
        assert status.splitlines()[0].endswith(f", instance {dev}"), status
        rows = {row["name"]: row for row in listed(label)["instances"]}
        assert {name: row["state"] for name, row in rows.items()} == {
            dev: "running",
            staging: "running",
        }
        assert rows[dev]["listeners"][0]["bound"] == f"127.0.0.1:{first}"
        title = subprocess.run(
            ["ps", "-o", "command=", "-p", str(rows[dev]["pid"])],
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()
        assert title == f"muru daemon {dev}", title
        refused = named("instance", "remove", dev, "--yes")
        assert f"instance {dev} is running" in refused.stderr, refused.stderr
    finally:
        for name in started:
            named("daemon", "stop", "--instance", name, "--shutdown")

        for name in (dev, staging):
            named("instance", "remove", name, "--state", "--yes")

    assert listed(label)["instances"] == []


def test_a_checkout_reaches_the_instance_its_local_file_names(tmp_path: Path) -> None:
    # Issue 811: the instance a command reaches comes from --instance,
    # MURU_INSTANCE or muru.local.toml, never a machine-wide setting; a
    # development run of the checkout still answers what it can first.

    label = f"test-{uuid.uuid4().hex[:8]}"
    name = f"dev-{uuid.uuid4().hex[:6]}"
    home = tmp_path / "home"
    home.mkdir()
    env = {
        key: value
        for key, value in os.environ.items()
        if key
        not in (
            "MURU_RUN_DIR",
            "MURU_STATE_DIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        )
    }
    env.update({"HOME": str(home), "MURU_INSTANCE_LABEL": label})
    checkout = tmp_path / "checkout"
    shutil.copytree(SHOP, checkout)
    path = support.muructl_path()
    assert path is not None

    def here(
        *arguments: str, cwd: Path = checkout, **extra: str
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(path), *arguments],
            capture_output=True,
            text=True,
            env={**env, **extra},
            cwd=cwd,
            timeout=120,
        )

    port = support.free_port()
    assert here("instance", "create", name, "--listen", f"127.0.0.1:{port}").returncode == 0
    (checkout / "muru.local.toml").write_text(f"instance = '{name}'\n")

    # Defined, not running.

    stopped = here("status")
    assert (
        f"instance {name} is not running; start it with muructl daemon start --instance {name}"
        in stopped.stderr
    ), stopped.stderr

    try:
        # The daemon's label comes from --label alone.

        started = here(
            "daemon", "start", "--instance", name, "--label", label, MURU_INSTANCE_LABEL=""
        )
        assert started.returncode == 0, started.stderr

        status = here("status")
        assert status.stdout.splitlines()[0].endswith(f", instance {name}"), status.stdout

        deployed = here(
            "deploy", "shop", str(checkout), "--python", sys.executable, "--json", cwd=tmp_path
        )
        assert deployed.returncode == 0, deployed.stderr
        assert f"deploying shop to instance {name} (from muru.local.toml)" in deployed.stderr
        assert json.loads(deployed.stdout)["instance"] == name

        # A name nothing defines, and two ways at once, are refused.

        unknown = here("status", MURU_INSTANCE="nowhere")
        assert (
            "nowhere is not the name, tag, ID, label or application of a running instance; "
            "muructl instance list shows" in unknown.stderr
        ), unknown.stderr
        both = here("status", "--socket", "/tmp/x.sock", "--instance", name)
        assert "--socket and --instance both name what to reach" in both.stderr
        both = here("status", MURU_SOCKET="/tmp/x.sock", MURU_INSTANCE=name)
        assert "MURU_SOCKET and MURU_INSTANCE are both set" in both.stderr

        # A development run of the checkout answers first, saying so.

        (checkout / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
        run = support.start(
            "app:application",
            workers=1,
            threads=1,
            chdir=checkout,
            env={"MURU_INSTANCE_LABEL": label},
        )

        try:
            # The run is under the session's run directory, as a shell
            # running both would have it.

            session = {"MURU_RUN_DIR": os.environ["MURU_RUN_DIR"]}
            status = here("status", **session)
            assert status.stdout.startswith("run      pid "), status.stdout
            assert f"muru.local.toml names instance {name}, pass --instance {name}" in status.stderr
            assert here("status", "--instance", name, **session).stdout.startswith("daemon ")

            # Issue 868: what the list shows selects the run, as do the
            # start of its ID and MURU_INSTANCE; a label two instances
            # share is refused with both.

            rows = {row["kind"]: row for row in listed(label)["instances"]}
            assert rows["run"]["selector"] == f"run:{run.port}", rows["run"]
            assert rows["daemon"]["selector"] == name, rows["daemon"]

            for wanted in (f"run:{run.port}", rows["run"]["id"][:6]):
                reached = here("status", "--instance", wanted)
                assert reached.stdout.startswith("run      pid "), (wanted, reached.stderr)

            chosen = here("status", MURU_INSTANCE=f"run:{run.port}", cwd=tmp_path)
            assert chosen.stdout.startswith("run      pid "), chosen.stderr

            # A command run inside the run, with its MURU_INSTANCE_ID,
            # reaches the run from anywhere, unless only a daemon answers
            # it, which finds the daemon as it would elsewhere.

            inside = {"MURU_INSTANCE_ID": rows["run"]["id"]}
            reached = here("status", cwd=tmp_path, **inside)
            assert reached.stdout.startswith("run      pid "), reached.stderr
            releases = here("release", "list", "shop", **inside)
            assert releases.returncode == 0, releases.stderr

            shared = here("status", "--instance", label)
            assert f"{label} is the label of 2 running instances" in shared.stderr, shared.stderr
            listed_run = re.search(rf"\n  run:{run.port} +the development run", shared.stderr)
            assert listed_run, shared.stderr

            refused = here("daemon", "start", "--instance", f"run:{run.port}")
            assert "it takes a name, not the tag" in refused.stderr, refused.stderr
        finally:
            run.stop()
    finally:
        here("daemon", "stop", "--instance", name, "--shutdown")
        here("instance", "remove", name, "--state", "--yes")

    assert listed(label)["instances"] == []
