# Services

A service is a named command the daemon keeps running: a cache, a queue
worker, a tunnel, an exporter. Muru starts it, starts it again when it
exits, by a policy the service states, keeps its output in a log of its
own and reports its state. It takes no requests from the edge. A server
that should take requests is a [managed HTTP server](managed-servers.md)
or an [upstream](upstreams.md).

## Declaring a service

A service is a `[service.NAME]` table in a file given to `muructl
apply`, beside the applications, hosts and routes of the node:

```toml
[service.redis]
description = 'Cache shared by shop and blog'
command = ['redis-server', '/etc/redis/muru.conf', '--daemonize', 'no']
user = 'redis'
readiness = { tcp = '127.0.0.1:6379', timeout = '15s' }
stop_timeout = '30s'
```

```console
$ muructl apply -f node.toml
muru: applied
  create-service  service redis            created
```

That is a complete definition. The service starts when it is applied
and whenever the daemon starts, is started again whenever it exits, and
is stopped with `TERM`.

| Key | Default | Meaning |
| --- | --- | --- |
| `description` | none | One line shown by `muructl service list` and `status`. |
| `command` | required | The program and its arguments, run without a shell. A bare name is found on `PATH`, and a relative path with a directory in it is taken from `cwd`. |
| `cwd` | a directory of the service's own under the state directory | The directory the command runs in. A relative path is taken from the file's directory. |
| `env` | none | Environment variables: values, or secret references as a release has them, resolved when the daemon starts the service. |
| `user`, `group`, `umask` | the service user and its primary group; the daemon's umask | Whom the instances run as, when the daemon runs as root. |
| `autostart` | `true` | Whether the service starts with the daemon. With `false` it is defined and listed, and runs only between `muructl service start` and a stop. |
| `instances` | `1` | How many copies of the command run. |
| `restart` | `always`, see below | What happens when an instance exits. |
| `readiness` | `{ after = '1s' }` | How an instance is known to be up. |
| `liveness` | none | A check that an instance still answers, which restarts one that does not. |
| `stop_signal` | `TERM` | The signal that asks an instance to stop: `TERM`, `INT`, `QUIT`, `HUP`, `USR1`, `USR2` or `WINCH`. |
| `stop_timeout` | `10s` | How long an instance asked to stop has before it is killed. |
| `stop_group` | `false` | Send the stop signal to the instance's whole process group, not its leader alone. The kill after `stop_timeout` always goes to the group. |
| `reload_signal` | none | The signal `muructl service signal NAME reload` sends. |

A service's name is lower-case letters, digits and hyphens, as an
application's is.

### What an instance is given

An instance starts from the same clean environment a release does: the
daemon's `PATH`, `HOME`, `USER`, `LANG`, `TZ` and the variables its
`pass_env` names, never the rest of the environment of whoever started
the daemon. Over that come the service's `env` and these:

| | |
| --- | --- |
| `MURU_SERVICE` | the service's name |
| `MURU_SERVICE_INDEX` | the instance's index, from 0 |
| `MURU_SERVICE_RUN_DIR` | a directory of the instance's own under the runtime directory, for a socket; it is removed when the service is removed and when the daemon shuts down |

In `command`, in the values of `env` and in the addresses of
`readiness`, `{index}` is replaced by the instance's index and
`{run_dir}` by the same directory as `MURU_SERVICE_RUN_DIR`. `{{` and
`}}` are literal braces.

Standard input is `/dev/null`. Standard output and standard error go to
the service's log. Each instance leads a session of its own, so a signal
sent to the daemon's process group or terminal does not reach it.

The program must stay in the foreground. One that puts itself in the
background exits at once and leaves its real process behind, which is
reported as a `service.daemonised` error: the processes it left are
stopped and the service is `failed`. Most servers have an option for
it, such as `--daemonize no` for Redis.

Secrets belong in `env`, which takes a secret by reference. `command`
takes none: a command line is visible to every user of the node. A
secret is resolved when the daemon starts the service: when it is
applied, when the daemon starts, and on `muructl service start` and
`muructl service restart`. An instance started again after an exit of
its own has the values its service was started with.

### More examples

A service that is defined and not started. It runs only between
`muructl service start pgbouncer-debug` and the matching stop:

```toml
[service.pgbouncer-debug]
description = 'A pgbouncer with verbose logging, for chasing pool problems'
command = ['pgbouncer', '-v', '/etc/pgbouncer/debug.ini']
user = 'postgres'
autostart = false
```

A tunnel that is retried without end, slowly, while the network is
down:

```toml
[service.tunnel]
command = ['cloudflared', 'tunnel', '--no-autoupdate', 'run', 'shop']
env = { TUNNEL_TOKEN = { secret = { store = 'node', key = 'cloudflared-token' } } }

[service.tunnel.restart]
policy = 'always'
delay = '2s'
max_delay = '5m'
```

A program that exits 0 when it has nothing left to do and then stays
down, and is started again if it crashes:

```toml
[service.importer]
command = ['/opt/importer/bin/importer', '--queue', 'incoming']
cwd = '/var/lib/importer'

[service.importer.restart]
policy = 'on-failure'
success_exit_codes = [0, 3]
give_up_after = 5
```

A step that runs once each time the daemon starts from nothing, and
whose result shows afterwards as `exited` or `failed`:

```toml
[service.warm-cache]
command = ['/usr/local/bin/warm-cache', '--all']
restart = { policy = 'never' }
```

Four copies of one command, each with a port of its own:

```toml
[service.thumbnailer]
command = ['thumbnailer', '--listen', '127.0.0.1:91{index}0']
instances = 4
readiness = { tcp = '127.0.0.1:91{index}0' }
stop_signal = 'INT'
```

A program whose whole process group is told to stop, and which rereads
its configuration on `HUP`:

```toml
[service.exporter]
command = ['multi-exporter', '--config', '/etc/exporter.yaml']
stop_group = true
reload_signal = 'HUP'
```

Muru gives a service no port. Instances that each need one work it out
from `{index}`, and nothing checks that two services do not choose the
same.

## States

Each instance is in one state, and a service's state is the least
healthy of its instances' states.

| State | Meaning | Leaves it by |
| --- | --- | --- |
| `stopped` | Not running, and nothing is due: never started (`autostart = false`), or stopped by hand. | `service start` |
| `starting` | The process is running and has not passed its readiness check. | the check passing, the process exiting, the readiness timeout |
| `running` | The process is running and ready. | the process exiting, a stop |
| `backoff` | The process exited and the next start is due at a known time. | the wait ending, `service start` (which starts it now), `service stop` |
| `stopping` | The stop signal was sent and the process has not gone. | the process exiting, the kill at `stop_timeout` |
| `exited` | The process ended cleanly and the policy does not start it again. | `service start` |
| `failed` | The process ended in failure and is not started again: the policy is `never`, or `give_up_after` was reached. | `service start`, a changed definition |

## Readiness

`readiness` says when a `starting` instance becomes `running`. It is
what `muructl service start --wait` waits for and what `muructl service
list` shows.

| Form | Ready when |
| --- | --- |
| `{ after = '1s' }` (the default) | the process has stayed up that long |
| `{ tcp = '127.0.0.1:6379' }` | a TCP connection to the address is accepted |
| `{ unix = '{run_dir}/control.sock' }` | a connection to the socket is accepted |
| `{ http = 'http://127.0.0.1:9187/metrics' }` | a GET answers 2xx or 3xx |
| `{ command = ['redis-cli', 'ping'] }` | the command, run as the service's user with its environment, exits 0 |

Every form takes `timeout` (`60s` by default), and the forms that probe
take `interval` (`500ms` by default). An instance that is not ready
within the timeout is stopped, as a stop stops it, with a
`service.not_ready` warning. That counts as a quick exit and a failure,
so the restart policy decides what follows.

## Liveness

A process can be running and no longer answering. `liveness` is a check
made for as long as an instance is `running`, and an instance that
fails it several times in a row is stopped and started again:

```toml
[service.broker]
command = ['broker', '--port', '5555']
liveness = { tcp = '127.0.0.1:5555', interval = '10s', timeout = '2s', failures = 3 }
```

The forms are those of `readiness` except `after`: `tcp`, `unix`,
`http` and `command`. `interval` is how often the check is made (`10s`
by default), `timeout` how long one check may take (`2s`), and
`failures` how many in a row it takes (`3`). The checks begin once the
instance is ready.

An instance that fails is a `service.unresponsive` warning. It is
stopped as a stop stops it, and that counts as a failure and a quick
exit, so the restart policy decides what follows, as for an instance
that was not ready in time.

## When an instance exits

The `restart` table says what happens. It is the table a [managed HTTP
server](managed-servers.md#the-restart-policy) has, with the same keys
and defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `policy` | `always` | `always` starts the instance again after any exit. `on-failure` starts it again after a failure and leaves a clean exit alone. `never` never starts it again. |
| `success_exit_codes` | `[0]` | The exit statuses that count as a clean exit. Any other status is a failure, as is an instance killed by a signal the daemon did not send. |
| `delay` | `1s` | The wait before the next start after a run that lasted, and after the first quick exit. |
| `max_delay` | `1m` | The longest wait. The wait doubles with each quick exit in a row up to this. |
| `stable_after` | `1m` | A run this long was not a quick exit, and clears the count of them. |
| `give_up_after` | none | Quick exits in a row after which the instance is left `failed`. Without it the instance is started again every `max_delay`, without end. |

An exit the daemon asked for, by a stop, a restart or a shutdown, is
not judged: the policy is for exits the program makes itself.

1. The exit is a clean one when its status is in `success_exit_codes`,
   and a failure otherwise. An instance that was not ready in time, and
   a command that could not be started at all, are failures.

2. A run shorter than `stable_after` is a quick exit and adds to the
   count of them in a row. A longer run clears the count. Only the
   length of the run matters here, so a program that exits 0 straight
   away under `always` backs off as one that crashes does.

3. The policy decides whether the instance is started again:

   | Policy | Clean exit | Failure |
   | --- | --- | --- |
   | `always` | started again | started again |
   | `on-failure` | `exited` | started again |
   | `never` | `exited` | `failed` |

4. An instance that would be started again is left `failed` when the
   count has reached `give_up_after`, with a `service.gave_up` event.

5. Otherwise it is in `backoff` until the wait ends: one second after a
   run that lasted and after the first quick exit, then two, four,
   eight, sixteen and thirty-two seconds, and a minute from the seventh
   on. Each wait is lengthened by up to a tenth, at random, so
   instances that failed together do not all start together.

A service that exits as soon as it starts, with every default:

```
12:00:00.0  started (pid 5101)                  starting
12:00:00.2  exited with exit status: 1          backoff, next start in 1s
12:00:01.2  started (pid 5107)                  starting
12:00:01.4  exited with exit status: 1          backoff, 2 quick exits in a row, next start in 2s
12:00:03.4  started, exited                     backoff, 3 quick exits in a row, next start in 4s
            service.crash_looping (error)
12:00:07.6  started, exited                     backoff, 4 quick exits in a row, next start in 8s
            and so on, to once a minute, until it stays up, is stopped or its definition changes
```

The third quick exit in a row is a `service.crash_looping` event at
`error` level, once for each loop. `muructl service list` shows the
service in `backoff` with its restarts, and
`muru_service_restarts_total` counts them. With `give_up_after = 5` the
fifth quick exit is the last: the service is `failed`, and stays so
through restarts of the daemon, until `muructl service start` or a
changed definition.

A service that crashes once a day never counts a quick exit, and is
started again a second later each time.

## Commands

Every command takes `--json`. `status`, `start`, `stop` and `restart`
take several names, or `--all` for every service.

### `muructl service list`

```console
$ muructl service list
SERVICE          STATE     UP    PID     SINCE     RESTARTS  COMMAND
exporter         running   1/1   48211   3d 4h     0         multi-exporter --config /etc/exporter.yaml
importer         exited    0/1   -       12m       0         /opt/importer/bin/importer --queue incoming
pgbouncer-debug  stopped   0/1   -       -         0         pgbouncer -v /etc/pgbouncer/debug.ini
redis            running   1/1   48213   3d 4h     0         redis-server /etc/redis/muru.conf --daemonize no
thumbnailer      backoff   3/4   -       40s       7         thumbnailer --listen 127.0.0.1:91{index}0
tunnel           running   1/1   50377   6h        2         cloudflared tunnel --no-autoupdate run shop
```

`UP` is the instances running over the instances declared. `PID` is
shown for a service of one instance. `SINCE` is how long the service
has been in its state. `RESTARTS` counts the starts that followed an
exit since the daemon first started the service, not the starts made by
hand.

`--state backoff,failed` lists the services in those states, and with
`--quiet` prints nothing and exits 1 when any service matches, for a
monitoring check.

### `muructl service status`

```console
$ muructl service status thumbnailer
service      thumbnailer
state        backoff since 2026-11-06T09:41:20Z (40s)
command      thumbnailer --listen 127.0.0.1:91{index}0
cwd          /var/lib/muru/services/thumbnailer
autostart    yes
restart      always, 1s to 60s, stable after 60s
readiness    tcp 127.0.0.1:91{index}0 within 60s
stop         INT, killed after 10s
log          /var/log/muru/services/thumbnailer.log
instance 0   running, pid 61002, up 2d 1h
instance 1   running, pid 61003, up 2d 1h
instance 2   backoff, starting again in 22s after 7 quick exits in a row
instance 3   running, pid 61005, up 2d 1h

last exits of instance 2
  09:39:16  exited with exit status: 1 after 1s
  09:39:48  exited with exit status: 1 after 1s
  09:40:20  exited with exit status: 1 after 1s
  09:41:20  exited with exit status: 1 after 1s
```

The `user` line is there when the table names one. The times an
instance ran for are as the daemon saw them, which it does once a
second.

The last five exits of each instance are listed, and are kept when the
daemon restarts. The [events](events.md) have every one.

`status` exits 0 when the service is `running`, 3 when it is not, and 4
when there is no such service. With several names it prints each and
exits with the worst.

### `muructl service start`

```console
$ muructl service start pgbouncer-debug
muru: pgbouncer-debug started (pid 70113)
$ muructl service start pgbouncer-debug --wait
muru: pgbouncer-debug is running (pid 70113)
$ muructl service start redis
muru: redis is already running (pid 48213)
```

`start` puts a `stopped`, `exited` or `failed` service back under its
policy with its count of quick exits cleared, and starts an instance in
`backoff` now. `--wait` returns when every instance is `running` (exit
0), when the service reaches `failed` or `exited` (exit 1), or at
`--timeout` (exit 2). `--all` starts every service that is not running,
those with `autostart = false` apart.

### `muructl service stop`

```console
$ muructl service stop tunnel
muru: tunnel stopped
$ muructl service stop thumbnailer --index 2
muru: thumbnailer instance 2 stopped; the other 3 keep running
$ muructl service stop redis --timeout 2m
$ muructl service stop redis --kill
```

A service stopped by hand stays stopped, through restarts of the
daemon and reboots, until `muructl service start`. `--timeout` replaces
`stop_timeout` for this stop, and `--kill` skips the stop signal.
`stop` returns once the processes are gone. An instance that does not
stop within its timeout is killed with its whole process group, with a
`service.killed` warning.

### `muructl service restart`

```console
$ muructl service restart redis
muru: redis restarted (pid 70201)
$ muructl service restart thumbnailer --rolling
$ muructl service restart thumbnailer --index 2
```

`restart` stops every instance and then starts every one, and works on
a `stopped` or `failed` service as `start` does. With `--rolling` the
instances are restarted one at a time, each ready before the next is
stopped. A restart by hand counts neither as a restart in `RESTARTS`
nor as a quick exit.

### `muructl service logs`

```console
$ muructl service logs redis
2026-11-06T02:10:44.101Z muru.service[0].stdout: 48213:M 03 Nov 2026 02:10:44.101 * Ready to accept connections tcp
$ muructl service logs thumbnailer --follow --lines 20
```

`--lines N` (100 by default) and `--follow` behave as they do for
`muructl logs`, through a rotation of the file.

### `muructl service signal`

```console
$ muructl service signal exporter reload
muru: sent HUP to exporter (pid 48211)
$ muructl service signal thumbnailer USR1 --index 1
muru: sent USR1 to thumbnailer instance 1 (pid 70214)
```

`reload` stands for the service's `reload_signal`. `KILL` and `STOP`
are refused: a kill goes through `muructl service stop --kill`, so that
the daemon knows it was asked for.

### Elsewhere

`muructl status` has a line for the services, which names those that
are not running or stopped:

```
services 6 running, 1 backoff (thumbnailer), 1 stopped
```

`muructl status --processes` shows each instance and the process that
carries its output, the daemon's [dashboard](dashboard.md) lists the
services, `muructl logs list` lists their logs, and the [MCP
server](../reference/mcp-tools.md) has tools that list, show, start,
stop and restart them.

A service that answers HTTP can be the address of an
[upstream](upstreams.md), which routes requests to it. That gives a
server Muru supervises and proxies to with none of a release's
lifecycle: no deploys, traffic shares, drains or rollbacks. A server
that should have those is a [managed HTTP
server](managed-servers.md).

## Logs

Each service has a log, `services/NAME.log` in the log directory, which
holds the output of every instance and continues across restarts.
Every line has the time it was read and a tag naming the instance and
the stream: `muru.service[0]:` for standard error and
`muru.service[0].stdout:` for standard output.

The output is carried by a process of its own for each instance, not
by the daemon, so an instance keeps writing while the daemon is
stopped, restarted or upgraded.

The daemon's [log rotation](logging.md) covers these files with the
`[logs]` policy. With `rotate = 'external'`, the files are reopened
when they are renamed or on `muructl logs reopen`, as a release's log
is.

## Changing a definition

Services are part of what the files declare: `muructl apply` makes the
node match them.

| Change | Effect |
| --- | --- |
| A new `[service.NAME]` | recorded, and started if `autostart = true` |
| A removed table, with `--prune` | stopped and removed; its log is left |
| `command`, `cwd`, `env`, `user`, `group`, `umask`, `readiness`, `stop_signal` or `stop_group` changed | every instance stopped and started again |
| `instances` raised or lowered | instances started, or the highest numbered stopped; the others are not touched |
| `restart`, `stop_timeout`, `reload_signal` or `description` changed | in force at once, nothing restarted |
| `autostart` turned on | started, unless it was stopped by hand |
| `autostart` turned off | nothing: a running service keeps running until it is stopped |

`--dry-run` says what an apply would do before it does it:

```console
$ muructl apply -f node.toml --prune --dry-run
muru: plan (nothing changed)
  update-service  service tunnel           env changed; its 1 instance is restarted
  update-service  service thumbnailer      instances 4 to 6; 2 instances are started
  remove-service  service old-agent        not declared; its instances are stopped
```

A `failed` service whose definition changes is started again. A
service stopped by hand stays stopped. A secret that has changed in its
store reaches a service when `muructl service restart` restarts it.

## The daemon stopping and starting

`muructl daemon stop`, `muructl daemon restart` and `muructl daemon
upgrade` leave the services running, as they leave the applications,
and the daemon that starts next takes them over. While no daemon runs
nothing is started again: an instance that exits then stays down until
the next daemon starts, which reports how it ended and applies the
policy.

A daemon that starts from nothing, as after a reboot, starts every
service that has `autostart = true` and was not stopped by hand, before
it starts the applications. It does not wait for them: an application
that needs a service at its start and does not find it is [tried
again](daemon.md#stopping-and-restarting).

`muructl daemon stop --shutdown` stops the services after the
applications, since an application uses them until its last request.
Each has its `stop_timeout` or what is left of the shutdown's `budget`,
whichever is shorter, so a service with a long `stop_timeout` wants a
budget to match. The daemon says so in its log when a service's
`stop_timeout` is longer than the budget.

## Refusing services on a node

A service is a command the node runs for as long as it is declared,
and a project's services are named by its own settings. The daemon's
own file says what a node accepts:

```toml
[daemon]
services = 'node'
```

| Value | Meaning |
| --- | --- |
| `all` (the default) | The node's services and those its applications' projects declare are run. |
| `node` | Only `[service.NAME]` tables in applied files are run. A release that declares services is refused when it is deployed or applied. |
| `none` | No service is run. An applied file with a `[service.NAME]` table is refused, as is a release that declares services. |

The refusal names the setting. A change to the setting is taken by a
reload, and is for what is applied or deployed from then on: a release
deployed before keeps its services, and a service already applied runs
on until a file with its table is applied again.

## Events

The subject of each event is the service's name.

| Event | Level | When |
| --- | --- | --- |
| `service.started` | info | an instance's process was started |
| `service.ready` | info | an instance passed its readiness check |
| `service.exited` | info for a clean exit the policy leaves alone, warning otherwise | an instance exited by itself, with what happens next |
| `service.failed` | error | the command could not be started |
| `service.not_ready` | warning | an instance was not ready in time and was stopped |
| `service.unresponsive` | warning | an instance failed its liveness check and is stopped |
| `service.crash_looping` | error | the third quick exit in a row |
| `service.gave_up` | error | `give_up_after` was reached, and the instance is `failed` |
| `service.daemonised` | error | the program put itself in the background |
| `service.stopped` | info | an instance asked to stop has stopped |
| `service.restart_stopped` | error | a restart one at a time ended early, an instance not having come back |
| `service.killed` | warning | an instance did not stop within its timeout and was killed, or was killed with `--kill` |
| `service.adopted` | info | a daemon took over the running instances |
| `service.changed` | info | a definition was added, changed or removed, with what that did |
| `service.user_is_root` | warning | a service started as root |
| `service.user_not_applied` | warning | a service names a user and the daemon is not root, so its instances run as the daemon does |

## Metrics

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `muru_service_instances` | gauge | service, state | instances in each state |
| `muru_service_up` | gauge | service | 1 when every instance is `running` |
| `muru_service_restarts_total` | counter | service | starts that followed an exit |
| `muru_service_exits_total` | counter | service, outcome | exits the program made: `clean` or `failed` |
| `muru_service_start_time_seconds` | gauge | service, instance | when the instance's process started |

## A project's services

A project declares the long-running processes that belong with its
code, such as a queue's workers or a scheduler, in its own settings.
They run the same release as its web workers, with the same settings
and secrets, and are replaced when the release is.

```toml
# pyproject.toml
[tool.muru]
entrypoint = 'shop.wsgi:application'

[tool.muru.services.worker]
description = 'Task queue workers'
command = ['celery', '-A', 'shop', 'worker', '--hostname', 'worker{index}@%h']
instances = 4
stop_timeout = '2m'
switch = 'start-first'

[tool.muru.services.beat]
description = 'Periodic task scheduler'
command = ['celery', '-A', 'shop', 'beat', '--schedule', '{run_dir}/beat-schedule']
restart = { give_up_after = 5 }

[tool.muru.services.reindex]
description = 'Rebuild the search index; started by hand'
command = ['python', 'manage.py', 'reindex', '--watch']
autostart = false
```

The keys are those of a node's service, with three differences. There
is no `user`, `group` or `umask`: the instances run as the application
does. `cwd` is the release's source directory, or a path relative to
it. And there are two more keys:

| Key | Default | Meaning |
| --- | --- | --- |
| `switch` | `stop-first` | What happens when another release takes over. `stop-first` stops the old release's instances before the new release's are started, so there are never two. `start-first` starts the new release's and stops the old ones once the new are ready, so there is no gap. |
| `on_idle` | `run` | With `stop`, the service stops when the application's host is stopped for idleness and starts when a request wakes it. |

The services are fixed with each release, as its commands are. A
release declared in an applied file may carry tables of its own, which
replace the project's:

```toml
[app.shop.release.r7]
source = '/srv/shop'

[app.shop.release.r7.services.worker]
command = ['celery', '-A', 'shop', 'worker', '--concurrency', '8']
instances = 2
```

A [managed HTTP server](managed-servers.md) release carries its
services the same way, so the worker beside a gunicorn that is being
cut over comes along unchanged.

### The node's own settings for a service

The node's operator can change some of what a project declared without
a new release of its code. These are settings of the application, as
its worker counts and variables are: every release deployed from then
on takes them over the project's own.

```toml
[app.shop.service.worker]
instances = 8

[app.shop.service.beat]
autostart = false           # this node does not run the scheduler
```

`autostart`, `instances`, `restart`, `stop_timeout`, `on_idle` and `env`
can be set there. The command is the release's. The same by command:

```console
$ muructl app set shop --service worker.instances=8 --service beat.autostart=false
$ muructl app unset shop service.worker.instances
$ muructl app unset shop service.beat
```

`--service` takes `restart.KEY` for a key of the restart table and
`env.NAME` for a variable. `muructl app show` lists what is set, and an
export writes it back. A service the serving release does not declare
has nothing to change. The settings reach the next release deployed,
not the one that serves.

### What an instance runs with

What a [command](commands.md) of the release runs with: the release's
environment with its secrets, the directory of its interpreter first on
`PATH`, `MURU_APP`, `MURU_RELEASE` and `MURU_RELEASE_DIR`, and the
application's user. `python` as the first word of `command` is the
release's interpreter. The service's own `env` and `MURU_SERVICE`,
`MURU_SERVICE_INDEX` and `MURU_SERVICE_RUN_DIR` come over all of it.

### Which release's services run

An application's services are those of its serving release, the one a
plain deploy or a promotion made serving. They are named with the
application, as `shop/worker`:

```console
$ muructl service list --app shop
SERVICE       STATE     UP    PID    SINCE  RESTARTS  COMMAND
shop/beat     running   1/1   71002  12s    0         celery -A shop beat --schedule {run_dir}/beat-schedule
shop/reindex  stopped   0/1   -      -      0         python manage.py reindex --watch
shop/worker   running   4/4   -      14s    0         celery -A shop worker --hostname worker{index}@%h
```

| Moment | What happens to the services |
| --- | --- |
| A first deploy | started once the release is serving |
| A later deploy, a promotion | when the new release takes over, each service changes over as its `switch` says |
| `deploy --hold`, a preview | the held release runs no services |
| A canary or blue/green rollout under way | the release being replaced keeps its services, and the candidate runs none until it is promoted |
| A rollback | the services go back with the traffic |
| A release kept warm | its services are stopped |
| `muructl drain` of the serving release | its services stop with it |
| A new release that no longer declares a service | the old release's service stops and nothing takes its place |
| Maintenance mode, a group scaled to zero | no change |
| The host stopped for idleness | no change, unless the service has `on_idle = 'stop'` |
| A release whose host fails and is started again | its services run on |

A service stopped with `muructl service stop shop/worker` stays stopped
when another release takes over, until `muructl service start`.

A deploy does not fail because a service does: the release is serving
once its workers are, and a service that will not start is in `backoff`
on a serving release. `muructl deploy --wait-services` waits for every
service that starts automatically to be `running`, and exits with an
error when one is not within its readiness timeout. The release is
serving all the same.

During a canary two releases take requests and one runs the workers, so
a task the candidate enqueues is run by the old release's code, and the
other way round after the promotion for tasks still queued. Tasks have
to be compatible across adjacent releases.

The log of a project's service is `apps/APP/services/NAME.log` in the
log directory, and continues across releases.

## In a container

Under `muructl serve` the project's services run beside the application
in the container, with their output on standard output with every other
line:

```console
$ docker run registry.example.com/shop
muru: serving shop.wsgi:application on http://0.0.0.0:8080
muru: service worker instance 0 started (pid 15)
muru.service.worker[0]: worker ready, polling the queue
```

Which of them run is a setting, so that one image serves several roles:
`--services`, `MURU_SERVE_SERVICES` or `services` in the serve table
takes `all` (the default), `none`, or the names.

```console
$ docker run -e MURU_SERVE_SERVICES=none registry.example.com/shop
$ docker run -e MURU_SERVE_SERVICES=worker registry.example.com/shop
```

A service can be tied to the container's health:

```toml
[tool.muru.services.litestream]
command = ['litestream', 'replicate', '-config', '/app/litestream.yml']
required = true
```

With `required = true` the startup probe waits for the service to be
ready, the readiness probe fails while it is not `running`, and when it
is `failed` muructl shuts down and exits with status 1, so that
whatever runs the container replaces it. A required service gives up
after five quick exits in a row unless its `restart.give_up_after` says
otherwise. A service that is not required never affects the probes.

On `SIGTERM` the services are stopped after the application, within the
shutdown budget, each with its `stop_timeout` or what is left. Nothing
is kept: a service stopped by hand is started again with the next
container.

The `muructl service` commands work inside the container. `muructl
service logs` prints the last lines muructl has kept, a thousand for
each service.

A worker that should scale apart from the web side belongs in a
container of its own, whose command is `muructl command run`, and
needs no service.

## In development

`muructl run` starts the project's services that start automatically
beside the application, prints their lines, restarts them when a
reload starts a new release, so that a worker picks up the edited code,
and stops them on Ctrl-C. `--no-services`, or `services = false` in the
run table, turns that off, and `--services worker` picks some.

## Limits

- A program that puts itself in the background and also starts a
  session of its own leaves nothing the daemon can find.

- A process an instance starts in a session of its own is not stopped
  with it.

- Nothing limits a service's memory or counts it against the node's
  budget.

- Services start in no stated order, and one cannot name another that
  it needs.

- A service that runs for longer than `stable_after` before each crash
  is started again a second later every time, and `give_up_after` never
  applies to it. `muru_service_restarts_total` shows it.
