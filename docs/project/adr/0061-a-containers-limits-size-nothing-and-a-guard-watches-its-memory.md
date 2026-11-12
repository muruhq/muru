---
status: accepted
date: 2026-11-13
---

# ADR 0061: A container's limits size nothing, and a guard watches its memory

## Context and problem statement

Section 13.9 had `muructl serve` read the container's cgroup v2 limits
once at start and make two decisions from them. `--processes auto`, the
default, was the CPU quota of `cpu.max` rounded up. With a `memory.max`
and no `hard_limit`, the host took a budget (the limit less 10 %, at
least 128 MiB), subtracted what it held after preload, and divided the
rest equally over the workers of every group as each policy's hard
limit.

Both decided for the user from too little. The count took no account of
memory, of threads or of async concurrency, and each named group without
its own count took it again, so a 2-CPU container with one such group
ran 4 workers. On a container platform, capacity is added by running
more containers, so a count guessed per container buys little. The
derived hard limit assumed equal workers and a fixed share of shared
memory, though in CPython reference counting erodes a worker's
copy-on-write share of the host until it holds a copy of its own; the
share was fixed at start, so it was wrong once a group scaled; and a
group's own `hard_limit` still counted as a share, so the limits could
add up to more than the container had. The limits were also read once,
so an in-place resize took effect only on a restart (issue 289).

The question, settled with the maintainer on 2026-11-13 (issue 621), is
what serve does with the limits instead.

## Decision drivers

- A setting Muru cannot get right from what it can see should be the
  user's, stated plainly, rather than a guess that looks like a fact.

- What protects the container should read the real figure, what is left
  under `memory.max` now, rather than a per-worker share worked out once.

- An in-place resize, which rewrites the limits while the container
  runs, should apply without a restart.

- The daemon in a container, or under a systemd unit with `MemoryMax=`,
  should be protected the same way (issues 384 and 622).

## Considered options

- Keep `auto` and size it across groups. It stays CPU only.

- Size `auto` from memory as well as CPU. That needs a worker's size
  before any worker has run, which is the guess the derived hard limit
  makes.

- Keep the derived hard limit and correct it for groups and scaling. The
  equal split and the eroding shared memory still make it a guess.

- Trigger on pressure stall information (`memory.pressure`). It says the
  cgroup is stalling, which comes after it is short.

- Size nothing from the limits. Report them, warn when the configuration
  does not fit them, and guard `memory.max` at run time by replacing the
  largest worker when less than the headroom is left.

## Decision outcome

Chosen option: size nothing from the limits, and guard `memory.max`,
because it leaves the count and the per-worker limit to the user, who
knows the application, and protects the container from the figure that
matters, read when it is used.

- `--processes` has a fixed default of 1, as under `muructl run`, and
  `auto` is removed. A `hard_limit` is only ever the user's.

- The limits are read at start and at every memory check, so a resize
  applies at the next check. A change is logged, and the start warnings
  are given again against the new limits.

- Warnings, never refusals: the workers over the CPU quota, a preload
  that would not fit once each worker holds a copy of it, hard limits
  over `memory.max`, and request bodies spooled to a memory-backed
  filesystem, whose quota then defaults to the headroom.

- The memory guard runs in the controller, which sees every worker's
  report: muructl under serve and the daemon under the daemon, where the
  cgroup's workers may belong to several releases and the largest is
  chosen across all of them. It has the host replace the worker through
  the usual drain, one at a time, and stops, with a warning, while
  replacing workers cannot bring the cgroup back above the headroom.

- A worker replaced for memory stops before its replacement starts when
  less is left than its current size, with or without a hard limit.

### Consequences

- Good: no figure is presented as worked out when it was guessed; the
  start lines say what was read and what does not fit.

- Good: the guard acts on what is left now, so it holds for groups,
  scaling and workers of different sizes alike, and follows a resize.

- Good: one mechanism serves `muructl serve` and the daemon.

- Bad: an image that relied on `auto` runs one worker until it sets
  `processes`.

- Bad: without a hard limit, a single worker can grow until the
  container is short before anything acts, and then it is replaced at
  the check, not at its own threshold.

## More information

- Section 13.9 (container limits) and section 7.7 (the memory policy and
  the guard).

- Issues 621 (this change), 289 (reading the limits on a resize, folded
  into 621), 623 (scaling under serve, admitted against the same room),
  384 and 622 (the daemon's budget and guard from its cgroup).
