"""Services (design specification section 9.17).

A `[service.NAME]` table in an applied file names a command the daemon
keeps running: it starts the instances, starts again one that exits by
the service's restart policy, keeps their output in a log of the
service's own, and takes them over across its own restarts. The commands
of `muructl service` list, show, start, stop, restart and signal them.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
import tomllib
from collections.abc import Callable
from pathlib import Path

from support.daemon import Daemon, start_daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

# A program that says who it is, answers HUP, and runs until it is stopped.

WORKER = (
    'echo "$MURU_SERVICE $MURU_SERVICE_INDEX of $COPIES in $MURU_SERVICE_RUN_DIR"; '
    "trap 'echo reloaded' HUP; trap 'exit 0' TERM; while :; do sleep 0.2; done"
)


def wait_for(condition: Callable[[], bool], timeout: float = 20) -> None:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if condition():
            return

        time.sleep(0.1)

    assert condition()


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False

    return True


def apply(daemon: Daemon, path: Path, text: str, *flags: str) -> subprocess.CompletedProcess[str]:
    path.write_text(text)
    return daemon.muructl("apply", "-f", str(path), *flags, check=False)


def services(daemon: Daemon) -> dict[str, dict]:
    listed = json.loads(daemon.muructl("service", "list", "--json").stdout)
    return {service["name"]: service for service in listed["services"]}


def states(daemon: Daemon, name: str) -> list[str]:
    return [instance["state"] for instance in services(daemon)[name]["instance_list"]]


def pids(daemon: Daemon, name: str) -> list[int]:
    return [instance["pid"] for instance in services(daemon)[name]["instance_list"]]


def events(daemon: Daemon, kind: str) -> list[str]:
    lines = daemon.muructl("events").stdout.splitlines()
    return [line.split(None, 5)[5] for line in lines if f" {kind} " in line]


def table(name: str, command: str, extra: str = "") -> str:
    return f"[service.{name}]\ncommand = ['/bin/sh', '-c', {json.dumps(command)}]\n{extra}\n"


def test_a_service_is_applied_run_and_managed_by_hand(daemon: Daemon, tmp_path: Path) -> None:
    node = tmp_path / "node.toml"
    worker = table(
        "worker",
        WORKER,
        "description = 'Two copies'\ninstances = 2\nenv = { COPIES = 'copy {index}' }\n"
        "readiness = { after = '200ms' }\nreload_signal = 'HUP'\nstop_timeout = '5s'\n",
    )
    spare = table("spare", "exec sleep 600", "autostart = false\n")

    # A plan says what an apply would do, and the apply does it: the
    # service that starts automatically is started, the other only listed.

    planned = apply(daemon, node, worker + spare, "--dry-run").stdout
    assert "create-service  service spare" in planned and "not started automatically" in planned
    assert "create-service  service worker" in planned and ", 2 instances" in planned
    assert services(daemon) == {}
    applied = apply(daemon, node, worker + spare).stdout
    assert "create-service  service worker" in applied and "created" in applied
    wait_for(lambda: states(daemon, "worker") == ["running", "running"])
    listed = services(daemon)
    assert listed["spare"]["state"] == "stopped" and listed["spare"]["running"] == 0
    assert (listed["worker"]["running"], listed["worker"]["instances"]) == (2, 2)
    assert listed["worker"]["deployment"] == "default"
    rows = daemon.muructl("service", "list").stdout.splitlines()
    assert rows[0].split() == ["SERVICE", "STATE", "UP", "PID", "SINCE", "RESTARTS", "COMMAND"]
    assert rows[1].split()[:4] == ["spare", "stopped", "0/1", "-"]
    assert rows[2].split()[:3] == ["worker", "running", "2/2"]

    # Each instance was given its index, its own directory and the
    # service's variables, and its output is in the service's log, tagged.
    # The directories are numbered: spare is the first service, worker
    # the second.

    log = daemon.muructl("service", "logs", "worker").stdout
    run_dir = daemon.run_dir / "s" / "2"
    assert f"muru.service[0].stdout: worker 0 of copy 0 in {run_dir}/0" in log
    assert f"muru.service[1].stdout: worker 1 of copy 1 in {run_dir}/1" in log
    assert (daemon.base / "logs" / "services" / "worker.log").read_text() == log
    shown = daemon.muructl("service", "status", "worker").stdout
    assert "description  Two copies" in shown
    assert "readiness    after 200ms" in shown
    assert "stop         TERM, killed after 5s" in shown
    assert f"instance 1   running, pid {pids(daemon, 'worker')[1]}, up " in shown

    # The services are in the daemon's status, its process tree, its
    # metrics and its list of logs.

    assert "services 1 running, 1 stopped" in daemon.muructl("status").stdout
    tree = daemon.muructl("status", "--processes").stdout
    assert "service worker instance 0: sh" in tree and "service worker output to" in tree
    metrics = daemon.muructl("metrics").stdout
    assert 'muru_service_up{service="worker"} 1' in metrics
    assert 'muru_service_up{service="spare"} 0' in metrics
    assert 'muru_service_instances{service="worker",state="running"} 2' in metrics
    assert 'muru_service_restarts_total{service="worker"} 0' in metrics
    assert 'muru_service_start_time_seconds{service="worker",instance="1"} ' in metrics
    logs = json.loads(daemon.muructl("logs", "list", "--json").stdout)["logs"]
    kinds = {log["name"]: (log["kind"], log["orphaned"]) for log in logs}
    assert kinds["services/worker.log"] == ("service", False)

    # A rotation of the log is followed: what is written next is in the
    # new file.

    daemon.muructl("logs", "rotate", "--all")
    daemon.muructl("service", "signal", "worker", "HUP", "--index", "0")
    current = daemon.base / "logs" / "services" / "worker.log"
    wait_for(lambda: "reloaded" in current.read_text())
    assert "worker 0 of copy 0" not in current.read_text()
    assert len(list(current.parent.glob("worker.log.*"))) == 1

    # Status says by its exit status whether the services run.

    assert daemon.muructl("service", "status", "worker", check=False).returncode == 0
    assert daemon.muructl("service", "status", "worker", "spare", check=False).returncode == 3
    missing = daemon.muructl("service", "status", "nothing", check=False)
    assert missing.returncode == 4 and "there is no service named nothing" in missing.stderr
    quiet = daemon.muructl("service", "list", "--state", "failed,backoff", "--quiet", check=False)
    assert (quiet.returncode, quiet.stdout) == (0, "")
    quiet = daemon.muructl("service", "list", "--state", "stopped", "--quiet", check=False)
    assert (quiet.returncode, quiet.stdout) == (1, "")

    # A signal goes to the instances: the service's own by `reload`.

    first, second = pids(daemon, "worker")
    assert daemon.muructl("service", "signal", "worker", "reload").stdout.splitlines() == [
        f"muru: sent its reload signal to worker instance 0 (pid {first})",
        f"muru: sent its reload signal to worker instance 1 (pid {second})",
    ]
    wait_for(lambda: daemon.muructl("service", "logs", "worker").stdout.count("reloaded") == 3)
    refused = daemon.muructl("service", "signal", "worker", "KILL", check=False)
    assert refused.returncode != 0 and "muructl service stop --kill" in refused.stderr
    refused = daemon.muructl("service", "signal", "spare", "reload", check=False)
    assert "spare names no reload_signal" in refused.stderr

    # One instance stopped leaves the other running.

    stopped = daemon.muructl("service", "stop", "worker", "--index", "1").stdout.strip()
    assert stopped == "muru: worker instance 1 stopped; the other 1 keeps running"
    assert states(daemon, "worker") == ["running", "stopped"]
    assert not alive(second) and alive(first)

    # A restart starts it too, and counts neither a restart nor an exit;
    # one at a time, each instance is ready before the next is stopped.

    assert daemon.muructl("service", "restart", "worker").stdout.strip() == (
        "muru: worker restarted"
    )
    wait_for(lambda: states(daemon, "worker") == ["running", "running"])
    between = pids(daemon, "worker")
    assert first not in between and 0 not in between
    daemon.muructl("service", "restart", "worker", "--rolling")
    assert states(daemon, "worker") == ["running", "running"]
    assert not set(pids(daemon, "worker")) & set(between)
    worker_now = services(daemon)["worker"]
    assert worker_now["restarts"] == 0
    assert all(instance["quick_exits"] == 0 for instance in worker_now["instance_list"])
    order = events(daemon, "service.exited")[-2:] + events(daemon, "service.ready")[-2:]
    assert [line.split(" ", 2)[1] for line in order] == ["0", "1", "0", "1"]
    lines = daemon.muructl("events").stdout.splitlines()
    ready_0 = max(n for n, line in enumerate(lines) if "instance 0 is ready" in line)
    stopped_1 = max(n for n, line in enumerate(lines) if "instance 1 stopped for a restart" in line)
    assert ready_0 < stopped_1, lines[-8:]

    # A service stopped whole is held, and one started by hand runs
    # though it is not started automatically.

    assert daemon.muructl("service", "stop", "worker").stdout.strip() == "muru: worker stopped"
    held = services(daemon)["worker"]
    assert (held["state"], held["held"]) == ("stopped", True)
    assert (
        "state        stopped by hand"
        in daemon.muructl("service", "status", "worker", check=False).stdout
    )
    started = daemon.muructl("service", "start", "spare", "--wait").stdout.splitlines()
    assert started[0].startswith("muru: spare started")
    assert started[1] == f"muru: spare is running (pid {pids(daemon, 'spare')[0]})"
    again = daemon.muructl("service", "start", "spare").stdout.strip()
    assert again == f"muru: spare is already running (pid {pids(daemon, 'spare')[0]})"
    killed = pids(daemon, "spare")[0]
    assert daemon.muructl("service", "stop", "spare", "--kill").stdout.strip() == (
        "muru: spare killed"
    )
    assert not alive(killed)

    # --all starts what is not running, the one that is not started
    # automatically apart.

    daemon.muructl("service", "start", "--all", "--wait")
    listed = services(daemon)
    assert (listed["worker"]["state"], listed["worker"]["held"]) == ("running", False)
    assert listed["spare"]["state"] == "stopped"

    # The export writes the services back, and applies with no change.

    exported = daemon.muructl("deployment", "export").stdout
    declared = tomllib.loads(exported)["service"]
    assert declared["worker"]["instances"] == 2
    assert declared["spare"]["autostart"] is False
    assert "no changes" in apply(daemon, tmp_path / "again.toml", exported).stdout

    # A changed count stops the last instance and leaves the first; a
    # changed policy is in force with nothing restarted.

    kept = pids(daemon, "worker")[0]
    fewer = worker.replace("instances = 2", "instances = 1") + "restart = { policy = 'never' }\n"
    planned = apply(daemon, node, fewer + spare, "--dry-run").stdout
    assert "instances 2 to 1; 1 instance is stopped" in planned
    apply(daemon, node, fewer + spare)
    wait_for(lambda: len(pids(daemon, "worker")) == 1)
    assert pids(daemon, "worker") == [kept]
    assert "restart      never, " in daemon.muructl("service", "status", "worker").stdout

    # What an instance was started with restarts it.

    changed = fewer.replace("copy {index}", "changed {index}")
    assert "env changed; its 1 instance is restarted" in (
        apply(daemon, node, changed + spare, "--dry-run").stdout
    )
    apply(daemon, node, changed + spare)
    wait_for(lambda: states(daemon, "worker") == ["running"] and pids(daemon, "worker") != [kept])
    assert not alive(kept)
    assert "worker 0 of changed 0 in" in daemon.muructl("service", "logs", "worker").stdout

    # A table that cannot be run is refused, and changes nothing.

    refused = apply(daemon, tmp_path / "bad.toml", "[service.bad]\ncommand = ['x', '{port}']\n")
    assert refused.returncode != 0
    assert "service bad: command: " in refused.stdout + refused.stderr
    assert "bad" not in services(daemon)

    # Pruned, a service is stopped and gone; its log is left.

    running = pids(daemon, "worker")[0]
    pruned = apply(daemon, node, spare, "--prune").stdout
    assert "remove-service  service worker" in pruned
    wait_for(lambda: not alive(running))
    assert list(services(daemon)) == ["spare"]
    assert (daemon.base / "logs" / "services" / "worker.log").exists()
    changes = events(daemon, "service.changed")
    assert changes[0].startswith("defined: ")
    assert changes[-1] == "no longer declared; its instances are stopped"


def test_a_service_that_keeps_exiting_backs_off_and_gives_up(
    daemon: Daemon, tmp_path: Path
) -> None:
    node = tmp_path / "node.toml"
    broken = tmp_path / "broken"
    broken.touch()
    flaky = table(
        "flaky",
        f"test -e {broken} && exit 7; exec sleep 600",
        "readiness = { after = '100ms' }\n"
        "restart = { delay = '200ms', max_delay = '400ms', give_up_after = 4 }\n",
    )
    background = table("background", "sleep 600 & exit 0")
    apply(daemon, node, flaky + background)

    # The third quick exit in a row is a crash loop, and the fourth the
    # last the table allows: the service is failed, and says why.

    wait_for(lambda: services(daemon)["flaky"]["state"] == "failed", 30)
    assert len(events(daemon, "service.crash_looping")) == 1
    assert events(daemon, "service.crash_looping")[0].startswith(
        "instance 0 is crash looping: it has exited or failed to start 3 times in a row"
    )
    assert "(give_up_after = 4)" in events(daemon, "service.gave_up")[0]
    exits = [line for line in events(daemon, "service.exited") if "exit status: 7" in line]
    assert len(exits) == 4, exits
    assert exits[0].endswith("starting it again in 200ms")
    assert exits[2].endswith("starting it again in 400ms")
    flaky_now = services(daemon)["flaky"]
    assert flaky_now["restarts"] == 3
    assert len(flaky_now["instance_list"][0]["exits"]) == 4
    shown = daemon.muructl("service", "status", "flaky", check=False).stdout
    assert "instance 0  failed, not started again after 4 quick exits in a row" in shown
    assert "last exits of instance 0" in shown and "exited with exit status: 7 after " in shown

    # It stays failed until it is started by hand, which then works.

    time.sleep(1.5)
    assert len(events(daemon, "service.started")) == len(events(daemon, "service.started"))
    assert services(daemon)["flaky"]["state"] == "failed"
    broken.unlink()
    daemon.muructl("service", "start", "flaky", "--wait")
    recovered = services(daemon)["flaky"]
    assert recovered["state"] == "running"
    assert recovered["instance_list"][0]["quick_exits"] == 0

    # A program that puts itself in the background is named, and what it
    # left running is stopped.

    wait_for(lambda: services(daemon)["background"]["state"] == "failed")
    assert events(daemon, "service.daemonised")[0].startswith(
        "instance 0 put itself in the background"
    )
    assert services(daemon)["background"]["restarts"] == 0


def test_services_outlive_the_daemon_and_are_stopped_by_a_shutdown(tmp_path: Path) -> None:
    node = tmp_path / "node.toml"
    ready = "readiness = { after = '100ms' }\n"
    text = (
        table("kept", WORKER, ready)
        + table("stopped", "exec sleep 600", ready)
        + table("dies", "exec sleep 600", ready)
    )
    first = start_daemon(tmp_path)
    second = None

    try:
        apply(first, node, text)
        wait_for(lambda: all(service["state"] == "running" for service in services(first).values()))
        first.muructl("service", "stop", "stopped")
        kept = pids(first, "kept")[0]

        # An exit this daemon sees is one the next still lists.

        os.kill(pids(first, "dies")[0], signal.SIGKILL)
        wait_for(lambda: services(first)["dies"]["restarts"] == 1)
        wait_for(lambda: services(first)["dies"]["state"] == "running")
        seen = services(first)["dies"]["instance_list"][0]["exits"]
        assert len(seen) == 1
        dies = pids(first, "dies")[0]

        # The daemon detaches, and the services run on with nobody
        # watching: one of them exits meanwhile.

        assert "detached" in first.muructl("daemon", "stop").stdout
        first.process.wait(timeout=30)
        assert alive(kept)
        os.kill(dies, signal.SIGKILL)
        wait_for(lambda: not alive(dies))

        # The next daemon takes over what runs, starts again what exited
        # and says how it ended, and leaves stopped what was stopped by
        # hand.

        second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)
        wait_for(lambda: services(second)["dies"]["state"] == "running")
        listed = services(second)
        assert pids(second, "kept") == [kept]
        assert (listed["stopped"]["state"], listed["stopped"]["held"]) == ("stopped", True)
        assert listed["dies"]["restarts"] == 1
        assert listed["dies"]["instance_list"][0]["exits"][0] == seen[0]
        adopted = dict(
            line.split(None, 5)[4:6]
            for line in second.muructl("events").stdout.splitlines()
            if " service.adopted " in line
        )
        assert adopted["kept"] == f"took over its instances (pid {kept})"
        assert adopted["dies"].startswith(
            "none of its instances was still running; instance 0 exited with signal: 9 (SIGKILL) "
            "while no daemon ran and is started again in "
        )
        assert adopted["stopped"] == (
            "none of its instances was still running; instance 0 stays stopped"
        )

        # The instance kept writes to the same log as before.

        second.muructl("service", "signal", "kept", "HUP")
        wait_for(lambda: "reloaded" in second.muructl("service", "logs", "kept").stdout)
    finally:
        if second is None:
            second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

        running = [pid for name in ("kept", "dies") for pid in pids(second, name)]
        second.stop(shutdown=True)

    # A shutdown stops the services, and a daemon that then starts from
    # nothing starts them again, the one stopped by hand apart.

    wait_for(lambda: not any(alive(pid) for pid in running))
    third = start_daemon(tmp_path)

    try:
        wait_for(lambda: services(third)["kept"]["state"] == "running")
        listed = services(third)
        assert listed["dies"]["state"] == "running"
        assert (listed["stopped"]["state"], listed["stopped"]["held"]) == ("stopped", True)
    finally:
        third.stop(shutdown=True)


WORKER_SCRIPT = (
    "import os, sys, time\n"
    "print('worker of', os.environ['MURU_RELEASE'], os.environ['TASKS'], "
    "os.path.realpath(sys.executable), flush=True)\n"
    "time.sleep(600)\n"
)


def project_with_services(directory: Path, beat: bool = True) -> Path:
    """A project that serves a page and declares services."""

    directory.mkdir(exist_ok=True)
    (directory / "worker.py").write_text(WORKER_SCRIPT)
    (directory / "web.py").write_text(
        "def application(environ, start_response):\n"
        "    start_response('200 OK', [('Content-Type', 'text/plain')])\n"
        "    return [b'site']\n"
    )
    services = (
        "[tool.muru.services.worker]\n"
        "command = ['python', 'worker.py']\n"
        "env = { TASKS = 'queue-{index}' }\n"
        "readiness = { after = '200ms' }\n"
        "switch = 'start-first'\n\n"
        "[tool.muru.services.reindex]\n"
        "command = ['/bin/sleep', '600']\n"
        "autostart = false\n"
    )

    if beat:
        services += (
            "\n[tool.muru.services.beat]\n"
            "command = ['/bin/sh', '-c', 'echo beat of $MURU_RELEASE in $PWD; exec sleep 600']\n"
            "readiness = { after = '200ms' }\n"
        )

    (directory / "pyproject.toml").write_text(
        "[project]\nname = 'shop'\nversion = '0.0.0'\n\n"
        "[tool.muru]\nentrypoint = 'web:application'\nprocesses = 1\n\n" + services
    )
    return directory


def test_a_projects_services_run_with_its_serving_release(daemon: Daemon, tmp_path: Path) -> None:
    project = project_with_services(tmp_path / "shop")

    def of_app() -> dict[str, dict]:
        listed = json.loads(daemon.muructl("service", "list", "--app", "shop", "--json").stdout)
        return {service["name"]: service for service in listed["services"]}

    def running_for(release: str) -> bool:
        return all(
            (service["release"], service["state"]) == (release, "running")
            for name, service in of_app().items()
            if name != "shop/reindex" and not service["held"]
        )

    # The first deploy brings the release's services with it: those that
    # start automatically run, named with the application.

    daemon.deploy("shop", project, "--wait-services")
    listed = of_app()
    assert list(listed) == ["shop/beat", "shop/reindex", "shop/worker"]
    assert all(
        service["app"] == "shop" and service["release"] == "r1" for service in listed.values()
    )
    assert [listed[name]["state"] for name in listed] == ["running", "stopped", "running"]

    # A service runs with what a command of the release runs with: its
    # interpreter for `python`, its source as the directory, the
    # release's name, and its own variables over them.

    worker_log = daemon.muructl("service", "logs", "shop/worker").stdout
    assert f"worker of r1 queue-0 {os.path.realpath(sys.executable)}" in worker_log
    assert f"beat of r1 in {project}" in daemon.muructl("service", "logs", "shop/beat").stdout
    assert (daemon.base / "logs" / "apps" / "shop" / "services" / "worker.log").exists()
    logs = json.loads(daemon.muructl("logs", "list", "--json").stdout)["logs"]
    kinds = {log["name"]: (log["kind"], log["app"]) for log in logs}
    assert kinds["apps/shop/services/worker.log"] == ("service", "shop")

    # A held release runs no services: those of the serving one run on.

    first = {name: service["instance_list"][0]["pid"] for name, service in listed.items()}
    daemon.deploy("shop", project, "--hold")
    time.sleep(1.5)
    assert running_for("r1")
    assert of_app()["shop/worker"]["instance_list"][0]["pid"] == first["shop/worker"]

    # A deploy that takes over changes each service over as it asks: the
    # worker's new instance is ready before the old one stops, and the
    # scheduler's old one stops before the new one starts.

    daemon.deploy("shop", project, "--wait-services")
    assert running_for("r3")
    wait_for(lambda: not alive(first["shop/worker"]) and not alive(first["shop/beat"]))
    assert "worker of r3 queue-0" in daemon.muructl("service", "logs", "shop/worker").stdout
    changed = dict(
        (line.split(None, 5)[4], line.split(None, 5)[5])
        for line in daemon.muructl("events").stdout.splitlines()
        if " service.changed " in line and "shop@r3 takes over" in line
    )
    assert changed["shop/worker"].endswith("the old ones stopped once they are ready")
    assert changed["shop/beat"].endswith("the old instances are stopped, and its own started then")

    def after_takeover() -> list[str]:
        lines = daemon.muructl("events").stdout.splitlines()
        worker = [line for line in lines if " shop/worker " in line]
        took_over = next(n for n, line in enumerate(worker) if "shop@r3 takes over" in line)
        return [line.split(None, 5)[5] for line in worker[took_over + 1 :]]

    wait_for(lambda: any("stopped (" in line for line in after_takeover()))
    after = after_takeover()
    ready = next(n for n, line in enumerate(after) if "is ready" in line)
    stopped = next(n for n, line in enumerate(after) if "stopped (" in line)
    assert ready < stopped, after

    # A service stopped by hand stays stopped when another release takes
    # over, and a rollback takes the services back with the traffic.

    daemon.muructl("service", "stop", "shop/beat")
    daemon.deploy("shop", project, "--wait-services")
    listed = of_app()
    assert (listed["shop/beat"]["state"], listed["shop/beat"]["held"]) == ("stopped", True)
    assert (listed["shop/worker"]["release"], listed["shop/worker"]["state"]) == ("r4", "running")
    daemon.muructl("rollback", "shop")
    wait_for(lambda: of_app()["shop/worker"]["release"] == "r3")
    wait_for(lambda: of_app()["shop/worker"]["state"] == "running")
    daemon.muructl("service", "start", "shop/beat", "--wait")
    assert of_app()["shop/beat"]["release"] == "r3"

    # The node's own settings for a service are taken by the releases
    # deployed from then on, over what the project declares.

    daemon.muructl(
        "app", "set", "shop", "--service", "worker.instances=2", "--service", "beat.autostart=false"
    )
    shown = daemon.muructl("app", "show", "shop").stdout
    assert "service.worker.instances" in shown and "service.beat.autostart" in shown
    exported = tomllib.loads(daemon.muructl("deployment", "export").stdout)
    assert exported["app"]["shop"]["service"] == {
        "beat": {"autostart": False},
        "worker": {"instances": 2},
    }
    daemon.deploy("shop", project, "--wait-services")
    listed = of_app()
    assert [instance["state"] for instance in listed["shop/worker"]["instance_list"]] == [
        "running",
        "running",
    ]
    wait_for(lambda: of_app()["shop/beat"]["state"] == "stopped")
    assert of_app()["shop/beat"]["held"] is False
    daemon.muructl("app", "unset", "shop", "service.worker", "service.beat")

    # A release that no longer declares a service leaves nothing in its
    # place, and a drained application runs none.

    project_with_services(project, beat=False)
    daemon.deploy("shop", project, "--wait-services")
    wait_for(lambda: list(of_app()) == ["shop/reindex", "shop/worker"])
    serving = of_app()["shop/worker"]
    pid = serving["instance_list"][0]["pid"]
    daemon.muructl("drain", f"shop@{serving['release']}")
    wait_for(lambda: of_app() == {})
    assert not alive(pid)

    # A service that does not come up fails --wait-services, and the
    # release is serving all the same.

    (project / "worker.py").write_text("raise SystemExit(3)\n")
    settings = project / "pyproject.toml"
    settings.write_text(
        settings.read_text().replace(
            "switch = 'start-first'", "switch = 'start-first'\nrestart = { give_up_after = 2 }"
        )
    )
    failed = daemon.muructl(
        "deploy", "shop", str(project), "--python", sys.executable, "--wait-services", check=False
    )
    assert failed.returncode != 0
    assert "its service shop/worker is not running" in failed.stderr
    assert daemon.get().body == b"site"

    # A node's service takes none of the keys that follow a release.

    refused = apply(
        daemon, tmp_path / "node.toml", table("x", "sleep 1", "switch = 'start-first'\n")
    )
    assert (
        "service x: switch is for a service a project declares" in refused.stdout + refused.stderr
    )


def test_a_projects_services_run_beside_a_development_run(tmp_path: Path) -> None:
    from support import server as support

    runtime = support.short_dir("muru-rt-")
    project = tmp_path / "shop"
    project.mkdir()
    (project / "web.py").write_text(
        "def application(environ, start_response):\n"
        "    start_response('200 OK', [('Content-Type', 'text/plain')])\n"
        "    return [b'one']\n"
    )
    (project / "pyproject.toml").write_text(
        "[project]\nname = 'shop'\nversion = '0.0.0'\n\n"
        "[tool.muru]\nentrypoint = 'web:application'\n\n"
        "[tool.muru.services.worker]\n"
        "command = ['python', '-c', 'import os, time; "
        'print("worker of", os.environ["MURU_RELEASE"], flush=True); time.sleep(600)\']\n'
        "readiness = { after = '200ms' }\n\n"
        "[tool.muru.services.reindex]\ncommand = ['/bin/sleep', '600']\nautostart = false\n"
    )

    def command(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(support.muructl_path()), *arguments],
            capture_output=True,
            text=True,
            timeout=60,
            env={**os.environ, "MURU_RUN_DIR": str(runtime)},
        )

    server = support.start(
        None,
        workers=1,
        chdir=project,
        args=["--reload"],
        env={"MURU_RUN_DIR": str(runtime)},
    )

    try:
        # The services that start automatically run beside the
        # application, their lines with the run's own.

        assert server.wait_for_log("muru.service.worker[0]: worker of r1", timeout=20), server.log()
        assert server.wait_for_log("admin socket"), server.log()
        wait_for(
            lambda: (
                [
                    (service["name"], service["state"])
                    for service in json.loads(command("service", "list", "--json").stdout)[
                        "services"
                    ]
                ]
                == [("reindex", "stopped"), ("worker", "running")]
            )
        )
        first = json.loads(command("service", "list", "--json").stdout)["services"][1]
        pid = first["instance_list"][0]["pid"]

        # A reload starts a new release, and the worker with it, so that
        # it runs the edited code.

        (project / "web.py").write_text((project / "web.py").read_text().replace("one", "two"))
        assert server.wait_for_log("switched to release r2", timeout=30), server.log()
        assert server.wait_for_log("muru.service.worker[0]: worker of r2", timeout=20), server.log()
        wait_for(lambda: not alive(pid))
    finally:
        server.stop()

    # The services stop with the run.

    listed = subprocess.run(["ps", "-ax", "-o", "command="], capture_output=True, text=True).stdout
    assert 'print("worker of"' not in listed

    # They can be left out.

    quiet = support.start(None, workers=1, chdir=project, args=["--no-services"])

    try:
        assert quiet.get("/").status == 200
        time.sleep(1.5)
        assert "muru.service." not in quiet.log()
    finally:
        quiet.stop()
