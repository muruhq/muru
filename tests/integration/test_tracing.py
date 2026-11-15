"""Tracing with wrapture (section 11.6), end to end through muructl run.

wrapture is the application's dependency, not Muru's, so the project
carries a stand-in module of that name, which says on standard error what
it was asked to do and marks the responses of the applications it wraps.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from support import server as support

pytestmark = pytest.mark.skipif(
    sys.version_info < (3, 12), reason="tracing needs Python 3.12 or later"
)

WRAPTURE = """
import os
import sys


def say(message):
    print(f"TRACE {message} pid={os.getpid()}", file=sys.stderr, flush=True)


class AppliedConfig:
    started = False

    def start(self):
        # Idempotent, as wrapture's is.

        if not self.started:
            self.started = True
            say("start")


class Config:
    def __init__(self, source):
        self.source = source

    def apply(self, *, start=True):
        say(f"apply {self.source} start={start}")
        return AppliedConfig()


def find_config():
    return "pyproject.toml"


def load_config(source):
    return Config(source)


class WSGIMiddleware:
    def __init__(self, application, *, label):
        self.application = application
        self.label = label

    def __call__(self, environ, start_response):
        def marked(status, headers, exc_info=None):
            return start_response(status, [*headers, ("X-Traced", self.label)], exc_info)

        return self.application(environ, marked)


ASGIMiddleware = WSGIMiddleware
"""

APPS = """
def page(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"page"]


def plain(environ, start_response):
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"plain"]
"""

PROJECT = """
[tool.muru]
entrypoint = "traced_apps:page"
preload = "{preload}"
tracing = true

[tool.muru.mounts.plain]
path = "/plain"
entrypoint = "traced_apps:plain"
tracing_middleware = true
"""


def trace_lines(server: support.Server) -> list[str]:
    return [line[line.find("TRACE ") :] for line in server.log().splitlines() if "TRACE " in line]


def test_tracing_applies_in_the_host_starts_in_each_worker_and_wraps_what_it_is_asked(
    tmp_path: Path,
) -> None:
    (tmp_path / "wrapture.py").write_text(WRAPTURE)
    (tmp_path / "traced_apps.py").write_text(APPS)

    # Preloaded: the configuration is applied in the host with its outputs
    # held back, and each worker starts them. Only the mount asked for the
    # request middleware.

    (tmp_path / "pyproject.toml").write_text(PROJECT.format(preload="auto"))
    server = support.start(None, chdir=tmp_path, workers=2)

    try:
        assert server.get("/").header("X-Traced") is None
        assert server.get("/plain/").header("X-Traced") == "traced_apps:plain"

        lines = trace_lines(server)
        workers = {f"pid={pid}" for pid in server.worker_pids().values()}

        assert lines[0] == f"TRACE apply pyproject.toml start=False pid={server.host_pid()}", lines
        assert {line.split()[-1] for line in lines if line.startswith("TRACE start")} == workers
    finally:
        server.stop()

    # Per-worker imports: the worker applies and starts at once, so the
    # load is traced too.

    (tmp_path / "pyproject.toml").write_text(PROJECT.format(preload="false"))
    server = support.start(None, chdir=tmp_path, workers=1)

    try:
        assert server.get("/plain/").header("X-Traced") == "traced_apps:plain"

        worker = server.worker_pids()[0]

        assert trace_lines(server) == [
            f"TRACE apply pyproject.toml start=False pid={worker}",
            f"TRACE start pid={worker}",
        ]
    finally:
        server.stop()
