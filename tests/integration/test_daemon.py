"""The daemon (design specification sections 9.1, 9.5, 10 and 13.3).

A daemon runs with its own state, runtime and log directories; releases are
deployed from source directories with the blue/green strategy, traffic
moves when the new release is ready and the previous one drains, rollback
restores the previous release, and every state change is an event.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import tomllib
from pathlib import Path

from support import instances
from support import server as support
from support.daemon import FACTORY, SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def test_status_before_any_release(daemon: Daemon) -> None:
    status = daemon.muructl("status").stdout

    assert f"http://127.0.0.1:{daemon.port}" in status
    assert "apps     0 (0 releases)" in status
    assert daemon.get("/").status == 503

    # Issue 805: the daemon is in the user's instance registry under the
    # ID kept in its state directory, which its status reports.

    answer = json.loads(daemon.muructl("status", "--json").stdout)
    instance_id = (daemon.base / "state" / "instance-id").read_text().strip()
    assert answer["instance_id"] == instance_id and answer["hostname"]
    record = json.loads((support.user_registry() / instance_id / "instance.json").read_text())
    assert record["kind"] == "daemon" and "name" not in record
    assert record["pid"] == daemon.process.pid
    assert record["state_dir"] == str(daemon.base / "state")
    assert record["admin"]["path"] == str(daemon.run_dir / "admin.sock")
    assert record["listeners"][0]["bound"] == f"127.0.0.1:{daemon.port}"


def test_deploy_serves_and_records_the_release(daemon: Daemon) -> None:
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    response = daemon.get("/")
    assert response.status == 200
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]
    assert "shop" in daemon.muructl("app", "list").stdout

    # A worker's own events, and the host's load (issue 753), come from
    # the host, which the daemon hears only once it has attached to it, so
    # one may arrive after the release's; the release's events are in
    # order among themselves. release.state_changed notes how the runtime
    # library was found, a fetch from PyPI among them, which depends on
    # the machine.

    kinds = daemon.events()
    releases = [
        kind
        for kind in kinds
        if not kind.startswith("worker.")
        and kind not in ("release.state_changed", "release.loaded")
    ]
    assert releases[-3:] == ["release.created", "release.ready", "traffic.switched"]
    assert "release.loaded" in kinds


def test_second_deploy_moves_traffic_without_failed_requests(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid_before = json.loads(daemon.get("/info").body)["pid"]

    # Requests keep flowing while the deploy happens, on kept-alive
    # connections as real clients use (and so the test does not run out
    # of ephemeral ports).

    with Load(daemon.port) as load:
        assert (
            daemon.deploy("shop", SHOP, "--processes", "2")
            == "muru: release shop@r2 is ready and serving"
        )
        time.sleep(0.5)

    load.check("the deploy")
    assert json.loads(daemon.get("/info").body)["pid"] != pid_before
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "retained", "serving": ""},
    ]


def test_rollback_restores_the_previous_release(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--release", "second")

    assert (
        daemon.muructl("rollback", "shop").stdout.strip() == "muru: rolled back to shop@r1 (ready)"
    )
    assert daemon.get("/").status == 200
    assert daemon.releases("shop") == [
        {"release": "shop@second", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]
    assert "APP                  CURRENT      PREVIOUS" in daemon.muructl("app", "list").stdout
    assert "shop                 r1           second" in daemon.muructl("app", "list").stdout


def test_rollback_without_a_previous_release_is_refused(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl("rollback", "shop", check=False)

    assert completed.returncode != 0
    assert "no previous release" in completed.stderr


def test_drain_takes_the_release_out_of_traffic(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)

    assert daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
    assert daemon.get("/").status == 503
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "stopped", "serving": ""}]


def test_failed_deploy_keeps_the_previous_release_serving(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    completed = daemon.muructl(
        "deploy",
        "shop",
        str(SHOP),
        "--python",
        sys.executable,
        "--entrypoint",
        "shop.wsgi:missing",
        check=False,
    )

    assert completed.returncode != 0
    assert "host exited" in completed.stderr or "failed" in completed.stderr
    assert daemon.get("/").status == 200
    assert daemon.releases("shop")[0] == {"release": "shop@r2", "state": "failed", "serving": ""}
    assert "release.failed" in daemon.events()


def test_settings_come_from_the_table_and_flags(daemon: Daemon) -> None:
    # The shop project's pyproject.toml sets processes and threads; the
    # factory project's muru.toml names a factory entrypoint.

    daemon.deploy("shop", SHOP)
    assert "2 (2x3)" in daemon.muructl("release", "list", "shop").stdout

    daemon.deploy("factory", FACTORY, "--threads", "2", "--hold")
    listing = daemon.muructl("release", "list", "factory").stdout
    assert "1x2" in listing
    assert daemon.releases("factory")[0]["state"] == "ready"


def test_release_wait_and_no_wait(daemon: Daemon) -> None:
    # A release is ready only once the edge has it, so the first request
    # after the wait reaches the new release. Several deploys give the
    # window between the workers being ready and the edge being told a
    # chance to show.

    for number in range(1, 5):
        line = daemon.deploy("shop", SHOP, "--no-wait", "--processes", "1")
        assert line.startswith(f"muru: release shop@r{number} recorded")

        waited = daemon.muructl("release", "wait", f"shop@r{number}", "--timeout", "60")
        assert waited.stdout.strip() == f"muru: release shop@r{number} is ready"

        response = daemon.get("/info")
        assert response.status == 200
        assert json.loads(response.body)["release"] == f"r{number}"


def test_a_dead_host_is_started_again(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    pid = json.loads(daemon.get("/info").body)["pid"]

    # The host writes to its own log; its pid is in the daemon's record.

    record = (daemon.run_dir / "apps" / "shop" / "r1" / "host.toml").read_text()
    host_pid = int(
        next(line.split("=")[1] for line in record.splitlines() if line.startswith("pid"))
    )
    os.kill(host_pid, signal.SIGKILL)

    deadline = time.monotonic() + 30

    while time.monotonic() < deadline:
        if "release.restarting" in daemon.events() and daemon.get("/").status == 200:
            break
        time.sleep(0.2)

    assert daemon.get("/").status == 200
    assert json.loads(daemon.get("/info").body)["pid"] != pid
    assert daemon.releases("shop") == [{"release": "shop@r1", "state": "ready", "serving": "yes"}]


def test_current_release_is_started_again_after_a_shutdown(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)

    try:
        first.deploy("shop", SHOP)
        assert first.get("/").status == 200
    finally:
        first.stop(shutdown=True)

    # Nothing is left for the next daemon to adopt: no host or edge record,
    # no rate limit buckets, and no empty release directories.

    assert not (first.run_dir / "apps").exists()
    assert not (first.run_dir / "edge.toml").exists()
    assert not (first.run_dir / "edge" / "rate-buckets-v1").exists()

    second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and second.get("/").status != 200:
            time.sleep(0.2)
        assert second.get("/").status == 200
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
    finally:
        second.stop()


def test_a_second_daemon_on_a_running_daemons_directories_is_refused(daemon: Daemon) -> None:
    # Issue 804: a daemon owns its state and run directories, so a second
    # one on either is refused before it touches the first's socket.

    muructl = support.muructl_path()
    assert muructl is not None
    socket_path = daemon.run_dir / "admin.sock"
    inode = socket_path.stat().st_ino
    other_state = support.short_dir("muru-st-")

    for state in (daemon.base / "state", other_state):
        refused = subprocess.run(
            [
                str(muructl),
                "daemon",
                "run",
                "--state-dir",
                str(state),
                "--run-dir",
                str(daemon.run_dir),
                "--log-dir",
                str(daemon.base / "logs"),
                "--cache-dir",
                str(daemon.base / "cache"),
                "--config",
                str(daemon.base / "muru.toml"),
                "--listen",
                f"127.0.0.1:{support.free_port()}",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        directory = "state directory" if state == daemon.base / "state" else "run directory"
        assert refused.returncode != 0
        assert (
            f"another daemon (pid {daemon.process.pid}) is using the {directory}" in refused.stderr
        ), refused.stderr
        assert f"its admin socket is {socket_path}" in refused.stderr, refused.stderr

    # The first keeps its socket and answers on it.

    assert socket_path.stat().st_ino == inode
    assert f"daemon   pid {daemon.process.pid} " in daemon.muructl("daemon", "status").stdout


def edge_pid_of(daemon: Daemon) -> int:
    """The edge process pid from the status output."""

    status = daemon.muructl("status").stdout
    return int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))


def test_detached_daemon_leaves_traffic_flowing_and_is_readopted(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        # One worker, so that the pid each answer carries is the one worker's.

        first.deploy("shop", SHOP, "--processes", "1")
        pid = json.loads(first.get("/info").body)["pid"]
        edge_pid = edge_pid_of(first)

        # The daemon goes; the edge and the host do not.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert first.get("/").status == 200
        assert json.loads(first.get("/info").body)["pid"] == pid

        # The next daemon takes them over, with no gap and no new processes.

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert edge_pid_of(second) == edge_pid
        assert json.loads(second.get("/info").body)["pid"] == pid
        assert second.releases("shop") == [
            {"release": "shop@r1", "state": "ready", "serving": "yes"}
        ]
        assert "release.adopted" in second.events()

        # And deploys keep working through the adopted edge.

        assert second.deploy("shop", SHOP) == "muru: release shop@r2 is ready and serving"
        assert json.loads(second.get("/info").body)["pid"] != pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        ours = instances.pids_of(first.run_dir / "admin.sock") - {second.process.pid}
        second.stop(shutdown=True)

    # Nothing is left: nothing that names the run directory, and none of
    # the edge, hosts and relays it supervised, which carry titles.

    listing = subprocess.run(
        ["ps", "-ax", "-o", "command="], capture_output=True, text=True, check=False
    ).stdout
    assert not any(first.run_dir.name in line for line in listing.splitlines())
    assert ours and not support.still_running(ours), ours


def test_a_relative_interpreter_path_is_resolved_where_the_command_runs(
    daemon: Daemon,
) -> None:
    # The daemon starts the host elsewhere, so a path relative to the
    # shell is made absolute by the client; a bare name stays a command.

    relative = os.path.relpath(sys.executable)
    assert not os.path.isabs(relative) and os.sep in relative
    deployed = daemon.muructl("deploy", "shop", str(SHOP), "--python", relative)
    assert deployed.stdout.strip() == "muru: release shop@r1 is ready and serving"
    inspected = daemon.muructl("debug", "inspect", "shop").stdout
    assert f"at {os.path.abspath(relative)}," in inspected, inspected


def test_daemon_stop_gives_up_on_a_silent_socket(tmp_path: Path) -> None:
    """A socket that accepts connections and never answers cannot hold
    daemon stop past its timeout: every exchange is bounded."""

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built")
    run_dir = support.short_dir("muru-run-")
    path = run_dir / "admin.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen(1)

    try:
        started = time.monotonic()
        stopped = subprocess.run(
            [str(muructl), "daemon", "stop", "--timeout", "2s", "--socket", str(path)],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        elapsed = time.monotonic() - started
    finally:
        listener.close()

    assert stopped.returncode != 0
    assert "did not answer within 2 seconds" in stopped.stderr, stopped.stderr
    assert elapsed < 20, elapsed


def test_a_daemon_that_exits_at_start_is_reported_with_its_log(tmp_path: Path) -> None:
    """A daemon that cannot bind its listener exits at once, and daemon
    start says so with what it wrote, rather than waiting for it."""

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built")
    run_dir = support.short_dir("muru-run-")
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()

    try:
        started = subprocess.run(
            [
                str(muructl),
                "daemon",
                "start",
                "--listen",
                f"127.0.0.1:{listener.getsockname()[1]}",
                "--config",
                str(tmp_path / "muru.toml"),
                "--state-dir",
                str(tmp_path / "state"),
                "--run-dir",
                str(run_dir),
                "--log-dir",
                str(tmp_path / "logs"),
                "--cache-dir",
                str(tmp_path / "cache"),
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
    finally:
        listener.close()

    assert started.returncode != 0
    assert "the daemon exited before it answered (exit status: 1)" in started.stderr
    assert "cannot listen on" in started.stderr, started.stderr

    # Issue 837: the program holding the address is named, as lsof or ss
    # say of it, though it is not an instance of Muru's.

    assert f"(pid {os.getpid()}); choose another listener, or stop it" in started.stderr, (
        started.stderr
    )

    # Every line of daemon.log, the error it exited with among them,
    # starts with the full UTC time.

    lines = (tmp_path / "logs" / "daemon.log").read_text().splitlines()
    assert lines and all(re.match(r"\d{4}-\d\d-\d\dT[\d:.]+Z muru: ", line) for line in lines), (
        lines
    )
    assert "muru: error: cannot listen on" in lines[-1]


def test_background_daemon_start_status_stop(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        support.unavailable("muructl or the extension is not built")
    run_dir = support.short_dir("muru-run-")
    port = support.free_port()
    dashboard = support.free_port()
    location = [
        "--config",
        str(tmp_path / "muru.toml"),
        "--state-dir",
        str(tmp_path / "state"),
        "--run-dir",
        str(run_dir),
        "--log-dir",
        str(tmp_path / "logs"),
        "--cache-dir",
        str(tmp_path / "cache"),
    ]

    # Issue 851: every start says where it listens and serves its
    # dashboard, so nothing depends on a default port being free.

    serving = ["--listen", f"127.0.0.1:{port}", "--dashboard", f"127.0.0.1:{dashboard}"]

    started = subprocess.run(
        [str(muructl), "daemon", "start", *serving, *location],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )

    try:
        assert started.returncode == 0, started.stdout + started.stderr
        assert "daemon started" in started.stdout
        again = subprocess.run(
            [str(muructl), "up", *serving, *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert "already running" in again.stdout
        pid = again.stdout.split("(pid ")[1].split(")")[0]
        assert (run_dir / "daemon.pid").read_text() == pid

        # A daemon that detaches is gone too, so its pid file goes with it.

        detached = subprocess.run(
            [str(muructl), "daemon", "stop", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert "detached" in detached.stdout, detached.stdout + detached.stderr
        assert not (run_dir / "daemon.pid").exists()
        restarted = subprocess.run(
            [str(muructl), "up", *serving, *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert restarted.returncode == 0, restarted.stdout + restarted.stderr
        status = subprocess.run(
            [str(muructl), "daemon", "status", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert f"http://127.0.0.1:{port}" in status.stdout
        assert f"127.0.0.1:{dashboard}" in status.stdout, status.stdout
        assert f"logs     {tmp_path / 'logs'}\n" in status.stdout
        assert "cache    " in status.stdout and "config   " in status.stdout
    finally:
        stopped = subprocess.run(
            [str(muructl), "daemon", "stop", "--shutdown", *location],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    assert "daemon stopped" in stopped.stdout, stopped.stdout + stopped.stderr
    assert "releases drained and stopped" in stopped.stdout
    assert (tmp_path / "logs" / "daemon.log").is_file()
    assert not (run_dir / "daemon.pid").exists()

    # With the daemon stopped, its own log is read from the file, and its
    # status says where its files would be.

    shown = subprocess.run(
        [str(muructl), "logs", "--daemon", *location],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert shown.returncode == 0 and shown.stdout, shown.stderr
    status = subprocess.run(
        [str(muructl), "daemon", "status", *location],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert status.returncode == 3, status.stdout + status.stderr
    assert status.stdout.startswith("daemon   not running\n")
    assert f"logs     {tmp_path / 'logs'}\n" in status.stdout
    assert f"socket   {run_dir / 'admin.sock'}\n" in status.stdout


def test_status_names_the_edge_process(daemon: Daemon) -> None:
    status = daemon.muructl("status").stdout
    edge_pid = int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))

    assert edge_pid != daemon.process.pid
    assert os.path.exists(f"/proc/{edge_pid}") or os.kill(edge_pid, 0) is None


def test_a_dead_edge_is_started_again_without_refusing_connections(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    status = daemon.muructl("status").stdout
    edge_pid = int(next(line.split()[2] for line in status.splitlines() if line.startswith("edge")))

    # The daemon holds the listener too, so a connection made while no
    # edge runs waits in the backlog and is served by the new edge.

    os.kill(edge_pid, signal.SIGKILL)
    time.sleep(0.2)
    assert daemon.get("/").status == 200

    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and "edge.restarted" not in daemon.events():
        time.sleep(0.1)
    assert "edge.restarted" in daemon.events()
    new_status = daemon.muructl("status").stdout
    new_pid = int(
        next(line.split()[2] for line in new_status.splitlines() if line.startswith("edge"))
    )
    assert new_pid != edge_pid


def test_a_new_edge_replaces_a_worker_that_cannot_answer_it(daemon: Daemon) -> None:
    """An edge started again has no connections to keep, so it connects to
    every worker afresh. One that accepts the connection and never answers
    HELLO (a stopped process) is left out of its pool, so the snapshot still
    lands, and the daemon names it and replaces it."""

    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    daemon.wait_for_routing("shop")
    frozen = json.loads(daemon.get("/").body)["pid"]
    os.kill(frozen, signal.SIGSTOP)

    try:
        os.kill(edge_pid_of(daemon), signal.SIGKILL)
        deadline = time.monotonic() + 30

        while time.monotonic() < deadline and "worker.unreachable" not in daemon.events():
            time.sleep(0.2)

        log = daemon.muructl("events").stdout
        assert "worker.unreachable" in daemon.events(), log
        assert "did not answer HELLO within 5 s" in log and "it is being replaced" in log, log

        while time.monotonic() < deadline and "worker.restarted" not in daemon.events():
            time.sleep(0.2)

        assert "worker.restarted" in daemon.events(), daemon.muructl("events").stdout
    finally:
        try:
            os.kill(frozen, signal.SIGCONT)
        except ProcessLookupError:
            pass

    assert json.loads(daemon.get("/").body)["pid"] != frozen


def test_a_deploy_pushes_one_snapshot(daemon: Daemon) -> None:
    """The host's first announcement on its control connection, taken just
    after the deploy answers, repeats the sockets the daemon already routes
    by, so it is acknowledged without another push: an apply the edge did
    not need, and one that replaced a worker busy at that moment."""

    before = daemon.snapshot_version()
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    daemon.wait_for_routing("shop")
    assert daemon.snapshot_version() == before + 1

    # A worker the host replaces is a change the edge must hear about.

    pid = json.loads(daemon.get("/").body)["pid"]
    daemon.muructl("restart", "shop", "--pid", str(pid))
    daemon.wait_for_routing("shop")
    assert daemon.snapshot_version() == before + 2


def test_superseded_release_is_retained_and_rollback_is_instant(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    first_pid = json.loads(daemon.get("/info").body)["pid"]
    daemon.deploy("shop", SHOP, "--processes", "1")

    # The first release is warm, with its workers untouched.

    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "ready", "serving": "yes"},
        {"release": "shop@r1", "state": "retained", "serving": ""},
    ]
    assert "warm until" in daemon.muructl("release", "list", "shop").stdout

    assert (
        daemon.muructl("rollback", "shop").stdout.strip() == "muru: rolled back to shop@r1 (ready)"
    )
    assert json.loads(daemon.get("/info").body)["pid"] == first_pid
    assert daemon.releases("shop") == [
        {"release": "shop@r2", "state": "retained", "serving": ""},
        {"release": "shop@r1", "state": "ready", "serving": "yes"},
    ]


def test_retention_can_be_switched_off_or_shortened(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--retain", "0")
    daemon.wait_for_state("shop@r1", "stopped")
    assert daemon.releases("shop")[1] == {"release": "shop@r1", "state": "stopped", "serving": ""}

    daemon.deploy("shop", SHOP, "--retain", "1s")
    assert daemon.releases("shop")[1] == {"release": "shop@r2", "state": "retained", "serving": ""}
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and daemon.releases("shop")[1]["state"] != "stopped":
        time.sleep(0.2)
    assert daemon.releases("shop")[1] == {"release": "shop@r2", "state": "stopped", "serving": ""}
    assert "the retention window passed" in daemon.muructl("release", "list", "shop").stdout


def test_only_one_release_is_retained_per_application(daemon: Daemon) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    daemon.wait_for_state("shop@r1", "stopped")

    assert [row["state"] for row in daemon.releases("shop")] == ["ready", "retained", "stopped"]
    assert "superseded again" in daemon.muructl("release", "list", "shop").stdout


def test_a_relative_or_deep_run_directory_is_refused_at_start(tmp_path: Path) -> None:
    """A worker's socket path must fit the platform's limit and be bound
    where the daemon says, so a run directory that is relative or leaves
    no room is refused by muructl daemon start before the daemon exists,
    rather than after a 30 s wait for a daemon that never answers."""

    muructl = support.muructl_path()

    if muructl is None:
        import pytest

        pytest.skip("muructl is not built")

    def start(run_dir: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                str(muructl),
                "daemon",
                "start",
                "--run-dir",
                run_dir,
                "--state-dir",
                str(tmp_path / "state"),
                "--log-dir",
                str(tmp_path / "logs"),
                "--config",
                str(tmp_path / "muru.toml"),
                "--cache-dir",
                str(tmp_path / "cache"),
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )

    relative = start("muru/run")
    assert relative.returncode != 0
    assert "is a relative path" in relative.stderr, relative.stderr

    deep = start("/tmp/" + "x" * 90)
    assert deep.returncode != 0
    assert "leaves" in deep.stderr and "bytes" in deep.stderr, deep.stderr
    assert "shorter path" in deep.stderr


def test_the_daemons_file_is_named_only_where_it_is_read(daemon: Daemon, tmp_path: Path) -> None:
    # Section 13.8: a command that only talks to a running daemon reads
    # none of the daemon's own files, and takes no flag that names one.

    for flag in ["--config", "--state-dir", "--log-dir", "--cache-dir"]:
        refused = daemon.muructl("status", flag, str(tmp_path), check=False)
        assert refused.returncode != 0
        assert f"unexpected argument '{flag}'" in refused.stderr, refused.stderr

    # The variable that names the file says whose file it is, and its
    # earlier name is refused so a daemon never passes its file over.

    muructl = support.muructl_path()
    assert muructl is not None

    # Directories of its own: the fixture's daemon owns its state
    # directory, and another daemon on it would be refused first.

    where = {
        "MURU_STATE_DIR": str(tmp_path / "other-state"),
        "MURU_LOG_DIR": str(tmp_path / "other-logs"),
        "MURU_RUN_DIR": str(support.short_dir("muru-rt-")),
        "MURU_CACHE_DIR": str(tmp_path / "cache"),
    }
    config = tmp_path / "node.toml"
    config.write_text("[[listener]]\naddress = 'not an address'\n")

    def run(**env: str) -> str:
        completed = subprocess.run(
            [str(muructl), "daemon", "run"],
            capture_output=True,
            text=True,
            check=False,
            env={**os.environ, **where, **env},
            timeout=60,
        )
        assert completed.returncode != 0
        return completed.stdout + completed.stderr

    old = run(MURU_CONFIG=str(config))
    assert "MURU_CONFIG is set, and the daemon's configuration file is now named by" in old, old
    assert "MURU_DAEMON_CONFIG" in old, old
    assert "not an address" in run(MURU_DAEMON_CONFIG=str(config))


def test_a_deploy_takes_nothing_from_the_release_before(daemon: Daemon) -> None:
    # Section 3.3, issue 861: a flag applies to its own release alone. What
    # neither the next deploy's flags nor the source's table give is the
    # default, a timeout, a memory key and an environment variable alike.

    daemon.deploy(
        "shop", SHOP, "--timeout", "soft=3s", "--memory", "soft_limit=800MB", "--env", "DEBUG=1"
    )
    daemon.deploy("shop", SHOP)
    shown = daemon.muructl("config", "show", "shop", "--effective").stdout
    assert "#   shop@r2: timeouts.soft_timeout = off (default)" in shown
    assert "#   shop@r2: memory.soft_limit = off (default)" in shown
    assert "#   shop@r2: threads = 3 ([tool.muru])" in shown
    exported = tomllib.loads(daemon.muructl("config", "show", "shop").stdout)
    assert "env" not in exported["app"]["shop"]["release"]["r2"]


def test_a_deploy_records_where_its_code_came_from(daemon: Daemon, tmp_path: Path) -> None:
    # Section 9.14, issue 866: a deploy from a git checkout records its
    # commit and branch, and whether it has changes, beside the metadata
    # its flags give; every view shows it.

    source = tmp_path / "shop"
    shutil.copytree(SHOP, source, ignore=shutil.ignore_patterns("__pycache__"))

    identity = ["-c", "user.name=Muru", "-c", "user.email=m@example.com"]

    def git(*arguments: str) -> str:
        completed = subprocess.run(
            ["git", "-C", str(source), *identity, "-c", "commit.gpgsign=false", *arguments],
            capture_output=True,
            text=True,
            check=True,
        )
        return completed.stdout.strip()

    git("init", "-q", "-b", "main")
    git("add", ".")
    git("commit", "-q", "-m", "shop")
    commit = git("rev-parse", "HEAD")

    line = daemon.deploy(
        "shop", source, "--metadata", "url=https://git.example.com/shop/pull/42", git_metadata=True
    )
    assert line.endswith(f", commit {commit[:12]} of main"), line
    shown = json.loads(daemon.muructl("release", "show", "shop@r1", "--json").stdout)
    assert shown["metadata"] == {
        "commit": commit,
        "ref": "refs/heads/main",
        "url": "https://git.example.com/shop/pull/42",
    }
    assert (
        f"from {source.resolve()}, commit {commit[:12]} of main" in daemon.muructl("events").stdout
    )

    # A change makes the next release dirty, which the list marks; a key
    # given empty leaves out what git filled.

    (source / "notes.txt").write_text("draft\n")
    daemon.deploy("shop", source, "--metadata", "ref=", git_metadata=True)
    listed = daemon.muructl("release", "list", "shop").stdout
    assert re.search(rf"shop@r2 .* {commit[:12]}\+ ", listed), listed
    assert re.search(rf"shop@r1 .* {commit[:12]} ", listed), listed
    second = json.loads(daemon.muructl("release", "show", "shop@r2", "--json").stdout)
    assert second["metadata"] == {"commit": commit, "dirty": "true"}

    # The export writes it, and a limit refuses a deploy before it is sent.

    exported = tomllib.loads(daemon.muructl("config", "show", "shop").stdout)
    assert exported["app"]["shop"]["release"]["r2"]["metadata"]["dirty"] == "true"
    refused = daemon.muructl(
        "deploy", "shop", str(source), "--metadata", "url=ftp://example.com", check=False
    )
    assert refused.returncode != 0
    assert "must be an absolute http or https URL" in refused.stderr
