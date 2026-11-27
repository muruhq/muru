# Upgrading in place on a host with limited memory

A deploy starts the new release beside the one that serves, so for a
while the node holds two copies of the application. On a node whose
[budget](scaling.md#the-nodes-budget) has room for one copy and a
little more, a deploy of the new release at full size is refused:

```console
$ muructl deploy shop /srv/shop-2
muru: error: not started: shop: the release needs about 812MB and the node's memory budget of 1.3GB has 1012MB held
```

The upgrade is then done in steps: the new release starts held with one
worker, and a [canary that shares
capacity](rollouts.md#sharing-capacity) moves the traffic across a
share at a time, with the new release growing and the old one shrinking
at each step. The node stays within its budget throughout, and both
releases keep enough workers for the share of requests they are taking.

The example is an application `shop` whose project sets `processes = 4`,
so every release of it is declared with 4 workers. Each worker is
counted at 200MB, and the node has a memory budget of 1.3GB: room for 6
workers and the hosts of two releases. The serving release `r1` holds 5
of the 6, its 4 workers and the
[room to replace one](scaling.md#room-for-replacing-workers):

```console
$ muructl daemon status
...
budget   1012MB of 1.3GB memory, 1 worker held for swaps
```

The 12MB over is the release's host, which counts at its own memory
where that is measured, on Linux. Leave room for a second host when
working out what fits.

## Choose a quiet time

The upgrade runs the application on fewer workers than it has at its
peak: partway through, each release has half of them. Pick a time when
the application needs no more than that. `muructl scale status shop`
shows how many of the release's slots are busy, and
`muru_capacity_utilisation` in the [metrics](metrics.md) shows it over
time.

## Start the new release held, with one worker

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
host, which fit beside the serving release. The release is still
declared with its 4 workers, and it stays at one for as long as it
takes no traffic. Check it through a
[preview](rollouts.md#holding-a-release) before going on.

When even one worker does not fit, make the room first by pinning the
serving release one worker down, `muructl scale shop --processes 3`,
and wait for `muructl daemon status` to show it. The rollout then
shares the 3 workers it finds, and the new release still ends at the 4
it is declared with.

A release with [worker groups](worker-groups.md) has a count for each
group: `--initial-processes NAME=N` gives one group's.

## Roll out, sharing the serving release's workers

```console
$ muructl rollout canary shop@r2 --steps 25,50,75 --capacity shared --wait
muru: rollout 1 of shop@r2 started against r1: step 1 of 3: 25% for 2m, on a shared total of 4 workers
muru: rollout 1 running step 1 of 3: 25% for 2m, on a shared total of 4 workers
muru: rollout 1 running step 1 of 3: 25% for 2m
muru: rollout 1 running step 2 of 3: 50% for 2m: putting its workers in place
muru: rollout 1 running step 2 of 3: 50% for 2m
muru: rollout 1 running step 3 of 3: 75% for 2m: putting its workers in place
muru: rollout 1 running step 3 of 3: 75% for 2m
muru: rollout 1 running every step passed its gates: putting its workers in place
muru: rollout 1 promoted: every step passed its gates
```

`--capacity shared` has the two releases share the 4 workers the
serving release had when the rollout started, each sized by its share
of the traffic. Choose steps that match whole workers: with 4 workers,
steps of 25, 50 and 75 percent are one worker each.

| Step | Share of `r2` | `r1` workers | `r2` workers |
| --- | --- | --- | --- |
| 1 | 25% | 3 | 1 |
| 2 | 50% | 2 | 2 |
| 3 | 75% | 1 | 3 |
| promoted | 100% | 1, kept warm | 4 |

At each step the new release gets its workers before its share of the
requests grows, so that they do not queue for a release too small for
them, and the old release gives up a worker only after its share has
gone down. The [events](events.md) say each move:

```console
$ muructl events
...
rollout.started      shop@r2  rollout 1 against r1: 25% for 2m, 50% for 2m, 75% for 2m, on a shared total of 4 workers
rollout.capacity     shop@r2  r1 default: drained from 4 to 3 workers, its share with r2 at 25%
rollout.step         shop@r2  step 2 of 3: 50% for 2m
rollout.capacity     shop@r2  r2 default: grown from 1 to 2 workers, its share at 50%, ahead of the traffic
rollout.capacity     shop@r2  r1 default: drained from 3 to 2 workers, its share with r2 at 50%
rollout.step         shop@r2  step 3 of 3: 75% for 2m
rollout.capacity     shop@r2  r2 default: grown from 2 to 3 workers, its share at 75%, ahead of the traffic
rollout.capacity     shop@r2  r1 default: drained from 2 to 1 worker, its share with r2 at 75%
rollout.capacity     shop@r2  r2 default: grown from 3 to 4 workers, the whole total, ahead of its promotion
traffic.switched     shop@r2  traffic moved to the release
rollout.completed    shop@r2  rollout 1: every step passed its gates
```

When the last step passes its gates the new release is grown to the
whole 4 and promoted: it takes all the traffic, and the old release
drains and is [kept warm](daemon.md#releases) with one worker for each
group, for a rollback. The rollout's pins are lifted, and the new
release is at the count it is declared with:

```console
$ muructl scale status shop
shop@r2
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      4            1/12       3.67    under 1 ms    -          static
  last: unpinned (8s ago)
```

The workers the new release gains at each step are checked against the
budget. On a node with no room for them until the old release has given
some up, the old release goes first, as many workers at a time as its
[`max_unavailable`](recycling.md#how-many-at-once-and-in-which-order)
allows; with the default of 0 such a step waits, and `muructl rollout
status` says what for. [Sharing capacity](rollouts.md#sharing-capacity)
has the rules in full.

### When a step fails

A failing gate, or `muructl rollout abort shop`, grows the old release
back, gives it all the traffic at once and drains the new one:

```console
$ muructl rollout abort shop
muru: rollout 1 rolled_back: aborted by hand at 25%
$ muructl events
...
rollout.capacity     shop@r2  r1 default: grown back from 3 to 4 workers of its 4, before it takes the traffic again
rollout.rolled_back  shop@r2  rollout 1: aborted by hand at 25%; the candidate is kept for inspection
```

The new release leaves traffic, drops to one worker for each group and
is kept for 10 minutes so that its logs and workers can be looked at.
The old release grows back as far as the budget has room, and the rest
of the way as the new release's workers go. Requests that arrive while
it is still small wait in its queue, and are answered 503 at the
[queue timeout](timeouts.md) when it cannot keep up. This is the reason
for the quiet time.

## What the rollout does, by hand

The same upgrade can be done a step at a time with
[pins](scaling.md#watching-and-overriding) and a canary that waits for
an approval at each step. It is what `--capacity shared` does, and the
way to do it where the counts want deciding by a person.

A release that takes traffic grows to the count it is declared with, as
far as the budget lets it. Here the workers move when you say, so pin
the new release where it is before it is given any, and start the
canary:

```console
$ muructl scale shop@r2 --processes 1
muru: shop@r2 default pinned to 1 worker
$ muructl rollout canary shop@r2 --steps 25,50,75 --approve-each-step
muru: rollout 1 of shop@r2 started against r1: step 1 of 3: 25% for 2m
$ muructl scale shop@r1 --processes 3
muru: shop@r1 default pinned to 3 workers
```

The first step needs no more workers: `r2` takes a quarter of the
requests on its one worker, and `r1` gives one up. With
`--approve-each-step` the rollout stops before each step after the
first until `muructl rollout resume` approves it, which is when the
workers are moved. When the step's hold has passed with its
[gates](rollouts.md#gates) satisfied, the rollout waits:

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           awaiting_approval  25%      step 2 of 3: 50% for 2m awaits approval: muructl rollout resume shop
```

At each step, grow the new release, move the traffic, then shrink the
old one:

```console
$ muructl scale shop@r2 --processes 2
muru: shop@r2 default pinned to 2 workers
$ muructl scale status shop@r2
shop@r2
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      2 pinned     1/6        1.67    under 1 ms    -          static
  last: pinned to 2 (2s ago)
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
room for it. The room held for replacing workers gives way to a pin:
while both releases take traffic each is granted room to replace one
worker when there is some, and `muructl daemon status` says how much is
held.

| Step | Share of `r2` | `r1` workers | `r2` workers | Before approving | After approving |
| --- | --- | --- | --- | --- | --- |
| 1 | 25% | 3 | 1 | | `scale shop@r1 --processes 3` |
| 2 | 50% | 2 | 2 | `scale shop@r2 --processes 2` | `scale shop@r1 --processes 2` |
| 3 | 75% | 1 | 3 | `scale shop@r2 --processes 3` | `scale shop@r1 --processes 1` |

When the last step passes its gates the new release is promoted, and
lifting its pin is all that is left to do:

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           promoted           100%     every step passed its gates
$ muructl scale shop --unpin
muru: shop@r2 default unpinned; the autoscaler decides again
$ muructl scale status shop
shop@r2
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      4            0/12       4.00    under 1 ms    -          static
  last: scaled out to 4: back toward the static count of 4 (5s ago)
```

The new release serves, so it takes the 4 workers it is declared with,
beside the one worker the old release keeps, and nothing holds it there
but its own count. An application with a [scaling policy](scaling.md)
is sized by the policy from then on. The workers are added as the
budget admits them; when they do not all fit, a
`scaling.denied_by_budget` event says so, and `muructl drain shop@r1`
stops the old release to make the room, at the cost of starting it
again for a rollback.

A failing gate, or `muructl rollout abort shop`, gives all the traffic
back to the old release at once, at whatever size it is pinned to at
that moment. Lift its pin straight away, and it grows back to the 4
workers it is declared with, as the budget admits them:

```console
$ muructl rollout abort shop
muru: rollout 1 rolled_back: aborted by hand at 50%
$ muructl scale shop --unpin
muru: shop@r1 default unpinned; the autoscaler decides again
```

The one worker the new release keeps for inspection is within what the
node has room for. On a node with less room, stop it first with
`muructl drain shop@r2`, at the cost of what it would have shown.
