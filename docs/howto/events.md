# Events and webhooks

The daemon records every change of state as an event. `muructl events`
shows or follows them, and webhook sinks receive them as signed
CloudEvents requests.

## The event stream

Every change the daemon makes or notices is an event: a release created,
ready, draining or stopped, traffic switched, a host restarted, a worker
started, recycled or killed, the edge replaced, logs reopened, rotated,
truncated or pruned, a
certificate that could not be read. Each has
a node-wide sequence number, a time, a type such as `release.ready`, a
subject such as `shop@r1`, a level (`info`, `notice`, `warning` or
`error`) and a sentence.

```
$ muructl events
    41 12:20:01 info     release.created      shop@r3                  created from /srv/shop
    42 12:20:03 info     release.ready        shop@r3                  ready: 2 workers listening
    43 12:20:03 info     traffic.switched     shop                     traffic moved to shop@r3
$ muructl events --follow --after 43
```

`--follow` keeps printing as events happen and `--after N` starts after a
sequence number, so a watcher that reconnects misses nothing. Events are
kept in the state store, the last ten thousand of them.

A release's host raises the events about its workers, and it goes on
serving and replacing workers while the daemon is stopped, restarting or
being upgraded. It holds the events it raises meanwhile, the last 256,
and sends them to the daemon that takes it over, which records each with
the time it happened and counts it in the [metrics](metrics.md) and for
a rollout's gates, and delivers it to the sinks. Such an event takes the
next sequence number when it is recorded, so in a listing its time is
earlier than that of the events above it:

```
$ muructl events
    57 12:31:10 info     daemon.started       daemon                   serving on http://0.0.0.0:8000, admin socket /run/muru/admin.sock
    58 12:31:10 info     release.adopted      shop@r3                  re-adopted the host (pid 4810)
    59 12:31:07 info     worker.started       shop@r3                  worker 1 started as pid 5120
    60 12:31:07 info     worker.ready         shop@r3                  worker 1 (pid 5120) is ready
```

When a host raises more than 256 events with no daemon, the oldest are
dropped: its log says when that begins, and a `host.events_dropped`
warning, recorded ahead of the events it kept, says how many. The
release's log has a line for every event either way.

A [development run](run-settings.md) and `muructl serve` answer `muructl
events` too, for the events their hosts raise: workers started,
recycled, replaced or exited, requests past a timeout or the deadline,
debugging sessions. They keep the last 256 in memory for as long as
they run, across reloads, numbered from 1, each with the release that
raised it as its subject (`shop@r2` after the first reload).
`--after` and `--follow` work as they do against the daemon. Each event
is printed to the run's terminal as well.

## Webhooks

A `[[events.sink]]` entry in the daemon's configuration file names a URL
to deliver events to:

```toml
[[events.sink]]
name = "ops"
url = "https://hooks.example.com/muru"
secret_file = "/etc/muru/hooks/ops.secret"
types = ["release.*", "traffic.*", "edge.*"]
levels = ["notice", "warning", "error"]
```

| Key | Effect |
| --- | --- |
| `name` | names the sink; events about the sink itself carry it as their subject |
| `url` | where deliveries go, `http://` or `https://` |
| `secret`, `secret_file` | the key deliveries are signed with, inline or read from a file; one of them is required |
| `types` | patterns the event type must match (`*` matches anything); every type when omitted |
| `levels` | levels delivered; every level when omitted |

Each delivery is an HTTP POST with one event as CloudEvents 1.0 JSON:

```json
{"specversion": "1.0", "id": "42", "source": "muru://node1",
 "type": "dev.muru.release.ready", "subject": "shop@r3",
 "time": "2026-10-24T12:20:03.418Z", "datacontenttype": "application/json",
 "muruseq": 42, "level": "info",
 "data": {"message": "ready: 2 workers listening"}}
```

The headers carry the same identification and the signature:

| Header | Value |
| --- | --- |
| `Content-Type` | `application/cloudevents+json` |
| `X-Muru-Event` | the type |
| `X-Muru-Delivery` | the id, which is the sequence number |
| `X-Muru-Signature` | `sha256=` and the hex HMAC-SHA256 of the body with the secret |

A receiver checks the signature over the exact bytes of the body before
trusting a delivery:

```python
import hashlib
import hmac

def verify(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```

Any 2xx response accepts the delivery. Anything else, or no answer within
ten seconds, is retried with a wait that doubles from one second to a
minute, for as long as it takes: deliveries to a sink happen one at a
time and in order, from a cursor the daemon keeps in its state store, so
a receiver that is down gets everything it missed once it is back, and a
daemon restart carries on where the last one stopped. A receiver can
therefore see an event twice, when the answer was lost, and the id tells
it so. A sink named for the first time starts from the events after it
was added. After three failed attempts in a row the daemon records an
`events.sink.failing` warning, and an `events.sink.recovered` notice when
deliveries get through again; both are delivered to the sink too, when
its filters allow.
