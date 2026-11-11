"""The admin socket of a development run (design specification sections
11.7, 13.1 and 13.8): `muructl run` serves the admin protocol, so the
commands that look into the workers or read the edge's figures work
against it unchanged, the socket is found from the project directory
without a flag, and the commands that manage the daemon's state say so."""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request
from collections.abc import Callable
from pathlib import Path

import pytest
from support import server as support


@pytest.fixture
def runtime() -> Path:
    # A runtime directory of its own, short enough for the socket path
    # limit, as the daemon tests make theirs.

    return support.short_dir("muru-rt-")


def muructl(
    runtime: Path,
    *arguments: str,
    cwd: Path | None = None,
    check: bool = True,
    stdin: str | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a muructl command with the test's runtime directory."""

    path = support.muructl_path()
    assert path is not None
    completed = subprocess.run(
        [str(path), *arguments],
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
        cwd=cwd,
        input=stdin,
        env={**os.environ, "MURU_RUN_DIR": str(runtime), **(env or {})},
    )

    if check and completed.returncode != 0:
        raise RuntimeError(
            f"muructl {' '.join(arguments)} failed:\n{completed.stdout}{completed.stderr}"
        )

    return completed


def leave_socket_behind(path: Path) -> None:
    """Make the file of a socket nothing listens on, as a process killed
    outright leaves it."""

    path.parent.mkdir(parents=True, exist_ok=True)

    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(path))


def start(runtime: Path, port: int | None = None, **kwargs: object) -> support.Server:
    args = list(kwargs.pop("args", []))

    if port is not None:
        args += ["--dashboard", f"127.0.0.1:{port}"]

    server = support.start(
        "plain_wsgi:application",
        workers=2,
        threads=2,
        args=args,
        env={"MURU_RUN_DIR": str(runtime)},
        **kwargs,  # type: ignore[arg-type]
    )
    assert server.wait_for_log("admin socket"), server.log()
    return server


def test_a_debug_reply_larger_than_the_default_limit_arrives(runtime: Path) -> None:
    server = start(runtime)

    try:
        size = 5 * 1024 * 1024
        large = muructl(
            runtime,
            "debug",
            "exec",
            "plain-wsgi",
            "--worker",
            "0",
            "--",
            f"print('x' * {size})",
            cwd=support.FIXTURES,
        ).stdout
        assert large.count("x") >= size, len(large)
    finally:
        server.stop()


def test_the_commands_reach_a_development_run(runtime: Path) -> None:
    server = start(runtime, args=["--timeout", "deadline=2s"])

    try:
        assert server.get("/").status == 200

        # From the project directory the socket needs no flag, and status
        # says what was found.

        status = muructl(runtime, "status", cwd=support.FIXTURES).stdout
        assert status.startswith("run      pid ")
        assert "app      plain-wsgi@r1" in status
        assert f"project  {support.FIXTURES.resolve()}" in status
        assert f"socket   {runtime}/runs/{server.port}/admin.sock" in status
        assert "serving  http://127.0.0.1:" in status

        # The debug commands ask the run's workers, by the application
        # the terminal names, with no debug_exec needed for exec.

        rows = muructl(runtime, "debug", "status", "plain-wsgi", cwd=support.FIXTURES).stdout
        assert rows.splitlines()[0].split()[:3] == ["WORKER", "PID", "GROUP"]
        assert len(rows.splitlines()) == 3
        stacks = muructl(runtime, "debug", "stacks", "plain-wsgi@r1", cwd=support.FIXTURES).stdout
        assert "MainThread in 2 workers" in stacks
        ran = muructl(
            runtime,
            "debug",
            "exec",
            "plain-wsgi",
            "--worker",
            "0",
            "--",
            "print(6 * 7)",
            cwd=support.FIXTURES,
        ).stdout
        assert "42" in ran
        assert server.wait_for_log("muru: debug.exec: ran code in worker 0")
        setup = muructl(runtime, "debug", "logging", cwd=support.FIXTURES).stdout
        assert "the same in worker 1" in setup and "root: level" in setup, setup

        # The edge's figures: metrics, the access log and the WebSockets.

        metrics = muructl(runtime, "metrics", cwd=support.FIXTURES).stdout
        assert "muru_requests_total{" in metrics
        access = muructl(runtime, "logs", "--access", "-n", "5", cwd=support.FIXTURES).stdout
        assert "200 GET / " in access
        raw = muructl(runtime, "logs", "--access", "--json", "-n", "1", cwd=support.FIXTURES).stdout
        assert json.loads(raw)["status"] == 200
        output = muructl(runtime, "logs", "plain-wsgi", cwd=support.FIXTURES).stdout
        assert "muru.host[" in output and "is ready" in output

        # The run's application need not be named (issue 375).

        output = muructl(runtime, "logs", cwd=support.FIXTURES).stdout
        assert "muru.host[" in output and "is ready" in output
        wrong = muructl(runtime, "logs", "other", cwd=support.FIXTURES, check=False)
        assert "no application other here" in wrong.stderr, wrong.stderr
        assert "no WebSockets open" in muructl(runtime, "websockets", cwd=support.FIXTURES).stdout

        # A request past its deadline is answered 504 and goes on in its
        # worker; the host raises an event for it, once (issue 463).

        assert server.get("/sleep?seconds=4").status == 504
        assert server.wait_for_log("request.past_deadline: worker "), server.log()
        assert "GET /sleep?seconds=4 is past its deadline" in server.log()

        # The run keeps the events its hosts raised and answers events
        # as the daemon does, with --after and --follow (issue 470).

        listed = muructl(runtime, "events", "--json", cwd=support.FIXTURES).stdout
        events = json.loads(listed)["events"]
        assert [event["sequence"] for event in events] == list(range(1, len(events) + 1))
        past = [event for event in events if event["type"] == "request.past_deadline"]
        assert len(past) == 1, events
        assert past[0]["subject"] == "plain-wsgi@r1" and past[0]["level"] == "warning"

        last = events[-1]["sequence"]
        after = muructl(runtime, "events", "--json", "--after", str(last - 1), cwd=support.FIXTURES)
        assert [event["sequence"] for event in json.loads(after.stdout)["events"]] == [last]

        path = support.muructl_path()
        assert path is not None
        follow = subprocess.Popen(
            [str(path), "events", "--follow", "--json", "--after", str(last)],
            stdout=subprocess.PIPE,
            text=True,
            cwd=support.FIXTURES,
            env={**os.environ, "MURU_RUN_DIR": str(runtime)},
        )

        try:
            muructl(runtime, "restart", "plain-wsgi", "--worker", "0", cwd=support.FIXTURES)
            assert follow.stdout is not None
            followed = json.loads(follow.stdout.readline())
            assert followed["sequence"] == last + 1, followed
        finally:
            follow.kill()
            follow.wait()

        # Logging levels and a restart go through the host as they do
        # under the daemon; the restart's event reaches the terminal.

        changed = muructl(
            runtime, "logging", "level", "plain-wsgi", "=DEBUG", "--for", "1m", cwd=support.FIXTURES
        ).stdout
        assert "DEBUG" in changed
        restarted = muructl(
            runtime, "restart", "plain-wsgi", "--worker", "1", cwd=support.FIXTURES
        ).stdout
        assert "worker 1" in restarted and "is being replaced by pid" in restarted
        assert server.wait_for_log(": worker.restarted: worker 1")

        # A change to what the access log writes goes into the run's edge
        # at once and is printed as the daemon records it.

        quiet = muructl(
            runtime,
            "logging",
            "access",
            "off",
            "--path",
            "/quiet",
            "--for",
            "1m",
            cwd=support.FIXTURES,
        ).stdout
        assert "the access log writes no request to /quiet for 1m00s" in quiet, quiet
        assert "  no request to /quiet, " in quiet, quiet
        assert server.wait_for_log("muru: access_log.changed: the access log writes no request")
        server.get("/quiet")
        server.get("/marker")
        assert server.wait_for_log(" GET /marker ")
        assert " GET /quiet " not in server.log()
        reset = muructl(runtime, "logging", "access", "--reset", cwd=support.FIXTURES).stdout
        assert "put back 1 change to the access log" in reset, reset
        assert "writes what the configuration says" in reset

        # What manages the daemon's state says it needs the daemon.

        refused = muructl(runtime, "deploy", "shop", ".", cwd=support.FIXTURES, check=False)
        assert refused.returncode != 0
        assert "muructl deploy needs the daemon: this is the development run of plain-wsgi" in (
            refused.stderr
        )
        refused = muructl(runtime, "logs", "reopen", cwd=support.FIXTURES, check=False)
        assert "no access log file is set (--access-log-file)" in refused.stderr, refused.stderr

        # Another application, or a release that is not serving, is not
        # found.

        missing = muructl(runtime, "debug", "status", "shop", cwd=support.FIXTURES, check=False)
        assert "no application shop here" in missing.stderr
        missing = muructl(
            runtime, "debug", "status", "plain-wsgi@r7", cwd=support.FIXTURES, check=False
        )
        assert "plain-wsgi@r7 is not serving" in missing.stderr
    finally:
        server.stop()

    # The run's place in the runtime directory goes with it.

    assert not (runtime / "runs" / str(server.port)).exists()


def test_the_socket_is_found_by_project_or_named(runtime: Path, tmp_path: Path) -> None:
    other = tmp_path / "other"
    other.mkdir()
    (other / "plain_wsgi.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    first = start(runtime)
    second = start(runtime, chdir=other)

    try:
        # Each project's directory finds its own run.

        assert (
            f"runs/{first.port}/admin.sock"
            in muructl(runtime, "status", cwd=support.FIXTURES).stdout
        )
        assert f"runs/{second.port}/admin.sock" in muructl(runtime, "status", cwd=other).stdout

        # Elsewhere, with no daemon, the runs are named so one can be
        # chosen. A socket file left behind by a daemon is no daemon.

        leave_socket_behind(runtime / "admin.sock")
        ambiguous = muructl(runtime, "status", cwd=tmp_path, check=False)
        assert ambiguous.returncode != 0
        assert "several development runs are" in ambiguous.stderr
        assert f"--socket {runtime}/runs/{first.port}/admin.sock" in ambiguous.stderr
        assert f"--socket {runtime}/runs/{second.port}/admin.sock" in ambiguous.stderr
        chosen = muructl(
            runtime,
            "status",
            "--socket",
            f"{runtime}/runs/{second.port}/admin.sock",
            cwd=tmp_path,
        ).stdout
        assert f"project  {other.resolve()}" in chosen
        chosen = muructl(
            runtime,
            "status",
            cwd=tmp_path,
            env={"MURU_SOCKET": f"{runtime}/runs/{first.port}/admin.sock"},
        ).stdout
        assert f"project  {support.FIXTURES.resolve()}" in chosen
    finally:
        first.stop()
        second.stop()

    # With none running, the command says so, naming both ways to start one.

    nothing = muructl(runtime, "status", cwd=tmp_path, check=False)
    assert "nothing is listening at" in nothing.stderr
    assert "no development run (muructl run)" in nothing.stderr

    # A run that has gone, whose pid another process has since been given
    # and whose socket file is still there, is not taken for a live one.
    # The daemon's socket left behind above is what the command tries.

    gone = runtime / "runs" / "1"
    leave_socket_behind(gone / "admin.sock")
    (gone / "run.json").write_text(
        json.dumps(
            {
                "pid": os.getpid(),
                "app": "gone",
                "project": str(tmp_path),
                "listen": "127.0.0.1:1",
                "socket": str(gone / "admin.sock"),
            }
        )
    )
    nothing = muructl(runtime, "status", cwd=tmp_path, check=False)
    assert f"nothing is listening at {runtime}/admin.sock" in nothing.stderr
    assert not gone.exists()


def test_the_dashboard_has_the_worker_pages(runtime: Path) -> None:
    port = support.free_port()
    server = start(runtime, port=port)

    def read(path: str) -> str:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=10) as response:
            body: bytes = response.read()
            return body.decode()

    try:
        assert server.wait_for_log(f"dashboard on http://127.0.0.1:{port}/")
        assert 'href="workers?release=plain-wsgi@r1"' in read("/fragment/overview")
        page = read("/workers?release=plain-wsgi@r1")
        assert "<title>Muru: plain-wsgi@r1 workers</title>" in page
        assert '<a href="./">run</a>' in page
        assert "Restart" not in page
        status = json.loads(read("/v1/debug/status?release=plain-wsgi@r1"))
        assert [worker["index"] for worker in status["workers"]] == [0, 1]
        assert all(worker["result"]["ready"] for worker in status["workers"])
        stacks = read("/fragment/workers/stacks?release=plain-wsgi@r1")
        assert "MainThread" in stacks

        with pytest.raises(urllib.error.HTTPError) as refused:
            read("/v1/debug/status?release=plain-wsgi@r9")

        assert refused.value.code == 409
        assert "is not serving" in refused.value.read().decode()
    finally:
        server.stop()


PAUSED = """
import os


def application(environ, start_response):
    value = 20

    if environ["PATH_INFO"] == "/pause":
        breakpoint(timeout=float(environ.get("QUERY_STRING") or 30))

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [f"value={value}".encode()]
"""


def test_a_breakpoint_is_debugged_under_a_run(runtime: Path, tmp_path: Path) -> None:
    # Section 11.7 under section 13.1: breakpoint() names the command
    # that attaches, and it works as it is, with no debug_exec to set.

    project = tmp_path / "paused"
    project.mkdir()
    (project / "paused.py").write_text(PAUSED)
    server = support.start(
        "paused:application",
        workers=1,
        threads=2,
        chdir=project,
        env={"MURU_RUN_DIR": str(runtime)},
    )

    try:
        assert server.wait_for_log("admin socket"), server.log()
        server.mark_log()
        thread, answers = in_background(lambda: server.get("/pause"))
        assert server.wait_for_log("waits 30 s for muructl debug attach paused --worker 0")

        session = muructl(
            runtime,
            "debug",
            "attach",
            "paused",
            "--worker",
            "0",
            cwd=project,
            stdin="p value\nvalue = 99\ncontinue\n",
        )
        assert "muru: attached to worker 0" in session.stderr
        assert "paused.py" in session.stderr
        assert "(Pdb) 20" in session.stdout.replace("\n", " ")
        thread.join(30)
        assert answers[0].body == b"value=99"
        assert server.wait_for_log("muru: debug.attached: a debugger session opened in worker 0")
    finally:
        server.stop()


def in_background(
    call: Callable[[], support.Response],
) -> tuple[threading.Thread, list[support.Response]]:
    answers: list[support.Response] = []
    thread = threading.Thread(target=lambda: answers.append(call()))
    thread.start()
    return thread, answers


SPEWING_APP = """
import sys
import threading
import time


def spew():
    for _ in range(20000):
        sys.stderr.write("x" * 99 + "\\n")


def application(environ, start_response):
    if environ["PATH_INFO"] == "/spew":
        threading.Thread(target=spew, daemon=True).start()

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""


def test_a_stalled_terminal_leaves_the_run_answering(runtime: Path, tmp_path: Path) -> None:
    """Output the terminal does not read holds up the thread that writes
    it, not muructl's runtime: with one runtime thread, the run still
    answers its admin socket while its standard error is full."""

    muructl_path = support.muructl_path()
    if muructl_path is None or not support.extension_available():
        support.unavailable("muructl or the extension is not built")
    (tmp_path / "spewing.py").write_text(SPEWING_APP)
    port = support.free_port()

    # Its output goes to a pipe nothing reads, as a terminal paused with
    # Ctrl-S or a pager that is not paged.

    process = subprocess.Popen(
        [
            str(muructl_path),
            "run",
            "spewing:application",
            "--chdir",
            str(tmp_path),
            "--bind",
            f"127.0.0.1:{port}",
            "--python",
            sys.executable,
            "--processes",
            "1",
            "--no-reload",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env={**os.environ, "MURU_RUN_DIR": str(runtime), "TOKIO_WORKER_THREADS": "1"},
    )

    try:
        support.wait_for_port(port, process)
        assert urllib.request.urlopen(f"http://127.0.0.1:{port}/spew", timeout=30).read() == b"ok"

        # About 2 MB of output fills every pipe on the way; the admin
        # socket still answers.

        time.sleep(2)
        status = muructl(runtime, "status", cwd=tmp_path)
        assert "spewing" in status.stdout, status.stdout + status.stderr
    finally:
        process.kill()
        process.communicate(timeout=30)


def test_a_runs_access_log_file_is_reopened_after_a_rename(runtime: Path, tmp_path: Path) -> None:
    """muructl logs reopen and SIGUSR1 make a run open its access log
    file again by name, so the lines after a rename go to a new file."""

    log = tmp_path / "access.log"
    server = start(runtime, args=["--access-log-file", str(log)])

    def wait_for(predicate: Callable[[], bool], what: str) -> None:
        deadline = time.monotonic() + 10

        while not predicate():
            assert time.monotonic() < deadline, f"timed out waiting for {what}"
            time.sleep(0.05)

    def written(path: Path, target: str) -> bool:
        return path.is_file() and target in path.read_text()

    try:
        assert server.get("/before").status in (200, 404)
        wait_for(lambda: written(log, "/before"), "the first line")

        log.rename(tmp_path / "access.log.1")
        reopened = muructl(runtime, "logs", "reopen", cwd=support.FIXTURES).stdout
        assert reopened.strip() == "muru: reopened the log files: the edge", reopened
        assert server.get("/after-reopen").status in (200, 404)
        wait_for(lambda: written(log, "/after-reopen"), "a line in the new file")

        log.rename(tmp_path / "access.log.2")
        server.mark_log()
        os.kill(server.process.pid, signal.SIGUSR1)
        assert server.wait_for_log("reopened the access log", timeout=10)
        assert server.get("/after-signal").status in (200, 404)
        wait_for(lambda: written(log, "/after-signal"), "a line after SIGUSR1")
        assert "/after-signal" not in (tmp_path / "access.log.2").read_text()
    finally:
        server.stop()
