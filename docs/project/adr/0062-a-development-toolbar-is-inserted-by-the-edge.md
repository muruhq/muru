---
status: accepted
date: 2026-11-13
---

# ADR 0062: A development toolbar is inserted by the edge

## Context and problem statement

While an application is developed under `muructl run`, what Muru knows
about one request is spread over the access log, the dashboard's recent
requests and the terminal, and matching them up means copying a request
ID by hand. Issue 536 collected ideas for showing it in the browser,
beside the page the request produced, as the debug toolbars of web
frameworks do. Settled with the maintainer on 2026-11-13 and tracked by
issue 642, the question here is how the toolbar reaches the page, where
its files and data come from, and in which modes it exists (sections 4,
11.7 and 13.1).

## Decision drivers

- No change to the application: a project that runs under `muructl run`
  gets the toolbar without a dependency, a middleware or a template tag.

- The toolbar reads what the edge and the workers saw, which lives in
  the run's process, not in the application.

- The page and the toolbar's requests share an origin, so the browser's
  cookies, its content security policy and a development proxy that
  serves the run under a path (jupyter-server-proxy) apply to both
  alike.

- Nothing of it in production: a toolbar shows internals, so it belongs
  to a run on loopback that the person at the terminal started.

- A change to an HTML response can break a page, so it is asked for,
  not imposed.

## Considered options

- A template helper in the application, as Flask and Django debug
  toolbars are installed.

- A browser extension that reads the response headers and asks the
  dashboard for the rest.

- The toolbar served from the dashboard's origin, reached from the
  page through CORS or in a frame.

- Headers only: `Server-Timing` and a link to the dashboard, with no
  toolbar on the page.

- The edge inserts the toolbar's tag into HTML pages, opt-in, under
  `muructl run` only, with its files and data at a reserved prefix on
  the application's own origin.

## Decision outcome

Chosen option: the edge inserts the toolbar, because it is the only one
that needs nothing from the application or the browser and keeps every
request on one origin.

- **Headers on every response of a development run,** with no option:
  `Server-Timing` with the queue and application times and the request
  ID, and `Muru-Request-Link` naming the request's page on the
  dashboard. They cost a few bytes, change nothing a page does, and
  show in the browser's own Network tab. The option of headers alone is
  kept this way for JSON APIs and pages that cannot carry a toolbar.

- **The toolbar is opt-in,** with `--toolbar` on `muructl run` or
  `toolbar = true` in `[tool.muru.run]` or `muru.local.toml`. With it,
  the edge adds one `<script>` tag before the end of the body of each
  HTML page, under the rules of section 4.20.

- **A reserved prefix, `/_muru/`,** on the application's origin, is
  answered by the edge from the run's request recorder: the toolbar's
  script and stylesheet, each request's record and its panels. Nothing
  under it reaches a worker.

- **Development mode only.** `muructl run` on loopback has the headers,
  the recorder, the prefix and the toolbar; under `--allow-public`,
  `muructl serve` and the daemon none of them exist. Forwarding W3C
  trace context, which the toolbar's trace builds on, is not part of the
  toolbar and applies in every mode (section 11.6).

- **The toolbar only reads,** as the dashboard does (ADR 0060).

### Consequences

- Good: a page is inspected where it is looked at, with no install, and
  a JSON API still gets its timings in the Network tab and a link to the
  record.

- Good: one origin means no CORS, no frame and no second sign-in, and
  the prefix works behind a development proxy that serves the run under
  a path.

- Bad: the edge rewrites HTML bodies when asked to, so it has to know
  which responses it may change (no encoded, partial or empty bodies,
  no files it sends) and how to satisfy a content security policy.

- Bad: an application route under `/_muru/` is not reached in a
  development run. The prefix was chosen as unlikely to be one.

- Bad: a page whose HTML another server serves (a front end's own
  development server calling a Python API that Muru serves) gets no
  toolbar, only the headers on its API calls.

## More information

Sections 4.20, 10.7, 11.7 and 13.1 of the design specification; issues
536, 642 and 632 to 641; ADR 0063, which says how the toolbar's panels
are drawn; ADR 0060, the read-only dashboard.
