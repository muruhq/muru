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
from support import http2
from support import instances as instances_support
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
        self.start = self.handle.tell()
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
        """What this run has written, after what runs before it wrote."""

        with self.log_path.open("rb") as log:
            log.seek(self.start)
            return log.read().decode(errors="replace")

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
        detour = Detour(
            project, daemon, "--open-preview", "--toolbar", env={"BROWSER": str(launcher)}
        )
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

        # The toolbar is the developer's: a page reached by the preview
        # gets it, and its files and records answer the preview.

        document = {"Sec-Fetch-Dest": "document"}
        page = daemon.request("/page", headers={**preview, **document})
        assert b'src="/_muru/toolbar/toolbar.js"' in page.body, page.body
        assert daemon.request("/_muru/toolbar/toolbar.js", headers=preview).status == 200
        assert daemon.request("/_muru/toolbar/history", headers=preview).status == 200

        # It answers whatever name the daemon routed to the detour, and
        # still refuses another site's page.

        for name in ("shop.lan", "x.echo.test"):
            assert daemon.request("/_muru/toolbar/toolbar.js", name, headers=preview).status == 200
        cross = {**preview, "Sec-Fetch-Site": "cross-site"}
        assert daemon.request("/_muru/toolbar/history", headers=cross).status == 403

        # The detour is listed, with no secret, and its run is an instance
        # of kind detour that --instance reaches by its tag.

        listed = daemon.muructl("detour", "list", "--json")
        assert [detour["release"] for detour in json.loads(listed.stdout)["detours"]] == [release]
        assert value.split(".")[1] not in listed.stdout
        muructl = support.muructl_path()
        assert muructl is not None
        instances = subprocess.run(
            [
                str(muructl),
                "instance",
                "list",
                "--kind",
                "detour",
                "--label",
                instances_support.label(),
                "--json",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        tags = [found["selector"] for found in json.loads(instances.stdout)["instances"]]
        assert tags == [seen["instance"]], tags
        status = subprocess.run(
            [str(muructl), "status", "--instance", tags[0]],
            capture_output=True,
            text=True,
            check=True,
        )
        assert f"detour detoured@{release}" in status.stdout, status.stdout

        # The daemon counts the detour's requests under its own release
        # and the group detour, which conditions, gates and the
        # autoscaler leave out.

        metrics = daemon.muructl("metrics").stdout
        assert f'release="{release}",group="detour"' in metrics, metrics

        # The run's traceback reaches the client past the daemon's error
        # override, which still replaces the serving release's.

        boom = daemon.request("/boom", headers={**preview, "Accept": "text/html"})
        assert boom.status == 500
        assert b"boom from the detoured fixture" in boom.body
        plain = daemon.request("/boom", headers={"Accept": "text/html"})
        assert plain.status == 500
        assert b"boom from the detoured fixture" not in plain.body

        # A WebSocket goes through the detour over HTTP/1.1, whether the
        # client opened it over HTTP/1.1 or HTTP/2, and a gRPC call over
        # h2c.

        websockets = pytest.importorskip("websockets.sync.client")

        with websockets.connect(
            f"ws://127.0.0.1:{daemon.port}/ws/echo", additional_headers=preview
        ) as socket:
            socket.send("hello")
            assert socket.recv(timeout=10).split() == [seen["instance"], "hello"]

        status, echoed = http2.echo_over_http2(
            daemon.port, "/ws/echo", "over-h2", f"127.0.0.1:{daemon.port}", preview
        )
        assert status == 200
        assert echoed.split() == [seen["instance"], "over-h2"]

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

        # A client the traffic choice sends to the detour gets the page as
        # the application made it, and none of the run's records, even
        # naming the header the daemon's edge tells the run with.

        page = daemon.request("/page", headers=document)
        assert page.status == 200 and b"_muru/toolbar" not in page.body, page.body

        for forged in ({}, {"X-Muru-Detour": "preview"}):
            for path in ("/_muru/toolbar/toolbar.js", "/_muru/toolbar/history"):
                assert daemon.request(path, headers=forged).status == 404, (path, forged)

        page = daemon.request("/page", headers={**preview, **document})
        assert b'src="/_muru/toolbar/toolbar.js"' in page.body, page.body

        daemon.muructl("detour", "traffic", "detoured", "preview")
        assert not served(daemon)["instance"].startswith("detour:")
        assert served(daemon, **preview)["instance"].startswith("detour:")
        assert "detour:detoured" not in daemon.muructl("overlay", "list").stdout

        # A timed choice goes back to preview on its own, and preview
        # takes no time.

        reverted = detour.log().count("now takes previews only") + 1
        refused = daemon.muructl(
            "detour", "traffic", "preview", "--for", "1m", cwd=project, check=False
        )
        assert refused.returncode != 0 and "preview takes neither" in refused.stderr
        timed = daemon.muructl("detour", "traffic", "all", "--for", "2s", "--json", cwd=project)
        assert json.loads(timed.stdout)["until_unix_ms"] > 0
        assert served(daemon)["instance"].startswith("detour:")
        assert " until " in daemon.muructl("detour", "list").stdout
        deadline = time.monotonic() + 15

        while served(daemon)["instance"].startswith("detour:") and time.monotonic() < deadline:
            time.sleep(0.2)

        assert not served(daemon)["instance"].startswith("detour:")
        listed = json.loads(daemon.muructl("detour", "list", "--json").stdout)["detours"]
        assert [(found["traffic"], found.get("until_unix_ms", 0)) for found in listed] == [
            ("preview", 0)
        ]
        assert "the time it was set for is over" in daemon.muructl("events").stdout

        # The run heard of both changes.

        detour.wait_for_log("now takes all of detoured's traffic, until ")
        deadline = time.monotonic() + 10

        while (
            detour.log().count("now takes previews only") < reverted and time.monotonic() < deadline
        ):
            time.sleep(0.1)

        assert detour.log().count("now takes previews only") >= reverted, detour.log()

        # Behind a route at a sub path the toolbar lives under the route,
        # so its requests reach the detour as the page's do: the route's
        # path when it strips nothing, the forwarded prefix when it strips.

        hosts = tmp_path / "hosts.toml"
        hosts.write_text(
            '[host.any]\ndefault = true\n\n[[route]]\nhost = "any"\napp = "detoured"\n\n'
            '[host.sub]\nnames = ["sub.test"]\n\n'
            '[[route]]\nhost = "sub"\npath = "/app"\napp = "detoured"\n\n'
            '[host.strip]\nnames = ["strip.test"]\n\n'
            '[[route]]\nhost = "strip"\npath = "/app"\napp = "detoured"\n'
            'strip_prefix = "/app"\n'
        )
        daemon.muructl("apply", "-f", str(hosts))

        for name in ("sub.test", "strip.test"):
            page = daemon.request("/app/page", name, headers={**preview, **document})
            assert b'src="/app/_muru/toolbar/toolbar.js"' in page.body, (name, page.body)
            script = daemon.request("/app/_muru/toolbar/toolbar.js", name, headers=preview)
            assert script.status == 200, (name, script.body)
            unpreviewed = daemon.request("/app/_muru/toolbar/history", name)
            assert not json.loads(unpreviewed.body)["instance"].startswith("detour:")

        assert (
            daemon.request("/_muru/toolbar/toolbar.js", "sub.test", headers=preview).status == 404
        )

        # Behind the stripping route the detour sees what the release it
        # stands in for does; for the stripped part itself it is sent the
        # path /, as any upstream is.

        through = daemon.request("/app/foo", "strip.test", headers=preview)
        release = daemon.request("/app/foo", "strip.test")

        for response in (through, release):
            report = json.loads(response.body)
            assert (report["script_name"], report["path_info"]) == ("/app", "/foo")

        assert json.loads(through.body)["instance"].startswith("detour:")
        bare_part = json.loads(daemon.request("/app", "strip.test", headers=preview).body)
        assert (bare_part["script_name"], bare_part["path_info"]) == ("/app", "/")

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

        # A checkout whose mounts differ from the serving release's says
        # so as its detour opens, and serves its own; with --mounts
        # release it serves the release's.

        bare = tmp_path / "bare"
        bare.mkdir()
        for module in project.glob("*.py"):
            shutil.copy(module, bare / module.name)

        (bare / "muru.toml").write_text('entrypoint = "detoured:wsgi_app"\nthreads = 2\n')
        detour = Detour(bare, daemon, "detoured")
        detour.wait_for_log("on detour detoured@")
        assert "warning: the serving release detoured@" in detour.log(), detour.log()
        assert "mounts detoured:asgi_app at /ws (auto) as ws, which this checkout does not" in (
            detour.log()
        )
        assert "--mounts release" in detour.log()
        bare_value = daemon.muructl("detour", "preview", "--header", cwd=bare).stdout
        bare_preview = dict([bare_value.strip().split(": ", 1)])
        own = served(daemon, "/ws/x", **bare_preview)
        assert own["instance"].startswith("detour:")
        assert (own["script_name"], own["path_info"]) == ("", "/ws/x")
        detour.stop()

        detour = Detour(bare, daemon, "detoured", "--mounts", "release")
        detour.wait_for_log("on detour detoured@")
        assert "serving the mounts of the serving release detoured@" in detour.log()
        assert "warning: the serving release" not in detour.log()
        bare_value = daemon.muructl("detour", "preview", "--header", cwd=bare).stdout
        bare_preview = dict([bare_value.strip().split(": ", 1)])

        with websockets.connect(
            f"ws://127.0.0.1:{daemon.port}/ws/echo", additional_headers=bare_preview
        ) as socket:
            socket.send("mounted")
            assert socket.recv(timeout=10).split()[1] == "mounted"

        detour.stop()
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
