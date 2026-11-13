---
status: accepted
date: 2026-11-13
---

# ADR 0063: Toolbar panels are data drawn by templates

## Context and problem statement

The development toolbar (ADR 0062) and the dashboard's request page show
one request's record as panels: its timing, where it was routed, its
response, its connection, and later its log lines, errors and a trace of
what the application did. Muru's own parts add panels first, and the
mechanism has to take panels from instrumentation later (issue 641)
without the toolbar and the dashboard drawing them differently. The
question, settled with the maintainer on 2026-11-13, is what a panel is
and who turns it into HTML (sections 10.7 and 11.7).

## Decision drivers

- One look in the toolbar, on the dashboard and, as JSON, in `muructl`
  and the MCP server, from one definition of each panel.

- Panels from outside Muru's own code must not run script in the
  application's origin, where the toolbar lives.

- What is kept per request is bounded, and a panel may be drawn long
  after the request, or never.

- The built-in panels prove the mechanism that later panels use, rather
  than taking a path of their own.

## Considered options

- Panels that bring their own JavaScript, as some framework toolbars
  allow.

- HTML rendered by the worker when a panel is opened, from data it
  kept.

- HTML rendered when the request ends and stored with the record.

- askama templates, checked at compile time, for the built-in panels,
  with another mechanism for panels added later.

- Panels as data in the record, drawn by the edge from minijinja
  templates whose output is sanitised, the built-in panels the same way,
  with no panel JavaScript.

## Decision outcome

Chosen option: panels are data drawn by templates, because it gives one
mechanism for every panel, keeps script out of panels and keeps the
record small.

- **A panel is data in the record,** in one of three shapes: `spans`,
  named intervals on the request's clock, which may nest; `table`,
  columns and rows; and `values`, names with values. The JSON of the
  record carries the data, not HTML.

- **A template draws it.** The edge renders each panel from a minijinja
  template when the panel is asked for, in the run's process. The
  built-in panels (Timing, Where, Response and Connection first) are
  templates embedded in the binary.

- **The output is sanitised,** the built-in panels' included, with
  ammonia and an allowlist: text, tables, lists, `code`, `pre`,
  `details`, `summary`, `meter`, `progress`, `time` and `abbr`; the
  `class` attribute, with `title`, `datetime`, `value`, `min` and `max`
  where those elements take them; and links only to `/_muru/` or the
  dashboard. A small stylesheet gives panels their classes: muted,
  monospace, warning, error, badge and numbers aligned right.

- **No panel JavaScript.** The toolbar's own script switches tabs,
  loads a panel when it is opened, and reloads a panel when a link in it
  is followed.

- **The dashboard's request page** is an askama shell holding the same
  rendered panels, so the toolbar and the dashboard cannot drift apart.

### Consequences

- Good: a panel added later is a template and data, and is drawn in
  the toolbar and on the dashboard with nothing more.

- Good: the sanitiser is exercised by Muru's own panels before anyone
  else depends on its allowlist.

- Good: the record holds data alone, bounded by the recorder, and
  nothing is rendered for a panel nobody opens.

- Bad: askama's compile-time checks are lost for the panel templates.
  Each built-in template has a unit test that renders it from a fixture
  record instead.

- Bad: a panel cannot be interactive beyond what the toolbar's script
  offers: tabs, lazy loading and links that reload the panel.

## More information

Sections 10.7 and 11.7 of the design specification; issues 634 and 641;
ADR 0062, the toolbar this draws for; ADR 0011, the dashboard's askama
pages.
