---
status: accepted
date: 2026-11-28
---

# ADR 0087: How a rollout shares capacity between its two releases

## Context and problem statement

ADR 0085 records the decisions for a canary whose capacity follows
traffic (issue 321, decisions 8 to 13): the total is the baseline's
worker count with the budget as a cap, every group is sized by the
release's share of traffic, each side with a share keeps one worker,
small totals round in the baseline's favour, a step waits for the
baseline's drain and a rollback does not, and managed HTTP server
releases are refused. Section 9.16 states them. Building it needed a
number of calls those decisions leave open: the option's name, the
arithmetic, what "room in the budget" is measured against, how the
engine knows a worker has gone, what a rollback and a promotion by hand
do, and what happens to an operator's pins. This record holds them. It
changes none of the decisions of ADR 0085.

## Decision drivers

- The rollout is one thing an operator can see and stop: its record
  says where the sharing stands, and it survives a restart of the
  daemon.

- The engine does what the procedure by hand does (issue 958), with the
  same primitives: pins, and a canary's steps.

- A node at its budget stays within it, and a rollout on default
  settings on a node sized for one copy and its swap room makes
  progress.

## Considered options

- Sizing through pins of the rollout's own, kept as pins are, with the
  phase of a step in the rollout's record.

- A new rollout state for a step whose workers are being moved.

- Sizing through the scalers' floors and caps (section 7.5), as a
  schedule does.

## Decision outcome

Chosen option: "Pins of the rollout's own, and the phase in the
record", because a pin already passes the slow scale-in rules, outlives
the daemon and shows in `muructl scale status`, and a flag in the
record leaves the rollout's states, which the API, the dashboard and
the export all name, as they are. Floors and caps go through the
autoscaler's windows and the churn budget, at the pace of a schedule
and not of a rollout.

### Details fixed by the implementation

- **The option is `--capacity`,** with `shared` and the default `own`,
  each release at its own size. `StartRolloutRequest.capacity` carries
  it, and a `Rollout` says `capacity`, `totals`, `baseline_workers`,
  `candidate_workers` and `sizing`.

- **The arithmetic** (`shared::sizes`). At a share of p percent of a
  total T, the candidate has `floor(T * p / 100)` workers and the
  baseline `T` less that, each at least 1 while it has any share. This
  is decisions 9 and 10 as one rule: 4 gives 3 and 1, 2 and 2, 1 and 3;
  2 gives 2 and 1 until half; 1 gives 1 and 1.

- **The total is counted when the rollout starts,** for each group of
  the baseline that has workers: the larger of the workers the edge
  routes to and the count the group is being brought to, so a pin an
  operator set just before is taken. A group at zero is left to its own
  policy. A group only the candidate has is not sized, and a group only
  the baseline has shrinks by its share with nothing growing against
  it.

- **A step has two phases, and the record says which** (`sizing` in
  `SharedCapacity`). When a step begins, its workers are put in place
  with the step before's share still in force (`Rollout::percent`), so
  the first step begins with the candidate at no traffic. When every
  group of the candidate has its count, ready, the traffic moves, the
  step's hold and its gates' window start, and the baseline is pinned
  down to its counts. The gates judge nothing while workers are being
  placed, since no traffic of the step has been served.

- **Room is measured without the room held for swaps.** The workers a
  candidate gains are admitted against the budget over what the
  releases hold without their surge allowances (section 7.9), which
  give way to them as they do to an operator's pin. Counting the
  allowances would leave a node sized for one copy and its swap room,
  the node this is for, with no room ever and every step in the
  drain-first order.

- **With room the candidate grows by as many workers as fit,** and by
  the rest as room appears. **Without, the baseline gives up its own
  `max_unavailable` workers at a time** ahead of the traffic
  (`shared::plan`): they exit, the candidate takes the room, its
  workers become ready, and the next exchange begins. The baseline is
  never taken below the step's count that way. With `max_unavailable =
  0` the step waits and says so: that setting says the group gives up
  no worker ahead of its traffic.

- **The budget lowers the total.** When the baseline is down to the
  step's count and there is still no room, the candidate takes the
  step's traffic with the workers it has, and a `rollout.capacity`
  warning says how short it is. Waiting would wait for ever.

- **A worker that was given up is gone when the host says so.** The
  engine counts the baseline's `worker.exited` events with the reason
  `scaled_in` (the counter of section 11.1) and waits for as many as it
  asked for, for no longer than the release's stop budget and five
  seconds, which is the "drain grace" of decision 12: past it the host
  has killed the worker. The wait is not kept through a restart of the
  daemon, which then goes on at once.

- **The promotion is a last move of workers.** When the last step
  passes, the candidate is grown to the whole total with the last
  step's share in force (`finishing`), and promoted when those workers
  are ready; the baseline keeps one worker until then, since it still
  has traffic. Where the budget has no room the candidate is promoted
  short, as above.

- **A rollback** pins the baseline up toward the total, as far as the
  budget has room, records the rollout rolled back, gives the edge the
  snapshot and drains the candidate to its inspection floor, in that
  order and in one go. The rest of the baseline's count comes from its
  own policy once the pins are lifted and the candidate's workers have
  gone.

- **`muructl rollout promote`** grows the candidate toward the total as
  far as the budget has room and promotes it at once: a promotion by
  hand is an operator saying now.

- **The pins are the rollout's.** They are kept as an operator's are,
  so a restarted daemon restores them, and `muructl scale --processes`
  and `--unpin` are refused for the two releases while the rollout is
  active. When it ends they are all lifted, one an operator set before
  included, and the release that serves is sized by its count or
  policy; with `--initial-processes` (ADR 0086) that is its declared
  count.

- **A scaled group of the candidate is never pinned over its
  ceiling,** its `max_workers` being the most its policy allows.

- **A candidate over its share is brought down to it** when a step
  begins (`Move::Trim`): one deployed held at its own size has its
  share of the total, as decision 11 says of every group of both
  releases.

- **One event kind, `rollout.capacity`,** for each move of workers, at
  notice level, and at warning level when the candidate goes on short.

### Consequences

- Good: an upgrade on a node sized for one copy is one command, within
  the budget, and stops where it is when the daemon does.

- Good: the procedure by hand and the engine are the same moves.

- Bad: while a step's workers are placed, its gates do not run, so a
  candidate that fails only then is caught at the next step's judging.

- Bad: a step on a node with no room takes the baseline under its share
  ahead of the traffic, by as much as the step moves.

- Bad: an operator's pin on the baseline does not come back when the
  rollout ends.

## More information

- Issue 321, and issue 962 for the design. ADR 0085 holds the
  decisions, and ADR 0086 the count a release starts with.

- Section 9.16, and sections 7.4 (pins), 7.9 (the surge allowance) and
  9.3 (rollouts).
