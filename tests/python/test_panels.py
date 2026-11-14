"""Unit tests for the panels Muru's extensions add to a request's record (issues 677, 679)."""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any

import pytest

from muru import _panels
from muru._context import _request_id
from muru._panels import Span, SpansPanel, TablesPanel, ValuesPanel


@pytest.fixture(autouse=True)
def declared(monkeypatch: pytest.MonkeyPatch) -> None:
    # Each test declares its panels afresh.

    monkeypatch.setattr(_panels, "_declared", {})


@pytest.fixture
def recorded() -> Iterator[str]:
    # A request the edge asked to record, being served in this context.

    _panels._start("R1")
    token = _request_id.set("R1")

    try:
        yield "R1"
    finally:
        _request_id.reset(token)
        _panels._requests.pop("R1", None)


def test_outside_a_recorded_request_a_panel_keeps_nothing_and_says_so() -> None:
    values = ValuesPanel("library", "Library").current()
    spans = SpansPanel("trace", "Trace").current()
    tables = TablesPanel("queries", "Queries", tables={"Queries": ["statement"]}).current()

    assert not values.active and not spans.active and not tables.active
    assert spans.now() == 0.0 and spans.at(123.0) == 0.0

    values["books"] = 7
    assert not spans.add(Span("view", 0, 1))
    assert not tables.table("Queries").add_row(["SELECT 1"])
    spans.add_dropped(3)

    with spans.span("view"):
        ran = True

    assert ran

    # A wrong caption fails whether or not the request is recorded.

    with pytest.raises(ValueError, match="has no table 'Others'"):
        tables.table("Others")


def test_a_requests_panels_are_taken_in_the_shapes_the_edge_draws(recorded: str) -> None:
    values = ValuesPanel("library", "Library").current()
    queries = TablesPanel(
        "queries", "Queries", tables={"By statement": ["statement", "times"]}
    ).current()
    trace = SpansPanel("trace", "Trace").current()

    assert values.active and trace.now() >= 0.0

    values["books"] = 7
    values["books"] = 8
    assert queries.table("By statement").add_row(["SELECT 1", 3])
    assert trace.add(Span("view", 1.0, 2.0, "view", [Span("query", 1.5, 0.5)]))

    taken = _panels._take(recorded)

    assert taken == [
        ("library", "Library", [], [("books", "8")], [], 0),
        (
            "queries",
            "Queries",
            [],
            [],
            [("By statement", ["statement", "times"], [["SELECT 1", "3"]])],
            0,
        ),
        (
            "trace",
            "Trace",
            [("view", 1.0, 2.0, "view", [("query", 1.5, 0.5, "", [])])],
            [],
            [],
            0,
        ),
    ]


def test_a_handle_kept_past_its_request_keeps_nothing(recorded: str) -> None:
    values = ValuesPanel("library", "Library").current()
    values["books"] = 1
    _panels._take(recorded)

    assert not values.active

    values["authors"] = 2
    values.add_dropped(4)

    assert _panels._take(recorded) == []


def test_timed_blocks_nest_as_spans_and_are_added_as_they_end(recorded: str) -> None:
    trace = SpansPanel("trace", "Trace").current()
    other = SpansPanel("other", "Other").current()

    with trace.span("view", category="view"):
        with trace.span("query", category="database"):
            pass

        # A block of another panel does not nest beneath this one's.

        with other.span("aside"):
            pass

    with pytest.raises(RuntimeError), trace.span("broken"):
        raise RuntimeError("the block raised")

    taken = {panel[0]: panel for panel in _panels._take(recorded)}

    view, broken = taken["trace"][2]
    assert view[0] == "view" and view[3] == "view" and view[2] >= view[4][0][2]
    assert [child[0] for child in view[4]] == ["query"]
    assert broken[0] == "broken"
    assert [span[0] for span in taken["other"][2]] == ["aside"]


def test_entries_past_a_panels_cap_or_the_requests_are_counted(
    recorded: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_panels, "PANEL_LIMIT", 3)
    monkeypatch.setattr(_panels, "REQUEST_LIMIT", 4)
    trace = SpansPanel("trace", "Trace").current()
    values = ValuesPanel("library", "Library").current()

    # A span counts for itself and those beneath it.

    assert trace.add(Span("view", 0, 1, children=[Span("query", 0, 1)]))
    assert not trace.add(Span("view", 0, 1, children=[Span("query", 0, 1)]))
    assert trace.add(Span("view", 0, 1))
    values["books"] = 1
    values["authors"] = 1

    # The panel's own code may count entries it left out.

    trace.add_dropped(5)

    taken = {panel[0]: panel for panel in _panels._take(recorded)}

    assert len(taken["trace"][2]) == 2 and taken["trace"][5] == 7
    assert taken["library"][3] == [("books", "1")] and taken["library"][5] == 1


def test_a_panel_is_declared_once_and_not_as_a_built_in() -> None:
    first = ValuesPanel("library", "Library")
    assert ValuesPanel("library", "Library") == first

    with pytest.raises(ValueError, match="already declared otherwise"):
        ValuesPanel("library", "Books")

    with pytest.raises(ValueError, match="already declared otherwise"):
        SpansPanel("library", "Library")

    for name in ("timing", "logs", "page", "Library", "a b"):
        with pytest.raises(ValueError, match="cannot name a panel"):
            ValuesPanel(name, "X")

    with pytest.raises(ValueError, match="names its tables"):
        TablesPanel("odd", "Odd", tables={})


def test_concurrent_requests_keep_their_own_entries() -> None:
    # Each request finds its own record through the request id of the
    # thread serving it, so concurrent requests, on a GIL build or a
    # free-threaded one, never mix their entries.

    import contextvars
    import threading

    values = ValuesPanel("library", "Library")
    rows = TablesPanel("queries", "Queries", tables={"Queries": ["request", "n"]})
    requests, rounds = 16, 100
    taken: dict[int, list[tuple[Any, ...]]] = {}
    together = threading.Barrier(requests)

    def serve(index: int) -> None:
        _panels._start(f"C{index}")
        _request_id.set(f"C{index}")
        together.wait()

        for n in range(rounds):
            values.current()["page"] = f"/page/{index}"
            rows.current().table("Queries").add_row([index, n])

        taken[index] = _panels._take(f"C{index}")

    threads = [
        threading.Thread(target=contextvars.copy_context().run, args=(serve, index))
        for index in range(requests)
    ]

    for thread in threads:
        thread.start()

    for thread in threads:
        thread.join()

    for index in range(requests):
        found = {panel[0]: panel for panel in taken[index]}
        assert found["library"][3] == [("page", f"/page/{index}")]
        kept = found["queries"][4][0][2]
        assert len(kept) == rounds and all(row[0] == str(index) for row in kept)

    assert not [request for request in _panels._requests if request.startswith("C")]
