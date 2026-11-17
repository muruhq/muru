"""Deferring imports with unwrapt (section 6.17), end to end through muructl run.

unwrapt comes from the development environment, which the host runs in.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from support import server as support

pytest.importorskip("unwrapt")

APP = """
import sys

import heavy


def page(environ, start_response):
    # Whether the worker had loaded heavy before this request used it.

    before = "heavy" in sys.modules
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [f"{before} {heavy.VALUE}".encode()]
"""


def test_an_import_unwrapt_defers_loads_in_the_worker_on_first_use(tmp_path: Path) -> None:
    (tmp_path / "lazy_app.py").write_text(APP)
    (tmp_path / "heavy.py").write_text("VALUE = 42\n")
    (tmp_path / "pyproject.toml").write_text(
        '[tool.muru]\nentrypoint = "lazy_app:page"\nlazy_imports = true\n\n'
        '[tool.unwrapt.lazy_modules]\n"lazy_app" = ["heavy"]\n'
    )
    server = support.start(None, chdir=tmp_path, workers=1)

    try:
        # The host preloaded the application without heavy, and said so
        # as it forked; the worker loads it when the request uses it.

        assert server.get("/").body == b"False 42"
        assert server.get("/").body == b"True 42"
        assert "lazy_imports: 1 import deferred, none of them used before the fork" in server.log()
    finally:
        server.stop()
