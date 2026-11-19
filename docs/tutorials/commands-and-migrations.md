# Commands and migrations

An application needs more run than its web requests: a database
migration before a release serves, a backup that takes an hour, a
cleanup every night. Muru runs each as a command with a release: its
interpreter, environment and source directory, so it sees exactly what
the application sees.

In about twenty minutes you will declare commands and run them, make a
migration a gate that a release must pass before it serves, run a long
backup detached from your terminal and from the daemon itself, and put a
cleanup on a schedule.

This tutorial assumes you have done [deploy with the daemon and roll
back](deploy-and-roll-back.md).

## A catalog with a database

```
mkdir catalog && cd catalog
uv init --bare
uv venv
```

The application lists the products in a sqlite database beside it. Save
this as `app.py`:

```{literalinclude} code/catalog/app.py
:language: python
```

The schema lives in numbered files under `migrations/`, and `migrate.py`
applies those not applied yet, each in a transaction of its own. Save it
as `migrate.py`:

```{literalinclude} code/catalog/migrate.py
:language: python
```

and the first migration as `migrations/0001_products.sql`:

```{literalinclude} code/catalog/migrations/0001_products.sql
:language: sql
```

Two scripts do the rest. Save this as `manage.py`, which adds a product:

```{literalinclude} code/catalog/manage.py
:language: python
```

and this as `ops.py`, with the backup and the cleanup:

```{literalinclude} code/catalog/ops.py
:language: python
```

## Declare a command and run it

Commands are declared in `pyproject.toml`, beside Muru's other settings,
and each release created from the project carries them. Add these
tables, without the line `before_fork = true`, which the next step adds:

```{literalinclude} code/catalog/pyproject.toml
:language: toml
:start-at: "[tool.muru]"
:end-before: "[tool.muru.commands.cleanup]"
```

`run` is a program and its arguments, run without a shell, and `python`
is the release's interpreter. `call` names a function instead. An input
is given with `--input NAME=VALUE`, can be required, have a default and
a pattern its value must match, and fills `{name}` in `run`.

```
muructl daemon start
muructl deploy catalog .
muructl command list catalog
```

```
catalog@r1

add-product: Add a product, or change its price
  run: python manage.py add {name} {price}
  input name (required): The product's name
  input price (default 1.00, matching [0-9]+[.][0-9][0-9])

backup: Copy the database to backups/
  call: ops:backup
  input seconds (default 30): How long the copy takes
  timeout 600s, one run at a time

migrate: Apply the database migrations
  run: python migrate.py
  timeout 60s
```

The database has no table yet. Run the migration, then add a product:

```
muructl command run catalog migrate
```

```
applied 0001_products.sql
the schema is up to date
```

```
muructl command run catalog add-product --input name=teapot --input price=12.50
```

```
added teapot at 12.50
```

The output streams as the command writes it, and `muructl` exits with
the command's exit status. A value that does not match its input's
pattern is refused before anything runs:

```
muructl command run catalog add-product --input name=cup --input price=4
```

```
muru: error: add-product input price: "4" does not match [0-9]+[.][0-9][0-9]
```

```
curl http://127.0.0.1:8080/
```

```
teapot  12.50
```

## A migration as a gate

Running the migration by hand before each deploy is a step to forget.
Add `before_fork = true` to the `migrate` table, and a second migration,
`migrations/0002_stock.sql`:

```{literalinclude} code/catalog/migrations/0002_stock.sql
:language: sql
```

`before_fork` makes the command a gate: when a release starts, the
daemon runs it once the release's host has imported the application and
before any worker forks, and the release serves only if it passes.
Deploy:

```
muructl deploy catalog .
```

```
muru: release catalog@r2 is ready and serving
```

The deploy waited for the gate. Every run is recorded, with what started
it:

```
muructl command runs catalog
```

```
RUN    COMMAND     RELEASE  TRIGGER              STARTED              STATE
3      migrate     r2       gate:before_fork     2026-11-02T10:02:16Z exited with status 0 after 20ms
2      add-product r1       manual               2026-11-02T10:01:40Z exited with status 0 after 16ms
1      migrate     r1       manual               2026-11-02T10:01:31Z exited with status 0 after 19ms
```

and `muructl release show catalog@r2` lists the gate the release passed:

```
gate migrate before_fork passed 2026-11-02T10:02:16Z, run 3
```

A release's gate runs once: a host of the release started again, after a
crash or a daemon restart, skips the gates the release has passed.

Now a migration that fails. Save this as `migrations/0003_restock.sql`;
it adds a column the second migration already added:

```{literalinclude} code/catalog/broken/0003_restock.sql
:language: sql
```

```
muructl deploy catalog .
```

```
muru: error: the before_fork gate migrate exited with status 1 after 19ms
```

The new release failed before any of its workers forked, so nothing of
it ever served, and the release that served goes on serving:

```
muructl release list catalog
```

```
RELEASE                  STATE      SERVING  WORKERS               DISK SOURCE
catalog@r3               failed              - (1x4)              5.0MB /home/you/catalog
                         the before_fork gate migrate exited with status 1 after 19ms
catalog@r2               ready      yes      1 (1x4)              5.0MB /home/you/catalog
catalog@r1               retained            1 (1x4)              5.0MB /home/you/catalog
                         warm until 10:32 UTC; a rollback to it is instant
```

```
curl http://127.0.0.1:8080/
```

```
teapot  12.50
```

What the migration said is in its run's own log, run 4:

```
muructl command logs 4
```

```
2026-11-02T10:03:05.118Z [command migrate] 0003_restock.sql failed: duplicate column name: stock
2026-11-02T10:03:05.121Z [command migrate] exited with status 1 after 19ms
```

Remove the broken migration before going on:

```
rm migrations/0003_restock.sql
```

A migration as a gate suits a schema change that works with the code of
the release before it, since the two releases serve side by side while
the new one starts. [Database migrations](../howto/migrations.md) covers
changes that do not.

## One run at a time

`exclusive = true` on the backup lets only one run of it go at a time,
whoever asks for it. Start one, which takes thirty seconds:

```
muructl command run catalog backup
```

```
copied 1 of 30
copied 2 of 30
...
```

and while it runs, in another terminal, ask for another:

```
muructl command run catalog backup
```

```
muru: error: backup runs one at a time for catalog, and run 5 is under way
```

The first finishes:

```
copied 30 of 30
backed up to backups/catalog-20261102T100340.db
```

## A long run, detached

A command `muructl` waits for stops when you interrupt it, or when the
terminal it runs in closes. A long one runs detached instead:

```
muructl command run catalog backup --detach
```

```
muru: started run 6; muructl command logs 6 --follow follows it
```

The run is the daemon's now. Follow its output, and press Ctrl-C after a
few lines:

```
muructl command logs 6 --follow
```

```
2026-11-02T10:04:01.735Z [command backup] copied 1 of 30
2026-11-02T10:04:02.744Z [command backup] copied 2 of 30
^C
```

Ctrl-C stopped following, not the run:

```
muructl command status 6
```

```
run 6 of backup for catalog@r2
  started    2026-11-02T10:04:00Z by you, manual, detached
  inputs     seconds=30
  state      under way (detached, pid 19296)
  output     apps/catalog/commands/backup/6.log
```

Once it has finished, start a backup that would take ten minutes, and
stop it:

```
muructl command run catalog backup --detach --input seconds=600
muructl command stop 7
muructl command status 7
```

```
muru: started run 7; muructl command logs 7 --follow follows it
muru: stopping run 7 of backup
run 7 of backup for catalog@r2
  started    2026-11-02T10:05:10Z by you, manual, detached
  inputs     seconds=600
  ended      2026-11-02T10:05:11Z: it stopped because you stopped it; it was killed by signal 15 after 1s
  exit       143, failed
  output     apps/catalog/commands/backup/7.log
```

`command stop` sends the run SIGTERM, then SIGKILL ten seconds later if
it is still there.

## It outlives the daemon

A detached run does not depend on the daemon either. Start a backup,
stop the daemon without `--shutdown`, which leaves the releases and the
detached runs running, and start it again:

```
muructl command run catalog backup --detach
muructl daemon stop
muructl daemon start
muructl command status 8
```

```
run 8 of backup for catalog@r2
  started    2026-11-02T10:06:00Z by you, manual, detached
  inputs     seconds=30
  state      under way (detached, pid 19558)
  output     apps/catalog/commands/backup/8.log
```

The new daemon took the run over, and when it ends records how:

```
  ended      2026-11-02T10:06:30Z: it exited with status 0 after 30s
  exit       0
```

`muructl daemon stop --shutdown` would have stopped it, with everything
else.

## On a schedule

Add the cleanup command to `pyproject.toml`:

```{literalinclude} code/catalog/pyproject.toml
:language: toml
:start-at: "[tool.muru.commands.cleanup]"
```

`cron = "* * * * *"` runs it at the start of every minute, so you need
not wait long; a real cleanup would run nightly, as `"30 3 * * *"`. A
command with a schedule must have a `timeout`, so each scheduled run
ends. Schedules come with the release, so deploy:

```
muructl deploy catalog .
muructl command list catalog
```

```
cleanup: Remove old backups
  call: ops:cleanup
  input keep (default 1d)
  input pause (default 0): Seconds to wait before starting
  timeout 300s
  schedule: * * * * *; next 2026-11-02T10:08:00Z
```

After the next minute begins, the run is there, asked for by the
schedule:

```
muructl command runs catalog cleanup
```

```
RUN    COMMAND RELEASE  TRIGGER              STARTED              STATE
10     cleanup r4       schedule             2026-11-02T10:08:00Z exited with status 0 after 74ms
```

A scheduled run due while another run of the same command is under way
is skipped. Start a cleanup by hand that waits seventy seconds first:

```
muructl command run catalog cleanup --detach --input pause=70
```

When the next minute begins, the event says the scheduled run was
skipped:

```
muructl events
```

```
    46 10:09:00 info     command.skipped      catalog@r4               cleanup: the run due at 2026-11-02T10:09:00.000Z was skipped, since run 11 is under way
```

Schedules run under the daemon only. A run that falls due while no
daemon is running is not made up later.

## Clean up

```
muructl daemon stop --shutdown
```

## From here

- [Running commands](../howto/commands.md) covers every key of a command,
  secret inputs, `muructl exec` for a command that is not declared, and
  what is recorded of each run.

- [Database migrations](../howto/migrations.md) covers changing a schema
  in steps that keep both releases working, and `before_promote` gates,
  which run before a [rollout](first-canary.md) moves all traffic to a
  release.

- [Kubernetes](../howto/kubernetes.md#migrations) and
  [containers](../howto/containers.md) cover running the same commands
  under `muructl serve`, where a Job runs the migration.
