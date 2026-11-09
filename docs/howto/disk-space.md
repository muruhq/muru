# Disk space

Every `muructl deploy` adds a release, and a release keeps files: usually
an environment under the state directory, its bytecode, and a log. Muru
removes these only when asked.

## Seeing what releases take

```console
$ muructl release list shop
RELEASE                  STATE      SERVING  WORKERS               DISK SOURCE
shop@r5                  ready      yes      4 (4x8)            218.4MB /srv/shop
shop@r4                  retained            1 (4x8)            218.4MB /srv/shop
shop@r3                  stopped             - (4x8)            211.0MB /srv/shop
shop@r2                  failed              - (4x8)             40.1KB /srv/shop
shop@r1                  stopped             - (4x8)            196.2MB /srv/shop
```

`DISK` counts a release's environment, bytecode, log and log archives,
with each hard-linked file counted once. With `--json`, `disk_freed_bytes`
says what removing the release would give back, which is less when uv's
cache also holds its environment's files. A release's source directory is
not counted.

## Removing old releases

`muructl release prune` removes old releases of one application, or of
every application when none is named. `--keep` says how many to keep:

```console
$ muructl release prune shop --keep 1
  shop@r1 (stopped, 196.2MB)
Remove 1 release, freeing 196.2MB? [y/N]
```

It lists each release it would remove with its state and what it would
free, then asks. `--yes` answers without asking, and without a terminal the
command is refused unless given `--yes`. `--dry-run` lists what would go,
what stays and why, and changes nothing.

`--keep N` keeps the N newest releases that have not failed, in addition
to these, which prune never takes:

- the release that serves

- the release a rollback restores

- any release that is still running, a retained one among them, or on
  its way to running

- a release a route's traffic split names, or the candidate of a rollout
  under way

`--keep 0` therefore still leaves the serving and rollback releases.

A failed release may still be needed for its logs, so prune keeps it until
both the serving and the rollback release were created after it. An
application with only one of those, or neither, keeps its failed releases.
Past that point a failed release is removed whatever N is, so a run of
failed deploys does not push out good releases.

A release that applied files declare stays unless `--force` is given,
since the next `muructl apply` would create it again.

With the release go its environment, its bytecode and its log and log
archives. `--keep-logs` leaves the log and archives. A release's source
directory is never touched.

## Removing one release

`muructl release remove` removes the one release named:

```console
$ muructl release remove shop@r2
  shop@r2 (failed, 40.1KB)
Remove 1 release, freeing 40.1KB? [y/N]
```

It asks in the same way, with `--yes`, `--dry-run` and `--keep-logs`. A
failed release named this way goes at once, without waiting for the serving
and rollback releases to be newer. The releases prune never takes are
refused:

```console
$ muructl release remove shop@r5
Error: shop@r5 cannot be removed: it serves the application's traffic
```

## Removing an application

`muructl app remove` removes a whole application: it drains whatever is
running and removes every release, with its environment, bytecode and logs.

```console
$ muructl app remove shop
  shop@r5 (ready, 218.4MB)
  shop@r4 (stopped, 218.4MB)
Remove application shop and its 2 releases, freeing 436.8MB? [y/N]
```

It asks in the same way, with `--yes` and `--dry-run`. It is refused while
a route still sends traffic to the application, while a rollout of it is
under way and while a release of it is being built.

`muructl apply -f config/ --prune` removes the releases the files do not
declare in the same way, logs included.
