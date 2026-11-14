"""Panels an extension adds in the worker reach the request's record (issue 677)."""

from __future__ import annotations

import json
import time
from pathlib import Path

from support import server as support

APP = """
from muru import _panels

library = _panels.panel("library", "Library", "values")
trace = _panels.panel("trace", "Trace", "spans")


def application(environ, start_response):
    started = _panels.now()
    _panels.add_value(library, "recording", _panels.recording())
    _panels.add_value(library, "books", 7)
    query = _panels.Span("query", started + 0.5, 1.0, "database")
    _panels.add_span(trace, _panels.Span("view", started, 2.0, "view", [query]))
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"listed"]
"""


def test_a_panel_added_in_the_worker_is_in_the_record_and_drawn(tmp_path: Path) -> None:
    (tmp_path / "panelled.py").write_text(APP)
    (tmp_path / "pyproject.toml").write_text("[tool.muru]\nentrypoint = 'panelled:application'\n")
    server = support.start(None, chdir=tmp_path, workers=1, args=["--toolbar"])

    try:
        response = server.get("/")
        request_id = response.header("x-request-id")
        assert response.body == b"listed" and request_id is not None

        deadline = time.monotonic() + 10

        while True:
            found = json.loads(server.get(f"/_muru/requests/{request_id}.json").body)

            if not found["in_progress"] and found["worker_part"] == "arrived":
                break

            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        panels = {panel["name"]: panel for panel in found["panels"]}

        # After the built-in panels, with the values set and the spans on
        # the request's clock, after the queue.

        assert list(panels)[-2:] == ["library", "trace"], list(panels)
        assert panels["library"]["values"] == [
            {"name": "recording", "value": "True"},
            {"name": "books", "value": "7"},
        ]
        view = panels["trace"]["spans"][0]
        assert view["name"] == "view" and view["children"][0]["name"] == "query"
        assert view["start_ms"] >= found["queue_ms"]

        drawn = server.get(f"/_muru/requests/{request_id}/panels/trace.html").body.decode()
        assert "view" in drawn and "query" in drawn and "depth-2" in drawn, drawn
    finally:
        server.stop()
