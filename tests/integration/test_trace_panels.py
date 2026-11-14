"""wrapture's events as the toolbar's Trace and SQL panels (issue 640), with
wrapture and its sqlite3 instrumentation, through muructl run."""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

import pytest
from support import server as support

pytestmark = pytest.mark.skipif(
    sys.version_info < (3, 12), reason="tracing needs wrapture, which needs Python 3.12 or later"
)

APP = """
import sqlite3


def application(environ, start_response):
    connection = sqlite3.connect(":memory:")
    connection.execute("CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT)")

    for book in range(3):
        connection.execute("SELECT title FROM book WHERE id = ?", (book,)).fetchall()

    connection.close()
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"shelved"]
"""

PROJECT = """
[tool.muru]
entrypoint = "shelf:application"
tracing = true
tracing_middleware = true

[[tool.wrapture.instrument]]
name = "sqlite3"
statement = true
"""


def test_a_requests_queries_are_its_trace_and_sql_panels(tmp_path: Path) -> None:
    pytest.importorskip("wrapture")
    pytest.importorskip("wrapture_instrumentation")

    (tmp_path / "shelf.py").write_text(APP)
    (tmp_path / "pyproject.toml").write_text(PROJECT)
    server = support.start(None, chdir=tmp_path, workers=1, args=["--toolbar"])

    try:
        response = server.get("/books")
        request_id = response.header("x-request-id")
        assert response.body == b"shelved" and request_id is not None

        deadline = time.monotonic() + 10

        while True:
            found = json.loads(server.get(f"/_muru/requests/{request_id}.json").body)

            if not found["in_progress"] and found["worker_part"] == "arrived":
                break

            assert time.monotonic() < deadline, found
            time.sleep(0.05)
    finally:
        server.stop()

    panels = {panel["name"]: panel for panel in found["panels"]}

    # The request is the tree's root, its queries beneath it, named by
    # their statements, after the queue on the request's clock.

    (request,) = panels["trace"]["spans"]
    assert request["name"] == "GET /books" and request["start_ms"] >= found["queue_ms"]
    names = [child["name"] for child in request["children"]]
    assert names.count("SELECT title FROM book WHERE id = ?") == 3, names

    # The same statement three times is one row of three.

    by_statement, each = panels["sql"]["tables"]
    assert ["SELECT title FROM book WHERE id = ?", "3"] == by_statement["rows"][0]["cells"][:2]
    assert len(each["rows"]) == 4
