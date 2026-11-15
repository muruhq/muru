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
