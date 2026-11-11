"""muructl debug bundle (design specification section 11.9): the archive
of a system for triage, against a daemon that runs and one that has
stopped, a development run and muructl serve, with its manifest, where
it is written and how its making is recorded."""

from __future__ import annotations

import datetime
import gzip
import io
import json
import os
import platform
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tarfile
import time
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon, start_daemon
from test_run_admin import start as start_run
from test_serve import start as start_serve


def muructl(
    *arguments: str, cwd: Path | None = None, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[bytes]:
    """Run muructl, its output kept as bytes since it may be an archive."""

    path = support.muructl_path()
    assert path is not None
    return subprocess.run(
        [str(path), *arguments],
        capture_output=True,
        check=False,
        timeout=120,
        cwd=cwd,
        env={**os.environ, **(env or {})},
    )


def bundle(daemon: Daemon, *arguments: str, cwd: Path | None = None) -> str:
    """Make a bundle of a test's daemon, by its locations, and return what was said."""

    completed = muructl(
        "debug",
        "bundle",
        *arguments,
        "--run-dir",
        str(daemon.run_dir),
        "--state-dir",
        str(daemon.base / "state"),
        "--log-dir",
        str(daemon.base / "logs"),
        "--config",
        str(daemon.base / "muru.toml"),
        cwd=cwd,
    )
    assert completed.returncode == 0, (completed.stdout + completed.stderr).decode()
    return completed.stdout.decode()


def refused(daemon: Daemon, *arguments: str) -> str:
    """Why a bundle of a test's daemon was refused."""

    completed = muructl(
        "debug", "bundle", *arguments, "--socket", str(daemon.run_dir / "admin.sock")
    )
    assert completed.returncode != 0, completed.stdout.decode()
    return completed.stderr.decode()


def members(archive: bytes) -> dict[str, bytes]:
    """The members of an archive by their path below its one directory,
    each checked to be readable by its owner alone."""

    found = {}

    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        roots = {member.name.split("/", 1)[0] for member in tar.getmembers()}
        assert len(roots) == 1 and next(iter(roots)).startswith("muru-bundle-"), roots

        for member in tar.getmembers():
            assert member.mode == 0o600, member.name
            extracted = tar.extractfile(member)
            assert extracted is not None
            found[member.name.split("/", 1)[1]] = extracted.read()

    assert list(found)[-1] == "manifest.json"
    return found


def manifest_of(found: dict[str, bytes]) -> dict[str, Any]:
    """The manifest, checked to list every member and nothing else as collected."""

    manifest: dict[str, Any] = json.loads(found["manifest.json"])
    collected = {part["path"]: part for part in manifest["parts"] if part["collected"]}
    assert set(collected) == set(found) - {"manifest.json"}

    for path, part in collected.items():
        assert part["bytes"] == len(found[path]), path

    return manifest


def events_of(found: dict[str, bytes]) -> list[dict[str, Any]]:
    return [json.loads(line) for line in found["events.jsonl"].splitlines()]


def records_of(content: bytes) -> list[dict[str, Any]]:
    return [json.loads(line) for line in content.splitlines()]


def stamp(ago: float) -> str:
    """The full time Muru writes, for a moment so many seconds ago."""

    moment = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=ago)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def access_record(ago: float, app: str, client: str, target: str) -> str:
    """An access log record, as the edge writes one."""

    return (
        json.dumps(
            {
                "time": stamp(ago),
                "client": client,
                "app": app,
                "release": "r1",
                "target": target,
                "status": 200,
            },
            separators=(",", ":"),
        )
        + "\n"
    )


def test_bundle_of_a_daemon_running_and_stopped(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        listeners=[(support.free_port(), False)],
        config_extra=(
            f"[overlay_webhook]\nlisten = '127.0.0.1:{support.free_port()}'\nsecret = 'hunter2'\n"
        ),
    )
    out = tmp_path / "out"
    out.mkdir()

    # A copy of the project with a lock file of its own, which names an
    # index by a URL with a password.

    source = tmp_path / "source"
    shutil.copytree(SHOP, source)
    (source / "requirements.txt").write_text(
        "--index-url https://mirror:hunter3@pypi.example/simple\nflask==3.0.3\n"
    )

    try:
        deployed = daemon.deploy(
            "shop",
            source,
            "--processes",
            "2",
            "--build",
            "never",
            "--env",
            "API_TOKEN=abc123",
            "--env",
            "DATABASE_URL=postgres://shop:s3cret@db.example/shop",
        )
        assert deployed == "muru: release shop@r1 is ready and serving"
        assert daemon.get("/catalogue?page=2").status == 200

        # The edge writes the record from a thread of its own.

        deadline = time.monotonic() + 20

        while b"/catalogue?page=2" not in (tmp_path / "logs" / "access.log").read_bytes():
            assert time.monotonic() < deadline, "the access record was never written"
            time.sleep(0.05)

        # A dry run says what the bundle would hold, writes nothing and
        # records nothing.

        planned = bundle(daemon, "--dry-run", cwd=out)
        assert planned.startswith("A bundle of the whole node, reaching back 1h to ")
        assert re.search(
            r"^  installation\.json +asked for when the bundle is made$", planned, re.M
        )
        assert re.search(r"^  events\.jsonl +\d+B$", planned, re.M)
        assert re.search(r"^  config/releases/shop@r1\.json +[\d.]+KB$", planned, re.M)
        assert f"{out} has " in planned and " free." in planned
        assert list(out.iterdir()) == []
        assert "debug.bundle" not in daemon.muructl("events").stdout

        # With no --output the archive is a file named for the time, in
        # the current directory, that only its owner can read.

        said = bundle(daemon, cwd=out)
        (written,) = out.iterdir()
        assert written.name.startswith("muru-bundle-") and written.name.endswith(".tar.gz")
        assert stat.S_IMODE(written.stat().st_mode) == 0o600
        assert said.startswith(f"muru: wrote the bundle to {written.name} (")
        assert "it holds production data" in said and "only its owner can read it" in said

        found = members(written.read_bytes())
        manifest = manifest_of(found)
        assert manifest["format"] == 1 and manifest["kind"] == "bundle"
        assert manifest["target"] == "" and manifest["running"] is True
        assert manifest["runs"] == "a user daemon"
        assert manifest["recorded"] == "as the event debug.bundle"
        window = manifest["window"]
        assert window["until_unix_ms"] - window["since_unix_ms"] == 3_600_000
        assert "secret values" in manifest["left_out"]

        # The summary is the report's facts, with the names as they are.

        installation = json.loads(found["installation.json"])
        assert installation["runs"] == "a user daemon" and installation["running"] is True
        assert installation["locations"]["state"]["path"] == str(tmp_path / "state")
        (release,) = installation["releases"]
        assert release["app"] == "shop" and release["id"] == "r1"
        assert [worker["state"] for worker in release["workers"]] == ["ready", "ready"]

        # The configuration in force: the release's settings with where
        # each came from, its specification, the effective document, what
        # is in force outside it and the daemon's own file, with what
        # looks like a secret masked wherever it is.

        record = json.loads(found["config/releases/shop@r1.json"])
        assert record["release"]["serving"] is True
        assert {
            "scope": "shop@r1",
            "key": "processes",
            "value": "2",
            "source": "deploy flags",
        } in record["settings"]
        assert record["spec"]["entrypoint"] == "shop.wsgi:application"
        assert {"name": "API_TOKEN", "value": "***"} in record["spec"]["env"]
        assert {
            "name": "DATABASE_URL",
            "value": "postgres://shop:***@db.example/shop",
        } in record["spec"]["env"]

        effective = found["config/effective.toml"].decode()
        assert effective.startswith("# No active overlay changes this configuration.\n")
        assert "[app.shop" in effective
        assert sorted(json.loads(found["config/in-force.json"])) == [
            "maintenance",
            "overlays",
            "rollouts",
            "scale_pins",
        ]
        daemon_file = found["config/daemon.toml"].decode()
        assert "[overlay_webhook]" in daemon_file and 'secret = "***"' in daemon_file
        assert json.loads(found["config/access-log-overrides.json"]) == []
        assert json.loads(found["config/routing-snapshot.json"])["version"] >= 1
        setups = json.loads(found["config/logging-setup.json"])
        assert len(setups["shop@r1"]["workers"]) == 2
        assert setups["shop@r1"]["workers"][0]["result"]["loggers"][0]["name"] == "root"
        levels = json.loads(found["config/logger-levels.json"])
        assert [worker["result"] for worker in levels["shop@r1"]["workers"]] == [
            {"changed": []},
            {"changed": []},
        ]

        for secret in (b"abc123", b"s3cret", b"hunter2", b"hunter3"):
            holding = [path for path, content in found.items() if secret in content]
            assert holding == [], f"{secret!r} is in {holding}"

        # The deploy changed the configuration in the window, which the
        # manifest says plainly, and what Muru keeps no record of is
        # listed as not collected with why.

        assert manifest["configuration_changed"] is True
        changes = [json.loads(line)["type"] for line in found["config/changes.jsonl"].splitlines()]
        assert changes == ["release.created", "traffic.switched"]
        changed = next(part for part in manifest["parts"] if part["path"] == "config/changes.jsonl")
        assert changed["note"].startswith("the configuration changed 2 times during the window")
        missing = {part["path"]: part["why"] for part in manifest["parts"] if not part["collected"]}
        assert missing == {
            "config/routing-snapshots": (
                "only the routing snapshot in force is kept, not those before it: the events "
                "in changes.jsonl say what changed, and when"
            ),
            "figures/history.json": (
                "the history the dashboard's charts draw is kept in memory, and the admin "
                "socket does not offer it"
            ),
        }

        # The logs of the window, by their names in the log directory.

        (served,) = records_of(found["logs/access.log"])
        assert served["target"] == "/catalogue?page=2" and served["client"] == "127.0.0.1"
        assert b"muru.host[" in found["logs/apps/shop/r1.log"]
        assert b"edge serving" in found["logs/edge.log"]
        noted = {part["path"]: part.get("note") for part in manifest["parts"]}
        assert noted["logs/access.log"] == "the lines of the window"

        # What the workers were doing: their status, their stacks taken
        # three times, and their tasks, each as the command's own --json
        # gives it; the requests in flight and the open WebSockets; and
        # the figures.

        status = json.loads(found["live/shop@r1/status.json"])
        assert status["release"] == "r1"
        assert [worker["result"]["state"] for worker in status["workers"]] == ["ready", "ready"]

        for sample in (1, 2, 3):
            stacks = json.loads(found[f"live/shop@r1/stacks-{sample}.json"])
            assert len(stacks["workers"]) == 2 and all(worker["ok"] for worker in stacks["workers"])

        assert len(json.loads(found["live/shop@r1/tasks.json"])["workers"]) == 2
        assert json.loads(found["live/requests.json"])["in_flight"] == 0
        assert json.loads(found["live/websockets.json"])["open"] == 0
        assert b"muru_requests_total" in found["figures/metrics.txt"]
        assert json.loads(found["figures/scaling/shop@r1.json"])["groups"][0]["group"] == "default"

        # The machine, what the release's environment holds, and what its
        # workers have loaded, with the release's variables masked as
        # debug inspect masks them.

        host = json.loads(found["host/host.json"])
        assert host["cpus"] >= 1 and len(host["kernel"]) == 4
        assert host["memory_bytes"] > 0 and len(host["load"]) == 3
        disks = {disk["of"]: disk for disk in host["disks"]}
        assert disks["state"]["path"] == str(tmp_path / "state")
        assert disks["state"]["free_bytes"] > 0

        if sys.platform == "linux":
            assert host["memory_available_bytes"] > 0
            limited = dict(host["process_limits"])
            assert "Max open files" in limited["daemon"] and "edge" in limited
        else:
            assert host["process_limits"] == [] and host["cgroup"] is None

        packages = found["host/shop@r1/packages.txt"].decode().splitlines()
        assert packages == sorted(packages) and any(line.startswith("pytest ") for line in packages)
        assert found["host/shop@r1/requirements.txt"].decode() == (
            "--index-url https://mirror:***@pypi.example/simple\nflask==3.0.3\n"
        )
        inspected = json.loads(found["host/shop@r1/inspect.json"])["workers"][0]["result"]
        assert inspected["python"] == platform.python_version()
        assert inspected["environment"]["API_TOKEN"] == "***"
        assert "shop" in inspected["modules"]

        # Nothing has gone wrong yet, and nothing was asked for beyond
        # the default.

        assert found["failures/events.jsonl"] == b""
        assert manifest["options"] == {"scrub": False, "memory": False, "profile_s": None}
        assert not any(path.endswith(("memory.json", "profile.collapsed")) for path in found)

        # Every event of the window is there in full, and the making of
        # the bundle is an event of its own.

        types = [event["type"] for event in events_of(found)]
        assert "release.ready" in types and "traffic.switched" in types
        recorded = daemon.muructl("events").stdout
        assert "debug.bundle" in recorded
        assert "a diagnostic bundle of the whole node was made, reaching back 1h" in recorded

        # A worker that is killed is a failure the next bundle holds.

        os.kill(status["workers"][0]["pid"], signal.SIGKILL)
        deadline = time.monotonic() + 20

        while "worker.exited" not in daemon.muructl("events").stdout:
            assert time.monotonic() < deadline, daemon.muructl("events").stdout
            time.sleep(0.1)

        # A file that exists is left alone. With - the archive goes to
        # standard output, here of one release and a shorter window, with
        # the workers' objects counted and a profile of them, which are
        # collected only when asked for; a target that names nothing is
        # refused.

        assert "exists: name another file with --output, or remove it" in refused(
            daemon, "--output", str(written)
        )
        piped = muructl(
            "debug",
            "bundle",
            "shop@r1",
            "--since",
            "10m",
            "--memory",
            "--profile",
            "1s",
            "--output",
            "-",
            "--socket",
            str(daemon.run_dir / "admin.sock"),
        )
        assert piped.returncode == 0, piped.stderr.decode()
        assert piped.stderr.decode().startswith("muru: wrote the bundle to standard output: ")
        found = members(piped.stdout)
        manifest = manifest_of(found)
        assert manifest["target"] == "shop@r1"
        assert manifest["window"]["until_unix_ms"] - manifest["window"]["since_unix_ms"] == 600_000
        assert "config/releases/shop@r1.json" in found
        assert json.loads(found["installation.json"])["releases"][0]["id"] == "r1"

        # The command was given only the socket: the daemon says where
        # its files are, and they are read from there.

        assert "[overlay_webhook]" in found["config/daemon.toml"].decode()
        assert json.loads(found["installation.json"])["locations"]["logs"] == {
            "path": str(tmp_path / "logs"),
            "rule": "daemon",
        }
        assert b"/catalogue?page=2" in found["logs/access.log"]
        assert manifest["options"] == {"scrub": False, "memory": True, "profile_s": 1.0}
        assert (
            "a diagnostic bundle of shop@r1 was made, reaching back 10m, with memory, profile"
            in daemon.muructl("events").stdout
        )
        (failure,) = [json.loads(line) for line in found["failures/events.jsonl"].splitlines()]
        assert failure["type"] == "worker.exited" and failure["subject"] == "shop@r1"
        counted = json.loads(found["live/shop@r1/memory.json"])["workers"]
        assert counted and all(worker["ok"] for worker in counted)
        noted = {part["path"]: part.get("note") for part in manifest["parts"]}
        assert re.fullmatch(
            r"collapsed stacks merged from \d of \d workers",
            noted["live/shop@r1/profile.collapsed"],
        )
        assert "no application named nope" in refused(daemon, "nope")
        assert "shop has no release r9" in refused(daemon, "shop@r9")
        assert "is not a release: expected app@release" in refused(daemon, "shop@")
    finally:
        daemon.stop()

    # With the daemon stopped the bundle is made from its files, which
    # are left exactly as they were, and nothing records it.

    state = tmp_path / "state"
    before = {path.name: path.stat().st_mtime_ns for path in state.iterdir()}
    stored = (state / "muru.sqlite").read_bytes()

    stopped = tmp_path / "stopped.tar.gz"
    bundle(daemon, "--output", str(stopped))
    found = members(stopped.read_bytes())
    manifest = manifest_of(found)
    assert manifest["running"] is False
    assert manifest["recorded"] == "no event: nothing is running to record one"
    (release,) = json.loads(found["installation.json"])["releases"]
    assert release["state"] == "stopped" and release["workers"] == []
    types = [event["type"] for event in events_of(found)]
    assert "daemon.stopped" in types and types.count("debug.bundle") == 2

    # What the files hold is all there, and what only the running daemon
    # knows is listed with why it is not.

    assert json.loads(found["config/releases/shop@r1.json"])["release"]["state"] == "stopped"
    assert "host/host.json" in found and "host/shop@r1/packages.txt" in found
    assert "host/shop@r1/requirements.txt" in found
    assert b"worker.exited" in found["failures/events.jsonl"]
    assert "[app.shop" in found["config/effective.toml"].decode()
    assert 'secret = "***"' in found["config/daemon.toml"].decode()
    missing = {part["path"]: part["why"] for part in manifest["parts"] if not part["collected"]}

    for path in (
        "config/access-log-overrides.json",
        "config/routing-snapshot.json",
        "live",
        "figures/metrics.txt",
    ):
        assert missing[path] == "nothing is running to ask", path

    assert {path.name: path.stat().st_mtime_ns for path in state.iterdir()} == before
    assert (state / "muru.sqlite").read_bytes() == stored

    # Logs of the test's own making: an access log that starts three
    # hours ago, with another application's records and a client's
    # address in it, an archive the rotation left in the window and one
    # from before it, and the output of another application.

    logs = tmp_path / "logs"
    served = (logs / "access.log").read_text()
    (logs / "access.log").write_text(
        "".join(access_record(3 * 3600 + ago, "shop", "203.0.113.9", "/old") for ago in (20, 10, 0))
        + served
        + access_record(60, "other", "203.0.113.9", "/other?id=1")
        + access_record(30, "shop", "203.0.113.9", "/orders?id=7&token=abc")
    )

    with gzip.open(logs / "access.log.20260101-000000.gz", "wt") as archive:
        archive.write(access_record(1800, "shop", "198.51.100.4", "/archived?page=1"))
        archive.write(access_record(1700, "other", "198.51.100.4", "/archived"))

    before_window = logs / "access.log.20250101-000000"
    before_window.write_text(access_record(4 * 3600, "shop", "203.0.113.9", "/long-ago"))
    os.utime(before_window, (time.time() - 4 * 3600,) * 2)
    (logs / "apps" / "other").mkdir()
    (logs / "apps" / "other" / "r1.log").write_text(
        f"{stamp(40)} muru.worker[0]: saw 203.0.113.9 at /pay?card=4111\n"
    )

    # The window is cut from the log, and its archives come with it. A
    # socket that listens and gives no answer, as a daemon that hangs
    # would leave it, costs the time one question is given and what only
    # the daemon knows: the manifest says so, and the files are all read.

    whole = tmp_path / "whole.tar.gz"

    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(daemon.run_dir / "admin.sock"))
        listener.listen(64)
        bundle(daemon, "--output", str(whole))

    (daemon.run_dir / "admin.sock").unlink()
    found = members(whole.read_bytes())
    manifest = manifest_of(found)
    missing = {part["path"]: part["why"] for part in manifest["parts"] if not part["collected"]}
    assert manifest["running"] is False
    assert missing["live state"] == (
        f"what listens at {daemon.run_dir}/admin.sock gave no answer within 5 seconds"
    )
    assert "config/releases/shop@r1.json" in found and "events.jsonl" in found
    targets = [entry["target"] for entry in records_of(found["logs/access.log"])]
    assert targets == ["/catalogue?page=2", "/other?id=1", "/orders?id=7&token=abc"]
    assert "logs/access.log.20250101-000000" not in found
    packed = records_of(gzip.decompress(found["logs/access.log.20260101-000000.gz"]))
    assert [entry["target"] for entry in packed] == ["/archived?page=1", "/archived"]
    assert b"/pay?card=4111" in found["logs/apps/other/r1.log"]
    noted = {part["path"]: part.get("note") for part in manifest["parts"]}
    assert noted["logs/access.log.20260101-000000.gz"] == (
        "the whole archive, which was last written in the window"
    )

    # A bundle of one application leaves out the others' output and
    # records, and --scrub takes the clients' addresses and the query
    # strings out, the same address reading the same throughout.

    scrubbed = tmp_path / "scrubbed.tar.gz"
    planned = bundle(daemon, "shop", "--scrub", "--dry-run")
    assert "the records of shop alone, scrubbed: its size before that" in planned
    bundle(daemon, "shop", "--scrub", "--output", str(scrubbed))
    found = members(scrubbed.read_bytes())
    manifest = manifest_of(found)
    assert manifest["options"] == {"scrub": True, "memory": False, "profile_s": None}
    assert "--scrub" in manifest["left_out"][-1]
    assert "logs/apps/other/r1.log" not in found

    kept = records_of(found["logs/access.log"])
    assert [(entry["client"], entry["target"]) for entry in kept] == [
        ("127.0.0.1", "/catalogue?..."),
        ("<address2>", "/orders?..."),
    ]
    assert records_of(found["logs/access.log.20260101-000000"]) == [
        {
            "app": "shop",
            "client": "<address1>",
            "release": "r1",
            "status": 200,
            "target": "/archived?...",
            "time": packed[0]["time"],
        }
    ]
    noted = {part["path"]: part.get("note") for part in manifest["parts"]}
    assert noted["logs/access.log"] == (
        "the lines of the window, the records of shop alone, scrubbed"
    )

    for held in (b"203.0.113.9", b"198.51.100.4", b"token=abc", b"page="):
        holding = [path for path, content in found.items() if held in content]
        assert holding == [], f"{held!r} is in {holding}"


def test_bundle_of_a_development_run_and_of_a_serve(tmp_path: Path) -> None:
    runtime = support.short_dir("muru-rt-")
    server = start_run(runtime)

    try:
        server.get("/marker?x=1")
        assert server.wait_for_log("/marker?x=1"), server.log()
        written = tmp_path / "run.tar.gz"
        completed = muructl(
            "debug",
            "bundle",
            "--output",
            str(written),
            cwd=support.FIXTURES,
            env={"MURU_RUN_DIR": str(runtime)},
        )
        assert completed.returncode == 0, completed.stderr.decode()

        found = members(written.read_bytes())
        manifest = manifest_of(found)
        assert manifest["runs"] == "muructl run" and manifest["running"] is True
        assert manifest["recorded"] == "as a line of muructl run's output"
        assert server.wait_for_log("debug.bundle: a diagnostic bundle of the whole node was made")

        installation = json.loads(found["installation.json"])
        assert installation["locations"] is None
        (release,) = installation["releases"]
        assert release["app"] == "plain-wsgi" and len(release["workers"]) == 2

        # A run keeps the events its hosts raise, of which there may be
        # none yet.

        assert all(event["subject"] == "plain-wsgi@r1" for event in events_of(found))

        # A run writes no log files: the lines it keeps in memory are
        # asked for.

        assert b"worker 0 started as pid" in found["logs/apps/plain-wsgi/r1.log"]
        (served,) = records_of(found["logs/access.log"])
        assert served["target"] == "/marker?x=1"
        noted = {part["path"]: part.get("note") for part in manifest["parts"]}
        assert noted["logs/access.log"] == (
            "the lines of the window among those muructl run keeps in memory"
        )

        assert len(json.loads(found["live/plain-wsgi@r1/stacks-3.json"])["workers"]) == 2
        assert json.loads(found["host/host.json"])["disks"][0]["of"] == "run"
        assert "host/plain-wsgi@r1/inspect.json" in found
        assert found["host/plain-wsgi@r1/packages.txt"]
        assert b"muru_requests_total" in found["figures/metrics.txt"]
        assert not any(path.startswith("figures/scaling/") for path in found)

        # A run has no state store: its settings are what config show
        # --effective prints, and it has no daemon's file.

        effective = found["config/effective.toml"].decode()
        assert "#   plain-wsgi@r1: processes = 2 (command line)" in effective
        assert "config/daemon.toml" not in found
        missing = {part["path"]: part["why"] for part in manifest["parts"] if not part["collected"]}
        assert missing["config/in-force.json"] == "muructl run keeps no state store"
        assert manifest["configuration_changed"] is False
    finally:
        server.stop()

    serve = start_serve(tmp_path)

    # The same against muructl serve, which is found in its run
    # directory: the plan lists what would be asked of it.

    try:
        planned = muructl("debug", "bundle", "--dry-run", env={"MURU_RUN_DIR": str(serve.run_dir)})
        assert planned.returncode == 0, planned.stderr.decode()
        listed = planned.stdout.decode()

        for path in (
            "installation.json",
            "config/effective.toml",
            "logs/apps/plain-wsgi/r1.log",
            "logs/access.log",
            "live/plain-wsgi@r1/stacks-3.json",
            "figures/metrics.txt",
        ):
            assert re.search(
                rf"^  {re.escape(path)} +asked for when the bundle is made$", listed, re.M
            ), path
    finally:
        serve.stop()
