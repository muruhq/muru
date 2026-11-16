"""Panels code in the worker adds to the development toolbar (issues 677, 679 to 681, ADR 0063).

A development run keeps a record of each request it serves (section 11.7),
drawn as panels in the toolbar, on the dashboard's request page and by
``muructl debug request``. The built-in panels are worked out by the edge
from the record; this module lets code in the worker add panels of its
own, the application's or a library's, as data in the shapes the edge
draws, each declared once by a class of its own:

- ``SpansPanel``: named intervals on the record's clock that may nest.
- ``TablesPanel``: rows under captioned columns, in one or more tables.
- ``ValuesPanel``: names with values.
- ``TextPanel``: text, plain or markdown, which the edge draws.

A panel with no template of its own is drawn by the edge's generic one.

During a request, ``current()`` on a declared panel gives a handle for the
request being served, and entries are added through its methods. Entries
are kept only for a request the edge asked the worker to record, which it
does in a development run alone; for any other request the handle keeps
nothing, so code adds to a panel the same way wherever it runs. A handle's
``active`` says whether it keeps anything, for code that would do real
work only to feed a panel.

Each panel keeps at most ``PANEL_LIMIT`` entries and a request
``REQUEST_LIMIT`` across its panels; what is past them is counted and
shown. A span counts once for itself and once for each span beneath it,
and a block of text once, a text panel keeping at most ``TEXT_LIMIT``
characters.

The worker starts keeping a request's panels when it takes the request
and takes them with the worker's part of its record, by the request's id,
as it does the request's log records. A handle kept past that keeps
nothing. A panel is data only: it supplies no template or script of its
own.
"""

from __future__ import annotations

import contextlib
import re
import threading
import time
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextvars import ContextVar
from dataclasses import dataclass, field
from typing import Any, ClassVar

from muru import logging as muru_logging
from muru._context import request_id

__all__ = [
    "PANEL_LIMIT",
    "REQUEST_LIMIT",
    "TEXT_LIMIT",
    "Panel",
    "Span",
    "Spans",
    "SpansPanel",
    "Table",
    "Tables",
    "TablesPanel",
    "Text",
    "TextPanel",
    "Values",
    "ValuesPanel",
    "on_first_record",
]

PANEL_LIMIT = 500
"""The most entries one panel of a request keeps."""

REQUEST_LIMIT = 2000
"""The most entries a request keeps across its panels."""

TEXT_LIMIT = 65536
"""The most characters one text panel of a request keeps."""

# The built-in panels' names, and the toolbar's page view and history
# tabs, which a declared panel may not take, and the names of Muru's own
# panels from tracing, which only Muru declares, within _own().

RESERVED = frozenset(
    {"timing", "where", "response", "logs", "errors", "connection", "hints", "page", "history"}
)
OWN = frozenset({"trace", "sql", "outbound", "exceptions"})

_owning = False


@contextlib.contextmanager
def _own() -> Iterator[None]:
    # Muru's own panels are declared within this.

    global _owning

    _owning = True

    try:
        yield
    finally:
        _owning = False


_NAME = re.compile(r"^[a-z][a-z0-9-]{0,31}$")


@dataclass
class Span:
    """A named interval of a spans panel, with the spans beneath it.

    Times are milliseconds on the record's clock, as a handle's ``now()``
    gives them. ``failed`` marks an operation that failed, which the
    toolbar draws so it stands out from its category's colour.
    """

    name: str
    start: float
    duration: float
    category: str = ""
    children: list[Span] = field(default_factory=list)
    failed: bool = False

    def size(self) -> int:
        """Count the entries the span takes: itself and every span beneath it."""

        return 1 + sum(child.size() for child in self.children)


# What one request's panels keep, from when the worker took it.


@dataclass
class _Entries:
    # One panel's entries for one request.

    spans: list[Span] = field(default_factory=list)
    values: dict[str, str] = field(default_factory=dict)
    rows: dict[str, list[list[str]]] = field(default_factory=dict)
    text: list[str] = field(default_factory=list)
    characters: int = 0
    kept: int = 0
    dropped: int = 0


@dataclass
class _Request:
    # A request's panels, by name, with their declarations. Once taken, it
    # keeps nothing more.

    started: float
    lock: threading.Lock = field(default_factory=threading.Lock)
    kept: int = 0
    taken: bool = False
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


# The declarations.

_declared: dict[str, Panel] = {}
_declaring = threading.Lock()


class Panel:
    """A declared panel: its name, and its title as its tab shows it.

    Declared by a class for its shape, once per process; declaring it
    again the same way gives an equal panel whose handles add to the
    same entries.
    """

    shape: ClassVar[str] = ""

    __slots__ = ("_idle", "markdown", "name", "tables", "title")

    def __init__(
        self,
        name: str,
        title: str,
        tables: tuple[tuple[str, tuple[str, ...]], ...] = (),
        markdown: bool = False,
    ) -> None:
        if not _NAME.match(name) or name in RESERVED or (name in OWN and not _owning):
            raise ValueError(f"{name!r} cannot name a panel")

        self.name = name
        self.title = title
        self.tables = tables
        self.markdown = markdown

        with _declaring:
            existing = _declared.setdefault(name, self)

        if existing != self:
            raise ValueError(f"the panel {name!r} is already declared otherwise")

    def __eq__(self, other: object) -> bool:
        if type(other) is not type(self) or not isinstance(other, Panel):
            return NotImplemented

        return self._key() == other._key()

    def __hash__(self) -> int:
        return hash((type(self), *self._key()))

    def _key(self) -> tuple[Any, ...]:
        return (self.name, self.title, self.tables, self.markdown)

    def __repr__(self) -> str:
        return f"{type(self).__name__}({self.name!r}, {self.title!r})"


class SpansPanel(Panel):
    """A panel of spans: named intervals on the record's clock that may nest."""

    shape = "spans"

    def __init__(self, name: str, title: str) -> None:
        super().__init__(name, title)
        self._idle = Spans(self, None)

    def current(self) -> Spans:
        """Return the panel for the request being served."""

        kept = _current()
        return self._idle if kept is None else Spans(self, kept)


class TablesPanel(Panel):
    """A panel of tables, each named by its caption, with its columns."""

    shape = "tables"

    def __init__(self, name: str, title: str, *, tables: Mapping[str, Sequence[str]]) -> None:
        if not tables:
            raise ValueError("a tables panel names its tables and their columns")

        super().__init__(
            name,
            title,
            tuple((caption, tuple(columns)) for caption, columns in tables.items()),
        )
        self._idle = Tables(self, None)

    def current(self) -> Tables:
        """Return the panel for the request being served."""

        kept = _current()
        return self._idle if kept is None else Tables(self, kept)


class ValuesPanel(Panel):
    """A panel of names with values."""

    shape = "values"

    def __init__(self, name: str, title: str) -> None:
        super().__init__(name, title)
        self._idle = Values(self, None)

    def current(self) -> Values:
        """Return the panel for the request being served."""

        kept = _current()
        return self._idle if kept is None else Values(self, kept)


class TextPanel(Panel):
    """A panel of text, plain or markdown, which the edge draws.

    Plain text keeps its line breaks. Markdown is drawn as HTML, sanitised
    as every panel is, and its links may go to outside addresses, which
    open in a new tab.
    """

    shape = "text"

    def __init__(self, name: str, title: str, *, markdown: bool = False) -> None:
        super().__init__(name, title, markdown=markdown)
        self._idle = Text(self, None)

    def current(self) -> Text:
        """Return the panel for the request being served."""

        kept = _current()
        return self._idle if kept is None else Text(self, kept)


# The handles.


class _Handle:
    # A declared panel for one request, or for none, when it keeps nothing.

    __slots__ = ("_kept", "_panel")

    def __init__(self, panel: Panel, kept: _Request | None) -> None:
        self._panel = panel
        self._kept = kept

    @property
    def active(self) -> bool:
        """Say whether entries added now are kept, the request being recorded."""

        return self._kept is not None and not self._kept.taken

    def now(self) -> float:
        """Return the record's clock: milliseconds since the worker took the request.

        0 when the handle keeps nothing.
        """

        return self.at(time.perf_counter())

    def at(self, seconds: float) -> float:
        """Return a ``time.perf_counter()`` reading on the record's clock.

        Milliseconds since the worker took the request, as ``now()`` gives
        them; 0 when the handle keeps nothing.
        """

        if self._kept is None:
            return 0.0

        return (seconds - self._kept.started) * 1000.0

    def add_dropped(self, count: int) -> None:
        """Count entries the panel's own code left out, to be shown with those past its caps."""

        kept = self._kept

        if kept is None or count <= 0:
            return

        with kept.lock:
            if not kept.taken:
                kept.entries(self._panel).dropped += count

    def _add(self, size: int, add: Callable[[_Entries], None]) -> bool:
        # An entry of `size`, kept by `add` if the caps allow it.

        kept = self._kept

        if kept is None:
            return False

        with kept.lock:
            if kept.taken:
                return False

            entries = kept.entries(self._panel)

            if not kept.admit(entries, size):
                return False

            add(entries)

        return True


# The span a block timed by Spans.span() is in, with its request and panel,
# so a block within it of the same panel nests beneath it.

_open_span: ContextVar[tuple[_Request, str, Span] | None] = ContextVar(
    "muru_open_span", default=None
)

_NOTHING = contextlib.nullcontext()


class Spans(_Handle):
    """A spans panel for the request being served."""

    __slots__ = ()

    def add(self, span: Span) -> bool:
        """Add a span, with those beneath it. Returns whether it was kept."""

        return self._add(span.size(), lambda entries: entries.spans.append(span))

    def span(self, name: str, category: str = "") -> contextlib.AbstractContextManager[None]:
        """Time a block as a span, beneath the one an enclosing block is timing.

        The span is added as its block ends, however it ends, and with
        those timed within it once the outermost ends. A block that an
        exception escapes is marked failed, and the exception goes on.
        """

        if self._kept is None:
            return _NOTHING

        return self._timed(self._kept, name, category)

    @contextlib.contextmanager
    def _timed(self, kept: _Request, name: str, category: str) -> Iterator[None]:
        opened = Span(name, self.now(), 0.0, category)
        enclosing = _open_span.get()

        if enclosing is not None and not (
            enclosing[0] is kept and enclosing[1] == self._panel.name
        ):
            enclosing = None

        token = _open_span.set((kept, self._panel.name, opened))

        try:
            yield
        except BaseException:
            opened.failed = True
            raise
        finally:
            _open_span.reset(token)
            opened.duration = self.now() - opened.start

            if enclosing is not None:
                enclosing[2].children.append(opened)
            else:
                self.add(opened)


class Tables(_Handle):
    """A tables panel for the request being served."""

    __slots__ = ()

    def table(self, caption: str) -> Table:
        """Return one of the panel's tables, by its caption."""

        if not any(declared == caption for declared, _ in self._panel.tables):
            raise ValueError(f"the panel {self._panel.name!r} has no table {caption!r}")

        return Table(self, caption)


class Table:
    """One table of a tables panel, for the request being served."""

    __slots__ = ("_caption", "_tables")

    def __init__(self, tables: Tables, caption: str) -> None:
        self._tables = tables
        self._caption = caption

    def add_row(self, cells: Sequence[Any]) -> bool:
        """Add a row, each cell as text. Returns whether it was kept."""

        if self._tables._kept is None:
            return False

        row = [str(cell) for cell in cells]
        caption = self._caption
        return self._tables._add(
            1, lambda entries: entries.rows.setdefault(caption, []).append(row)
        )


class Values(_Handle):
    """A values panel for the request being served; a value is set by name, as in a dict."""

    __slots__ = ()

    def __setitem__(self, name: str, value: Any) -> None:
        kept = self._kept

        if kept is None:
            return

        with kept.lock:
            if kept.taken:
                return

            entries = kept.entries(self._panel)

            # Setting a value again replaces it, and takes no more room.

            if name not in entries.values and not kept.admit(entries, 1):
                return

            entries.values[name] = str(value)


class Text(_Handle):
    """A text panel for the request being served."""

    __slots__ = ()

    def write(self, text: str) -> bool:
        """Add a block of text, after a blank line if any came before.

        Returns whether it was kept: a block past the panel's caps, or
        past ``TEXT_LIMIT`` characters in all, is not.
        """

        kept = self._kept

        if kept is None:
            return False

        with kept.lock:
            if kept.taken:
                return False

            entries = kept.entries(self._panel)

            if entries.characters + len(text) > TEXT_LIMIT:
                entries.dropped += 1
                return False

            if not kept.admit(entries, 1):
                return False

            entries.text.append(text)
            entries.characters += len(text)

        return True


# Setting up for the first record.

_first: list[Callable[[], None]] = []
_first_started = False


def on_first_record(callback: Callable[[], None]) -> None:
    """Run `callback` once, as the worker starts recording its first request.

    The edge asks a worker for records only in a development run, so this
    is how code sets up what only the toolbar needs, such as a
    sink, without costing anything elsewhere. After the first record has
    started, the callback runs at once.
    """

    if _first_started:
        callback()
        return

    _first.append(callback)


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

    They come in their order of first use, as plain tuples: name, title,
    spans, values, tables, how many entries were not kept, the text, and
    whether it is markdown. A span is a tuple of its name, start,
    duration, category, the spans beneath it and whether it failed.
    """

    kept = _requests.pop(request, None)

    if kept is None:
        return []

    with kept.lock:
        kept.taken = True

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
                "\n\n".join(entries.text),
                declared.markdown,
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
        bool(span.failed),
    )
