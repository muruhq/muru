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
```

It checks one installation: the daemon whose locations the command's
flags and the environment give, as `muructl daemon start` takes them
(`--system`, `--instance`, `--state-dir`, `--run-dir`, `--log-dir`,
`--cache-dir` and `--config`), and the development runs under its run
directory. `--json` prints one document with `ok`, `sockets`, `files`
and `checks`.

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

Doctor repairs nothing itself.

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
over a file that exists.
