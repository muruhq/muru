"""muructl serve as a plain process (section 13.9 of the specification).

The behaviour a container sees, the settings in layers, the admin port's
probes and metrics, one output stream, the run directory and the runtime
library found and never fetched, is tested here without a container
runtime; running as a container's process 1 is in test_containers.py.
"""

from __future__ import annotations

import http.client
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import tomllib
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest
from support import server as support

TIMED = r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z "


@dataclass
class Serve:
    """A running muructl serve, its output read as it arrives."""

    process: subprocess.Popen[str]
    port: int
    admin_port: int
    run_dir: Path
    stdout: list[str] = field(default_factory=list)
    stderr: list[str] = field(default_factory=list)

    def get(
        self, path: str, port: int | None = None, headers: dict[str, str] | None = None
    ) -> tuple[int, str]:
        connection = http.client.HTTPConnection("127.0.0.1", port or self.port, timeout=10)

        try:
            connection.request("GET", path, headers=headers or {})
            response = connection.getresponse()
            return response.status, response.read().decode()
        finally:
            connection.close()

    def admin(self, path: str) -> tuple[int, str]:
        return self.get(path, self.admin_port)

    def output(self) -> str:
        return "".join(self.stdout)

    def wait_for(self, text: str, timeout: float = 10.0) -> None:
        deadline = time.monotonic() + timeout

        while text not in self.output():
            assert time.monotonic() < deadline, (
                f"{text!r} not in:\n{self.output()}{''.join(self.stderr)}"
            )
            time.sleep(0.05)

    def stop(self) -> int:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)

        try:
            return self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return self.process.wait()


def start(
    tmp_path: Path,
    *args: str,
    env: dict[str, str] | None = None,
    app: str | None = "plain_wsgi:application",
    wait: bool = True,
    short: bool = False,
    wrap: Callable[[list[str]], list[str]] | None = None,
) -> Serve:
    """Start muructl serve; `short` starts it as `muructl serve` alone, as a
    container's command line does, with every setting in its variable, and
    `wrap` runs its command line through another, which execs it."""

    muructl = support.muructl_path()
    if muructl is None:
        support.unavailable("muructl is not built (cargo build -p muructl)")
    if not support.extension_available():
        support.unavailable("the muru extension is not built into this interpreter (just dev)")

    port = support.free_port()
    admin_port = support.free_port()

    run_dir = support.short_dir("muru-serve-")

    # The test's own flags stand in for these defaults.

    defaults = {
        "--chdir": str(support.FIXTURES),
        "--listen": f"127.0.0.1:{port}",
        "--admin-listen": f"127.0.0.1:{admin_port}",
        "--run-dir": str(run_dir),
        "--python": sys.executable,
        "--processes": "1",
    }
    options = [
        item for flag, value in defaults.items() if flag not in args for item in (flag, value)
    ]

    if "--run-dir" in args:
        run_dir = Path(args[args.index("--run-dir") + 1])

    command = [str(muructl), "serve", *([app] if app is not None else []), *options, *args]
    variables = {**os.environ, **(env or {})}

    if short:
        command = ["muructl", "serve"]
        variables.update(
            {
                f"MURU_SERVE_{flag[2:].replace('-', '_').upper()}": value
                for flag, value in defaults.items()
            }
        )
        variables["MURU_SERVE_ENTRYPOINT"] = app or ""

    process = subprocess.Popen(
        wrap(command) if wrap else command,
        executable=None if wrap else str(muructl),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=variables,
    )
    serve = Serve(process, port, admin_port, run_dir)

    def read(stream: Iterator[str], into: list[str]) -> None:
        for line in stream:
            into.append(line)

    assert process.stdout is not None and process.stderr is not None
    threading.Thread(target=read, args=(process.stdout, serve.stdout), daemon=True).start()
    threading.Thread(target=read, args=(process.stderr, serve.stderr), daemon=True).start()

    if wait:
        try:
            support.wait_for_port(port, process)
        except RuntimeError as error:
            serve.stop()
            raise RuntimeError(f"{error}\n{serve.output()}{''.join(serve.stderr)}") from error

        # The listener accepts before the workers are ready, as a platform
        # expects, so a test that reads the admin port waits for readiness.

        if "--admin-listen" not in args:
            deadline = time.monotonic() + 30

            while serve.admin("/readyz")[0] != 200:
                assert time.monotonic() < deadline, f"never ready:\n{serve.output()}"
                time.sleep(0.05)

    return serve


def test_started_with_a_short_command_line_its_title_names_the_application(
    tmp_path: Path,
) -> None:
    """Started as `muructl serve` alone, the process's title runs on past its
    arguments, over the environment, which its workers still have (issue
    606)."""

    serve = start(tmp_path, short=True, env={"MURU_TEST_KEPT": "kept"})

    try:
        title = subprocess.run(
            ["ps", "-o", "command=", "-p", str(serve.process.pid)],
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()
        assert title == "muru serve plain-wsgi", title

        status, body = serve.get("/env?name=MURU_TEST_KEPT")
        assert status == 200 and json.loads(body)["value"] == "kept", body
    finally:
        serve.stop()


def test_a_named_group_and_a_mount_are_served_and_ready_counts_the_group(
    tmp_path: Path,
) -> None:
    """A project's named group and mount work under serve: a request for the
    mount's path reaches the group's worker, /status lists both groups, and
    /readyz counts each required group's workers, so the reports group with
    one process holds it back at --min-ready 2 (issue 620)."""

    mounted = support.ROOT / "tests" / "fixtures" / "projects" / "mounted"
    serve = start(
        tmp_path,
        "--chdir",
        str(mounted),
        "--processes",
        "2",
        "--min-ready",
        "2",
        "--admin-status",
        app=str(mounted),
        wait=False,
    )

    try:
        support.wait_for_port(serve.port, serve.process)

        def groups() -> dict[str, Any]:
            status, body = serve.admin("/status")
            return json.loads(body).get("groups", {}) if status == 200 else {}

        deadline = time.monotonic() + 30

        while {name: group["ready"] for name, group in groups().items()} != {
            "default": 2,
            "reports": 1,
        }:
            assert time.monotonic() < deadline, (groups(), serve.output())
            time.sleep(0.05)

        status, body = serve.get("/reports/daily")
        assert status == 200, body
        answered = json.loads(body)
        assert (answered["mount"], answered["group"]) == ("reports", "reports"), answered
        assert json.loads(serve.get("/")[1])["group"] == "default"

        status, reason = serve.admin("/readyz")
        assert status == 503 and "group reports has 1 of 2 workers ready" in reason, reason
    finally:
        serve.stop()


def test_serves_with_probes_and_metrics_on_the_admin_port(tmp_path: Path) -> None:
    serve = start(tmp_path)

    try:
        assert serve.get("/")[0] == 200
        assert serve.admin("/livez") == (200, "ok\n")
        assert serve.admin("/startupz") == (200, "ok\n")
        assert serve.admin("/readyz") == (200, "ok\n")
        status, metrics = serve.admin("/metrics")
        assert status == 200
        assert "muru_requests_total" in metrics

        # A request holding one of the worker's four slots shows in the
        # utilisation, sampled every second.

        sleeper = threading.Thread(target=serve.get, args=("/sleep?seconds=2.5",))
        sleeper.start()
        time.sleep(2)
        metrics = serve.admin("/metrics")[1]
        sleeper.join()
        line = next(
            line for line in metrics.splitlines() if line.startswith("muru_capacity_utilisation{")
        )
        assert 'group="default"' in line, line
        assert 0 < float(line.rsplit(" ", 1)[1]) <= 0.25, line

        # /status is opt-in, and the probes are not on the application's port.

        assert serve.admin("/status")[0] == 404
        assert serve.get("/readyz")[1] != "ok\n"
    finally:
        assert serve.stop() == 0


def test_sigterm_fails_readiness_and_keeps_accepting_for_the_delay(tmp_path: Path) -> None:
    serve = start(tmp_path, "--shutdown-delay", "2s", "--shutdown-budget", "6s")

    try:
        assert serve.admin("/readyz") == (200, "ok\n")
        signalled = time.monotonic()
        serve.process.send_signal(signal.SIGTERM)

        # Readiness fails at once, while the listener still serves, with
        # every connection closing after its response.

        deadline = time.monotonic() + 1

        while serve.admin("/readyz")[0] == 200:
            assert time.monotonic() < deadline, "readiness still passing after SIGTERM"
            time.sleep(0.05)

        connection = http.client.HTTPConnection("127.0.0.1", serve.port, timeout=10)

        try:
            connection.request("GET", "/")
            response = connection.getresponse()
            response.read()
            assert response.status == 200
            assert response.getheader("Connection") == "close"
        finally:
            connection.close()

        assert serve.process.wait(timeout=10) == 0
        assert 2 <= time.monotonic() - signalled < 6
        assert (
            "stopping: readiness now fails; serving for the shutdown delay of 2s, "
            "then draining for up to 6s" in serve.output()
        ), serve.output()
        assert re.search(
            r"stopped accepting connections; \d+ still open, draining", serve.output()
        ), serve.output()
    finally:
        serve.stop()


def test_everything_goes_to_standard_output_without_times(tmp_path: Path) -> None:
    serve = start(tmp_path)

    try:
        serve.get("/")
        serve.wait_for(" 200 GET / ")
    finally:
        assert serve.stop() == 0

    lines = serve.output().splitlines()
    assert serve.stderr == []
    assert any(line.startswith("muru.host[") for line in lines), lines
    assert any(line.startswith("muru: serving plain_wsgi:application") for line in lines), lines

    # The access line starts with the request's full UTC time, since
    # standard output is a pipe here; the other lines have none.

    access = next(line for line in lines if " 200 GET / " in line)
    assert re.match(TIMED, access), access
    assert not any(re.match(TIMED, line) for line in lines if line is not access)


def test_settings_come_from_the_environment(tmp_path: Path) -> None:
    serve = start(
        tmp_path,
        env={
            "MURU_SERVE_ADMIN_STATUS": "true",
            "MURU_SERVE_THREADS": "2",
            "MURU_SERVE_NAME": "orders",
        },
    )

    try:
        status, body = serve.admin("/status")
        assert status == 200, body
        report = json.loads(body)
        assert report["state"] == "serving"
        assert report["settings"]["threads"] == 2
        assert report["groups"]["default"]["ready"] == 1
        _status, metrics = serve.admin("/metrics")
        assert 'app="orders"' in metrics, metrics
    finally:
        assert serve.stop() == 0


def test_a_named_proxy_has_the_header_types_of_the_built_in_trust(tmp_path: Path) -> None:
    # Naming loopback as a proxy applies X-Forwarded-Prefix as the trust
    # serve has without one does, rather than the four types of [proxy].

    serve = start(tmp_path, "--trusted-proxy", "loopback")
    headers = {"X-Forwarded-For": "203.0.113.5", "X-Forwarded-Prefix": "/shop"}

    try:
        status, body = serve.get("/info", headers=headers)
        assert status == 200, body
        environ = json.loads(body)["environ"]
        assert environ["REMOTE_ADDR"] == "203.0.113.5"
        assert environ["SCRIPT_NAME"] == "/shop"
    finally:
        assert serve.stop() == 0


def test_print_config_names_where_each_setting_came_from(tmp_path: Path) -> None:
    # A project settings file named in place of the one beside the code,
    # as a deployment mounts one, with serve's settings under [serve].

    config = tmp_path / "settings.toml"
    config.write_text(
        "entrypoint = 'plain_wsgi:application'\nsync_threads = 2\n"
        "\n[serve]\naccess_log = 'errors'\nthreads = 3\n\n[run]\nport = 9\n"
    )
    serve = start(
        tmp_path,
        "--print-config",
        env={"MURU_SERVE_CONFIG": str(config), "MURU_SERVE_THREADS": "5"},
        app=None,
        wait=False,
    )
    assert serve.process.wait(timeout=30) == 0
    time.sleep(0.2)
    output = serve.output()

    def source(key: str) -> str:
        lines = [line for line in output.splitlines() if line.startswith(f"{key} = ")]
        assert len(lines) == 1, output
        return lines[0].split("# ", 1)[1]

    assert output.startswith(
        "# The settings this muructl serve would use, and where each came from.\n"
        f"# The project's settings are read from {config}.\n"
    ), output
    assert source("entrypoint") == str(config)
    assert source("sync_threads") == str(config)
    assert source("threads") == "MURU_SERVE_THREADS"
    assert source("processes") == "command line"
    assert source("access_log") == f"[serve] in {config}"
    assert source("admin_listen") == "command line"
    assert source("slow") == "default"
    assert "# The run table is muructl run's, and is not read here." in output

    # It reads as the project's file does, so it can be saved as one.

    document = tomllib.loads(output)
    assert document["threads"] == 5 and document["sync_threads"] == 2
    assert document["serve"]["access_log"] == "errors"
    assert document["serve"]["compression"] is True


def test_a_named_settings_file_is_used_in_place_of_the_projects(tmp_path: Path) -> None:
    # Section 13.9: --config names a project settings file, and the one
    # beside the code is not read at all. Paths in it are the project's.

    muructl = support.muructl_path()
    assert muructl is not None
    project = tmp_path / "shop"
    project.mkdir()
    (project / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    (project / "public").mkdir()
    (project / "public" / "hello.txt").write_text("from the project\n")
    (project / "pyproject.toml").write_text(
        "[tool.muru]\nentrypoint = 'missing:application'\n"
        "[tool.muru.commands.only-in-project]\nrun = ['true']\n"
    )
    mounted = tmp_path / "mounted"
    mounted.mkdir()
    settings = mounted / "settings.toml"
    settings.write_text(
        "entrypoint = 'app:application'\nenv = { GREETING = 'mounted' }\n"
        "\n[[static]]\npath = '/public'\ndir = 'public'\n"
        "\n[commands.greet]\nrun = ['python', '-c', 'import os; print(os.environ[\"GREETING\"])']\n"
        "\n[serve]\naccess_log = 'off'\n"
    )
    serve = start(tmp_path, "--chdir", str(project), "--config", str(settings), app=None)

    def run(*arguments: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(muructl), *arguments],
            capture_output=True,
            text=True,
            timeout=60,
            cwd=tmp_path,
            stdin=subprocess.DEVNULL,
            env={**os.environ, **env},
        )

    try:
        assert serve.get("/info")[0] == 200
        assert serve.get("/public/hello.txt") == (200, "from the project\n")

        # The serve's commands are those of the file it was given.

        listed = run("command", "list", MURU_RUN_DIR=str(serve.run_dir))
        assert "greet" in listed.stdout and "only-in-project" not in listed.stdout, listed
        ran = run("command", "run", "greet", MURU_RUN_DIR=str(serve.run_dir))
        assert (ran.returncode, ran.stdout) == (0, "mounted\n"), ran.stderr
    finally:
        assert serve.stop() == 0

    # With nothing serving, as in a Job, a command reads the same file
    # from the variable the Deployment sets, or from the flag.

    empty = support.short_dir("muru-none-")
    where = ["--chdir", str(project), "--python", sys.executable]
    own = run(
        "command", "run", "greet", *where, MURU_RUN_DIR=str(empty), MURU_SERVE_CONFIG=str(settings)
    )
    assert (own.returncode, own.stdout) == (0, "mounted\n"), own.stderr
    flagged = run("command", "list", "--config", str(settings), *where, MURU_RUN_DIR=str(empty))
    assert "greet" in flagged.stdout and "only-in-project" not in flagged.stdout, flagged
    beside = run("command", "list", *where, MURU_RUN_DIR=str(empty))
    assert "only-in-project" in beside.stdout, beside.stdout + beside.stderr


def test_json_lines_carry_their_kind_time_and_pod(tmp_path: Path) -> None:
    serve = start(
        tmp_path, "--log-format", "json", env={"POD_NAME": "shop-1", "POD_NAMESPACE": "prod"}
    )

    try:
        serve.get("/")
        serve.get("/error")
        serve.wait_for("RuntimeError: deliberate failure")
    finally:
        assert serve.stop() == 0

    records = [json.loads(line) for line in serve.output().splitlines()]
    kinds = {record["kind"] for record in records}
    assert {"muru", "app", "access", "serve.stopping", "serve.draining"} <= kinds, kinds
    assert all(record["time"].endswith("Z") for record in records)
    assert all((record["pod"], record["namespace"]) == ("shop-1", "prod") for record in records)
    host = next(record for record in records if record["kind"] == "app")
    assert host["source"].startswith("muru.host["), host

    # The exception the application did not catch is logged on the muru
    # logger at ERROR, as one record with its traceback and the id of the
    # request, which joins it to that request's access line.

    failed = next(record for record in records if record.get("logger") == "muru")
    assert failed["source"] == "muru.worker[0]", failed
    assert (failed["level"], failed["message"]) == ("ERROR", "the application raised an exception")
    assert failed["exception"].startswith("Traceback (most recent call last):\n")
    assert failed["exception"].endswith("RuntimeError: deliberate failure")
    access = next(
        record
        for record in records
        if record["kind"] == "access" and record.get("target") == "/error"
    )
    assert failed["request_id"] == access["request_id"], (failed, access)


def test_python_records_in_json_are_the_records_themselves(tmp_path: Path) -> None:
    serve = start(tmp_path, "--log-format", "json", app="logging_wsgi:application")

    try:
        serve.get("/fail")
        serve.wait_for('"level":"ERROR"')
    finally:
        assert serve.stop() == 0

    records = [json.loads(line) for line in serve.output().splitlines()]
    failed = next(record for record in records if record.get("message") == "failed for /fail")
    access = next(record for record in records if record["kind"] == "access")
    assert failed["kind"] == "app" and failed["logger"] == "demo", failed
    assert failed["exception"].endswith("ValueError: boom"), failed
    assert failed["request_id"] == access["request_id"]


def test_probe_answers_over_the_run_directory(tmp_path: Path) -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    serve = start(tmp_path, "--admin-listen", "off")

    try:
        ready = subprocess.run(
            [str(muructl), "probe", "ready", "--run-dir", str(serve.run_dir)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert ready.returncode == 0, ready.stdout
        assert ready.stdout.strip() == "ok"

        # A command inside the container finds the serve's admin socket
        # from the run directory.

        status = subprocess.run(
            [str(muructl), "status"],
            capture_output=True,
            text=True,
            timeout=30,
            env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
        )
        assert status.returncode == 0, status.stdout + status.stderr
        assert "plain-wsgi" in status.stdout, status.stdout

        # The commands that take an application name the one served when
        # given none.

        for command, expected in [
            (["debug", "stacks"], "MainThread"),
            (["logging", "level", "demo=DEBUG", "--for", "1m"], "demo"),
            (["restart", "--worker", "0"], "worker 0"),
        ]:
            completed = subprocess.run(
                [str(muructl), *command],
                capture_output=True,
                text=True,
                timeout=60,
                env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
            )
            output = completed.stdout + completed.stderr
            assert completed.returncode == 0, output
            assert expected in output, output
    finally:
        assert serve.stop() == 0

    gone = subprocess.run(
        [str(muructl), "probe", "live", "--run-dir", str(serve.run_dir)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert gone.returncode == 1


def test_a_command_of_the_project_runs_through_the_serve(tmp_path: Path) -> None:
    # Section 9.12: muructl in the container, as kubectl exec runs it,
    # runs the project's commands through the serve it finds in the run
    # directory, with what the application is served with.

    muructl = support.muructl_path()
    assert muructl is not None
    project = tmp_path / "shop"
    project.mkdir()
    (project / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    (project / "pyproject.toml").write_text(
        """[tool.muru]
entrypoint = 'app:application'
env = { GREETING = 'hello' }

[tool.muru.commands.greet]
run = ['python', '-c', 'import os, sys; print(os.environ["GREETING"], sys.argv[1])', '{name}']
inputs.name = { default = 'pod' }
"""
    )
    serve = start(tmp_path, "--chdir", str(project), "--log-format", "json", app=None)

    def run(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(muructl), *arguments],
            capture_output=True,
            text=True,
            timeout=60,
            cwd=tmp_path,
            stdin=subprocess.DEVNULL,
            env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
        )

    try:
        listed = run("command", "list")
        assert listed.returncode == 0, listed.stderr
        assert listed.stdout.startswith("app@r1\n") and "greet" in listed.stdout, listed.stdout

        ran = run("command", "run", "greet")
        assert ran.returncode == 0, ran.stderr
        assert ran.stdout == "hello pod\n"

        # Its output and events are records of the serve's one stream.

        serve.wait_for("command.finished: greet")
        records = [json.loads(line) for line in serve.stdout]
        assert any(
            record["kind"] == "command"
            and record["command"] == "greet"
            and record["message"] == "hello pod"
            for record in records
        ), records
        assert any(
            record["kind"] == "muru" and record["message"].startswith("command.started: greet")
            for record in records
        )

        one_off = run("exec", "--", "python", "-c", "import sys; sys.exit(7)")
        assert one_off.returncode == 7, one_off.stderr
    finally:
        assert serve.stop() == 0


def test_an_unwritable_run_directory_says_what_fixes_it(tmp_path: Path) -> None:
    if os.geteuid() == 0:
        pytest.skip("root can write anywhere")

    locked = tmp_path / "locked"
    locked.mkdir()
    locked.chmod(0o500)

    try:
        serve = start(tmp_path, "--run-dir", str(locked / "muru"), wait=False)
        assert serve.process.wait(timeout=30) != 0
        time.sleep(0.2)
        message = serve.output() + "".join(serve.stderr)
        assert "cannot be written" in message, message
        assert f"cannot be written by user {os.getuid()}" in message, message
        assert "mount a writable volume at" in message, message
    finally:
        locked.chmod(0o700)


def test_a_missing_runtime_library_is_not_fetched(tmp_path: Path) -> None:
    base = getattr(sys, "_base_executable", sys.executable)

    if base == sys.executable:
        pytest.skip("the tests run outside a virtual environment")

    probe = subprocess.run([base, "-c", "import muru"], capture_output=True)

    if probe.returncode == 0:
        pytest.skip("the base interpreter has muru installed")

    serve = start(
        tmp_path,
        "--python",
        base,
        "--runtime-dir",
        str(tmp_path / "empty"),
        wait=False,
    )
    assert serve.process.wait(timeout=60) != 0
    time.sleep(0.2)
    message = serve.output() + "".join(serve.stderr)
    assert "no runtime library for" in message, message
    assert "muructl build --runtime-dir" in message, message


def sudo_write(path: Path, text: str) -> None:
    """Write a cgroup file as root."""

    subprocess.run(
        ["sudo", "-n", "tee", str(path)],
        input=text,
        text=True,
        check=True,
        stdout=subprocess.DEVNULL,
    )


@pytest.mark.requires_root
@pytest.mark.requires_linux
def test_the_guard_and_status_follow_a_memory_limit_changed_while_serving(
    tmp_path: Path,
) -> None:
    """A cgroup of its own, as a container's, whose memory.max the test
    changes under a running serve, as docker update or an in-place resize
    does (sections 7.7 and 13.9): the change is logged and shown, and with
    less than the headroom left the guard replaces the largest worker,
    which stops first. Needs a cgroup v2 root can write, as in the Linux
    virtual machine; not in CI."""

    root = Path("/sys/fs/cgroup")
    assert (root / "cgroup.controllers").is_file(), "cgroup v2 is needed"
    cgroup = root / f"muru-test-{os.getpid()}-{tmp_path.name}"
    subprocess.run(["sudo", "-n", "mkdir", str(cgroup)], check=True)
    serve: Serve | None = None

    try:
        sudo_write(cgroup / "memory.max", str(512 << 20))

        # The shell moves itself into the cgroup, then becomes serve.

        def into_cgroup(command: list[str]) -> list[str]:
            joined = " ".join(f"'{part}'" for part in command)
            return [
                "sh",
                "-c",
                f"echo $$ | sudo -n tee {cgroup}/cgroup.procs >/dev/null && exec {joined}",
            ]

        serve = start(
            tmp_path,
            "--processes",
            "2",
            "--admin-status",
            "--memory",
            "check_interval=1s",
            wrap=into_cgroup,
        )
        serve.wait_for("memory.max 512MB with 128MB of headroom")
        serve.wait_for("once the application is imported")

        def limits() -> dict[str, Any]:
            return dict(json.loads(serve.admin("/status")[1])["limits"])

        assert limits()["memory_max"] == 512 << 20
        assert limits()["guard"] == "watching"

        # Raised while it serves: said, and shown.

        sudo_write(cgroup / "memory.max", str(1 << 30))
        serve.wait_for("container limits changed: memory.max from 512MB to 1.0GB")
        deadline = time.monotonic() + 10

        while limits()["memory_max"] != 1 << 30:
            assert time.monotonic() < deadline, limits()
            time.sleep(0.1)

        # One worker takes 400MB, then the limit comes down to leave less
        # than the headroom: that worker is replaced, stopping first.

        assert serve.get("/grow?mb=400")[0] == 200
        deadline = time.monotonic() + 10

        while limits()["workers_memory"] < 400 << 20:
            assert time.monotonic() < deadline, limits()
            time.sleep(0.1)

        sudo_write(cgroup / "memory.max", str(limits()["memory_used"] + (60 << 20)))
        serve.wait_for("the container was short of memory", timeout=15)
        serve.wait_for("stops before its replacement starts", timeout=15)
        serve.wait_for("replaced by pid", timeout=30)
        assert limits()["last_replaced"].startswith("plain-wsgi@r1 worker ")
        assert serve.admin("/readyz")[0] == 200
    finally:
        if serve is not None:
            serve.stop()

        subprocess.run(["sudo", "-n", "rmdir", str(cgroup)], check=False)


def test_a_scaling_policy_scales_the_serve_out_under_load_and_back_in(tmp_path: Path) -> None:
    """Section 13.9: a project's scaling policy applies under serve, as
    under the daemon, and `muructl scale status` answers inside it."""

    muructl = support.muructl_path()
    assert muructl is not None
    project = tmp_path / "shop"
    project.mkdir()
    (project / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    (project / "pyproject.toml").write_text(
        """[tool.muru]
entrypoint = 'app:application'
processes = 1
threads = 1

[tool.muru.scaling]
mode = 'spare'
min_workers = 1
max_workers = 2
min_spare = 1
max_spare = 3
scale_out_window = '1s'
scale_in_window = '3s'
hold_after_growth = '2s'
reap_interval = '1s'
idle_reap_after = '3s'
min_lifetime = '1s'
"""
    )
    serve = start(tmp_path, "--chdir", str(project), app=None)
    stop = threading.Event()

    def load() -> None:
        while not stop.is_set():
            try:
                serve.get("/sleep?seconds=0.3")
            except OSError:
                time.sleep(0.1)

    try:
        # Two requests at a time against one single-threaded worker: the
        # group is short and grows to its ceiling.

        threads = [threading.Thread(target=load, daemon=True) for _ in range(2)]

        for thread in threads:
            thread.start()

        serve.wait_for("default: scale out from 1 to 2 workers", timeout=30)
        stop.set()

        for thread in threads:
            thread.join(timeout=10)

        scale = subprocess.run(
            [str(muructl), "scale", "status"],
            capture_output=True,
            text=True,
            timeout=30,
            env={**os.environ, "MURU_RUN_DIR": str(serve.run_dir)},
        )
        assert scale.returncode == 0, scale.stderr
        assert "spare 1-2 workers" in scale.stdout, scale.stdout

        # Idle again: drained back to the floor.

        serve.wait_for("default: scale in from 2 to 1 workers", timeout=60)
    finally:
        stop.set()
        serve.stop()


@pytest.mark.requires_root
@pytest.mark.requires_linux
def test_memory_holds_a_scale_out_back_and_the_guard_scales_in(tmp_path: Path) -> None:
    """Section 13.9: a scale-out is admitted against the cgroup's
    memory.max, a worker counting at its hard limit, and short of memory
    a group with a policy is scaled in rather than having its worker
    replaced. Needs a cgroup v2 root can write, as in the Linux virtual
    machine; not in CI."""

    root = Path("/sys/fs/cgroup")
    assert (root / "cgroup.controllers").is_file(), "cgroup v2 is needed"
    cgroup = root / f"muru-test-{os.getpid()}-{tmp_path.name}"
    subprocess.run(["sudo", "-n", "mkdir", str(cgroup)], check=True)
    project = tmp_path / "shop"
    project.mkdir()
    (project / "app.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    (project / "pyproject.toml").write_text(
        """[tool.muru]
entrypoint = 'app:application'
processes = 1
threads = 1

[tool.muru.memory]
hard_limit = '300MB'
check_interval = '1s'

[tool.muru.scaling]
mode = 'spare'
min_workers = 1
max_workers = 3
min_spare = 1
max_spare = 3
scale_out_window = '1s'
"""
    )
    serve: Serve | None = None
    stop = threading.Event()

    try:
        # 560MB less 128MB of headroom leaves room for one more worker at
        # its hard limit beside the first, and not a third.

        sudo_write(cgroup / "memory.max", str(560 << 20))

        def into_cgroup(command: list[str]) -> list[str]:
            joined = " ".join(f"'{part}'" for part in command)
            return [
                "sh",
                "-c",
                f"echo $$ | sudo -n tee {cgroup}/cgroup.procs >/dev/null && exec {joined}",
            ]

        serve = start(
            tmp_path, "--chdir", str(project), "--admin-status", app=None, wrap=into_cgroup
        )
        running = serve

        def load() -> None:
            while not stop.is_set():
                try:
                    running.get("/sleep?seconds=0.3")
                except OSError:
                    time.sleep(0.1)

        threads = [threading.Thread(target=load, daemon=True) for _ in range(3)]

        for thread in threads:
            thread.start()

        serve.wait_for("default: scale out from 1 to 2 workers", timeout=30)
        serve.wait_for("more workers asked for fit memory.max: a worker counts 300MB", timeout=30)
        stop.set()

        for thread in threads:
            thread.join(timeout=10)

        # One worker grows by 120MB, under its hard limit. Then less than
        # the headroom is left, which replacing it would make up: the
        # group, above its one worker, is scaled in by it instead.

        def limits() -> dict[str, Any]:
            return dict(json.loads(serve.admin("/status")[1])["limits"])

        assert serve.get("/grow?mb=120")[0] == 200
        deadline = time.monotonic() + 10

        while limits()["workers_memory"] < 120 << 20:
            assert time.monotonic() < deadline, limits()
            time.sleep(0.1)

        sudo_write(cgroup / "memory.max", str(limits()["memory_used"] + (60 << 20)))
        serve.wait_for("default: scale in from 2 to 1 workers: for memory", timeout=30)
        serve.wait_for("leaves routing: scaled in for memory", timeout=10)
        assert "the container was short of memory" in serve.output()
        assert "replaced by pid" not in serve.output(), serve.output()
    finally:
        stop.set()

        if serve is not None:
            serve.stop()

        subprocess.run(["sudo", "-n", "rmdir", str(cgroup)], check=False)
