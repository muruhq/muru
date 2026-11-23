"""Panels code in the worker adds with muru.panels reach the request's record."""

from __future__ import annotations

import json
import time
from pathlib import Path

from support import server as support

APP = """
from muru import panels

LIBRARY = panels.ValuesPanel("library", "Library")
STEPS = panels.SpansPanel("steps", "Steps")
ANALYSIS = panels.TextPanel("analysis", "Analysis", markdown=True)


def application(environ, start_response):
    library = LIBRARY.current()
    library["recording"] = library.active
    library["books"] = 7

    with STEPS.current().span("view", category="view"):
        with STEPS.current().span("query", category="database"):
            pass

    ANALYSIS.current().write("# Queries\\n\\nSee [the guide](https://example.com/guide).")
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
            found = json.loads(server.get(f"/_muru/toolbar/requests/{request_id}.json").body)

            if not found["in_progress"] and found["worker_part"] == "arrived":
                break

            assert time.monotonic() < deadline, found
            time.sleep(0.05)

        panels = {panel["name"]: panel for panel in found["panels"]}

        # After the built-in panels, with the values set and the spans on
        # the request's clock, after the queue.

        assert list(panels)[-3:] == ["library", "steps", "analysis"], list(panels)
        assert panels["library"]["values"] == [
            {"name": "recording", "value": "True"},
            {"name": "books", "value": "7"},
        ]
        view = panels["steps"]["spans"][0]
        assert view["name"] == "view" and view["children"][0]["name"] == "query"
        assert view["start_ms"] >= found["queue_ms"]

        drawn = server.get(f"/_muru/toolbar/requests/{request_id}/panels/steps.html").body.decode()
        assert "view" in drawn and "query" in drawn and "depth-2" in drawn, drawn

        # A markdown panel is drawn by the edge, its outside link opening
        # in a new tab.

        assert panels["analysis"]["markdown"] is True
        drawn = server.get(
            f"/_muru/toolbar/requests/{request_id}/panels/analysis.html"
        ).body.decode()
        assert "<h3>Queries</h3>" in drawn, drawn
        assert 'href="https://example.com/guide"' in drawn and 'target="_blank"' in drawn, drawn
    finally:
        server.stop()
