# Upgrading in place on a host with limited memory

A deploy starts the new release beside the one that serves, so for a
while the node holds two copies of the application. On a node whose
[budget](scaling.md#the-nodes-budget) has room for one copy and a
little more, a deploy of the new release at full size is refused:

```console
$ muructl deploy shop /srv/shop-2
muru: error: not started: shop: the release needs about 1000MB and the node's memory budget of 1.3GB has 1000MB held
```

The upgrade is then done in steps, by hand: the serving release gives up
a worker, the new release starts held with one worker, and a
[canary](rollouts.md#canary) moves the traffic across a share at a time,
with the new release growing and the old one shrinking at each step.
The node stays within its budget throughout, and both releases keep
enough workers for the share of requests they are taking.

The example is an application `shop` whose project sets `processes = 4`,
so every release of it is declared with 4 workers. Each worker is
counted at 200MB, and the node has a memory budget of 1.3GB: room for 6
workers and the hosts of two releases. The serving release `r1` holds 5
of the 6, its 4 workers and the
[room to replace one](scaling.md#room-for-replacing-workers):

```console
$ muructl daemon status
...
budget   1000MB of 1.3GB memory, 1 worker held for swaps
```

On Linux each release's host counts at its own memory as well, so leave
room for a second host when working out what fits.

## Choose a quiet time

The procedure runs the application on fewer workers than it has at its
peak: partway through, each release has half of them. Pick a time when
the application needs no more than that. `muructl scale status shop`
shows how many of the release's slots are busy, and
`muru_capacity_utilisation` in the [metrics](metrics.md) shows it over
time.

## Make room and start the new release held

A [pin](scaling.md#watching-and-overriding) sets a release's worker
count, whatever its scaling policy says. Pin the serving release one
worker down, and wait for the budget to show it:

```console
$ muructl scale shop --processes 3
muru: shop@r1 default pinned to 3 workers
$ muructl daemon status
...
budget   800MB of 1.3GB memory, 1 worker held for swaps
```

Then deploy the new release held, starting with one worker:

```console
$ muructl deploy shop /srv/shop-2 --hold --initial-processes 1
muru: release shop@r2 is ready and held: not serving
muru: muructl preview shop@r2 opens a preview of it
$ muructl release show shop@r2
...
workers      4 processes of 3 threads; 1 to start with and while it takes no traffic
```

`--hold` starts the release and leaves the traffic where it is.
`--initial-processes 1` is the count the release starts with and the
count the budget checks the deploy at: one worker and the release's
host, which fit in the room made. The release is still declared with
its 4 workers, and it stays at one for as long as it takes no traffic.
Check it through a [preview](rollouts.md#holding-a-release) before
going on.

A release that takes traffic grows to the count it is declared with, as
far as the budget lets it. In this procedure the workers move when you
say, so pin the new release where it is before it is given any:

```console
$ muructl scale shop@r2 --processes 1
muru: shop@r2 default pinned to 1 worker
```

A release with [worker groups](worker-groups.md) has a count for each
group: `--initial-processes NAME=N` gives one group's, and each step
below is done for each group, with `--group NAME` on `muructl scale`.

## Start the canary with an approval at each step

```console
$ muructl rollout canary shop@r2 --steps 25,50,75 --approve-each-step
muru: rollout 1 of shop@r2 started against r1: step 1 of 3: 25% for 2m
```

With `--approve-each-step` the rollout stops before each step after the
first until `muructl rollout resume` approves it, which is when the
workers are moved. Choose steps that match whole workers: with 4
workers, steps of 25, 50 and 75 percent are one worker each.

The first step needs no change: `r2` takes a quarter of the requests on
its one worker, and `r1` has three for the rest. When the step's hold
has passed with its [gates](rollouts.md#gates) satisfied, the rollout
waits:

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           awaiting_approval  25%      step 2 of 3: 50% for 2m awaits approval: muructl rollout resume shop
```

## At each step: grow the new release, move the traffic, shrink the old one

The order matters. The new release gets its workers before its share
of the requests grows, so that they do not queue for a release too
small for them, and the old release gives up a worker only after its
share has gone down.

```console
$ muructl scale shop@r2 --processes 2
muru: shop@r2 default pinned to 2 workers
$ muructl scale status shop@r2
shop@r2
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      2 pinned     0/6        2.00    -             -          static
$ muructl rollout resume shop
muru: rollout 1 running: step 2 of 3: 50% for 2m
$ muructl scale shop@r1 --processes 2
muru: shop@r1 default pinned to 2 workers
```

Wait for `scale status` to show the new release's workers before
approving the step: a worker is counted there once the edge routes to
it.

A pin is not limited by the budget, so the arithmetic is yours. Before
each pin that adds a worker, check that `muructl daemon status` shows
room for it. In the example the node holds 5 workers for the moment
between growing `r2` and shrinking `r1`, which is the room that
pinning `r1` down at the start made. The room held for replacing
workers gives way to a pin: while both releases take traffic each is
granted room to replace one worker when there is some, and
`muructl daemon status` says how much is held.

| Step | Share of `r2` | `r1` workers | `r2` workers | Before approving | After approving |
| --- | --- | --- | --- | --- | --- |
| 1 | 25% | 3 | 1 | | |
| 2 | 50% | 2 | 2 | `scale shop@r2 --processes 2` | `scale shop@r1 --processes 2` |
| 3 | 75% | 1 | 3 | `scale shop@r2 --processes 3` | `scale shop@r1 --processes 1` |

## Promotion

When the last step passes its gates the new release is promoted: it
takes all the traffic, and the old release drains and is
[kept warm](daemon.md#releases) with one worker for each group, for a
rollback.

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           promoted           100%     every step passed its gates
$ muructl scale shop --unpin
muru: shop@r2 default unpinned; the autoscaler decides again
$ muructl scale status shop
shop@r2
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      4            3/12       3.00    under 1 ms    -          static
  last: scaled out to 4: back toward the static count of 4 (7s ago)
```

Lifting the pin is all that is left to do: the new release serves, so
it takes the 4 workers it is declared with, beside the one worker the
old release keeps, and nothing holds it there but its own count. An
application with a [scaling policy](scaling.md) is sized by the policy
from then on. The workers are added as the budget admits them; when
they do not all fit, a `scaling.denied_by_budget` event says so, and
`muructl drain shop@r1` stops the old release to make the room, at the
cost of starting it again for a rollback.

## When a step fails

A failing gate, or `muructl rollout abort shop`, gives all the traffic
back to the old release at once, at whatever size it is pinned to at
that moment:

```console
$ muructl rollout abort shop
muru: rollout 1 rolled_back: aborted by hand at 50%
$ muructl scale status shop
shop@r1
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      2 pinned     0/6        2.00    -             -          static
```

The new release leaves traffic, drops to one worker for each group and
is kept for 10 minutes so that its logs and workers can be looked at.
Bring the old release back to full size straight away, by lifting its
pin:

```console
$ muructl scale shop --unpin
muru: shop@r1 default unpinned; the autoscaler decides again
```

It grows back to the 4 workers it is declared with, as the budget
admits them. The one worker the new release keeps is the
fifth the node has room for. On a node with less room, stop it first
with `muructl drain shop@r2`, at the cost of what it would have shown.

Requests that arrive while the old release is still small wait in its
queue, and are answered 503 at the
[queue timeout](timeouts.md) when it cannot keep up, so the sooner the
pin is lifted, the fewer of them. This is the reason for the quiet
time.
