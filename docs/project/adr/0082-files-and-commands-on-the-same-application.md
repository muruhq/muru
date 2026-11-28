---
status: accepted
date: 2026-11-26
---

# ADR 0082: Files and commands on the same application

## Context and problem statement

An application can be changed by command and by applied files, and what
happens when both act on it was never examined (issue 450). A file that
declares `serving = "r2"` puts r2 back on every apply, even after an
emergency `muructl rollback` away from it. A file states an
application's table in full (ADR 0076), so a setting made with `muructl
app set` is gone at the next apply. A release removed by command while
its file remains is deployed again and given the traffic (issue 926).
With files applied at every start (ADR 0081), each of these can happen
at a restart, with nobody reading the plan. The question is who owns an
application, its traffic, its settings and its releases when both ways
of working meet.

## Decision drivers

- An apply never undoes an emergency step taken by hand without saying
  so, and never the one that took a bad release out of traffic.

- A change that will not last is refused or announced when it is made,
  not discovered at an unrelated time.

- What an apply does depends on the files and the store, never on a
  history nobody can see.

- A release is never removed while it serves or while a rollback would
  return to it.

## Considered options

- Three-way merging: an apply reverts only what the files changed since
  they were last applied.

- An application that may be both: declared in files for its settings,
  host and routes, with its releases made by `muructl deploy`.

- A record of removed release ids, so a file that still declares one is
  refused.

- An application is the files' or the command line's, with the meeting
  points refused or announced one by one.

## Decision outcome

Chosen option: an application is the files' or the command line's.
Merging makes an apply depend on what was applied before. The mixed
application needed a prune rule of its own and disagreed with the prune
that makes files the whole truth (ADR 0080). A record of removed ids
refuses a name such as `bugfix` when it is used again weeks later.

- **An application declared in applied files gets its releases and its
  settings from the files.** One that no file declares is the command
  line's: its releases come from `muructl deploy` and its settings from
  `muructl app set`. Hosts and routes are declared in files either way,
  and a route in a file may name an application no file declares, so
  hosts and routes in version control with the application run by
  command stays a supported layout.

- **`muructl deploy` on a declared application is the emergency path.**
  It works, the release belongs to the application's deployment, and
  the deploy always warns: the release is not in the files, and this is
  what the next apply does with it. `muructl deployment diff` lists it
  as not declared and made by command, and `muructl deployment export`
  writes it for the files.

- **A `serving` key in a file is owned by the file.** An apply makes it
  so. A file without the key leaves traffic to commands: an apply then
  decides traffic only for the releases it adds.

- **An apply never undoes a rollback on its own.** After `muructl
  rollback`, an apply whose `serving` names the release rolled back
  from is refused, naming the rollback and when it was made, until the
  key names another release or that release is promoted again by
  command. This extends the rule for a rollout that rolled back
  (section 10.4). No flag overrides it: it would be left beside
  `--apply` in a unit file. After a rollback by command, a start with
  `--apply` therefore fails until the file or the daemon is brought
  into agreement.

- **Any other revert is allowed and announced.** The store records how
  the serving release was last set, and when an apply moves traffic
  away from a release a command made serving, its plan says so.

- **An apply that adds more than one release of an application needs
  the `serving` key**, and is refused without it. This replaces "the
  last in id order serves", which is arbitrary for ids with no order,
  such as commits. Only releases the store does not have count as
  added, and an apply that adds exactly one gives it the traffic, as
  before. An added release taking traffic only when it sorts after the
  one serving was not taken, for the same reason.

- **A release kept in files is removed through the files.** The store
  already records what the last apply declared for each application,
  and `muructl release prune` leaves a declared release unless
  `--force` (section 9). `muructl release remove` gets the same guard.
  The message says to delete the release's file and apply with
  `--prune`, or the next apply creates the release again. Nothing about
  removed releases is remembered.

- **For a declared application, a prune treats each release the files
  do not declare by where it stands.** Serving: the whole apply is
  refused, as before. The rollback release: skipped, the plan saying
  so, and removed by a later prune once it is no longer the rollback
  release. Neither: removed. Run on 0.1.0a36, an apply with `--prune`
  removed a retained release a rollback would have returned to, and the
  rollback then failed for a release that did not exist.

- **An application's own settings are declared in one file of an
  apply.** A second file that sets any key of `[app.NAME]`, `env`
  included, is an error, as a host declared twice is, in place of a
  later file's value replacing an earlier one's in file name order.
  Releases of one application still add up across files.

- **An apply's plan names each setting it removes from an
  application**, since a file states the table in full.

- **`muructl app set` on an application kept in files** is refused in a
  named deployment and warns in `default` (ADR 0080).

### Consequences

- Good: a restart cannot put a rolled back release back in traffic, or
  bring back a release that was removed.

- Good: whether an application is run from files or by command is one
  fact about it, and each command says so when it crosses the line.

- Good: releases named by date or by commit work, since nothing depends
  on the order of ids.

- Bad: an application's settings cannot be kept in a file while it is
  released with a bare `muructl deploy`, without a warning each time.

- Bad: a new or reset node given files with several releases of an
  application needs the `serving` key, where it used to pick the last.

- Bad: after a rollback by command, files that still name the bad
  release block every apply, a start's included, until they are
  changed.

- Bad: the store records more: how the serving release was last set,
  and who last wrote an application's settings.

## More information

Sections 9, 10.4 and 10.13. ADR 0054 defined the `serving` key, whose
rule for several added releases this record replaces. ADR 0076 put
release settings on the application and said a document states them in
full. ADRs 0079, 0080 and 0081 define the documents, the deployments
and the start apply this record builds on. Issues 450, 925 (settings in
an applied file are dropped) and 926 (a removed release is deployed
again and given the traffic).
