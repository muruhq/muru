---
status: accepted
date: 2026-11-20
---

# ADR 0070: A named instance is a definition whose locations follow from its name

## Context and problem statement

Running a second daemon for one user needed `--config`, `--state-dir`,
`--run-dir`, `--log-dir` and `--cache-dir` on every daemon command, and
nothing recorded that the second daemon existed, so it was easy to start
it on the wrong directories or forget it. The instance registry (ADR
0069) says what runs, but not what a user has set up and stopped. The
question is how a user defines several daemons so that one word reaches
each (section 13.8, issue 810).

## Decision drivers

- One name on every daemon command, with nothing else to remember.

- No new registry of definitions that can disagree with the files on
  disk.

- The same namespace for instances on another machine later, which have
  no local files but a definition.

- Nothing re-creatable kept twice.

## Considered options

- A file listing every named instance and its locations.

- A directory per instance in the configuration area, `instances/NAME/`,
  holding a small `instance.toml` and the daemon's own `muru.toml`, with
  every location following from the name.

- Keep the location flags and let the user write a shell alias.

## Decision outcome

Chosen option: a directory per instance whose locations follow from its
name, because a name is then all a command needs, the definition is the
files themselves, and a remote instance can later be `kind = "remote"` in
the same place.

- `--instance NAME`, or `MURU_INSTANCE`, puts the configuration file, the
  state, run and log directories under `instances/NAME` in their areas;
  on macOS the state gets a `state` directory of its own, since the
  default configuration and state locations are one directory there. An
  explicit flag or variable still overrides one location; every one of
  them with `--instance` is an error.

- The cache is shared: everything in it can be re-created, and the
  runtime libraries and certificates are the same for every instance.

- `instance.toml` says what the definition is, `kind = "daemon"`, with
  room for a description, and never holds a secret, so listings and the
  MCP tool can show definitions freely. A directory without one is not a
  definition. Muru owns the file; `muructl instance create` writes it.

- Names are 1 to 24 of `[a-z0-9-]`, starting with a letter, so titles
  stay short and socket paths stay within 104 bytes on macOS. `user` and
  `system` are reserved and name the default daemons.

- `muructl instance create` refuses an address another defined or
  running instance uses, and `instance remove` refuses while the instance
  runs, and keeps its state unless `--state`.

### Consequences

- Good: `muructl daemon start --instance dev` and `stop --instance dev`
  need nothing else, and `instance list` shows a defined instance that is
  stopped.

- Good: a systemd template unit, `muru@.service`, runs any named
  instance with `%i`.

- Bad: a variable such as `MURU_RUN_DIR` set in a shell still overrides
  the run directory, so two named instances started from that shell share
  it and the second is refused.

- Bad: `MURU_INSTANCE` also carries the instance tag of the process it
  runs in, so a value that is not a name, or is the short form of the
  `MURU_INSTANCE_ID` beside it, is not taken as one.

## More information

Section 13.8 ("Named instances") and 13.3 of the specification, issue
810 and the tracking issue 813. ADR 0069 records the registry this
catalogue sits beside, and issue 814 the remote instances it leaves room
for.
