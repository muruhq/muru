# The daemon

The daemon serves one or more applications on one node, each deployed
from a source directory, with rollback, drains and an event log, and can
be stopped and started again without interrupting traffic. This page
covers starting it and deploying; the other guides cover what it does
with a deployed application, from [rollouts](rollouts.md) to
[scaling](scaling.md).

`muructl run` is for development. For an application that stays up, start
the daemon once and deploy releases to it:

```
muructl daemon start
muructl deploy shop /srv/shop
```

The daemon serves HTTP on 127.0.0.1:8080 unless its configuration file
names listeners (see [virtual hosts and TLS](virtual-hosts.md)) or
`--listen` says otherwise, answering 503 until a release is ready. The
same file names the proxies to trust (see
[behind a proxy](behind-a-proxy.md)). The
first deploy creates the application and its first release; each later
deploy creates the next release, starts it beside the one that serves,
moves traffic to it once every worker is ready, and then drains and stops
the previous one. Requests in flight on the previous release finish
first. No request is refused during the switch.

A release reaches the daemon in one of two ways. `muructl deploy` makes
one now, from a source directory and the flags given. Where a record of
each release is kept in version control, `muructl render` writes the
release as a file from the same flags, and `muructl apply` makes the
daemon match the file, as [declaring applications in
files](#declaring-applications-in-files) describes.

## Releases

A release is a source directory and the settings fixed at creation: the
entrypoint, interface, interpreter, processes, threads, preload mode,
timeouts, memory policy, environment and the rest a flag can set, read
from the directory's `[tool.muru]` table unless a flag overrides them,
exactly as `muructl run` reads them. Release ids are `r1`, `r2` and so
on unless `--release` names one.

A release's settings come from its flags, then the [settings on its
application](#settings-on-the-application), then the source's
`[tool.muru]` table, then the defaults. Nothing comes from the release
before it, so a `--timeout soft=3s` or `--env DEBUG=1` given for one
deploy applies to that release alone, and after a rollback the next
deploy is what its own flags, the application and the source say. The
tables in `[tool.muru]`, worker groups, mounts, static entries, logging,
scaling and error pages, come from the source alone, so a source that
drops a group or a static entry drops it. The interpreter is `--python`,
else the source directory's `.venv`, else the source's own Python
request.

`deploy --dry-run` asks the daemon what the deploy would do and changes
nothing. It prints the id the release would take, each setting with the
layer it would come from, each environment variable and secret by name
with where it comes from (their values are not shown), whether an
environment would be built, and what becomes of the release that serves:

```console
$ muructl deploy shop /srv/shop --processes 4 --dry-run
muru: plan (nothing changed): release shop@r4, shop.wsgi:application 4x8 from /srv/shop
  entrypoint = shop.wsgi:application    ([tool.muru])
  python = /srv/shop/.venv/bin/python   (the source's .venv)
  processes = 4                         (deploy flags)
  threads = 8                           ([tool.muru])
  sync_threads = 8                      (default)
  max_requests = 5000                   (the application)
  ...
  env DEBUG (--env)
  env SHOP_DATABASE (the application)
  env SENTRY_ENVIRONMENT ([tool.muru.env])
  secret STRIPE_KEY = vault/stripe (the application)
  env HTTP_PROXY (passed from the daemon)
  env PATH (passed from the daemon)
  the release would take over from shop@r3, which would stay warm for 1800s with 1 worker to a group
```

When the deploy would be refused, the plan lists why instead: a release
id already taken, an overlay that freezes the application's deploys, a
setting that fails its check, or a release the node's memory budget has
no room for. An interpreter the node does not have is reported and not
installed. The exit code is 0 when the deploy would go ahead and 1 when
it would be refused, and `--json` prints the plan as JSON.

```
muructl release list shop
muructl release show shop@r3
muructl release wait shop@r3
muructl rollback shop
muructl drain shop@r2
muructl restart shop --worker 1
muructl events --follow
```

The `WORKERS` column of `muructl release list` gives the workers running
now and, in brackets, the shape the release asks for: `2 (2x4)` is two
workers of a release of two processes with four threads each, and
`0 (1x4)` a release scaled to zero. `-` in place of the count means no
host runs for the release, because it is stopped, failed or idle. A
managed server shows its instances the same way, as `1 (1 gunicorn)`.

A release that a deploy replaces is drained and then kept warm for a
retention window of 30 minutes (`deploy --retain` changes it; `--retain
0` stops it as soon as it has drained), with one worker in each of its
groups, or none in a group whose `min_workers` is 0 (`deploy
--retain-workers` sets the count for every group; `--retain-workers 0`
keeps the host alone), so `rollback` within the window is instant:
traffic moves back to the warm workers at once, and the groups grow back
to their counts by forking from the host. `muructl release list` notes
what a rollback to a retained release costs: `a rollback to it is
instant` while it keeps workers, `a rollback to it forks a worker` when
it keeps its host alone, and `a rollback to it waits for its import` when
its host had already stopped while idle. After the window, or when another
deploy supersedes it again, the retained release is stopped. One release
per application is retained at a time as the rollback target. A rollout's
candidate that was rolled back or aborted is kept beside it for 10
minutes, for its logs and workers to be looked at, and does not take its
place. `rollback` to a release that was
stopped starts it again first and moves traffic when it is ready; the
release it replaces is retained in turn. `drain` takes a release out of
traffic and stops it, answering once the release has stopped; draining
the serving release leaves the application answering 503 until the next
deploy. Releases start and stop beside the daemon's other work, so
`status`, deploys of other applications and the daemon's own checks
answer while one starts, runs its gates or finishes its requests. A
drain of a release that is still starting cancels its start, and a
scale of it is refused until it is ready. `deploy --no-wait` returns once the
release is recorded, and `release wait` waits for a state. `restart`
replaces one worker of a release, or each in turn, as the [recycling
guide](recycling.md#restarting-a-worker-by-hand) describes.

`release show` prints one release: its state, source, interpreter,
entrypoint and sizes, and its worker groups and mounts:

```
release      shop@r3
state        ready, serving
source       /srv/shop
python       /srv/shop/.venv/bin/python
entrypoint   shop.wsgi:application, WSGI (detected)
workers      2 processes of 8 threads and an event loop of concurrency 1000 with 8 sync threads
group reports /reports/*: 6 processes of 1 thread and an event loop of concurrency 1000 with 8 sync threads
mount ws     /ws from shop.asgi:websocket_app, ASGI (detected)
```

While WebSockets are open on a release's workers, a `websockets` line
says how many; [watching WebSockets](asgi.md#watching-websockets) shows
how to list them.

An interface declared `auto` is shown as the host detected it when it
imported the application. When the workers import it themselves, with
preload off or a factory called in each worker, it is `interface auto,
not yet detected`, and the release's log (`muructl logs shop`) has each
worker's line saying what it serves. The sizes shown are the ones the
interfaces use: threads for WSGI, the event loop for ASGI and gRPC.

A release whose host exits on its own is marked failed; if it was serving,
the daemon starts it again. A deploy whose release fails to start leaves
the previous release serving and reports the failure.

### Where a release came from

A release carries metadata, a small map of keys to values saying where
its code came from. When the source directory is in a git checkout,
`muructl deploy` records the commit (`commit`), the branch, or with a
detached HEAD the one tag on it (`ref`), and `dirty = true` when the
checkout has changes, untracked files included. `--metadata KEY=VALUE`,
given as often as needed, records more and overrides what git gave:

```console
$ muructl deploy shop /srv/shop \
    --metadata url=https://git.example.com/shop/pull/42 \
    --metadata "description=Fix the basket total"
muru: release shop@r7 is ready and serving, commit 4f2a9c1e0b3d of main
```

`commit`, `ref`, `dirty`, `url` and `description` are shown by name; any
other key is kept and shown as given, so a deploy tool can record its
own, such as `hosting.example/pr=42`. `--metadata KEY=` leaves out a key
git filled, a `commit` given by flag leaves out the filled `dirty`, and
`--no-git-metadata` fills nothing from git. A source directory in no
checkout, or a machine without git, records nothing from git; a git
command that fails or takes longer than 5 seconds records nothing either,
says why, and the deploy goes ahead.

A release keeps at most 32 keys. A key is up to 63 lowercase letters,
digits, `.`, `_`, `-` or `/`, starting with a letter or digit; a value is
up to 1024 bytes with no control characters other than newlines and
tabs, and `url` must be an `http` or `https` URL; all of it together is
at most 8 KiB. A key with an empty value is not kept. The application is
never given the metadata, in its environment or anywhere else.

The metadata stays with the release, as its settings do. `release list`
shows the commit in its `COMMIT` column, shortened, with `+` when the
checkout had changes and `-` for none:

```console
$ muructl release list shop
RELEASE                  STATE      SERVING  WORKERS          COMMIT             DISK SOURCE
shop@r7                  ready      yes      2 (2x8)          4f2a9c1e0b3d    212.4MB /srv/shop
shop@r6                  retained            1 (2x8)          9b01c33d7a2e+   212.4MB /srv/shop
```

`release show` gives every key, the well-known ones first, the
`release.created` event names the commit and ref
(`from /srv/shop, commit 4f2a9c1e0b3d of main`), the dashboard's
releases table shows the commit, linked to `url`, with the ref and the
description, and `--json` gives the map as `metadata`. A release
declared in a file takes it as a `metadata` table, and `deployment export`
writes it there:

```toml
[app.shop.release.r7]
source = '/srv/shop'
metadata = { commit = '4f2a9c1e0b3d5a6b7c8d9e0f1a2b3c4d5e6f7a8b', ref = 'refs/heads/main' }
```

`muructl apply` records what the file says and fills nothing from git. A
file whose release has metadata other than what the daemon recorded for
it is refused, as one with other settings is: declare a new release id
instead.

### Settings on the application

A release's flags apply to that release. A setting meant to last
through every deploy goes on the application, when it belongs to the
deployment, such as a database URL, a secret or this node's worker
count, or in `[tool.muru]` when it follows the code:

```console
$ muructl app set shop --env SHOP_DATABASE=postgres://db/shop --secret STRIPE_KEY=vault:stripe --processes 4
muru: shop's settings are changed for the releases deployed from now; no running release changes
processes = 4
env.SHOP_DATABASE = postgres://db/shop
env.STRIPE_KEY = secret vault:stripe
```

Every release deployed after that takes them, over the source's
`[tool.muru]` and under the deploy's own flags, so `--processes 2` on one
deploy gives that release 2 and leaves the application's 4 for the next.
Variables and secrets layer by name: a release gets every name from the
application, the source and its flags, and the flag wins for a name
given in more than one. A secret is kept as its reference, never its
value. No release that is running changes; the settings take effect at
the next deploy. Setting them on an application that does not exist yet
creates it, so they can be in place before its first release.

`muructl app set` takes the deploy flags for the variables and secrets,
`--processes`, `--threads`, `--sync-threads`, `--concurrency`,
`--max-requests`, `--max-lifetime`, `--timeout NAME=DURATION` and
`--memory KEY=VALUE`, and `--replace` removes the settings not given.
The entrypoint, interface, interpreter and the other settings that say
what the code is stay with the source and the deploy, as do the tables.
`muructl app unset` removes settings by key, and `muructl app show`
lists them:

```
muructl app unset shop env.SHOP_DATABASE processes
muructl app unset shop timeouts.hard_timeout memory.soft_limit
muructl app unset shop --all
muructl app show shop
```

The same settings go on an application's table in a file given to
`muructl apply` (see [declaring applications in
files](#declaring-applications-in-files)), and `muructl deployment export`
writes them there.

### What a release's environment holds

A release's host, workers, commands and managed servers start from a
clean environment, whoever started the daemon and from wherever:

1. What Muru sets: the `MURU_` variables, the interpreter's directory first
   on `PATH`, `VIRTUAL_ENV`, `PYTHONPATH` with the runtime library first,
   and the bytecode settings.

2. A small base taken from the daemon's own environment when set there:
   `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TZ` and `TMPDIR`.
   A release run as another user has that user's `HOME`, `USER` and
   `LOGNAME` from the system's password database instead, and
   `muructl deploy --dry-run` lists them as set by Muru (see
   [per-application users](users.md)).

3. Variables `pass_env` in the `[daemon]` table of the daemon's file
   passes through by name, with a `*` suffix allowed:

   ```toml
   [daemon]
   pass_env = ["HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "SSL_CERT_FILE", "AWS_*"]
   ```

4. What the release declares, in `[tool.muru.env]`, on its application,
   with `--env` and as secrets, which overrides the base and `pass_env`
   but not what Muru sets.

Anything else the daemon's own environment holds does not reach a
release. A release that needs another variable declares it, or the
daemon's file passes it on. `muructl daemon reload` applies a changed
`pass_env` to releases started from then on. `muructl debug inspect`
shows the values of what Muru sets, the base, what is passed through and
what the release declares.

`muructl run` keeps the environment of the shell that started it, and
`muructl serve` keeps the container's.

## Declaring applications in files

A file declares applications and their releases with the keys `muructl
deploy` takes, and virtual hosts and routes (see
[virtual hosts and TLS](virtual-hosts.md)), and `muructl apply -f FILE`
makes the daemon match it:

```toml
[app.shop.release.r1]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"
python = "/srv/shop/.venv/bin/python"
processes = 2
threads = 8
```

```
muructl apply -f shop.toml
muructl apply -f config/ --dry-run
muructl apply -f config/ --prune
muructl deployment export > config/shop.toml
muructl deployment diff -f config/
```

Each `[app.NAME.release.ID]` table needs a `source`; a relative path is
taken from the file's own directory, as every other path in the file is:
a route's or an overlay's `static` directory, an upstream's
`sendfile_roots`, a host's certificate and key, a host's and a route's
error page files, an upstream's `tls.ca` and a client class's lists. A
release's error page files and its `sendfile_roots`, its mounts' among
them, are the exceptions, taken from its `source`. The other keys are
those of the
`[tool.muru]` table (`entrypoint`, `interface`, `factory`,
`factory_phase`, `python`, `processes`, `threads`, `sync_threads`,
`concurrency`, `preload`, `hooks` and `preload_modules`), and what a
table leaves out comes from the application's settings, then the
source's `[tool.muru]` table and then the defaults, exactly as a deploy
resolves them. A directory applies every `.toml` file in it, in name
order, as one configuration, and `-f` may be given more than once, the
files and directories named applied as one. A project's own directory
is not such a directory: its `pyproject.toml` holds the project's
settings, so name the file in it, as in `muructl apply -f
muru.deploy.toml`. Releases of one application add up across
the files, and so do routes; a release, a host or an upstream declared
in two files is refused, and so is an application whose own settings
two files set: the keys of its `[app.NAME]` table go in one file.

The application's own table holds the [settings on the
application](#settings-on-the-application) that `muructl app set` sets,
with the same keys: `env`, `processes`, `threads`, `sync_threads`,
`concurrency`, `max_requests`, `max_lifetime`, a `timeouts` table of the
host's timeouts and a `memory` table. The file states them in full, as
it states the application's user, so applying a file whose application
has none removes those `muructl app set` gave, and `muructl deployment
export` writes them, so settings made at a terminal can be kept in a
file:

```toml
[app.shop]
processes = 4

[app.shop.env]
SHOP_DATABASE = "postgres://db/shop"
STRIPE_KEY = { secret = { store = "vault", key = "stripe" } }

[app.shop.release.r1]
source = "/srv/shop"
```

So a release's table needs only its `source` when the project's own
file says the rest:

```toml
# muru.deploy.toml, beside the project's pyproject.toml
[app.shop.release.r1]
source = "."
```

A file kept beside a project's code like this is named
`muru.deploy.toml`. `muru.toml` there is the project's settings file,
which has other keys, and a file in this format saved under that name is
refused as one. Anywhere else, the file's name is yours to choose.

### Writing a release's table

`muructl render` writes the release a command line describes as a file
of this form. It takes an application, a source directory and the
settings `muructl deploy` takes for a release, and prints the table:

```text
$ muructl render shop /srv/shop --release 2026-11-06 --processes 4
# The release this command line describes, as a table for muructl apply.
# Save it as muru.deploy.toml and give it to the daemon with:
#   muructl apply -f muru.deploy.toml
# Settings come from the flags, then the source's own settings, then the
# defaults. The source's settings are [tool.muru] in /srv/shop/pyproject.toml.
# A setting left at its default is not written; --full writes those too.

[app.shop.release.2026-11-06]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"
processes = 4
threads = 8

[app.shop.release.2026-11-06.env]
DJANGO_SETTINGS_MODULE = "shop.settings.production"
```

```
muructl render shop /srv/shop --release 2026-11-06 > muru.deploy.toml
muructl apply -f muru.deploy.toml
```

The command asks nothing of a daemon and changes nothing, so it runs
where no daemon does, such as a job that writes the file for a later
step to commit. It reads the source's settings, so the source is a
directory it can read.

- **The release id** is the one `--release` names, which is required:
  an id of your choosing, such as a date or a commit.
- **The source** is written as the command line gives it. A relative
  path stays relative, and `muructl apply` takes it from the file's
  directory, so `muructl render shop . --release 2026-11-06 >
  muru.deploy.toml` beside the code applies from there. With no source the current directory is
  written as its full path.
- **The settings** come from the flags, then the source's settings,
  then the defaults. Nothing comes from a release or the settings the
  application already has, so applying the file gives the release
  `muructl deploy` gives for the same flags to an application with no
  settings of its own.
- **The interpreter** is written when `--python` or the source's
  settings name one. Otherwise the table has no `python` key, and the
  daemon finds the interpreter when the file is applied, as it does for
  a deploy. A release that builds its environment from `uv.lock` is
  built by the daemon when the file is applied.
- **Secrets** are written as the references `--secret` gives. No secret
  store is read.
- **Where the code came from** is written as a deploy records it: a
  `metadata` table with the commit and ref of the git checkout the
  source is in, and what `--metadata KEY=VALUE` gives over them.
  `--no-git-metadata` fills nothing from git.

A command line that a deploy would refuse is refused with the same
message, and nothing is printed. `--json` writes the document as JSON.

`--full` writes every setting, the defaults included, with a comment
beside each saying where it came from: the command line, the source's
settings or the default.

```text
$ muructl render shop /srv/shop --release 2026-11-06 --processes 4 --full
...
[app.shop.release.2026-11-06]
source = "/srv/shop"
entrypoint = "shop.wsgi:application"    # [tool.muru]
interface = "auto"                      # default
bytecode = "redirect"                   # default
processes = 4                           # command line
threads = 8                             # [tool.muru]
sync_threads = 8                        # default
concurrency = 1000                      # default
preload = "auto"                        # default
...

# Not set, and off unless set: timeouts.soft_timeout, timeouts.hard_timeout, memory.soft_limit, memory.hard_limit, max_requests, max_lifetime.
# python is not set: the daemon finds the interpreter when the file is applied,
# as a deploy does.
```

A file written this way keeps its meaning when a later version of Muru
changes a default.

### What an apply does

An apply creates the applications the files name, deploys each declared
release the daemon does not have, creates or updates the hosts and
routes, and leaves anything already recorded as declared alone. A
release the apply adds serves when it is the only one it adds for its
application, or the one the application's [`serving`
key](#which-release-serves) names; the others are held when a route
names them, else left stopped. An apply that adds more than one release
of an application needs the key, and is refused without it, naming the
releases: the order of their ids decides nothing. Releases are
immutable: a declared
release whose settings differ from the recorded ones is an error, and
the file is applied only when it has no errors at all. Unknown keys,
invalid names, a missing source directory and a source without an
entrypoint are errors too. So is a table of the daemon's own
configuration, such as `[[listener]]` or `[limits]`: an apply changes
what the daemon serves and never the daemon's own settings, which are
in its [configuration file](#reloading-the-configuration). In turn the
daemon's file holds no application, host, route, upstream or overlay,
and a daemon does not start or reload with one in it. A client class,
rate limit or condition may be in either: one the daemon's file defines
is the node's, and an applied file that defines the same name is
refused.

The apply happens in two steps. First it creates the applications and
starts the releases it adds, held, with traffic left where it is and
nothing yet pointing at them. Then it writes the hosts, routes, overlays
and the rest of the files together, with which release serves each
application, and the edge gets all of it in one snapshot. A release that
does not start stops the apply before the second step: the releases and
applications it added are removed, and the report says nothing was
applied, naming the release and why:

```console
$ muructl apply -f config/
muru: nothing applied:
  shop@r3 did not start: the host exited with exit status: 1 before its workers were ready: cannot load app:application: KeyError: 'DATABASE_URL'
  deploy          shop@r3                  failed: the host exited with exit status: 1 before its workers were ready: cannot load app:application: KeyError: 'DATABASE_URL'
  create-host     host shop                not applied
  create-route    route shop /             not applied
```

Once planned, an apply is carried out by the daemon. Interrupting
`muructl apply`, with Ctrl-C or by closing its terminal, stops the
report, not the apply: the daemon finishes it, or removes what it added
when a release does not start, and the `config.applied` or
`config.apply_failed` event says how it ended.

Applies happen one at a time. An apply given while another is under way,
including one given `--no-wait` that the daemon is still finishing,
waits for it to end and is then planned against what it left, and its
report says so. A dry run does not wait.

Four flags say how the releases an apply adds are deployed, as they do
on `muructl deploy`:

| Flag | What it does |
| --- | --- |
| `--hold` | Starts the releases and leaves traffic where it is |
| `--no-wait` | Returns once the releases are recorded; the rest follows once they start |
| `--retain DURATION` | How long a release the apply replaces stays warm |
| `--retain-workers N` | The workers each group of that release keeps meanwhile |

```
muructl apply -f muru.deploy.toml --hold
muructl rollout bluegreen shop@2026-11-06
```

A held release takes no traffic and is reached with the `Muru-Preview`
header, until a [rollout](rollouts.md) or a promotion moves traffic to
it. `--hold` holds every release the apply adds, in every application,
and the plan and the report say `held` of each:

```console
$ muructl apply -f config/ --hold --dry-run
muru: plan (nothing changed)
  deploy          api@r8                   held: api.asgi:application 2x8 from /srv/api
  deploy          shop@2026-11-06          held: shop.wsgi:application 4x8 from /srv/shop
```

An application whose first release is held serves nothing until a
rollout. To hold the releases of one application and not another's,
give it a `serving` key naming the release that serves now, which keeps
the others out of traffic without the flag: held where a route names
them, else stopped.

`--dry-run` prints the plan and changes nothing. For each release that
would take over from one that serves, it says how long that one stays
warm. `--prune` also removes the routes, hosts, releases and
applications the files do not contain, of the
[deployments](#deployments) they name: the routes and hosts with the
rest of the files, and the releases and applications after the edge has
the snapshot, a running release draining and stopping first. A release
the files do not declare is left for a later `--prune` while it is the
one a rollback restores, the plan saying so, as `muructl release prune`
leaves it. The release
that serves each application after the apply must be declared, or the
apply is refused, since `--prune` would remove it, and so must every
release a route the files keep names in its traffic, and every
application such a route sends requests to. `--json` writes the report
as JSON. The exit code is 0 when there is nothing to do or everything
applied, 2 for a dry run with changes planned, and 1 for errors, for a
release that did not start, for a change that failed, and for a
snapshot the edge did not take, which the report names: the changes
were made, and the edge serves what it had until a later snapshot
reaches it.

`muructl deployment export` writes desired state as a file of this form, with
a header naming the daemon's version, the node, the time and the state
version. It writes the release that serves each application, every key
spelled out, and the application's `serving` key naming it, and the
releases its routes' splits and client classes name, so applying the
file back to the daemon it came from changes nothing and, applied to
another node, it starts those releases held. A release neither is left
out. `--app NAME` writes one application with the routes that reach it,
a static route whose `fallback` is the application among them, and
their hosts, and `--json` the same document as JSON. A [rollout](rollouts.md) under way is not part of the file;
with `--resolve-rollouts` it is written as where it stands, the
candidate declared beside the serving release and the application's
routes splitting traffic between them at the candidate's current share.
`muructl deployment diff -f FILE` compares a file with desired state in
both directions, listing what the file would add and what the daemon
holds that the file does not, with exit code 2 when they differ.

`muructl deployment show [APP] --effective` writes the same document headed
by each serving release's settings and where each came from: the
release table of an applied file or the deploy's flags, the
application's settings, the source's `[tool.muru]` table, the source's
`.venv` for the interpreter, or the default:

```console
$ muructl deployment show shop --effective
# Muru effective state: muructl 0.1.0 on web1, 2026-11-02T02:10:00Z, state version 812.
# No active overlay changes this configuration.

# The releases' settings, and the layer each came from:
#   shop@r3: entrypoint = shop.wsgi:application ([tool.muru])
#   shop@r3: python = /srv/shop/.venv/bin/python (the source's .venv)
#   shop@r3: processes = 4 (deploy flags)
#   shop@r3: threads = 8 (the application)
#   shop@r3: sync_threads = 8 (default)
...
```

### Which release serves

An application's `serving` key names the release that serves its
traffic. Declaring a new release beside it stages that release:

```toml
[app.shop]
serving = "r1"

[app.shop.release.r1]
source = "/srv/shop/r1"

[app.shop.release.r2]
source = "/srv/shop/r2"
```

Applied while r1 serves, this records r2 and leaves it stopped, since
nothing serves it or names it. A route whose traffic split or client
classes in the same files name r2, even at 0%, starts it held instead:
it does not take the application's traffic, and serves only that share
and requests that name it with the `Muru-Preview` header. So does an
[overlay](overlays.md) in the same files that pins r2, splits to it or
waits for it with `release_ready`, so that the overlay can activate. A
stopped release is started by a rollout, a promotion or a split that
names it later; a preview alone does not start it. `--hold` starts
every release the apply adds held.

Changing the key to `serving = "r2"` and applying the files again
promotes r2, starting it if it is stopped: all of the application's
traffic moves to it, and r1 drains and stays warm as `--retain` and
`--retain-workers` say. The promotion is written with the hosts and
routes, and reaches the edge in the same snapshot, so a split in the
same files that still names r1 keeps it running. The plan says what
happens to each release, `serving`, `held` or `stopped` for one it adds:

```console
$ muructl apply -f config/ --dry-run
muru: plan (nothing changed)
  promote         shop@r2                  serving in place of shop@r1
  2 objects as declared
muru: shop@r1 stays warm for 1800s with 1 worker to a group, when shop@r2 takes its place
```

The key names a declared release or one the daemon already has; naming
neither is an error. Without the key, a release serves when it is the
only one the apply adds for the application, and an apply that adds
several is refused until the key says which serves, or `--hold` holds
them all. `--hold` with a key that would move traffic is an error,
since the flag leaves traffic where it is.

While a [rollout](rollouts.md) is under way, the rollout moves the
traffic. The key may name its baseline or its candidate, and the apply
promotes nothing. Naming the baseline also warns that once the rollout
promotes the candidate, applying the same files again promotes the
baseline back: change the key to the candidate before the rollout ends
to keep its outcome. Naming any other release is an error until the
rollout ends, or `muructl rollout promote` or `muructl rollout abort`
ends it. Once a rollout has rolled back, by its gates or by an abort,
naming its candidate is an error too, since the apply would promote it
past its gates: name another release, or promote it by hand with
`muructl rollout bluegreen`, after which the files and the daemon
agree.

The same holds after `muructl rollback`. The files still name the
release the rollback took out of traffic, and an apply does not put it
back on its own:

```console
$ muructl rollback shop
$ muructl apply -f config/
muru: nothing applied; the configuration has errors:
  shop: serving names r2, which muructl rollback took out of traffic at 2026-11-06T14:02:11.000Z; name another release with serving, or promote it again with muructl rollout bluegreen shop@r2
```

Any other move of traffic away from a release a command made serving
is allowed, and the plan says that it is one:

```console
$ muructl apply -f config/ --dry-run
muru: plan (nothing changed)
  promote         shop@r1                  serving in place of shop@r2
warning: shop@r2 was made serving by a command, not by applied files; this apply moves the application's traffic to shop@r1
```

### Deployments

A node that serves unrelated things can keep each as a deployment of
its own: a named set of applications, hosts, routes and the rest that
is applied, pruned and removed together. A `deployment` key at the top
of a file names the one its tables belong to:

```toml
deployment = "shop"

[app.shop.release.r1]
source = "/srv/shop"

[host.shop]
names = ["shop.example.com"]

[[route]]
host = "shop"
app = "shop"
```

Files with the same name are one deployment, and one apply may carry
several: the files given are read a deployment at a time, and applied
together. Files with no key belong to the deployment named `default`,
and so does what commands make, so a node with one set of files, or
none, names nothing.

```console
$ muructl deployment list
NAME                     APPS                             HOSTS ROUTES  OTHER
api                      api                                  0      1      0
default                  landing                              0      0      0
shop                     shop                                 1      1      0
```

- **Names are shared across the node.** A route in one deployment may
  send requests to an application in another, and may be on a host
  another declares.
- **Only its owner changes an object.** An apply that declares an
  object another named deployment owns is refused, naming that
  deployment.
- **A named deployment takes an object from `default` by declaring
  it**, which is how an application made by command moves to files.
  The apply says `app landing moves from the deployment default to
  site`.
- **`--prune` reaches only the deployments the files name.** It removes
  what they own and the files no longer declare, and leaves every other
  deployment alone. `--prune=all` reaches every deployment, `default`
  among them, so the files are the whole of what the daemon serves: a
  deployment they do not mention is removed, and so is an application
  made by command.
- **Nothing is removed while another deployment names it.** A prune
  that would remove a host, an application, an upstream, a client class
  or a rate limit that a route of another deployment names is refused,
  naming the route. Take that route out first, or apply both
  deployments together.

`muructl deployment remove NAME` removes a deployment as a unit. It
lists what goes, asks, drains what the deployment runs, and removes its
applications with their releases, its hosts, routes and the rest:

```console
$ muructl deployment remove shop
muru: plan (nothing changed)
  remove-route    route shop /             not declared
  remove-host     host shop                not declared
  remove-release  shop@r1                  ready and not declared
  remove-app      shop                     not declared
Remove the deployment shop? [y/N]
```

`--dry-run` only lists, and `--yes` goes ahead without asking. It is
refused as a prune is while another deployment names something of it.
`muructl deployment remove default` removes what `default` holds and
leaves it in place, empty. Application data directories are kept.

`muructl deployment export` follows the same division. A file carries
one `deployment` key, so a daemon with named deployments is exported
one at a time, or into a directory:

```console
$ muructl deployment export shop > shop.toml
$ muructl deployment export --dir backup/
backup/api.toml
backup/default.toml
backup/shop.toml
$ muructl apply -f backup/ --dry-run
muru: no changes (7 objects as declared)
```

`export NAME` writes what that deployment owns, with its key at the
top; `default` is written without one. `--dir DIR` writes a file for
each deployment into an empty directory, which applies back without
changes. With no name and no directory, a daemon whose files name no
deployment writes everything, and one with named deployments says to
name one. `--app APP` writes one application with the routes that reach
it and their hosts, and `--as NAME` puts `deployment = "NAME"` at the
top, for moving an application made by command into files:

```console
$ muructl deployment export --app landing --as site > site.toml
$ muructl apply -f site.toml
muru: app landing moves from the deployment default to site
```

Client classes, rate limits and conditions the daemon's own file
defines are not written: they are the node's, and stay in that file.

What you would remove together belongs together. A host that
separately released applications share goes in a deployment of its
own, and each application's deployment holds its routes on that host.

### Files and commands on one application

An application that applied files declare gets its releases and its
settings from the files, and one that no file declares gets them from
`muructl deploy` and `muructl app set`. Hosts and routes are declared
in files either way, and a route may name an application no file
declares.

`muructl app set` and `muructl app unset` are refused on an application
a named deployment owns, since that deployment's files state its
settings: change `[app.NAME]` there and apply, or give the value to one
release with `muructl deploy`. In `default` they work, and say when
applied files wrote the settings last, since their next apply removes
what they do not carry.

`muructl deploy` on an application the files declare is for an
emergency. It works, and says that the files do not have the release:

```console
$ muructl deploy shop /srv/shop/hotfix
muru: release shop@r5 is ready and serving
muru: warning: shop is declared in applied files, which do not have shop@r5: the next apply moves traffic back when the files name the release that serves, and an apply with --prune removes r5 once it neither serves nor is the release a rollback restores. Add it to the files to keep it; muructl deployment export writes it
```

A setting `muructl app set` gives such an application lasts until the
next apply, since a file states the application's table in full, and
the plan names each setting an apply removes:

```console
$ muructl apply -f config/ --dry-run
muru: plan (nothing changed)
  set-settings    shop                     processes; removes env.STRIPE_KEY
```

### Applying files when the daemon starts

`--apply` on `muructl daemon start`, `muructl daemon run` and `muructl
up` applies files once the daemon is up, as `muructl apply -f` does, so
one command brings up a daemon that serves:

```console
$ muructl daemon start --apply deploy.d/
muru: daemon started (pid 4301), serving on http://127.0.0.1:8080, log /home/me/.local/state/muru/log/daemon.log
muru: start apply of /home/me/deploy.d: 2 changes applied
```

It takes a file or a directory and may be given more than once.
`--prune` beside it also removes what the files do not contain, as on
`muructl apply`, and `--prune=all` makes the files the whole of what
the daemon serves at each start.

- **Ready means applied.** The command returns, and the daemon answers
  its [probes](#probes) and tells a service manager that it is ready,
  once the files are applied.
- **Every start applies them**, and so does a start that finds the
  daemon already running, which then says so and applies the files to
  it. The same line can be run every time.
- **With nothing running**, as after a reboot, the daemon first starts
  what it had, then applies the files. Its events say what the apply
  will change before either happens.
- **A start whose files cannot be applied fails.** Nothing is changed,
  the daemon exits with the reason, and whatever was serving is left
  serving for the next daemon to take over, as after `muructl daemon
  stop`. With nothing serving, as on a first start, nothing is left
  running: the daemon exits before it listens when the files cannot be
  read, and stops its edge when a release they declare does not start.
  A daemon that was already running stays running.
- **An upgrade does not apply again.** `muructl daemon upgrade` starts
  the daemon with its own arguments and leaves the files alone.

```console
$ muructl daemon start --apply deploy.d/
muru: error: start apply of /home/me/deploy.d failed: /home/me/deploy.d/shop.toml: TOML parse error at line 19, column 9: invalid type: integer `7`, expected a sequence; the daemon exits, and what was serving is left serving
```

A restart applies whatever the files say at that moment, an edit
nobody has applied yet among it, and puts back what a command changed
where the files state otherwise. On a machine that is changed by hand,
leave `--apply` off the service's command line and run `muructl apply`
yourself. To keep a daemon running on what it had when files are bad,
do the same: start it without `--apply`, and apply as a step of its
own.

## Building the environment on the node

A source with a `uv.lock` and no `.venv` gets an environment of its own,
built by the daemon before the release starts: `uv sync --frozen` without
the dev group, without building sdists, with bytecode compiled and the
project's extras and groups, into the state directory under
`envs/APP/RELEASE`, as the release's user. The release is in the
`building` state meanwhile, uv's lines go to the release log tagged
`[build]`, each with the full UTC time in front as every line of the
file has, and a failed build fails the release with uv's last line. A
source with its own `.venv` is used as it is, as before.

```
muructl deploy shop /srv/shop                        # builds when the source has a lock and no .venv
muructl deploy shop /srv/shop --build always          # builds although the source has a .venv
muructl deploy shop /srv/shop --build never           # runs on what is there
muructl deploy shop /srv/shop --build-extra postgres
```

The same keys live in the project's `[tool.muru.build]` table and in a
declared release's `build` table: `when`, `extras`, `groups`,
`source_builds` and `compile_bytecode`. `muructl release show` names the
environment a release runs on, and it is removed with the release. A
Python the request names and the node lacks is downloaded into the state
directory first, unless the `[tools]` table says `python_downloads =
"never"` or `offline = true`.

## Bytecode caches

Python writes compiled bytecode into `__pycache__` beside the source,
which an immutable release directory should not take and a release
directory owned by another user cannot. Under the daemon, bytecode goes to
a directory of the release's own under the state directory, `pycache/APP/RELEASE`,
writable by the release's user and removed with the release, so cold
starts stay fast and the source tree stays clean. A source that already
holds bytecode for the release's interpreter, compiled by `muructl build
--deployment` or `python -m compileall`, is used as it is. `bytecode` in
the project's `[tool.muru]` table, in a declared release or as
`--bytecode` on `muructl deploy` takes `redirect` (the default), `source`
or `off`, and `muructl release show` says which is in force.
A release whose own environment sets `PYTHONDONTWRITEBYTECODE` writes no
bytecode.

## Where things live

The daemon keeps its state store, run directory and logs in the user
locations for the platform, or the system locations with `--system`:

| What | Linux, user | macOS, user | System |
| --- | --- | --- | --- |
| State store | `~/.local/state/muru` | `~/Library/Application Support/muru` | `/var/lib/muru` |
| Admin socket and run directories | `$XDG_RUNTIME_DIR/muru` | `/tmp/muru-UID` | `/run/muru` |
| Logs | `~/.local/state/muru/logs` | `~/Library/Logs/muru` | `/var/log/muru` |
| Configuration file | `~/.config/muru/muru.toml` | `~/Library/Application Support/muru/muru.toml` | `/etc/muru/muru.toml` |

`--state-dir`, `--run-dir`, `--log-dir`, `--cache-dir` and `--config`
override each, as do `MURU_STATE_DIR`, `MURU_RUN_DIR`, `MURU_LOG_DIR`,
`MURU_CACHE_DIR` and `MURU_DAEMON_CONFIG`, and the directories systemd
provides through `StateDirectory=` and its siblings are used when set.
`muructl daemon run` prints every location and which rule chose it.

The daemon's own commands take all five flags: the `muructl daemon`
commands, `muructl up` and `muructl mcp serve --up`, so one set of
flags starts, stops and reloads a daemon. So does `muructl secret`,
which reads the daemon's file. Every other command only talks to a
running daemon, and takes `--socket`, `--run-dir` and `--system` to find
it. The run
directory must be an absolute path, and a Unix socket path is limited to
104 bytes on macOS and 108 on Linux, so a run directory too deep to leave
room for the workers' sockets, which sit about 35 bytes plus the
application's name below it, is refused at start with a message saying
so.

The configuration file is optional. Its `[[listener]]` entries name the
addresses to serve, each with `tls = true` when the edge terminates TLS
on it, and its `[limits]` table sets the body size limit and timeouts.

## Running it

`muructl daemon start` starts the daemon in the background, logging to
`daemon.log` in the log directory, and waits until it answers. A daemon
that exits before it answers, such as one that cannot listen on its
address, is reported at once with its exit status and the lines it
wrote to `daemon.log`, the last twenty of them:

```console
$ muructl daemon start
muru: error: the daemon exited before it answered (exit status: 1); from /var/log/muru/daemon.log:
  2026-11-02T05:40:11.204Z muru: config: /etc/muru/muru.toml (default)
  2026-11-02T05:40:11.204Z muru: state: /var/lib/muru (default)
  2026-11-02T05:40:11.204Z muru: run: /run/muru (default)
  2026-11-02T05:40:11.204Z muru: logs: /var/log/muru (default)
  2026-11-02T05:40:11.204Z muru: cache: /var/cache/muru (default)
  2026-11-02T05:40:11.209Z muru: error: cannot listen on 0.0.0.0:80: port 80 on 0.0.0.0 is in use by nginx (pid 812); choose another listener, or stop it
```

A listener whose address another program holds names that program and
its pid, found with lsof or ss when either is installed, or the instance
of Muru's that holds it, as a reload that adds a listener does too.
Without either, the error is the operating system's own.

A daemon owns its state directory and its run directory for as long as
it runs, through a lock on `daemon.lock` in each. A second daemon given
either of them, by the same flags or by different ones that share a
directory, is refused before it touches anything, naming the daemon that
has it:

```console
$ muructl daemon run --state-dir /srv/muru/state
muru: error: another daemon (pid 4242) is using the run directory /run/muru; its admin socket is /run/muru/admin.sock
```

`muructl daemon start` reports the same refusal. The locks go with the
process however it ends, so a daemon that was killed leaves nothing in
the way of the next.

The daemon writes its pid to `daemon.pid` in the runtime directory and
removes the file when it exits. `muructl up` does the same and is harmless when one already runs. `muructl daemon
status` and `muructl status` show the daemon, its edge process, its
listener, its admin socket, its configuration file and its state, run,
log and cache directories:

```console
$ muructl daemon status
daemon   pid 4242 version 1.0.0
edge     pid 4250
serving  http://0.0.0.0:80
socket   /run/muru/admin.sock
config   /etc/muru/muru.toml
state    /var/lib/muru
run      /run/muru
logs     /var/log/muru
cache    /var/cache/muru
apps     2 (3 releases)
websockets 0 open
```

With no daemon running, `muructl daemon status` says so, names where a
daemon started with the same location options and environment would
keep the same files, and exits with status 3. That is where to look
for `daemon.log` when a daemon did not start, and `muructl logs
--daemon` reads it. `muructl status` with nothing running says that
nothing is listening.

### The processes

`muructl status --processes` shows every process of the instance as the
daemon supervises it, with what each is for, the user it runs as, how
long it has been up, its CPU and resident memory, and each worker's
figures over the last second:

```console
$ muructl status --processes
daemon  state /var/lib/muru
edge  1,284 req/s  3.0 err/s  p50 4.0 ms  p95 38 ms  in flight 41  ws 212  conns 1,903  tls 12/s  in 2.1MB/s  out 46.4MB/s

PID    ROLE                     WHAT                                   USER      UP     CPU       RSS   REQ/S    BUSY   QUEUE      P95   GIL/LAG
4242   daemon                   system daemon                          muru    3d2h    0.3%    38.0MB
4250   ├─ edge                  listening on :80, :443                 muru    3d2h    2.1%    61.0MB   1,284
4400   ├─ relay                 blog@r3 output to apps/blog/r3.log     blog    1d0h    0.0%     7.0MB
4401   │  └─ server             blog@r3 instance 0: gunicorn           blog    1d0h    0.8%    74.0MB
4402   │     ├─ server-worker   gunicorn worker                        blog    1d0h    2.4%    96.0MB
4403   │     └─ server-worker   gunicorn worker                        blog    1d0h    2.2%    95.0MB
4330   ├─ host                  api@r7 serving                         api     1d4h    0.0%    88.0MB     203    6/64       0    12 ms     48 ms
4331   │  ├─ relay              api@r7 output to apps/api/r7.log       api     1d4h    0.0%     7.0MB
4340   │  └─ worker             group default, worker 0                api     1d4h    4.2%   131.0MB     203    6/64            12 ms     48 ms
4290   ├─ host                  shop@r41 retained until 07:40 UTC      shop   5h13m    0.0%    91.0MB
4291   │  ├─ relay              shop@r41 output to apps/shop/r41.log   shop   5h13m    0.0%     7.0MB
       │  ├─ group              crawlers
4293   │  ├─ worker             group crawlers, worker 0               shop   5h13m    0.0%   118.0MB
       │  ├─ group              default
4292   │  └─ worker             group default, worker 0                shop   5h13m    0.0%   131.0MB
4301   └─ host                  shop@r42 serving                       shop   5h12m    0.0%    92.0MB
4302      ├─ relay              shop@r42 output to apps/shop/r42.log   shop   5h12m    0.1%     7.0MB
          ├─ group              crawlers                                                                   61     4/8       0   120 ms    0.4 ms
4320      ├─ worker             group crawlers, worker 0               shop   5h12m    1.0%   120.0MB      61     4/8           120 ms    0.4 ms
          ├─ group              default                                                                 1,020   27/32       3    41 ms    2.1 ms
4310      ├─ worker             group default, worker 0                shop   5h12m   12.3%   140.0MB     512   14/16            40 ms    2.0 ms
4311      └─ worker             group default, worker 1                shop   2h03m   11.8%   138.0MB     508   13/16            42 ms    2.2 ms
```

The first line names the daemon's state directory. The next is the
instance's last second: requests and 5xx responses per second, the median and 95th percentile total time,
requests in flight, WebSockets and connections open, TLS handshakes per
second and the body bytes received and sent per second.

The roles are `daemon`, `edge`, `host` (one per running release),
`relay` (the release's log relay, which writes its output to the
release log, or the relay of a managed server's instance, which starts
the instance and writes its output), `worker`, `server` (a
[managed server](managed-servers.md) instance, named by the program its
command runs, such as `gunicorn`), `server-worker` (a process a managed
server's instance started, such as one of gunicorn's workers, which
serve the release's requests) and `command` (a `muructl command run` or
`muructl exec` while it runs). The tree is the one Muru supervises: a host adopted
again after the daemon restarted has the init process as its parent
in `ps`, which its row says.

The daemon's row says whose daemon it is, `user` or `system`. A host's row gives its release's part in routing after
its name, so an idle instance shows which release serves:

| Shown | The release |
| --- | --- |
| `serving` | is the one its application's traffic goes to |
| `canary 10%` | is a rollout's candidate, taking that share of the traffic |
| `held` | is ready and takes no traffic, as a deploy with `--hold` leaves it |
| `draining` | is off the route, finishing the requests it has |
| `retained until 07:40 UTC` | is kept warm for a rollback until then, given in UTC as `release list` gives it |

A release still starting shows its state, `starting` or `warming`.
`--json` gives the same as each host's `state`.

For a worker the figures are its requests per second, its slots holding
a request of all its slots, the 95th percentile total time and, under
GIL/LAG, the 95th percentile of what holds its requests up: its event
loop's lag when the release's default mount is ASGI, and its threads'
wait for the GIL when it is WSGI. On a build of Python without the GIL
a WSGI worker has no GIL wait, and the column is empty. Requests wait
for a slot in their worker group, so QUEUE is a group's: a release with
several groups has a row for each, and the host's row has the figures
of a release with one, its GIL/LAG the workers' together. `--json`
gives both figures, `gil_wait_p95_ms` and `loop_lag_p95_ms`, and the
`interface` that chooses between them.

A worker that exits within a second after it served, as one scaled in
does, still counts in its host's or group's figures for that second,
though it is no longer listed. Such a row's REQ/S is marked `*`, and a
line under the table says why; `--json` gives the count as `departed`.

On a terminal the view fits the terminal's width. The figure columns
empty in every row are left out, the columns close up, and a long WHAT
is cut short with an ellipsis; when that is still too wide, the last
columns are left out, and a line under the table names them. The
instance's figures wrap onto a second line. Output to a pipe or a file,
and `--json`, have every column in full.

- `--app APP[@RELEASE]` shows the daemon and one application's
  processes, or one release's.

- `--all` adds the other processes Muru did not start that descend from
  one it did, such as a subprocess a worker ran, beneath the process
  they descend from, with their command line, as `other`.

- `--watch` shows the view again every second until interrupted, or at
  the interval given, such as `--watch 5s`. On a terminal it is redrawn
  in place; otherwise each frame is printed after the last, and with
  `--json` each frame is one JSON document on a line.

- `--json` prints the tree as the [JSON output
  reference](../reference/json-output.md) describes.

`muructl status --processes` works the same against a development run
and inside a `muructl serve` container, whose first process is `run` or
`serve`, the controller and edge in one.

Each process also sets its title, so `ps`, `top` and `htop` tell them
apart:

| Process | Title |
| --- | --- |
| The daemon | `muru daemon user`, `muru daemon system`, or `muru daemon DIR (ID)` |
| The edge | `muru edge user`, `muru edge system`, or `muru edge DIR (ID)` |
| A development run, or `muructl serve` | `muru run APP ADDRESS dashboard ADDRESS` (without a dashboard, `muru run APP ADDRESS`), `muru serve APP ADDRESS` |
| A release's host | `muru host APP@RELEASE (INSTANCE)` |
| Its log relay | `muru relay APP@RELEASE (INSTANCE)` |
| A worker | `muru worker APP@RELEASE GROUP/INDEX (INSTANCE)` |

The daemon's and the edge's titles name the daemon as its row does:
`user` or `system`, or its state directory when that is not the default
for its scope, as with `--state-dir`, so two daemons of one user are
told apart. An edge left running while its daemon restarts has the
init process as its parent, and its title still says whose it is.

Every title names the instance the process belongs to. A host's,
relay's and worker's title ends with the instance in parentheses:
`user` or `system` for the default daemons, `run:8000` for a
development run on port 8000, `serve` under `muructl serve`, and the
first 8 hex digits of the daemon's instance ID for a daemon started
with its own `--state-dir`, which its own title and its edge's carry
too:

```console
$ ps -o pid,command -U "$USER" | grep 'muru '
 4123 muru daemon user
 4130 muru edge user
 4144 muru host shop@r3 (user)
 4145 muru relay shop@r3 (user)
 4150 muru worker shop@r3 default/0 (user)
 6001 muru run blog 127.0.0.1:8000 dashboard 127.0.0.1:8001
 6012 muru host blog@r1 (run:8000)
```

When a title is too long for the room it has, the words before the
instance are cut first. Every process the daemon, a run or a serve
starts, and every process those start in turn, has `MURU_INSTANCE_ID`,
the instance's full ID, and `MURU_INSTANCE_TAG`, its name as the titles show
it, in its environment, which is what ties a subprocess a worker started,
with no title of Muru's, to its instance.

A title longer than the command line the process was started with,
such as `muru serve shop 0.0.0.0:8080` for a process started as `muructl serve`,
takes the room of the environment strings that follow it, which the
process keeps a copy of, so the environment it and the processes it
starts see is unchanged, while `/proc/PID/environ` shows the title in
its place.

On Linux the short name `top` shows by default, and `/proc/PID/comm`
holds, is `muru-daemon`, `muru-edge`, `muru-run`, `muru-serve`, `muru-host`,
`muru-relay` or `muru-worker`.

`--metrics ADDRESS`, or a `[metrics]` table
in the configuration file, serves Prometheus metrics on an internal
address; the [metrics guide](metrics.md) lists them. `--dashboard
ADDRESS`, or a `[dashboard]` table, serves the [dashboard and JSON
gateway](dashboard.md) on a loopback address, for a session signed in
with `muructl dashboard login`. An `[overlay_webhook]`
table serves the [signed webhook](overlays.md#the-webhook) that
monitoring systems activate and end overlays with.

The daemon binds the listening sockets itself and hands them to an edge
process started from the same executable, which carries the traffic;
the daemon is never on the request path. A daemon that runs as root
starts the edge, and the applications that name no user, as the service
user, `muru` unless `--user` or the `[daemon]` table names another, and
refuses to start when that account does not exist; the [per-application
users guide](users.md) has the details. If the edge exits, the daemon
starts another on the same socket with the same routing, and connections
made in between wait in the socket's backlog rather than being refused.
The edge's own output goes to `edge.log` in the log directory, and each
release's host writes to `apps/APP/RELEASE.log` there.

The access log is written as JSON lines to `access.log` in the log
directory, `muructl logs` shows or follows it and each release's output,
and `muructl logs reopen` works with logrotate; the [logging
guide](logging.md) has the details. The events `muructl events` shows are
kept in the state store, and `[[events.sink]]` entries deliver them to
webhooks; the [events guide](events.md) has the details.

## Stopping and restarting

`muructl daemon stop` detaches: the daemon exits, and the edge and every
host keep serving exactly as they were. The next `muructl daemon start`
(or `up`, or a service manager restarting the unit) finds them through
the records in the runtime directory, takes them over, and carries on,
so a daemon restart or upgrade never costs a request. `muructl daemon
stop --shutdown` drains every release, stops the hosts and the edge, and
then exits, for taking a node out of service. It leaves no records
behind in the runtime directory, and the next daemon starts the current
releases afresh.

A host whose daemon is gone keeps its workers; a host that dies takes
its workers with it, and the next daemon starts its release again. A
worker that is killed, by a hard timeout say, may leave a subprocess a
request started; on Linux the host collects it, giving it the release's
shutdown timeout to finish before it is stopped, so nothing lingers or
sits as a zombie.

The shutdown is one sequence, and it fits a budget. The daemon file's
`[shutdown]` table sets the parts:

```toml
[shutdown]
on_sigterm = 'detach'   # or 'shutdown'
delay = '0s'
budget = '8s'
```

For `delay` the edge keeps accepting and serving, with every response
carrying `Connection: close` (GOAWAY on HTTP/2) and idle connections
closed, so clients with keep-alive connections reconnect, to another
node when a load balancer is in front; the default of none suits a
single node. Then the edge stops accepting and the requests in flight
finish, every host stops at once, each given its workers' stop budget or
what remains of the shutdown's, whichever ends first, and the edge goes
last, so the workers' WebSocket close frames reach their clients. The
whole takes at most `budget`; whatever has not finished by then is
killed. A long `shutdown_timeout` on a release wants a budget to match.

SIGTERM (and an interrupt) means `muructl daemon stop` without
`--shutdown`, that is a detach, which is what a service manager's restart
wants, unless `on_sigterm = 'shutdown'` makes it the shutdown above. In a
container that is the default; see below.

`muructl daemon run` runs it in the foreground, for a service manager or
a terminal; SIGTERM or an interrupt detaches it the same way, and
`muructl daemon stop --timeout` bounds how long the command waits for the
daemon to exit. The [systemd
guide](systemd.md) has the unit and what it does. Commands find the
daemon through its admin socket: `--socket` or `MURU_SOCKET`, else a
development run whose project is the current directory, else one started
from the current directory, else the daemon's socket in the runtime
directory, else the one development run listening; with several runs and
no daemon the command lists them, each with the `--instance` value that
chooses it. `muructl status` says which was found. When the daemon
answers while a development run is listening, the command says so on
standard error, with how to reach the run:

```console
$ muructl logs --access --lines 1
muru: using the daemon; the development run of shop on http://127.0.0.1:8080 is also listening: --instance run:8080, or the command run from /home/me/shop, reaches it
```

With several runs listening it says how many, and `muructl instance
list` names them with what to pass to `--instance` for each. With
several development runs of one project, the newest is reached, and the
command gives the `--instance` value of each other one on standard
error. `muructl run` prints the directories its commands are reached
from when it starts, and any other run of the same project already
listening. A run
directory you may not enter is passed over like one that is not there;
when nothing else is found, the command says permission was denied on
its socket.

## In a container

The daemon can run as the main process of a container that is kept and
updated in place, such as a container on a virtual machine used to keep
it apart from other services. Releases are deployed into it as on a
machine, with rollouts, canaries and rollbacks, and the state store is
kept on a volume.

For a container that is replaced rather than changed, one application
configured from its image with its logs on standard output, use
[`muructl serve`](containers.md) instead.

### Volumes

Two directories outlive the container:

- the state store, so that the deployed releases, their history and the
  events survive a new container;
- the log directory, so that the logs can be read after the container
  has gone and collected while it runs.

The run directory and `/tmp` hold only what the running processes need,
and can be memory-backed.

### Logs

Each stream goes to its own file in the log directory, as on a machine:
`daemon.log`, `edge.log`, `access.log`, and `apps/APP/RELEASE.log` for
each release's output. Every line of every file starts with its full UTC
time, and the access log is JSON lines, one record a request. A log
agent on the machine that runs the container, such as Fluent Bit,
Vector or Promtail, tails the log directory's volume. The container's
standard output holds the daemon's own lines and nothing else.

`muructl logs` reads the same files through `docker exec`:

```
docker exec shop-node muructl logs shop --follow
docker exec shop-node muructl logs --access --follow
```

### Stopping

When muructl finds itself PID 1 it starts as an init of its own, as
`tini` would: PID 1 forwards SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2
and SIGQUIT to the daemon, which runs as its child, collects every
process that exits, so nothing in the container stays a zombie, and
exits with the daemon's status when the daemon exits. An image that uses
`tini` or `docker run --init` sets `MURU_NO_INIT=1`, and the daemon runs
as PID 1 itself.

As a container's main process the daemon takes SIGTERM as shutdown
rather than detach, since when PID 1 exits the kernel kills every other
process in the container. `docker stop` therefore drains and stops
everything inside the [shutdown budget](#stopping-and-restarting), 8
seconds by default, under the 10 that `docker stop` allows, and the
container exits 0. An application whose requests or shutdown take longer
raises both, the budget in the daemon file and the container's own grace
with `docker run --stop-timeout` or Compose's `stop_grace_period`:

```toml
[shutdown]
budget = "25s"
```

A `[shutdown]` table with `on_sigterm = 'detach'` turns the shutdown
off, for a container that restarts its own daemon and keeps its hosts
across the restart.

### Probes

`muructl probe` asks the daemon the questions a container platform asks,
over its admin socket, so a health check needs no port and no `curl`:

```dockerfile
HEALTHCHECK --interval=5s CMD ["muructl", "probe", "ready", "--run-dir", "/run/muru"]
```

It exits 0 when the answer is yes and 1 when it is no, saying why on
standard error.

| Probe | Yes when |
| --- | --- |
| `live` | the daemon answers and its edge is running; a busy worker never makes it fail |
| `ready` | the daemon has started and is not stopping, and each application's serving release has a worker ready in each of its worker groups that is not optional |
| `startup` | the daemon has finished starting, having taken over the processes it found or started the releases afresh, and each serving release has a worker ready again |

An application with no serving release, never deployed or with its
deploys failed, holds neither `ready` nor `startup` back. `--app APP`
asks `ready` and `startup` of one application, and `--min-ready N` asks
`ready` for that many workers in each group rather than one.

With a [metrics address](metrics.md), the daemon answers the same three
on it as `/livez`, `/readyz` and `/startupz`, 200 for yes and 503 for no
with the reason, for something outside the container that can only make
HTTP requests.

### Listening and the admin socket

The daemon listens on `127.0.0.1:8080` unless told otherwise, so in a
container it takes `--listen 0.0.0.0:8080` or a `[[listener]]` entry.
The admin socket is in the run directory, inside the container, so the
commands that manage it run through `docker exec`, and only someone who
may exec into the container can use them:

```
docker exec shop-node muructl deploy shop /srv/shop
docker exec shop-node muructl status --processes
docker exec shop-node muructl dashboard login
```

### Memory limits

When the container has a memory limit, the daemon reads it from its own
cgroup's `memory.max` at every memory check, and the memory guard of
[container limits](containers.md#the-memory-guard) watches it across
every release: with less than the headroom left (10% of the limit, at
least 128MB), the largest worker of any release is replaced, one at a
time, or its group scaled in by it when the group has a scaling policy
above its `min_workers`. `muructl status` shows the limits, what is used
and left, and what the guard is doing.

The [node's budget](scaling.md#the-nodes-budget) takes its `memory` and
`cpu` from the same cgroup when the daemon file leaves them out, so a
deploy, a canary beside the serving release and a scale-out are admitted
against the container's limits before anything starts.

### Building nothing at run time

The daemon does not know it is in a container, and does not need to: it
resolves interpreters and builds environments as it does on a machine,
which is right in a development container, where the source is mounted
and the network is there. An image built ahead of time, with its
interpreters and environments in it, says so in the daemon file, and the
daemon then installs nothing at run time and fails at once, naming what
the image lacks:

```toml
[tools]
offline = true                  # nothing is downloaded, the runtime library included
python_downloads = "never"      # a Python the image lacks fails the release
```

with `--build never` on each deploy, or `when = "never"` in the project's
`[tool.muru.build]` table or a declared release's `build` table, so that
the environment in the image is used as it is.

### A Compose service

```yaml
services:
  shop-node:
    image: registry.example.com/muru-node:1.0.0
    command:
      - muructl
      - daemon
      - run
      - --listen
      - 0.0.0.0:8080
      - --state-dir
      - /var/lib/muru
      - --run-dir
      - /run/muru
      - --log-dir
      - /var/log/muru
    ports: ["8080:8080"]
    volumes:
      - state:/var/lib/muru
      - logs:/var/log/muru
    tmpfs: [/run/muru, /tmp]
    mem_limit: 2g
    healthcheck:
      test: ["CMD", "muructl", "probe", "ready", "--run-dir", "/run/muru"]
      interval: 5s
    stop_grace_period: 30s
volumes:
  state:
  logs:
```

The state store and the logs are on named volumes, the run directory and
`/tmp` in memory. `stop_grace_period` is above the shutdown budget the
daemon file sets, as above. `docker compose ps` shows the service healthy
once the daemon has started and every serving release has its workers
ready.

## Upgrading

Installing a new version of Muru changes nothing until `muructl daemon
upgrade`, which re-executes the daemon from the installed `muructl`,
keeping its process id and its children, and replaces the edge without
refusing a connection. Releases keep the runtime library they started
with until a new release replaces them. [Upgrading Muru](upgrading.md)
covers the command, development runs, the daemon and releases.

## The daemon's configuration file

The daemon reads one file of its own, which says how it runs: its
listeners, limits, logs, metrics and the rest. It need not exist;
without it the daemon serves HTTP on 127.0.0.1:8080 with the defaults.
What the daemon serves is not in it: an application, a host or a route
there is refused, and goes in a file for [`muructl
apply`](#declaring-applications-in-files).

`muructl daemon config` finds the file, shows it, checks it and changes
it. A running daemon says which file it reads; with none running it is
the file a daemon started with the same options would read.

```console
$ muructl daemon config path
/home/me/.config/muru/muru.toml (not present)
```

`set` changes one key by its dotted path. It checks the result as the
daemon would, writes the file whole, keeping its comments and layout,
and has the running daemon reload:

```console
$ muructl daemon config set limits.max_body_size 50MB
muru: wrote /home/me/.config/muru/muru.toml
muru: reloaded /home/me/.config/muru/muru.toml: changed [limits] (the edge has it); ...
```

A value that is TOML, such as `true`, `4` or `["a", "b"]`, is taken as
it is, and anything else as a string. `unset` takes a key out. A key in
a list of entries, such as a `[[listener]]`, is changed with `edit`.

A few settings are read only when the daemon starts. The file is
written, and the reload says the daemon keeps what it started with:

```console
$ muructl daemon config set daemon.detours true
muru: wrote /home/me/.config/muru/muru.toml
muru: reloaded /home/me/.config/muru/muru.toml: ...; [daemon] detours changed and is read at start, so the daemon keeps what it started with until it is restarted; ...
```

`muructl daemon stop` and `muructl daemon start` restart the daemon
with its hosts and its edge serving throughout.

`edit` opens the file in `$VISUAL`, else `$EDITOR`, else `vi`, starting
from a short commented file when there is none. It checks what was
saved before it writes anything, and a file with a mistake is never
written:

```console
$ muructl daemon config edit
muru: /home/me/.config/muru/muru.toml: "50 megs" is not a size such as 10MB or 512KB
Edit again? [Y/n]
```

The file is an ordinary file, and can be changed by other means: an
editor of your own, a checkout, configuration management. `check` says
whether the daemon would take it, and `muructl daemon reload` applies
it:

```console
$ muructl daemon config check
muru: /home/me/.config/muru/muru.toml is valid
$ muructl daemon reload
```

`check FILE` checks a file that is somewhere else, its relative paths
taken from its own directory, as a daemon started with `--config FILE`
would read it. `replace FILE` installs a whole file as the daemon's,
checked in the daemon's place, written and reloaded, and takes `-` for
standard input, for a script that generates the file:

```console
$ ./make-config | muructl daemon config replace -
```

`set`, `unset`, `edit` and `replace` take `--check`, which checks the
result and writes nothing, and `--no-reload`, which writes the file and
leaves the daemon as it is. With no daemon running they write the file
and say that the daemon reads it when it starts. A file this user may
not write, as a system daemon's is to anyone but root, is refused
before anything is edited, with the way to run the command with the
rights to.

`show` prints the file as it is written. `show --full` prints every
setting of its plain tables: what the file sets, marked so, and as
comments the default of every other key, with the flags the daemon was
started with that stand over the file. The result is itself a file the
daemon takes. A flag such as `--listen` holds over the file until the
daemon is started without it, and a change to a key such a flag stands
over is written with a warning saying so.

## Starting again from nothing

`muructl daemon reset` puts a daemon back to nothing. It removes the
state store, with every application, release, host and route in it, and
what the releases left: the environments built for them, their bytecode
and static directories and the upload spool. It works on the files
alone, so it needs no daemon, and it helps with a state store a daemon
can no longer read.

It runs only when nothing of the daemon is running. Stop everything
first:

```console
$ muructl daemon stop --shutdown
$ muructl daemon reset
muru: this removes the state of the daemon at /home/me/.local/state/muru:
  remove  /home/me/.local/state/muru/muru.sqlite  the state store: 3 applications, 7 releases, 2 hosts and 5 routes 212.0KB
  remove  /home/me/.local/state/muru/envs  the environments built for releases 1.2GB
  keep    /home/me/.local/state/muru/data  the applications' data (--data removes it)
  keep    /home/me/.local/state/muru/logs  the logs (--logs removes them)
  keep    /home/me/.config/muru/muru.toml  the daemon's configuration file
Remove? [y/N]
```

While a daemon answers, or its edge and hosts still serve after a
`muructl daemon stop` that left them, nothing is removed, and the
command names what runs and exits 1.

- `--dry-run` prints the list and stops, exiting 0 when a reset would
  go ahead.
- `--yes` goes ahead without asking. Without it and without a terminal,
  the list is printed and nothing is removed.
- `--json` prints the list as JSON: `allowed`, `running`, `remove` and
  `keep`.
- `--data` removes the applications' data directories under the state
  directory as well, and `--logs` the log files.

The daemon's configuration file, its file secret stores and the
instance's identity are always kept. The next `muructl daemon start`
finds nothing to serve.

## Reloading the configuration

`muructl daemon reload`, or SIGHUP, reads the daemon's configuration
file again, checks it whole, and applies what changed:

- the `[proxy]`, `[compression]`, `[error_pages]`, `[error_override]`,
  `[[client_class]]`, `[rate_limit]` and `[limits]` tables, and the
  `h2c` setting and the `limits` and `proxy` tables of a `[[listener]]`
  entry, which the edge is given at once; a connection already open keeps the limits it was accepted
  under;

- the `[[listener]]` entries: a changed set starts a new edge on the new
  listeners and tells the old one to stop, as `muructl daemon upgrade`
  does, so a listener that stays keeps its socket and its backlog and no
  request is lost; a listener that cannot be bound (its port in use,
  say) fails the reload before anything is applied;

- the `[[events.sink]]` entries: a sink added starts, one dropped stops,
  and one whose entry changed is restarted; a sink's cursor stays in the
  state store, so one that comes back delivers what it missed;

- the certificate and key files of every applied TLS host, read again
  and, when changed, sent to the edge; the [virtual hosts
  guide](virtual-hosts.md#tls) has the detail;

- the `[secrets.store.NAME]` tables, which the next release to start
  resolves its secrets from;

- the `[condition]` tables: a [condition](conditions.md) added starts
  being evaluated, one dropped stops, and one whose table changed starts
  again from `unknown`; one left as it was keeps its state;

- the `[logging.python]` table: each application takes the new Python
  logging defaults at its next restart or deploy, since a running host
  keeps those it started with;
- the `[logs]` table: the daemon rotates on the new policy from its
  next check, and a period that changed rotates each file once;
  `reopen_on_rename` turns the check for renamed files on or off at
  once; the access log's `access_log`, `access_log_exclude`, `slow`,
  `access_log_format` and `access_log_query` go to the edge at once,
  for the requests completed from then on;
  `on_overflow` and `output_queue` apply to each application at its
  next restart or deploy;

- the `[shutdown]` table, for the next stop signal or `muructl daemon
  stop --shutdown`;

- the [node's budget](scaling.md#the-nodes-budget), `[daemon.budget]`;

- the dashboard's `actions` switch, `[dashboard] actions`;

- `[daemon] pass_env`, for releases started from then on.

The rest of the file is read when the daemon starts: the `[metrics]`
table, the `[dashboard]` table's `listen` address, the
`[overlay_webhook]` table and the `[daemon]` table's `user` and
`detours`. A change to
those takes effect after `muructl daemon stop` and `muructl daemon
start` (`systemctl restart muru` under systemd), which detaches from the
hosts and the edge and takes them back, so it costs no request. A daemon
started with `--listen`, `--metrics`, `--dashboard` or `--user` keeps
those over the file's settings on every reload.

The command prints what changed and what was done about it, what is
unchanged and what is read at start, and the `config.reloaded` event
records the same. A file the daemon cannot parse, or a table it refuses,
leaves everything as it was and the command fails saying why.
