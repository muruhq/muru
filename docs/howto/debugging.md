# Inspecting workers

`muructl debug` looks into the workers of a release through the daemon,
the host and the worker's control connection, so nothing listens anywhere
new. Each command takes a release as `app@release`, or an application for
its current release, and asks every worker unless `--worker` names one;
`--json` prints each worker's answer as JSON.

A worker is named by its index within its group: `--worker 1` is worker
1 of the default group, and a worker of a named group takes the group's
name in front, as `--worker reports/1`. The same name is in the
worker's process title, in `muructl status --processes`, in the access
log's `worker` and `group` fields and in the tags of its lines in the
release's log, `muru.worker[1]` and `muru.worker[reports/1]`.

A development run serves the same commands: `muructl run` has an admin
socket of its own, which a command run from the project directory finds
without a flag, and the run's application and release are the ones its
terminal names, so `muructl debug stacks app` reads the run's workers.
The application can be left out, since a run serves only one:
`muructl debug stacks` does the same, and so does `muructl serve`'s
socket inside a container.
`debug exec` and `debug attach` need no `debug_exec` there, since the
person at the terminal started the workers. `muructl status` says which
socket a command found.

```
muructl debug status shop
muructl debug stacks shop@r3 --worker 1
muructl debug tasks api
muructl debug inspect shop --json
```

The daemon's [dashboard](dashboard.md#looking-into-the-workers) shows
the status, stacks, tasks and memory of a release's workers too, and so
does a run's, from the Workers link on its first page.

`muructl status --processes` shows which processes make up the instance
and what each is: the run or the daemon, the edge, each release's host,
log relay and workers, with their pids, CPU, memory and each worker's
figures over the last second, and `--watch` keeps the view live. The
[daemon guide](daemon.md#the-processes) describes it.

## Status

Each worker's pid, group, uptime, requests served, what it holds as of
the moment it answers, its memory, whether the edge routes to it, and
its state.

```
WORKER  PID      GROUP      UP       REQUESTS   THREADS  THREAD-OLDEST  RSS     USS     ROUTED  STATE
0       41213    default    5m18s    12044      1/4      0.1s           48MB    31MB    yes     ready
1       41214    default    5m18s    11980      0/4      -              47MB    30MB    yes     ready
```

THREADS is the application threads busy of the worker's threads, and
THREAD-OLDEST how long the longest has been held: a request holds its
thread from the moment a pool thread picks it up, so one waiting at
`breakpoint()` or streaming its response, which the timeouts leave
alone, is counted for as long as it holds the thread. A worker with an
event loop shows LOOP, the requests on the loop of its `concurrency`,
and LOOP-OLDEST; open WebSockets are left out of both, since a socket
may stay for hours, and `muructl websockets` counts them. Beside them,
SYNC is the threads running synchronous code for the loop, of the
threads that may: an ASGI application whose views call blocking code
fills them, and at `8/8` every new call waits for a thread while the
loop itself has room. Such code runs on one of
[three pools](asgi.md#one-event-loop-per-worker), the loop's own,
anyio's and asgiref's shared thread, and SYNC is the fullest of them,
the one whose calls wait first; `--json` gives each as `sync_pools`.
SYNC-OLDEST is the age of the oldest request with a call on the loop's
pool or asgiref's thread, and a dash when the busy threads are anyio's.

```
WORKER  PID      GROUP      UP       REQUESTS   LOOP      LOOP-OLDEST  SYNC    SYNC-OLDEST  RSS     USS     ROUTED  STATE
0       41213    default    5m18s    12044      31/1000   4.2s         8/8     4.2s         52MB    34MB    yes     ready
```

A worker shows the sides it has, and the table the columns any worker
of the release has, so a WSGI release has the thread columns and an
ASGI release the loop and sync columns. `muructl debug stacks` names,
on each thread of the pool, the request whose call it is running. Every header is one word, so a script can
pick a column by its name.

The figures are as of the moment the status answers: the host asks each
worker, and one that does not answer within a second, which is a hung
one, is shown from its last report, up to a second old, its figures
marked with a star. STATE is `starting`, `ready`, `debugging` (a
debugger session is open, so the worker takes no requests), `replacing`
(a fresh worker is taking its place) or `retiring` (scaled in). ROUTED
says whether the edge sends it requests: a replacement that is ready
but not yet announced to the edge reads `ready` and `no`.

## Requests in flight

`muructl requests` shows the requests in flight now, as `muructl
websockets` shows open sockets: those queued at the edge for a worker
slot, those holding a thread, and those on an event loop, with the age
of the oldest of each, one row per worker group, since the edge queues
per group and a count per release would hide which group is starved.
The thread columns show for a release whose workers run requests on
threads, the loop columns for one with an event loop:

```console
$ muructl requests shop
RELEASE              GROUP        QUEUED  QUEUED-OLDEST  THREADS  THREAD-OLDEST  LOOP   LOOP-OLDEST
shop@r7              default      3       1.2s           8/8      1m02s          14     31.0s
shop@r7              reports      0       -              2/4      4.1s           -      -
```

`--list` lists them, the oldest first, with `--limit` (100) on how many:
the age, whether the request is queued or running, its kind, the worker
that has it, the method and target, the client and the request id.

```console
$ muructl requests shop --list --limit 5
AGE      STATE    KIND            WORKER  REQUEST                                  CLIENT           REQUEST-ID
1m02s    running  wsgi            1       GET /checkout                            10.0.0.7         01M5R1GN3RD59QX94CBQH28ENT
31.0s    running  asgi            0       GET /live/summary                        10.0.0.9         01M5R1GN3RD59QX94CBQH28ENV
4.1s     running  wsgi            1       GET /reports/annual                      10.0.0.7         01M5R1GN3RD59QX94CBQH28ENW
1.2s     queued   -               -       GET /reports/monthly                     10.0.0.12        01M5R1GN3RD59QX94CBQH28ENX
0.8s     running  wsgi streaming  0       GET /events                              10.0.0.3         01M5R1GN3RD59QX94CBQH28ENY
```

A request's kind is what the edge can tell of it as of now: `wsgi` or
`asgi` by the side of the worker it was sent to, `wsgi streaming` or
`asgi streaming` once its response head asked for immediate flushing
(`text/event-stream`, or `X-Accel-Buffering: no`), which the timeouts
leave alone but which holds its thread or loop slot, `grpc`, `handoff`
once the worker handed it to its other interface, and `upstream` for a
request sent to a managed server. A request that became a WebSocket
leaves the list and is counted by `muructl websockets`; a static file
is answered by the edge at once and is never listed.

THREADS and LOOP here are what the edge charged each request to, from
what the worker declared when it connected. `debug status` shows what
the worker itself holds, so the two can differ for a worker serving
both WSGI and ASGI. What a worker does inside a request, such as a
thread waiting at `breakpoint()`, stays with `debug stacks` and
`debug status`. The command works against the daemon, a development run
and `muructl serve`, and `--json` gives the same figures.

## Stacks

The Python stack of every thread in every worker, with the request a
thread is serving, taken from the runtime's own bookkeeping: its id,
method, path and how long it has run. Threads that show the same stack
across workers are shown once with the workers they came from, so
sixteen workers waiting on the same lock read as one stack.

```
muru-app-0 in worker 1:
  serving GET /reports/monthly for 42.3 s (request 01M5R1GN3RD59QX94CBQH28ENT)
  File "/srv/shop/shop/reports.py", line 88, in monthly
    rows = cursor.fetchall()
```

On Linux with Python 3.15 or later the host reads the stacks from
outside the worker (PEP 768), so a worker stuck in C code with the
interpreter held, or stopped altogether, shows them at once. Elsewhere,
on older interpreters and on macOS, where reading another process needs
root, the worker is asked. A worker asked that cannot answer in five
seconds, because something holds the interpreter, is reported as such,
and the host sends it SIGQUIT so that `faulthandler`, which needs no
interpreter, writes every thread's stack to the release's log instead;
`muructl logs APP` shows it. On Linux with Python 3.14 such a worker is
also read from outside, which there gives only each thread's innermost
frames, up to the first call from C code; the log has the whole stacks.

## Tasks

For ASGI workers, the tasks on the event loop, the coroutine each runs,
what it awaits and where it is, and the loop's lag: how long a callback
scheduled from another thread waited to run.

## Inspect

Each worker's interpreter and executable, working directory, `sys.path`,
the modules loaded with versions where they declare one, threads, open
descriptors and environment.

The environment lists every variable's name, and gives values only for
these:

- the variables Muru sets, whose names start with `MURU_`;

- the interpreter's: `PATH`, `VIRTUAL_ENV`, `LANG`, `TZ`, and names
  starting with `PYTHON` or `LC_`;

- the release's own, from `[tool.muru.env]` and `muructl deploy --env`,
  except a value that comes from a secret store. A secret delivered as
  a file shows its `NAME_FILE` path;

- under the daemon, what it passes on: `HOME`, `USER`, `LOGNAME` (the
  release's own user's, when it runs as another user), `TMPDIR` and the
  variables `[daemon] pass_env` names.

Every other variable is named without its value. Under `muructl run`
and `muructl serve`, which keep the environment they were started with,
that is most of them:

```
  environment (64 values left out; --env shows them):
    DATABASE_URL=postgres://shop:***@db/shop
    HOME
    MURU_APP=shop
    PATH=/usr/bin:/bin
    SENTRY_ENVIRONMENT
```

`muructl debug inspect --env` shows every variable's value. In `--json`,
a value left out is `null`.

Either way, the value of a key that looks like a secret (`SECRET`,
`TOKEN`, `PASSWORD`, `KEY`, `CREDENTIAL` or `AUTH` in its name, or a
name ending in `_DSN`) is replaced by `***`, and a password in a URL
under any other key is: `DATABASE_URL` holding
`postgres://shop:hunter2@db/shop` is shown as
`postgres://shop:***@db/shop`.

## Profile

`muructl debug profile` samples the stack of every thread running Python
code, every 10 ms for 10 s unless `--interval` and `--for` say otherwise,
in each worker of the release at once, and merges the samples into
collapsed stacks, the format flame graph tools read:

```
muructl debug profile shop --for 30s --output shop.folded
flamegraph.pl shop.folded > shop.svg
```

Each line is a stack, root first and under the thread's name, then how
many samples found it. A function is named with the file and the line it
starts on, so its samples are counted together whichever of its lines
was running. The functions that took most samples are printed
on standard error with their share. Threads waiting outside Python, such
as idle threads of the pool, are not sampled, so the profile is of the
work being done.

A profile at the default interval makes no measurable difference to a
busy worker's throughput or latency, so it is safe on a release serving
traffic. On Linux with Python 3.15 or later the samples are read from
outside the worker, which is not interrupted at all. Elsewhere, and
with `--path` or `--requests`, a thread of the worker takes them: with
the GIL it waits its turn for the interpreter like any other thread,
and on a free-threaded build it pauses the other threads for the moment
each sample takes. Sampling much more often costs more in proportion;
at a 1 ms interval, expect a busy worker to lose a few percent.

On a busy worker, `--path` samples only the threads serving requests
whose path starts with a prefix, and `--requests` ends the profile once
that many of them have finished, within `--for`:

```console
muructl debug profile shop --path /reports --requests 20 --for 5m --output reports.folded
```

A request shorter than the interval may finish unseen and is not
counted. Requests on an ASGI worker's event loop have no thread of their
own and are not sampled this way.

`--format pstats --output FILE` writes the samples as Python's pstats
instead, for `python -m pstats`, snakeviz and other tools that read
it. The figures come from the samples: a function's calls are the
samples it appeared in, its own time the samples it was running in, and
its cumulative time the samples it was on the stack for, each sample
worth one interval.

## Trace

`muructl debug trace` counts every call of every Python function and the
time spent in each, with sys.monitoring, for `--for` (10 s), and prints
the functions that took longest with their call counts. It needs Python
3.12 or later. Exact counts cost the worker more than sampling, so a
worker serving requests on more than half its threads, or holding more
than half its event loop's concurrency, is refused unless `--force` is
given. The worker keeps serving while the trace runs, so
the trace counts the requests it takes meanwhile, each slowed by the
tracing.

## Memory

`muructl debug memory` shows each worker's resident, proportional and
unique size, the garbage collector's counts, the collections it has run
and the object types it holds most of. With `--trace`, tracemalloc runs
for `--for` (30 s) and
the sites that allocated most in that time are shown, by bytes and
objects added:

```
muructl debug memory shop --trace --for 1m --top 30
```

To compare two moments of your choosing, such as before and after a
batch job, `--snapshot` keeps a tracemalloc snapshot in each worker,
starting tracemalloc if it is off, and `--diff` later shows the sites
that allocated most since then. The snapshot is kept for further
`--diff`s until `--drop`, which also stops tracemalloc if `--snapshot`
started it:

```
muructl debug memory shop --snapshot
muructl debug memory shop --diff
muructl debug memory shop --drop
```

The collections are timed in every worker from the moment it starts, by
generation (`0`, `1` and `2`, or `full` on a free-threaded interpreter,
whose collector takes the whole heap each time): how many ran, how long
they took in all and at the 95th percentile, how many objects they freed,
and how many of them ran while the worker had requests in flight, with
how many requests were waiting through them:

```
worker 0 (pid 4133)
  rss 61.2MB  pss 38.9MB  uss 30.1MB
  gc counts [412,3,0]
  gc generation 0: 1840 collections, 92.3 ms in all, p95 under 0.1 ms, 12 objects freed, 1611 while 2904 requests were in flight
  gc generation 2: 3 collections, 48.0 ms in all, p95 under 50 ms, 219 objects freed, 2 while 5 requests were in flight
  objects:
```

A collection pauses the whole worker, so pauses that landed on requests
in flight are tail latency no single endpoint explains. The same figures
are in the [metrics](metrics.md) as `muru_gc_pause_seconds`, and the
[scaling](scaling.md) diagnosis names collection when it takes a tenth of
the workers' time or more. Young collections that each free many objects
are clearing short-lived reference cycles, such as two objects that
refer to each other made for every request; the diagnosis says so, and
breaking the cycle, or making the reference back a `weakref`, takes that
collection away. Collections that free few objects are going over
long-lived ones. The host already calls `gc.freeze()` before every fork,
so a worker's collections scan only what the worker itself created; an
application whose collections still cost it can create fewer long-lived
cyclic objects after the fork, or call `gc.freeze()` itself once its
caches are warm.

## Running code

`muructl debug exec` runs a snippet in the workers and prints what it
printed and the value of its last expression, or the traceback when it
raised:

```
muructl debug exec shop --worker 0 -- 'import myapp.cache; len(myapp.cache.entries)'
```

It is off unless the daemon's configuration file allows it, and on
under `muructl run`, where the person at the terminal started the
workers:

```toml
[daemon]
debug_exec = true
```

Every use is the event `debug.exec` with the code that ran, as profiles,
traces and memory traces are `debug.profile`, `debug.trace` and
`debug.memory`.

## Attaching a debugger

`breakpoint()` in application code makes the worker wait there for a
debugger, for 60 seconds or the `timeout` it is given, and raises the
event `debug.breakpoint` with the worker, file and line. A WSGI request
waiting there is exempt from the soft and hard timeouts meanwhile; in an
ASGI application, `breakpoint()` holds the worker's event loop, as pdb
does, and the timeouts still apply. When nobody attaches it carries on,
with the event `debug.breakpoint_expired`.

```python
def checkout(request):
    order = build_order(request)
    breakpoint(timeout=300)
    return charge(order)
```

`muructl debug attach` opens a pdb session at the waiting breakpoint,
taking the worker that waits unless `--worker` names one. The event's
line names the command to run, with the application as the daemon or
the run calls it, and under `muructl run` it works as it is, with no
`debug_exec` to set:

```
$ muructl debug attach shop
muru: attached to worker 1 at /srv/shop/views.py:42; the end of input lets the code go on
> /srv/shop/views.py(42)checkout()
-> breakpoint(timeout=300)
(Pdb) p order.total
Decimal('120.00')
(Pdb) continue
muru: the session in worker 1 has ended
```

The session stops on the `breakpoint()` line itself, the line the
attach line names, as pdb does on Python 3.13 and later; on 3.11 and
3.12 it stops at the line after it. Each line typed is a pdb command.
`continue` lets the request go on and ends the session; so does the end
of input, as does an hour in all or ten minutes without a line. In a
worker with no breakpoint waiting, the session has a thread of its own,
from which the worker's modules and state can be examined.

While a session is open its worker takes no new requests, and nothing
replaces it: recycling, the hard timeout, the memory limits and scaling
in all wait until the session ends, when the worker serves again.
`muructl debug status` shows it as `debugging` meanwhile.

### From an IDE

`--dap` carries the Debug Adapter Protocol instead of a pdb session, so
an IDE such as VS Code debugs the worker through muructl. The release's
environment needs debugpy installed. muructl listens for the IDE on
`127.0.0.1:5678`, or the address `--listen` gives, and attaches once it
connects:

```
$ muructl debug attach shop --dap
muru: waiting for an IDE to attach to 127.0.0.1:5678 with the Debug Adapter Protocol
muru: attached to worker 1 at /srv/shop/views.py:42; the IDE disconnecting lets the code go on
```

In VS Code that is an `attach` configuration naming the address:

```json
{
  "name": "Muru worker",
  "type": "debugpy",
  "request": "attach",
  "connect": { "host": "127.0.0.1", "port": 5678 },
  "pathMappings": [{ "localRoot": "${workspaceFolder}", "remoteRoot": "/srv/shop" }]
}
```

A request waiting at `breakpoint()` stops at the line after it once the
IDE has attached, unlike pdb, and the IDE can step through it from
there; with none waiting, the IDE sees the worker's threads and can
pause them. As with
pdb, the worker takes no new requests meanwhile. Disconnecting ends the
session and lets the code go on.

A debugger runs code in the worker, so attaching needs `debug_exec =
true` as running a snippet does. Each session is the event
`debug.attached`, and `debug.detached` when it ends. Outside Muru,
`breakpoint()` is the ordinary pdb, so the call is safe to leave in
while developing; setting `PYTHONBREAKPOINT` keeps whatever it names.

## Automatic captures

Some events say what the worker was doing, captured before the worker
lost the evidence:

- `request.soft_timeout`: a request past the [soft timeout](timeouts.md),
  with where its thread was, taken before the timeout interrupted it.
- `request.past_deadline`: a request still running after the edge
  answered it 504 at its [deadline](timeouts.md#past-the-deadline);
  `debug stacks` shows where it is.
- `worker.timeout_killed`: a worker replaced for the hard timeout, after
  `worker.loop_stalled` or `worker.gil_stalled`, or because its reports
  stopped, with where its oldest request was. Its threads' stacks go to
  the log, through faulthandler when the worker cannot answer.
- `worker.restarted` with `--force`: a worker [killed on
  request](recycling.md#restarting-a-worker-by-hand), its stacks taken
  the same way first.
- `worker.recycled` for memory: a worker replaced over a [memory
  limit](memory.md), with the object types it held most of.

## Logger levels

`muructl logging level` sets a logger's level in every worker of a
release, for a period or until reset, without a restart:

```
muructl logging level shop shop.payments=DEBUG --for 10m
muructl logging level shop =DEBUG --for 1m        # the root logger
muructl logging level shop --reset
```

Each worker reports the level it set and the one it replaced. Levels
changed this way revert when the period ends, on `--reset`, and when the
worker is replaced, since a new worker starts from the release's
[logging defaults](logging.md).

With no level and no `--reset`, the command changes nothing and lists
what each worker has changed, with the level it replaced and when it
reverts:

```console
$ muructl logging level shop
worker 0 (pid 4242): shop.payments DEBUG (was INFO), until 2026-11-02T05:52:22Z
worker 0 (pid 4242): root DEBUG (was WARNING), until reset
worker 1 (pid 4243): no levels changed
```

## Logging setup

`muructl debug logging APP[@RELEASE]` shows the logging and warnings
setup each worker has in force, after the application and its
libraries have configured what they will. It changes nothing.

```console
$ muructl debug logging shop
worker 0 (pid 4242), the same in worker 1 (pid 4243):
  logging.disable off, warnings captured no
  last resort: StreamHandler, level NOTSET, to stderr, format "%(levelname)s %(name)s: %(message)s"
  loggers:
    root: level INFO (Muru's defaults), effective INFO
    shop.payments: level DEBUG (muructl logging level), effective DEBUG
    shop.audit: level INFO (set by the application or a library), effective INFO, does not propagate
      handler FileHandler, level NOTSET, to /srv/shop/audit.log, format "%(asctime)s %(message)s"
    urllib3: level WARNING (Muru's defaults), effective WARNING
    and 41 loggers with nothing of their own (--all lists them)
  warnings filters, the first matching wins:
    ignore::DeprecationWarning::0
    default::DeprecationWarning:__main__:0
```

Each logger has the level set on it and its effective level, whether it
passes records up to its parent, whether it is disabled, and its
handlers. A handler is shown with its class, level, where it writes (a
standard stream, a file, a socket address, syslog or a mail server),
its format and its filters, and nothing else from its settings, so a
mail handler's credentials are never shown; a URL's user information
is masked. Where a level came from is said as far as can be known:

| Shown | Meaning |
| --- | --- |
| Muru's defaults | the level the release's [logging settings](logging.md#python-logging) set, still as set |
| `muructl logging level` | a temporary change from `muructl logging level`, still as set |
| changed since Muru set it | one of those, changed since by the application or a library |
| Python's default | the root logger at WARNING, as Python starts it |
| set by the application or a library | a level Muru did not set |

Only the root and the loggers with a level, a handler or a flag of their
own are listed; `--all` lists every logger. When the workers' setups
match, one is shown and the others are named as the same; when they
differ, each setup is shown with the workers that have it. `--worker N`
asks one worker and `--json` gives everything, per worker. The command
works against a development run and `muructl serve` as against the
daemon, and `muructl debug bundle` keeps each running release's in
`config/logging-setup.json`.

## How it reaches the worker

Every worker has a control connection to its host, created at fork, and
the host has one to the daemon. A request travels daemon to host to
worker, where a thread of the runtime library takes it, attaches to the
interpreter only to answer, and sends the answer back the same way. A
`profile`, a `trace` or a `memory --trace` runs its period, and an
`exec` its code, on a thread of its own, so the worker goes on answering
`status` and `stacks`, and giving up requests stuck past the hard
timeout, while one runs. A worker runs one of them at a time, and
refuses a second with a message saying that another is running. A
profile read from outside the worker takes no thread of the worker and
is not counted. The daemon never reaches into a worker process directly, which is what lets
the same commands work on a release running as another user, and on
an application in a container under `muructl serve`.

A reply is held to 256 MiB. Past that, the command says the reply was
too large and to ask one worker with `--worker`, or a shorter `--for`
for `profile`, or a smaller `--top` for `trace` and `memory`.

## A report for a bug report

`muructl debug report` prints a short summary of an installation, to
attach to a bug report on Muru. It is safe to post in public, and small
enough to paste into the report's form:

```
muructl debug report
muructl debug report --format markdown
muructl debug report --json
```

`--format markdown` is the form to paste into an issue: it shows there
as tables, where plain text pasted without a code fence is reformatted.
Every value that came from the system is in a code span or a fenced
block, so nothing in it is read as markup, and a name such as `@alice`
or `#123` in it notifies no one and links to nothing. `--json` gives the
same as [JSON](../reference/json-output.md).

```
Muru 1.0.0 on linux x86_64 (kernel 6.8.0)
Runs as a user daemon, running, not in a container

RELEASES
  RELEASE STATE   SERVING INTERFACE FRAMEWORK   PYTHON         WORKERS
  app1@r3 ready   yes     wsgi      flask 3.0.3 3.13.1 regular 2 ready
  app1@r2 stopped no      auto      -           3.13.1 regular -

SETTINGS OF THE SERVING RELEASES
  app1@r3
    entrypoint = <module1>:application  ([tool.muru])
    python = ~/<path1>                  (deploy flags)
    processes = 2                       (deploy flags)
    at their defaults:
      build.when = auto, bytecode = source (precompiled), interface = auto,
      preload = auto, threads = 1, sync_threads = 8, concurrency = 1000

DAEMON SETTINGS
  limits.max_body_size = 2MB
  listener[0].address = <address1>:443

EVENTS IN THE LAST 24 HOURS
      3  info     release.ready
      1  warning  worker.timeout_killed

MOST RECENT WARNINGS AND ERRORS
  2026-11-02T05:42:22Z warning worker.timeout_killed app1@r3: worker 1 (pid 41214): a request has run for 61.0 s, past the hard timeout of 60 s

Placeholders stand for 1 application name, 1 path, 1 address, 1 module. Read the report before posting it.
```

The report holds:

- Muru's version, the operating system, its kernel and the architecture.

- How Muru runs: `muructl run`, a user daemon, a system daemon or
  `muructl serve`, whether it is running, and whether it is in a
  container.

- Every release: its state, whether it serves, its interface, the
  frameworks one of its workers has loaded, the Python version and
  build (regular or free-threaded) and its workers' states. The
  frameworks are the known ones among the worker's loaded modules, so
  no module of the application's own is named.

- The effective settings of the serving releases, each with where it
  came from, as [`muructl config show
  --effective`](daemon.md#what-an-apply-does) reports them. For a
  development run and `muructl serve` these are the settings
  `--print-config` prints. A setting left at its default is listed with
  the others at their defaults.

- What the daemon's configuration file sets. The tables that configure
  the daemon itself (`daemon`, `tools`, `listener`, `limits`, `proxy`,
  `compression`, `logs`, `shutdown`, `metrics` and `dashboard`) are
  listed setting by setting, and any other table, such as the virtual
  hosts, routes and event sinks, only by how many entries it has.

- How many events of each type and level the last 24 hours hold, and
  the ten most recent warnings and errors.

It holds no application logs, access log lines, request data or client
addresses, no environment variables, and no secret values. What
identifies the system is replaced by a placeholder, the same one
wherever the same value appears:

| What | Reads as |
| --- | --- |
| An application's name | `app1`, `app2`, so `shop@r3` reads as `app1@r3` |
| A virtual host's name, and the host of a URL | `host1`, `host2` |
| The machine's own name | `node` |
| A user's or a group's name | `user1`, `user2` |
| Muru's own directories and the daemon's configuration file | `<state-dir>`, `<run-dir>`, `<log-dir>`, `<cache-dir>` and `<daemon-config>`, with what follows kept |
| The home directory, and a path under it | `~`, and `~/<path1>` |
| Any other path, except the system's own such as `/usr/bin/python3` | `<path1>`, `<path2>` |
| A network address, except loopback and the unspecified address | `<address1>`, `<address2>` |
| The module of an entry point or of hooks, wherever it is named | `<module1>`, so `shop.wsgi:application` reads as `<module1>:application` |
| A package such a module is in | `<package1>`, `<package2>` |
| The value of a setting whose name marks it as a secret | `<redacted>` |
| The value of a setting that is free text, such as a list of host names | `<set>` |

An event's sentence is given with the same replacements. The sentence
of an event that quotes a request, a command or the application's code
(the `request.`, `command.` and `debug.exec` events and the breakpoint
ones) is left out. A sentence can still quote an error the application
raised, such as the exception that stopped a release from loading, so
read the report before posting it. Nothing in it is hidden or encoded,
and the last line says how many values of each kind were replaced.

The command works with no daemon running: it reads the stopped daemon's
state store and configuration file, and changes neither. It takes the
daemon's location options for that (`--state-dir`, `--config`,
`--run-dir` and `--system`). A running daemon says where its files are,
so no option is needed then. A part that cannot be collected is listed
under `NOT COLLECTED` with the reason, in place of its figures, and the
command still succeeds.

## A bundle of a system for triage

`muructl debug bundle` writes one archive of a whole system for a time
window, to triage a problem with afterwards, somewhere else. It is for
the people who own the system: it holds real log lines, client addresses
and configuration, and is not for posting. For a public bug report, use
[the report](#a-report-for-a-bug-report).

```
muructl debug bundle
muructl debug bundle --since 30m --output /mnt/scratch/shop.tar.gz
muructl debug bundle shop@r3 --since 2d
muructl debug bundle shop --scrub
muructl debug bundle shop --memory --profile 30s
muructl debug bundle --dry-run
ssh web1 muructl debug bundle --output - > web1.tar.gz
kubectl exec deploy/shop -- muructl debug bundle --output - > shop.tar.gz
```

The archive is a `.tar.gz` that any tool opens. `--since` says how far
back it reaches, an hour when left out. A target limits it to one
application, or to one release as `app@release`; the whole node is the
default.

`--output` names the file, and `-` writes the archive to standard
output, to send through `ssh` or `kubectl exec`. With no `--output` the
file is `muru-bundle-` and the time in UTC, in the current directory.
The file is created readable and writable by its owner alone, and a
file that already exists is refused. Before writing, the command checks
that the file system has room, and refuses with how much the bundle
needs when it has not:

```
$ muructl debug bundle
error: the bundle needs about 1.2GB, and /var/lib/muru has 310.0MB free: write it elsewhere with --output, or to standard output with --output -
```

`--dry-run` prints what the bundle would hold and about how large it
would be, writes nothing, and asks nothing of the workers:

```
$ muructl debug bundle --dry-run
A bundle of the whole node, reaching back 1h to 2026-11-02T04:42:22Z, would hold:
  installation.json             asked for when the bundle is made
  events.jsonl                  48.2KB
  config/releases/shop@r3.json  6.1KB
  config/effective.toml         1.9KB
  logs/access.log               212.4MB, the lines of the window
  logs/apps/shop/r3.log         18.0MB, the lines of the window
  logs/edge.log                 3.1KB, the lines of the window
About 57.7MB once compressed, from 230.5MB on disk and what is asked for when it is made.
/home/ops has 41.0GB free.
```

When it finishes, the command says where the archive is, how many parts
it holds, each part it could not collect, and that it holds production
data:

```
$ muructl debug bundle
muru: wrote the bundle to muru-bundle-20261102-054222.tar.gz (3.4MB): 31 parts, 1 not collected
  live/shop@r3/stacks-2.json: no answer within 5 seconds
muru: it holds production data (logs, client addresses and configuration) and only its owner can read it: handle it as such
```

### What it holds

Everything is under one directory named as the file is.

| Path | Holds |
| --- | --- |
| `manifest.json` | What the bundle is of, its window, and every part: whether it was collected, its size, and why not when it was not |
| `installation.json` | Muru's version, the platform, how Muru runs and where its directories are, and every release with its state, interface, Python version and workers' states |
| `events.jsonl` | Every event of the window, in full, a JSON object a line |
| `config/releases/APP@RELEASE.json` | For every release, serving or not: what it is, its effective settings with where each came from, as `muructl config show --effective` lists them, and its whole specification |
| `config/effective.toml` | The configuration in force as `muructl config show --effective` writes it: applications, releases, virtual hosts and routes as the active overlays leave them |
| `config/in-force.json` | What is in force outside the configuration: each rollout and where it stands, the sizes releases are pinned at, the applications and virtual hosts in maintenance mode, and each overlay's state |
| `config/daemon.toml` | The daemon's own configuration file |
| `config/access-log-overrides.json` | The changes `muructl logging access` has in force |
| `config/logging-setup.json` | The logging and warnings setup each running release's workers have in force, by `APP@RELEASE`, as [`muructl debug logging`](#logging-setup) gives it with `--json` |
| `config/logger-levels.json` | The logger levels `muructl logging level` has changed in each running release's workers, by `APP@RELEASE`, as the command lists them with `--json` |
| `config/routing-snapshot.json` | The version of the routing snapshot in force, which an access log record's `snapshot` names |
| `config/changes.jsonl` | The events of the window that changed the configuration: an apply or a reload, traffic moved, a deploy, a rollout's step, an overlay starting or ending |

| `logs/` | The logs of the window, by their names in the log directory: `access.log`, `edge.log`, `daemon.log`, a file virtual hosts name for their records, and each release's output as `apps/APP/RELEASE.log` |
| `live/APP@RELEASE/status.json` | Each worker's state, requests and memory, as [`muructl debug status --json`](#status) gives them |
| `live/APP@RELEASE/stacks-1.json`, `stacks-2.json`, `stacks-3.json` | The workers' [stacks](#stacks), taken three times two seconds apart |
| `live/APP@RELEASE/tasks.json` | The asyncio [tasks](#tasks) of the workers' event loops |
| `live/requests.json` | The [requests in flight](#requests-in-flight), up to a thousand, as `muructl requests --list --json` gives them |
| `live/websockets.json` | The open WebSockets, up to a thousand, as `muructl websockets --list --json` gives them |
| `live/processes.json` | The processes as `muructl status --processes --json` gives them, narrowed to the target's application when the bundle has one |
| `figures/metrics.txt` | The [metrics](metrics.md) as a scrape reads them, the garbage collection figures among them |
| `figures/scaling/APP@RELEASE.json` | Each worker group's scaling, as [`muructl scale status --json`](scaling.md) gives it |
| `host/host.json` | The machine: the kernel, the CPUs, the load averages, the memory and how much is available, the limit on open files, the room on the file system of each of Muru's directories, and on Linux the limits of the daemon's and the edge's processes and of the container's cgroup |
| `host/APP@RELEASE/packages.txt` | The distributions installed in the release's environment, each with its version |
| `host/APP@RELEASE/uv.lock` and the like | The lock files of the release's source that are there: `uv.lock`, `pylock.toml`, `requirements.txt`, `poetry.lock`, `pdm.lock` and `Pipfile.lock` |
| `host/APP@RELEASE/inspect.json` | What the release's workers have loaded: the interpreter and its build, `sys.path`, the modules with their versions, the threads and descriptors, and the environment as [`muructl debug inspect --json`](#inspect) gives it |
| `failures/events.jsonl` | Every event Muru still keeps that records something going wrong, from before the window too: a request past its soft timeout or its deadline, a worker killed for a timeout or for memory, recycled, stalled or gone, a release that failed |

With a target, the releases, events and output of other applications
are left out, the access log's records are those of the target alone,
and the events and logs of the node stay.

The `live/` parts are of each release that has workers. One sample of
the stacks cannot tell a thread that is stuck from one that is busy, so
they are taken three times: a thread at the same place in all three is
waiting there. The history the dashboard's charts draw is not in a
bundle, and the manifest lists it as not collected.

An event in `failures/events.jsonl` carries in its sentence what was
[captured](#automatic-captures) when it happened: where a request was
at its soft timeout, where a worker's oldest request was when it was
killed, the object types a worker held most of when it was recycled for
memory. The stacks a worker dumped before it was killed are in its
release's log, under `logs/`.

The packages are read from the names of the directories in the
environment's `site-packages`, so nothing is run to list them. A
password in a URL in a lock file is masked.

### What is collected only when asked for

Two parts cost the workers something, and are collected only with their
flag:

| Flag | Adds | Costs |
| --- | --- | --- |
| `--memory` | `live/APP@RELEASE/memory.json`: each worker's memory, gc state and most common object types, as [`muructl debug memory --json`](#memory) gives them | Each worker's interpreter is held while its objects are counted |
| `--profile DURATION` | `live/APP@RELEASE/profile.collapsed`: a [profile](#profile) of every worker of the release over the period, as collapsed stacks merged across the workers, the heaviest first | The bundle takes that much longer; the releases are profiled at once, over the same period |

Each is the event the debug command raises, `debug.memory` or
`debug.profile`, as well as the bundle's own.

### The logs of the window

A log is taken from where the window starts in it. The place is found
from the times the lines carry, by halving the file, so a log of
several gigabytes is not read to find its last hour, and only the
window is read and compressed. A line without a time, as the second
line of a traceback, comes with the line before it.

An archive the rotation left is included when it was last written in
the window. A plain one gives its lines of the window. A compressed one
is included whole, as the `.gz` or `.zst` file it is, since the window
cannot be found in it without decompressing all of it; the manifest's
note on each part says which it is.

A development run and `muructl serve` write no log files, so the bundle
holds the lines of the window among those they keep in memory: the last
2000 lines of output, and the access records the run keeps.

### Scrubbing

A bundle keeps client addresses and query strings. `--scrub` takes them
out, for a bundle that has to go to someone with a narrower need:

- A client address reads as `<address1>`, `<address2>` and so on, the
  same placeholder for the same address throughout the bundle, so a
  client can still be followed. Loopback addresses stay.

- A request target or a URL loses its query string, which reads as
  `?...`.

Every string of an access log record, of an event and of a `live/` part
is scrubbed this way. A line of an application's own output is free text, in which only
what has the shape of an address, or of a target with a query, is
found. A compressed archive is decompressed to be scrubbed, and is in
the bundle under its name without the `.gz` or `.zst`. The manifest's
`left_out` says the bundle was scrubbed.

`config/` holds the configuration as it was when the bundle was made.
The manifest's `configuration_changed` says whether it changed during
the window, and `config/changes.jsonl` what changed and when. Muru keeps
only the routing snapshot in force, so the manifest lists the earlier
snapshots as not collected.

A development run and `muructl serve` have no state store, so their
`config/` holds `effective.toml`, with the settings in force and where
each came from, and no release files.

A bundle never holds secret values, the value of an environment
variable beyond those [`muructl debug inspect`](#inspect) shows, or a
request's or a response's body. A variable a release declares is given
with its value, masked as `***` when its name looks like a secret's
(`SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`, `KEY`, `CREDENTIAL` or `AUTH`
in it, or `_DSN` at its end), and a password in a URL is masked alone.
A secret from a store is named by its store and key. In the daemon's
file, a setting whose key says it is a secret (`secret`, `token`,
`password`, or a key such as `signing_key`) is masked the same way.

### The manifest

`manifest.json` is the last member of the archive:

| Field | Meaning |
| --- | --- |
| `format` | The version of the layout, `1` |
| `kind` | `bundle` |
| `created`, `muru`, `node` | When the bundle was made, Muru's version and the machine's name |
| `target` | What it was limited to, as `app` or `app@release`; empty for the whole node |
| `window` | `since_unix_ms` and `until_unix_ms`, the period it covers |
| `runs`, `running` | How Muru runs, and whether it was running |
| `configuration_changed` | Whether the configuration changed during the window; `null` when the events could not be read to tell |
| `options` | What was asked for: `scrub`, `memory`, and `profile_s`, the period profiled, `null` without |
| `recorded` | How the making of the bundle was recorded, or why it was not |
| `left_out` | What a bundle never holds, and what `--scrub` took out of this one |
| `parts` | Every part, each with its `path` and `collected`; `bytes` for one in the archive, `why` for one that is not, and a `note` where there is something to know about it |

### On a system in trouble

The command reads the daemon's files where they hold what it needs (the
state store, which it opens read-only and never changes), and asks the
daemon only for what the daemon alone knows. It runs at a lower
priority than what serves, compresses at the fastest setting, and
writes the archive as a stream, so nothing is held in memory or written
beside it.

Each question it asks has a time limit of its own. A part that cannot
be collected, because a worker hangs or the daemon does not answer, is
listed in the manifest with the reason, and the rest is collected: a
bundle with parts missing is a normal result, and the command succeeds.

With no daemon running the bundle is made from the daemon's files, and
the command takes the daemon's location options for that (`--state-dir`,
`--log-dir`, `--config`, `--run-dir` and `--system`). A running daemon
says where its files are, so no option is needed then. It works against
a development run and `muructl serve` as well, which keep no files, so
everything is asked of them.

Each bundle made of a running daemon is the event `debug.bundle`, which
says what the bundle was of, how far back it reached, and which of
`--scrub`, `--memory` and `--profile` it was made with. A development
run and `muructl serve` print the same as a line of their output. A dry
run records nothing.
