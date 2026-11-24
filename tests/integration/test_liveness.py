"""The GIL stall timeout (design specification section 7.6).

A thread in every worker takes the interpreter lock every half second, and
a watchdog outside Python replaces the worker, as for the hard timeout,
when no thread has managed to for the GIL stall timeout: a C call that
never returns with the lock held, or a deadlock. The host replaces a
worker whose reports have stopped for the same period, such as one
stopped by a signal.
"""

from __future__ import annotations

import json
import os
import signal
import threading
import time
from collections.abc import Callable
from typing import Any

import pytest
from support import server as support


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def pid_serving(server: support.Server) -> int | None:
    """The pid that answers, or None while no worker does."""

    response = server.get("/info")

    if response.status != 200:
        return None

    return int(json.loads(response.body)["pid"])


def kill_capture(server: support.Server, request_id: str) -> dict[str, Any]:
    """The capture of a worker's kill in the record of `request_id`, once
    it has one (issue 715)."""

    deadline = time.monotonic() + 15

    while True:
        found = json.loads(server.get(f"/_muru/toolbar/requests/{request_id}.json").body)
        kills = [c for c in found.get("captures", []) if c["kind"] == "worker.timeout_killed"]

        if kills:
            return kills[0]

        assert time.monotonic() < deadline, found
        time.sleep(0.1)


def start(gil_stall: str, app: str = "plain_wsgi:application") -> support.Server:
    return support.start(
        app,
        workers=1,
        threads=2,
        args=[
            "--timeout",
            f"gil_stall={gil_stall}",
            "--timeout",
            "hard_drain_grace=1s",
            "--timeout",
            "shutdown=1s",
        ],
    )


@pytest.mark.skipif(
    not support.workers_have_a_gil(),
    reason="a free-threaded build has no interpreter lock to hold",
)
@pytest.mark.parametrize("app", ["plain_wsgi:application", "plain_asgi:application"])
def test_a_worker_holding_the_gil_is_replaced(app: str) -> None:
    server = start("2s", app)

    try:
        stuck = pid_serving(server)
        assert stuck is not None
        answers: dict[str, support.Response] = {}

        def ask(path: str) -> None:
            answers[path] = server.get(path, headers={"Accept": "application/json"})

        held = threading.Thread(target=ask, args=("/hold?seconds=60",), daemon=True)
        held.start()

        # Issue 836: a second request reaches the worker while the first
        # holds the lock. The run records its requests, so the worker
        # keeps each one's log records, which it must not wait on the
        # lock to start. Issue 842: an ASGI request must not wait on it
        # to start its task on the loop either.

        time.sleep(0.5)
        waiting = threading.Thread(target=ask, args=("/info",), daemon=True)
        waiting.start()

        # Nothing in the worker runs Python while the call holds the lock,
        # so the native watchdog tells the host, which replaces the worker
        # as for the hard timeout, with its stacks in the log.

        wait_for(lambda: "worker.gil_stalled" in server.log())
        wait_for(lambda: "worker.timeout_killed" in server.log())
        log = server.log()
        assert "past the GIL stall timeout of 2 s" in log, log
        assert "no thread of it ran Python for the GIL stall timeout of 2 s" in log, log
        assert "hold_the_interpreter" in log, "no stack dump"

        # Both requests are answered when the worker is condemned, not
        # when it is finally killed, since neither can finish.

        held.join(timeout=10)
        waiting.join(timeout=10)
        assert set(answers) == {"/hold?seconds=60", "/info"}, answers

        for answer in answers.values():
            assert answer.status == 502, answer.body
            assert json.loads(answer.body)["reason"] == "worker_stalled", answer.body

        assert "gave up 2 requests for the edge to answer" in server.log()

        # Issue 715: the worker could not answer for its stacks, but named
        # the request it gave up, so its record has the kill.

        answer = answers["/hold?seconds=60"]
        request_id = answer.header("x-request-id")
        assert request_id is not None
        kill = kill_capture(server, request_id)
        assert kill["worker"] == "0" and kill["pid"] == stuck, kill
        assert "GIL stall timeout of 2 s" in kill["reason"], kill
        assert "replaced by pid" in kill["message"], kill
        wait_for(lambda: pid_serving(server) not in (None, stuck))
        assert server.get("/sleep?seconds=0.1").status == 200
    finally:
        server.stop()


def test_a_worker_that_stops_reporting_is_replaced() -> None:
    server = start("2s")

    try:
        stopped = pid_serving(server)
        assert stopped is not None
        answers: list[support.Response] = []
        thread = threading.Thread(
            target=lambda: answers.append(server.get("/sleep?seconds=30")), daemon=True
        )
        thread.start()
        time.sleep(0.5)
        os.kill(stopped, signal.SIGSTOP)

        # A stopped process sends no report, from Python or outside it,
        # so the host notices the silence and replaces the worker.

        wait_for(lambda: "worker.timeout_killed" in server.log())
        log = server.log()
        assert "sent no report for" in log and "GIL stall timeout of 2 s" in log, log
        wait_for(lambda: pid_serving(server) not in (None, stopped))
        assert server.get("/").status == 200

        # Issue 715: a worker stopped whole names nothing, so the request
        # the edge still had in flight on it takes the kill.

        thread.join(timeout=20)
        assert answers, "the request on the stopped worker was not answered"
        request_id = answers[0].header("x-request-id")
        assert request_id is not None
        kill = kill_capture(server, request_id)
        assert kill["worker"] == "0" and kill["pid"] == stopped, kill
        assert "sent no report for" in kill["reason"], kill
    finally:
        try:
            os.kill(stopped, signal.SIGCONT)
        except ProcessLookupError:
            pass

        server.stop()
