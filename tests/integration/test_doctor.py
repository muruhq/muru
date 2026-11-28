"""`muructl doctor` (issue 950, design specification section 11.10): the
sockets and files of an installation with what each is for, the checks of
one part against another, and the state store read on its own. The
command changes nothing, works with no daemon, and exits 1 when a check
fails."""

from __future__ import annotations

import json
import re
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


@pytest.fixture
def base() -> Iterator[Path]:
    """A directory for a daemon's state that is not under a temporary
    directory, where doctor says a state directory must not be: pytest's
    own is under /tmp on Linux."""

    parent = support.ROOT / "target" / "doctor-tests"
    parent.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(dir=parent))

    try:
        yield directory
    finally:
        shutil.rmtree(directory, ignore_errors=True)


def doctor(daemon: Daemon, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Run muructl doctor on the daemon's own locations, whether or not it runs."""

    muructl = support.muructl_path()
    assert muructl is not None

    return subprocess.run(
        [
            str(muructl),
            "doctor",
            *arguments,
            "--state-dir",
            str(daemon.base / "state"),
            "--run-dir",
            str(daemon.run_dir),
            "--log-dir",
            str(daemon.base / "logs"),
            "--cache-dir",
            str(daemon.base / "cache"),
            "--config",
            str(daemon.base / "muru.toml"),
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )


def report(daemon: Daemon, *arguments: str) -> tuple[int, dict[str, Any]]:
    """Doctor's exit code and its JSON document."""

    completed = doctor(daemon, *arguments, "--json")
    document: dict[str, Any] = json.loads(completed.stdout)
    return completed.returncode, document


def by_path(items: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    return {item["path"]: item for item in items}


def failed(document: dict[str, Any]) -> list[dict[str, Any]]:
    return [check for check in document["checks"] if not check["ok"]]


def test_doctor_lists_an_installation_and_fails_on_what_is_wrong(base: Path) -> None:
    daemon = start_daemon(base)
    run_dir = daemon.run_dir

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")

        # Everything is listed with what it is for, its owner and its
        # condition, and every check passes.

        code, document = report(daemon)
        assert (code, document["ok"]) == (0, True), document
        sockets = by_path(document["sockets"])
        pid = daemon.process.pid

        admin = sockets[str(run_dir / "admin.sock")]
        assert (admin["what"], admin["owner"], admin["condition"]) == (
            "admin API",
            f"daemon pid {pid}",
            "answering",
        )

        # A worker's socket is connected to, and names the worker's pid.
        # The host's control socket is not: a connection would take the
        # daemon's place on it.

        worker = sockets[str(run_dir / "h" / "1" / "worker-0.sock")]
        assert worker["what"] == "requests to shop@r1 worker 0"
        assert worker["condition"] == "answering" and worker["owner"].startswith("pid ")
        control = sockets[str(run_dir / "h" / "1" / "control.sock")]
        assert control["what"] == "daemon to host, shop@r1"
        assert control["condition"] == "owner alive" and control["owner"].startswith("host pid ")
        assert sockets[str(run_dir / "e" / "edge-1.sock")]["condition"] == "answering"

        files = {item["what"]: item for item in document["files"]}
        assert files["state store"]["condition"].startswith("schema 25, ")
        assert files["lock on the state directory"]["condition"] == f"held by pid {pid}"
        assert files["registry entry"]["condition"] == f"locked by pid {pid}"
        assert files["the host of shop@r1, for the next daemon"]["ok"]

        checks = {check["check"]: check for check in document["checks"]}
        assert sorted(checks) == [
            "budget",
            "disk",
            "edge",
            "locks",
            "paths",
            "registry",
            "releases",
            "store",
            "versions",
        ]
        assert f"the daemon (pid {pid}) holds its entry" in checks["registry"]["detail"]
        assert "the one the daemon last sent" in checks["edge"]["detail"]
        assert "the daemon and its edge run version " in checks["versions"]["detail"]
        assert "has room: state " in checks["disk"]["detail"]

        # Issue 961: a node with no memory limit, from the daemon's file
        # or its own cgroup, is warned of, which fails nothing. Where the
        # tests run under a cgroup's limit, that is the node's budget.

        budget = checks["budget"]
        assert budget["ok"], budget

        if budget.get("warning"):
            assert budget["detail"].startswith("the node has no memory limit, so every scale-out")
            assert "[daemon.budget]" in budget["detail"]
            assert budget["fix"] == "muructl daemon config set daemon.budget.memory SIZE"
            assert "\nwarning budget " in doctor(daemon).stdout
        else:
            assert "from the memory limit of the daemon's cgroup" in budget["detail"], budget

        # With a limit in the daemon's file there is nothing to warn of.

        (daemon.base / "muru.toml").write_text("[daemon.budget]\nmemory = '2GB'\n")
        daemon.muructl("daemon", "reload")
        deadline = time.monotonic() + 20

        while True:
            budget = {check["check"]: check for check in report(daemon)[1]["checks"]}["budget"]

            if "configuration file" in budget["detail"] or time.monotonic() > deadline:
                break

            time.sleep(0.5)

        assert budget == {
            "check": "budget",
            "ok": True,
            "detail": "the node's memory budget is 2.0GB, from the daemon's configuration file",
        }, budget

        # A daemon that runs says where its files are, so its run
        # directory alone finds it.

        found = subprocess.run(
            [str(support.muructl_path()), "doctor", "--json", "--run-dir", str(run_dir)],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        assert found.returncode == 0, found.stdout + found.stderr
        assert by_path(json.loads(found.stdout)["sockets"]) == sockets

        # The text has one line for each, and the same exit code.

        text = doctor(daemon)
        assert text.returncode == 0, text.stdout + text.stderr
        assert "SOCKETS\n" in text.stdout and "FILES\n" in text.stdout
        assert f"\nok      registry      the daemon (pid {pid}) holds its entry" in text.stdout
        assert the_daemon_still_serves(daemon)

        # A regular file in the run directory, which holds sockets only.

        stray = run_dir / "h" / "1" / "notes.txt"
        stray.write_text("left here\n")
        code, document = report(daemon)
        assert code == 1 and not document["ok"]
        assert [check["check"] for check in failed(document)] == ["paths"]
        assert f"{stray} is a regular file in the run directory" in failed(document)[0]["detail"]
        stray.unlink()

        # A socket nothing is behind, in a host's directory nothing owns.

        (run_dir / "h" / "97").mkdir()
        orphan = run_dir / "h" / "97" / "control.sock"

        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(orphan))

        code, document = report(daemon)
        stale = by_path(document["sockets"])[str(orphan)]
        assert code == 1
        assert (stale["what"], stale["ok"]) == ("(nothing owns it)", False)
        assert stale["condition"] == "STALE, its host has no record"
        assert "(nothing owns it)" in doctor(daemon).stdout
        orphan.unlink()
        (run_dir / "h" / "97").rmdir()

        # The fault of issue 948: the daemon runs and serves, and its
        # registry entry's files are gone. The check says so and names
        # what puts it right.

        entry = support.user_registry() / (base / "state" / "instance-id").read_text().strip()

        for name in ("lock", "instance.json"):
            (entry / name).unlink()

        code, document = report(daemon)
        assert code == 1
        registry = next(check for check in failed(document) if check["check"] == "registry")
        assert f"the daemon (pid {pid}) runs and has no live entry" in registry["detail"]
        assert registry["fix"] == "muructl daemon restart"
        assert "see muructl daemon restart" in doctor(daemon).stdout

        # The emptied entry is the test's own, and goes with it.

        entry.rmdir()
    finally:
        daemon.stop(shutdown=True)

    # With no daemon, the same command still answers, from the files.

    code, document = report(daemon)
    assert (code, document["ok"]) == (0, True), document
    assert document["sockets"] == []
    details = {check["check"]: check["detail"] for check in document["checks"]}
    assert details["locks"] == "no daemon runs, and nothing holds its locks"
    assert details["releases"].startswith("2 releases in the store, 0 with a host")


def test_doctor_gives_a_timeline_and_reads_what_a_bundle_holds(base: Path) -> None:
    daemon = start_daemon(base)
    bundle = base / "bundle.tar.gz"
    muructl = support.muructl_path()
    assert muructl is not None

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")
        pid = daemon.process.pid

        # The events, the daemon's processes and its log lines, in order.

        shown = doctor(daemon, "timeline", "--json")
        assert shown.returncode == 0, shown.stdout + shown.stderr
        timeline = json.loads(shown.stdout)["timeline"]
        assert [entry["time_unix_ms"] for entry in timeline] == sorted(
            entry["time_unix_ms"] for entry in timeline
        )
        texts = {(entry["source"], entry["text"]) for entry in timeline}
        assert ("event", "traffic.switched shop@r1: traffic moved to the release") in texts
        assert any(
            source == "process" and text.startswith("muru daemon ") and f"(pid {pid})" in text
            for source, text in texts
        ), texts
        assert any(
            source == "process" and text.startswith("muru host shop@r1") for source, text in texts
        )

        text = doctor(daemon, "timeline", "--limit", "3")
        assert text.returncode == 0 and len(text.stdout.splitlines()) == 3, text.stdout
        assert re.match(r"\d{4}-\d\d-\d\dT[\d:.]+Z  (event|log|process)  ", text.stdout), (
            text.stdout
        )

        # A bundle carries what doctor finds as the bundle is made.

        made = subprocess.run(
            [
                str(muructl),
                "debug",
                "bundle",
                "--output",
                str(bundle),
                "--socket",
                str(daemon.run_dir / "admin.sock"),
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
        assert made.returncode == 0, made.stdout + made.stderr
    finally:
        daemon.stop(shutdown=True)

    # It is read with no daemon, as it would be on another machine: the
    # sockets as they answered then, the checks as they passed then.

    read = subprocess.run(
        [str(muructl), "doctor", "--bundle", str(bundle), "--json"],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert read.returncode == 0, read.stdout + read.stderr
    held = json.loads(read.stdout)
    assert held["ok"] and held["bundle"] == str(bundle) and held["muru"]
    sockets = by_path(held["report"]["sockets"])
    assert sockets[str(daemon.run_dir / "admin.sock")]["condition"] == "answering"
    assert sockets[str(daemon.run_dir / "h" / "1" / "worker-0.sock")]["condition"] == "answering"
    assert {check["check"] for check in held["report"]["checks"]} >= {"registry", "edge", "disk"}
    assert held["store"]["tables"]["releases"] == 1 and held["store"]["ok"]

    text = subprocess.run(
        [str(muructl), "doctor", "--bundle", str(bundle)],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert text.returncode == 0, text.stdout + text.stderr
    assert text.stdout.startswith(f"bundle   {bundle}, made ")
    assert "\nSOCKETS\n" in text.stdout and "\nTABLES\n" in text.stdout

    # An archive with nothing of doctor's in it says so.

    refused = subprocess.run(
        [str(muructl), "doctor", "--bundle", str(bundle), "store"],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert refused.returncode != 0 and "takes no part to look into" in refused.stderr


def the_daemon_still_serves(daemon: Daemon) -> bool:
    """Doctor connected to the sockets it may and to none it may not: the
    daemon still has its host's control connection, so a deploy and a
    request both work."""

    daemon.deploy("shop", SHOP, "--processes", "1")
    return daemon.get("/").status == 200 and "release.failed" not in daemon.events()


def test_doctor_store_checks_the_store_dumps_its_schema_and_copies_it(base: Path) -> None:
    daemon = start_daemon(base)
    store = base / "state" / "muru.sqlite"

    try:
        daemon.deploy("shop", SHOP, "--processes", "1")

        # Read beside a running daemon: sound, at this version, every
        # rule kept.

        code, document = report(daemon, "store")
        assert (code, document["ok"]) == (0, True), document
        assert document["schema_version"] == document["expected_schema_version"] == 25
        assert document["tables"]["releases"] == 1 and document["tables"]["apps"] == 1
        assert document["bytes"] > 0
        names = [check["check"] for check in document["checks"]]
        assert names[:3] == ["integrity", "foreign keys", "schema"]
        assert names.count("rule") == 7

        text = doctor(daemon, "store")
        assert text.returncode == 0, text.stdout + text.stderr
        assert "TABLES\n" in text.stdout
        assert "ok      schema        version 25, as this muructl expects" in text.stdout

        schema = doctor(daemon, "store", "--dump-schema")
        assert schema.returncode == 0
        assert "CREATE TABLE releases (" in schema.stdout
        assert "CREATE TABLE release_numbers (last INTEGER NOT NULL);" in schema.stdout

        # A copy is a whole store, taken while the daemon runs, and is
        # never written over a file.

        copy = base / "copy.sqlite"
        copied = doctor(daemon, "store", "--copy", str(copy))
        assert copied.returncode == 0, copied.stdout + copied.stderr
        assert f"copied {store} to {copy}" in copied.stdout

        with sqlite3.connect(copy) as connection:
            assert connection.execute("SELECT app, id, number FROM releases").fetchall() == [
                ("shop", "r1", 1)
            ]

        # Issue 928: the copy leaves out the release's preview secret,
        # in its rows and in the bytes of the file, and a schema dump
        # prints no rows at all.

        secret = daemon.preview("shop@r1").rpartition(".")[2]
        assert secret.encode() not in copy.read_bytes()
        assert secret not in schema.stdout and (copy.parent / "copy.sqlite-wal").exists() is False

        with sqlite3.connect(copy) as connection:
            assert connection.execute("SELECT preview_secret FROM releases").fetchall() == [(None,)]

        again = doctor(daemon, "store", "--copy", str(copy))
        assert again.returncode != 0
        assert "a copy is never written over a file" in again.stderr
    finally:
        daemon.stop(shutdown=True)

    # A store that breaks a rule of Muru's own: the application serves a
    # release that is not there.

    with sqlite3.connect(store) as connection:
        connection.execute("UPDATE apps SET current_release = 'r9'")

    code, document = report(daemon, "store")
    assert code == 1 and not document["ok"]
    assert [check["detail"] for check in failed(document)] == [
        "each application's serving and previous release exist: application shop serves r9, "
        "which is no release"
    ]

    # With no store there is nothing to check, which is said.

    store.unlink()
    missing = doctor(daemon, "store")
    assert missing.returncode != 0
    assert f"there is no state store at {store}" in missing.stderr
