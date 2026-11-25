---
status: accepted
date: 2026-11-25
---

# ADR 0077: The daemon's edge tells a detour's run how each request reached it

## Context and problem statement

A detour's run (sections 4.21, 9.13 and 13.12, ADR 0075) is a
development run behind the daemon's edge. Its edge decides on the
toolbar from its own `--toolbar` setting and the response alone, so
every client the daemon sends to the detour, by `all`, `share=N` or
`class=NAME` as well as by preview, gets the toolbar and can read the
run's request records through `/_muru/toolbar/`: other clients' paths,
addresses, timings, logs and errors (issue 913). Only the daemon's edge
knows how it chose the detour for a request, and only it knows which
route the request matched, which the toolbar needs to sit under a
route at a sub path (issue 912). The question is how the run learns
what the daemon's edge knows about each request, and what it does with
it.

## Decision drivers

- The toolbar's records are the developer's, as they are under
  `muructl run`, which turns the toolbar off under `--allow-public`.

- A previewed page and every request its toolbar makes must reach the
  same run whatever the traffic shares are; a preview does, since the
  edge honours it ahead of classes, stickiness and the split.

- What the run is told must not be something a client can send.

- One channel for what the run needs to know, so that issue 912's
  route path and later facts travel the same way.

## Considered options

- An internal request header the daemon's edge alone sets, which the
  run's edge trusts from its one peer and takes off before the
  application sees the request.

- A second preview secret the run knows, checked by the run's edge
  against the preview cookie. The daemon's edge strips the preview
  cookie and header before the run, on purpose, so the secret never
  reaches the run or the application.

- An option to show the toolbar to every client the detour serves.

## Decision outcome

Chosen option: the internal header, `x-muru-detour`, with no option to
show the toolbar to clients a traffic choice sends.

- The daemon's edge sets it on every request to a detour: `preview`
  when the request carries the detour's preview (its release and
  secret, by header or cookie), whatever chose the detour for it,
  otherwise `traffic`. A client's own header is removed with every
  other X-Muru request header before the request reaches any upstream.

- The run's edge takes the header off each request before anything
  else looks at it, and believes it only on its Unix socket, which the
  daemon's edge alone reaches. The application never sees it.

- A detour's run inserts the toolbar only into pages of previewed
  requests, and answers its reserved prefix only for previewed
  requests, with 404 for any other. Every request is still recorded,
  and the run's dashboard and the daemon's access log, metrics and
  dashboard show the shared traffic.

- The value has room for more: `; base=PATH` after the word carries a
  path, percent-encoded, which issue 912 fills with the matched route's
  path.

### Consequences

- Good: the toolbar and its records are the developer's alone, and
  consistent: every toolbar request carries the preview that chose the
  page's run.

- Good: the run knows nothing it must keep secret, and a client cannot
  pass itself off as previewed.

- Bad: a developer who wants the toolbar on shared traffic cannot have
  it; they preview, or read the run's dashboard.

## More information

Issues 913 and 912, the internal header's two uses; sections 4.20,
4.21 and 13.12.
