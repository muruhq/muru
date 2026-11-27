---
status: accepted
date: 2026-11-28
---

# ADR 0088: A supervisor acknowledges a host's events by number

## Context and problem statement

A host raises the events about its workers and sends each to its
supervisor over the control connection (sections 5.9 and 11.5). Since
issue 965 it holds the events it raises with no supervisor connected,
or whose send fails, and sends them to the supervisor that connects
next. A send that succeeds says only that the event is in a socket's
buffer. A daemon that is stopping, restarting or upgrading stops taking
host messages before it closes its connections, and what sits in the
socket, in the task reading it or in the channel to the daemon's loop
goes with the process (issue 982). The same loss follows a daemon that
is killed, and a run or a serve whose connection ends. How does a host
know an event was recorded, and how is one it sends again kept from
being recorded twice?

## Decision drivers

- Every event a host raises is recorded once, whenever its supervisor
  stops, including one that is killed.

- The host stays simple: it is a single-threaded loop that must never
  wait on its supervisor.

- The three supervisors (the daemon, `muructl run` and `muructl serve`)
  do the same thing.

## Considered options

- The daemon drains on detach: it shuts down its write half first and
  keeps taking host messages until each host has closed its end.

- An acknowledgement for each event, by an id.

- A cumulative acknowledgement: the host numbers its events and the
  supervisor says the number up to which it has recorded them.

## Decision outcome

Chosen option: "A cumulative acknowledgement", because it closes the
loss whatever way the supervisor goes, and costs the host one counter
and the queue it already had.

- The host numbers the events it raises from 1 (`seq`) and names itself
  by the time it started in nanoseconds (`stream`), both carried by
  every event. It holds every event, sent or not, until a supervisor
  says `{"type": "recorded", "seq": N}`, which lets go of every event
  numbered N or less. A supervisor that connects is sent whatever is
  held, after the host's state, as before.

- The supervisor acknowledges an event once it is recorded, not when
  it is read from the socket. The daemon records the event and the
  number of the last one recorded from that release's host in one
  transaction of the state store (table `host_events`, schema version
  24), so a daemon that is killed between recording and acknowledging,
  or whose acknowledgement the host never read, is sent the event again
  and does not record or count it again; it acknowledges it again. A
  run or a serve keeps its events in memory and dies with them, so it
  keeps the last number in memory.

- An event numbered at or below the last one recorded from the same
  stream is a repeat. Another stream is another host of the release,
  one started again after the first died, whose numbers start again.

- The bound stays: 256 events held, the oldest dropped past it. The
  `host.events_dropped` warning takes the number of the last event
  dropped, so it is recorded once and ahead of the events kept, and the
  count goes once that number is acknowledged.

The drain on detach was not chosen because it covers a daemon that
stops in order and nothing else, and needs each host to act on the end
of the daemon's stream within a bound. An acknowledgement for each
event by id needs the host to keep a set where a number does, and gives
nothing a number does not, since events are recorded in the order they
are sent.

### Consequences

- Good: an event is recorded once across a daemon's stop, restart,
  upgrade or death, and across a connection made again by any of the
  three supervisors.

- Good: nothing is added to a daemon's shutdown, which stays as fast as
  it was.

- Bad: every event is copied into the held queue, where only those
  raised with nothing connected were. The copy is without its evidence,
  as before.

- Bad: a host under a supervisor from before this record is never told
  its events are recorded, so it holds the last 256 and sends them
  again to whatever connects next. Nothing is done for that pairing:
  the project is in alpha and the two are released together. A
  supervisor under an older host sees events with no number and records
  each as it arrives, as it did.

- Bad: if an acknowledgement of the `host.events_dropped` warning is
  lost and more events are dropped before the next supervisor connects,
  the warning recorded then counts the earlier ones again.

- Bad: the state store's schema changes (version 24).

## More information

Section 11.5 of the specification states the behaviour. Issues 965 (the
held events), 982 (this loss) and 983 and 988 (connections made again).
