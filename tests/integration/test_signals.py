"""The signals Muru keeps for itself (issue 673), through muructl run.

Each process takes its signals as it starts, so a signal that arrives
while the application is still loading is handled: a stop unwinds the
load cleanly, and a reopen or reclaim neither kills the process nor is
lost. Python code cannot take them over.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from pathlib import Path

from support import server as support

# An application whose import says where it is loading, then takes a while.

SLOW_APP = """
import os
import sys
import time

print(f"IMPORTING pid={os.getpid()}", file=sys.stderr, flush=True)
time.sleep(float(os.environ.get("SLOW_IMPORT", "0")))


def application(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"slow"]
"""

# What the process does as it ends, and what the instrumentation module and
# the hooks hear.

TOOL = """
import atexit
import os
import sys


def say(message):
    print(f"{message} pid={os.getpid()}", file=sys.stderr, flush=True)


atexit.register(lambda: say("EXITED"))


def load_finished(context, outcome):
    say(f"LOADED {outcome}")


def worker_stopped(context):
    say("WORKER_STOPPED")
"""

HOOKS = """
import os
import sys
import time


def on_ready(context):
    print(f"READYING pid={os.getpid()}", file=sys.stderr, flush=True)
    time.sleep(float(os.environ.get("SLOW_READY", "0")))


def on_exit(context):
    print(f"ON_EXIT pid={os.getpid()}", file=sys.stderr, flush=True)


def on_reopen_logs(context):
    print(f"REOPENED pid={os.getpid()}", file=sys.stderr, flush=True)
"""

GREEDY = """
import signal

signal.signal(signal.SIGTERM, lambda signum, frame: None)


def application(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"greedy"]
"""


def project(directory: Path, preload: str) -> None:
    (directory / "slow_app.py").write_text(SLOW_APP)
    (directory / "slow_tool.py").write_text(TOOL)
    (directory / "slow_hooks.py").write_text(HOOKS)
    (directory / "pyproject.toml").write_text(
        f"[tool.muru]\nentrypoint = 'slow_app:application'\npreload = '{preload}'\n"
        "hooks = 'slow_hooks'\n[[tool.muru.instrumentation]]\nmodule = 'slow_tool'\n"
    )


def launch(
    directory: Path, slow: float, ready: float = 0
) -> tuple[subprocess.Popen[bytes], Path, int]:
    # muructl run, not waited on: the tests signal a process while it loads.

    muructl = support.muructl_path()
    if muructl is None or not support.extension_available():
        support.unavailable("muructl and the extension must be built")

    port = support.free_port()
    log = support.temporary_log("muru-signals-")

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "run",
                "--bind",
                f"127.0.0.1:{port}",
                "--python",
                sys.executable,
                "--processes",
                "1",
                "--chdir",
                str(directory),
                "--no-reload",
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
            env={**os.environ, "SLOW_IMPORT": str(slow), "SLOW_READY": str(ready)},
        )

    return process, log, port


def wait_for(log: Path, text: str, timeout: float = 20.0) -> str:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        for line in log.read_text(errors="replace").splitlines():
            if text in line:
                return line

        time.sleep(0.05)

    raise AssertionError(f"{text!r} not in the log:\n{log.read_text(errors='replace')}")


def pid_of(log: Path, marker: str) -> int:
    line = wait_for(log, f"{marker} pid=")
    return int(line.rsplit("pid=", 1)[1])


def stopped(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.send_signal(signal.SIGINT)

        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def test_a_stop_while_the_host_preloads_ends_it_cleanly(tmp_path: Path) -> None:
    project(tmp_path, preload="true")
    process, log, _ = launch(tmp_path, slow=30)

    try:
        host = pid_of(log, "IMPORTING")
        os.kill(host, signal.SIGTERM)
        process.wait(timeout=20)
    finally:
        stopped(process)

    text = log.read_text(errors="replace")

    # The load unwound, the instrumentation module heard why, and the host
    # ended as a Python process does, its atexit callbacks run.

    assert "received SIGTERM while loading, stopping" in text, text
    assert f"LOADED stopped pid={host}" in text, text
    assert f"EXITED pid={host}" in text, text
    assert "Traceback" not in text, text


def test_a_stop_while_a_worker_starts_unwinds_it(tmp_path: Path) -> None:
    # In the per-worker import, before the hooks module is loaded or the
    # worker started: the load hears it was stopped, and the worker ends
    # cleanly.

    project(tmp_path, preload="false")
    process, log, _ = launch(tmp_path, slow=30)

    try:
        worker = pid_of(log, "IMPORTING")
        os.kill(worker, signal.SIGTERM)
        wait_for(log, f"EXITED pid={worker}")
    finally:
        stopped(process)

    text = log.read_text(errors="replace")

    assert "worker 0 received SIGTERM while starting" in text, text
    assert f"LOADED stopped pid={worker}" in text, text
    assert f"(pid {worker}) exited with status 0" in text, text
    assert "Traceback" not in text, text

    # In on_ready, once the worker has started: its shutdown runs as a
    # stop's does, on_exit and worker_stopped included.

    process, log, _ = launch(tmp_path, slow=0, ready=30)

    try:
        worker = pid_of(log, "READYING")
        os.kill(worker, signal.SIGTERM)
        wait_for(log, f"WORKER_STOPPED pid={worker}")
    finally:
        stopped(process)

    text = log.read_text(errors="replace")

    assert "worker 0 received SIGTERM while starting" in text, text
    assert f"ON_EXIT pid={worker}" in text, text
    assert f"EXITED pid={worker}" in text, text


def test_a_reopen_while_a_worker_imports_is_acted_on_once_it_serves(tmp_path: Path) -> None:
    # SIGUSR1's default action would end the worker; taken early, it is
    # held for the native server, which reopens once it serves.

    project(tmp_path, preload="false")
    process, log, port = launch(tmp_path, slow=1.5)

    try:
        worker = pid_of(log, "IMPORTING")
        os.kill(worker, signal.SIGUSR1)
        support.wait_for_port(port, process)
        wait_for(log, f"REOPENED pid={worker}")
    finally:
        stopped(process)

    text = log.read_text(errors="replace")

    assert f"LOADED serve pid={worker}" in text, text
    assert "exited" not in text.split(f"IMPORTING pid={worker}", 1)[1].split("REOPENED")[0], text


def test_a_handler_for_a_signal_muru_keeps_is_refused_and_the_worker_still_stops(
    tmp_path: Path,
) -> None:
    (tmp_path / "greedy_app.py").write_text(GREEDY)
    (tmp_path / "slow_tool.py").write_text(TOOL)

    for preload in ("false", "auto"):
        (tmp_path / "pyproject.toml").write_text(
            f"[tool.muru]\nentrypoint = 'greedy_app:application'\npreload = '{preload}'\n"
            "[[tool.muru.instrumentation]]\nmodule = 'slow_tool'\n"
        )
        server = support.start(None, chdir=tmp_path, workers=1)

        try:
            assert server.get("/").body == b"greedy"
            worker = server.worker_pids()[0]
        finally:
            server.stop()

        log = server.log()

        # Refused where the application was imported, saying where it came
        # from, and the worker stopped gracefully.

        assert "signal.signal(SIGTERM) from greedy_app:" in log, (preload, log)
        assert f"WORKER_STOPPED pid={worker}" in log, (preload, log)
        assert f"EXITED pid={worker}" in log, (preload, log)


def test_stopping_the_run_while_its_worker_imports_stops_it_then(tmp_path: Path) -> None:
    # muructl run hears Ctrl-C while the workers come up, not once they
    # are ready, and the stop reaches the worker in its import.

    project(tmp_path, preload="false")
    process, log, _ = launch(tmp_path, slow=30)

    try:
        worker = pid_of(log, "IMPORTING")
        asked = time.monotonic()
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
        took = time.monotonic() - asked
    finally:
        stopped(process)

    text = log.read_text(errors="replace")

    assert took < 5, (took, text)
    assert "received SIGTERM, stopping" in text, text
    assert f"LOADED stopped pid={worker}" in text, text
    assert f"EXITED pid={worker}" in text, text
