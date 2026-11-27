# Worker recycling

A worker process that has served long enough is replaced by a fresh one,
without a request lost: the host forks the replacement on a new socket,
tells its supervisor (the daemon, or `muructl run`) about it, waits until
the edge routes to the new worker, and only then tells the old one to
stop, which finishes the requests it was serving and exits. Two settings
say when:

| Setting | Default | Replaces a worker |
| --- | --- | --- |
| `max_requests` | off | after it has served this many requests |
| `max_lifetime` | off | after it has lived this long, for example `1h` |

Both carry jitter of up to ten percent, so the workers of a release do
not all go at once.

```
muructl run app:application --max-requests 1000
muructl deploy shop /srv/shop --max-requests 1000 --max-lifetime 2h
```

```toml
[tool.muru]
max_requests = 1000
max_lifetime = "2h"
```

The same keys go in a declared release for `muructl apply`. A release
records its settings when it is created, so changing them means a new
deploy.

A [worker group](worker-groups.md#timeouts-memory-and-recycling-per-group)
can set `max_requests` and `max_lifetime` of its own, so a group that
leaks is replaced more often than the rest; a group that sets neither
takes the release's.

## How many at once, and in which order

Two more settings say how far a group of workers may move from its
count while its workers are replaced:

| Setting | Default | Meaning |
| --- | --- | --- |
| `max_surge` | 1 | workers the group may run over its count |
| `max_unavailable` | 0 | workers the group may run under its count |

With the defaults one worker of a group is replaced at a time, and its
replacement is started before it stops, so the group runs one worker
over its count for the length of each replacement and is never under
it.

```
muructl deploy shop /srv/shop --max-surge 0 --max-unavailable 1
```

```toml
[tool.muru]
max_surge = 0
max_unavailable = 1
```

With `max_surge = 0` the order is the other way round: the old worker
leaves routing, finishes its requests and exits, and only then is its
replacement started. The group is one worker under its count
meanwhile, and never over it. This is the setting for a host with no
memory to spare for an extra worker, and for an application whose
worker [needs a resource to itself](exclusive-workers.md). A group of
one worker then has none for the moment of each replacement: its
requests wait at the edge, and are answered once the new worker is
ready, or with 503 at the [queue timeout](timeouts.md).

Both settings 0 is refused, since no worker could then be replaced.
Larger values replace more workers at once: `max_surge = 2` starts two
replacements together, and `max_surge = 1` with `max_unavailable = 1`
replaces one worker each way at the same time.

A replacement that nothing is forcing waits its turn: a recycle for
`max_requests`, `max_lifetime` or the [soft memory limit](memory.md),
and a [restart of every worker](#restarting-every-worker). Each
group has a queue, taken in the order the workers came due. The next
one starts as soon as the group is within `max_surge`, with its
replacement started first, or else within `max_unavailable`, with the
old worker stopped first, and otherwise waits. A worker counts as under
the group's count whenever the edge is not routing to it, whatever the
cause: one stopped ahead of its replacement, one that crashed and is
starting again, one a scale-out has just started.

A replacement that cannot wait does not: a worker past the
[hard timeout](timeouts.md), one whose event loop or interpreter has
stalled, one over its hard memory limit or chosen by the
[memory guard](containers.md#the-memory-guard), and a restart of one
worker by hand. It goes ahead whatever else is being replaced, and it
keeps the order: its replacement is started at once when `max_surge`
is 1 or more, and with `max_surge = 0` only after the old worker has
exited.

On a daemon with a [budget](scaling.md#room-for-replacing-workers) the
room for a replacement started first is held in the budget, and a group
is granted as much of its `max_surge` as fits. A release that takes no
traffic is granted none.

A worker group sets `max_surge` and `max_unavailable` of its own, and
takes the release's where it sets neither. `muructl release show`
prints them, and `muructl deploy --dry-run` lists them with where each
came from.

## What happens

Each worker reports to the host once a second: the requests it has
served, the requests in flight and the age of the oldest. When a worker
reaches its count or its lifetime, the host forks a replacement, which
listens on a socket of its own named for its generation, and announces
the new set of worker sockets to the supervisor over the host's control
socket. The daemon pushes a routing snapshot with the new socket to the
edge (and a development run reconnects its pools), acknowledges, and the
host sends the old worker SIGTERM. The old worker stops accepting,
finishes the requests it holds and exits; requests still being dispatched
to it over the edge's existing connections complete normally. If the
supervisor does not answer within five seconds, the old worker is told to
stop regardless.

A worker whose request runs past the [hard timeout](timeouts.md) takes
the same path, except that it leaves routing before its replacement is
ready and its stacks are written to the log first, and so does a worker
over its [memory limits](memory.md).

Events record each replacement: `worker.started` and `worker.ready` for
the new worker, then `worker.recycled` (or `worker.restarted`) with the
reason once the old one has gone, under the release's subject. The
message of a worker that was stopped first ends with `it stopped before
its replacement started`. A replacement that has to wait its turn is a
`worker.swap_waiting` event, once, saying what it waits for:

```
worker.swap_waiting  shop@r1  worker 1 (pid 65835) waits to be replaced (served 1030 requests (max_requests 1000)): max_surge 1 and max_unavailable 0 of the group default allow no more replacements until one under way ends
```

A
`worker.timeout_killed` event comes when the host decides to replace the
worker, with the replacement already forked, and the old worker's exit
after its drain grace is a line in the log. A worker that exits on its
own is `worker.exited`.
The [metrics](metrics.md) count them too, as `muru_worker_recycles_total`
by reason and `muru_worker_exits_total`, and `muru_worker_swaps_waiting`
is the number of replacements waiting their turn in each group. The daemon restarts a release
whose host dies, and a host that cannot start a replacement three times
running keeps the old worker and stops trying.

## Restarting a worker by hand

`muructl restart` replaces one worker of a release the way a recycle
does, for a fault the policies do not see: a broken connection pool,
bad state left in the process, or a request that hangs with the hard
timeout off. The worker is named by its index within its group, as
`muructl debug status`, `muructl status --processes` and the access
log's `worker=` field show it, with the group's name in front for a
worker of a named group (`--worker reports/1`), or by its process id:

```
muructl restart shop --worker 1
muructl restart shop@r3 --pid 48213
```

The replacement is forked on a new socket, the edge routes to it, and
only then is the old worker told to stop, so no request is lost; the
command waits until the new worker serves and the old one has gone, and
prints both pids (`--no-wait` returns as soon as the restart is under
way). With `max_surge = 0` the old worker stops first, as it does for a
recycle. A worker that is frozen is recovered the same way, since the
host acts on its own records and signals and never needs the worker to
answer; it takes about ten seconds with the default timeouts.

`--force` is for a worker that must go now: it leaves routing at once,
its threads' stacks are written to the log as they are for the hard
timeout, and it is killed a second later without being asked to stop.
The requests it was serving fail with 502, and a debugger session in it
ends. Without `--force`, a worker being debugged is left alone and the
command says so.

```
muructl restart shop --worker 1 --force
```

## Restarting every worker

```console
$ muructl restart shop --all
muru: 4 workers of shop@r3 marked to be replaced, each in its turn
muru: the restart of shop@r3 has finished
$ muructl restart shop --group reports
muru: 2 workers of the group reports of shop@r3 marked to be replaced, each in its turn
muru: the restart of shop@r3 has finished
```

`--all` replaces every worker of the release that was running when the
command was given, and `--group NAME` every worker of one
[worker group](worker-groups.md). Each takes its turn among its
group's replacements: one at a time with the default `max_surge` and
`max_unavailable`, its replacement started first, so capacity holds
and no request is lost, or as those settings say
([how many at once, and in which
order](#how-many-at-once-and-in-which-order)). Recycles that come due
meanwhile take their turns in the same queue.

The release's host takes the turns, not the command. The command
waits until every worker has been replaced, for up to ten minutes, and
`--no-wait` returns once the workers are marked. Either way the restart
carries on if the command is interrupted and while the daemon is
stopped or being upgraded. `muructl release show` says how far it has
got:

```
restart      3 of 4 workers still to be replaced, each in its turn; muructl restart --cancel leaves them
```

`muructl restart shop --cancel` ends a restart that is under way: the
workers not yet replaced stay as they are, and a replacement that has
started is finished.

A worker that is replaced another way meanwhile, by a recycle or after
a crash, or that a scale-in stops, has had its restart. A worker being
debugged is left until its session ends. Giving the command again
while a restart is under way adds the workers started since. `--force`
is for one worker, and is not taken with `--all` or `--group`.

The events are `restart.started`, a `worker.restarted` for each worker
and `restart.finished`, which says how many were replaced and, after a
cancel, how many were left. In the admin API the restart is the call
`RestartRelease`, with `CancelRestart` to end it, apart from the debug
operations, so a client allowed to restart workers needs no other
access to them.

## What a restart does and does not do

A restart resets the worker's state but loads no new code: workers fork
from the host's already loaded interpreter, so the application is as it
was when the release started. New code is a new release (`muructl
deploy`), and in development `muructl run` reloads on its own when a
source file changes. The event is `worker.restarted`, with the reason and, for
`--force`, where the stacks were; manual restarts do not count against
the churn budget. The command works against a development run too,
through the run's admin socket, which prints the event to the terminal.
