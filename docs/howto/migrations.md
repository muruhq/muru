# Database migrations

A deploy runs the new release beside the old one until the new one is
ready, and a canary, a split or a sticky grace keeps both serving for
longer. All that time both releases use the same database, so a schema
change has to work with the code of both. This guide covers changing a
schema in steps that keep it so, and running each step as a
[command](commands.md) with the release it belongs to.

## Expand, deploy, contract

Split a change that would break the old release into three steps:

1. **Expand.** Add what the new code needs without removing anything the
   old code uses: a new table, a new nullable column, a new index. Run the
   migration while the old release serves; it does not notice.

2. **Deploy.** Deploy the release that uses the new schema. While both
   releases serve, the new one must cope with rows the old one writes,
   and the old one with rows the new one writes.

3. **Contract.** Once no release that uses the old schema serves or is
   retained for a rollback, remove what only it used: drop the old column,
   make the new one required. This is a migration of its own, shipped in
   a later release.

Renaming a column is the usual example. Expand adds the new column and
has the new code write both and read the new one, falling back to the
old; a backfill copies the old values across; contract drops the old
column a release or two later. Changes that already work with both
releases, such as adding a table or an index, need only the first step.

## Running the migration

Declare the migration as a command, so it is run the same way every time,
one at a time:

```toml
[tool.muru.commands.migrate]
description = "Apply database migrations"
run = ["python", "manage.py", "migrate", "--noinput"]
timeout = "30m"
exclusive = true
```

Run a migration with the release whose code it belongs to, so it uses
that release's migration files and settings. With the expand step in the
new release, hold the release, migrate with it, then move traffic:

```
muructl deploy shop . --hold
muructl command run shop@r8 migrate
muructl rollout bluegreen shop@r8
```

The held release starts with no traffic, so its workers are up before
the schema changes and nothing reaches them until it is promoted. For a
[canary](rollouts.md), start the rollout after the migration in the same
way.

With Alembic, or any tool the environment installs, the same applies,
with `run = ["alembic", "upgrade", "head"]`. `muructl exec` runs a
migration command that is not declared, once:

```
muructl exec shop@r8 -- alembic upgrade head
```

Run the contract step with the release that removes the old code's last
use, once it serves and the release before it is gone, its retention
window over or drained:

```
muructl command run shop migrate
```

`muructl command run` exits with the migration's status, so a script or
CI job that deploys can stop at a failed migration before it moves any
traffic.

## Gates

A command can be a gate in a release's life, run by the daemon at that
point, so a deploy needs no separate step:

```toml
[tool.muru.commands.migrate]
description = "Apply database migrations"
run = ["python", "manage.py", "migrate", "--noinput"]
timeout = "30m"
before_fork = true

[tool.muru.commands.contract]
run = ["python", "manage.py", "migrate", "--noinput", "--database", "contract"]
timeout = "30m"
before_promote = true
```

- `before_fork = true` runs the command when the release starts, once
  its host has imported the application and before it forks any worker.
  A gate that fails fails the start, as a failed import does: the
  release is `failed` with the gate and how it ended in its message, its
  host stops, and nothing of it ever serves. A canary of the release
  rolls back on the same path as any start that fails.

- `before_promote = true` runs the command when a rollout's last step or
  `muructl rollout bluegreen` is about to move all of the application's
  traffic to the release, with the release already running. A gate that
  fails stops the promotion: the release that served goes on serving,
  and a rollout is paused where it was, saying why, to be resumed,
  promoted or aborted. A deploy that serves at once, with no rollout, is
  no promotion and runs only the `before_fork` gates.

A gate runs once for each release. The daemon records each gate a
release passes, `muructl release show` lists them, and a host of the
release started again, after a crash or a daemon restart, skips a gate
the release has passed. `every_start = true` runs a `before_fork` gate
at every start of the host instead.

A gate needs a `timeout`, since the release waits for it, and is given
no inputs, so a required input needs a default. It is always exclusive.
Each gate's run is recorded like any other, with the trigger
`gate:before_fork` or `gate:before_promote`, its output in the release's
log and its own file, and `command.started` and `command.finished` or
`command.failed` events, with a `release.gate_passed` event when it
passes. The gates of a stage run one after another, in the order of
their names. While a `before_fork` gate runs, the release is `warming`,
and the deploy waits for it. The daemon goes on with everything else
meanwhile: `muructl status`, deploys of other applications and the
daemon's own checks answer as usual. A drain of the release cancels its
start, stopping the gate's run and the host; a scale of it is refused
until it is ready.

Gates run under the daemon only: `muructl run` and `muructl serve` run
none. `muructl serve` cannot know how many replicas of its pod there
are, so a gate run by each pod would run the migration once a replica.
Run the migration by name before the new image serves instead: a
Kubernetes Job or a Helm pre-upgrade hook running `muructl command run
migrate`, as [the Kubernetes guide](kubernetes.md#migrations) shows, or
under Docker Compose a one-off service that the application's service
depends on with `condition: service_completed_successfully`:

```yaml
services:
  migrate:
    image: registry.example.com/shop:1.4.2
    command: ["muructl", "command", "run", "migrate"]
  shop:
    image: registry.example.com/shop:1.4.2
    depends_on:
      migrate:
        condition: service_completed_successfully
```

## Without the daemon

The same declared command runs for [the project in the current
directory](commands.md#the-projects-own-commands) when no application is
named:

```
muructl command run migrate
```

- **In development**, it runs with the project's own interpreter and
  environment, through `muructl run` when that is up and on its own
  otherwise, so the migration is run the way it will be in production.
- **In a container platform**, run it from the new image before the
  image serves: a Kubernetes Job as the [Kubernetes
  guide](kubernetes.md#migrations) shows, an init container, a Helm
  pre-upgrade hook, or `docker run --rm IMAGE muructl command run
  migrate` before `docker compose up`. It needs no running container of
  the application.
- **In a serving container**, `docker exec` or `kubectl exec` runs it
  through the `muructl serve` there, which suits a command that goes
  with the code already serving, such as the contract step.

`exclusive = true` holds for one daemon or one serving process. A
migration started by several Jobs at once is kept to one run by the
platform, with a Job of `parallelism: 1`, or by the migration tool's own
lock.

## Rolling back

A rollback moves traffic to the previous release but leaves the schema
as it is. After an expand step that is safe by design, since the old
release works with the expanded schema. Undo a migration only after
rolling back, and only when the release that serves no longer needs it,
with that release's code:

```
muructl rollback shop
muructl exec shop -- python manage.py migrate orders 0006
```

## Caches, queues and files

The same holds for anything else both releases share. Cached values and
queued messages written by one release are read by the other, so add
fields rather than change their meaning, and give a cache entry whose
shape changes a new key, with a version in the key say. Files written to
shared storage follow the same rule.
