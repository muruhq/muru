---
status: accepted
date: 2026-11-20
---

# ADR 0069: Instances are found through a registry of entries, each owned by its instance

## Context and problem statement

Several Muru instances can run on one machine: daemons, development runs
and `muructl serve`. A run recorded itself in `RUNTIME/runs/PORT/run.json`,
which a run killed with SIGKILL left behind, and a daemon recorded nothing
another command could find, so one started with its own `--state-dir` or
`--run-dir` was found only by someone who knew its socket. Test sessions
used their own `MURU_RUN_DIR`, so what they leaked could not be found from
outside them. Commands need to list what runs, where it listens and what
was left behind, and that list must hold up while instances start and stop
at once, as parallel tests make them (section 13.8, issue 805).

## Decision drivers

- Concurrent starts and stops, parallel tests above all, must never
  contend on a shared file or corrupt each other's records.

- Whether an instance is alive must not depend on its pid, which the
  operating system reuses.

- One place per user, whatever run directory an instance was given, so a
  test session's leftovers are visible from outside it.

- Room for instances on other machines later, which have no local process.

## Considered options

- A shared index file, read, changed and written back under a lock by each
  instance that starts or stops.

- One entry per instance, a directory its owner alone writes, alive while
  the owner holds a `flock` on a file in it.

- No registry: scan the processes and their titles.

## Decision outcome

Chosen option: one entry per instance, because nothing is ever written by
two processes, a free lock proves the owner gone however it ended, and an
entry is published by renaming a locked directory into place, so a pruner
never takes a half-made entry for a stale one.

- The registry is `USER_RUNTIME/registry`, in the user's default runtime
  location whatever `--run-dir` says; root's and the system daemon's is
  `/run/muru/registry`, readable by every user. The runtime locations are
  tmpfs or local disk, which `flock` needs.

- It records presence alone. Durable definitions of named instances are a
  separate catalogue in the configuration directory, and `muructl instance
  list` merges the two. A remote instance would be a definition with no
  presence entry.

- An ID is 128 random bits, so IDs from different machines never collide.
  A daemon keeps its ID in `STATE/instance-id` and takes over its own
  entry when it restarts; a run or a serve gets a new ID at each start.
  `GetStatus` reports the ID and the hostname.

- The record's `admin` is an endpoint object rather than a socket path,
  and `client::connect` takes the endpoint, so another transport is a new
  variant rather than a change to every caller.

- A daemon that detaches leaves its entry, with its lock freed, so the
  edge and hosts it leaves serving are seen as a detached instance's; a
  shutdown removes it.

- A run's own directory, holding its socket, pid file and releases' run
  directories, is `RUNTIME/run-SHORTID`, named by the ID rather than the
  port, which collided between runs on one port on two addresses.

### Consequences

- Good: listing, discovery and pruning read the same entries, and a
  crashed instance is told from a live one without trusting a pid.

- Good: `muructl cache` reads what every live instance of the user uses,
  whatever its run directory.

- Bad: the runtime directory gains a directory per instance, and a reader
  makes one lock probe per entry.

- Bad: a run's directory is no longer predictable from its port, so a
  script finds its pid file from the line the run prints, or `$!`.

## More information

Section 13.8 of the specification ("The instance registry", "Finding a
running instance"), issue 805, and the tracking issue 813. Issue 814
describes the remote instances this structure leaves room for.
