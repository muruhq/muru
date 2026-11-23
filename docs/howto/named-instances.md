# Run more than one daemon on one machine

Each daemon on a machine can be a named instance, whose configuration
file, state, run directory and logs all follow from its name. One name is
then all the daemon's commands need.

## Defining an instance

`muructl instance create` defines an instance with the addresses it
serves:

```console
$ muructl instance create dev --listen 127.0.0.1:9000 --dashboard 127.0.0.1:9001
muru: instance dev is defined in /home/me/.config/muru/instances/dev; start it with muructl daemon start --instance dev
$ muructl instance create staging --listen 127.0.0.1:9100 --description "the build before it ships"
```

The definition is a directory in your configuration area holding two
files: `instance.toml`, which says what the definition is,

```toml
kind = "daemon"
description = "the build before it ships"
```

and `muru.toml`, the daemon's configuration file, which you edit as you
would any daemon's, with its `[[listener]]` entries, its `[dashboard]`
table and the rest. A definition holds no secrets.

A name is 1 to 24 lowercase letters, digits and hyphens, starting with
a letter. `user` and `system` name the default daemons, which need no
definition. `instance create` refuses a name already defined, and an
address another defined instance is configured with or a running
instance holds, so two instances never clash once both run.

## Where an instance keeps things

| Location | Linux | macOS | System, with `--system` |
| --- | --- | --- | --- |
| Configuration | `~/.config/muru/instances/NAME/muru.toml` | `~/Library/Application Support/muru/instances/NAME/muru.toml` | `/etc/muru/instances/NAME/muru.toml` |
| State | `~/.local/state/muru/instances/NAME` | `~/Library/Application Support/muru/instances/NAME/state` | `/var/lib/muru/instances/NAME` |
| Admin socket and run directories | `$XDG_RUNTIME_DIR/muru/instances/NAME` | `/tmp/muru-UID/instances/NAME` | `/run/muru/instances/NAME` |
| Logs | `~/.local/state/muru/logs/instances/NAME` | `~/Library/Logs/muru/instances/NAME` | `/var/log/muru/instances/NAME` |
| Cache | `~/.cache/muru` | `~/Library/Caches/muru` | `/var/cache/muru` |

The cache is shared by every instance, since everything in it can be
made again. A location flag, such as `--state-dir`, or its variable,
such as `MURU_STATE_DIR`, still replaces one location; giving every one
of them together with `--instance` is refused, since the name would
decide nothing. A `MURU_RUN_DIR` set in your shell applies to every
instance started from it, so unset it before running two instances.

## Starting, stopping and the rest

The daemon's commands take `--instance NAME`, or the `MURU_INSTANCE`
variable:

```console
$ muructl daemon start --instance dev
muru: daemon started (pid 5120), serving on http://127.0.0.1:9000, log /home/me/.local/state/muru/logs/instances/dev/daemon.log
$ muructl daemon start --instance staging
$ muructl daemon status --instance dev
daemon   pid 5120 version 1.0.0, instance dev
edge     pid 5131
serving  http://127.0.0.1:9000
...
$ muructl daemon stop --instance dev
```

So do `muructl up`, `muructl mcp serve --up`, `muructl secret` and the
`muructl logs` commands that work on the log files. A daemon started for
a name with no definition is refused:

```console
$ muructl daemon start --instance qa
muru: error: instance qa has no configuration file at /home/me/.config/muru/instances/qa/muru.toml; define it with muructl instance create qa
```

The instance's processes carry its name: its title is `muru daemon dev`,
its edge's `muru edge dev`, and its hosts' and workers' end with
`(dev)`.

## Deploying to each

Every command that talks to a daemon reaches the instance `--instance`
names:

```console
$ muructl deploy shop ./shop --instance dev
$ muructl deploy shop ./shop --instance staging
$ muructl release list --instance staging
```

## Reaching an instance from a checkout

A checkout can say which instance its commands reach, in its
`muru.local.toml`, the file of settings for this machine alone that is
never committed:

```toml
instance = "dev"
```

`muructl deploy`, `muructl status` and every other command run in that
directory, or `muructl deploy APP PATH` given that directory, then reach
`dev` without a flag. A command that changes something says where it
went:

```console
$ muructl deploy shop .
muru: deploying shop to instance dev (from muru.local.toml)
muru: release shop@r2 is ready and serving
```

With `--json`, the answer has an `instance` field. Nothing machine-wide
records a current instance, so two terminals in two checkouts reach two
daemons at once.

The instance a command reaches is the first of these that applies:

1. `--socket` or `--instance` on the command line;
2. `MURU_SOCKET` or `MURU_INSTANCE` in the environment;
3. for a command run inside an instance, such as by its web application,
   that instance, as [the machine overview](instances.md#reaching-an-instance)
   describes;
4. `instance` in the current directory's `muru.local.toml`;
5. a development run of the current directory, then the default
   daemon, as without named instances.

Giving `--socket` and `--instance` together is refused, and so is
setting both variables. `--instance` and `MURU_INSTANCE` also reach a
running instance that has no name, by what `muructl instance list`
shows for it, its label or its application, as [the machine
overview](instances.md#reaching-an-instance) describes. A value that
reaches no instance is refused with the instances it could reach, and a
name that is defined and not running says how to start it:

```console
$ muructl status
muru: error: instance dev is not running; start it with muructl daemon start --instance dev
```

A development run of the checkout still answers the commands it can,
such as `status`, `logs`, `events` and `debug`, before the instance the
file names, and says so; `--instance dev` reaches the daemon:

```console
$ muructl status
muru: reaching the development run of shop on http://127.0.0.1:8000; muru.local.toml names instance dev, pass --instance dev to reach it
```

Commands only a daemon answers, such as `deploy`, `release`, `rollout`
and `overlay`, always go to the instance. `instance` belongs in
`muru.local.toml` alone: in the project's `muru.toml` or `[tool.muru]`
table it is refused.

`muructl mcp serve` settles its instance in the same way once, when it
starts, and every tool reaches it unless the call names another in its
`instance` argument.

## Listing them

`muructl instance list` shows every defined instance, running or not,
beside the development runs and other daemons on the machine:

```console
$ muructl instance list
NAME      KIND     STATE     PID    UPTIME  LISTEN                         DASHBOARD               WHERE
user      daemon   running   4123   3d 2h   127.0.0.1:8080                 http://127.0.0.1:8081/  ~/.local/state/muru
dev       daemon   running   5120   2h 4m   127.0.0.1:9000                 http://127.0.0.1:9001/  instances/dev
staging   daemon   stopped   -      -       127.0.0.1:9100 (configured)    -                       instances/staging
```

A defined instance with nothing running is `stopped`, with the addresses
its file configures. `--ports` lists the ports of them all, and
`muructl daemon start` warns when an address it is about to use is
configured for a stopped instance too. The [machine overview](instances.md)
describes the list.

## Removing an instance

```console
$ muructl instance remove staging
remove   /home/me/.config/muru/instances/staging/instance.toml
remove   /home/me/.config/muru/instances/staging/muru.toml
remove instance staging? [y/N] y
muru: instance staging is removed
```

`--state` removes its state, run and log directories too. Both ask
first; `--yes` goes ahead without asking. An instance that is running
is refused until it is stopped.

## Under systemd

The template unit in the [systemd guide](systemd.md#several-instances)
runs any named instance as `muru@NAME`.
