---
status: accepted
date: 2026-11-16
---

# ADR 0065: An overlay's cap wins over floors and drains the excess at once

## Context and problem statement

Overlays and schedules could raise a worker group's floor and ceiling
but not lower its ceiling (section 7.5). Issue 712 adds `cap_workers`,
for back-pressure on a struggling dependency and for making room on a
node, and leaves three points open: what happens when a floor and a cap
conflict, how a group above a new cap gets under it, and what overlay
`muructl scale --cap` makes.

## Decision drivers

- A cap is set to protect something, a database or the room another
  application needs, and has to act exactly when the floor or the
  autoscaler would keep the workers.

- The deploy case, making room for another application's new release,
  needs the cap in force before the deploy starts.

- A floor and a cap set by hand for one group should not undo each
  other when one of them is changed or ends.

## Considered options

- Floor against cap: the floor wins (a floor is a promise of capacity),
  or the cap wins with `muructl apply` warning of pairs that can
  conflict.

- Getting under a cap: one worker per reap interval, as scale-in, or
  draining the excess at once.

- The temporary overlay: share `APP:scale` with `--min`, so the two
  together make one floor and cap, or an overlay of its own, `APP:cap`.

## Decision outcome

- The cap wins over every floor: a schedule's, an overlay's, the
  policy's `min_workers` and a static group's count. A floor that won
  would leave a back-pressure or make-room cap doing nothing at the
  moment it is wanted. `muructl apply` warns of a declared floor above
  another overlay's or schedule's cap on the same group. Within one
  overlay, a cap below its own floor, or beside `max_workers`, is
  refused.

- A group above a new cap drains the excess at once, as one
  `scaling.decision`, and these drains are not counted by the churn
  budget, which is there to stop the autoscaler flapping. One worker
  per reap interval could take many minutes, too long for a cap set
  ahead of a deploy. `muructl scale --cap` waits until the group is at
  or under the cap, unless `--no-wait` is given.

- `muructl scale --cap` makes its own overlay, `APP:cap` or
  `APP:cap:GROUP`, so a floor and a cap have independent lifetimes and
  `muructl overlay end` ends one of them.

- A pin stands over a cap, as it does over a floor and the node's
  budget, since a pin is an operator fixing the count; the cap applies
  once the group is unpinned. A canary takes its share of the cap, at
  least one worker, as it takes its share of a floor. A warm-up's lead
  applies the floors but not the cap.

### Consequences

- Good: a cap does what it says whatever else is active, and a cap set
  by hand is in force by the time `muructl scale --cap` returns.

- Good: the floor and the cap set by hand can be changed or ended one
  at a time.

- Bad: a schedule's floor, a promise of capacity, is broken while a cap
  below it is active; the apply warning is the only notice ahead of
  time.

- Bad: draining several workers at once drops capacity in a step, so a
  cap set too low turns into queueing and error pages at once rather
  than gradually.

## More information

Issue 712, and issue 711 for capacity on a condition in the scaling
guide. Specification section 7.5 ("Caps") and section 9.10.
