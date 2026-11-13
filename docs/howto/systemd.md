# Running under systemd

The repository ships a unit at `packaging/systemd/muru.service`:

```
[Unit]
Description=Muru
After=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/muructl daemon run --system
ExecReload=/usr/bin/muructl daemon reload
ExecStop=/usr/bin/muructl daemon stop --shutdown --timeout 60s
TimeoutStopSec=90
ConfigurationDirectory=muru
StateDirectory=muru
CacheDirectory=muru
LogsDirectory=muru
RuntimeDirectory=muru
RuntimeDirectoryPreserve=yes
KillMode=process
# A worker the kernel kills for memory is replaced by its host, so the
# service carries on rather than stopping over one worker.
OOMPolicy=continue
Restart=on-failure
LimitNOFILE=1048576
# To bound what Muru may use on this machine, give the whole service a
# limit; the node budget and the memory guard then work from it. See
# the limits section of the systemd guide, howto/systemd.
#MemoryMax=8G
#CPUQuota=400%

[Install]
WantedBy=multi-user.target
```

The unit runs the daemon as root, which keeps root for itself alone:
the edge and every application that names no user run as the service
user, `muru` by default, and the daemon does not start without that
account. The system packages create it with the sysusers.d entry at
`packaging/sysusers.d/muru.conf`; an installation by hand creates it
first, or names another account with `--user` on the `ExecStart` line
or the `[daemon]` table of the configuration file. The [per-application
users guide](users.md) says what the account needs to read.

Create the account, install the unit, put the configuration file at
`/etc/muru/muru.toml`, and start it:

```
$ sudo useradd --system --no-create-home --shell /usr/sbin/nologin muru
$ sudo cp packaging/systemd/muru.service /etc/systemd/system/
$ sudo systemctl daemon-reload
$ sudo systemctl enable --now muru
$ muructl --system status
```

## What the unit does

The directory settings make systemd create `/etc/muru`, `/var/lib/muru`,
`/var/cache/muru`, `/var/log/muru` and `/run/muru` and pass them to the
daemon in the environment, so the daemon finds its locations without any
path being written twice; `muructl daemon run` prints each one and the
rule that chose it. `--system` makes commands run as other users look in
the same places.

`Type=notify` means systemd waits for the daemon to say it is ready,
which it does once the admin socket answers and the releases that were
serving have been taken over, and `systemctl start` returns only then.
The status line systemd shows names the listeners.

`systemctl stop` runs the `ExecStop` line: `muructl daemon stop
--shutdown` drains every release, stops the hosts and the edge, then the
daemon exits, which is what a node going out of service wants. The
shutdown fits the `budget` of the daemon file's `[shutdown]` table,
eight seconds unless set, past which what is left is killed; a release
with a long `shutdown_timeout` wants a budget to match. `--timeout`
bounds how long muructl waits for the daemon, and `TimeoutStopSec` is
longer, so a shutdown that overruns is reported before systemd kills
what is left.

`systemctl restart`, or a daemon that is killed or crashes, is different:
`KillMode=process` means systemd stops only the daemon, and the daemon
treats SIGTERM as `muructl daemon stop` without `--shutdown`, that is it
detaches, leaving the hosts and the edge serving (the `[shutdown]`
table's `on_sigterm` can make it shut down instead, which a unit that
stops with SIGTERM alone would want). `RuntimeDirectoryPreserve`
keeps their sockets in `/run/muru`, and the next daemon finds them there
and carries on, so a restart of the daemon costs no
request. The [daemon guide](daemon.md) says more about detaching and
re-adoption.

`systemctl reload` runs `muructl daemon reload`, which re-reads the
configuration file for what can change while the daemon runs; the
[daemon guide](daemon.md#reloading-the-configuration) lists what that
is and what is read only at start, and the reload says so too. SIGHUP
does the same as the command. A certificate renewal needs the same
reload, so a certbot deploy hook of `systemctl reload muru` makes the
renewed certificate live, as the [virtual hosts
guide](virtual-hosts.md#tls) describes.

## Limits

The unit sets no memory or CPU limit, since only the operator can say
what Muru may use of the machine, and Muru sizes nothing from the
machine's own memory. To bound it, give the whole service a limit with a
drop-in:

```
$ sudo systemctl edit muru
```

```
[Service]
MemoryMax=8G
CPUQuota=400%
```

With a limit on the service's cgroup:

- **The node budget** takes its `memory` and `cpu` from it when the
  daemon file's `[daemon.budget]` leaves them out: `memory.max` less a
  headroom of 10% (at least 128MB), and the quota, here 4 CPUs. Deploys,
  canaries beside the serving release and scale-outs are admitted
  against it before anything starts, as the [scaling
  guide](scaling.md#the-nodes-budget) describes.
- **The memory guard** watches what is left under the limit at every
  memory check, and with less than the headroom left replaces the
  largest worker of any release, one at a time, or scales its group in
  when its scaling policy allows, as [container
  limits](containers.md#the-memory-guard) describes for a container.
- **The limits are read when they are used**, so a change takes effect
  without a restart:

  ```
  $ sudo systemctl set-property muru MemoryMax=12G
  ```

`muructl daemon status` shows the limits read, what is used and left,
what the guard is doing, and which budget limits came from the cgroup.

**When the kernel kills a worker.** Each worker raises its own
`oom_score_adj` to 500 as it starts, so when memory runs out, under the
service's limit or the machine's, the kernel's out-of-memory killer
picks a worker first, which its host replaces, rather than a host, the
edge, the daemon or another service on the machine. Hosts, log relays,
the edge and the daemon keep the score they started with.
`muructl status --processes --json` gives each process's
`oom_score_adj`. A worker the kernel kills is a `worker.oom_killed` event.

`OOMPolicy=continue` keeps the service running when that happens. With
systemd's default, `stop`, a single worker killed for memory stops the
whole unit: `ExecStop` drains and stops every release, and
`Restart=on-failure` then starts the daemon again.

**systemd-oomd**, enabled by default on Ubuntu and Fedora, acts on a
unit only when the unit or a slice above it sets
`ManagedOOMMemoryPressure=kill` or `ManagedOOMSwap=kill`, and then kills
a whole cgroup at once, which for this unit is the daemon, the edge and
every release together. Leave both unset for `muru.service`, and bound
it with `MemoryMax=` instead, which the memory guard works within.

## Logs

The daemon's own output goes to the journal, where `journalctl -u muru`
shows it. Each release's host writes to `/var/log/muru/apps/APP/RELEASE.log`
and the edge to `/var/log/muru/access.log` and `edge.log`, which `muructl
logs` shows or follows and `packaging/logrotate/muru` rotates; the
[logging guide](logging.md) has the details.
