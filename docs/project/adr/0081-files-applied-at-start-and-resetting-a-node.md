---
status: accepted
date: 2026-11-26
---

# ADR 0081: Files applied at start, and resetting a node

## Context and problem statement

A daemon that is to serve an application takes a start and then an
apply or a deploy, and answers 503 in between. No one step provisions a
node, which a container whose image carries its files wants, and which
muru-hosting tried to get by putting a site's application table in the
daemon's file (issue 862). With the daemon's file no longer holding a
deployment (ADR 0079), the question is how a start applies files, what
it does on every later start and when the apply fails, and how a node
is put back to nothing.

## Decision drivers

- A flag has one meaning, and files that are accepted are applied.

- A bad file never takes down what serves.

- A failure is visible to whatever supervises the daemon, which for
  `muructl daemon run` under systemd or in a container is the process
  exiting.

- A destructive step cannot sit in a unit file where it fires on every
  restart with nobody to ask.

## Considered options

- For later starts: apply every time, or seed only an empty store.

- For a failed apply: the daemon stays up on what the store held, or
  the start fails.

- For a start with nothing running: restore what the store holds and
  then apply, or apply first.

- For putting a node back to nothing: a `--reset` flag on the start, a
  command that removes everything from a running daemon, or a command
  that runs only when everything is stopped.

## Decision outcome

- **`--apply PATH` on `muructl daemon start`, `muructl daemon run` and
  `muructl up`** applies the files as `muructl apply -f PATH` would,
  and may be given more than once, as `-f` may. It takes a file or a
  directory, and several deployments (ADR 0080).

- **It applies at every start.** Seeding only an empty store would
  give the flag a second meaning that depends on state nobody sees, and
  would ignore a changed file without a word, which is the fault issue
  862 reported. The costs are that a restart can deploy an edit nobody
  had applied, and that a change made by command does not survive a
  restart where the files state otherwise. On a machine changed by
  hand, `--apply` is left off the unit.

- **It never prunes unless asked.** `--prune` and `--prune=all` beside
  it mean what they mean on `muructl apply`. With `--prune=all` the
  files are the whole truth at the point of start, for everything a
  file can state.

- **It always applies, running daemon or not.** `muructl up` is
  `muructl daemon start` under a shorter name, and with a daemon
  already running both print that it runs and exit 0 without looking at
  their other flags. With `--apply` they then apply the files to that
  daemon and report the apply, so the command means "running, and
  serving these files" and a script can run the same line every time.
  The other start flags are still not looked at.

- **`muructl daemon upgrade` does not apply again.** It re-executes the
  daemon with its own arguments, and an upgrade should not deploy.

- **A failed start apply fails the start.** Nothing changes unless the
  whole plan is valid, a prune's removals included. Additions come
  first and removals last, only once the releases the apply adds serve,
  as with any apply. The daemon exits non-zero with the apply's report:
  `muructl daemon start` and `muructl up` print it and fail, and
  `muructl daemon run` logs it and exits. The exit is a detach, as
  `muructl daemon stop` is: an edge and hosts that were running stay up
  for the next daemon to adopt. The first proposal, that the daemon
  stays up on what the store held, was dropped because `daemon run` has
  no separate command to fail, so the failure would be known only from
  the log.

  When the daemon was already running, a failed apply leaves it running
  and the command exits non-zero, as a failed `muructl apply` does.
  Only a daemon the command started exits. Either way what was serving
  keeps serving.

- **On a start with nothing running, the daemon restores what the store
  holds and then applies.** A start with `--apply` is then a start
  followed by an apply on a running daemon. A bad file and a reboot
  together are not an outage: the failed apply leaves the restored
  deployment detached and serving. Applying first would avoid starting
  a release only to replace it, at the price of that outage and of a
  way of its own to apply with nothing running. The edge serves the
  restored deployment while the apply runs, as a running node does
  during any apply.

- **Readiness waits for the apply.** `muructl daemon start` and
  `muructl up` return, and the daemon reports ready to a service
  manager or a probe (section 13.11), only once the apply has finished.

- **The log and the events say what is pending before anything is
  restored.** The files are read and planned against the store first,
  which needs nothing running, and the daemon records that a start
  apply is pending, with what it will change or why it cannot be
  applied, before it starts the old releases. A release it restores
  says it was restored from the store, and the apply's own events say
  they belong to the start. The names of the events are settled with
  the code.

- **`muructl daemon reset` puts a node back to nothing, offline.** It
  removes the state store and what releases left: environments,
  bytecode, static directories, the upload spool and events. It runs
  only when nothing of the instance is running, no daemon, edge, host,
  relay or managed server, and otherwise is refused, names what runs
  and says to use `muructl daemon stop --shutdown`. It lists what it
  will remove and what it keeps, and asks; `--yes` is for use without a
  terminal, without which it prints the list and removes nothing.
  `--dry-run` prints the list and stops, exiting 0 when a reset would
  be allowed; `--json` gives the list to a script. The list has counts
  read from the store and each directory with its size, and a store
  that cannot be read is listed as a file. The daemon's configuration
  file and the instance's definition are kept, and so are application
  data directories and logs unless `--data` and `--logs`.

  It is offline on purpose: nothing has to be drained, no apply can be
  half done, and it works on a store the daemon can no longer read.
  There is no `--reset` on a start, which could not ask. Making a
  running node match a set of files, whatever it held, is
  `--prune=all`, which converges and drains only the difference.

### Consequences

- Good: one command brings up a daemon that serves, and reports ready
  when it does.

- Good: a start with `--apply` means what a start and an apply mean,
  with no rules of its own.

- Good: a bad file does not stop a site whose hosts outlive the daemon,
  and a supervisor sees the failure.

- Bad: after a reboot that failed its apply, and until a start
  succeeds, the restored deployment serves with no controller and no
  admin API. In a container, where the daemon is the first process,
  nothing outlives it and a bad file is a crash loop, which is also
  what stops a bad image from rolling out.

- Bad: a start that changes something starts the old release before
  the new one, so it is slower than applying first would be.

- Bad: someone who wants the node to keep running on what it had when
  the files are bad has to leave `--apply` off the start and run
  `muructl apply` as a step of its own.

## More information

Sections 10.4, 10.10, 13.3, 13.4, 13.11 and 13.13. ADR 0079 separates
the daemon's configuration from deployments, and ADR 0080 defines the
deployments and the two levels of prune this record relies on. Issues
862 and 450.
