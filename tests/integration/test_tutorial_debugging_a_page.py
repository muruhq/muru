"""The steps of the debugging tutorial, with the tutorial's own files.

docs/tutorials/debugging-a-page.md includes the files under
docs/tutorials/code/shop, and this test runs its shop through the page's
steps in one development run, making each fix the page makes, from
joined.py and fixed.py, and reading each record as JSON rather than in a
browser. The soft timeout is the page's 2 s.
"""

from __future__ import annotations

import ast
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import pytest
from support import server as support

pytestmark = pytest.mark.skipif(
    sys.version_info < (3, 12), reason="tracing needs wrapture, which needs Python 3.12 or later"
)

SHOP = support.ROOT / "docs" / "tutorials" / "code" / "shop"

DOCUMENT = {"Sec-Fetch-Dest": "document"}

# The two lines of the loop in shop.py's report.

LOOP = ("for book_id in range(400_000_000):", "total += book_id % 7")


def test_the_debugging_tutorial(tmp_path: Path) -> None:
    pytest.importorskip("flask")
    pytest.importorskip("wrapture")
    pytest.importorskip("wrapture_instrumentation")

    for name in ("shop.py", "pyproject.toml"):
        shutil.copy(SHOP / name, tmp_path / name)

    server = support.start(None, chdir=tmp_path, workers=1, threads=4, reload=True)

    try:
        assert server.wait_for_log("watching")

        # Server-Timing, and the request's page on the dashboard.

        plain = server.get("/api/books")
        request_id = plain.header("x-request-id")
        assert request_id is not None
        timing = plain.header("server-timing") or ""
        assert "queue;dur=" in timing and "app;dur=" in timing and request_id in timing
        assert (plain.header("muru-request-link") or "").endswith(f"/request/{request_id}")

        # The toolbar on the page, and the list's N+1 queries in its SQL
        # panel, then one statement once the authors are joined.

        listed = page(server, "/")
        assert b'<script src="/_muru/toolbar.js"' in listed.body
        first = record(server, listed)
        assert ["SELECT name FROM author WHERE id = ?", "7"] == statements(first)[0][:2]

        fix(server, tmp_path, "joined.py", "books")
        joined = record(server, page(server, "/"))
        assert [row[1] for row in statements(joined)] == ["1"]

        # The failing page has the toolbar on Flask's own error page, the
        # exception in Errors and Exceptions, and its spans failed.

        failing = server.get("/authors/4", headers=DOCUMENT)
        assert failing.status == 500
        assert b"Internal Server Error" in failing.body
        assert b'<script src="/_muru/toolbar.js"' in failing.body
        failed = record(server, failing)
        errors = {value["name"]: value["value"] for value in panel(failed, "errors")["values"]}
        assert errors["answered"] == "the application answered 500 itself"
        assert errors["logged"].endswith("ZeroDivisionError: division by zero"), errors
        assert rows(failed, "exceptions") == [
            ["GET /authors/4", "ZeroDivisionError: division by zero (handled)"],
            ["author", "ZeroDivisionError: division by zero"],
        ]
        (request,) = panel(failed, "trace")["spans"]
        assert request["failed"] and request["children"][0]["failed"]

        fix(server, tmp_path, "fixed.py", "author")
        assert b"No books yet" in page(server, "/authors/4").body

        # The stock service called once per book, as one endpoint, then
        # once for the page.

        assert rows(joined, "outbound")[0][2:4] == ["/stock/{id}", "7"]
        fix(server, tmp_path, "fixed.py", "books", "in_stock")
        batched = record(server, page(server, "/"))
        assert [row[2:4] for row in rows(batched, "outbound")] == [["/stock", "1"]]

        # Neither fault's hint is left. A call to the stock service can
        # still be slow enough on a loaded machine for a hint of its own,
        # which says nothing of the fixes (issue 796).

        hints = [hint[0] for hint in rows(batched, "hints")]
        assert not [hint for hint in hints if hint.startswith("the same ")], hints

        # A form that redirects: the POST and the page it led to are one
        # entry of the history, and the POST's record has its log line.

        posted = server.request(
            "POST",
            "/books",
            body=b"title=Middlemarch&author=3",
            headers={**DOCUMENT, "Content-Type": "application/x-www-form-urlencoded"},
        )
        assert posted.status == 302
        page(server, "/")
        entries = json.loads(server.get("/_muru/history").body)["entries"]
        steps = entries[0]["steps"]
        assert [(step["method"], step["target"]) for step in steps] == [
            ("POST", "/books"),
            ("GET", "/"),
        ]
        added = record(server, posted)
        assert "added 'Middlemarch'" in added["from_worker"]["logs"][0]["message"]

        # A request past the soft timeout, with its stack in its record.

        report = server.get("/report", headers=DOCUMENT)
        assert report.status == 504
        (capture,) = record(server, report)["captures"]
        assert capture["kind"] == "request.soft_timeout"

        # The stack ends in report, on one of its loop's two lines: the
        # soft timeout can sample the thread on either.

        *_, frame, line = capture["stack"].splitlines()
        assert frame.endswith(", in report"), capture["stack"]
        assert line.strip() in LOOP, capture["stack"]

        # Hints on the first traced visit to the list, kept from before
        # its fixes.

        hints = [hint[0] for hint in rows(first, "hints")]
        assert hints[0].startswith("the same statement ran 7 times"), hints
        assert hints[1].startswith("the same endpoint was called 7 times"), hints
        assert hints[1].endswith("/stock/{id}"), hints

        # The JSON endpoint's record from the command line.

        printed = subprocess.run(
            [str(support.muructl_path()), "debug", "request", request_id],
            cwd=tmp_path,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        assert printed.startswith("GET /api/books HTTP/1.1  200")
        assert "\nHints\n  What stands out\n    none\n" in printed
    finally:
        server.stop()


def fix(server: support.Server, project: Path, source: str, *names: str) -> None:
    """Put the functions `names` of the tutorial's `source` in place of
    shop.py's own, as the page does, and wait for the run to reload."""

    shop = project / "shop.py"
    text = shop.read_text()
    fixed = (SHOP / source).read_text()

    for name in names:
        text = text.replace(function(text, name), function(fixed, name))

    server.mark_log()
    shop.write_text(text)
    assert server.wait_for_log("switched to release r", timeout=30)


def function(source: str, name: str) -> str:
    """A top-level function of `source`, with its decorators."""

    node = next(
        node
        for node in ast.parse(source).body
        if isinstance(node, ast.FunctionDef) and node.name == name
    )
    lines = source.splitlines(keepends=True)
    start = min([node.lineno, *(decorator.lineno for decorator in node.decorator_list)])
    return "".join(lines[start - 1 : node.end_lineno])


def page(server: support.Server, path: str) -> support.Response:
    """A page as a browser loads it, which answers 200."""

    response = server.get(path, headers=DOCUMENT)
    assert response.status == 200, response.body
    return response


def record(server: support.Server, response: support.Response) -> dict[str, Any]:
    """The record of the request that gave `response`, once complete."""

    request_id = response.header("x-request-id")
    assert request_id is not None
    deadline = time.monotonic() + 10

    while True:
        found: dict[str, Any] = json.loads(server.get(f"/_muru/requests/{request_id}.json").body)

        if not found["in_progress"] and found["worker_part"] == "arrived":
            return found

        assert time.monotonic() < deadline, found
        time.sleep(0.05)


def panel(found: dict[str, Any], name: str) -> dict[str, Any]:
    return next(panel for panel in found["panels"] if panel["name"] == name)


def rows(found: dict[str, Any], name: str) -> list[list[str]]:
    """The rows of a panel's first table."""

    return [row["cells"] for row in panel(found, name)["tables"][0]["rows"]]


def statements(found: dict[str, Any]) -> list[list[str]]:
    return rows(found, "sql")
