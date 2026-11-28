---
status: accepted
date: 2026-11-28
---

# ADR 0090: One restart policy for supervised processes, which never gives up by default

## Context and problem statement

The instances of a managed HTTP server release (section 9.9, ADR 0014)
are run by a supervisor in the controller that knows nothing of HTTP,
written to be shared with the services the daemon will run beside the
applications. It had one fixed behaviour and no policy: an instance that
exited was started again for ever after a wait doubling to 30 s, a clean
exit was treated as a crash, nothing marked a crash loop, a release that
failed to start when the daemon came up stayed down, and `muructl
restart` had nothing to act on, since it asks a release's host and a
managed server has none (issue 393). What is the policy, given that
services will take the same one?

## Decision drivers

- A server is declared because it should be up. After a reboot, or
  when a database returns, it should come back without a person.

- A loop that never ends must be impossible to miss.

- The policy is decided once: a service's `restart` table and a managed
  server's are the same table.

- A deploy has someone waiting for its answer. A daemon that starts
  after a reboot has not.

## Considered options

- Leave managed servers as they are, as a migration aid whose own
  master supervises its workers.

- Give up after a few tries by default, as supervisord does after three
  and systemd after five in ten seconds.

- Never give up by default, back off to a minute and make the loop
  loud, with the count of tries a setting for those who want one.

## Decision outcome

Chosen option: "Never give up by default, back off to a minute and make
the loop loud", because giving up needs a person afterwards and never
giving up recovers when a dependency returns, which is the common case
after a reboot. It is what Kubernetes does. The parts:

- **The `restart` table.** `policy` (`always`, `on-failure`, `never`),
  `success_exit_codes` (`[0]`), `delay` (1 s), `max_delay` (1 min),
  `stable_after` (1 min) and `give_up_after` (none), accepted on a
  `kind = 'http'` release and read into the supervisor's
  `RestartPolicy`. The key names are the ones a service will use.

- **`always` is the default.** A program that exits cleanly and should
  stay down is the exception, and says so with `on-failure` or `never`.
  A clean exit is a status in `success_exit_codes`. Death by a signal,
  an adopted process that is simply gone and one whose relay was lost
  are failures, since none has a status to judge.

- **Only quick exits count.** An exit within `stable_after` of the
  start adds to a count that doubles the wait, and an exit after a
  longer run clears it. `give_up_after` and the crash loop both read
  that count, so a server that crashes once a day never reaches either.

- **A crash loop is loud.** The third quick exit in a row is a
  `server.crash_looping` event at error level, once for a loop. An
  instance has a state (`running`, `starting`, `backoff`, `exited`,
  `failed`) that `muructl release show` gives with when the next start
  is, and `muru_server_exits_total` counts exits by outcome. Three,
  because by then the wait has doubled twice and the two exits before
  it were not a deploy settling.

- **Retries at the daemon's start.** A release restored when the daemon
  starts that fails is tried again with the same waits, a managed
  server by its table and a release of workers by the defaults. A
  deploy, a rollback or an apply that fails still fails at once.

- **A restart by hand** goes through the supervisor, one instance at a
  time, each answering its readiness probe before the next is stopped,
  and stops early when one does not come back. An exit that was asked
  for is not judged by the policy and counts as no quick exit.

### Consequences

- Good: a managed server comes back by itself after a reboot or an
  outage of something it needs, and an operator who looks at the
  events, the release or the metrics cannot mistake a loop for health.

- Good: services inherit a policy that is already in use, with its
  table, states and events, and the supervisor needs no second design.

- Good: `give_up_after` gives supervisord's and systemd's behaviour to
  those who want it, as a required service under `muructl serve` will,
  so that its container can be replaced.

- Bad: a server that can never start is tried once a minute for ever.
  The cost is a process start a minute and an event each time.

- Bad: the release stays `ready` with an instance, or every instance,
  down. Its state says what was deployed and proved to serve, and the
  instances' states say what runs now. A release with every instance
  `exited` or `failed` holds each request for the queue timeout before
  failing it.

- Bad: the longest wait went from 30 s to 1 min, so a server that keeps
  failing for a while and then can start waits up to half a minute
  longer for its next try.

- Bad: the restart by hand and the retries of a restore are the
  daemon's, kept in memory. A daemon that stops part way through a
  restart leaves the instances not yet reached as they were, and one
  that starts again counts a restore's attempts from one.

## More information

Issue 393, and decisions 2 and 3 of issue 485, which are the same
policy for services; the `restart` table is section 4.2 of the service
design in issue 486. Section 9.9 of the specification has the details
fixed by the implementation, and section 10.14 the restart call. What
is not decided here: an instance that hangs without exiting (issue
491), and a bound on an instance's memory (issue 996).
