---
status: accepted
date: 2026-11-15
---

# ADR 0060: A managed server's instance has a log relay of its own, which signals it

## Context and problem statement

ADR 0059 makes the log relay the parent of a managed server's instance,
the only holder of its pipes' read ends, and leaves two points open for
issue 492: whether each instance has a relay or one relay takes every
instance of a release, and whether the daemon signals an instance
directly, as it did when it started instances itself, or through its
relay (specification sections 9.9 and 11.3).

## Decision drivers

- A relay is a parent, and a process can be the parent only of what it
  starts and starts again.

- Restarts, backoff, leftover process groups and adoption are the
  supervisor's in the controller (section 9.9), shared with the other
  programs the daemon supervises, and stay there.

- ADR 0059: a relay that dies takes its instance's output with it, and
  the controller treats that as the instance exiting.

- A signal must never reach a process that has been given a pid the
  instance had.

## Considered options

The relays:

- One relay an instance, each appending to the release's one log.

- One relay a release, the parent of every instance.

How an instance is signalled:

- Directly by the daemon, by the pid and start time the relay recorded.

- Through its relay, which signals its own child.

## Decision outcome

**One relay an instance.** The controller starts a relay for each
instance and the relay starts the instance, so the supervisor's unit
stays one process it starts, waits for and starts again; the relay
records that process's end. A relay for the whole release would have to
start instances again itself, taking restarts and their waits out of the
supervisor, and its death would end every instance of the release at
once rather than one. The relays of a release all append to the one
release log with `O_APPEND`, each a write of whole lines, and each opens
the file again by name on SIGUSR1, so the daemon tells every relay of a
release when it reopens or rotates the log and waits for each to say it
has. Each relay has its control socket, `relay-N.sock`, and its record,
`relay-N.toml`, in the release's run directory, N being the instance's
index; the daemon adds what every relay dropped to the release's count.

**Signals go through the relay.** While the relay runs, the daemon
signals the relay and never the instance: SIGTERM to the relay sends the
instance the stop signal it was started with, and SIGUSR2 sends the
instance's process group SIGKILL. The relay is the instance's parent and
does not reap it before it has recorded its end, so until then the pid,
and the group under it, cannot have passed to another process; the
daemon checks the relay itself by its pid and start time, as it checks
any process it adopted. Signalling the instance directly would need the
same pid and start time check on every signal, and a pid can still be
given out between the check and the signal. A relay that does not exit
within 500 ms of SIGUSR2 is killed, and what is left of the instance is
then killed by the daemon by its recorded pid and start time, the one
case in which the daemon signals an instance itself.

In this mode SIGTERM no longer ends the relay as it does a release's
relay (ADR 0059): the relay ends once its instance has ended and the
instance's pipes have, or 5 s after the end for a pipe a process left
behind holds open. The relay, not the daemon, writes its record, since
the record's purpose is to say how the instance ended when no daemon
was there to see it; it writes it whole, by a rename.

### Consequences

- Good: an instance's output, exit status and restarts are one relay's
  and one supervisor slot's, so a relay that fails costs one instance a
  restart and nothing else.

- Good: no signal from the daemon can reach a stranger while the relay
  runs, and the record gives the real exit status of an instance that
  ended while no daemon ran.

- Bad: a release runs one more process for each instance, and several
  writers share the release log, whose lines from different instances
  interleave line by line, each with the time it was read.

- Bad: a rotation or reopen waits for every relay of the release.

## More information

- ADR 0059, whose decision for managed servers this completes.

- Issue 492; issue 688, whose detached command runs use the same mode.

- Specification sections 9.9, 11.2 and 11.3.
