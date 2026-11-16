# Adding panels to the toolbar

Code in the application, or in a library it uses, can add panels of its
own to the [development toolbar](toolbar.md) with `muru.panels`. A panel
is data: values, tables, spans on the request's clock, or text, plain or
markdown. Muru draws it after the built-in panels, the same way in the
toolbar, on the dashboard's request page and by `muructl debug request`.

Panels are kept only for the requests a development run records, under
`muructl run`. Everywhere else the same code runs and keeps nothing, so
it needs no check for where it runs.

## Declaring a panel

A panel is declared once, at module level, by the class for what it
holds, with a name and the title its tab shows:

```python
from muru.panels import SpansPanel, TablesPanel, TextPanel, ValuesPanel

CACHE = ValuesPanel("cache", "Cache")
QUERIES = TablesPanel(
    "queries",
    "Queries",
    tables={
        "By statement": ["statement", "times", "total ms"],
        "Each query": ["at ms", "ms", "statement"],
    },
)
STEPS = SpansPanel("steps", "Steps")
ANALYSIS = TextPanel("analysis", "Analysis", markdown=True)
```

A name is lowercase letters, digits and hyphens, starting with a letter,
at most 32 characters. The built-in panels' names, `timing`, `where`,
`response`, `logs`, `errors`, `connection` and `hints`, `page` and
`history`, the toolbar's own tabs, and `trace`, `sql`, `outbound` and `exceptions`, the panels
[tracing](tracing.md#in-the-toolbar) adds, cannot be used, and raise
`ValueError`. Declaring a panel again the same way,
as a module imported twice does, is allowed; declaring a name again
another way raises `ValueError`.

A tables panel names each of its tables by caption, with its columns.
Columns whose cells are all numbers are aligned right.

## Adding to a panel

During a request, `current()` on a declared panel gives the panel for the
request being served, and its methods add to it:

```python
def books(request):
    CACHE.current()["hits"] = cache.hits

    queries = QUERIES.current()

    for statement, took in timed_queries:
        queries.table("Each query").add_row([queries.now(), took, statement])

    with STEPS.current().span("render", category="template"):
        page = render(books)

    ANALYSIS.current().write("**3 queries** could be one. See `/books/joined`.")
    return page
```

For a request that is not recorded, `current()` gives a panel that keeps
nothing: setting a value, adding a row or a span, and writing text do
nothing, a timed block runs as it would otherwise, and the clock reads 0.
A table's caption is checked either way, so a wrong one fails in
development and production alike.

What each kind of panel takes:

| Panel | Adding to it |
| --- | --- |
| `ValuesPanel` | `panel[name] = value` sets a value, shown as text; setting it again replaces it |
| `TablesPanel` | `panel.table(caption).add_row(cells)` adds a row, each cell shown as text |
| `SpansPanel` | `panel.add(Span(name, start, duration, category, children, failed))` adds a span with those beneath it; `with panel.span(name, category=...)` times a block as one |
| `TextPanel` | `panel.write(text)` adds a block of text, after a blank line when there is one before it |

The add methods, `add_row()`, `add()` and `write()`, return whether the
entry was kept.

### Spans

A span is a named interval on the request's clock, with a category, the
spans beneath it, and whether it failed. A block timed with `span()`
inside another of the same panel's, in the same request, goes beneath
it, and each is added as its block ends, however the block ends. A block
that an exception escapes is marked failed, and the exception goes on as
it would without the block:

```python
steps = STEPS.current()

with steps.span("view", category="view"):
    with steps.span("query", category="database"):
        rows = run_query()
```

To add spans recorded another way, build `Span` objects, with times in
milliseconds on the request's clock, and add the outermost:

```python
from muru.panels import Span

steps = STEPS.current()
start = steps.now()
rows = run_query()
steps.add(Span("query", start, steps.now() - start, "database"))
```

`Span(..., failed=True)` marks a span added this way as failed.

The toolbar draws each span as a bar on the request's timeline, coloured
by its category, with those beneath it indented under it. A failed span
keeps its category's colour, with a red outline on its bar, a ✕ beside
its name and "failed" in its bar's tooltip; `muructl debug request`
writes `failed` at the end of its line, and the record's JSON has
`"failed": true` on it.

### Text

A plain text panel shows its text as written, keeping its line breaks.
A markdown panel, declared with `markdown=True`, is drawn from
CommonMark with tables and strikethrough: headings, emphasis, lists,
quotes, rules, code blocks and inline code, and tables with their columns'
alignment. HTML written into the markdown is shown as text. A link is
to a whole `http` or `https` address, such as
`http://127.0.0.1:8080/books`, since a panel is shown on the dashboard's
address too, and opens in a new tab; the page opened is not told the
address of the one it came from. A relative link is shown as its text,
without the link.

## The request's clock

A panel's times are milliseconds since the worker took the request.
`now()` gives the current time on that clock, and `at(reading)` turns a
`time.perf_counter()` reading taken during the request into it. Muru
moves a panel's spans by the time the request waited for a worker, so
they line up with the Timing panel's phases.

## Limits

A panel keeps at most 500 entries for a request, and a request at most
2,000 across its panels; a span counts once for itself and once for each
span beneath it. A text panel keeps at most 65,536 characters. What is
past a limit is not kept, and the panel says how many entries were not
kept. Code that leaves entries out itself, to fit, can count them on the
panel with `add_dropped(count)`, so the panel says so too.

## Work only for the toolbar

Building a panel's entries can cost more than adding them. `active` says
whether the panel keeps anything for the request being served:

```python
analysis = ANALYSIS.current()

if analysis.active:
    analysis.write(explain(plan))
```

`muru.panels.on_first_record(callback)` runs `callback` once, when the
worker starts recording its first request, which happens only under
`muructl run`. It is the place to set up something only the panels need,
such as a hook into a library, so it costs nothing elsewhere.

## Threads

`current()` finds the request from the request's context. A thread the
application starts copies that context only when it is started with it,
as `asyncio.to_thread()` and `contextvars.copy_context().run` do; in
another thread, `current()` gives a panel that keeps nothing. Getting the
panel in the request and handing it to the thread works either way: a
panel stays the request's wherever it is used. Once the request's record
has been taken, after its response, the panel keeps nothing more.
