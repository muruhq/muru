---
status: accepted
date: 2026-11-23
---

# ADR 0074: --instance selects a running instance by what the list shows

## Context and problem statement

`--instance` took only a named instance's name, `user` or `system`, while
`muructl instance list` showed every unnamed instance by a tag such as
`run:9080` or `91720d04` under a column headed NAME, and `MURU_INSTANCE`
inside a run held a tag the option refused. Most instances on a
development machine are never named: every `muructl run`, and every
daemon started with its own directories, as muru-hosting starts one per
site with a run of its helper beside each. Reaching one needed its admin
socket's path. The question is how a person, a script or a coding agent
names a running instance that has no name (section 13.8, issue 868).

## Decision drivers

- What the list shows can be given back to `--instance`, and
  `MURU_INSTANCE` takes what `--instance` takes.

- A name still decides the directories of the daemon it starts, so the
  commands that start or define a daemon keep the strict rule.

- A convenience never guesses: an ambiguous value fails, listing what
  would have worked.

- One resolver, reused by every command that chooses among instances,
  including those that will choose among the detours of one daemon
  (issue 873).

## Considered options

- Leave `--instance` for names, head the column for unnamed instances
  differently and point at `--socket`.

- Accept the printed tag and a prefix of the ID when selecting.

- Accept, in order, a name, the tag, an ID prefix, a label and an
  application name, with a `--label` flag to set the label, a line
  saying which instance discovery chose, and the same selector on the
  MCP tools.

## Decision outcome

Chosen option: the five rules with the label flag, the line and the MCP
selector, because the first two remove the confusion but leave several
unnamed instances on one machine awkward to reach, which is the normal
state for anyone running more than one project.

The first rule that matches anything decides, and more than one match is
an error rather than a reason to try the next rule. The application name
is last because two instances serving applications of one name is
ordinary. The rules live in `muru_controller::selector` as a function
over registry records, which reads nothing itself. A selected instance
has no locations of its own, so the commands that resolve locations
(`daemon status` for a stopped daemon, for one) use only a value that
names a defined instance.

### Consequences

- Good: every row the list shows for a running instance can be passed
  back, and the NAME column is the value that selects it alone.

- Good: a label set with `--label` gives an unnamed instance a stable
  handle without the directory layout a named instance implies.

- Good: `MURU_INSTANCE` means only what a person chose. The tag Muru
  passes down to an instance's processes, which shared the variable and
  made a value passed down look like a choice (the consequence ADR 0070
  records), is `MURU_INSTANCE_TAG`.

- Good: a `muructl` command run inside an instance, by its application
  or a project's command, reaches that instance by the
  `MURU_INSTANCE_ID` it carries, unless a flag or variable chooses
  another, where it used to depend on discovery from its directory. A
  command only a daemon answers passes over a run or serve found this
  way, so a run's application deploying still reaches the daemon.

- Bad: a value is resolved against what is running at that moment, so a
  tag or an application name that is unique today can be ambiguous
  tomorrow; the error says what to pass instead.

- Bad: commands reached by discovery on a busy machine print one more
  line on standard error.

## More information

Section 13.8 (Selecting a running instance), section 10.11 for the MCP
tools, issue 868, and issue 873 for the detours that will reuse the
resolver. ADR 0070 introduced named instances and is not changed by
this record.
