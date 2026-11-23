# Develop behind a local daemon with a detour

A detour serves your working copy of an application through a daemon on
your own machine, as a release of that application. Requests reach it
through the daemon's real routing: its host names and TLS, the other
applications under the same virtual host, authentication sub-requests,
rate limits, client classes, static files and the configuration you
applied. The code is served by a development run in your terminal, with
everything `muructl run` gives: reload on change, the toolbar, tracebacks
and a debugger with `--single`. The daemon routes to it but never starts,
builds or stops it.

Traffic takes the detour only while the run is there. When it ends, by
Ctrl-C or because the process exits, the daemon removes the release and
every request goes back to the serving release at once.

## Turning detours on

Detours are off by default. A user daemon takes them with `detours =
true` in the `[daemon]` table of its configuration file:

```toml
[daemon]
detours = true
```

The setting is read when the daemon starts. A system daemon refuses it,
and refuses a detour with a message saying why: its edge serves real
traffic, and a detour would send that traffic to a process owned by
whoever ran the command.

Nothing about a detour is declared in configuration files. It exists
only while its run does.

## Starting a detour

From the application's checkout, with the application already deployed
to the daemon:

```console
$ muructl detour run
muru: using [tool.muru] from ./pyproject.toml
...
muru: serving shop.wsgi:application on detour shop@dev-7f3a of the daemon at /run/user/1000/muru/admin.sock with 2 workers of 3 threads
muru: traffic: previews only; muructl detour preview gives an entry link
muru: development mode: error pages show tracebacks, no releases are kept, the run reloads on change, and the daemon's edge serves the clients and compresses; Ctrl-C ends the detour
muru: dashboard on http://127.0.0.1:52817/
muru: admin socket /run/user/1000/muru/run-7f3a91c2/admin.sock
muru: watching /home/me/shop for changes to **/*.py
```

The application is the one the checkout's commands name: `[project]
name` in `pyproject.toml`, or the module of the `[tool.muru]` entrypoint.
Name another with `muructl detour run APP`. The daemon is the one the
checkout's commands reach: `--instance`, `MURU_INSTANCE`, `instance` in
`muru.local.toml`, or the user daemon.

The run serves on a Unix socket in its own run directory and has no
port. It holds its detour open for as long as it runs. If the daemon
restarts, the run says the detour is closed and opens it again as soon
as the daemon is back:

```console
muru: the detour is closed, the daemon ended it: the daemon is stopping; opening it again every 2 seconds while this runs
muru: detour shop@dev-7f3a is open again
```

The run's own dashboard is on a free loopback port unless `--dashboard`
names an address, or `off`.

`detour run` takes the settings of a development run: `--processes`,
`--threads`, `--single`, `--python`, `--chdir`, `--build`, `--env-file`,
`--max-body-size`, `--reload` and its patterns, `--toolbar`,
`--dashboard` and the
`[tool.muru.run]` table in `pyproject.toml`, `muru.toml` and
`muru.local.toml` (see [settings for muructl run](run-settings.md)). The
keys of that table about the run's own listener are not used by a
detour, and are never refused, so a project with `port = 8000` starts a
detour as it is:

| Key | Not used because |
| --- | --- |
| `port`, `https`, `https_only`, `https_port`, `tls_cert`, `tls_key` | the detour has no listener; the daemon's edge serves the client |
| `hosts` | the host names come from the daemon |
| `compression` | the daemon's edge compresses responses, by its hosts' settings |

`muructl detour run --print-config` lists them as not used by a detour.
`--bind`, `--port`, `--allow-public` and `--open` do not exist on
`detour run`.

## Entering preview

A detour takes no traffic until you ask for it. The way in is preview: a
browser with the detour's preview cookie gets the detour, and every
other client gets the serving release as before.

`--open-preview` opens an entry link for the application's best address
in the browser once the detour is open:

```console
$ muructl detour run --open-preview
...
muru: opened https://shop.localhost:8443/ in preview of shop@dev-7f3a (2 other addresses: muructl detour preview --list)
```

The link is used once, within a minute. Following it gives the browser
the preview cookie and sends it on to the application's address. The
cookie lasts until the browser closes, the detour ends or you leave
preview.

`muructl detour preview` gives a new link while the detour runs:

```console
$ muructl detour preview                      # open a link for the best address
$ muructl detour preview --host admin.localhost
$ muructl detour preview --list               # a link for each of the application's addresses
$ muructl detour preview --print              # print a link, open nothing
$ muructl detour preview --header
Muru-Preview: dev-7f3a.3f9c0e7a51b2d4c68e1f0a9b7c5d3e21
```

`--header` prints the header that previews the detour without a cookie,
for curl or a test client:

```console
$ curl -H "$(muructl detour preview --header)" https://shop.localhost:8443/
```

The cookie and the header carry a secret the daemon makes when the
detour opens, so a detour cannot be reached by guessing its name. A
cookie left from a detour that has ended reaches nothing: the request
goes to the serving release and its response removes the cookie.

The toolbar's badge says the page came through the detour (`detour:
preview`), with a link to leave preview. Leaving preview is the address
`/_muru/preview/leave` on any of the application's hosts: it removes the
cookie and goes to `/`.

While detours are on, the daemon's edge answers paths under
`/_muru/preview/` itself, on every host. Everything else under `/_muru/`
goes to the application, so the toolbar of a detour, under
`/_muru/toolbar/`, is reached through the preview like the pages.

## Sending more traffic

`--traffic` chooses what the detour takes when it opens, and `muructl
detour traffic` changes it while it runs:

| Choice | Goes to the detour |
| --- | --- |
| `preview` (the default) | previews only |
| `all` | every request for the application, on every route |
| `share=N` | N percent of the application's clients, kept on the detour by the bucketing cookie |
| `class=NAME` | requests of the client class `NAME`, one the daemon already defines |

```console
$ muructl detour traffic all
muru: detour shop@dev-7f3a now takes all of shop's traffic (was preview)
$ muructl detour traffic preview
muru: detour shop@dev-7f3a now takes previews only (was all)
```

A choice other than preview is an overlay named `detour:APP`, owned by
the detour: `muructl overlay list` shows it with the source `detour`,
active while the choice holds, and it ends with the detour. It cannot be
activated, ended, exported or promoted with the overlay commands, and
nothing of it is written to the daemon's state. The serving release
keeps running and keeps its status, so when the detour ends every
request is back on it at once. A preview reaches the detour whatever the
choice.

`all` and `share` are refused while a rollout of the application is
under way, and `all` is refused while another detour of the application
takes all of it.

## What the daemon leaves to the run

The daemon's edge treats a detour as an upstream that a person is
working on:

- **Errors are the run's.** The daemon's error pages and error override
  are not applied to a detour's responses, so a traceback page reaches
  the browser as the run made it. A page the daemon's edge makes itself,
  such as a 502 when the run is not answering, is the daemon's as for any
  release.

- **No deadline.** A request held at a breakpoint is answered when the
  debugger lets it go: the detour has no request deadline, a response may
  pause for up to an hour, and the run is never taken out of service for
  failing.

- **Headers pass through.** `Server-Timing`, `Muru-Request-Link` and the
  run's other response headers reach the browser, after the host's and
  route's header rules, as for any release. The run takes the daemon's
  `X-Request-ID`, so its request records and the daemon's access log
  share their IDs, and it trusts the daemon's forwarding headers, so the
  application sees the client's host, scheme and address.

- **Compression is the daemon's.** The run sends responses uncompressed,
  and the daemon's edge compresses them by the host's settings, as it
  does in production. A response the application encoded itself is left
  alone.

- **Every protocol.** WebSockets and gRPC calls go through a detour as
  they go to the application, with nothing to set. WebSockets open
  through the detour close when it ends.

## Several detours of one application

Two checkouts of one application, such as two worktrees, can each run a
detour, each previewed by its own cookie. Give each a label to tell them
apart:

```console
$ muructl detour run --label feature-x
```

`muructl detour traffic` and `muructl detour preview` choose among them
by the application when one detour has it, and otherwise by the label,
the tag `instance list` shows, or a prefix of the detour's ID, given as
`APP`:

```console
$ muructl detour preview feature-x
```

An application name with several detours is refused with the value to
pass for each.

## Seeing what is open

```console
$ muructl detour list
APP    RELEASE   LABEL      PID    TRAFFIC  SINCE  DIRECTORY
shop   dev-7f3a  feature-x  41872  preview  12m    /home/me/shop-feature-x
```

`--json` gives the same as JSON. `muructl release list shop` shows the
detour beside the application's releases, with the kind `detour` and its
traffic, and the dashboard marks it as a development detour. The events
`detour.opened`, `detour.traffic` and `detour.closed` record each one.

The run is an instance of its own: `muructl instance list` shows it with
the kind `detour`, and `muructl status`, `logs`, `events`, `debug` and
the other commands reach it by the value in the NAME column, or by its
label, with `--instance`. Run from the detour's checkout, they reach the
detour's run, as they would a `muructl run`, and `deploy` and the other
commands only a daemon answers reach the daemon.

## Figures

A detour's requests are in the daemon's access log, `muructl metrics` and
the dashboard, under the application with the detour's release and the
group `detour`. Conditions, rollout gates and the autoscaler leave them
out, so a traceback in a detour never fires an overlay or holds a
rollout.

## The application's data

A detour uses the run's own data directory, as `muructl run` does (see
[keeping data between releases](data-directory.md)). `--app-data` gives
it the application's data directory on the daemon instead, the one the
daemon's releases share, for a database or saved files the work needs.
The detour then changes the data the serving release uses.

## From a coding agent

The MCP server's `detours` tool lists the daemon's detours and gives an
entry link to one, with its `Muru-Preview` header value, so an agent can
open a page of the detour it is working on, and `detour_traffic` changes
what a detour takes. See [coding agents](coding-agents.md) and the
[MCP tools](../reference/mcp-tools.md).
