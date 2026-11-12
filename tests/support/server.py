"""Start `muructl run` for a fixture application and talk to it over HTTP."""

from __future__ import annotations

import contextlib
import fcntl
import http.client
import json
import os
import re
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import tomllib
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO

import pytest

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "apps"

LOG_TAIL_LINES = 80  # how much of the log a timed-out `wait_for_log` prints

# The full UTC time a line written to a file starts with (section 11.8 of
# the specification): muructl's output goes to a file here, so every line
# carries one.

TIMED = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z ", re.MULTILINE)


def untimed(text: str) -> str:
    """Return text with the time each line starts with taken off."""

    return TIMED.sub("", text)


def muructl_path() -> Path | None:
    """Return the muructl binary to test, or None when it is not built."""

    candidate = Path(os.environ.get("MURUCTL", ROOT / "target" / "debug" / "muructl"))
    return candidate if candidate.is_file() else None


def extension_available() -> bool:
    """Whether the native extension is importable in this interpreter."""

    try:
        from muru import _native  # noqa: F401
    except ImportError:
        return False

    return True


def unavailable(reason: str) -> None:
    """Skip the test for a missing binary or extension, or fail it under CI.

    CI builds both before the suite runs, so there a skip would hide a
    broken build (as when the extension was built for the wrong ABI).
    """

    if os.environ.get("CI"):
        pytest.fail(f"{reason}; CI must not skip the server tests")
    pytest.skip(reason)


# The directories short_dir has made. Some outlive the test that made them,
# for a module's daemon or a second daemon on the first one's run
# directory, so tests/conftest.py removes them when the session ends.

_short_dirs: list[Path] = []
_temporary_files: list[Path] = []


def short_dir(prefix: str) -> Path:
    """Return a new directory under /tmp, for sockets: a socket's path is
    limited to about a hundred bytes, which pytest's temporary directories
    on macOS come close to. It is removed when the test session ends."""

    directory = Path(tempfile.mkdtemp(prefix=prefix, dir="/tmp"))
    _short_dirs.append(directory)
    return directory


def temporary_log(prefix: str) -> Path:
    """Return a new file for a server's output, kept after the server
    stops for the test to read and removed when the session ends."""

    log = Path(tempfile.mkstemp(prefix=prefix, suffix=".log")[1])
    _temporary_files.append(log)
    return log


def short_dirs_made() -> int:
    """How many directories short_dir has made so far, for
    end_processes_since."""

    return len(_short_dirs)


def recorded_pids(directory: Path) -> set[int]:
    """The pids Muru's records under a directory name that are still one of
    Muru's processes: the edge's, each host's and each relay's records, and
    each development run's. A process sets a title in place of its command
    line, so the directory is not on it; the title says the pid has not
    passed to another process since the record was written."""

    pids: set[int] = set()

    for record in directory.glob("runs/*/run.json"):
        try:
            pids.add(json.loads(record.read_text())["pid"])
        except (OSError, ValueError, KeyError):
            continue

    for record in directory.rglob("*.toml"):
        try:
            pid = tomllib.loads(record.read_text()).get("pid")
        except (OSError, ValueError):
            continue

        if isinstance(pid, int) and pid > 0:
            command = subprocess.run(
                ["ps", "-o", "command=", "-p", str(pid)],
                capture_output=True,
                text=True,
                check=False,
            ).stdout.strip()

            if command.startswith(("muru-edge", "muru host ", "muru relay ")):
                pids.add(pid)

    return pids


def still_running(pids: set[int]) -> set[int]:
    """Those of the pids that a process still has."""

    running = set()

    for pid in pids:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            pass

        running.add(pid)

    return running


def end_processes_using(directory: Path) -> None:
    """End whatever still runs from a directory of the tests.

    A test that fails before it stops what it started leaves it running: a
    daemon told to detach leaves its edge and hosts, and a development run
    that was not reached leaves its own process. What names the directory
    on its command line is found by it, and an edge, a host, a relay or a
    development run by its record under it (a host's workers end with it).
    None gets a grace period, since a test is over and nothing depends on
    what it was doing.
    """

    found = subprocess.run(
        ["pgrep", "-f", str(directory)], capture_output=True, text=True, check=False
    )
    pids = {int(line) for line in found.stdout.split()} | recorded_pids(directory)
    pids.discard(os.getpid())

    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def end_processes_since(made: int) -> None:
    """End whatever still runs from the directories made since `made`
    of them existed, so that one test's failure does not outlive it."""

    for directory in _short_dirs[made:]:
        end_processes_using(directory)


def remove_short_dirs() -> None:
    """End whatever still runs from every directory short_dir made, then
    remove them and the temporary logs."""

    while _short_dirs:
        directory = _short_dirs.pop()
        end_processes_using(directory)
        shutil.rmtree(directory, ignore_errors=True)

    while _temporary_files:
        _temporary_files.pop().unlink(missing_ok=True)


# Ports free_port has handed out. A port is free again once its probe
# socket closes, so two picks before either is bound can return the same
# one, as a dashboard port and a daemon's listener once did on Linux. A
# lock file per port does the same between processes, two checkouts' test
# runs above all: the lock is held until the process ends, and a port some
# other process holds is passed over. The files are empty and stay behind.

PORT_LOCKS = Path("/tmp/muru-test-ports")

_handed_out: set[int] = set()
_port_locks: list[IO[bytes]] = []


def _reserve_port(port: int) -> bool:
    """Take this process's lock on a port, or say that another holds it."""

    PORT_LOCKS.mkdir(exist_ok=True)

    # The directory is shared by whoever runs the tests on this machine.

    with contextlib.suppress(PermissionError):
        PORT_LOCKS.chmod(0o1777)

    try:
        lock = (PORT_LOCKS / str(port)).open("ab")
    except PermissionError:
        return False

    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return False

    _port_locks.append(lock)
    return True


def free_port() -> int:
    """Pick a port that is free right now, not handed out before and not
    reserved by another process on this machine."""

    while True:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = int(sock.getsockname()[1])

        if port not in _handed_out and _reserve_port(port):
            _handed_out.add(port)
            return port


def dashboard_args(args: Sequence[str]) -> tuple[list[str], int | None]:
    """The --dashboard arguments for a run, and the port they name.

    A run's dashboard is on 127.0.0.1:8081 unless told otherwise, so every
    run the tests start would take the same port, and fail to when a run
    outside the tests holds it (issue 378). A free port is chosen unless
    `args` set the dashboard, or --allow-public, which turns it off.
    """

    if {"--dashboard", "--allow-public"} & set(args):
        return [], None

    port = free_port()
    return ["--dashboard", f"127.0.0.1:{port}"], port


@dataclass
class Response:
    """One HTTP response, fully read."""

    status: int
    reason: str
    headers: list[tuple[str, str]]
    body: bytes

    def header(self, name: str) -> str | None:
        """Return the value of a header, with repeats joined by a comma."""

        values = self.header_values(name)
        return ", ".join(values) if values else None

    def header_values(self, name: str) -> list[str]:
        """Return every value of a header, in order."""

        return [value for key, value in self.headers if key.lower() == name.lower()]


@dataclass
class Server:
    """A running `muructl run` serving one fixture application."""

    port: int
    process: subprocess.Popen[bytes]
    log_path: Path
    workers: int
    threads: int
    https_port: int | None = None
    dashboard_port: int | None = None
    _log_offset: int = field(default=0, repr=False)

    def request(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> Response:
        """Make one request on a fresh connection and read the whole response."""

        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)

        try:
            connection.request(method, path, body=body, headers=headers or {})
            raw = connection.getresponse()
            return Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def request_tls(
        self,
        method: str,
        path: str,
        cafile: Path,
        server_hostname: str = "localhost",
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> Response:
        """Make one request over HTTPS, trusting `cafile`, and read the whole response.

        The connection goes to 127.0.0.1 while the certificate is checked
        against `server_hostname`, which is also what the SNI carries.
        """

        assert self.https_port is not None, "the server was started without --https"
        context = ssl.create_default_context(cafile=cafile)
        connection = http.client.HTTPConnection("127.0.0.1", self.https_port, timeout=30)
        connection.sock = context.wrap_socket(
            socket.create_connection(("127.0.0.1", self.https_port), timeout=30),
            server_hostname=server_hostname,
        )

        try:
            connection.request(method, path, body=body, headers=headers or {})
            raw = connection.getresponse()
            return Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def get(self, path: str, headers: dict[str, str] | None = None) -> Response:
        """GET a path."""

        return self.request("GET", path, headers=headers)

    def post(self, path: str, body: bytes, headers: dict[str, str] | None = None) -> Response:
        """POST a body to a path."""

        return self.request("POST", path, body=body, headers=headers)

    def raw(self, data: bytes, timeout: float = 10.0) -> bytes:
        """Send bytes on a new connection and return everything until it closes.

        Requests should carry `Connection: close`, or the read waits for the
        timeout on a kept-alive connection.
        """

        with socket.create_connection(("127.0.0.1", self.port), timeout=timeout) as sock:
            sock.sendall(data)
            chunks = []

            while True:
                try:
                    chunk = sock.recv(65536)
                except TimeoutError:
                    break

                if not chunk:
                    break
                chunks.append(chunk)

            return b"".join(chunks)

    def log(self) -> str:
        """Return everything the server has written to its log so far, without the times."""

        return untimed(self.timed_log())

    def timed_log(self) -> str:
        """Return the log as written, each line starting with its time."""

        return self.log_path.read_text(errors="replace")

    def mark_log(self) -> None:
        """Remember the current end of the log for `log_since_mark`."""

        self._log_offset = self.log_path.stat().st_size

    def log_since_mark(self) -> str:
        """Return the log written since `mark_log`."""

        with self.log_path.open("rb") as handle:
            handle.seek(self._log_offset)
            return untimed(handle.read().decode(errors="replace"))

    def wait_for_log(self, text: str, timeout: float = 5.0) -> bool:
        """Wait until the log written since `mark_log` contains text.

        On a timeout the log since the mark is printed, which pytest shows
        with a failing test, so a wait that never ends can be diagnosed.
        """

        deadline = time.monotonic() + timeout

        while time.monotonic() < deadline:
            if text in self.log_since_mark():
                return True
            time.sleep(0.02)

        lines = self.log_since_mark().splitlines()
        print(f"{text!r} not in the log within {timeout} s; the last lines since the mark:")
        print("\n".join(lines[-LOG_TAIL_LINES:]))
        return False

    def worker_pids(self) -> dict[int, int]:
        """Return the current pid of each worker index, from the host's log."""

        pids: dict[int, int] = {}

        for line in self.log().splitlines():
            if "started as pid" in line:
                words = line.split()
                pids[int(words[words.index("worker") + 1])] = int(words[-1])

        return pids

    def host_pid(self) -> int:
        """Return the pid of the host process, from its log prefix."""

        for line in self.log().splitlines():
            if line.startswith("muru.host["):
                return int(line[len("muru.host[") : line.index("]")])

        raise AssertionError("the host has not logged anything")

    def stop(self) -> None:
        """Stop the server and its workers."""

        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)

            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def wait_for_port(port: int, process: subprocess.Popen[bytes], timeout: float = 60.0) -> None:
    """Wait until something accepts connections on the port."""

    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"muructl exited early with {process.returncode}")

        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)

    raise RuntimeError("muructl did not start listening in time")


def start(
    app: str | None = None,
    workers: int | None = 2,
    threads: int | None = 2,
    chdir: Path | None = FIXTURES,
    args: Sequence[str] = (),
    env: dict[str, str] | None = None,
    https: bool = False,
    host: str = "127.0.0.1",
    new_session: bool = False,
    reload: bool | None = False,
) -> Server:
    """Start muructl for `module:callable`, skipping the test when it cannot run.

    `app` may also be a project directory, or None to run the project in
    `chdir`; `workers`, `threads` and `chdir` are left to muructl's own
    defaults when None. `args` are further `muructl run` options and `env`
    extra environment variables. With `https`, a free HTTPS port is chosen
    and passed as --https-port (the caller adds --https, --https-only or
    --tls-cert). `host` is the address to bind, which is loopback unless a
    test needs the server reachable from a container (with --allow-public
    in `args`). With `new_session`, muructl leads a session of its own, so
    a test can signal its process group as a terminal's Ctrl-C would. When
    muructl exits before its workers are ready, the RuntimeError raised
    carries its output. Reloading is off unless `reload` is True, or None
    to leave it to muructl's default; a `--reload` or `--no-reload` in
    `args` is left as given. The dashboard is on a free port, recorded as
    the server's `dashboard_port`, unless `args` set --dashboard or
    --allow-public.
    """

    muructl = muructl_path()
    if muructl is None:
        unavailable("muructl is not built (cargo build -p muructl)")
    if not extension_available():
        unavailable("the muru extension is not built into this interpreter (just dev)")

    port = free_port()
    https_port = free_port() if https else None
    https_args = ["--https-port", str(https_port)] if https_port is not None else []
    dashboard, dashboard_port = dashboard_args(args)
    options: list[str] = []
    if workers is not None:
        options += ["--processes", str(workers)]
    if threads is not None:
        options += ["--threads", str(threads)]
    if chdir is not None:
        options += ["--chdir", str(chdir)]
    if reload is not None and not {"--reload", "--no-reload"} & set(args):
        options += ["--reload" if reload else "--no-reload"]
    log = temporary_log("muru-test-")

    with log.open("wb") as handle:
        process = subprocess.Popen(
            [
                str(muructl),
                "run",
                *([app] if app is not None else []),
                "--bind",
                f"{host}:{port}",
                "--python",
                sys.executable,
                *options,
                *https_args,
                *dashboard,
                *args,
            ],
            stdout=handle,
            stderr=subprocess.STDOUT,
            env={**os.environ, **(env or {})},
            start_new_session=new_session,
        )

    server = Server(port, process, log, workers or 0, threads or 0, https_port, dashboard_port)

    try:
        wait_for_port(port, process)

        if https_port is not None:
            wait_for_port(https_port, process)
    except RuntimeError as error:
        server.stop()
        raise RuntimeError(f"{error}\n{server.log()}") from error

    # muructl prints its startup lines once the listener accepts: serving,
    # the dashboard, then the admin socket (or "no admin socket"), with only
    # the watching line after it. Waiting for that line lets a test read
    # the startup lines whole (issue 309).

    server.wait_for_log("admin socket", timeout=10)
    return server


def text(response: Response) -> str:
    """Return the body of a fixture `respond()` reply without its `;end` marker."""

    assert response.body.endswith(b";end"), response.body
    return response.body[:-4].decode("utf-8", "surrogateescape")
