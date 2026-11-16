"""Unit tests for wrapture's events as toolbar panels (issue 640).

The events are stand-ins with the fields the sink reads, so the tests need
no wrapture: a real one runs through the integration test.
"""

from __future__ import annotations

import itertools
import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from typing import Any

import pytest

from muru import _trace_panels, panels
from muru._context import _request_id

_seq = itertools.count(1)


@dataclass
class Event:
    kind: str
    path: str
    parent_id: int | None = None
    label: str | None = None
    category: str | None = None
    data: dict[str, Any] = field(default_factory=dict)
    seq: int = field(default_factory=lambda: next(_seq))
    started: float | None = None
    duration: float | None = None
    exception: BaseException | None = None
    caught: tuple[Any, ...] = ()


@dataclass
class Caught:
    exception: BaseException
    at: float


@pytest.fixture
def recorded() -> Iterator[str]:
    panels._start("T1")
    token = _request_id.set("T1")

    try:
        yield "T1"
    finally:
        _request_id.reset(token)
        panels._requests.pop("T1", None)


def run(trees: Any, event: Event, *children: tuple[Event, tuple[Any, ...]]) -> Event:
    # Enter an event, run its children the same way, and close it.

    trees.entered(event)
    event.started = time.perf_counter()

    for child, grandchildren in children:
        child.parent_id = event.seq
        run(trees, child, *grandchildren)

    event.duration = 0.001
    trees.closed(event)
    return event


def taken(request: str) -> dict[str, tuple[Any, ...]]:
    return {panel[0]: panel for panel in panels._take(request)}


def test_a_requests_tree_becomes_its_trace_sql_and_exceptions(recorded: str) -> None:
    trees = _trace_panels._Trees()
    query = "SELECT name FROM author WHERE id = ?"
    failure = ValueError("no such author")

    queries = [
        Event("call", "execute", category="database", data={"statement": query}) for _ in range(3)
    ]
    connect = Event("call", "connect", category="database", data={"operation": "CONNECT"})
    books = Event("call", "books", label="books", caught=(Caught(failure, 0.0),))
    lookup = Event("call", "lookup", label="lookup", exception=KeyError("shelf"))
    request = Event("request", "app", data={"method": "GET", "path": "/books"}, category="request")

    run(
        trees,
        request,
        (books, tuple((child, ()) for child in [*queries, connect])),
        (lookup, ()),
    )

    panels = taken(recorded)
    traced = panels["trace"][2][0]

    assert traced[0] == "GET /books" and traced[3] == "request"
    assert [child[0] for child in traced[4][0][4]] == [query, query, query, "connect"]

    # The span a note was made against is failed, as is one an exception
    # escaped; the rest are not.

    assert [(child[0], child[5]) for child in traced[4]] == [("books", True), ("lookup", True)]
    assert not traced[5] and not any(query[5] for query in traced[4][0][4])

    by_statement, each = panels["sql"][4]
    assert by_statement[2] == [[query, "3", by_statement[2][0][2]]]
    assert len(each[2]) == 3

    assert panels["exceptions"][4][0][2] == [
        ["books", "ValueError: no such author (handled)"],
        ["lookup", "KeyError: 'shelf'"],
    ]


def test_events_of_a_request_not_recorded_are_left_alone() -> None:
    trees = _trace_panels._Trees()
    run(trees, Event("call", "outside"))

    assert trees.open == {}
    assert panels._take("T1") == []


def test_a_tree_closing_outside_its_requests_context_is_still_its_requests(
    recorded: str,
) -> None:
    trees = _trace_panels._Trees()
    view = Event("call", "view")
    trees.entered(view)
    view.started = time.perf_counter()
    view.duration = 0.001

    token = _request_id.set(None)

    try:
        trees.closed(view)
    finally:
        _request_id.reset(token)

    assert taken(recorded)["trace"][2][0][0] == "view"


def test_a_tree_past_its_limit_is_cut_and_says_so(
    recorded: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_trace_panels, "TREE_LIMIT", 3)
    trees = _trace_panels._Trees()

    run(trees, Event("call", "view"), *((Event("call", f"step{n}"), ()) for n in range(5)))

    trace = taken(recorded)["trace"]
    assert len(trace[2][0][4]) == 2 and trace[5] == 3


def test_statements_group_by_their_shape() -> None:
    normalised = _trace_panels._normalised

    assert normalised("SELECT * FROM t WHERE id = 42") == "SELECT * FROM t WHERE id = ?"
    assert normalised("SELECT * FROM t WHERE name = 'o''brien'") == normalised(
        "SELECT * FROM t  WHERE name = 'x'"
    )
    assert (
        normalised("SELECT * FROM t WHERE id IN (?, ?, ?)") == "SELECT * FROM t WHERE id IN (?...)"
    )
    assert normalised("INSERT INTO t VALUES (?, ?)") == "INSERT INTO t VALUES (?, ?)"
    assert normalised("SELECT col2 FROM t2") == "SELECT col2 FROM t2"


def test_a_requests_calls_to_other_services_are_its_outbound_panel(recorded: str) -> None:
    trees = _trace_panels._Trees()

    def call(**data: Any) -> Event:
        return Event("call", "open", category="external", data=data)

    author = {
        "method": "GET",
        "url": "http://authors.test:80/authors",
        "host": "authors.test",
        "port": 80,
        "path": "/authors",
        "status": 200,
    }
    calls = [call(**author) for _ in range(2)]

    # Calls for each row group by the path with its ID as a placeholder.

    for row in (7, 8):
        calls.append(
            call(**{**author, "path": f"/authors/{row}", "url": f"http://a/authors/{row}"})
        )

    # A client built on another records twice; only the outer call counts.

    nested = call(**author)
    inner = call(method="GET", url="http://authors.test/authors", host="authors.test")
    failed = call(method="POST", url="https://pay.test:8443/charge", host="pay.test", port=8443)
    failed.exception = ConnectionRefusedError("refused")
    rpc = call(system="grpc", service="shop.Stock", operation="Count", host="stock", port=50051)
    rpc.data["code"] = "OK"
    request = Event("request", "app", data={"method": "GET", "path": "/books"}, category="request")

    run(
        trees,
        request,
        *((child, ()) for child in calls),
        (nested, ((inner, ()),)),
        (failed, ()),
        (rpc, ()),
    )

    by_endpoint, each = taken(recorded)["outbound"][4]
    assert [row[:4] for row in by_endpoint[2]] == [
        ["GET", "authors.test", "/authors", "3"],
        ["GET", "authors.test", "/authors/{id}", "2"],
        ["POST", "pay.test:8443", "/charge", "1"],
        ["gRPC", "stock:50051", "shop.Stock/Count", "1"],
    ]
    assert len(each[2]) == 7
    assert each[2][2][3] == "http://a/authors/7"
    assert each[2][0][2:] == ["GET", "http://authors.test:80/authors", "200"]
    assert each[2][5][2:] == ["POST", "https://pay.test:8443/charge", "ConnectionRefusedError"]
    assert each[2][6][2:] == ["gRPC", "shop.Stock/Count", "OK"]


def test_a_request_with_no_calls_has_no_outbound_panel(recorded: str) -> None:
    trees = _trace_panels._Trees()
    run(trees, Event("call", "view"))

    assert "outbound" not in taken(recorded)


def test_a_paths_id_like_segments_are_one_endpoint() -> None:
    templated = _trace_panels._templated

    assert templated("/stock/42") == "/stock/{id}"
    assert templated("/users/3f2b8c1e-0d4a-4e6b-9a1c-2b3d4e5f6a7b/orders") == "/users/{id}/orders"
    assert templated("/blobs/9f86d081884c7d659a2feaa0c55ad015") == "/blobs/{id}"
    assert templated("/v2/stock") == "/v2/stock"
    assert templated("/stock/deadbeef") == "/stock/deadbeef"
