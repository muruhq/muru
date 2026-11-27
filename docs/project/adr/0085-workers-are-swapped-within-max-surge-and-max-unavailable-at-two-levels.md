---
status: accepted
date: 2026-11-27
---

# ADR 0085: Workers are swapped within max_surge and max_unavailable, at two levels

## Context and problem statement

Workers are swapped while an application keeps serving in three places:
one worker is recycled, every worker of a release is restarted, and
traffic moves from one release to another. Each was handled apart, and
none was checked against the node's budget.

- A worker is replaced surge first: the host forks the replacement while
  the old worker serves (section 7), so each replacement runs its group
  one worker over its count. Nothing limited how many replacements of a
  group ran at once, and the budget of section 7.4 admitted only
  scale-ups and release starts, so a node at its budget went over it for
  the length of every replacement.

- Section 7.7 said recycles are rate limited and use `max_surge`, and
  sections 10.4 and 10.5 said a change to thread counts, `max_requests`,
  the memory policy or the hooks is applied to a running release by a
  rolling restart bounded by `max_surge`. No such setting existed, and
  nothing restarts a running release on a settings change: releases are
  immutable (issue 920).

- `muructl restart APP --all` restarted every worker in turn, with the
  turn-taking in `muructl`, over the admin API's `Debug` call, which a
  client of the API had to repeat (issue 871).

- On a host sized for one copy of an application two full copies do not
  fit, and an upgrade is a procedure worked out by hand (issues 321 and
  958). An application whose worker needs exclusive access to a resource
  has no way to have the old worker stopped first (issue 959).

The question is what one policy covers all three, who enforces it, and
how the budget holds it. The design was decided on 2026-11-27 and is
tracked in issue 962.

## Decision drivers

- Hosts keep serving and recycling while the daemon is stopped or
  upgrading (section 13.4), so whatever paces the swaps of one release
  cannot need the daemon.

- Only the daemon sees two releases, the traffic between them and the
  node's budget.

- The defaults are the behaviour before the change: one replacement at a
  time, started before the old worker stops.

- A node at its budget stays within it, and a host with nothing spare
  can ask for the other order.

- A release is immutable. What reaches traffic with different settings
  is a different release, with the drain and rollback every release has.

## Considered options

- One policy, `max_surge` and `max_unavailable` per release and group,
  enforced at two levels: by the host inside a release and by the
  rollout engine across two.

- Pacing every swap in the daemon, the host asking before each.

- `max_surge` alone, with drain first as a strategy of its own
  (recreate) for releases and a declaration (`exclusive`) for workers.

- Leaving the surge out of the budget.

## Decision outcome

Chosen option: "One policy enforced at two levels", because it is the
only one that keeps recycling going while the daemon is away and still
lets the one process that sees the budget hold it. Pacing in the daemon
would stop recycles for the length of a daemon upgrade. A recreate
strategy and an `exclusive` declaration would add a second way to say
what the two settings already say, and the edge would have to hold
requests across the gap to make it worth having. Leaving the surge
uncounted is what lets a node go over its memory.

The decisions, numbered as in issue 962:

1. **No in-place restart.** A change to thread counts, `max_requests`,
   `max_lifetime`, `max_surge`, `max_unavailable`, the memory policy or
   the hooks reaches traffic as a new release. Sections 10.4 and 10.5
   are corrected, and the plan's "Rolling restart" class goes (issue
   920).

2. **Two levels.** The host paces swaps inside a release and the rollout
   engine paces moves between releases.

3. **`max_unavailable`** is added beside `max_surge` (issue 151):
   `max_surge` (default 1) is how many workers a group may run over its
   count during a swap, `max_unavailable` (default 0) how many under.
   Both 0 is refused.

4. **Surge in the budget.** Each group's surge allowance is admitted
   like a scale-up and counted as workers held (issue 960).

5. **Involuntary replacements** (a crash, a stall, a hard timeout, the
   hard memory limit, the memory guard) skip the queue but keep the
   order `max_surge` sets (issue 151).

6. **No floor for drain first.** With `max_surge = 0` and one worker, a
   swap is a short gap, which is what was asked for (issue 151).

7. **Held releases** get no surge allowance and swap drain first (issue
   960).

8. **A shared capacity rollout's total** is the baseline's worker count
   when the rollout starts, group by group, with the budget as a cap. It
   does not come from the budget, so the option works on a node with
   none (issue 321).

9. **A floor of one each.** Each side with a share of traffic keeps one
   ready worker in each group, so a one-worker application runs two for
   the length of the rollout, as an ordinary canary does (issue 321).

10. **Small totals** round in the baseline's favour: the candidate's
    first worker is one over the total until its share earns a whole
    worker (issue 321).

11. **Worker groups.** Every group of both releases is sized by the
    release's share of traffic, each with its own floor and rounding
    (issue 321).

12. **A step waits** for the baseline to reach its count, bounded by the
    drain grace; a rollback does not wait (issue 321).

13. **Managed HTTP server releases** are refused by a shared capacity
    rollout until their instances scale (issues 175 and 321).

14. **A node with no memory limit** gets a warning from `muructl
    doctor`, and no default from physical memory, since other things run
    on the machine and a guessed limit would refuse releases that fit
    (issue 961).

15. **Recreate** (stop the old release before starting the new) is a
    documented manual procedure with a page of its own, not a strategy
    (issue 959).

Decisions 8 to 13 are for the rollouts of issue 321, which is the work
after this, and are recorded here because they were taken with the rest.

### Details fixed by the implementation

The design leaves these to the work. Each is the call that fits the
decisions above.

- **What a slot is.** A group has a surge slot in use for each of its
  workers that has a replacement alive beside it, and an unavailable
  slot in use for each worker of its count that is not in routing,
  whatever took it out: a drain ahead of a replacement, a replacement
  still starting, a crash waiting to be started again, a scale-out not
  yet ready. Counting every worker out of routing, and not only those a
  swap took out, keeps a voluntary drain-first swap from starting while
  a crash has already left the group short.

- **What is voluntary.** A recycle for `max_requests`, `max_lifetime` or
  the soft memory limit, and the restart of every worker by
  `RestartRelease`. The restart of one worker by its index is not
  queued: an operator asked for that worker to be replaced now, and
  `muructl restart --force` is the way to kill a stuck one, which
  cannot wait behind recycles. It keeps the order, as an involuntary
  replacement does.

- **The order of an involuntary replacement follows the surge
  allowance,** not only `max_surge`: surge first while the group's
  allowance is at least 1, and otherwise the old worker has exited
  before its replacement is forked. A group whose allowance the budget
  withdrew, or whose release takes no traffic, therefore replaces a
  worker past its hard timeout without going over the budget, at the
  cost of the hard drain grace before the replacement starts. The
  slots in use are not counted, so a second hard timeout while a surge
  is under way is not held back.

- **The floor of issue 662 is kept where it was made for.** A worker
  replaced for memory stops first, whatever the settings, while less is
  left under its cgroup's limit than its size, and the last worker of a
  group in routing is then not stopped while another is away. That
  floor does not apply to a group whose surge allowance is 0: there
  the order was asked for, and decision 6 says it has no floor.

- **The queue is first come, first served,** one for each group, held
  by the host. A swap leaves it when its worker goes another way (a
  crash, an involuntary replacement, a scale-in). A worker being
  debugged keeps its place and is passed over until its last session
  ends. A swap that waits raises `worker.swap_waiting` once, naming a
  surge slot, an unavailable slot or the budget.

- **Allowances are granted before scale-ups,** each second, from
  nothing: the daemon counts what the releases hold, grants each group
  as much of its `max_surge` as still fits, by priority and then in
  application, release and group order, counts what it granted as held,
  and admits scale-ups against the rest. Granting from nothing each
  time means a changed limit or count needs no special case, and an
  allowance is stable while nothing changes, since a scale-up is only
  ever admitted into what the allowances leave. A worker of an
  allowance is counted at what a worker of its group is counted at.

- **The host is told why it has less than `max_surge`:** the limit that
  left no room (workers, memory or CPU), or that the release takes no
  traffic, so that its `worker.swap_waiting` can say so with the daemon
  away.

- **A surge under way is not stopped** when its allowance is withdrawn.
  It ends on its own in the time a worker takes to start and another to
  drain.

- **Two calls, `RestartRelease` and `CancelRestart`,** and no flag on
  one call for the other, so that each has a request that says one
  thing. Both are apart from `Debug`.

- **The host holds a restart's marks,** by worker, and reports how many
  are left with its status, so the daemon's description of the release
  survives a restart of the daemon. A second `RestartRelease` while one
  is under way marks the workers started since the first, and is not
  refused: the call says "every worker started before now", which
  holds either way.

- **`muructl restart --all` waits by default,** as it did, and
  `--no-wait` returns once the workers are marked.

- **Doctor's finding is a warning** and does not change its exit code:
  a node with no memory limit is a way to run, and a development
  machine has none.

### Consequences

- Good: recycles of a group are rate limited by its `max_surge`, as the
  specification always said.

- Good: a node with a budget stays within it through recycles and
  restarts, and `muructl daemon status` shows the room the surges hold.

- Good: a rolling restart is one call of the admin API that survives a
  restart of the daemon.

- Good: an application that cannot run two workers at once has settings
  that say so for every way a worker is replaced.

- Bad: a node with many groups holds one worker's worth of its budget
  for each. `max_surge = 0` gives it back.

- Bad: on a node at its budget with the default `max_unavailable = 0`, a
  recycle waits until there is room. The soft memory limit then does
  nothing until the hard limit or the guard replaces the worker, which
  is not held back. `worker.swap_waiting` says so.

- Bad: with a surge allowance of 0, a worker past its hard timeout is
  replaced only once it has exited, so its group is one short for the
  hard drain grace and the start of a worker.

- Bad: a held release with one worker does not answer a preview for the
  moment its worker is swapped.

- Bad: an operator who changes a setting and expects running workers to
  take it has to deploy. The plan of an apply says "New release".

## More information

- Issue 962 tracks the design; issues 920, 958, 151, 960, 871, 959 and
  961 are its parts, and issue 321 the rollouts that follow.

- Sections 7.8 (swapping workers), 7.9 (the surge allowance), 9.15
  (stopping the old before starting the new), 9.16 (rollouts whose
  capacity follows traffic), 10.5 (what changes live), 10.14 (the
  restart call) and 11.10 (doctor).

- ADR 0061 (a container's limits size nothing and a guard watches its
  memory) and issue 662 are where a worker stopped first for want of
  memory comes from.
