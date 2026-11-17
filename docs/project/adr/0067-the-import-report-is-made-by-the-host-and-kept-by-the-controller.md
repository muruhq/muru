---
status: accepted
date: 2026-11-17
---

# ADR 0067: The import report is made by the host and kept by the controller

## Context and problem statement

unwrapt's profile recorder can write a report of a process's imports to
a file, but under Muru there is nowhere sensible for the host to write
one: its run directory is removed when the release stops, and it may not
be allowed to write to the state directory. Its advice also cannot take
account of what only Muru knows, such as the fork, the import check's
findings and the entry point being loaded. Issue 741 asks where the
report is made, analysed and kept, and how it is read (section 6.18).

## Decision drivers

- The report describes the host's load, and the analysis needs the
  process that loaded: unwrapt's analysis is Python, and the import
  check's findings are the host's.

- It must outlive the host, and go when the release goes.

- It is read as the other debug operations are, through the admin API,
  with the workers asked for what they have loaded since.

## Considered options

- The host analyses the report at `load_finished` and sends the result
  over its control connection; the controller keeps it, the daemon in
  its store beside the release and a run in memory.

- The host writes the report to a file the controller gives it, and the
  controller reads and analyses it.

- The report is kept in the host, and the debug operation asks it.

## Decision outcome

Chosen option: "the host analyses and sends, the controller keeps",
because the analysis runs where unwrapt and the import check's findings
are, the store already holds what belongs to a release and is cleared
with it, and the report survives a host that has stopped or been
replaced. A file needs a directory the host may write and the controller
keeps, which is what is missing, and the controller cannot run the
analysis. Kept only in the host, the report goes with it, and the host
that falls back to per-worker imports replaces its process. The report
is sent in parts, so no line of the connection holds the host's loop
long, and `muructl debug imports` is the debug operation `imports`, the
controller's kept report beside the workers' answers.

### Consequences

- Good: one report a release, read with the release's other debug
  operations, by the command and by a coding agent through MCP.

- Good: the analysis can name entry points, the fork and the import
  check's findings, and write its advice as configuration.

- Bad: a load that fails, or that `preload = true` refuses, ends before
  the host has a connection to send on, so its report is not kept;
  `muructl check --imports` shows it.

- Bad: the analysis adds to the load's time, which rules the report out
  as a default under `muructl run`.

## More information

Section 6.18 of the specification, issue 741, and ADR 0066 for the
module that records the report.
