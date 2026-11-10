"""muructl debug bundle (design specification section 11.9): the archive
of a system for triage, against a daemon that runs and one that has
stopped, a development run and muructl serve, with its manifest, where
it is written and how its making is recorded."""

from __future__ import annotations

import io
import json
import os
import re
import stat
import subprocess
import tarfile
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

    try:
        deployed = daemon.deploy(
            "shop",
            SHOP,
            "--processes",
            "2",
            "--env",
            "API_TOKEN=abc123",
            "--env",
            "DATABASE_URL=postgres://shop:s3cret@db.example/shop",
        )
        assert deployed == "muru: release shop@r1 is ready and serving"

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

        for secret in (b"abc123", b"s3cret", b"hunter2"):
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
        assert sorted(missing) == ["config/logger-levels.json", "config/routing-snapshots"]
        assert "only the routing snapshot in force is kept" in missing["config/routing-snapshots"]

        # Every event of the window is there in full, and the making of
        # the bundle is an event of its own.

        types = [event["type"] for event in events_of(found)]
        assert "release.ready" in types and "traffic.switched" in types
        recorded = daemon.muructl("events").stdout
        assert "debug.bundle" in recorded
        assert "a diagnostic bundle of the whole node was made, reaching back 1h" in recorded

        # A file that exists is left alone, and - is standard output.

        assert "exists: name another file with --output, or remove it" in refused(
            daemon, "--output", str(written)
        )
        piped = muructl(
            "debug",
            "bundle",
            "--since",
            "10m",
            "--output",
            "-",
            "--socket",
            str(daemon.run_dir / "admin.sock"),
        )
        assert piped.returncode == 0, piped.stderr.decode()
        assert piped.stderr.decode().startswith("muru: wrote the bundle to standard output: ")
        window = manifest_of(members(piped.stdout))["window"]
        assert window["until_unix_ms"] - window["since_unix_ms"] == 600_000

        # A target limits the bundle, and one that names nothing is
        # refused.

        target = tmp_path / "target.tar.gz"
        bundle(daemon, "shop@r1", "--output", str(target))
        found = members(target.read_bytes())
        assert manifest_of(found)["target"] == "shop@r1"
        assert "config/releases/shop@r1.json" in found
        assert json.loads(found["installation.json"])["releases"][0]["id"] == "r1"
        assert "a diagnostic bundle of shop@r1 was made" in daemon.muructl("events").stdout
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
    assert "daemon.stopped" in types and types.count("debug.bundle") == 3

    # What the files hold is all there, and what only the running daemon
    # knows is listed with why it is not.

    assert json.loads(found["config/releases/shop@r1.json"])["release"]["state"] == "stopped"
    assert "[app.shop" in found["config/effective.toml"].decode()
    assert 'secret = "***"' in found["config/daemon.toml"].decode()
    missing = {part["path"]: part["why"] for part in manifest["parts"] if not part["collected"]}
    assert missing["config/access-log-overrides.json"] == "nothing is running to ask"
    assert missing["config/routing-snapshot.json"] == "nothing is running to ask"

    assert {path.name: path.stat().st_mtime_ns for path in state.iterdir()} == before
    assert (state / "muru.sqlite").read_bytes() == stored


def test_bundle_of_a_development_run_and_of_a_serve(tmp_path: Path) -> None:
    runtime = support.short_dir("muru-rt-")
    server = start_run(runtime)

    try:
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

        # A run keeps the events its hosts raise, and none has been yet.

        assert events_of(found) == []

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

    try:
        piped = muructl(
            "debug", "bundle", "--output", "-", env={"MURU_RUN_DIR": str(serve.run_dir)}
        )
        assert piped.returncode == 0, piped.stderr.decode()
        found = members(piped.stdout)
        assert manifest_of(found)["runs"] == "muructl serve"
        assert json.loads(found["installation.json"])["releases"][0]["app"] == "plain-wsgi"
    finally:
        serve.stop()
