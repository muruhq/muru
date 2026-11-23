"""Detours (design specification section 9.13, ADR 0075).

A development run, `muructl detour run`, serves an application through a
local daemon's edge as a release of kind detour for as long as it runs:
previews reach it, a WebSocket and a gRPC call go through it, a traceback
reaches the browser past the daemon's error override, and when it ends the
daemon's traffic is back on the serving release. One grouped test, since
each step needs the daemon, a deployed release and a detour.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.parse
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "detoured"
GREETER = support.ROOT / "tests" / "fixtures" / "projects" / "grpcsvc" / "greeter.proto"

# The daemon's own error page replaces a 500 its releases send, which a
# detour's traceback passes by.

DAEMON_CONFIG = """
[daemon]
detours = true

[error_override]
statuses = [500]
"""


class Detour:
    """A `muructl detour run` of a project against a daemon."""

    def __init__(
        self, project: Path, daemon: Daemon, *arguments: str, env: dict[str, str] | None = None
    ) -> None:
        muructl = support.muructl_path()
        assert muructl is not None
        self.log_path = project.parent / "detour.out"
        self.handle = self.log_path.open("ab")
        self.process = subprocess.Popen(
            [
                str(muructl),
                "detour",
                "run",
                "--socket",
                str(daemon.run_dir / "admin.sock"),
                "--python",
                sys.executable,
                "--processes",
                "1",
                "--no-reload",
                *arguments,
            ],
            cwd=project,
            stdout=self.handle,
            stderr=subprocess.STDOUT,
            env={**os.environ, **(env or {})},
        )
        self.wait_for_log("admin socket")

    def log(self) -> str:
        return self.log_path.read_text(errors="replace")

    def wait_for_log(self, text: str, timeout: float = 60.0) -> None:
        deadline = time.monotonic() + timeout

        while time.monotonic() < deadline:
            if text in self.log():
                return

            if self.process.poll() is not None:
                raise RuntimeError(f"the detour exited early:\n{self.log()}")

            time.sleep(0.1)

        raise RuntimeError(f"the detour never said {text!r}:\n{self.log()}")

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)

            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

        self.handle.close()


def generate(project: Path) -> tuple[ModuleType, ModuleType] | None:
    """The greeter's stubs, and a gRPC mount for them, where grpcio is installed."""

    try:
        import grpc  # noqa: F401
        import grpc_tools  # noqa: F401
    except ImportError:
        if os.environ.get("CI") and not support.free_threaded_build():
            raise

        return None

    shutil.copy(GREETER, project / "greeter.proto")
    subprocess.run(
        [
            sys.executable,
            "-m",
            "grpc_tools.protoc",
            f"-I{project}",
            f"--python_out={project}",
            f"--grpc_python_out={project}",
            str(project / "greeter.proto"),
        ],
        check=True,
    )

    with (project / "muru.toml").open("a") as table:
        table.write('\n[mounts.api]\ninterface = "grpc"\nentrypoint = "detoured:register"\n')

    sys.path.insert(0, str(project))
    import greeter_pb2
    import greeter_pb2_grpc

    return greeter_pb2, greeter_pb2_grpc


def detour_release(daemon: Daemon) -> dict[str, Any] | None:
    """The detour among the application's releases, as release list gives it."""

    listed = json.loads(daemon.muructl("release", "list", "detoured", "--json").stdout)
    return next(
        (release for release in listed["releases"] if release.get("kind") == "detour"), None
    )


def served(daemon: Daemon, path: str = "/", **headers: str) -> dict[str, Any]:
    """What the application said it saw, with the response's headers."""

    response = daemon.request(path, headers=headers)
    assert response.status == 200, response.body
    seen: dict[str, Any] = json.loads(response.body)
    seen["headers"] = {name.lower(): value for name, value in response.headers}
    return seen


def test_a_detour_is_previewed_served_through_and_closed(tmp_path: Path) -> None:
    project = tmp_path / "detoured"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    stubs = generate(project)

    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon", config_extra=DAEMON_CONFIG)
    detour = None

    try:
        daemon.deploy("detoured", project)
        before = served(daemon)
        assert not before["instance"].startswith("detour:")

        # --open-preview gives the browser an entry link as the detour
        # opens, which $BROWSER names here.

        opened = tmp_path / "opened"
        launcher = tmp_path / "browser"
        launcher.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{opened}'\n")
        launcher.chmod(0o755)
        detour = Detour(project, daemon, "--open-preview", env={"BROWSER": str(launcher)})
        detour.wait_for_log("in preview of")
        deadline = time.monotonic() + 20

        while not opened.exists() and time.monotonic() < deadline:
            time.sleep(0.05)

        first = urllib.parse.urlsplit(opened.read_text().split()[0])
        assert first.path.startswith("/_muru/preview/")
        assert daemon.request(first.path).status == 303
        found = detour_release(daemon)
        assert found is not None, daemon.muructl("release", "list").stdout
        release = found["id"]
        assert release.startswith("dev-")

        # Opening a detour changes nothing for other clients, and its name
        # alone does not reach it: a preview carries its secret.

        assert not served(daemon)["instance"].startswith("detour:")
        assert not served(daemon, **{"Muru-Preview": release})["instance"].startswith("detour:")
        header = daemon.muructl("detour", "preview", "--header", cwd=project).stdout.strip()
        name, value = header.split(": ", 1)
        assert name == "Muru-Preview" and value.startswith(f"{release}.")
        preview = {"Muru-Preview": value}

        # A one-time entry link sets the preview cookie and leads to the
        # application, once.

        link = daemon.muructl("detour", "preview", "--print", cwd=project).stdout.strip()
        entry = urllib.parse.urlsplit(link).path
        assert entry.startswith("/_muru/preview/")
        entered = daemon.request(entry)
        assert entered.status == 303 and entered.header("location") == "/"
        cookie = (entered.header("set-cookie") or "").split(";")[0]
        assert cookie == f"muru_preview={value}"
        assert served(daemon, Cookie=cookie)["instance"].startswith("detour:")
        assert daemon.request(entry).status == 404
        left = daemon.request("/_muru/preview/leave")
        assert left.status == 303 and "Max-Age=0" in (left.header("set-cookie") or "")

        # A preview reaches the run, which takes the daemon's request ID
        # and forwarding headers, and sends its own development headers.

        seen = served(daemon, **preview)
        assert seen["instance"].startswith("detour:"), seen
        assert seen["request_id"] == seen["headers"]["x-request-id"]
        assert seen["host"] == f"127.0.0.1:{daemon.port}"
        assert seen["server_port"] == str(daemon.port)
        assert seen["remote_addr"] == "127.0.0.1"
        assert "server-timing" in seen["headers"]
        assert "muru-preview" not in seen["headers"]
        assert seen["preview"] == ""

        # The run's traceback reaches the client past the daemon's error
        # override, which still replaces the serving release's.

        boom = daemon.request("/boom", headers={**preview, "Accept": "text/html"})
        assert boom.status == 500
        assert b"boom from the detoured fixture" in boom.body
        plain = daemon.request("/boom", headers={"Accept": "text/html"})
        assert plain.status == 500
        assert b"boom from the detoured fixture" not in plain.body

        # A WebSocket goes through the detour over HTTP/1.1, and a gRPC
        # call over h2c.

        websockets = pytest.importorskip("websockets.sync.client")

        with websockets.connect(
            f"ws://127.0.0.1:{daemon.port}/ws/echo", additional_headers=preview
        ) as socket:
            socket.send("hello")
            assert socket.recv(timeout=10).split() == [seen["instance"], "hello"]

        if stubs is not None:
            import grpc

            pb, rpc = stubs

            with grpc.insecure_channel(f"127.0.0.1:{daemon.port}") as channel:
                reply = rpc.GreeterStub(channel).SayHello(
                    pb.Hello(name="Ada"), metadata=(("muru-preview", value),), timeout=10
                )

            assert reply.message == f"{seen['instance']} Ada"

        # All of the application's traffic goes to the detour while it
        # says so, by an overlay it owns, and back again.

        switched = daemon.muructl("detour", "traffic", "all", cwd=project)
        assert "now takes all of detoured's traffic (was preview)" in switched.stdout
        assert served(daemon)["instance"].startswith("detour:")
        overlays = daemon.muructl("overlay", "list").stdout
        assert "detour:detoured" in overlays and "detour" in overlays
        refused = daemon.muructl("overlay", "end", "detour:detoured", check=False)
        assert refused.returncode != 0
        assert "muructl detour traffic" in refused.stderr
        detour.wait_for_log("now takes all of detoured's traffic")

        daemon.muructl("detour", "traffic", "detoured", "preview")
        assert not served(daemon)["instance"].startswith("detour:")
        assert served(daemon, **preview)["instance"].startswith("detour:")
        assert "detour:detoured" not in daemon.muructl("overlay", "list").stdout

        # The detour ends with its run, and the daemon's traffic is back on
        # the serving release at once.

        detour.stop()
        assert detour_release(daemon) is None
        assert not served(daemon, **preview)["instance"].startswith("detour:")

        # Its cookie reaches nothing now, and the response lets it go.

        stale = served(daemon, Cookie=cookie)
        assert not stale["instance"].startswith("detour:")
        assert "Max-Age=0" in stale["headers"].get("set-cookie", "")
        events = daemon.events()
        assert "detour.opened" in events and "detour.closed" in events
    finally:
        if detour is not None:
            detour.stop()

        daemon.stop()

        if stubs is not None:
            sys.path.remove(str(project))


def test_a_daemon_without_the_setting_refuses_a_detour(tmp_path: Path) -> None:
    project = tmp_path / "detoured"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")

    try:
        muructl = support.muructl_path()
        assert muructl is not None
        refused = subprocess.run(
            [
                str(muructl),
                "detour",
                "run",
                "--socket",
                str(daemon.run_dir / "admin.sock"),
                "--python",
                sys.executable,
                "--no-reload",
            ],
            cwd=project,
            capture_output=True,
            text=True,
            timeout=120,
        )
        assert refused.returncode != 0
        assert "detours = true in the [daemon] table" in refused.stderr + refused.stdout
    finally:
        daemon.stop()
