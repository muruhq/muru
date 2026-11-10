---
status: accepted
date: 2026-11-10
---

# ADR 0058: Diagnostics are a report that may be posted and a bundle that may not

## Context and problem statement

Section 18.6 had `muructl debug report` produce "a redacted diagnostic
bundle" for bug reports: versions, paths, effective configuration, release
and worker states, recent events and recent log lines. The bug report
form asked for its output, and the command was never built. Triage of a
live system needs far more than a bug report does (the logs themselves,
client addresses, every release's configuration), and what triage needs
cannot be made safe to publish. Issues 540 to 545 ask what the commands
are, what each holds and what they are called.

## Decision drivers

- A bug report is public. Whatever the command prints may end up in an
  issue anyone can read.

- An application's log is free text. Names, addresses and secrets in it
  cannot be found and replaced reliably.

- The people who own a system need everything, from the live system,
  before a restart loses it.

- Two commands that describe the same installation should describe it in
  the same terms.

## Considered options

- One command with a flag that separates the output that is safe to post
  from the one that holds production data.

- Two commands with different defaults, over one shared collection.

## Decision outcome

Chosen option: two commands, because a flag that decides whether
production data is included is easy to get wrong, and the wrong way
round publishes it.

- `muructl debug report` is a short summary printed to standard output,
  safe to post: it has no log lines at all, no request data, environment
  or secrets, and replaces what identifies the system with placeholders.

- `muructl debug bundle` is an archive of the whole system for a time
  window, which stays with the people who own the system.

- Evidence gathered automatically is an incident, produced by a rule,
  and an incident exported writes the bundle's layout.

- What the commands collect is shared code, `muru_controller::diagnostics`,
  with the report a safe subset of it.

The names follow what exists: one word per kind of output under
`muructl debug`, `--format` and `--output` as `muructl debug profile`
has them, and `capture` left for per-request records and recorded
traffic.

### Consequences

- Good: the default of each command suits its reader, and neither needs
  a flag to be safe.

- Good: the report can be pasted into a public issue as Markdown without
  being checked line by line for a path or a host name.

- Bad: the report leaves out log lines, which are often what explains a
  bug. A person adds the relevant lines by hand, having read them.

- Bad: placeholders remove detail that sometimes matters, such as which
  directory an interpreter is in. The report says how many values it
  replaced, and the bundle has the detail for those who may see it.

## More information

Sections 11.9 and 18.6 of the specification, and appendix A. Issue 542
tracks the work, with issues 540 (the report), 541 (the bundle) and 543
to 545 (incidents).
