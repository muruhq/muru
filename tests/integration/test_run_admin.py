"""The admin socket of a development run (design specification sections
11.7, 13.1 and 13.8): `muructl run` serves the admin protocol, so the
commands that look into the workers or read the edge's figures work
against it unchanged, the socket is found from the project directory
without a flag, and the commands that manage the daemon's state say so."""

from __future__ import annotations

import fcntl
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

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
        assert f"socket   {server.admin_socket()}" in status
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

    assert not server.admin_socket().parent.exists()


def test_the_socket_is_found_by_project_or_named(runtime: Path, tmp_path: Path) -> None:
    other = tmp_path / "other"
    other.mkdir()
    (other / "plain_wsgi.py").write_text((support.FIXTURES / "plain_wsgi.py").read_text())
    first = start(runtime)
    second = start(runtime, chdir=other)

    try:
        # Each project's directory finds its own run.

        assert str(first.admin_socket()) in muructl(runtime, "status", cwd=support.FIXTURES).stdout
        assert str(second.admin_socket()) in muructl(runtime, "status", cwd=other).stdout

        # The directory a run was started from reaches it too, and the run
        # says so at start (issue 592). Both were started from here.

        here = Path.cwd().resolve()
        assert (
            f"muructl commands run from {other.resolve()} or {here} reach this run" in second.log()
        )
        assert f"{runtime}/run-" in muructl(runtime, "status", cwd=here).stdout

        # A second run of the first project is the one reached, whichever
        # port it has, and both it and the commands name the older one
        # (issue 714).

        third = start(runtime)

        try:
            older = f"--instance run:{first.port}"
            assert "another development run of this project is listening" in third.log()
            assert older in third.log(), third.log()
            reached = muructl(runtime, "status", cwd=support.FIXTURES)
            assert str(third.admin_socket()) in reached.stdout
            assert (
                "2 development runs of this project are listening; reaching the newest"
                in reached.stderr
            ), reached.stderr
            assert f"{older} reaches the other" in reached.stderr, reached.stderr
        finally:
            third.stop()

        # Elsewhere, with no daemon, the runs are named so one can be
        # chosen. A socket file left behind by a daemon is no daemon.

        leave_socket_behind(runtime / "admin.sock")
        ambiguous = muructl(runtime, "status", cwd=tmp_path, check=False)
        assert ambiguous.returncode != 0
        assert "no daemon is running and 2 development runs are" in ambiguous.stderr
        assert f"\n  --instance run:{first.port}  " in ambiguous.stderr, ambiguous.stderr
        assert f"\n  --instance run:{second.port}  " in ambiguous.stderr, ambiguous.stderr
        chosen = muructl(
            runtime,
            "status",
            "--socket",
            str(second.admin_socket()),
            cwd=tmp_path,
        ).stdout
        assert f"project  {other.resolve()}" in chosen
        chosen = muructl(
            runtime,
            "status",
            cwd=tmp_path,
            env={"MURU_SOCKET": str(first.admin_socket())},
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
    # and whose socket file is still there, is not taken for a live one:
    # its registry entry's lock is free. The daemon's socket left behind
    # above is what the command tries.

    gone = runtime / "run-00000000"
    leave_socket_behind(gone / "admin.sock")
    entry = support.user_registry() / ("0" * 31 + "1")
    entry.mkdir(parents=True)

    try:
        (entry / "lock").touch()
        (entry / "instance.json").write_text(
            json.dumps(
                {
                    "format": 1,
                    "id": entry.name,
                    "host": "here",
                    "kind": "run",
                    "scope": "user",
                    "pid": os.getpid(),
                    "started_unix_ms": 1,
                    "muru_version": "1.0.0",
                    "uid": os.getuid(),
                    "admin": {"transport": "unix", "path": str(gone / "admin.sock")},
                    "run_dir": str(gone),
                    "listeners": [{"name": "http", "configured": "127.0.0.1:1"}],
                    "app": "gone",
                    "project": str(tmp_path),
                }
            )
        )
        nothing = muructl(runtime, "status", cwd=tmp_path, check=False)
        assert f"nothing is listening at {runtime}/admin.sock" in nothing.stderr
    finally:
        shutil.rmtree(entry)


def test_a_request_is_looked_up_by_its_id(runtime: Path) -> None:
    """A development run keeps a record of each request (section 11.7),
    which `muructl debug request` and the dashboard's request page read
    by the ID the response names, with secret header values and query
    parameters masked."""

    port = support.free_port()
    server = start(runtime, port=port)

    def read(path: str) -> str:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=10) as response:
            body: bytes = response.read()
            return body.decode()

    def record(request_id: str) -> dict[str, Any]:
        answer = muructl(runtime, "debug", "request", request_id, "--json", cwd=support.FIXTURES)
        assert "hunter2" not in answer.stdout
        found: dict[str, Any] = json.loads(answer.stdout)
        return found

    try:
        response = server.get(
            "/?token=hunter2&page=2",
            headers={"Cookie": "session=hunter2", "X-Api-Key": "hunter2"},
        )
        assert response.status == 200
        request_id = response.header("x-request-id")
        assert request_id is not None

        # The record completes once the response has ended, which can
        # come a moment after the client has read it.

        deadline = time.monotonic() + 10

        while (found := record(request_id))["in_progress"]:
            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        assert (found["method"], found["target"], found["status"]) == (
            "GET",
            "/?token=***&page=2",
            200,
        )
        assert found["app"] == "plain-wsgi" and found["worker"] in (0, 1)
        assert found["connection"]["position"] == 1
        assert len(found["connection"]["id"]) == 26
        assert found["connection"]["closed_after"] is False
        sent = {header["name"]: header["value"] for header in found["request_headers"]}
        assert sent["cookie"] == "***" and sent["x-api-key"] == "***"
        answered = {header["name"] for header in found["response_headers"]}
        assert {"x-request-id", "server-timing", "muru-request-link"} <= answered
        assert found["compression"]["compressed"] is False
        assert [panel["name"] for panel in found["panels"]] == [
            "hints",
            "timing",
            "where",
            "response",
            "logs",
            "errors",
            "connection",
        ]

        # The worker's part follows the response (issue 635): its thread,
        # its times and where the body came from.

        assert found["worker_part"] == "arrived"
        worker = found["from_worker"]
        assert worker["thread"] and worker["body_source"] == "iterable"
        assert worker["outcome"] == "completed"
        assert 0 < worker["to_start_response"] <= worker["to_close"]

        # The text form names the request and draws each panel.

        text = muructl(runtime, "debug", "request", request_id, cwd=support.FIXTURES).stdout
        assert text.startswith("GET /?token=***&page=2 HTTP/1.1  200  "), text
        assert f"request {request_id}\n" in text
        assert "\nTiming\n" in text and "    response head at the edge  at " in text
        assert "    cookie: ***" in text

        # The access log's JSON names the connection in every mode.

        logged = muructl(runtime, "logs", "--access", "--json", cwd=support.FIXTURES).stdout
        line = next(json.loads(row) for row in logged.splitlines() if request_id in row)
        assert line["connection"] == found["connection"]["id"]
        assert line["connection_requests"] == 1

        # The dashboard's page holds the same panels, and its JSON is the
        # record; the recent requests link to the page.

        page = read(f"/request/{request_id}")
        assert '<section id="panel-timing" class="panel">' in page
        assert "<h2>Connection</h2>" in page and "hunter2" not in page
        assert json.loads(read(f"/v1/requests/{request_id}"))["request_id"] == request_id
        assert f'href="request/{request_id}"' in read("/fragment/requests")

        # A panel's link to another request's panel, as the toolbar
        # follows it, goes to that request's page here (issue 706).

        assert "<h2>Errors</h2>" in read(f"/_muru/toolbar/requests/{request_id}/panels/errors.html")

        # A failed request's record has the exception, its traceback and
        # the log record that reported it.

        failed = server.get("/error")
        assert failed.status == 500
        failed_id = failed.header("x-request-id")
        assert failed_id is not None
        deadline = time.monotonic() + 10

        while (broken := record(failed_id))["in_progress"]:
            assert time.monotonic() < deadline, broken
            time.sleep(0.05)

        worker = broken["from_worker"]
        assert (worker["outcome"], worker["error_phase"]) == ("failed", "call")
        assert "RuntimeError: deliberate failure" in worker["traceback"]
        assert any(line["logger"] == "muru" and line["level"] == "ERROR" for line in worker["logs"])

        # A request the run does not keep is said to be so.

        missing = muructl(
            runtime, "debug", "request", "NOSUCHREQUEST", cwd=support.FIXTURES, check=False
        )
        assert missing.returncode != 0
        assert "request NOSUCHREQUEST is no longer kept" in missing.stderr

        with pytest.raises(urllib.error.HTTPError) as refused:
            read("/v1/requests/NOSUCHREQUEST")

        assert refused.value.code == 404
        assert "no longer kept" in json.loads(refused.value.read())["error"]
    finally:
        server.stop()


def test_the_toolbar_is_inserted_into_pages_and_reads_the_record(runtime: Path) -> None:
    """With --toolbar the edge inserts the toolbar's tag into an HTML page
    a browser loads (section 4.20), and answers the reserved prefix from
    the run's records, to loopback and its own pages only."""

    port = support.free_port()
    server = start(runtime, port=port, args=["--toolbar"])

    try:
        assert server.wait_for_log("toolbar on:")
        page = server.get("/page", headers={"Sec-Fetch-Dest": "document"})
        request_id = page.header("x-request-id")
        assert request_id is not None
        assert page.header("content-length") is None
        tag = (
            f'<script src="/_muru/toolbar/toolbar.js" data-request="{request_id}" '
            f'data-dashboard="http://127.0.0.1:{port}/request/{request_id}" async></script>'
        )
        assert page.body.decode() == f"<html><body><h1>Page</h1>{tag}</body></html>"

        # HTML with no </body> gets the tag at its end only for a request
        # that said it loads a document, not for a client that sends no
        # Sec-Fetch-Dest (issue 719).

        appended = server.get("/fragment", headers={"Sec-Fetch-Dest": "document"})
        assert appended.body.startswith(b"<p>fragment</p><script src=")
        assert server.get("/fragment").body == b"<p>fragment</p>"

        # A fetch from the page is left alone, and a trusted proxy's prefix
        # goes in front of the script's address.

        fetched = server.get("/page", headers={"Sec-Fetch-Dest": "empty"})
        assert b"_muru" not in fetched.body
        proxied = server.get(
            "/page",
            headers={"Sec-Fetch-Dest": "document", "X-Forwarded-Prefix": "/proxy/8080"},
        )
        assert b'<script src="/proxy/8080/_muru/toolbar/toolbar.js"' in proxied.body

        # The reserved prefix: the script, the record and a panel, and
        # nothing for another site's page or an unknown host.

        script = server.get("/_muru/toolbar/toolbar.js", headers={"Sec-Fetch-Site": "same-origin"})
        assert script.status == 200
        assert script.header("content-type") == "text/javascript"
        deadline = time.monotonic() + 10

        while True:
            answer = server.get(f"/_muru/toolbar/requests/{request_id}.json")
            assert answer.status == 200
            found = json.loads(answer.body)

            if not found["in_progress"]:
                break

            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        assert found["request_id"] == request_id and found["target"] == "/page"
        panel = server.get(f"/_muru/toolbar/requests/{request_id}/panels/timing.html")
        assert panel.status == 200 and "<table" in panel.body.decode()
        assert server.get("/_muru/toolbar/requests/NOSUCH.json").status == 404

        # The page view's summaries, several in one answer, and the IDs
        # no longer kept.

        batch = json.loads(server.get(f"/_muru/toolbar/requests?ids={request_id},NOSUCH").body)
        assert [summary["request_id"] for summary in batch["requests"]] == [request_id]
        assert batch["not_kept"] == ["NOSUCH"]
        summary = batch["requests"][0]
        assert (summary["target"], summary["status"], summary["served_by_edge"]) == (
            "/page",
            200,
            False,
        )
        assert summary["connection"] == found["connection"]["id"]
        assert server.get("/_muru/toolbar/requests").status == 400
        refused = server.get(
            f"/_muru/toolbar/requests/{request_id}.json", headers={"Sec-Fetch-Site": "cross-site"}
        )
        assert refused.status == 403
        rebound = server.get(
            f"/_muru/toolbar/requests/{request_id}.json", headers={"Host": "evil.test"}
        )
        assert rebound.status == 403

        # Issue 708: a form's POST that redirects, then the page it led
        # to, are one entry of the history, the newest, with its steps.

        document = {"Sec-Fetch-Dest": "document"}
        posted = server.request("POST", "/form", body=b"name=x", headers=document)
        assert posted.status == 303
        landed = server.get("/page", headers=document)
        entries = json.loads(server.get("/_muru/toolbar/history").body)["entries"]
        assert [(step["method"], step["target"]) for step in entries[0]["steps"]] == [
            ("POST", "/form"),
            ("GET", "/page"),
        ], entries
        assert entries[0]["steps"][1]["request_id"] == landed.header("x-request-id")
        assert all(
            step["target"] != "/_muru/toolbar/history"
            for entry in entries
            for step in entry["steps"]
        )
    finally:
        server.stop()


def test_a_blocked_event_loop_is_on_the_requests_record(runtime: Path) -> None:
    """What held a request up is on its record (section 11.7): an ASGI
    request that blocks its event loop for longer than the loop's
    heartbeat shows the loop held, on the Timing panel too, and the
    module it loaded on first use."""

    server = support.start("plain_asgi:application", workers=1, env={"MURU_RUN_DIR": str(runtime)})

    try:
        assert server.wait_for_log("admin socket"), server.log()
        response = server.get("/block?seconds=1.2")
        assert response.status == 200
        request_id = response.header("x-request-id")
        deadline = time.monotonic() + 10

        while (found := json.loads(server.get(f"/_muru/toolbar/requests/{request_id}.json").body))[
            "in_progress"
        ]:
            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        # A heartbeat was due within half a second of the request's start,
        # so the loop was held for at least the rest of the block.

        worker = found["from_worker"]
        assert worker["loop_lag"] >= 500, worker
        assert worker["loop_lag"] + worker["loop_lag_at"] <= worker["to_close"] + 1, worker
        held = worker["loop_held_stretches"]
        assert max(stretch["ms"] for stretch in held) == worker["loop_lag"], worker
        panel = server.get(f"/_muru/toolbar/requests/{request_id}/panels/timing.html").body.decode()
        assert ">event loop held</td>" in panel and "During the request" in panel

        # Issue 738: the module the request loaded on first use, under
        # its own request's id on the loop, before the head too.

        assert "colorsys" in [load["module"] for load in worker["module_loads"]], worker
        assert worker["modules_ms"] > 0 and ">importing modules</td>" in panel
        assert "import;dur=" in response.header("server-timing")

        # With a GIL, the request's start on the loop waited for it;
        # without one, nothing is shown.

        if support.workers_have_a_gil():
            assert worker["gil_waits"] >= 1, worker
            assert len(worker["gil_wait_stretches"]) == min(worker["gil_waits"], 64), worker
        else:
            assert worker["gil_waits"] == 0 and ">waiting for the GIL</td>" not in panel
    finally:
        server.stop()


WAITING_APP = """
import threading

loading = threading.Event()


def app(environ, start_response):
    # /load loads the slow module; /wait asks for it once that has begun.

    if environ["PATH_INFO"] == "/wait":
        loading.wait(10)

    import waiting_slow  # noqa: F401

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"ok"]
"""


def test_a_request_waiting_on_another_requests_import_says_so(
    tmp_path: Path, runtime: Path
) -> None:
    """Issue 787: a request that asks for a module another request is
    still loading waits on it; its record and Server-Timing say so, with
    the module and the request loading it."""

    (tmp_path / "waiting_app.py").write_text(WAITING_APP)
    (tmp_path / "waiting_slow.py").write_text(
        "import time\n\nimport waiting_app\n\nwaiting_app.loading.set()\ntime.sleep(0.3)\n"
    )
    (tmp_path / "pyproject.toml").write_text('[tool.muru]\nentrypoint = "waiting_app:app"\n')
    server = support.start(
        None, chdir=tmp_path, workers=1, threads=2, env={"MURU_RUN_DIR": str(runtime)}
    )

    try:
        loaded: list[Any] = []
        loader = threading.Thread(target=lambda: loaded.append(server.get("/load")))
        loader.start()
        waited = server.get("/wait")
        loader.join()

        timing = waited.header("server-timing") or ""
        assert "import_wait;dur=" in timing and "import;dur=" not in timing, timing
        assert "import;dur=" in (loaded[0].header("server-timing") or "")

        request_id = waited.header("x-request-id")
        deadline = time.monotonic() + 10

        while (found := json.loads(server.get(f"/_muru/toolbar/requests/{request_id}.json").body))[
            "in_progress"
        ]:
            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        waits = found["from_worker"]["import_waits"]
        assert [(wait["module"], wait["loader"]) for wait in waits] == [
            ("waiting_slow", loaded[0].header("x-request-id"))
        ], found["from_worker"]
        panel = server.get(f"/_muru/toolbar/requests/{request_id}/panels/timing.html").body.decode()
        assert ">waiting for an import</td>" in panel, panel
    finally:
        server.stop()


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

        # Issue 764: the run's process id is printed, and written beside
        # its admin socket.

        written = re.search(r"pid (\d+), written to (\S+)", server.log())
        assert written, server.log()
        assert int(written.group(1)) == server.process.pid
        assert Path(written.group(2)).read_text() == f"{server.process.pid}\n"
        assert Path(written.group(2)).parent == server.admin_socket().parent
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

        # The process tree (issue 595): the run, its host with the relay
        # and both workers beneath it, each process titled as it says.

        def tree() -> dict[str, Any]:
            listed = muructl(runtime, "status", "--processes", "--json", cwd=support.FIXTURES)
            answer: dict[str, Any] = json.loads(listed.stdout)
            return answer

        deadline = time.monotonic() + 10

        while len(tree()["root"]["children"][0]["children"]) < 3:
            assert time.monotonic() < deadline, tree()
            time.sleep(0.1)

        answer = tree()
        root = answer["root"]
        assert (answer["mode"], root["role"], root["pid"]) == ("run", "run", server.process.pid)
        [host] = root["children"]
        assert (host["role"], host["what"]) == ("host", "plain_wsgi:application (r1)")
        assert host["interface"] == "wsgi", host
        roles = [(child["role"], child["index"]) for child in host["children"]]
        assert roles == [("relay", None), ("worker", 0), ("worker", 1)], roles
        assert all(child["os_parent_pid"] == host["pid"] for child in host["children"][1:])
        assert not any(child["readopted"] for child in host["children"])

        titles = {
            process["pid"]: subprocess.run(
                ["ps", "-o", "command=", "-p", str(process["pid"])],
                capture_output=True,
                text=True,
                check=False,
            ).stdout.strip()
            for process in [root, host, *host["children"]]
        }
        assert titles[root["pid"]] == (
            f"muru run plain-wsgi 127.0.0.1:{server.port} dashboard 127.0.0.1:{port}"
        ), titles
        tag = f"(run:{server.port})"
        assert titles[host["pid"]] == f"muru host plain-wsgi@r1 {tag}", titles
        assert titles[host["children"][0]["pid"]] == f"muru relay plain-wsgi@r1 {tag}", titles
        assert titles[host["children"][2]["pid"]] == (
            f"muru worker plain-wsgi@r1 default/1 {tag}"
        ), titles

        text = muructl(runtime, "status", "--processes", cwd=support.FIXTURES).stdout
        assert "└─ host" in text and "├─ relay" in text and "└─ worker" in text, text

        # The dashboard's Processes section reads the same answer.

        shown = json.loads(read("/v1/processes"))
        assert [child["pid"] for child in shown["root"]["children"][0]["children"]] == [
            child["pid"] for child in host["children"]
        ]
        fragment = read("/fragment/processes")
        assert 'href="worker?release=plain-wsgi@r1&#38;worker=0"' in fragment, fragment
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
            "--dashboard",
            "off",
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


def test_a_run_killed_outright_leaves_a_stale_entry_that_is_not_reached(runtime: Path) -> None:
    # Issue 805: a run registers in the user's instance registry, whatever
    # its run directory; killed outright, its lock is freed with it, so
    # its entry reads as stale and discovery passes it over.

    server = start(runtime)
    socket_path = server.admin_socket()

    # Issue 948: the registry is the machine's own, and may hold entries
    # of other instances whose record cannot be read, which are passed
    # over as discovery passes them over.

    def pid_of(entry: Path) -> int | None:
        try:
            return int(json.loads((entry / "instance.json").read_text())["pid"])
        except (OSError, ValueError, KeyError):
            return None

    entries = [
        entry
        for entry in support.user_registry().iterdir()
        if not entry.name.startswith(".") and pid_of(entry) == server.process.pid
    ]

    if len(entries) != 1:
        server.stop()

    assert len(entries) == 1, entries
    record = json.loads((entries[0] / "instance.json").read_text())
    assert record["kind"] == "run" and record["app"] == "plain-wsgi"
    assert record["admin"] == {"transport": "unix", "path": str(socket_path)}
    assert record["run_dir"] == str(socket_path.parent)
    assert record["listeners"][0]["bound"] == f"127.0.0.1:{server.port}"

    def locked() -> bool:
        with (entries[0] / "lock").open("rb") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                return True

            fcntl.flock(lock, fcntl.LOCK_UN)
            return False

    assert locked()
    assert str(socket_path) in muructl(runtime, "status", cwd=support.FIXTURES).stdout

    try:
        server.process.send_signal(signal.SIGKILL)
        server.process.wait(timeout=10)
        assert not locked()
        nothing = muructl(runtime, "status", cwd=support.FIXTURES, check=False)
        assert nothing.returncode != 0
        assert "nothing is listening at" in nothing.stderr, nothing.stderr
    finally:
        server.stop()
        shutil.rmtree(entries[0], ignore_errors=True)
