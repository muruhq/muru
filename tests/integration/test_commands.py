"""Commands an application declares (design specification section 9.12):
listed with their inputs and last runs, run by name with inputs checked,
secret inputs kept out of events, callables called, and exclusive
commands and timeouts enforced."""

from __future__ import annotations

import json
import subprocess
import textwrap
import time
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

PYPROJECT = """
[project]
name = "ops"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1

[tool.muru.commands.greet]
description = "Say hello"
run = [
    "python", "-c",
    "import os, sys; print('hello', sys.argv[1], os.environ['SECRET_WORD'])",
    "{who}",
]
env = { SECRET_WORD = "{word}" }
inputs.who = { required = true, pattern = "[a-z]+", description = "Whom to greet" }
inputs.word = { secret = true }

[tool.muru.commands.backup]
call = "ops:backup"
inputs.target = { default = "s3://backups" }
inputs.label = {}

[tool.muru.commands.slow]
run = ["python", "-c", "import time; print('up', flush=True); time.sleep(60)"]
exclusive = true
timeout = "2s"

[tool.muru.commands.hold]
run = ["python", "-c", "import time; print('up', flush=True); time.sleep(60)"]

[tool.muru.commands.long]
run = [
    "python", "-c",
    "import sys, time; print('first', flush=True); time.sleep(6); print('last'); sys.exit(3)",
]
"""

APP = """
def application(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""

OPS = """
import os


def backup(target, label="none"):
    print("backing up to", target, "as", label, "for", os.environ["MURU_COMMAND"])
    return 0 if target.startswith("s3://") else 4
"""


def deployed(tmp_path: Path) -> Daemon:
    project = tmp_path / "ops"
    project.mkdir()
    (project / "pyproject.toml").write_text(textwrap.dedent(PYPROJECT))
    (project / "app.py").write_text(APP)
    (project / "ops.py").write_text(OPS)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(
        tmp_path / "daemon",
        config_extra="[secrets.store.local]\nkind = 'file'\n",
    )
    assert daemon.deploy("ops", project) == "muru: release ops@r1 is ready and serving"
    return daemon


def test_declared_commands_run_with_their_inputs(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)

    try:
        listed = daemon.muructl("command", "list", "ops").stdout
        assert "greet: Say hello" in listed, listed
        assert "input who (required, matching [a-z]+): Whom to greet" in listed, listed
        assert "input word (secret)" in listed, listed
        assert "call: ops:backup" in listed, listed
        assert "timeout 2s, one run at a time" in listed, listed

        ran = daemon.muructl(
            "command", "run", "ops", "greet", "--input", "who=bob", "--input", "word=swordfish"
        )
        assert ran.stdout.strip() == "hello bob swordfish", ran

        # The secret's value is in no event.

        events = daemon.muructl("events").stdout
        assert "greet: started" in events and "who=bob, word=***" in events, events
        assert "swordfish" not in events

        # Inputs are checked before anything runs.

        missing = daemon.muructl("command", "run", "ops", "greet", check=False)
        assert missing.returncode != 0
        assert "greet needs the input who" in missing.stderr, missing.stderr

        bad = daemon.muructl("command", "run", "ops", "greet", "--input", "who=Bob", check=False)
        assert '"Bob" does not match [a-z]+' in bad.stderr, bad.stderr

        unknown = daemon.muructl("command", "run", "ops", "nothing", check=False)
        assert (
            "ops@r1 has no command named nothing; it declares backup, greet, hold, long, slow"
            in (unknown.stderr)
        )

        # A value can come from standard input, or from a secret store
        # without passing through the caller at all.

        piped = daemon.muructl(
            "command",
            "run",
            "ops",
            "greet",
            "--input",
            "who=amy",
            "--input-stdin",
            "word",
            stdin="from-stdin\n",
        )
        assert piped.stdout.strip() == "hello amy from-stdin", piped

        daemon.muructl("secret", "set", "local", "word", stdin="from-the-store")
        stored = daemon.muructl(
            "command",
            "run",
            "ops",
            "greet",
            "--input",
            "who=sam",
            "--secret-input",
            "word=local/word",
        )
        assert stored.stdout.strip() == "hello sam from-the-store", stored

        # The list says how each command last went.

        listed = daemon.muructl("command", "list", "ops", "--json").stdout
        commands = {command["name"]: command for command in json.loads(listed)["commands"]}
        assert commands["greet"]["last_run"]["ok"] is True, commands
        assert commands["greet"]["last_run"]["release"] == "r1"
        assert commands["backup"].get("last_run") is None
    finally:
        daemon.stop()


def test_a_callable_is_called_with_the_inputs(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)

    try:
        ran = daemon.muructl("command", "run", "ops", "backup")
        assert ran.stdout.strip() == "backing up to s3://backups as none for backup", ran

        ran = daemon.muructl(
            "command",
            "run",
            "ops",
            "backup",
            "--input",
            "target=/tmp/x",
            "--input",
            "label=b1",
            check=False,
        )
        assert ran.returncode == 4, ran
        assert "backing up to /tmp/x as b1" in ran.stdout, ran
        assert "muru: the command exited with status 4" in ran.stderr, ran
    finally:
        daemon.stop()


def test_exclusive_commands_run_one_at_a_time_and_time_out(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)
    muructl = support.muructl_path()
    assert muructl is not None
    socket = str(daemon.run_dir / "admin.sock")

    try:
        first = subprocess.Popen(
            [str(muructl), "command", "run", "ops", "slow", "--socket", socket],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert first.stdout is not None
        assert first.stdout.readline().strip() == "up"

        second = daemon.muructl("command", "run", "ops", "slow", check=False)
        assert "slow runs one at a time for ops, and run " in second.stderr, second

        # The first runs past its timeout and is stopped; then the
        # command may run again.

        _, stderr = first.communicate(timeout=30)
        assert first.returncode == 143, (first.returncode, stderr)
        assert "stopped because it ran past its timeout of 2s" in stderr, stderr

        deadline = time.monotonic() + 10

        while True:
            again = daemon.muructl("command", "run", "ops", "slow", "--json", check=False)

            if "one at a time" not in again.stdout:
                break

            assert time.monotonic() < deadline, again
            time.sleep(0.2)

        assert json.loads(again.stdout.splitlines()[0])["line"] == "up", again.stdout

        # The daemon shutting down stops a command running under it, and
        # its caller hears how it ended.

        held = subprocess.Popen(
            [str(muructl), "command", "run", "ops", "hold", "--socket", socket],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert held.stdout is not None
        assert held.stdout.readline().strip() == "up"
        daemon.stop()
        _, stderr = held.communicate(timeout=30)
        assert held.returncode == 143, (held.returncode, stderr)
        assert "stopped because the daemon shut down" in stderr, stderr
    finally:
        daemon.stop()


def test_a_detached_run_outlives_its_caller_and_a_daemon_restart(tmp_path: Path) -> None:
    daemon = deployed(tmp_path)

    try:
        started = daemon.muructl("command", "run", "ops", "long", "--detach").stdout
        run = int(started.split("started run ")[1].split(";")[0])
        listed = json.loads(daemon.muructl("command", "runs", "ops", "long", "--json").stdout)
        assert [(r["run"], r["under_way"], r["detached"]) for r in listed["runs"]] == [
            (run, True, True)
        ]

        # The daemon detaches and a new one takes the run over while it
        # is still under way.

        daemon.stop(shutdown=False)
        daemon = start_daemon(
            tmp_path / "daemon",
            run_dir=daemon.run_dir,
            listeners=daemon.listeners,
            config_extra="[secrets.store.local]\nkind = 'file'\n",
        )
        status = json.loads(daemon.muructl("command", "status", str(run), "--json").stdout)
        assert status["under_way"], status

        # Its end is recorded with its exit status, and its output is in
        # its own file with the time on every line.

        deadline = time.monotonic() + 30

        while status["under_way"]:
            assert time.monotonic() < deadline, status
            time.sleep(0.5)
            status = json.loads(daemon.muructl("command", "status", str(run), "--json").stdout)

        assert (status["exit_code"], status["ok"], status["trigger"]) == (3, False, "manual")
        assert status["message"].startswith("exited with status 3 after "), status
        output = daemon.muructl("command", "logs", str(run)).stdout.splitlines()
        assert [line.split(" ", 1)[1] for line in output[:2]] == [
            "[command long] first",
            "[command long] last",
        ], output
        assert all(line[:4].isdigit() and line[23] == "Z" for line in output), output
        assert "[command long] first" in daemon.muructl("logs", "ops").stdout
    finally:
        daemon.stop()


SCHEDULED = """
[project]
name = "ops"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1

[tool.muru.commands.warm]
run = ["python", "-c", "print('warmed')"]
timeout = "1m"
schedule = { at_boot = true }

[tool.muru.commands.nightly]
run = ["python", "-c", "import time; print('up', flush=True); time.sleep(120)"]
timeout = "3m"
schedule = { cron = "* * * * *" }
"""


def test_schedules_run_at_the_daemons_start_and_skip_a_run_due_while_one_is_under_way(
    tmp_path: Path,
) -> None:
    project = tmp_path / "ops"
    project.mkdir()
    (project / "pyproject.toml").write_text(textwrap.dedent(SCHEDULED))
    (project / "app.py").write_text(APP)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    def runs(name: str) -> list[dict]:
        listed = daemon.muructl("command", "runs", "ops", name, "--json").stdout
        return json.loads(listed).get("runs", [])

    def wait_for(what: str, condition, timeout: float = 90) -> None:  # type: ignore[no-untyped-def]
        deadline = time.monotonic() + timeout

        while not condition():
            assert time.monotonic() < deadline, what
            time.sleep(0.5)

    try:
        assert daemon.deploy("ops", project) == "muru: release ops@r1 is ready and serving"
        listed = daemon.muructl("command", "list", "ops").stdout
        assert "schedule: when the daemon starts" in listed, listed
        assert "schedule: * * * * *; next " in listed, listed

        # A daemon that starts from nothing, after a shutdown, runs warm
        # with the trigger schedule.

        daemon.stop(shutdown=True)
        daemon = start_daemon(tmp_path / "daemon", run_dir=daemon.run_dir, port=daemon.port)
        wait_for("warm to run", lambda: runs("warm") and not runs("warm")[0]["under_way"], 30)
        warm = runs("warm")[0]
        assert (warm["trigger"], warm["requested_by"], warm["ok"]) == ("schedule", "daemon", True)

        # A run of nightly by hand is under way when the schedule is due,
        # so the scheduled run is skipped, naming it.

        started = daemon.muructl("command", "run", "ops", "nightly", "--detach").stdout
        held = int(started.split("started run ")[1].split(";")[0])
        wait_for(
            "the scheduled run to be skipped",
            lambda: f"since run {held} is under way" in daemon.muructl("events").stdout,
        )
        assert [run["run"] for run in runs("nightly")] == [held]
        daemon.muructl("command", "stop", str(held))

        # A daemon that takes over from one that detached does not run it.

        daemon.stop(shutdown=False)
        daemon = start_daemon(tmp_path / "daemon", run_dir=daemon.run_dir, port=daemon.port)
        time.sleep(2)
        assert len(runs("warm")) == 1
    finally:
        daemon.stop()
