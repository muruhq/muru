---
status: accepted
date: 2026-11-23
---

# ADR 0075: A detour is a release a development process holds open

## Context and problem statement

`muructl run` serves an application on a port of its own, so code being
edited is never served through the routing it will live behind: a
daemon's TLS and host names, several applications under one virtual
host, authentication sub-requests, rate limits, client classes, static
serving and the applied configuration. Deploying every change to a local
daemon gives that routing but loses what development mode gives (reload,
the toolbar, tracebacks, a debugger with `--single`), and a managed HTTP
release whose command is `muructl run` makes the daemon own the process,
its output and its lifetime. The question is how a development run is
put behind a local daemon's edge as a release of an application, for as
long as it runs and no longer (issue 873; sections 4, 9, 10 and 13).

## Decision drivers

- Everything development mode gives carries through, and the daemon
  never owns the process: the developer's terminal does.

- Nothing about development reaches the files that describe production,
  and nothing a development process does outlives it.

- Other clients see no change until the developer asks for one, and
  asking is a conscious act on a command line.

- A system daemon's real traffic never reaches a process owned by
  whoever typed a command.

- The daemon's routing, policies and figures stay as they are for every
  other release.

## Considered options

- **The command.** A separate `muructl detour` command, or an option of
  `muructl run`.

- **The hop.** The daemon's edge reaching the run's edge over HTTP, or
  speaking the worker link to the run's workers.

- **The lifetime.** A streaming admin call that holds the release, or a
  release stored by the daemon and removed by a later call.

- **The traffic.** Preview only by default with more by a runtime-only
  overlay the detour owns, or a stable `APP@detour` name for ordinary
  overlays in applied files.

- **The hop protocol** (issue 875). `http1` only with gRPC refused; `h2c`
  for everything with WebSockets over HTTP/2; or `h2c` for gRPC and
  `http1` for everything else, chosen per request.

- **The name.** Detour, or intercept, graft, splice, divert, shunt,
  bypass, branch, fork or tap.

## Decision outcome

Chosen: a separate command whose run holds a release of kind `detour`
open with a streaming admin call, reached by the daemon's edge over HTTP
on a Unix socket, with the protocol chosen per request, preview only by
default and more traffic by an overlay the detour owns; detours a daemon
setting, off by default and refused on a system daemon; and the name
detour.

- **A separate command.** Most of `muructl run`'s flags are about its
  listeners (`--bind`, `--port`, `--https` and its keys, `--host`,
  `--allow-public`, `--open`), which a detour does not have, and the
  detour's own choices (`--traffic`, `--open-preview`, `--app-data`,
  `--label`) send a daemon's traffic to a checkout, which should be
  chosen on each command line rather than from a file. `muructl detour
  run` reads the `[run]` table as run does and ignores, never refuses,
  the keys about a listener, its hosts and its compression; there is no
  `[detour]` table.

- **HTTP between the edges.** The worker link would tie the run's
  controller to the daemon's: endpoints, reload switches, descriptor
  passing across users, the daemon's view of the run's workers. Over
  HTTP the daemon's edge sees one stable address while the run reloads
  behind its own edge, and the toolbar, `Server-Timing` and
  `Muru-Request-Link` come from the run's edge unchanged.

- **A stream holds the release.** The release exists while the call is
  open and is never written to the store, so a crash, a dropped
  connection or a daemon restart cannot leave one behind. A daemon that
  restarts has no detours, and the run opens its call again, retrying
  while it runs. The run's registry entry and its lock (ADR 0069) are a
  second check that the process is alive.

- **Preview only by default.** Opening a detour changes nothing for
  other clients. A one-time entry link gives a browser a cookie with a
  secret the daemon made for the detour, since a bare release name is
  guessable and a detour shows tracebacks. More traffic is a runtime
  overlay `detour:APP`: listed by `overlay list` with the source
  `detour`, ended with the detour, and never declared in, exported to or
  promoted from configuration. A stable name for ordinary overlays was
  rejected because it puts development into the files that describe
  production.

- **Off by default, never on a system daemon.** On a system daemon the
  edge would send real traffic to a process owned by whoever ran the
  command, so the setting refuses it there.

- **The namespaces under `/_muru/`.** A run's toolbar is under
  `/_muru/toolbar/` (issue 872). A daemon with detours enabled answers
  `/_muru/preview/` itself, on every host, and only then; everything else
  under `/_muru/` goes to the routed release, so a detour's toolbar is
  reached through the preview.

- **`h2c` for gRPC, `http1` for everything else** (issue 875, decided by
  Graham on 2026-11-23). A detour serves whatever the application
  serves. `http1` alone loses gRPC, which needs HTTP/2 end to end; `h2c`
  alone loses WebSockets, which the edge relays to `http1` upstreams
  only, and WebSockets over HTTP/2 (RFC 8441) still lack their tests
  (issue 318). The run's socket accepts both, told apart by the
  connection preface. The choice stays inside the edge's handling of a
  detour; no other upstream gains a per-request protocol.

- **The name.** Traffic takes a detour through the developer's checkout
  and goes back to the main road when it ends, and that it is temporary
  is what keeps development apart from production. Intercept and tap
  suggest watching traffic rather than serving it; graft and splice
  suggest something joined for good; divert and shunt say where traffic
  goes but not that it comes back; bypass says the opposite of going
  through the daemon; branch and fork are taken by git and by process
  management.

### Details fixed by the implementation

These are smaller choices the issues left open.

- **The release's name.** `dev-` and the first four hexadecimal digits
  of the run's instance ID (`dev-7f3a`), longer when two open detours of
  one application would share it, so a run that opens its detour again
  after a daemon restart keeps its name. `APP@detour` names the
  application's one detour and is refused, listing the candidates, when
  it has several. A deploy or an apply refuses a release ID of `detour`
  or one starting with `dev-`, which are kept for detours.

- **The admin calls.** `OpenDetour` is a server stream: the request
  names the application, the run's socket, its label, its instance ID,
  pid and directory, and the traffic wanted; the first message names the
  release, the application's host names and its data directory, and
  later messages carry a traffic change. `ListDetours`, `SetDetourTraffic`
  and `PreviewDetour` are unary. No call streams from the client, since
  everything the run sends after opening is said by closing the stream.

- **The setting.** `detours = true` in the `[daemon]` table, read when
  the daemon starts. A system daemon whose file sets it refuses to start,
  saying why.

- **The edge's policy for a detour.** No passive ejection and no active
  probes; no request deadline, and a response idle timeout of an hour,
  so a request held at a breakpoint is answered when the debugger lets
  it go; no error override and none of the route's, host's or node's
  error pages applied to the detour's responses, while the edge's own
  responses (a 502 when the socket refuses) keep theirs; response
  headers passed through after the host's and route's rules; X-Muru
  headers trusted, as for a managed HTTP release. The relays of
  WebSockets opened through a detour are closed when a snapshot no
  longer has the detour.

- **The preview secret and tokens.** The secret is 128 random bits in
  hexadecimal, made by the daemon when the detour opens. The cookie and
  the `Muru-Preview` header carry `dev-7f3a.SECRET`; a bare `dev-7f3a`
  is ignored, as is a sticky cookie naming a detour the route's policy
  does not include. Entry tokens are 128 random bits, valid for a
  minute and used once; the daemon sends each edge the token's SHA-256,
  its expiry, the release and the path to redirect to, and the edge
  keeps no token itself, at most 1024 of them. A used, expired or
  unknown token is a 404 page saying so. The preview paths are answered
  before routing and recorded nowhere, so a token never reaches the
  access log. The cookie is a session cookie, `HttpOnly`,
  `SameSite=Lax`, `Path=/`, `Secure` on HTTPS and without a `Domain`; a
  cookie of the detour form that names no open detour is expired by the
  response it gets.

- **The run.** The socket is bound before the host starts, so the
  daemon can check it when the detour opens, and connections wait in its
  backlog until the edge takes them. The run's own dashboard is on a
  free loopback port by default, since a fixed one would stop a detour
  beside a `muructl run`. The daemon's edge strips its preview header and
  cookie from what it sends the run, so the application never sees the
  secret, and adds X-Forwarded-Port, since the run has no port of its own
  for SERVER_PORT.

- **Retrying.** A run whose call ends while it is still running says so
  once and opens it again every two seconds, saying when it is back.
  The first open must succeed, or the run does not start.

- **Traffic.** `all` pins each of the application's routes, and the
  default traffic when no host is declared, to the detour; `share=N` is
  a split of N percent to the detour and the rest to the serving
  release; `class=NAME` gives the class the detour in each route's
  `traffic.classes`. The overlay is applied over every declared one.
  The overlay commands refuse to activate, end or promote it, since its
  life is the detour's.

- **Figures.** A detour's requests carry the group label `detour` in
  the request metrics, with the detour's release; conditions, rollout
  gates and the autoscaler skip samples of that group.

- **The instance.** The run is a registry entry of kind `detour` whose
  tag is `detour:` and its short ID. Commands run from its checkout reach
  it as they reach a run.

- **The MCP tools.** Two rather than one `detour` tool: `detours`, which
  reads (the list, and an entry link with the header value), and
  `detour_traffic`, which changes, since a tool is marked as one or the
  other and `--read-only` would otherwise leave out listing and links.

- **Choosing a detour.** The admin calls name a detour by its
  application and release; muructl chooses it from ListDetours by its
  release, then its application when one detour has it, then the
  instance resolver over the detours' runs (tag, ID prefix, label), and
  with nothing named the project's application in a checkout, else the
  one detour open.

### Consequences

- Good: code being edited is served through the routing it will live
  behind, with reload, the toolbar, tracebacks and a debugger, and
  nothing is configured to get there.

- Good: a detour cannot outlive its process or reach a file, so
  development cannot leak into production's configuration.

- Good: WebSockets and gRPC both work through a detour with no setting.

- Bad: the release a detour adds is known only to the daemon that holds
  it; `release list` and the dashboard show it from memory, and a
  daemon restart drops it until the run opens it again, with a new
  secret, so preview cookies from before the restart need a new link.

- Bad: a detour reaches only a daemon on the same machine and user,
  since its socket is the run's own; a team's shared development daemon
  is out of reach.

- Bad: the edge carries a second policy for one kind of upstream, which
  tests have to cover apart from the generic path.

## More information

Issue 873 tracks the work, with issues 874 to 882. Sections 4.21, 9.13,
10.12 and 13.12 of the specification. ADR 0014 (managed HTTP releases),
ADR 0062 (the toolbar inserted by the edge), ADR 0068 (promoting a pin),
ADR 0069 (the instance registry) and ADR 0074 (selecting a running
instance), whose resolver chooses among detours.
