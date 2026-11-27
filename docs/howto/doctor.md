# Check Muru itself

`muructl doctor` says what is wrong with Muru itself, as opposed to your
application: a daemon that runs and is not listed, a socket nothing is
behind, a record of a process that is gone, a state store that does not
add up. It reads and never changes anything, works with no daemon
running, and exits 1 when anything it checked is wrong.

```console
$ muructl doctor
SOCKETS
/run/muru/admin.sock               admin API                                  daemon pid 4120   answering
/run/muru/e/edge-1.sock            daemon to edge control                     edge pid 4133     answering
/run/muru/e/edge-1-listener.sock   edge hands its listeners to a new daemon   edge pid 4133     owner alive
/run/muru/h/7/control.sock         daemon to host, shop@r4                    host pid 4190     owner alive
/run/muru/h/7/output.sock          output of shop@r4, to its log relay        host pid 4190     owner alive
/run/muru/h/7/relay.sock           daemon to the log relay of shop@r4         relay pid 4189    answering
/run/muru/h/7/worker-0.sock        requests to shop@r4 worker 0               pid 4201          answering
/run/muru/h/7/worker-1.sock        requests to shop@r4 worker 1               pid 4202          answering

FILES
/etc/muru/muru.toml                                             configuration                                present
/var/lib/muru/muru.sqlite                                       state store                                  schema 22, 2.1MB
/var/lib/muru/daemon.lock                                       lock on the state directory                  held by pid 4120
/var/lib/muru/instance-id                                       the daemon's instance ID                     7cd3a1f0
/run/muru/registry/7cd3a1f09b2e4c6d8a1f3e5b7c9d0e2f             registry entry                               locked by pid 4120
/var/lib/muru/run/daemon.pid                                    the background daemon's pid                  pid 4120
/var/lib/muru/run/edge.toml                                     the edge, for the next daemon to take over   edge pid 4133, generation 1
/var/lib/muru/run/edge/rate-buckets-v1                          the rate limits' counts                      2.0MB
/var/lib/muru/run/h/7/host.toml                                 the host of shop@r4, for the next daemon     pid 4190
/var/lib/muru/run/h/7/relay.toml                                a log relay of shop@r4                       pid 4189
/var/log/muru                                                   logs                                         38 files, 412.0MB
/var/lib/muru/envs                                              environments built for releases              4810 files, 96.2MB

CHECKS
ok      registry      the daemon (pid 4120) holds its entry 7cd3a1f0
ok      locks         the daemon (pid 4120) holds the state and run directories
ok      paths         the longest of 8 socket paths is 32 bytes of the 107 allowed, and the run directory holds sockets only
ok      store         schema 22, as this muructl expects; muructl doctor store checks it in full
ok      releases      3 releases in the store, 1 with a host or instances that run
ok      edge          the edge (pid 4133) serves snapshot 12, the one the daemon last sent
ok      versions      the daemon and its edge run version 1.0.0, as this muructl is, and so does the runtime library of 1 hosts
ok      disk          each location belongs to root and has room: state 41.2GB, run 1.9GB, log 41.2GB, cache 41.2GB free
ok      budget        the node's memory budget is 12.0GB, from the daemon's configuration file
```

It checks one installation: the daemon whose locations the command's
flags and the environment give, as `muructl daemon start` takes them
(`--system`, `--instance`, `--state-dir`, `--run-dir`, `--log-dir`,
`--cache-dir` and `--config`), and the development runs under its run
directory. A daemon that runs says where its files are, so for one
started with locations of its own the run directory is enough:

```
muructl doctor --run-dir /srv/shop/run
```

`--json` prints one document with `ok`, `sockets`, `files` and `checks`.

## Sockets and files

Each socket and each file is listed with what it is for, who owns it and
its condition. The list comes from what Muru knows of its own layout,
which the [daemon guide](daemon.md#sockets-and-files) shows: the
daemon's record of its edge names the edge's sockets, each host's record
its workers', the state store which release a host's number is. What a
directory holds beyond that is listed too, as `(not accounted for)`,
`(nothing owns it)` or `(not in the record of shop@r4)`.

A socket's condition is one of:

| Condition | Means |
| --- | --- |
| `answering` | Something accepted a connection. The admin socket was asked for its status; the others were connected to and closed, with nothing sent. |
| `owner alive` | The socket was not connected to, and the process its record names is running. |
| `STALE` | The socket is there and nothing is behind it: nothing answers, its owner is gone, or no record names it. |
| `ABSENT` | A record names the socket and it is not there. |

A host's `control.sock` and `output.sock`, and the edge's listener
socket, are never connected to. A host takes a new connection on either
of its two as its daemon or its log relay and drops the one it had, and
the edge hands its listeners to whoever connects, so their condition is
whether their owner is alive.

A file whose record names a process that is gone is `STALE`, as is a
development run's directory of files that no live run names.

## The checks

| Check | Passes when |
| --- | --- |
| `registry` | The daemon's entry in the registry of running instances is held by the daemon's process, and each development run under the run directory holds its own. A daemon that stopped and left its edge and hosts serving passes, as detached. |
| `locks` | The lock on the state directory, the process that answers on the admin socket and the daemon's process are one, and the pid file agrees. With no daemon, nothing holds the lock on either directory. |
| `paths` | The run directory leaves room for the longest socket path, no socket's path is past the platform's limit, the run directory holds no regular file and nothing a daemon of an earlier version left, and neither the state directory nor the registry is under a temporary directory. |
| `store` | The state store opens and its schema version is one this `muructl` reads. |
| `releases` | No host's record names a process that is gone for a release the store says runs, no host runs for a release the store says is stopped, and every host's files belong to a release the store holds. |
| `edge` | The edge that answers is the process the daemon and its record name, and the routing it serves is the routing the daemon last sent it. |
| `versions` | The daemon and its edge run the version of this `muructl`. A release keeps the runtime library it started with through an [upgrade](upgrading.md#releases), so the releases on another version are named and are no failure. |
| `disk` | Each of the state, run, log and cache directories belongs to the daemon's user, who may write to it, no other user may write to the run directory, and each has 64MB free or more. |
| `budget` | Always: it says where the node's memory limit comes from, and warns when there is none. |

A check that fails says what it found, and names the command that puts
it right where one does:

```console
$ muructl doctor
...
CHECKS
FAILED  registry      the daemon (pid 4120) runs and has no live entry in /run/muru/registry, so muructl instance list does not show it; see muructl daemon restart
ok      locks         the daemon (pid 4120) holds the state and run directories
FAILED  paths         /run/muru/h/7/notes.txt is a regular file in the run directory, which holds sockets only
ok      store         schema 22, as this muructl expects; muructl doctor store checks it in full
ok      releases      3 releases in the store, 1 with a host or instances that run
ok      edge          the edge (pid 4133) serves snapshot 12, the one the daemon last sent
FAILED  versions      the daemon (pid 4120) runs version 1.0.0, and this muructl is version 1.1.0; see muructl daemon upgrade
ok      disk          each location belongs to root and has room: state 41.2GB, run 1.9GB, log 41.2GB, cache 41.2GB free
ok      budget        the node's memory budget is 12.0GB, from the daemon's configuration file
$ echo $?
1
```

| Named | Does |
| --- | --- |
| `muructl daemon restart` | Runs the daemon again in place, which registers it again and writes its pid file; the edge and the releases keep serving. |
| `muructl daemon start` | Starts a daemon, which takes over what still runs, starts the serving releases that do not, and removes the files of hosts that are gone. |
| `muructl instance prune` | Removes registry entries nothing runs behind, with a development run's directories. |
| `muructl instance list --orphans` | Lists Muru's processes that no running instance accounts for, which `muructl instance prune` stops. |
| `muructl daemon reset` | Removes what a daemon that no longer runs left in its directories. |
| `muructl daemon upgrade` | Runs the daemon again from the installed `muructl` and replaces its edge, with nothing refused in between. |
| `muructl release prune` | Removes releases that no longer serve, with their environments and logs, for the room. |

Doctor repairs nothing itself.

## A node with no memory limit

The [node's budget](scaling.md#the-nodes-budget) limits memory only
when it is given a limit: `memory` in the `[daemon.budget]` table of
the daemon's configuration file, or else a memory limit on the systemd
unit or the container the daemon runs in. A daemon with neither has no
memory budget, and doctor says so:

```
warning budget        the node has no memory limit, so every scale-out is admitted and every release starts whatever memory the machine has, no worker is replaced for the node's memory, and what stops a node that takes too much is swap or the kernel's OOM killer: set memory in the [daemon.budget] table of the daemon's configuration file, or a memory limit on the unit or container the daemon runs in; see muructl daemon config set daemon.budget.memory SIZE
```

It is a warning and not a failure: doctor still exits 0, and in the
JSON document the check has `ok` true and `warning` true. Muru takes no
limit from the machine's own memory. To give the node one:

```
muructl daemon config set daemon.budget.memory 12GB
```

or, under systemd, `MemoryMax=` in the daemon's unit, or the memory
limit of its container. With no daemon running, doctor reads the
configuration file alone, and says that a limit of the unit or
container is known once the daemon runs.

## The state store

`muructl doctor store` opens the state store read-only, whether or not
the daemon runs, and checks it:

```console
$ muructl doctor store
store    /var/lib/muru/muru.sqlite (2.1MB, write-ahead log 531.1KB)

TABLES
apps                  2
events                4311
releases              3
routes                4
...

CHECKS
ok      integrity     SQLite finds the database sound
ok      foreign keys  every row names rows that exist
ok      schema        version 22, as this muructl expects
ok      rule          each release belongs to an application
ok      rule          each application's serving and previous release exist
ok      rule          scale pins, gates and import reports belong to a release
ok      rule          rollouts, runs and maintenance belong to an application
ok      rule          each release has a number of its own, none past the count
ok      rule          event sequence numbers have no gaps
ok      rule          no route, host, overlay, rate limit, worker group or rollout names something removed
```

- `integrity` and `foreign keys` are SQLite's own `integrity_check` and
  `foreign_key_check`.

- `schema` compares the version the store records with the one this
  `muructl` writes. An older store fails with a note that a daemon of
  this version brings it up to date when it starts.

- Each `rule` is one the daemon keeps whenever it writes, so a rule that
  fails is a fault in Muru and never something a command of yours left.
  The line names the rows that break it:

  ```
  FAILED  rule          each application's serving and previous release exist: application shop serves r9, which is no release
  ```

`--json` prints the same as one document, with `path`, `bytes`,
`wal_bytes`, `schema_version`, `expected_schema_version`, `tables` and
`checks`.

To look at a store elsewhere, `--dump-schema` prints its schema as SQL
and `--copy FILE` writes a copy of the whole store:

```console
$ muructl doctor store --copy /tmp/muru-copy.sqlite
muru: copied /var/lib/muru/muru.sqlite to /tmp/muru-copy.sqlite (2.4MB)
$ sqlite3 /tmp/muru-copy.sqlite 'select app, id, state from releases'
shop|r4|ready
shop|r3|retained
blog|r1|ready
```

The copy is one consistent database, taken while the daemon goes on
using the store, with no write-ahead log beside it. It is never written
over a file that exists. It leaves out the releases' [preview
secrets](traffic.md#the-preview-secret), and a daemon started on a copy
gives each release a new one.

## What happened last

`muructl doctor timeline` puts three records of the recent past in one
order: the events the daemon keeps, the last lines of its own log, and
when each of its processes started. It works with no daemon running.

```console
$ muructl doctor timeline --limit 8
2026-11-02T05:40:11.204Z  process  muru daemon system started (pid 4120)
2026-11-02T05:40:11.911Z  process  muru edge system started (pid 4133)
2026-11-02T05:40:11.960Z  event    daemon.started daemon: serving on http://0.0.0.0:80, admin socket /run/muru/admin.sock
2026-11-02T05:42:22.031Z  event    release.created shop@r4: from /srv/shop
2026-11-02T05:42:22.840Z  process  muru host shop@r4 (system) started (pid 4190)
2026-11-02T05:42:23.118Z  event    release.ready shop@r4: 2 workers ready in 0.3 s
2026-11-02T05:42:23.120Z  event    traffic.switched shop@r4: traffic moved to the release
2026-11-02T06:10:04.552Z  log      muru: warning: the edge did not apply the snapshot within 30 s
```

Each line has its time in UTC, where it is from (`event`, `log` or
`process`) and what happened. `--limit N` prints the latest N, 50 when
left out, and `--json` prints `{"timeline": [...]}`.

## On a bundle from another machine

[`muructl debug bundle`](debugging.md) puts what doctor finds, as the
bundle is made, in `doctor/report.json`, and its checks of the state
store in `doctor/store.json`. `--bundle` prints both wherever the
archive is read:

```console
$ muructl doctor --bundle muru-bundle-20261102-061500.tar.gz
bundle   muru-bundle-20261102-061500.tar.gz, made 2026-11-02T06:15:00Z on web1 by Muru 1.0.0

SOCKETS
/run/muru/admin.sock               admin API                                  daemon pid 4120   answering
...
```

The sockets, files and checks are as they were on the machine the
bundle was made on, when it was made, and the exit code is 1 if a check
failed there. Nothing is checked again: whether a socket answers can
only be asked where the socket is.

A bundle does not hold the state store itself, since a release's record
has its environment values, which a bundle never holds. To send the
store to someone who may have it, take a copy with `muructl doctor store
--copy FILE`.

## From a coding agent

The MCP server's `doctor` tool runs the same three commands on the
daemon the server reaches, with `part` set to `installation`, `store`
or `timeline`, and the [agent skill](coding-agents.md) tells an agent to
run it before reporting a problem with Muru.
