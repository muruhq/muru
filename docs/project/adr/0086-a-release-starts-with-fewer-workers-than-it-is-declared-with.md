---
status: accepted
date: 2026-11-27
---

# ADR 0086: A release starts with fewer workers than it is declared with

## Context and problem statement

The budget check of a new release (section 13.6) counts it at its own
`processes`, so on a node sized for one copy of an application the new
release of an upgrade by hand had to be deployed with `--processes 1`
(issue 958). A release is immutable, so that 1 was its count for good:
after its promotion only a pin held it at full size, and
`muructl scale --unpin` brought a static release back to one worker
(issue 964). The question is how a release says both the count it runs
at and the count it starts at, and when it moves from one to the other.

The design was decided on 2026-11-27 with the rest of issue 962: a
deploy flag `--initial-processes`, checked by the budget, kept while
the release has no share of traffic, and left for the declared count or
the scaling policy once it is pinned or serves. This record holds the
calls the work had to make within that.

## Decision drivers

- A release is immutable (ADR 0003), so what it starts with is part of
  its spec and not something the daemon remembers beside it.

- Nothing depends on a pin once the release serves.

- The move to the declared count is a scale-up like any other, admitted
  by the budget (section 7.4).

- A daemon that restarts finds the release where it was and treats it
  the same.

## Considered options

- An initial count in the release's spec beside `processes`, and a hold
  on the autoscaler while the release takes no traffic.

- Deploying with the small count and changing the release's `processes`
  at promotion.

- A scaling policy invented for the purpose, with the small count as
  `min_workers` and the declared one as `max_workers`.

## Decision outcome

Chosen option: "An initial count in the spec and a hold while there is
no traffic", because it leaves `processes` meaning what it always has,
changes no release after it is made, and uses the scale-up the budget
already admits. Changing `processes` at promotion breaks immutability,
and an invented policy would have the autoscaler size a release its
owner declared static.

### Details fixed by the implementation

- **The spec carries `initial_processes`,** for the release and for
  each named group, set only when it is below the group's `processes`.
  The host forks that many (`ReleaseSpec::starting_processes`), the
  budget checks the release at it (`View::release_memory`), and a wake
  from zero counts it.

- **The flag's forms.** `--initial-processes N` is the release's and so
  the default group's, and a named group with none of its own takes it,
  as a group without `processes` takes the release's, capped at the
  group's own `processes`. `--initial-processes NAME=N` is one group's.
  `--processes` has no form for a group on a deploy, a group's count
  coming from the project's table, so the form is new with this flag.

- **What is refused.** A count of 0, a count over the `processes` it is
  given for (the release's own for N, the group's for NAME=N), a group
  the release does not have, and the flag on a managed HTTP server
  release, whose instances do not scale (issue 175). A count equal to
  the declared one is taken and stored as no initial count.

- **No traffic is computed, not remembered.** Each second the daemon
  takes the releases that have an initial count and are not retained,
  and holds those with no share of their application's traffic
  (`Daemon::starting_small`, by `traffic_share`, the figure schedules,
  overlays and the surge allowance use), each group at its initial
  count (`Scaler::hold_at`). Nothing is stored, so a restarted daemon
  holds the same releases, and a release whose traffic is all taken
  away again, by a traffic policy say, returns to its initial count.

- **A pin stands over the hold and does not end it.** The issue says a
  release moves when it is pinned or serves. A pinned group is at its
  pin; when the pin is lifted while the release still takes no traffic
  the group returns to its initial count, which is the reading that
  keeps "a release with no share of traffic stays at its initial
  count" true whatever was done to it meanwhile.

- **Once it serves** the hold is simply not applied: a static group is
  below its count and is brought to it in batches of `max_step_up`,
  and a scaled group is raised to its `min_workers` and then sized by
  its signals. Both ask the budget, and a `scaling.denied_by_budget`
  event says when the count does not fit.

- **A release started again starts small again.** A rollback to a
  stopped release, and the releases a daemon starts afresh after a
  shutdown, fork the initial count and grow once they serve. Starting
  them at the declared count would need the start to know it is for
  traffic, and the growth takes a scale-out window.

- **The manual upgrade pins the new release before its first step,**
  since a release that takes a canary's first share now grows toward
  its declared count as far as the budget lets it. The guide says so.

### Consequences

- Good: a promoted release is at its declared count with no pin, and
  `muructl scale --unpin` leaves it there.

- Good: a deploy fits a budget at the count it will really start with.

- Bad: a release deployed with an initial count and not held serves
  its first seconds on that count, until the scale-up is admitted.

- Bad: the upgrade by hand has one more pin at its start.

## More information

- Issue 964, and issue 962 for the design it belongs to. ADR 0085
  records the decisions for swapping workers and for the rollouts of
  issue 321, which size the candidate with pins of their own.

- Sections 7.10 (the count a release starts with), 13.6 and 9.16.
