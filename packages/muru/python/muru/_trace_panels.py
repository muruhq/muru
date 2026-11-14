"""wrapture's events as toolbar panels: Trace, SQL and Exceptions (issue 640).

When tracing is on (section 11.6), a sink of Muru's own hears every event
wrapture records in a worker and turns each request's tree into panels of
the request's record (issue 677), drawn by the toolbar after the built-in
panels:

- **Trace:** the request's tree as spans on the record's clock, coloured
  by wrapture's category. A request reads as its method and path, and a
  query as its statement where the instrumentation records one.

- **SQL:** the queries, from events whose category is ``database`` other
  than a connection's open, commit or rollback, in two tables: grouped by
  normalised statement, the most run first, so an N+1 pattern shows as
  one statement run many times; and each query in order.

- **Exceptions:** the exceptions that escaped an operation or were noted
  against one with ``note_exception``, where and what they were.

An event belongs to the request whose ``muru.request_id`` is current when
its tree's root begins, whose panels the root takes then; the events
beneath follow their parent links, so work on a thread that copied the
request's context is attributed too, and work on one that did not is not
recorded at all. A tree is added to its request's panels once its root
closes, wherever it closes. At most ``TREE_LIMIT``
operations of a tree are kept, the rest counted on the panels as not kept,
and long names are cut.

The sink is added only once the worker records its first request, which
the edge asks for in a development run alone, so outside one wrapture
records nothing for it and its cost is nothing.
"""

from __future__ import annotations

import re
import threading
from dataclasses import dataclass, field
from typing import Any

from muru import _panels

__all__ = ["install"]

TREE_LIMIT = 400
"""The most operations of one request's tree the panels keep."""

NAME_LIMIT = 200
"""The longest name a span or a statement keeps, in characters."""

# The kinds of event drawn: calls, requests and blocks. Log events are the
# Logs panel's, and attribute access is not drawn.

KINDS = frozenset({"call", "request", "block"})

TRACE = _panels.SpansPanel("trace", "Trace")
SQL = _panels.TablesPanel(
    "sql",
    "SQL",
    tables={
        "By statement": ["statement", "times", "total ms"],
        "Each query": ["at ms", "ms", "statement"],
    },
)
EXCEPTIONS = _panels.TablesPanel(
    "exceptions", "Exceptions", tables={"Exceptions": ["in", "exception"]}
)


@dataclass
class _Panels:
    # The request's panels a tree is added to, taken as its root begins.

    trace: _panels.Spans
    sql: _panels.Tables
    exceptions: _panels.Tables


@dataclass
class _Node:
    # One open or closed operation of a request's tree. The root holds
    # the request's panels.

    root: _Node | None
    event: Any
    panels: _Panels | None = None
    children: list[_Node] = field(default_factory=list)
    closed: bool = False
    size: int = 1
    dropped: int = 0


class _Trees:
    # The trees being built, by the seq of each open event.

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.open: dict[int, _Node] = {}

    def entered(self, event: Any) -> None:
        if event.kind not in KINDS:
            return

        with self.lock:
            parent = self.open.get(event.parent_id) if event.parent_id is not None else None

            if parent is not None:
                root = parent.root or parent

                if root.size >= TREE_LIMIT:
                    root.dropped += 1
                    return

                root.size += 1
                node = _Node(root, event)
                parent.children.append(node)
                self.open[event.seq] = node
                return

        # A root, which begins a tree only in a request being recorded.

        trace = TRACE.current()

        if not trace.active:
            return

        panels = _Panels(trace, SQL.current(), EXCEPTIONS.current())

        with self.lock:
            self.open[event.seq] = _Node(None, event, panels)

    def closed(self, event: Any) -> None:
        with self.lock:
            node = self.open.pop(event.seq, None)

            if node is None:
                return

            node.closed = True

            if node.root is not None:
                return

        # The root has closed: the tree is done.

        if node.panels is not None:
            _add(node, node.panels)


def _add(root: _Node, panels: _Panels) -> None:
    # The finished tree, as the three panels of its request.

    # Times go onto the record's clock through the trace's handle.

    trace = panels.trace
    trace.add(_span(root, trace))
    trace.add_dropped(root.dropped)

    queries: list[tuple[float, float, str]] = []
    failures: list[tuple[str, str]] = []
    _collect(root, trace, queries, failures)

    grouped: dict[str, list[float]] = {}

    for _, took, statement in queries:
        grouped.setdefault(_normalised(statement), []).append(took)

    by_statement = panels.sql.table("By statement")

    for statement, times in sorted(grouped.items(), key=lambda item: -len(item[1])):
        by_statement.add_row([statement, len(times), f"{sum(times):.3f}"])

    each_query = panels.sql.table("Each query")

    for started, took, statement in queries:
        each_query.add_row([f"{started:.3f}", f"{took:.3f}", statement])

    exceptions = panels.exceptions.table("Exceptions")

    for where, what in failures:
        exceptions.add_row([where, what])


def _span(node: _Node, clock: _panels.Spans) -> _panels.Span:
    event = node.event
    started = clock.at(event.started) if event.started is not None else 0.0
    duration = (event.duration or 0.0) * 1000.0

    return _panels.Span(
        _name(event),
        started,
        duration,
        event.category or event.kind,
        [_span(child, clock) for child in node.children],
    )


def _collect(
    node: _Node,
    clock: _panels.Spans,
    queries: list[tuple[float, float, str]],
    failures: list[tuple[str, str]],
) -> None:
    # The queries and the exceptions of a tree, in the order they began.

    event = node.event

    if _query(event) and event.started is not None:
        statement = event.data.get("statement") or event.label or event.path
        queries.append(
            (clock.at(event.started), (event.duration or 0.0) * 1000.0, _cut(str(statement)))
        )

    if event.exception is not None:
        failures.append((_name(event), _described(event.exception)))

    for caught in event.caught:
        failures.append((_name(event), f"{_described(caught.exception)} (handled)"))

    for child in node.children:
        _collect(child, clock, queries, failures)


# What a database event's operation is, by wrapture's contract, when it is
# not a query: the SQL's leading keyword otherwise.

_NOT_QUERIES = frozenset({"CONNECT", "COMMIT", "ROLLBACK"})


def _query(event: Any) -> bool:
    # A database event that ran a statement, not one that opened, committed
    # or rolled back a connection.

    if event.category != "database":
        return False

    operation = str(event.data.get("operation") or "").upper()
    return operation not in _NOT_QUERIES


def _name(event: Any) -> str:
    # What a span reads as: a request as its method and path, a query as
    # its statement where it was recorded, anything else as its label.

    data = event.data

    if event.kind == "request" and data.get("method"):
        return _cut(f"{data['method']} {data.get('path', '')}")

    if event.category == "database" and data.get("statement"):
        return _cut(str(data["statement"]))

    return _cut(event.label or event.path)


def _described(exception: BaseException) -> str:
    return _cut(f"{type(exception).__name__}: {exception}")


def _cut(text: str) -> str:
    return text if len(text) <= NAME_LIMIT else f"{text[: NAME_LIMIT - 1]}…"


# Literals in a statement, which normalising replaces with a placeholder:
# quoted strings, numbers, and the lists an IN takes.

_STRING = re.compile(r"'(?:[^']|'')*'")
_NUMBER = re.compile(r"(?<![\w.])-?\d+(?:\.\d+)?\b")
_LIST = re.compile(r"\bIN\s*\(\s*\?(?:\s*,\s*\?)+\s*\)", re.IGNORECASE)
_SPACE = re.compile(r"\s+")


def _normalised(statement: str) -> str:
    """Return a statement with its literals as placeholders and its spacing collapsed.

    So queries that differ only in the values they were written with group
    together, as those passing parameters already do.
    """

    text = _STRING.sub("?", statement)
    text = _NUMBER.sub("?", text)
    text = _LIST.sub("IN (?...)", text)
    return _SPACE.sub(" ", text).strip()


def install(wrapture: Any) -> None:
    """Add the sink once the worker records its first request."""

    trees = _Trees()

    class ToolbarSink(wrapture.Sink):  # type: ignore[misc]
        """wrapture's events into the request's panels.

        A request's method and path are captured as its arguments are,
        so the sink asks for bounded summaries of arguments, as wrapture's
        own streaming sinks do, and nothing of results.
        """

        capture_args = "summary"
        capture_result = "none"

        def on_enter(self, event: Any) -> None:
            trees.entered(event)

        def on_exit(self, event: Any) -> None:
            trees.closed(event)

        def on_error(self, event: Any) -> None:
            trees.closed(event)

    _panels.on_first_record(lambda: wrapture.add_sink(ToolbarSink()))
