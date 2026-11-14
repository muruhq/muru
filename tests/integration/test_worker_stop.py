"""How a worker stops (design specification section 7.6): the requests get
their time, and the shutdown work after them gets the exit grace."""

from __future__ import annotations

import http.client
import shutil
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
        assert "STOP atexit used the loop" in log
    else:
        assert "STOP on_exit ran" in log

    assert log.index("STOP on_exit ran") < log.index("STOP atexit ran")


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


@pytest.mark.parametrize("fails", [False, True])
def test_atexit_callbacks_run_once_under_single(tmp_path: Path, fails: bool) -> None:
    server_args = ["--hooks", "stop_hooks", "--single"]
    env = {"MURU_TEST_FAIL_ON_READY": "1"} if fails else None

    if fails:
        with pytest.raises(RuntimeError) as start_failed:
            support.start("plain_wsgi:application", workers=None, args=server_args, env=env)

        output = str(start_failed.value)
    else:
        server = support.start("plain_wsgi:application", workers=None, args=server_args)
        server.stop()
        output = server.log()

    assert output.count("STOP atexit ran") == 1, output


LIFE_TOOL = """
import os, sys


def worker_started(context):
    print(f"STOP worker_started pid={os.getpid()}", file=sys.stderr, flush=True)


def worker_stopped(context):
    print(f"STOP worker_stopped pid={os.getpid()}", file=sys.stderr, flush=True)
"""


@pytest.mark.parametrize("fails", [False, True])
def test_worker_stopped_is_the_last_thing_a_worker_does(tmp_path: Path, fails: bool) -> None:
    # Section 6.15: worker_started just before post_fork, and worker_stopped
    # after on_exit and the atexit callbacks, also when the worker fails
    # to start.

    for name in ("stop_hooks.py", "plain_wsgi.py"):
        shutil.copy(support.FIXTURES / name, tmp_path)

    (tmp_path / "life_tool.py").write_text(LIFE_TOOL)
    (tmp_path / "pyproject.toml").write_text(
        "[tool.muru]\nentrypoint = 'plain_wsgi:application'\nhooks = 'stop_hooks'\n"
        "[[tool.muru.instrumentation]]\nmodule = 'life_tool'\n"
    )

    if fails:
        with pytest.raises(RuntimeError) as start_failed:
            support.start(
                None,
                workers=None,
                chdir=tmp_path,
                args=["--single"],
                env={"MURU_TEST_FAIL_ON_READY": "1"},
            )

        log = support.untimed(str(start_failed.value))
    else:
        server = support.start(None, workers=1, chdir=tmp_path, args=SHORT)
        server.stop()
        log = support.untimed(server.log())
        assert log.index("STOP on_exit ran") < log.index("STOP atexit ran")

    assert log.count("STOP worker_stopped") == 1, log
    assert log.index("STOP worker_started") < log.index("STOP atexit ran"), log
    assert log.index("STOP atexit ran") < log.index("STOP worker_stopped"), log
