"""How a worker stops (design specification section 7.6): the requests get
their time, and the shutdown work after them gets the exit grace."""

from __future__ import annotations

import http.client
import threading
from pathlib import Path

import pytest
from support import server as support
from test_serve import start as start_serve

SHORT = ["--timeout", "shutdown=500ms", "--timeout", "exit_grace=2s"]


def _ask_stuck(port: int) -> None:
    """Ask for a request that will not finish."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    try:
        connection.request("GET", "/stuck")
        connection.getresponse().read()
    except (OSError, http.client.HTTPException):
        pass


def ask_in_the_background(server: support.Server) -> threading.Thread:
    """Send a request that will not finish, on a thread of its own."""

    thread = threading.Thread(target=_ask_stuck, args=(server.port,), daemon=True)
    thread.start()
    return thread


@pytest.mark.parametrize("app", ["stop_asgi:application", "stop_wsgi:application"])
def test_a_stuck_request_leaves_time_for_the_shutdown_work(app: str) -> None:
    server = support.start(app, workers=1, args=["--hooks", "stop_hooks", *SHORT])
    asked = ask_in_the_background(server)

    try:
        assert server.wait_for_log("STOP request began", timeout=10)
        server.stop()
        asked.join(timeout=5)
    finally:
        server.stop()

    log = support.untimed(server.log())
    assert "did not stop, killing" not in log

    if "asgi" in app:
        began = log.index("STOP lifespan shutdown began")
        finished = log.index("STOP lifespan shutdown finished")
        assert began < finished < log.index("STOP on_exit ran")
    else:
        assert "STOP on_exit ran" in log


def test_a_slow_lifespan_shutdown_is_reported_and_the_hook_still_runs() -> None:
    server = support.start(
        "stop_asgi:application",
        workers=1,
        args=["--hooks", "stop_hooks", *SHORT],
        env={"MURU_TEST_LIFESPAN_SECONDS": "30"},
    )

    server.stop()

    log = support.untimed(server.log())
    assert "STOP lifespan shutdown began" in log
    assert "did not complete lifespan shutdown in time" in log
    assert "STOP on_exit ran" in log
    assert "did not stop, killing" not in log


def test_a_whole_shutdown_leaves_the_workers_time_past_a_long_shutdown_timeout(
    tmp_path: Path,
) -> None:
    serve = start_serve(
        tmp_path,
        "--shutdown-budget",
        "3s",
        "--timeout",
        "shutdown=60s",
        "--timeout",
        "exit_grace=1s",
        "--hooks",
        "stop_hooks",
        app="stop_asgi:application",
    )
    asked = threading.Thread(
        target=lambda: _ask_stuck(serve.port),
        daemon=True,
    )
    asked.start()
    serve.wait_for("STOP request began")
    assert serve.stop() == 0
    asked.join(timeout=5)
    output = serve.output() + "".join(serve.stderr)

    # The request could have had a minute. The shutdown's budget cuts it
    # short and the worker keeps its exit grace, so the work ran whole.

    finished = output.index("STOP lifespan shutdown finished")
    assert finished < output.index("STOP on_exit ran")
    assert "did not stop, killing" not in output
