"""Panels Muru's extensions add to a request's record (issue 677, ADR 0063).

A development run keeps a record of each request it serves (section 11.7),
drawn as panels in the toolbar, on the dashboard's request page and by
``muructl debug request``. The built-in panels are worked out by the edge
from the record; this module lets code in the worker add panels of its
own, as data in the shapes the edge draws: ``spans``, named intervals on
the record's clock that may nest; ``tables``, rows under captioned
columns; and ``values``, names with values. A panel with no template of
its own is drawn by the edge's generic one.

A panel is declared once, with ``panel()``, and filled during a request
with ``add_span()``, ``add_row()`` and ``add_value()``. Entries are kept
only for a request the edge asked the worker to record, which it does in
a development run alone: ``recording()`` says whether the current one is,
and outside one every call does nothing and returns False. Each panel
keeps at most ``PANEL_LIMIT`` entries and a request ``REQUEST_LIMIT``
across its panels; what is past them is counted and shown. A span counts
once for itself and once for each span beneath it.

The worker starts keeping a request's panels when it takes the request
and takes them with the worker's part of its record, by the request's id,
as it does the request's log records. The API is Muru's own for now:
third-party panels are not offered.
"""

from __future__ import annotations

import re
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

from muru import logging as muru_logging
from muru._context import request_id

__all__ = [
    "PANEL_LIMIT",
    "REQUEST_LIMIT",
    "Panel",
    "Span",
    "add_dropped",
    "add_row",
    "add_span",
    "add_value",
    "at",
    "now",
    "on_first_record",
    "panel",
    "recording",
]

PANEL_LIMIT = 500
"""The most entries one panel of a request keeps."""

REQUEST_LIMIT = 2000
"""The most entries a request keeps across its panels."""

SHAPES = ("spans", "tables", "values")

# The built-in panels' names, and the page view's tab, which a declared
# panel may not take.

RESERVED = frozenset({"timing", "where", "response", "logs", "errors", "connection", "page"})

_NAME = re.compile(r"^[a-z][a-z0-9-]{0,31}$")


@dataclass(frozen=True)
class Panel:
    """A declared panel.

    Its name, its title as its tab shows it, its shape, and for a tables
    panel each table's caption and columns.
    """

    name: str
    title: str
    shape: str
    tables: tuple[tuple[str, tuple[str, ...]], ...] = ()


@dataclass
class Span:
    """A named interval of a spans panel, with the spans beneath it.

    Times are milliseconds on the record's clock, as ``now()`` gives them.
    """

    name: str
    start: float
    duration: float
    category: str = ""
    children: list[Span] = field(default_factory=list)

    def size(self) -> int:
        """Count the entries the span takes: itself and every span beneath it."""

        return 1 + sum(child.size() for child in self.children)


_declared: dict[str, Panel] = {}
_declaring = threading.Lock()


def panel(
    name: str,
    title: str,
    shape: str,
    *,
    tables: Mapping[str, Sequence[str]] | None = None,
) -> Panel:
    """Declare a panel, once per process.

    Declaring it again the same way returns the same panel.
    `name` is lowercase letters, digits and hyphens, and none of the
    built-in panels'. `shape` is ``spans``, ``tables`` or ``values``. A
    tables panel names its tables, by caption, with their columns.
    """

    if not _NAME.match(name) or name in RESERVED:
        raise ValueError(f"{name!r} cannot name a panel")

    if shape not in SHAPES:
        raise ValueError(f"a panel's shape is one of {', '.join(SHAPES)}, not {shape!r}")

    if (shape == "tables") != bool(tables):
        raise ValueError("a tables panel, and only one, names its tables and their columns")

    declared = Panel(
        name,
        title,
        shape,
        tuple((caption, tuple(columns)) for caption, columns in (tables or {}).items()),
    )

    with _declaring:
        existing = _declared.setdefault(name, declared)

    if existing != declared:
        raise ValueError(f"the panel {name!r} is already declared otherwise")

    return existing


@dataclass
class _Entries:
    # One panel's entries for one request.

    spans: list[Span] = field(default_factory=list)
    values: dict[str, str] = field(default_factory=dict)
    rows: dict[str, list[list[str]]] = field(default_factory=dict)
    kept: int = 0
    dropped: int = 0


@dataclass
class _Request:
    # What one request's panels keep, from when the worker took it.

    started: float
    lock: threading.Lock = field(default_factory=threading.Lock)
    kept: int = 0
    panels: dict[str, tuple[Panel, _Entries]] = field(default_factory=dict)

    def entries(self, declared: Panel) -> _Entries:
        found = self.panels.get(declared.name)

        if found is None:
            found = self.panels[declared.name] = (declared, _Entries())

        return found[1]

    def admit(self, entries: _Entries, size: int) -> bool:
        # Within the panel's cap and the request's, or counted as dropped.

        if entries.kept + size > PANEL_LIMIT or self.kept + size > REQUEST_LIMIT:
            entries.dropped += size
            return False

        entries.kept += size
        self.kept += size
        return True


_requests: dict[str, _Request] = {}


def _current() -> _Request | None:
    # The record of the request being served, if it is being recorded.

    if not _requests:
        return None

    current = request_id()
    return _requests.get(current) if current is not None else None


def recording() -> bool:
    """Say whether the request being served is recorded, so entries for it are kept."""

    return _current() is not None


def now() -> float:
    """Return the record's clock: milliseconds since the worker took the request.

    Outside a recorded request it is 0.
    """

    kept = _current()

    if kept is None:
        return 0.0

    return (time.perf_counter() - kept.started) * 1000.0


def at(seconds: float) -> float:
    """Return a ``time.perf_counter()`` reading on the record's clock.

    Milliseconds since the worker took the request being served, as
    ``now()`` gives them; 0 outside a recorded request.
    """

    kept = _current()

    if kept is None:
        return 0.0

    return (seconds - kept.started) * 1000.0


def add_dropped(declared: Panel, count: int) -> None:
    """Count entries the panel's own code left out of the request being served.

    For an extension that cuts what it adds to fit, so the panel says
    how many were not kept, with those past its caps.
    """

    kept = _current()

    if kept is None or count <= 0:
        return

    with kept.lock:
        kept.entries(declared).dropped += count


_first: list[Callable[[], None]] = []
_first_started = False


def on_first_record(callback: Callable[[], None]) -> None:
    """Run `callback` once, as the worker starts recording its first request.

    The edge asks a worker for records only in a development run, so this
    is how an extension sets up what only the toolbar needs, such as a
    sink, without costing anything elsewhere. After the first record has
    started, the callback runs at once.
    """

    if _first_started:
        callback()
        return

    _first.append(callback)


def add_span(declared: Panel, span: Span) -> bool:
    """Add a span, with those beneath it, to a spans panel of the request being served.

    Returns whether it was kept.
    """

    _check(declared, "spans")
    kept = _current()

    if kept is None:
        return False

    with kept.lock:
        entries = kept.entries(declared)

        if not kept.admit(entries, span.size()):
            return False

        entries.spans.append(span)

    return True


def add_row(declared: Panel, cells: Sequence[Any], *, table: str | None = None) -> bool:
    """Add a row to a tables panel of the request being served.

    The row goes in `table`, by its caption, else the first table. Returns
    whether it was kept.
    """

    _check(declared, "tables")
    captions = [caption for caption, _ in declared.tables]
    caption = captions[0] if table is None else table

    if caption not in captions:
        raise ValueError(f"the panel {declared.name!r} has no table {caption!r}")

    kept = _current()

    if kept is None:
        return False

    with kept.lock:
        entries = kept.entries(declared)

        if not kept.admit(entries, 1):
            return False

        entries.rows.setdefault(caption, []).append([str(cell) for cell in cells])

    return True


def add_value(declared: Panel, name: str, value: Any) -> bool:
    """Set a value of a values panel of the request being served.

    Setting it again replaces it. Returns whether it was kept.
    """

    _check(declared, "values")
    kept = _current()

    if kept is None:
        return False

    with kept.lock:
        entries = kept.entries(declared)

        if name not in entries.values and not kept.admit(entries, 1):
            return False

        entries.values[name] = str(value)

    return True


def _check(declared: Panel, shape: str) -> None:
    if declared.shape != shape:
        raise ValueError(f"the panel {declared.name!r} holds {declared.shape}, not {shape}")


# The worker's side: called by the runtime as it takes a request it records
# and as it builds the worker's part of the record.


def _start(request: str) -> None:
    """Start keeping a request's panels, from now on its clock."""

    global _first_started

    if not _first_started:
        _first_started = True

        for callback in _first:
            try:
                callback()
            except Exception as error:
                muru_logging._report(error, "a panel's setup for the first record failed")

        _first.clear()

    _requests[request] = _Request(time.perf_counter())


def _take(request: str) -> list[tuple[Any, ...]]:
    """Stop keeping a request's panels, and return them for the runtime to send.

    They come in their order of first use, as plain tuples.
    """

    kept = _requests.pop(request, None)

    if kept is None:
        return []

    with kept.lock:
        return [
            (
                declared.name,
                declared.title,
                [_span(span) for span in entries.spans],
                list(entries.values.items()),
                [
                    (caption, list(columns), entries.rows.get(caption, []))
                    for caption, columns in declared.tables
                ],
                entries.dropped,
            )
            for declared, entries in kept.panels.values()
        ]


def _span(span: Span) -> tuple[Any, ...]:
    return (
        span.name,
        float(span.start),
        float(span.duration),
        span.category,
        [_span(child) for child in span.children],
    )
