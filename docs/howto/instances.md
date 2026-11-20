# See what Muru is running on a machine

Every Muru instance on a machine, a daemon, a development run with
`muructl run` or a `muructl serve`, registers itself when it starts, in
a registry of your own in your runtime directory. `muructl instance list`
reads it, so you can see what runs, where each instance listens and where
it was started from, and what was left behind when one did not stop
cleanly.

## Listing the instances

```console
$ muructl instance list
NAME       KIND     STATE     PID    UPTIME  LISTEN                       DASHBOARD               WHERE
user       daemon   running   4123   3d 2h   127.0.0.1:8080               http://127.0.0.1:8081/  ~/Library/Application Support/muru
run:8000   run      running   6001   14m     127.0.0.1:8000               http://127.0.0.1:8001/  ~/src/shop (shop)
7f3a9c2e   daemon   detached  -      -       127.0.0.1:8200               -                       /tmp/scratch/state
run:8123   run      stale     -      -       127.0.0.1:8123               -                       ~/src/blog (blog)
system     daemon   running   812    9d      0.0.0.0:80, :443 (tls)       -                       /var/lib/muru
```

- **NAME** is the instance's name as its processes' titles carry it:
  `user` or `system` for the default daemons, `run:PORT` for a
  development run, `serve` for `muructl serve`, and the first 8 hex digits
  of its instance ID for a daemon started with its own `--state-dir`.

- **STATE** is `running` while the instance's process is alive,
  `detached` when it has gone but processes it started still run, as
  when a daemon detaches and leaves its edge and hosts serving for the
  next daemon to take over, and `stale` when nothing of it runs, as when
  a development run was killed outright.

- **LISTEN** is what it is bound to, a later address on the same host
  shortened to its port and marked `(tls)` when it carries TLS, or the
  configured addresses marked `(configured)` when nothing is bound.

- **WHERE** is a run's project directory and application, or a daemon's
  state directory, with your home directory shortened to `~`.

A LABEL column is added when any instance has a label, which an instance
takes from `MURU_INSTANCE_LABEL` in the environment it starts with, so a
script or a test session can mark what it starts:

```console
$ MURU_INSTANCE_LABEL=nightly muructl daemon start --state-dir /tmp/nightly/state --run-dir /tmp/nightly/run
$ muructl instance list --label nightly
```

`--kind daemon`, `run` or `serve` keeps one kind. The system daemon's
entry is in a registry every user can read, so it is listed for you too;
its admin socket still decides who may use it. As root, `--all-users`
also lists every user's instances.

`--json` prints one document with `instances`, each instance's record
with its `state` and its `location`, `local`.

The list is the same whatever run directory an instance was given, so
the development runs a test session starts under its own `MURU_RUN_DIR`
are listed, though commands run outside the session never reach them.
`muructl status` with nothing listening where it looked says how many
instances the list shows.

## Processes left behind

Every process of an instance names its instance, in its title and in
`MURU_INSTANCE_ID` and `MURU_INSTANCE` in its environment, and so do the
processes those start. `--orphans` adds the Muru processes that no
running or detached instance accounts for:

```console
$ muructl instance list --orphans
NAME       KIND     STATE     PID    UPTIME  LISTEN           DASHBOARD               WHERE
user       daemon   running   4123   3d 2h   127.0.0.1:8080   http://127.0.0.1:8081/  ~/Library/Application Support/muru

PID     ROLE     USER   UPTIME   PPID   INSTANCE   TITLE
61234   host     me     2h 4m    1      run:8123   muru host blog@r1 (run:8123)
61240   worker   me     2h 4m    61234  run:8123   muru worker blog@r1 default/0 (run:8123)
```

A process is an orphan when the instance it names is stale or gone,
when it has a Muru title that names no instance, as Muru's processes had
before they carried one, or when it belongs to a running instance that
does not supervise it, such as a host its daemon has lost track of. To
find the last kind, `--orphans` asks each running instance which
processes it supervises. With `--json` the orphans are under `orphans`,
each with why it is one.

With `--orphans`, the command exits 1 when there are orphans, so a
script can check that nothing was left running:

```console
$ muructl instance list --orphans --label nightly > /dev/null || echo "something was left running"
```

On macOS the environment of a subprocess that runs one of the system's
own programs, such as `/bin/sleep`, cannot be read, so such a process is
found only while its worker or host is alive.

## From a coding agent

The read-only `instances` tool of `muructl mcp serve` returns what
`muructl instance list --json` prints, with `orphans` and `label`
arguments, so an agent can see what it has left running.
