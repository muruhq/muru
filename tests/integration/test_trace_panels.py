"""wrapture's events as the toolbar's Trace, SQL, Outbound and Exceptions
panels (issues 640, 704 and 707), with wrapture and its sqlite3 and
urllib.request instrumentation, through muructl run."""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path
from typing import Any

import pytest
from support import server as support

pytestmark = pytest.mark.skipif(
    sys.version_info < (3, 12), reason="tracing needs wrapture, which needs Python 3.12 or later"
)

APP = """
import sqlite3
import urllib.request

import wrapture


def application(environ, start_response):
    path = environ["PATH_INFO"]

    if path == "/handled":
        try:
            raise LookupError("no such shelf")
        except LookupError as error:
            wrapture.note_exception(error)

        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"no shelf"]

    if path == "/broken":
        raise RuntimeError("the shelf fell")

    if path == "/outbound":
        for _ in range(2):
            address = f"http://{environ['HTTP_HOST']}/handled?shelf=1"

            try:
                urllib.request.urlopen(address, timeout=10)
            except urllib.error.HTTPError:
                pass

        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"asked"]

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

[[tool.wrapture.instrument]]
name = "urllib.request"
"""


def test_a_requests_queries_are_its_trace_and_sql_panels(tmp_path: Path) -> None:
    pytest.importorskip("wrapture")
    pytest.importorskip("wrapture_instrumentation")

    (tmp_path / "shelf.py").write_text(APP)
    (tmp_path / "pyproject.toml").write_text(PROJECT)
    server = support.start(None, chdir=tmp_path, workers=1, args=["--toolbar"])

    try:
        response = server.get("/books")
        assert response.body == b"shelved"
        found = record(server, response)

        handled = server.get("/handled")
        assert handled.status == 404
        handled_record = record(server, handled)

        broken = server.get("/broken")
        assert broken.status == 500
        broken_record = record(server, broken)

        outbound = server.get("/outbound")
        assert outbound.body == b"asked"
        outbound_record = record(server, outbound)
    finally:
        server.stop()

    panels = named(found)

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

    # An exception the application caught and noted is in the Exceptions
    # panel as handled, against the request it was noted in.

    assert exceptions(handled_record) == [["GET /handled", "LookupError: no such shelf (handled)"]]
    assert named(handled_record)["trace"]["spans"][0]["failed"] is True
    assert request["failed"] is False

    # One that escaped is there unmarked, and in the Errors panel's
    # traceback too.

    assert exceptions(broken_record) == [["GET /broken", "RuntimeError: the shelf fell"]]
    assert named(broken_record)["trace"]["spans"][0]["failed"] is True
    assert broken_record["from_worker"]["outcome"] == "failed"
    assert "RuntimeError: the shelf fell" in broken_record["from_worker"]["traceback"]

    # The application's calls to its own server are one endpoint called
    # twice, each call with its URL without the query and its status; a
    # request that made none has no Outbound panel.

    by_endpoint, each_call = named(outbound_record)["outbound"]["tables"]
    host = f"127.0.0.1:{server.port}"
    assert [row["cells"][:4] for row in by_endpoint["rows"]] == [["GET", host, "/handled", "2"]]
    assert [row["cells"][2:] for row in each_call["rows"]] == [
        ["GET", f"http://{host}/handled", "404"]
    ] * 2
    assert "outbound" not in panels


def record(server: support.Server, response: support.Response) -> dict[str, Any]:
    """The record of the request that gave `response`, once its worker's
    part has arrived."""

    request_id = response.header("x-request-id")
    assert request_id is not None
    deadline = time.monotonic() + 10

    while True:
        found: dict[str, Any] = json.loads(server.get(f"/_muru/requests/{request_id}.json").body)

        if not found["in_progress"] and found["worker_part"] == "arrived":
            return found

        assert time.monotonic() < deadline, found
        time.sleep(0.05)


def named(found: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {panel["name"]: panel for panel in found["panels"]}


def exceptions(found: dict[str, Any]) -> list[list[str]]:
    (table,) = named(found)["exceptions"]["tables"]
    return [row["cells"] for row in table["rows"]]
