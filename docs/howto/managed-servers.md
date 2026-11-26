# Managed HTTP servers

A managed HTTP server release runs an ordinary HTTP server, such as the
gunicorn or uvicorn an existing deployment already uses, instead of
Muru's own workers. Muru starts the server's instances, waits until they
answer, routes requests to them the way it routes to an
[upstream](upstreams.md), restarts an instance that exits, and stops them
when the release drains. The release is a release like any other: it
serves or is held, takes a share of a route's traffic, is canaried
against, drained, kept warm and rolled back to. The intended use is a
cutover: run the current deployment unchanged under Muru first, then move
traffic to native workers of the same application a step at a time.

## Declaring one

```toml
[app.legacy.release.r9]
kind = 'http'
profile = 'gunicorn'
command = ['gunicorn', '--workers', '4', '--bind', '{bind}', 'legacy.wsgi:application']
cwd = '/srv/legacy'
env = { DJANGO_SETTINGS_MODULE = 'legacy.settings' }
bind = 'unix'
instances = 1
readiness = { path = '/healthz', timeout = '60s' }
```

| Key | Default | Meaning |
| --- | --- | --- |
| `kind` | `workers` | `http` runs a server from `command` instead of workers from an entrypoint |
| `profile` | required | `gunicorn`, `uvicorn`, `granian`, `mod_wsgi-express`, `uwsgi` or `generic`; fixes how the server binds and how it is stopped |
| `command` | required | the server's command line; a bare name is found on `PATH`, a relative path with a directory in it is taken from `cwd` |
| `cwd` | `source` | the directory the command runs in; one of the two is required |
| `bind` | `unix` | `unix`, a socket per instance in the release's socket directory, or `port`, a free loopback port per instance |
| `instances` | `1` | how many instances of the command run, each with its own address |
| `readiness` | none | `path` is probed with a GET every 100 ms until it answers 2xx or 3xx; without it any HTTP response makes the instance ready. `timeout` (default `60s`) is how long the instances get |
| `stop_signal` | the profile's | the signal that stops an instance gracefully: `TERM`, `INT`, `QUIT`, `HUP`, `USR1`, `USR2` or `WINCH`; required by `generic` |
| `env` | none | environment variables and secret references, as for a workers release |
| `timeouts.shutdown_timeout` | `5s` | how long an instance told to stop gets before it, and every process it started, is killed |

The same release is deployed from the command line with `--server` and
the command after `--`:

```console
$ muructl deploy legacy /srv/legacy --server gunicorn --readiness /healthz -- \
    gunicorn --workers 4 --bind '{bind}' legacy.wsgi:application
muru: release legacy@r9 is ready and serving
```

`--bind`, `--instances`, `--readiness-timeout` and `--stop-signal` set
the other keys, and `--env` and `--secret` the environment. The worker
settings of a release (`entrypoint`, `processes`, `threads`, `preload`,
`groups`, `mounts`, `scaling`, `memory`, the soft and hard timeouts and
the rest) do not apply to a managed server and are refused.

## Binding

Each instance gets an address of its own: a Unix socket in the release's
socket directory, which is named by the release's number (`h/7` in the
daemon's run directory for the release numbered 7), or a loopback port. The command says where the
server takes it:

- `{bind}` is replaced by the address in the server's own form: for
  gunicorn and generic `unix:/path` or `127.0.0.1:PORT`, for uvicorn,
  Granian and uWSGI the socket path or `127.0.0.1:PORT`.
- `{socket}` is replaced by the socket path and `{port}` by the port
  number, for servers with a flag of their own for each.
- A command naming none of them gets the profile's flags appended:
  `--bind ADDRESS` for gunicorn, `--uds PATH` or
  `--host 127.0.0.1 --port PORT` for uvicorn and Granian,
  `--host 127.0.0.1 --port PORT` for mod_wsgi-express, which binds a
  port only, and `--http-socket ADDRESS` for uWSGI. A generic command must
  name `{bind}` or read the environment.

Every instance also has `MURU_HTTP_BIND` (the address as `unix:/path` or
`127.0.0.1:PORT`), `PORT` when it binds a port, and `MURU_SERVER_INDEX`,
its index from 0, in its environment.

```toml
[app.api.release.r3]
kind = 'http'
profile = 'uvicorn'
command = ['.venv/bin/uvicorn', 'api.main:app', '--workers', '2']
cwd = '/srv/api'
bind = 'port'
instances = 2
```

## Readiness, supervision and stopping

The release is `warming` until every instance answers its probe, and
`failed` if one cannot be started, one exits first or the timeout passes,
with the instance and the reason in the release's message. A release
that fails leaves nothing running: the instances that had started are
stopped as a drain stops them.

An instance that exits later is
started again after a wait that doubles with each exit in a row, from one
second up to thirty, and starts over once an instance has run for a
minute; the exit and the restart are `server.exited` and `server.started`
events, and a start that fails is `server.failed`. An instance is never
started beside what the one before it left running. Processes still in
the process group of an instance that exited, such as workers whose
master was killed, are sent the stop signal when the exit is seen, and
the `server.exited` event says that processes were left behind. Any
still running when the wait ends are killed, and the instance starts on
the daemon's next check, a second later. They are also killed if the
daemon stops or upgrades before the wait ends.

An instance that has exited is out of service until it is started
again and answers the readiness probe, the same probe the release's
start waits on, tried every 100 ms. While it is, requests go to the
other instances, and its refused connections do not count against it.
Once it answers, a `server.ready` event says so and it takes requests
again:

```
instance 1 answers and takes requests again
```

An instance that has not answered when the readiness timeout ends takes
requests all the same, with a `server.ready` warning, and is set aside
for a while if they fail, as any instance is after three failed requests
in a row.

When every instance is starting, as when a release with one instance
has that instance started again, a request waits for one to answer
rather than failing. It waits for at most the queue timeout
(`queue_timeout`, 5 seconds by default) and is sent as soon as an
instance takes requests again; the time it waits counts towards its
request deadline. One still waiting when the queue timeout ends is sent
to the instances all the same, and is answered 502 `upstream_failed` if
none takes it. At most 256 requests wait for one release in each edge
process, and any beyond them are answered 503 `upstream_starting` with
`Retry-After: 1`. An instance is out of service from the moment it
ends, even while the last of its output is still being written to the
release's log; a request that arrives in the instant before the edge
hears of the end is not held, and with no other instance to go to is
answered 502 `upstream_failed`.

With every instance out of service for other reasons, such as failed
requests, requests still try them all, and are answered 502
`upstream_failed` when none takes them.
`muru_upstream_health` reports each instance by address under the
release's name, 0 while it is out of service. The instance count is
fixed: `muructl scale` refuses a managed server.

Draining stops new requests at the edge, waits for those in flight, then
sends the process of each instance the profile's stop signal (`TERM` for
every profile unless `stop_signal` says otherwise). The signal goes to
that process alone, the one the command started, such as a gunicorn
master, and not to the processes it started in turn.

Each instance runs in a process group of its own, which holds every
process it started, such as a master's workers. What is left of an
instance when `shutdown_timeout` ends is killed as a group:

- An instance whose process is still there is killed, and every process
  in its group with it.

- An instance whose process exited in time but left processes of its
  group running has those sent the stop signal as well, as soon as it
  has exited, and they are killed when `shutdown_timeout` ends, or half
  a second after the signal if less than that was left.

A process that has moved itself out of the instance's process group, by
starting a session of its own as a daemonising server does, is not found
and is left running.

A superseded release is kept warm as a workers release is, with its
instances running, so a rollback to it is instant.

## uWSGI

uWSGI runs with `--http-socket`, so its workers speak HTTP themselves and
the edge is the router; `--http` would put uWSGI's own HTTP router
process in front of them. The profile also adds `--master` and
`--hook-master-start "unix_signal:15 gracefully_kill_them_all"` to every
command: uWSGI 2.0 takes TERM as a reload, which drops the requests in
flight, and the hook makes it a graceful stop that finishes them first.
Both are harmless when the command or an ini file sets them too.

```toml
[app.legacy.release.r10]
kind = 'http'
profile = 'uwsgi'
command = ['.venv/bin/uwsgi', '--module', 'legacy.wsgi:application', '--processes', '4']
cwd = '/srv/legacy'
```

## Requests, logs and the daemon

Requests reach the server re-serialised as they reach an
[upstream](upstreams.md): the client's headers less hop-by-hop ones, with
`X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Real-IP`
and `X-Request-ID` added, bodies re-framed with a `Content-Length`, and
the path as the client sent it. The server's `X-Muru-Redirect` response
header is an [internal redirect](internal-redirects.md), as from an
application's workers. WebSocket upgrades are relayed. The access log
records each request under the application and release with `http` as
its group and the instance's address as its mount.

Each instance is started by a log relay of its own, which reads the
instance's standard output and error and writes them to the release's
log file, shown by `muructl logs APP`. Every line has the full UTC time
it was read in front, and a tag naming the instance, by its
`MURU_SERVER_INDEX`, and the stream: `muru.instance[0]:` for standard
error and `muru.instance[0].stdout:` for standard output.

```
2026-11-02T05:42:22.840Z muru.instance[0]: [2026-11-02 05:42:22 +0000] [48213] [INFO] Listening at: unix:/run/muru/h/7/server-0.sock
2026-11-02T05:42:22.851Z muru.instance[1]: [2026-11-02 05:42:22 +0000] [48214] [INFO] Listening at: unix:/run/muru/h/7/server-1.sock
```

The relays hold, cut and drop lines under the `[logs]` settings, and open
the file again by name when it is rotated or `muructl logs reopen`
runs, as [the logging guide](logging.md#managed-http-servers) describes,
so a rotation loses no lines. The stop signal and the kill at the end of
`shutdown_timeout` are sent by the instance's relay, which is the
instance's parent. A relay that exits takes its instance with it: what
is left of the instance is killed, and the instance is started again
with a new relay, as one that exits is.

A daemon that stops without `--shutdown`, or upgrades itself, leaves
the instances and their relays running, and the next daemon takes them
over from the record the daemon keeps in its state directory, one instance at a
time. An instance that is still running is taken over as it is, with
its relay. An instance that was waiting to be started again when the
daemon stopped is started again within a second at the same address,
and its waits carry on from the exits it had in a row. An instance that
exited while no daemon was running is counted as an exit when it is
taken over, with the exit status its relay recorded, and waits as any
exit does before it is started again. The release stays `ready`, and its
`release.adopted` event names the pids taken over, how each instance
that exited ended, and when it starts again:

```
re-adopted the server instances (pid 48213); instance 1 exited with exit status: 3 while no daemon ran and is started again in 1s
```

`server.started` follows when the instance has started:

```
instance 1 started again (pid 48377)
```

The record holds the pid of each instance and of its relay, each
together with the time the kernel gives for when that process started.
A process counts as still running only when the process that has its
pid now started at the recorded time. A pid that has passed to another
process since is not taken over, and that process is never signalled.

`muructl release list` shows a
managed server as its instance count and profile in the workers column.
`muructl release show` lists each instance with its pid and address, and
with `--json` as `server_instances`, each with `index`, `pid` (0 while it
waits to be started again), `address` and `started_unix_ms`:

```
command      .venv/bin/gunicorn shop.wsgi:application
instance 0   pid 48213, unix:/run/muru/h/7/server-0.sock
instance 1   pid 48214, unix:/run/muru/h/7/server-1.sock
```

## Cutting over

```console
$ muructl deploy legacy /srv/legacy --server gunicorn -- gunicorn legacy.wsgi:application
muru: release legacy@r1 is ready and serving
$ muructl deploy legacy /srv/legacy --hold
muru: release legacy@r2 is ready and held: not serving
$ muructl rollout canary legacy@r2 --steps 5,25,50 --hold 10m --wait
muru: rollout 1 promoted: every step passed its gates
$ muructl drain legacy@r1
```

The first release runs the deployment as it is. The second is the same
application on Muru's workers, held without traffic and checked through a
preview. The [canary](rollouts.md) moves clients to it step by step,
judged against the gunicorn release, and rolls back to it when a gate
fails. Once promoted, the gunicorn release drains and is kept warm until
it is drained for good, and nothing in the application's routes or hosts
changed along the way.
