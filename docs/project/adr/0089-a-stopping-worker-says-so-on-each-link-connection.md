---
status: accepted
date: 2026-11-28
---

# ADR 0089: A stopping worker says so on each link connection

## Context and problem statement

The edge keeps connections to each worker idle between requests
(section 5.3) and sends a request again only when no worker can have
seen it (section 4.8, ADR 0032): when the worker cannot be connected to,
or writing the request head fails. A worker told to stop closed its
connections, and a request the edge wrote to one a moment before was
neither read nor refused: the write succeeded, the connection ended
with no response, and the edge could not tell a request that was never
started from one that died part way, so it answered 502
`worker_crashed`. With a daemon the edge is given a snapshot without
the worker a second before the worker is told to stop, so it has
stopped sending. With the daemon stopped, restarting or upgrading
nobody tells the edge, and about two requests failed for each worker
replaced under load (issue 986). How does the edge learn that such a
request was not started?

## Decision drivers

- No request fails when a worker is replaced, with a daemon or without.

- A request a worker has started is still never sent again (ADR 0032).

- Only the daemon talks to the edge's control socket; a host does not.

## Considered options

- The host tells the edge that a worker is leaving, without the daemon.

- The edge sends again any request whose connection closed before a
  response head, when the connection was one it had kept idle.

- The old worker goes on taking requests on the connections it has for
  a while after it is told to stop.

- The worker says on each connection that it is stopping, and the edge
  sends again a request answered that way.

## Decision outcome

Chosen option: "The worker says on each connection that it is
stopping", with a new frame of the worker protocol, STOPPING (14, empty
payload, worker to edge), because it is the only option that tells the
edge exactly which requests were not started.

- A worker told to stop writes STOPPING on each link connection and
  closes it: at once on one with no request, and after the response on
  one with a request. From the stop on it starts no request: one whose
  bytes are there unread, and one its closed thread pools refuse, are
  answered STOPPING as well.

- The edge, reading STOPPING where a response head would be, sends the
  request once more, as it does one whose head could not be written,
  when all of the request is still at the edge: no body, a buffered
  body, stored uploads, whose descriptors go with the head again, or a
  WebSocket handshake. A body passed on as it arrives is not sent
  again: the request is answered 502 `worker_unavailable`. A gRPC call
  is answered UNAVAILABLE, which a client's retry policy acts on.

- A request sent again takes a new connection, and the idle connections
  the pool kept to that worker are closed. The new connection is made
  to the worker's path, which with no daemon is by then a second name
  for its replacement's socket (issue 981), and with one worker in the
  pool is the only place to go.

A new frame kind is additive within the protocol's major version
(section 5.8).

The host telling the edge was not chosen because it gives a second
writer to the edge's routing, which the daemon alone owns, and a host
would need the edge's control socket and its snapshot format. Sending
again on any closed connection breaks ADR 0032: a worker that crashed
while running the request closes the connection the same way. A worker
that goes on serving has no end to wait for, since the edge never reads
or closes a connection it holds idle, so every stop would run to a
timer.

### Consequences

- Good: a worker replaced, recycled or restarted while the daemon is
  away costs no request, and the same holds for any stop the edge was
  not told of in time.

- Good: two races inside a stopping worker are closed with it: a
  request read as the pools closed, and one whose head was being read
  as the worker counted its requests in flight, both of which ended as
  502.

- Bad: an edge from before the frame takes STOPPING for an unexpected
  frame and answers 502 `worker_crashed`, as it did for the closed
  connection, so nothing is lost and nothing gained until the edge is
  replaced.

- Bad: a request with a streamed body caught in that moment still
  fails, with 502 `worker_unavailable`.

- Bad: a stopping worker keeps its runtime up to half a second longer,
  inside its stop budget, when a connection is slow to write the frame.

## More information

Sections 4.8, 5.3 and 13.4 of the specification. Issues 981 (the second
name for a replacement's socket), 983 and 986. ADR 0032 (a request a
worker has read is never sent again).
