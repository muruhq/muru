---
status: accepted
date: 2026-11-11
---

# ADR 0059: A release has a log relay process beside its host

## Context and problem statement

The Python host relays its workers' output itself, from the one `select`
loop that also reads the workers' reports, enforces the hard and GIL
stall timeouts and serves the supervisor (sections 11.2 and 11.3). Every
stage is a blocking write, so a destination that stalls (a slow disk, a
paused terminal, a container runtime that stops reading) stalls the
loop, then fills the workers' pipes, then blocks every thread that
prints or logs, and the requests behind them. Section 11.3 says that
backpressure never reaches requests, and that application output uses a
large drop-oldest queue by default, or blocks if configured; neither is
built.

Issue 167 settled, with the maintainer, that the queue lives in a small
Rust process per release rather than in a thread of the host (a thread
running at fork can leave a lock held in the child, and CPython 3.12 and
later warn on every such fork), or in the daemon or any one process
shared by every application (a restart or a crash would cut off every
application's output at once, and one application's flood would delay
everyone else's lines). It left five things to decide: whether the
relay is the host's parent or a sibling, how the relay and the host
meet, how the relay reports to the controller, which binary it is, and
how it is adopted again and restarted. Issue 492 extends the same relay
to managed servers and services, so this record covers them as well.

## Decision drivers

- Robustness in production. A relay that fails must cost no more than a
  pause in output: the host keeps serving, nothing is restarted but the
  relay, and nothing has to be reconfigured to recover.

- Isolation. Each application's output path is its own, so a flood, a
  stall or a crash in one never touches another, and no single process
  slows every application.

- The host stays single-threaded and nothing in its loop waits on
  output, including its own lines.

- Hosts keep serving while the daemon restarts and are adopted again;
  the relay must survive the same restart and be adopted the same way.

- A host's last lines, a failed start's traceback above all, must reach
  the release log: the daemon reads that log to say why a start failed.

- Nothing new to install or keep in step: one binary is shipped, and a
  relay must speak the same protocol as the daemon that starts it.

## Considered options

The relay's place:

- The relay is the host's parent, as containerd's shim and `conmon` are
  for containers: the controller starts the relay, the relay starts the
  host and holds its pipes, and can record the host's exit status after
  a daemon restart.

- The relay is the host's sibling: the controller starts both, and the
  host holds the read ends of every pipe so that any relay can be given
  them.

How they meet:

- A socket pair inherited by both at start.

- A Unix socket path in the release's run directory, on which the host
  listens and a relay connects.

- A socket path on which the relay listens and the host connects,
  reconnecting when the relay changes.

How the relay reports:

- Through the host, which forwards counts and events over its existing
  control connection to the controller.

- A status file the relay rewrites in the run directory, read when the
  controller wants it.

- A control socket of the relay's own, which the controller connects to
  as it does to a host's.

The binary:

- `muructl` in an internal mode, as the edge is (`muructl edge run`).

- A binary of its own.

## Decision outcome

**The relay is a sibling of the host, never its parent.** The controller
starts the relay and the host side by side, under the daemon each in a
session of its own so both outlive it. The host's life never depends on
the relay: a relay that crashes is replaced, and in the meantime only a
worker whose own pipe fills waits, for the time the replacement takes.
As the parent, a relay's crash would orphan its host, and the relay
would sit on the host's start, stop, signal and adoption paths, where
today the controller deals with the host directly. The exit status a
parent can record is not needed for a host, which reports its workers'
exits itself and whose own exit after a daemon restart is seen as it is
today.

**The host listens on a socket in its run directory, `output.sock`, and
a relay connects.** The host makes each worker's stdout and stderr pipes
at fork as before, keeps their read ends and never reads them. When a
relay connects, the host sends it every read end it holds, one per
message with `SCM_RIGHTS`, each message a JSON line naming the pipe's
number, its worker and its stream; each worker forked after that is sent
as it forks. The host's own standard output and error are a pipe the
controller makes before either process starts: the first relay inherits
its read end, so even an interpreter that fails before the host's code
runs is heard, and the host inherits a copy that it holds and sends to
any later relay with the rest. A relay that has a pipe already closes
the copy. The host holds an exited worker's read ends until the relay
says, on the same socket, that it has read that pipe to its end, so a
crash's traceback survives a relay that is being replaced; it holds at
most 64 exited workers' pipes this way, closing the oldest beyond that.
A connection from a new relay replaces the old one. The host's sends
never block: a send the socket cannot take now waits in the host's
loop for the socket to have room. An inherited socket pair would join
only the processes started together, so a relay started after a crash,
or by a new daemon, could not reach a host already running; a socket
the relay listens on would need the host to notice a new relay and
reconnect, on timers of its own.

**The relay serves a control socket of its own, `relay.sock`, beside
`output.sock`.** It writes JSON lines to whoever connects: a status line
on connecting and again whenever its counts change, at most once a
second, with the lines dropped since it started, what its queues hold,
and whether it is dropping or holding a worker back; and an event line
when a stall begins and when it ends. The controller connects after
starting it and again after adopting it, and adds what a relay that
exited had counted to the release's total, so
`muru_app_output_dropped_total{app,release}` does not go back when a
relay is replaced. Forwarding through the host would make the relay's
reports depend on the host, which a managed server does not have; a
status file cannot carry an event when it happens.

**The relay is `muructl relay run`, an internal command like `muructl
edge run`.** The daemon starts it from its own executable, so the relay
always speaks its starter's protocol, and nothing else is packaged or
versioned. A relay left running by a daemon that is then upgraded keeps
running, as hosts do, and protocol changes keep to the same
compatibility window as the host's.

**Adoption and restarts follow the host's.** The controller writes the
relay's pid to `relay.toml` in the release's run directory. A daemon
that starts again adopts a relay whose pid is alive and whose command
line names that run directory, and connects to its control socket; it
starts a new one when the relay is gone. While the host runs, the
controller checks on its relay with the host, and starts another when
it has exited, waiting a second after the first exit and doubling to 30
seconds while relays keep failing within a minute of starting. The new
relay connects to `output.sock` and is given every pipe again.

**Stopping drains.** A relay exits once the host's connection and the
host's own stream have both ended and every pipe it holds has reached
its end, after writing what it holds. A stop that the controller waits
for (stopping a release, or a host that failed to start) waits for the
relay to exit, bounded, so a host's last lines are in the release log
before the daemon reads it or says the release has stopped. SIGTERM
makes a relay write what it already holds and exit, within two seconds.

**Its destination.** Under the daemon the relay opens the release log
itself with `O_APPEND`, and SIGUSR1 opens it again by name, so rotation,
`muructl logs reopen` and `reopen_on_rename` signal the relay where they
signalled the host. Under `muructl run` and `muructl serve` it writes to
its standard error, a pipe to muructl, whose relay and writer thread
(issue 573) keep writing muructl's own streams. The relay puts the full
UTC time in front of every line, whatever the destination, and muructl
takes a line's time from that stamp, so a delay in either process does
not move it. Gathering a traceback into one record under `muructl serve
--log-format json` stays in muructl, which is the only output that
writes JSON.

**What it keeps.** One queue per worker, the stdout and stderr of a
worker together, and one for the host's own stream, each bounded in
bytes (`[logs] output_queue`, 1MB by default). A line is stamped as it
is read and tagged with its worker and stream as the host tagged it;
one longer than 64 KiB is cut there, with a marker. A writer thread
drains the queues in turn, a slice of each at a time, so one noisy
worker cannot delay a quiet worker's traceback behind its backlog, and
gathers what it takes into writes of up to 64 KiB. Under `[logs]
on_overflow = "drop_oldest"`, the default, a full queue drops its
oldest lines to make room; under `"block"`, the relay stops reading
that worker's pipes until its queue has room, so that worker blocks on
its own output and nothing else does. Where lines were dropped, the
relay writes a line in their place naming the worker and how many, and
lines a failed write loses are counted and reported the same way. One
warning event marks the start of a stall, and one more gives the count
when it ends.

**For managed servers and services (issue 492)** the relay is the same
command, with the same queues, writer, stamping, reopening and control
socket, started in a second mode in which it is the supervised
process's parent. Such a process cannot hand its pipes to another
relay, so its relay is the only holder of their read ends whatever its
place; being the parent then costs nothing more, and gives what that
issue needs: the real exit status of a process whose daemon was
restarted, written to the relay's record for the adopting daemon, and a
process identity that a reused pid cannot be mistaken for. A relay that
dies there takes its instance's output with it, and the controller
treats that as the instance exiting.

### Consequences

- Good: a stalled destination stops at the relay. The host's loop never
  writes a worker's output, so timeouts, reports and the supervisor
  connection keep running; under `drop_oldest` no worker waits at all,
  and under `block` only the worker whose queue is full.

- Good: each application's output has its own process and its own
  queues, so one application's flood or stall cannot touch another's,
  and the cost grows with the number of applications instead of
  meeting in one place.

- Good: the relay survives a daemon restart with its host, and a relay
  that dies costs a pause rather than lost lines, since the pipes stay
  open in the host until another relay has them.

- Good: the release log has one writer, which owns its reopening, and
  the host's code to relay and reopen is gone.

- Bad: a release runs one more process, and each worker pipe is open in
  two processes. The host now has a second socket to serve, and its own
  lines are dropped, and counted, when its pipe is full while no relay
  reads it.

- Bad: a line's order across workers is the order the writer takes the
  queues in, not quite the order the lines were read, when more is
  waiting than one write takes. Each line keeps the time it was read.

## More information

- Specification sections 11.2, 11.3, 11.8, 13.3 and 13.4, and appendix C.

- Issue 167, which builds the relay for Python hosts; issue 492, which
  extends it to managed servers and services; issue 572, the host's
  handling of a failed write, which the relay replaces; issue 573,
  muructl's writer thread, which stays; issue 166, `reopen_on_rename`.
