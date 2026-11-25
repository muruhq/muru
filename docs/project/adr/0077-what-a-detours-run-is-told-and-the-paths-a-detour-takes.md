---
status: accepted
date: 2026-11-25
---

# ADR 0077: What a detour's run is told, and the paths a detour takes

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

- A detour's run does not check the Host of a request under its
  reserved prefix against loopback and a list of the application's
  names (issue 911). The list the daemon sent when the detour opened
  left out the default host and pattern names and went stale when the
  hosts changed, and the check, which is against DNS rebinding, adds
  nothing when the run's one peer has already matched the request to a
  virtual host and routed it here. The `Sec-Fetch-Site` check stays,
  and the daemon no longer sends the list.

- After the word, `; base=PATH` carries the path of the route the
  request matched as the run receives it: the route's `path` with its
  `strip_prefix` taken off (ADR 0078), left out when empty. The run
  answers its reserved prefix under that path and puts it in front of
  the inserted tag's path, after any forwarded prefix, so behind a
  route at `/app` that strips nothing the toolbar is at
  `/app/_muru/toolbar/` (issue 912). The toolbar's requests then match
  the application's own route and reach the run by the same routing
  and preview as the page. Routing `/_muru/toolbar/` itself to the
  detour cannot work: the host's `/` may be another application's, and
  under `all`, `share=N` or `class=NAME` such a request names no
  application. This reserves `/app/_muru/` in the application's paths
  under that route, as `/_muru/` is at the root under `muructl run`.

### The paths a detour takes (issue 909)

A detour took all of an application's paths or none: a preview reached
it on every route, and `all`, `share=N` and `class=NAME` were laid over
every route (ADR 0075). A developer working on one part of an
application, its API under `/app/api` say, often wants only that part
from the checkout. Considered: routes derived in the overlay, a copy of
each route at the scope's path, which covers the overlay's choices but
not preview, which the edge honours on every route, and whose copies
would count as routes of their own for rate-limit buckets, route names
and sticky salts; and a check in the edge after the release choice,
chosen.

- `muructl detour run` and `muructl detour traffic` take `--path
  PREFIX`, more than once, and `detour traffic --all-paths` clears the
  scope; `detour run --mount NAME` is a shorthand for the URL path of
  the checkout's mount under each of the application's routes, after
  the route's `strip_prefix`. A mount with no path is refused.

- A scope path is a client URL path, from the host's root, matched on
  segment boundaries on the path the router matched, before any
  `strip_prefix`, since it is written as the developer sees the URLs.
  It decides which requests reach the detour and nothing else: the
  application sees `SCRIPT_NAME` and `PATH_INFO` as its mounts, the
  route's `strip_prefix` and a trusted prefix give them.

- The snapshot's detour release carries its paths. After the release
  choice returns a detour, a path outside them goes to the serving
  release instead, for a preview and every overlay choice alike, with
  the preview cookie kept and a sticky cookie still recording the
  client's assignment. A previewed request so sent gets a
  `Server-Timing` entry `muru-release;desc="RELEASE"`, and the toolbar's
  "This page" tab says it was served by the serving release. An
  internal redirect is pinned to the release it came from, so one from
  the detour to a path outside its scope stays on the detour.

- The detour's toolbar, under `{route path}/_muru/toolbar/` (above), is
  in its scope for its previewed requests whatever paths it names, so a
  page the detour served can load it.

- Two detours of one application may both take `all` when their paths
  do not overlap: the route's policy names both, the first with the
  whole and the rest with none, since shares cannot add up to more, and
  the edge gives a request outside the first's paths to another detour
  the policy names whose paths take it. Overlapping scopes, or one
  taking every path, are refused as two detours taking all were. A
  scoped `all` or `share` is refused during a rollout as an unscoped
  one is.

- The paths show in `detour list`, `release list`, `overlay list`, the
  dashboard, the run's start-up line, the `detour.opened` and
  `detour.traffic` events, and the MCP `detour_traffic` tool takes them.

This supersedes the part of ADR 0075 that says the overlay covers each
of the application's routes whole.

### Consequences

- Good: the toolbar and its records are the developer's alone, and
  consistent: every toolbar request carries the preview that chose the
  page's run.

- Good: the run knows nothing it must keep secret, and a client cannot
  pass itself off as previewed.

- Bad: a developer who wants the toolbar on shared traffic cannot have
  it; they preview, or read the run's dashboard.

- Good: one part of an application can come from a checkout while the
  rest stays on the serving release.

- Bad: one client's pages are served by two releases, so sessions and
  CSRF tokens hold across the split only when both share the session
  store and the secret key.

## More information

Issues 913 and 912, the internal header's two uses, 911, the toolbar's
host names, and 909, the paths a detour takes; sections 4.20, 4.21,
9.13, 10.12 and 13.12. Supersedes in part ADR 0075.
