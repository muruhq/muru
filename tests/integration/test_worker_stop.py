"""How a worker stops (design specification section 7.6): the requests get
their time, and the shutdown work after them gets the exit grace."""

from __future__ import annotations

import http.client
import threading

import pytest
from support import server as support

SHORT = ["--timeout", "shutdown=500ms", "--timeout", "exit_grace=2s"]


def ask_in_the_background(server: support.Server) -> threading.Thread:
    """Send a request that will not finish, on a thread of its own."""

    def ask() -> None:
        connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=30)

        try:
            connection.request("GET", "/stuck")
            connection.getresponse().read()
        except OSError:
            pass

    thread = threading.Thread(target=ask, daemon=True)
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
