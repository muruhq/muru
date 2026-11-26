# Three kinds of configuration

Under the daemon, configuration is of three kinds. Each says a
different thing, lives in one place and is changed in one way. This
page describes the three and how a release's settings are drawn from
them. The how-to guides on [the daemon](../howto/daemon.md) and on
[commands and files](../howto/commands-and-files.md) cover the commands,
and the [configuration reference](../reference/configuration/index.md)
lists every key.

| | What it says | Where it lives | How it changes |
| --- | --- | --- | --- |
| Project settings | What a release of this code is: its entrypoint, its workers, its static files | `[tool.muru]` in the project's `pyproject.toml`, or a `muru.toml` beside it | Edited with the code; the next release takes them |
| The daemon's configuration | How this daemon runs: the addresses it listens on, limits, logs, metrics | One file, whose path `muructl daemon config path` prints | `muructl daemon config`, then a reload |
| Deployments | What the daemon serves: applications with their settings on this node, their releases, hosts and routes | The daemon's state store | Commands such as `muructl deploy`, or files given to `muructl apply` |

`muructl run` and `muructl serve` take their settings from the project,
their flags and the environment. They have no daemon configuration file
and no deployments.

## The daemon's configuration says how it runs

The daemon reads one file of its own. It holds the listeners, the
limits, the proxy settings, the secret stores, the logs and the like:
what is true of this daemon whatever it serves. The file is read when
the daemon starts and again on a reload, and it need not exist.

It never holds an application, a host or a route. A daemon does not
start, and does not reload, with one in its file, and says to give it
to `muructl apply`.

## Deployments say what it serves

What the daemon serves is held in its state store, in deployments: sets
of applications, their releases, hosts, routes, upstreams and overlays.
Commands change it, and so do files: `muructl apply` reads files that
declare those objects and makes the store match them. The store is the
only copy the daemon uses. The daemon does not read an applied file
again by itself.

An applied file never holds a listener or a limit. An apply fails on a
table of the daemon's own configuration, and says where it goes.

A deployment has a name. Files say theirs with a `deployment` key, and
files without one belong to the deployment named `default`, as does
everything commands make. A node with one set of files, or none, names
nothing. Each object belongs to the deployment that declared it, which
alone changes or removes it, while names are shared: a route in one
deployment may send requests to an application in another.

## Three kinds of definition are in both

Client classes, rate limits and conditions may be defined in either
place. Those the daemon's file defines are the node's: every deployment
may use them, and none may define one of the same name. One that
belongs to an application, or to the upstream it watches, goes in that
deployment's files, and other deployments use it by name.

The daemon's file needs nothing a deployment defines in order to load.
A condition there may still name an application in a signal's labels,
which only picks out figures: with no such application the condition
reads `unknown`.

## Where a release's setting comes from

A release records its settings when it is made. Each setting is the
first that is given of:

1. the release's own: a flag of `muructl deploy`, or a key of the
   release's table in an applied file;
2. the application's: `muructl app set`, or the application's own table
   in an applied file;
3. the project's: `[tool.muru]`;
4. the daemon's configuration, then the default.

Environment variables and secrets are layered by name across all of
them.

So a setting goes where it belongs. One that follows the code, the same
on every node, goes in the project. One that belongs to this node, such
as a database URL, a secret or a worker count, goes on the application.
One for a single release goes on the release.

## An application is the files' or the command line's

An application that applied files declare has its releases and its
settings from those files. One that no file declares has them from
`muructl deploy` and `muructl app set`. Hosts and routes are declared in
files either way, and a route may name an application no file declares.

Where the two meet, Muru refuses or says what happens:

- `muructl deploy` on an application the files declare works, and warns
  that the files do not have the release.
- `muructl app set` on an application a named deployment owns is
  refused, since that deployment's files state its settings in full.
- An apply does not put back in traffic a release that `muructl
  rollback` took out of it, and says when it moves traffic away from a
  release a command made serving.
- `muructl release remove` leaves a release that applied files declare,
  since their next apply would create it again.

## A deployment file is written for one node

An application's own table holds what belongs to this node: its data
directory, the user it runs as, its environment and its worker counts.
Source paths, host names and certificates are as particular. A set of
deployment files therefore describes one node. Nodes that share most of
a deployment keep the shared part in one file and each node's part in
another, with the same `deployment` key, and apply the two together.
An application's own settings are declared in one file of an apply.
