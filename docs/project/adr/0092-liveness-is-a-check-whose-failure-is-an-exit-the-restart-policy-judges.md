---
status: accepted
date: 2026-11-28
---

# ADR 0092: Liveness is a check whose failure is an exit the restart policy judges

## Context and problem statement

The supervisor of ADR 0090 starts again a process that exits. A process
that is still there and no longer answers, deadlocked or stuck on
something that never returns, is not seen by it: a managed server's
instance in that state takes requests it never answers until the edge
ejects it, and a service's instance looks `running` for good
(specification sections 9.9 and 9.17, issue 491). Related is the release
whose instances have all exited and are not started again: nothing is
going to answer, yet a request was held for one to start (issue 1001).

## Decision drivers

- One place judges what follows a failure: the restart policy, with its
  waits, its crash loop and its giving up.

- A check that is wrong must not take a working process down by default.

- The daemon's loop is not held by a check that hangs.

- A request is not held for something that is not coming.

## Considered options

- Leave liveness to the edge's ejection for servers, and to nothing for
  services.

- A check on by default, the readiness probe repeated.

- A check that is declared, whose failure stops the instance as a
  failure, and the policy decides the rest.

## Decision outcome

Chosen option: "a check that is declared", because the others either
leave a stuck process running or restart processes on a guess.

`liveness` is a table of its own on a service and on a managed server
release, and nothing is asked without it. A service's takes the probing
forms of its readiness (`tcp`, `unix`, `http`, `command`); a managed
server's is an HTTP path, the readiness path unless it names another.
Each instance in service is asked every `interval` (10 s), one check at
a time on a task beside the daemon's loop, each within `timeout` (2 s),
and `failures` (3) in a row stop the instance as a stop does, through
the supervisor's `fail`: once the process has gone the policy judges a
failure and a quick exit, so a process that keeps stopping answering
backs off, is called a crash loop and is given up on as one that keeps
exiting is. The count starts over with each process and with each
answer. A managed server's instance is marked out of service at the
edge before it is asked to stop.

For the release with nothing left to answer, the daemon marks the
instances that are `exited` or `failed` as down in the edge's snapshot,
and with every address down the edge answers 503 `upstream_down` at
once, letting go the requests it holds when a snapshot says the last one
is down. A release with an instance in backoff is still held for, since
that instance is coming.

### Consequences

- Good: a stuck process is replaced, with the same events, waits and
  limits as one that exits, and nothing new to learn about what follows.

- Good: a check that is wrong restarts only what its author declared it
  for, and a change to it is in force without a restart.

- Bad: a liveness failure after a long healthy run still counts as a
  quick exit, so the first wait after it is `delay` and a second failure
  soon after doubles it. That is the wanted behaviour for a process that
  keeps hanging, and a surprise for one that hung once.

- Bad: a managed server's check is HTTP only. A server whose health is
  not an HTTP answer has the edge's ejection and nothing else.

## More information

Specification sections 9.9 and 9.17. Issues 491 and 1001. ADR 0090 is
the restart policy, and ADR 0091 what Muru supervises.
