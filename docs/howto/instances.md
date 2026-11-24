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

- **NAME** is what `--instance` takes to reach the instance (see
  [Reaching an instance](#reaching-an-instance)): `user` or `system` for
  the default daemons, a named instance's name, `run:PORT` for a
  development run, `serve` for `muructl serve`, `detour:` and the first 8
  hex digits of its ID for a [detour](detours.md), and the first 8 hex
  digits of its instance ID for a daemon started with its own
  `--state-dir`. When two running instances would show the same value,
  such as two serves, each shows the first 8 hex digits of its ID
  instead. A `detached` or `stale` instance cannot be reached, and shows
  the name its processes' titles carry.

- **STATE** is `running` while the instance's process is alive,
  `detached` when it has gone but processes it started still run, as
  when a daemon detaches and leaves its edge and hosts serving for the
  next daemon to take over, and `stale` when nothing of it runs, as when
  a development run was killed outright.

- **LISTEN** is what it is bound to, a later address on the same host
  shortened to its port and marked `(tls)` when it carries TLS, or the
  configured addresses marked `(configured)` when nothing is bound.

- **WHERE** is a run's or a detour's project directory and application, or a daemon's
  state directory, with your home directory shortened to `~`.

A LABEL column is added when any instance has a label. An instance
takes its label from `--label` on `muructl run`, `muructl detour run`, `muructl serve`,
`muructl daemon start`, `muructl daemon run` and `muructl up`, or else
from `MURU_INSTANCE_LABEL` in the environment it starts with, so a
script or a test session can mark what it starts:

```console
$ muructl daemon start --label nightly --state-dir /tmp/nightly/state --run-dir /tmp/nightly/run
$ muructl instance list --label nightly
```

A label is 1 to 64 letters, digits, hyphens, underscores and dots.

`--kind daemon`, `run`, `serve` or `detour` keeps one kind. The system daemon's
entry is in a registry every user can read, so it is listed for you too;
its admin socket still decides who may use it. As root, `--all-users`
also lists every user's instances.

`--json` prints one document with `instances`, each instance's record
with its `state`, its `location`, `local`, and for an instance that can
be reached its `selector`, the value the NAME column shows.

The list is the same whatever run directory an instance was given, so
the development runs a test session starts under its own `MURU_RUN_DIR`
are listed, though commands run outside the session never reach them.
`muructl status` with nothing listening where it looked says how many
instances the list shows.

## Reaching an instance

Every command that talks to a running instance, such as `status`,
`logs`, `events`, `debug` and `deploy`, takes `--instance` with what the
list shows. The value is matched against the instances in this order,
and the first that matches decides:

1. the name of a named instance, or `user` or `system` for the default
   daemons, whether or not it is running;

2. the NAME the list shows for a running instance, such as `run:50907`
   or `91720d04`;

3. the start of an instance's ID, at least 4 hex digits, as `muructl
   instance list --json` gives the ID in full;

4. a label, when one running instance has it;

5. the application of a development run or a serve, when one running
   instance has it.

```console
$ muructl instance list
NAME        KIND     STATE     PID     UPTIME  LISTEN            DASHBOARD   WHERE                    LABEL
91720d04    daemon   running   97379   1h 9m   127.0.0.1:50906   -           /srv/site/state          admin-demo
run:50907   run      running   97383   1h 9m   127.0.0.1:50907   -           ~/work/helper (helper)   -
$ muructl status --instance run:50907
$ muructl logs --instance helper
$ muructl events --instance admin-demo
```

A label is a handle you choose for an instance that has no name, without
the directories a named instance brings with it. Starting each instance
with its own label lets a script reach it by that label every time.

A value that matches more than one instance by the same rule is refused,
with the instances it matched, one to a line, each with the value that
reaches it alone. Up to five are listed, and the rest counted:

```console
$ muructl status --instance site
muru: error: site is the application of 2 running instances; pass one of these to --instance:
  run:9080  the development run of site in ~/work/hub (pid 4120)
  run:9180  the development run of site in ~/work/blog (pid 4188)
```

A value that matches no running instance is refused and points at
`muructl instance list`. When it matches one that is listed as `stale`
or `detached`, the error names that instance and why it cannot be
reached:

```console
$ muructl status --instance run:9080
muru: error: run:9080 is the development run of hub in ~/work/hub, which is not running: its entry was left behind when it stopped, and muructl instance prune removes it; muructl instance list shows the running instances
```

`MURU_INSTANCE` takes the same values.

A `muructl` command run inside an instance, by its web application, by
one of its project's commands or from a shell `muructl exec` started,
reaches the instance it runs under, which it knows from the
`MURU_INSTANCE_ID` every process of the instance has. `--instance`,
`--socket`, `MURU_INSTANCE` or `MURU_SOCKET` chooses another. A command
only a daemon answers, such as `muructl deploy`, run inside a
development run, goes to the daemon it would reach from anywhere else.

The commands that start a daemon, `muructl daemon start`, `muructl
daemon run` and `muructl up`, and the commands that define or remove a
named instance take a name and nothing else, since the daemon's
directories follow from it.

When a command chose its instance without `--instance`, `--socket` or a
variable, and more than one running instance could have answered it, it
says on standard error which one it reached:

```console
$ muructl status
muru: reaching the development run of shop in ~/src/shop (pid 6001, dashboard http://127.0.0.1:8001/), instance run:8000, one of 2 running; pass --instance to choose another, as muructl instance list names them
```

## Ports

When Muru cannot bind a port because another instance of Muru's holds
it, in `muructl run`, `muructl serve`, the daemon's listeners or a
dashboard's address, it names the instance, and names another program
only when no instance holds the port:

```console
$ muructl run --port 8000
muru: error: port 8000 on 127.0.0.1 is in use by the development run of shop in ~/src/shop (pid 4123, dashboard http://127.0.0.1:8001/); choose another with --port, or stop it
```

`muructl daemon start` checks the addresses the daemon would bind before
it starts anything, and refuses one a running instance holds.

`--ports` lists one row for each port an instance uses, its listeners
and its dashboard, and for each one it is configured with but has not
bound. A configured address that would clash with another instance's,
the same port on an overlapping address, a wildcard address overlapping
every other, is marked:

```console
$ muructl instance list --ports
PORT   ADDRESS     INSTANCE   KIND     USE         STATE
8000   127.0.0.1   run:8000   run      http        bound
8001   127.0.0.1   run:8000   run      dashboard   bound
9000   127.0.0.1   dev        daemon   http        bound
9000   127.0.0.1   staging    daemon   http        configured   clashes with dev
```

`--json` gives the same rows under `ports`.

## Processes left behind

Every process of an instance names its instance, in its title and in
`MURU_INSTANCE_ID` and `MURU_INSTANCE_TAG` in its environment, and so do the
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

## Cleaning up

`muructl instance prune` removes stale entries and stops orphaned
processes. It lists what it will do and asks first:

```console
$ muructl instance prune
remove   run run:8123 (stale, 9b21c4e0)
stop     host 61234: muru host blog@r1 (run:8123) (its instance is stale)
keep     daemon 7f3a9c2e: it is detached and its processes keep serving; --detached stops them
remove 1 entry and stop 1 process? [y/N] y
muru: removed 1 entry and stopped 1 process
```

- **Stale entries** are removed only while their lock is free, so an
  instance that is alive is never touched. A development run's own
  directory goes with its entry.

- **Orphans**, as `--orphans` lists them, are stopped: SIGTERM, then
  after a grace period of 5 seconds, or `--grace DURATION`, SIGKILL for
  what remains. Hosts are stopped before their log relays, and workers end
  with their host. Before each signal the process's start time is checked
  against the one it was found with, so a pid the system has since given
  to another process is never signalled.

- **A detached instance's processes**, such as the edge and hosts a
  daemon leaves serving when it detaches for the next daemon to take
  over, are kept unless `--detached` is given, which stops them and
  removes the instance's entry.

`--yes` goes ahead without asking, and without a terminal the command
refuses unless it is given. `--dry-run` lists and changes nothing.
`--json` reports what was `removed`, `stopped`, `kept` and `failed`, each
with why.

`--label LABEL` prunes only what carries the label, which is how a script
or a test session cleans up after itself; `--instance NAME|ID` only one
instance's leftovers, by its name or its instance ID (or the ID's first 8
hex digits); `--entries-only` removes stale entries and stops nothing.

`muructl instance prune` works on your own registry and processes. As
root it also works on the system daemon's registry, and with
`--all-users` on every user's. A process you may not signal is reported
and left.

On macOS the environment of a subprocess that runs one of the system's
own programs, such as `/bin/sleep`, cannot be read, so such a process is
found only while its worker or host is alive.

## From a coding agent

The read-only `instances` tool of `muructl mcp serve` returns what
`muructl instance list --json` prints, with `orphans` and `label`
arguments, so an agent can see what it has left running. Every other
tool takes an `instance` argument with any value `--instance` takes, so
an agent that sees several instances can ask about each.
