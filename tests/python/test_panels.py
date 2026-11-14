"""Unit tests for the panels Muru's extensions add to a request's record (issue 677)."""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from muru import _panels
from muru._context import _request_id
from muru._panels import Span


@pytest.fixture
def recorded(monkeypatch: pytest.MonkeyPatch) -> Iterator[str]:
    # A request the edge asked to record, being served in this context.

    monkeypatch.setattr(_panels, "_declared", {})
    _panels._start("R1")
    token = _request_id.set("R1")

    try:
        yield "R1"
    finally:
        _request_id.reset(token)
        _panels._requests.pop("R1", None)


def test_outside_a_recorded_request_nothing_is_kept() -> None:
    values = _panels.panel("library", "Library", "values")

    assert not _panels.recording()
    assert _panels.now() == 0.0
    assert not _panels.add_value(values, "books", 7)


def test_a_requests_panels_are_taken_in_the_shapes_the_edge_draws(recorded: str) -> None:
    values = _panels.panel("library", "Library", "values")
    queries = _panels.panel(
        "queries", "Queries", "tables", tables={"By statement": ["statement", "times"]}
    )
    trace = _panels.panel("trace", "Trace", "spans")

    assert _panels.recording()
    assert _panels.now() >= 0.0

    assert _panels.add_value(values, "books", 7)
    assert _panels.add_value(values, "books", 8)
    assert _panels.add_row(queries, ["SELECT 1", 3])
    assert _panels.add_span(trace, Span("view", 1.0, 2.0, "view", [Span("query", 1.5, 0.5)]))

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
    assert not _panels.recording()


def test_entries_past_a_panels_cap_or_the_requests_are_counted(
    recorded: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_panels, "PANEL_LIMIT", 3)
    monkeypatch.setattr(_panels, "REQUEST_LIMIT", 4)
    trace = _panels.panel("trace", "Trace", "spans")
    values = _panels.panel("library", "Library", "values")

    # A span counts for itself and those beneath it.

    assert _panels.add_span(trace, Span("view", 0, 1, children=[Span("query", 0, 1)]))
    assert not _panels.add_span(trace, Span("view", 0, 1, children=[Span("query", 0, 1)]))
    assert _panels.add_span(trace, Span("view", 0, 1))
    assert _panels.add_value(values, "books", 1)
    assert not _panels.add_value(values, "authors", 1)

    taken = {panel[0]: panel for panel in _panels._take(recorded)}

    assert len(taken["trace"][2]) == 2 and taken["trace"][5] == 2
    assert taken["library"][3] == [("books", "1")] and taken["library"][5] == 1


def test_a_panel_is_declared_once_and_not_as_a_built_in(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(_panels, "_declared", {})

    first = _panels.panel("library", "Library", "values")
    assert _panels.panel("library", "Library", "values") is first

    with pytest.raises(ValueError, match="already declared otherwise"):
        _panels.panel("library", "Books", "values")

    for name in ("timing", "logs", "page", "Library", "a b"):
        with pytest.raises(ValueError, match="cannot name a panel"):
            _panels.panel(name, "X", "values")

    with pytest.raises(ValueError, match="shape is one of"):
        _panels.panel("odd", "Odd", "chart")

    with pytest.raises(ValueError, match="names its tables"):
        _panels.panel("odd", "Odd", "tables")

    with pytest.raises(ValueError, match="holds values, not spans"):
        _panels.add_span(first, Span("x", 0, 0))


def test_concurrent_requests_keep_their_own_entries(monkeypatch: pytest.MonkeyPatch) -> None:
    # Each request finds its own record through the request id of the
    # thread serving it, so concurrent requests, on a GIL build or a
    # free-threaded one, never mix their entries.

    import contextvars
    import threading

    monkeypatch.setattr(_panels, "_declared", {})
    values = _panels.panel("library", "Library", "values")
    rows = _panels.panel("queries", "Queries", "tables", tables={"Queries": ["request", "n"]})
    requests, rounds = 16, 100
    taken: dict[int, list[tuple[object, ...]]] = {}
    together = threading.Barrier(requests)

    def serve(index: int) -> None:
        _panels._start(f"C{index}")
        _request_id.set(f"C{index}")
        together.wait()

        for n in range(rounds):
            _panels.add_value(values, "page", f"/page/{index}")
            _panels.add_row(rows, [index, n])

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
