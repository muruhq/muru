# A worker that needs a resource to itself

Some applications can run only one copy at a time: the worker holds a
lock, a device, a port or a store with a single writer, and a second
worker cannot start while the first has it. Muru starts the new beside
the old wherever it replaces something, a worker or a release, so such
an application needs two things: a release whose one worker is always
stopped before its replacement starts, and a deploy done by hand that
stops the old release before the new one starts.

There is a gap in service each time, for as long as a worker takes to
stop and its replacement to start. Requests that arrive in it wait at
the edge for the new worker, and are answered `503` when it takes
longer than the [queue timeout](timeouts.md).

## Take the resource in the worker

A release's code is imported once, in the release's host, and the
workers are forked from it ([preloading](../explanation/preload.md)).
The host runs for as long as the release does, so a resource taken
when a module is imported is held by the host, and is still held while
a worker is replaced. Take it in the worker instead, in a `post_fork`
[hook](../reference/python-api.md#lifecycle-hooks), which runs in each
worker after the fork and before it serves:

```python
# ledger/hooks.py
import fcntl

held = None


def post_fork(context):
    global held
    held = open("/var/lib/ledger/ledger.lock", "a+")
    fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
```

A worker whose hook fails does not start, and a release whose worker
fails three times at start has failed, which is what happens to a
second copy started while the first holds the lock:

```console
$ muructl deploy ledger /srv/ledger-2 --hold
muru: error: the host exited with exit status: 1 before its workers were ready: BlockingIOError: [Errno 35] Resource temporarily unavailable
```

The resource is given up when the worker exits. Give it up sooner, in
an `on_exit` hook, only if something outside the process has to be told.

## The release: one worker, stopped first

```toml
[tool.muru]
entrypoint = "ledger.wsgi:application"
hooks = "ledger.hooks"
processes = 1
max_surge = 0
max_unavailable = 1
```

`processes = 1` with no scaling policy is a fixed count of one worker.
With [`max_surge = 0` and `max_unavailable = 1`](recycling.md#how-many-at-once-and-in-which-order)
every replacement of that worker stops the old one first and starts
the new one only when it has exited: a recycle for `max_requests`,
`max_lifetime` or the soft memory limit, a restart by `muructl
restart`, and the replacements that cannot wait, a worker past its
[hard timeout](timeouts.md), one whose event loop or interpreter has
stalled, one over its hard memory limit and one the
[memory guard](containers.md#the-memory-guard) chooses. A worker that
crashes has exited already when it is started again.

`muructl release show` confirms the order, and the log says so at each
replacement:

```console
$ muructl release show ledger@r1
...
workers      1 process of 4 threads
swaps        max_surge 0, max_unavailable 1 (the old worker stops first)
$ muructl restart ledger --worker 0
muru: worker 0 (pid 72992) is stopping, and is replaced once it has stopped
muru: worker 0 is served by pid 73290; pid 72992 has stopped
```

```
worker 0 (pid 72992) stops before its replacement starts: max_surge of the group default is 0
worker 0 started as pid 73290
worker.restarted: worker 0 (pid 72992) replaced by pid 73290: restarted on request; it stopped before its replacement started
```

A worker past its hard timeout is given its
[`hard_drain_grace`](timeouts.md) to finish its other requests before
it is stopped, so its replacement starts that much later than it would
with a surge. Shorten the grace when the gap matters more than those
requests.

## Deploying, by hand

`muructl deploy` starts the new release while the old one serves, and
so do a held release, a [preview](traffic.md#previews), a
[canary](rollouts.md) and a blue/green switch: each runs a second
copy, and none of them is for this application. The deploy is four
commands instead.

```console
$ muructl maintenance on ledger
muru: ledger is in maintenance: clients get 503 maintenance until `muructl maintenance off ledger`
$ muructl drain ledger@r1
muru: release ledger@r1 is stopped
$ muructl deploy ledger /srv/ledger-2
muru: release ledger@r2 is ready and serving
$ muructl maintenance off ledger
muru: ledger serves as usual again
```

1. [Maintenance mode](error-pages.md#maintenance-mode) answers every
   request for the application with `503` and `Retry-After` at the
   edge, with its maintenance page, so clients are told what is
   happening and nothing queues for a release that is about to stop.

2. `muructl drain` takes the serving release out of traffic, lets its
   requests in flight finish and stops it: its worker and its host
   both exit, so nothing of the release is left holding anything. The
   command returns once they have.

3. `muructl deploy` starts the new release, which now finds the
   resource free, and gives it the traffic. No release is kept warm
   behind it, since the old one has stopped already.

4. With maintenance off, requests reach the new release.

### When the new release does not start

The deploy fails, the application has no release running, and the
maintenance page is still up:

```console
$ muructl deploy ledger /srv/ledger-2
muru: error: the host exited with exit status: 1 before its workers were ready: cannot load ledger.wsgi:application: RuntimeError: broken release
```

Either start the release that served before, which `muructl rollback`
does, and turn maintenance off:

```console
$ muructl rollback ledger
muru: rolled back to ledger@r1 (ready)
$ muructl maintenance off ledger
```

or fix the code and deploy again, with maintenance left on until that
deploy has succeeded. `muructl release list ledger` shows the failed
release with why it failed, and its log has the traceback.

After a rollback the old release serves again, so the next attempt
starts from the top: maintenance on, drain, deploy.
