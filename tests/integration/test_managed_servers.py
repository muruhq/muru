"""Managed HTTP server releases (design specification section 9.9).

A release with kind = 'http' runs an ordinary HTTP server that the daemon
starts, probes, supervises and stops, and that the edge routes to like an
upstream, so releases, traffic policies, drains and rollbacks apply to it
as to native workers. The intended use is a cutover: the existing server
runs unchanged under Muru first, then traffic is canaried to native workers.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
import tomllib
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture

LEGACY = support.ROOT / "tests" / "fixtures" / "projects" / "legacy"
SERVER = [sys.executable, "server.py", "--bind", "{bind}"]


def wait_for(condition: Callable[[], bool], timeout: float = 15) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if condition():
            return

        time.sleep(0.1)

    assert condition()


def deploy_server(daemon: Daemon, app: str, *flags: str, command: list[str] | None = None) -> str:
    """Deploy the legacy server as a generic managed release and return the line printed."""

    arguments = ["deploy", app, str(LEGACY), "--server", "generic", "--stop-signal", "TERM"]
    completed = daemon.muructl(*arguments, *flags, "--", *(command or SERVER))
    return completed.stdout.strip()


def report(daemon: Daemon, path: str = "/", host: str | None = None, **headers: str) -> dict:
    response = daemon.request(path, host=host, headers=headers)
    assert response.status == 200, (response.status, response.body)
    return json.loads(response.body)


def instances(daemon: Daemon, release: str) -> list[dict]:
    shown = json.loads(daemon.muructl("release", "show", release, "--json").stdout)
    return shown["server_instances"]


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False

    return True


def test_a_generic_server_is_started_probed_and_served(daemon: Daemon) -> None:
    line = deploy_server(daemon, "legacy", "--readiness", "/healthz")
    assert line == "muru: release legacy@r1 is ready and serving"

    # The request reaches the server re-serialised, with the forwarding
    # headers of section 4.13, in the directory and environment given.

    seen = report(daemon, "/catalogue?page=2")
    assert seen["server"] == "legacy"
    assert seen["path"] == "/catalogue?page=2"
    assert seen["headers"]["x-forwarded-for"] == "127.0.0.1"
    assert seen["headers"]["x-forwarded-proto"] == "http"
    assert "x-request-id" in seen["headers"]
    assert seen["cwd"] == str(LEGACY)
    assert seen["env"]["MURU_HTTP_BIND"].startswith("unix:")
    assert seen["env"]["MURU_SERVER_INDEX"] == "0"
    assert seen["env"]["PORT"] is None

    # The release is listed by its server, the server's output is in the
    # release's log, and the health of its instance is a gauge.

    rows = daemon.muructl("release", "list").stdout.splitlines()
    assert any(row.split()[3:6] == ["1", "(1", "generic)"] for row in rows[1:]), rows
    wait_for(lambda: "legacy listening on unix:" in daemon.muructl("logs", "legacy").stdout)
    metrics = daemon.muructl("metrics").stdout
    assert 'muru_upstream_health{upstream="legacy@r1",address="unix:' in metrics
    assert "} 1\n" in metrics.split('muru_upstream_health{upstream="legacy@r1"')[1]
    assert 'muru_workers_ready{app="legacy",release="r1",group="http"} 1' in metrics

    # The server's X-Muru-Redirect is honoured, as an application's is.

    assert report(daemon, "/redirect")["path"] == "/info"

    # The access log names the release and its instance.

    wait_for(lambda: "/catalogue" in daemon.muructl("logs", "--access").stdout)
    record = next(
        json.loads(line)
        for line in daemon.muructl("logs", "--access", "--json").stdout.splitlines()
        if '"/catalogue?page=2"' in line
    )
    assert (record["release"], record["group"]) == ("r1", "http")
    assert record["mount"].startswith("unix:")


def test_instances_bind_ports_and_their_count_is_fixed(daemon: Daemon) -> None:
    deploy_server(daemon, "legacy", "--bind", "port", "--instances", "2")
    reports = [report(daemon, f"/n/{n}") for n in range(12)]
    pids = {seen["pid"] for seen in reports}
    assert len(pids) == 2, reports
    first = reports[0]
    port = first["env"]["MURU_HTTP_BIND"].rsplit(":", 1)[1]
    assert first["env"]["MURU_HTTP_BIND"].startswith("127.0.0.1:")
    assert first["env"]["PORT"] == port
    assert {seen["env"]["MURU_SERVER_INDEX"] for seen in reports} == {"0", "1"}

    # release show names each instance's process and address.

    shown = instances(daemon, "legacy@r1")
    assert [instance["index"] for instance in shown] == [0, 1]
    assert {instance["pid"] for instance in shown} == pids
    by_pid = {seen["pid"]: seen["env"]["MURU_HTTP_BIND"] for seen in reports}
    assert all(by_pid[instance["pid"]] == instance["address"] for instance in shown)
    assert all(instance["started_unix_ms"] > 0 for instance in shown)
    text = daemon.muructl("release", "show", "legacy@r1").stdout
    assert f"pid {shown[0]['pid']}, {shown[0]['address']}" in text

    refused = daemon.muructl("scale", "legacy@r1", "--processes", "3", check=False)
    assert refused.returncode != 0
    assert "instance count is fixed" in refused.stdout + refused.stderr


def test_readiness_waits_for_the_path_and_gives_up_in_time(daemon: Daemon, tmp_path: Path) -> None:
    flag = tmp_path / "ready"
    started = time.monotonic()
    failed = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "generic",
        "--stop-signal",
        "TERM",
        "--readiness",
        "/healthz",
        "--readiness-timeout",
        "2s",
        "--env",
        f"LEGACY_READY={flag}",
        "--",
        *SERVER,
        check=False,
    )
    assert failed.returncode != 0
    assert "instance 0 was not ready within 2 s: /healthz did not answer 2xx or 3xx" in (
        failed.stdout + failed.stderr
    )
    assert time.monotonic() - started < 15
    assert daemon.releases("legacy") == [{"release": "legacy@r1", "state": "failed", "serving": ""}]

    # The next release becomes ready once the probe passes.

    recorded = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "generic",
        "--stop-signal",
        "TERM",
        "--readiness",
        "/healthz",
        "--env",
        f"LEGACY_READY={flag}",
        "--no-wait",
        "--",
        *SERVER,
    )
    assert "legacy@r2 recorded" in recorded.stdout
    wait_for(lambda: daemon.releases("legacy")[0]["state"] == "warming")
    flag.write_text("go")
    daemon.muructl("release", "wait", "legacy@r2", "--timeout", "30")
    assert daemon.releases("legacy")[0] == {
        "release": "legacy@r2",
        "state": "ready",
        "serving": "yes",
    }
    assert report(daemon)["server"] == "legacy"


def test_a_crashed_instance_is_started_again(daemon: Daemon) -> None:
    deploy_server(daemon, "legacy")
    before = report(daemon)["pid"]
    assert report(daemon, "/crash")["pid"] == before
    wait_for(lambda: not alive(before))
    wait_for(
        lambda: any(
            "server.exited" in line for line in daemon.muructl("events").stdout.splitlines()
        )
    )
    wait_for(
        lambda: any(
            "server.started" in line for line in daemon.muructl("events").stdout.splitlines()
        ),
        20,
    )
    wait_for(lambda: daemon.request("/").status == 200, 20)
    after = report(daemon)["pid"]
    assert after != before
    wait_for(lambda: [instance["pid"] for instance in instances(daemon, "legacy@r1")] == [after])
    events = daemon.muructl("events").stdout
    assert "instance 0 exited with exit status: 3; starting it again in 1s" in events
    assert f"instance 0 started again (pid {after})" in events
    assert daemon.releases("legacy") == [
        {"release": "legacy@r1", "state": "ready", "serving": "yes"}
    ]


def test_a_crash_loop_is_loud_and_ends_as_the_restart_table_says(
    daemon: Daemon, tmp_path: Path
) -> None:
    # The instance serves until the file is there, and from then on
    # exits as soon as it is started.

    broken = tmp_path / "broken"
    script = f"test -e {shlex.quote(str(broken))} && exit 7; exec {shlex.join(SERVER)}"
    deploy_server(
        daemon, "legacy", "--restart", "give_up_after=4", command=["/bin/sh", "-c", script]
    )
    assert [instance["state"] for instance in instances(daemon, "legacy@r1")] == ["running"]
    broken.touch()
    report(daemon, "/crash")

    def events(kind: str) -> list[str]:
        lines = daemon.muructl("events").stdout.splitlines()
        return [line for line in lines if f" {kind} " in line]

    # The third quick exit in a row is a crash loop, and the instance
    # waits in backoff, with when it is started next.

    wait_for(lambda: len(events("server.crash_looping")) == 1, 30)
    assert (
        "instance 0 is crash looping: it has exited or failed to start 3 times"
        in (events("server.crash_looping")[0])
    )
    (waiting,) = instances(daemon, "legacy@r1")
    assert (waiting["state"], waiting["pid"], waiting["quick_exits"]) == ("backoff", 0, 3)
    assert waiting["next_start_unix_ms"] > time.time() * 1000
    shown = daemon.muructl("release", "show", "legacy@r1").stdout
    assert re.search(r"backoff, starting again in \ds after 3 quick exits in a row", shown), shown

    # The fourth is the last the table allows: the instance is left
    # failed, the release stays ready, and the exits are counted.

    wait_for(lambda: len(events("server.gave_up")) == 1, 30)
    assert "(give_up_after = 4)" in events("server.gave_up")[0]
    (failed,) = instances(daemon, "legacy@r1")
    assert (failed["state"], failed["pid"], failed["quick_exits"]) == ("failed", 0, 4)
    shown = daemon.muructl("release", "show", "legacy@r1").stdout
    assert "failed, not started again after 4 quick exits in a row" in shown
    exits = [line for line in events("server.exited") if "exit status" in line]
    assert len(exits) == 4, exits
    assert exits[-1].endswith("exited with exit status: 7; it is not started again")
    metrics = daemon.muructl("metrics").stdout
    assert 'muru_server_exits_total{app="legacy",release="r1",outcome="failed"} 4' in metrics
    assert daemon.releases("legacy") == [
        {"release": "legacy@r1", "state": "ready", "serving": "yes"}
    ]
    time.sleep(2.5)
    assert len(events("server.started")) == 3

    # A restart by hand starts the instance that was given up on.

    broken.unlink()
    daemon.muructl("restart", "legacy", "--all")
    (back,) = instances(daemon, "legacy@r1")
    assert (back["state"], back["quick_exits"]) == ("running", 0)
    assert report(daemon)["pid"] == back["pid"]


def test_a_restart_by_hand_restarts_the_instances_one_at_a_time(daemon: Daemon) -> None:
    deploy_server(daemon, "legacy", "--bind", "port", "--instances", "2", "--readiness", "/healthz")
    before = {instance["index"]: instance["pid"] for instance in instances(daemon, "legacy@r1")}

    # A managed server has no workers to name, and nothing to cancel yet.

    refused = daemon.muructl("restart", "legacy", "--worker", "0", check=False)
    assert refused.returncode != 0
    assert "muructl restart --all restarts its instances, one at a time, and --index N" in (
        refused.stderr
    )
    refused = daemon.muructl("restart", "legacy", "--cancel", check=False)
    assert "no restart of its instances is under way" in refused.stderr

    with Load(daemon.port, clients=4) as load:
        restarted = daemon.muructl("restart", "legacy", "--all").stdout.splitlines()
        time.sleep(0.5)

    load.check("a restart of a managed server's instances", at_least=20)
    assert restarted == [
        "muru: 2 instances of legacy@r1 to be restarted, one at a time",
        "muru: the restart of legacy@r1 has finished",
    ]

    # Every instance is a new process that serves, at the address it had.

    after = instances(daemon, "legacy@r1")
    assert [instance["state"] for instance in after] == ["running", "running"]
    assert all(instance["pid"] not in before.values() for instance in after)
    assert not any(alive(pid) for pid in before.values())
    assert {report(daemon, f"/n/{n}")["pid"] for n in range(12)} == {
        instance["pid"] for instance in after
    }

    # The second instance was not stopped until the first answered again.

    lines = daemon.muructl("events").stdout.splitlines()
    told = [
        line.split(None, 5)[5]
        for line in lines
        if " restart." in line or (" server." in line and "instance" in line)
    ]
    assert told[0] == (
        "restarting 2 instances one at a time, each to answer its readiness probe before the "
        "next is stopped"
    )
    order = [
        re.sub(r" \(.*\)$", "", line) for line in told[1:-1] if not line.startswith("instance 1 a")
    ]
    assert order[:4] == [
        "instance 0 stopped for a restart",
        "instance 0 started again",
        "instance 0 answers and takes requests again",
        "instance 1 stopped for a restart",
    ], told
    assert told[-1] == "every instance has been restarted and answers"
    assert daemon.releases("legacy") == [
        {"release": "legacy@r1", "state": "ready", "serving": "yes"}
    ]

    # One instance by its index is restarted the same way, and the other
    # is left with the process it had; with --force it is killed at once.

    one, other = (instance["pid"] for instance in after)
    restarted = daemon.muructl("restart", "legacy", "--index", "0").stdout.splitlines()
    assert restarted == [
        "muru: instance 0 of legacy@r1 to be restarted",
        "muru: the restart of legacy@r1 has finished",
    ]
    now = [instance["pid"] for instance in instances(daemon, "legacy@r1")]
    assert now[0] not in (0, one) and now[1] == other
    daemon.muructl("restart", "legacy", "--index", "1", "--force")
    assert instances(daemon, "legacy@r1")[1]["pid"] not in (0, other)
    events = daemon.muructl("events").stdout
    assert "instance 1 stopped for a restart (exited with signal: 9 (SIGKILL))" in events
    missing = daemon.muructl("restart", "legacy", "--index", "5", check=False)
    assert "it has no instance 5; it has 2 instances" in missing.stderr


def group_gone(pgid: int) -> bool:
    """Whether nothing is left of a process group; signal 0 sends nothing."""

    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return True

    return False


def test_nothing_is_left_of_an_instance_whose_processes_ignore_the_stop_signal(
    daemon: Daemon,
) -> None:
    # The instance is a shell that ignores TERM and runs the server as a
    # child, which ignores it as well, as a master with a worker would.

    script = f"trap '' TERM; {shlex.quote(sys.executable)} server.py --bind {{bind}} & wait"
    deploy_server(daemon, "legacy", "--timeout", "shutdown=1s", command=["/bin/sh", "-c", script])
    killed = instances(daemon, "legacy@r1")[0]["pid"]
    orphan = report(daemon)["pid"]
    assert orphan != killed
    assert os.getpgid(orphan) == killed

    # The shell is killed, as a master might be, and its child runs on.
    # The instance is not left to start again beside it: the child is
    # stopped, and killed since it ignores that, and then the instance
    # starts.

    os.kill(killed, signal.SIGKILL)
    wait_for(lambda: instances(daemon, "legacy@r1")[0]["pid"] not in (0, killed), 20)
    wait_for(lambda: not alive(orphan))
    wait_for(lambda: group_gone(killed))
    assert "leaving processes behind" in daemon.muructl("events").stdout
    wait_for(lambda: daemon.request("/").status == 200, 20)
    leader = instances(daemon, "legacy@r1")[0]["pid"]
    child = report(daemon)["pid"]
    assert child not in (orphan, leader)
    assert os.getpgid(child) == leader

    # The stop signal does nothing, so the drain ends with the kill, which
    # takes the child with the shell: nothing is left of the group.

    assert "is stopped" in daemon.muructl("drain", "legacy@r1").stdout
    wait_for(lambda: not alive(leader) and not alive(child))
    wait_for(lambda: group_gone(leader))


def test_gunicorn_and_uvicorn_profiles_add_their_own_flags(daemon: Daemon, tmp_path: Path) -> None:
    for module in ("gunicorn", "uvicorn"):
        try:
            __import__(module)
        except ImportError:
            support.unavailable(f"{module} is not installed")

    line = daemon.muructl(
        "deploy",
        "gun",
        str(LEGACY),
        "--server",
        "gunicorn",
        "--",
        sys.executable,
        "-m",
        "gunicorn",
        "--workers",
        "1",
        "legacy:application",
    ).stdout.strip()
    assert line == "muru: release gun@r1 is ready and serving"

    # A host for the first application lets the second one serve.

    first = daemon.base / "gun.toml"
    first.write_text('[host.gun]\nnames = ["gun.test"]\n\n[[route]]\nhost = "gun"\napp = "gun"\n')
    daemon.muructl("apply", "-f", str(first))
    line = daemon.muructl(
        "deploy",
        "uvi",
        str(LEGACY),
        "--server",
        "uvicorn",
        "--bind",
        "port",
        "--",
        sys.executable,
        "-m",
        "uvicorn",
        "asgi:app",
    ).stdout.strip()
    assert line == "muru: release uvi@r1 is ready and serving"

    config = tmp_path / "hosts.toml"
    config.write_text(
        """[host.gun]
names = ["gun.test"]

[host.uvi]
names = ["uvi.test"]

[[route]]
host = "gun"
app = "gun"

[[route]]
host = "uvi"
app = "uvi"
"""
    )
    daemon.muructl("apply", "-f", str(config))
    gun = report(daemon, "/shop", host="gun.test")
    assert (gun["server"], gun["path"], gun["forwarded_for"]) == ("gunicorn", "/shop", "127.0.0.1")
    uvi = report(daemon, "/api", host="uvi.test")
    assert (uvi["server"], uvi["path"]) == ("uvicorn", "/api")
    rows = {
        row.split()[0]: row.split()[3:6]
        for row in daemon.muructl("release", "list").stdout.splitlines()[1:]
    }
    assert rows == {"gun@r1": ["1", "(1", "gunicorn)"], "uvi@r1": ["1", "(1", "uvicorn)"]}

    # Issue 841: the instance is named by the program it runs, though its
    # process is the interpreter, and gunicorn's worker is listed beneath
    # it without --all.

    tree = json.loads(daemon.muructl("status", "--processes", "--app", "gun", "--json").stdout)
    relay = next(child for child in tree["root"]["children"] if child["role"] == "relay")
    server = relay["children"][0]
    assert (server["role"], server["what"]) == ("server", "gun@r1 instance 0: gunicorn"), server
    assert [(child["role"], child["what"]) for child in server.get("children", [])] == [
        ("server-worker", "gunicorn worker")
    ], server


def test_a_cutover_canaries_from_the_server_to_native_workers(
    daemon: Daemon, tmp_path: Path
) -> None:
    deploy_server(daemon, "legacy")
    config = tmp_path / "hosts.toml"
    config.write_text(
        """[host.legacy]
names = ["legacy.test"]
default = true

[[route]]
host = "legacy"
app = "legacy"
"""
    )
    daemon.muructl("apply", "-f", str(config))

    # The native release of the same application is held, then canaried
    # against the server; both answer during the rollout.

    held = daemon.muructl(
        "deploy", "legacy", str(SHOP), "--python", sys.executable, "--hold"
    ).stdout.strip()
    assert held == "muru: release legacy@r2 is ready and held: not serving"

    # Section 9.16: a server's instances are not scaled, so the two
    # cannot share capacity; an ordinary canary runs each at its own size.

    refused = daemon.muructl(
        "rollout", "canary", "legacy@r2", "--steps", "50", "--capacity", "shared", check=False
    )
    assert refused.returncode != 0
    assert (
        "legacy@r1 runs a managed HTTP server, whose instances are not scaled, "
        "so it cannot share capacity"
    ) in refused.stderr, refused.stderr
    daemon.muructl(
        "rollout", "canary", "legacy@r2", "--steps", "50", "--hold", "2s", "--min-requests", "20"
    )

    def served_by(cookie: str) -> str:
        response = daemon.request("/", host="legacy.test", headers={"Cookie": cookie})
        assert response.status == 200, response.body
        body = json.loads(response.body)
        return "legacy" if body.get("server") == "legacy" else "shop"

    seen = {served_by(f"muru_id=cutover-{n}") for n in range(40)}
    assert seen == {"legacy", "shop"}

    # The gates pass and the native release is promoted; the server is
    # drained and kept warm, and a rollback brings it straight back.

    def states() -> dict[str, tuple[str, str]]:
        return {row["release"]: (row["state"], row["serving"]) for row in daemon.releases("legacy")}

    wait_for(lambda: states().get("legacy@r2") == ("ready", "yes"), 60)
    wait_for(lambda: states().get("legacy@r1") == ("retained", ""), 30)
    assert json.loads(daemon.request("/", host="legacy.test").body).get("server") != "legacy"
    pid = None

    rolled = daemon.muructl("rollback", "legacy").stdout
    assert "legacy@r1" in rolled
    assert report(daemon, host="legacy.test")["server"] == "legacy"
    pid = report(daemon, host="legacy.test")["pid"]
    wait_for(lambda: states().get("legacy@r2") == ("retained", ""), 30)

    # Drained for good, the instance is stopped and its socket removed.

    sockets = daemon.host_sockets("legacy@r1")
    assert sockets.is_dir()
    daemon.muructl("drain", "legacy@r1")
    wait_for(lambda: states().get("legacy@r1") == ("stopped", ""), 30)
    wait_for(lambda: not alive(pid))
    assert not sockets.exists()


def test_a_gunicorn_deployment_cuts_over_to_native_workers_under_load(
    daemon: Daemon, tmp_path: Path
) -> None:
    """The exit criterion of phase 3 (section 14): an application served
    by gunicorn runs unchanged as a managed release, then is canaried to
    native workers, promoted, and its server drained, with no request
    failing throughout."""

    try:
        __import__("gunicorn")
    except ImportError:
        support.unavailable("gunicorn is not installed")

    line = daemon.muructl(
        "deploy",
        "legacy",
        str(LEGACY),
        "--server",
        "gunicorn",
        "--",
        sys.executable,
        "-m",
        "gunicorn",
        "--workers",
        "2",
        "legacy:application",
    ).stdout.strip()
    assert line == "muru: release legacy@r1 is ready and serving"
    assert not report(daemon)["native"]

    def states() -> dict[str, tuple[str, str]]:
        return {row["release"]: (row["state"], row["serving"]) for row in daemon.releases("legacy")}

    with Load(daemon.port, clients=4) as load:
        # The same application on native workers, held, then canaried
        # against gunicorn and promoted once the gates pass.

        held = daemon.muructl(
            "deploy",
            "legacy",
            str(LEGACY),
            "--entrypoint",
            "legacy:application",
            "--python",
            sys.executable,
            "--hold",
        ).stdout.strip()
        assert held == "muru: release legacy@r2 is ready and held: not serving"
        daemon.muructl(
            "rollout",
            "canary",
            "legacy@r2",
            "--steps",
            "50",
            "--hold",
            "2s",
            "--min-requests",
            "20",
        )
        wait_for(lambda: states().get("legacy@r2") == ("ready", "yes"), 60)
        wait_for(lambda: states().get("legacy@r1") == ("retained", ""), 30)

        # gunicorn is drained for good.

        daemon.muructl("drain", "legacy@r1")
        wait_for(lambda: states().get("legacy@r1") == ("stopped", ""), 30)
        time.sleep(0.5)

    load.check("the cutover from gunicorn to native workers", at_least=200)
    assert report(daemon)["native"]


def test_declared_servers_are_applied_exported_and_checked(daemon: Daemon, tmp_path: Path) -> None:
    config = tmp_path / "legacy.toml"
    config.write_text(
        f"""[app.legacy.release.r1]
kind = 'http'
profile = 'generic'
command = {json.dumps(SERVER)}
cwd = '{LEGACY}'
instances = 1
readiness = {{ path = '/healthz', timeout = '20s' }}
stop_signal = 'TERM'
env = {{ LEGACY_SETTING = 'declared' }}

[app.legacy.release.r1.restart]
policy = 'on-failure'
success_exit_codes = [0, 3]
max_delay = '2m'
give_up_after = 5

[host.legacy]
names = ['legacy.test']
default = true

[[route]]
host = 'legacy'
app = 'legacy'
"""
    )
    planned = daemon.muructl("apply", "-f", str(config), "--dry-run", check=False).stdout
    assert "generic, 1 instance in" in planned, planned
    daemon.muructl("apply", "-f", str(config))
    assert report(daemon, host="legacy.test")["env"]["LEGACY_SETTING"] == "declared"

    exported = daemon.muructl("deployment", "export").stdout
    assert "kind = 'http'" in exported.replace('"', "'")
    assert "profile = 'generic'" in exported.replace('"', "'")
    assert "stop_signal = 'TERM'" in exported.replace('"', "'")
    assert "entrypoint" not in exported

    # The restart table is exported with every key, the defaults of
    # those the file left out among them.

    restart = tomllib.loads(exported)["app"]["legacy"]["release"]["r1"]["restart"]
    assert restart == {
        "policy": "on-failure",
        "success_exit_codes": [0, 3],
        "delay": "1s",
        "max_delay": "120s",
        "stable_after": "60s",
        "give_up_after": 5,
    }, exported
    again = tmp_path / "again.toml"
    again.write_text(exported)
    assert "no changes" in daemon.muructl("apply", "-f", str(again)).stdout

    def refused(table: str) -> str:
        bad = tmp_path / "bad.toml"
        bad.write_text(f"[app.bad.release.r1]\n{table}\n")
        completed = daemon.muructl("apply", "-f", str(bad), check=False)
        assert completed.returncode != 0
        return completed.stdout + completed.stderr

    base = f"kind = 'http'\ncommand = ['x']\ncwd = '{LEGACY}'\n"
    assert "profile: a managed HTTP release names its server" in refused(base)
    assert "stop_signal: the generic profile needs" in refused(base + "profile = 'generic'\n")
    assert "processes does not apply to a managed HTTP release" in refused(
        base + "profile = 'gunicorn'\nprocesses = 2\n"
    )
    assert "mod_wsgi-express binds a port only" in refused(base + "profile = 'mod_wsgi-express'\n")
    assert 'readiness.path "healthz" must start with /' in refused(
        base + "profile = 'gunicorn'\nreadiness = { path = 'healthz' }\n"
    )
    assert "instances must be at least 1" in refused(base + "profile = 'gunicorn'\ninstances = 0\n")
    assert "stop_signal must be one of" in refused(
        base + "profile = 'gunicorn'\nstop_signal = 'KILL'\n"
    )
    assert "profile applies to a managed HTTP release" in refused(
        f"source = '{SHOP}'\nprofile = 'gunicorn'\n"
    )
    assert "restart applies to a managed HTTP release" in refused(
        f"source = '{SHOP}'\nrestart = {{ policy = 'never' }}\n"
    )
    assert "restart.max_delay (5s) must not be shorter than restart.delay (10s)" in refused(
        base + "profile = 'gunicorn'\nrestart = { delay = '10s', max_delay = '5s' }\n"
    )
    assert "restart.give_up_after must be at least 1" in refused(
        base + "profile = 'gunicorn'\nrestart = { give_up_after = 0 }\n"
    )
    assert "sometimes" in refused(
        base + "profile = 'gunicorn'\nrestart = { policy = 'sometimes' }\n"
    )
    assert "a managed HTTP release needs the command" in refused(
        f"kind = 'http'\nprofile = 'gunicorn'\ncwd = '{LEGACY}'\n"
    )


def test_a_daemon_restart_readopts_the_instances(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        deploy_server(first, "legacy", "--bind", "port")
        pid = report(first)["pid"]
        started = instances(first, "legacy@r1")[0]["started_unix_ms"]

        # The daemon detaches; the instance and the edge stay up.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert report(first)["pid"] == pid

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        assert report(second)["pid"] == pid
        adopted = instances(second, "legacy@r1")
        assert [(instance["pid"], instance["started_unix_ms"]) for instance in adopted] == [
            (pid, started)
        ]
        assert second.releases("legacy") == [
            {"release": "legacy@r1", "state": "ready", "serving": "yes"}
        ]
        assert any(
            "re-adopted the server instances" in line
            for line in second.muructl("events").stdout.splitlines()
        )

        # An adopted instance that crashes is started again by the new
        # daemon, as its own child.

        assert report(second, "/crash")["pid"] == pid
        wait_for(lambda: not alive(pid))

        # Issue 922: the relay reports the end at once, so a request sent
        # before the daemon's next check is held, not refused, and is
        # answered by the instance started in its place.

        time.sleep(0.2)
        replacement = report(second)["pid"]
        assert replacement != pid
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)

    wait_for(lambda: not alive(replacement))


def test_a_release_that_cannot_start_when_the_daemon_does_is_tried_again(tmp_path: Path) -> None:
    # The server starts unless the file is there.

    broken = tmp_path / "broken"
    script = f"test -e {shlex.quote(str(broken))} && exit 7; exec {shlex.join(SERVER)}"
    first = start_daemon(tmp_path)
    second = None

    try:
        deploy_server(first, "legacy", command=["/bin/sh", "-c", script])
        pid = report(first)["pid"]
        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)

        # As after a reboot: nothing of the release runs, its runtime
        # files are gone, and what the server needs is not there yet.

        os.kill(pid, signal.SIGKILL)
        wait_for(lambda: not alive(pid))
        (first.files_dir / "h" / "1" / "servers.toml").unlink()
        broken.touch()
        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        def events(kind: str) -> list[str]:
            lines = second.muructl("events").stdout.splitlines()
            return [line for line in lines if f" {kind} " in line]

        # Each start that fails is an event, as any failed start is, and
        # another says when the next attempt is, the waits doubling.

        wait_for(lambda: len(events("release.restarting")) >= 2, 30)
        again = events("release.restarting")
        assert again[0].endswith(
            "could not restart after the daemon restarted (attempt 1): instance 0 exited with "
            "exit status: 7 before it was ready; trying again in 1s"
        )
        assert "(attempt 2)" in again[1] and again[1].endswith("trying again in 2s")
        assert len(events("release.failed")) >= 2
        shown = json.loads(second.muructl("release", "show", "legacy@r1", "--json").stdout)
        assert shown["state"] == "failed"
        assert "trying again in" in shown["message"]

        # Once the server can start, the next attempt brings it back.

        broken.unlink()
        wait_for(lambda: second.request("/").status == 200, 30)
        assert second.releases("legacy") == [
            {"release": "legacy@r1", "state": "ready", "serving": "yes"}
        ]
        attempts = len(events("release.restarting"))
        time.sleep(2.5)
        assert len(events("release.restarting")) == attempts
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)


def test_a_daemon_restart_adopts_the_instances_still_there_and_starts_the_rest(
    tmp_path: Path,
) -> None:
    first = start_daemon(tmp_path)
    second = None
    ready = tmp_path / "ready"
    ready.touch()
    last: set[int] = set()

    try:
        deploy_server(
            first,
            "legacy",
            "--bind",
            "port",
            "--instances",
            "2",
            "--readiness",
            "/healthz",
            "--env",
            f"LEGACY_READY={ready}",
        )
        before = instances(first, "legacy@r1")
        survivor, lost = before[0]["pid"], before[1]["pid"]

        # The daemon detaches, and one instance exits while none is there.
        # Whatever is started in its place answers its readiness probe
        # only once the file is back.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        os.kill(lost, signal.SIGKILL)
        wait_for(lambda: not alive(lost))
        ready.unlink()

        # The next daemon takes over the instance that is still there, as
        # it was, and starts the other again at its address. The release
        # is the one it was, not a fresh start beside the survivor.

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        wait_for(lambda: instances(second, "legacy@r1")[1]["pid"] not in (0, lost), 20)
        after = instances(second, "legacy@r1")
        assert after[0] == before[0]
        assert after[1]["index"] == 1
        assert after[1]["address"] == before[1]["address"]
        replacement = after[1]["pid"]
        assert second.releases("legacy") == [
            {"release": "legacy@r1", "state": "ready", "serving": "yes"}
        ]
        events = second.muructl("events").stdout
        assert f"re-adopted the server instances (pid {survivor})" in events

        # Its end is told once, by the adoption (issue 916).

        assert (
            "instance 1 exited with signal: 9 (SIGKILL) while no daemon ran "
            "and is started again in 1s"
        ) in events
        assert events.count("instance 1 exited") == 1
        assert f"instance 1 started again (pid {replacement})" in events

        # Issue 917: started again but not ready, the instance takes no
        # requests, and none fails, until it answers its probe; then both
        # serve.

        def serving() -> set[int]:
            responses = [second.request(f"/n/{n}") for n in range(8)]
            assert [r.status for r in responses] == [200] * 8
            return {json.loads(r.body)["pid"] for r in responses}

        assert serving() == {survivor}
        ready.touch()
        wait_for(lambda: serving() == {survivor, replacement}, 20)
        assert "instance 1 answers and takes requests again" in second.muructl("events").stdout

        # Issue 919: with both started again and neither ready, a request
        # waits for one to answer rather than failing.

        ready.unlink()
        os.kill(survivor, signal.SIGKILL)
        os.kill(replacement, signal.SIGKILL)
        wait_for(
            lambda: all(
                instance["pid"] not in (0, survivor, replacement)
                for instance in instances(second, "legacy@r1")
            ),
            20,
        )
        answers: list[support.Response] = []
        held = threading.Thread(target=lambda: answers.append(second.request("/held")))
        held.start()
        time.sleep(0.5)
        assert held.is_alive()
        ready.touch()
        held.join(timeout=10)
        assert [answer.status for answer in answers] == [200]
        last = {instance["pid"] for instance in instances(second, "legacy@r1")}
        assert json.loads(answers[0].body)["pid"] in last
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)

    # The shutdown stops them all: the survivor was not left behind.

    wait_for(lambda: not any(alive(pid) for pid in {survivor, replacement, *last}))


def test_instance_lines_are_timed_tagged_and_kept_through_a_rotation(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra="[logs]\ncompress = 'off'\n",
    )
    logs = tmp_path / "logs" / "apps" / "legacy"

    try:
        deploy_server(daemon, "legacy", "--instances", "2")

        # Requests go on while the release's log is rotated; the server
        # prints a line on standard output for each.

        stop = threading.Event()
        served: list[str] = []

        def requests() -> None:
            n = 0

            while not stop.is_set():
                path = f"/n/{n}"

                if daemon.request(path).status == 200:
                    served.append(path)

                n += 1

        load = threading.Thread(target=requests)
        load.start()

        try:
            wait_for(lambda: len(served) > 20)
            answer = json.loads(daemon.muructl("logs", "rotate", "legacy@r1", "--json").stdout)
            assert answer["logs"][0]["error"] == ""
            rotated_at = len(served)
            wait_for(lambda: len(served) > rotated_at + 20)
        finally:
            stop.set()
            load.join()

        # Every request's line is in the log or its archive, with the time
        # in front and its instance and stream named.

        def written() -> str:
            return "".join(path.read_text() for path in sorted(logs.glob("r1.log*")))

        def logged(path: str) -> bool:
            return f'"GET {path} HTTP' in written()

        wait_for(lambda: all(logged(path) for path in served))
        assert len(list(logs.glob("r1.log.*"))) == 1
        pattern = re.compile(
            r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z muru\.instance\[[01]\]\.stdout: legacy: "
        )
        lines = [line for line in written().splitlines() if "legacy: " in line]
        assert lines and all(pattern.match(line) for line in lines), lines[:5]
        tagged = {line.split(" ", 2)[1] for line in lines}
        assert tagged == {"muru.instance[0].stdout:", "muru.instance[1].stdout:"}
    finally:
        daemon.stop()


def test_an_instance_that_exits_while_no_daemon_runs_is_reported_with_its_status(
    tmp_path: Path,
) -> None:
    first = start_daemon(tmp_path)
    second = None

    try:
        deploy_server(first, "legacy", "--bind", "port")
        pid = instances(first, "legacy@r1")[0]["pid"]

        # The daemon detaches, and the instance exits with status 3 while
        # none is there; its relay records how.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert report(first, "/crash")["pid"] == pid
        wait_for(lambda: not alive(pid))

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        wait_for(lambda: "instance 0 exited with exit status: 3" in second.muructl("events").stdout)
        wait_for(lambda: instances(second, "legacy@r1")[0]["pid"] not in (0, pid), 20)
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        second.stop(shutdown=True)


def test_a_recorded_pid_that_another_process_has_is_not_adopted(tmp_path: Path) -> None:
    first = start_daemon(tmp_path)
    second = None
    bystander = None

    try:
        deploy_server(first, "legacy", "--bind", "port")
        pid = instances(first, "legacy@r1")[0]["pid"]
        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)

        # The instance exits while no daemon is there, and the record is
        # changed to say its pid is that of a process of the test's own,
        # as it would be had the pid been given out again. The start time
        # recorded beside it is still the instance's, which started well
        # before the bystander: a deploy and a daemon's stop ago.

        os.kill(pid, signal.SIGKILL)
        wait_for(lambda: not alive(pid))
        bystander = subprocess.Popen(["/bin/sleep", "600"])
        record = first.files_dir / "h" / "1" / "servers.toml"
        text = record.read_text()
        assert f"pid = {pid}\n" in text
        assert "start_time = " in text and "start_time = 0\n" not in text
        record.write_text(text.replace(f"pid = {pid}\n", f"pid = {bystander.pid}\n"))

        # The next daemon does not take the bystander for the instance.
        # It starts the instance again.

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        wait_for(lambda: instances(second, "legacy@r1")[0]["pid"] != 0, 20)
        replacement = instances(second, "legacy@r1")[0]["pid"]
        assert replacement not in (pid, bystander.pid)
        events = second.muructl("events").stdout
        assert "none of the server instances was still there" in events
        assert f"instance 0 started again (pid {replacement})" in events
        wait_for(lambda: second.request("/").status == 200, 20)
        assert report(second)["pid"] == replacement
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        # The shutdown stops what the daemon runs, and the bystander was
        # never signalled: it is still running.

        second.stop(shutdown=True)

        if bystander is not None:
            untouched = bystander.poll() is None
            bystander.kill()
            bystander.wait()

    assert untouched
    wait_for(lambda: not alive(replacement))
