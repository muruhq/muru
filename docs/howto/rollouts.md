# Rollouts

A deploy moves traffic to the new release as soon as it is ready. A
rollout does it in stages: the release is started without traffic, gets a
share of the application's requests step by step, is judged at each step
against the release that still serves, and is promoted when every step
passes or rolled back when one fails. The shares are applied on the
application's routes (see [virtual hosts](virtual-hosts.md)), or, on a
daemon with no host declared, to every request, which go to the one
application serving (see [virtual hosts](virtual-hosts.md)).

## Holding a release

```console
$ muructl deploy shop /srv/shop --hold
muru: release shop@r2 is ready and held: not serving
muru: muructl preview shop@r2 opens a preview of it
```

With `--hold` the release is started and left running without traffic.
It is checked through a preview, which routes a request to it whatever
the traffic policy says:

```console
$ muructl preview shop@r2
muru: opened https://shop.example.com/ in preview of shop@r2
$ curl -H "$(muructl preview shop@r2 --header)" https://shop.example.com/
```

`muructl preview` opens a one-time entry link in the browser, which
gives it the preview cookie, and `--header` prints the `Muru-Preview`
header for curl or a test client. Both carry the release's preview
secret, so the release's ID alone reaches nothing (see [traffic between
releases](traffic.md#previews)). `muructl deploy --hold --open-preview`
opens the link as soon as the release is ready.

A held release stays until a rollout or a promotion takes it, or
`muructl drain` stops it. On a node whose
[budget](scaling.md#releases-and-the-budget) has no room for it at full
size, `--initial-processes N` starts it with fewer workers, which it
keeps until it takes traffic.

## Blue/green

```console
$ muructl rollout bluegreen shop@r2
muru: release shop@r2 is ready and serving
```

All of the application's traffic moves to the release at once, and the
release that served drains and is kept warm, as after a deploy. This is
the deploy's own strategy, available separately so a held release can be
checked first. While a canary of the application is under way, a
promotion, from the command, the dashboard or the API, is refused until
`muructl rollout promote` or `muructl rollout abort` ends the canary.

## Canary

```console
$ muructl rollout canary shop@r2 --steps 10,50 --hold 5m --wait
muru: rollout 1 of shop@r2 started against r1: step 1 of 2: 10% for 5m
muru: rollout 1 running step 2 of 2: 50% for 5m
muru: rollout 1 promoted: every step passed its gates
```

The candidate takes each step's share of the application's requests in
turn, with clients spread as a traffic policy spreads them: hashed to
stable buckets by the anonymous id cookie the edge issues, so a client in
the canary at 10% is still in it at 50%. Each step holds for `--hold`,
and when the hold has passed with the gates satisfied the next step
begins. When the last step passes, the candidate is promoted: it takes all
the traffic, and the baseline drains and is kept warm. Without `--wait`
the command returns at once and `muructl rollout status` follows the
rollout.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--steps` | `10,50` | The candidate's shares, in percent, increasing |
| `--hold` | `2m` | How long each step lasts before its gates decide |
| `--max-error-rate` | `1` | The candidate's 5xx rate over the step, in percent |
| `--max-p95` | none | The candidate's 95th percentile request time over the step |
| `--max-p99` | none | The candidate's 99th percentile request time over the step |
| `--max-worker-faults` | `0` | The candidate's worker crashes and timeout kills over the step |
| `--max-queue-wait` | none | The candidate's 95th percentile wait for a worker thread over the step |
| `--probe` | none | A URL requested from the candidate and the baseline every 5 seconds; repeatable |
| `--min-requests` | `100` | Requests the candidate must serve in a step before the gates judge |
| `--capacity` | `own` | `own`: each release runs at its own size. `shared`: the two share the baseline's worker count, each sized by its share of the traffic |
| `--approve-each-step` | off | Wait for `rollout resume` before each step after the first |

## Gates

A step is judged on the requests served under it alone, counted from
the moment its share took effect, and the candidate is compared with the
baseline over the same window. A gate fails only when the candidate is
both over its limit and worse than the baseline, so a fault both releases
share, such as a database outage, does not roll the candidate back. A
baseline with fewer than `--min-requests` requests in the window is not
compared, and the limits alone apply.

The gates are checked every second once the candidate has served
`--min-requests` requests in the step. A failing gate rolls back at once,
without waiting for the hold to end: the candidate leaves traffic and is
kept for 10 minutes, so its logs and workers can be looked at, and the
rollout records why. `muructl rollout abort` keeps it the same way. The
candidate is kept beside the application's retained release, not in its
place, so the release that served before the deploy stays warm for an
instant rollback; `muructl release list` notes the candidate as kept for
inspection and not a rollback target:

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           rolled_back        0%       gate failed at 10%: 5xx rate 4.00% over the limit of 1.00% (baseline 0.00%)
```

A step whose hold has passed with too few requests to judge holds on
until there are enough, saying so in its message. A quiet application can
be promoted by hand instead.

A worker that crashes, or that the [hard timeout](timeouts.md#the-hard-timeout)
or the loop stall timeout replaces, counts against its release at once,
whatever the step's request count: by default one more than the baseline
had over the step rolls the candidate back.

A probe is a URL the daemon requests through the edge every 5 seconds
while the rollout runs, once from the candidate and once from the
baseline, each as a preview of that release, which the daemon sends
with the release's preview secret. A probe passes
with a 2xx or 3xx answer within 5 seconds. Like a crash, a failed probe
counts at once: one more failure on the candidate than on the baseline
over the step rolls it back, so a health check failing on both
releases, as during a database outage, does not. Give the URL of
a listener the edge serves the application's route on:

```console
$ muructl rollout canary shop@r2 --probe http://127.0.0.1:8080/health --probe http://127.0.0.1:8080/cart
```

Probe requests count as requests of their release, as any other request
does.

## Sharing capacity

```console
$ muructl rollout canary shop@r2 --steps 25,50,75 --capacity shared
muru: rollout 1 of shop@r2 started against r1: step 1 of 3: 25% for 2m, on a shared total of 4 workers
```

With `--capacity shared` the baseline and the candidate share one total
of workers, in proportion to each step's traffic, where a canary
otherwise runs each release at its own size. It is for a node whose
[budget](scaling.md#the-nodes-budget) has no room for two full copies of
the application, and works the same on a node with no budget.

**The total** is the number of workers the baseline has when the
rollout starts, for each [worker group](worker-groups.md) by itself.
At a step that gives the candidate a share of the traffic, the
candidate has that share of the total, rounded down, and the baseline
the rest. With a total of 4:

| Share of the candidate | Baseline | Candidate |
| --- | --- | --- |
| 25% | 3 | 1 |
| 50% | 2 | 2 |
| 75% | 1 | 3 |
| promoted | kept warm | 4 |

**Each side keeps one worker** in each group while it takes any
traffic, so a share too small to earn a whole worker still has one: a
total of 4 at 10% runs 4 and 1, and a total of 2 runs 2 and 1 until the
candidate's share reaches half. A group of one runs one worker of each
release for the length of the rollout.

**A step moves the workers before and after the traffic.** The
candidate is grown to its count, and once those workers are ready the
traffic moves; then the baseline is drained to its count. A candidate
that has more workers than its share, having been deployed at its own
size, is drained to its share first. The workers
the candidate gains are checked against the node's budget, with the
room held for [replacing workers](scaling.md#room-for-replacing-workers)
giving way to them. When the budget has no room for them, the order is
the other way: the baseline gives up workers first and the candidate
gains them, as many at a time as the baseline group's
[`max_unavailable`](recycling.md#how-many-at-once-and-in-which-order),
and then the traffic moves. With `max_unavailable = 0`, which lets the
baseline give up no worker ahead of its traffic, such a step waits for
room and its message says so. When the baseline is down to its count
and the budget still has no room for the candidate's, the candidate
takes the step's traffic with the workers it has.

**A step lasts until the baseline has drained.** The gates judge the
step over its hold as they do in any canary, and the next step does not
begin before the workers the baseline gave up have exited, or their
[shutdown timeout](timeouts.md) has passed.

**The last step passing** grows the candidate to the whole total, and
once those workers are ready promotes it. **A rollback**, by a failing
gate or `muructl rollout abort`, does not wait: the baseline is grown
back as far as the budget lets it, takes all the traffic at once, and
the candidate drains to the one worker it is kept with.

The rollout sizes the two releases with pins of its own, so
`muructl scale status` shows each group as pinned while it runs, and
`muructl scale --processes` and `--unpin` are refused for the two
releases until it ends. When it ends, either way, the pins are lifted
and the release that serves is sized by its own count or scaling policy
again: a candidate deployed with
[`--initial-processes`](scaling.md#releases-and-the-budget) takes the
count it is declared with. `muructl rollout promote` grows the
candidate to the total as far as the budget lets it and promotes it at
once, without waiting for those workers.

Each move of workers is a `rollout.capacity` event. A release that runs
a [managed HTTP server](managed-servers.md) is refused, as baseline or
as candidate: its instances are not scaled. An ordinary canary still
works for it.

## Controlling a rollout

| Command | Effect |
| --- | --- |
| `muructl rollout status [APP] [--wait]` | The rollouts, or one application's; with `--wait`, follow the application's rollout until it ends |
| `muructl rollout pause APP` | Hold the current step and its share; the gates stop judging |
| `muructl rollout resume APP` | Continue a paused rollout, or approve a step that awaits approval; the step's hold and window start afresh |
| `muructl rollout promote APP` | Give the candidate all the traffic now and end the rollout |
| `muructl rollout abort APP` | Take the candidate out of traffic now, keep it warm and end the rollout |

With `--approve-each-step`, the rollout stops before each step after the
first with the state `awaiting_approval` and the previous step's share
still in force, until `rollout resume` approves it.

A rollout runs two releases at once. An application that can run only
one copy is deployed by hand instead: see [a worker that needs a
resource to itself](exclusive-workers.md).

A canary runs the candidate beside the baseline at whatever size each
has. On a node whose budget cannot hold two full copies of the
application, `--capacity shared` has the two share one total of
workers: see [sharing capacity](#sharing-capacity).

A rollout is not part of the configuration `muructl deployment export`
writes. `deployment export --resolve-rollouts` writes it as where it stands:
the candidate declared beside the serving release, and the
application's routes splitting traffic between them at the candidate's
current share, which a file in version control can keep.

An application has at most one rollout under way. A rollout's candidate
is kept running while the rollout lasts, a deploy or rollback that would
drain it leaves it in place, and `muructl deployment export` does not include
its share, which is live state rather than declared configuration.

## Deploy freezes

While an overlay or a [schedule](scaling.md#scheduled-capacity) with
`freeze_deploys = true` is active for an application, its deploys and
rollouts are refused: `muructl deploy`, `muructl apply` when it would
deploy a release of the application, `muructl rollout canary`,
`bluegreen` and `promote`. A rollout already under way holds its step
until the freeze ends. Rolling back, pausing and aborting are never
frozen.

Each of those commands takes `--override-freeze` to go ahead anyway,
which is recorded as a `deploy.freeze_overridden` event at warning level,
saying what was done and which overlay froze it.

## Events

Every change is an event: `rollout.started`, `rollout.step` for each new
step or one awaiting approval, `rollout.capacity` for each move of
workers when [capacity is shared](#sharing-capacity), `rollout.paused` and `rollout.resumed`,
`rollout.gate_failed` with the gate and the numbers, `rollout.completed`
when the candidate is promoted and `rollout.rolled_back` when it is taken
out, by a gate or by hand.
