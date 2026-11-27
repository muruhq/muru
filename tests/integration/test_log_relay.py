"""A release's log relay (design specification section 11.3, ADR 0059).

The relay reads the output of a host and its workers and writes it to the
release log, or to muructl's output under `muructl run`. It is replaced
when it dies, adopted with its host after a daemon restart, writes a
host's last lines before it exits, and takes a stalled destination on
itself, so requests are not held up.
"""

from __future__ import annotations

import http.client
import os
import re
import signal
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon

NOISY = """\
import sys


def application(environ, start_response):
    for number in range(20000):
        print(f"line {number}")

    sys.stdout.flush()
    start_response("200 OK", [("Content-Length", "2")])
    return [b"ok"]
"""


ECHO = """\
def application(environ, start_response):
    print("echo", environ["PATH_INFO"], flush=True)
    start_response("200 OK", [("Content-Length", "2")])
    return [b"ok"]
"""


STARTING = """\
import sys


def post_fork(context):
    print("STARTING post_fork", context.release, file=sys.stderr, flush=True)


def on_ready(context):
    print("STARTING on_ready", context.release, file=sys.stderr, flush=True)
"""


def wait_for(predicate: Callable[[], bool], timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def relay_pid(files_dir: Path) -> int:
    """The pid in the relay record of the first release's host, among the
    daemon's runtime files, or 0 while it is not there."""

    record = files_dir / "h" / "1" / "relay.toml"

    try:
        return int(record.read_text().split("=")[1])
    except (OSError, IndexError, ValueError):
        return 0


def test_a_relay_is_replaced_adopted_and_drains_at_stop(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        daemon.deploy("shop", SHOP)
        assert daemon.request("/first").status == 200
        wait_for(lambda: "shop: served /first" in daemon.muructl("logs", "shop").stdout)
        first = relay_pid(daemon.files_dir)
        assert first

        # A relay that dies is replaced, and is given the same pipes: the
        # workers' output goes on reaching the log.

        os.kill(first, signal.SIGKILL)
        wait_for(lambda: relay_pid(daemon.files_dir) not in (0, first))
        assert "output.relay_restarting" in daemon.events()
        assert daemon.request("/second").status == 200
        wait_for(lambda: "shop: served /second" in daemon.muructl("logs", "shop").stdout)

        # A daemon that restarts adopts the relay with its host.

        second = relay_pid(daemon.files_dir)
        daemon.stop(shutdown=False)
        assert daemon.request("/detached").status == 200
        daemon = start_daemon(tmp_path, run_dir=daemon.run_dir, port=daemon.port)
        assert relay_pid(daemon.files_dir) == second
        wait_for(lambda: "shop: served /detached" in daemon.muructl("logs", "shop").stdout)
    finally:
        daemon.stop()

    # A stop waits for the relay, so the host's last line is in the log.

    log = tmp_path / "logs" / "apps" / "shop" / "r1.log"
    assert "received SIGTERM, stopping" in log.read_text()


def test_a_relay_is_replaced_under_single(tmp_path: Path) -> None:
    """A host serving in its own process has nothing to answer a relay
    with, so the replacement has to be given its output by muructl."""

    (tmp_path / "echo.py").write_text(ECHO)
    server = support.start(
        "echo:application", workers=None, threads=None, chdir=tmp_path, args=["--single"]
    )

    try:
        assert server.get("/first").status == 200
        assert server.wait_for_log("echo /first"), server.log()
        written = re.search(r"pid \d+, written to (\S+)", server.log())
        assert written, server.log()
        record = Path(written.group(1)).parent / "h" / "1" / "relay.toml"
        first = int(record.read_text().split("=")[1])

        os.kill(first, signal.SIGKILL)
        wait_for(lambda: int(record.read_text().split("=")[1]) != first)

        assert server.get("/second").status == 200
        assert server.wait_for_log("echo /second", timeout=20.0), server.log()
    finally:
        server.stop()


def test_a_stalled_destination_holds_up_no_request(tmp_path: Path) -> None:
    """Standard error is a pipe nobody reads, so muructl's writer stalls
    behind it and the relay's queues fill. Requests go on being answered;
    once it is read, a line says what was dropped."""

    muructl = support.muructl_path()

    if muructl is None or not support.extension_available():
        support.unavailable("muructl and the extension must be built")

    (tmp_path / "noisy.py").write_text(NOISY)
    port = support.free_port()
    process = subprocess.Popen(
        [
            str(muructl),
            "run",
            "noisy:application",
            "--python",
            sys.executable,
            "--port",
            str(port),
            "--processes",
            "1",
            "--threads",
            "1",
            "--no-reload",
            "--dashboard",
            "off",
        ],
        cwd=tmp_path,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        support.wait_for_port(port, process)

        # Each request writes about half a megabyte: far more, over these
        # requests, than the pipes and the queue hold.

        for _ in range(16):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            connection.request("GET", "/")
            assert connection.getresponse().status == 200
            connection.close()

        process.send_signal(signal.SIGTERM)
        _out, err = process.communicate(timeout=60)
    finally:
        process.kill()

    text = err.decode()
    assert "of worker 0's output here: the log was not keeping up" in text, text[-2000:]


def test_a_new_releases_start_up_lines_come_before_the_switch(tmp_path: Path) -> None:
    """Issue 838: muructl has the new release's relay write what its
    workers wrote before they were ready, and writes that itself, before
    it says it has switched to the release."""

    workers = 2
    (tmp_path / "echo.py").write_text(ECHO)
    (tmp_path / "starting.py").write_text(STARTING)
    server = support.start(
        "echo:application",
        workers=workers,
        threads=1,
        chdir=tmp_path,
        args=["--hooks", "starting"],
        reload=True,
    )

    try:
        assert server.wait_for_log("watching", timeout=10)

        for version in range(3):
            # A whole second keeps the modification time distinct on
            # coarse file systems.

            server.mark_log()
            time.sleep(1.1)
            (tmp_path / "echo.py").write_text(ECHO + f"\nVERSION = {version}\n")
            assert server.wait_for_log("switched to release r", timeout=30)

        log = server.log()
    finally:
        server.stop()

    # A release whose start a further change abandoned has no switch line,
    # and its lines are not counted.

    switches = [
        (match.start(), match.group(1)) for match in re.finditer(r"switched to release (r\d+)", log)
    ]
    assert len(switches) == 3, log

    for at, release in switches:
        before = re.findall(rf"STARTING (post_fork|on_ready) {release}$", log[:at], re.MULTILINE)
        assert len(before) == 2 * workers, f"{release}:\n{log}"
