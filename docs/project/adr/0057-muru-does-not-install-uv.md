---
status: accepted
date: 2026-11-10
---

# ADR 0057: Muru does not install uv

## Context and problem statement

Section 8.4 ended uv's lookup order with a managed copy installed by
`muructl tools install uv`, a pinned and checksum-verified download kept
under STATE/tools, or the user cache in development. ADR 0036 mentioned
the command when it left out a second builder. The command was never
built: the daemon looked in STATE/tools for a copy nothing put there, and
development looked nowhere. `muructl cache clear` then had to decide what
to do with a directory it could not fill again.

## Decision drivers

- A `muructl tools` group that only ever installs one program has no
  reason to exist.

- uv already has ways onto a machine: the `muructl[uv]` extra, which puts
  it beside muructl, `uv tool install muructl`, which needs it already,
  uv's own installer and distribution packages.

- Downloading a pinned binary means keeping a version and a checksum per
  platform in Muru.

## Considered options

- Build `muructl tools install uv` as specified.

- Remove the command and the managed copy from the design and the lookup.

## Decision outcome

Chosen option: remove them, because the other ways of getting uv cover
every case the command was for, and a lookup location nothing fills is
only a place for a stray copy to hide.

The lookup order is `tools.uv`, `MURU_UV`, a uv beside muructl, and uv on
PATH. A machine without uv is told to install it. `muructl cache` does not
list or remove a `tools` directory, so everything `muructl cache clear`
removes can be re-created.

### Consequences

- Good: one fewer download path, and no checksums to maintain.

- Good: the statement that a cleared cache is re-created on demand holds
  for everything it covers.

- Bad: a node with no uv needs one more step before it can build, outside
  Muru. ADR 0036's position is unchanged: a node where fetching uv is
  forbidden deploys a prebuilt directory or builds in CI.

## More information

Section 8.4 of the specification and issue #538. ADR 0036 keeps its
decision; only its passing mention of the command no longer applies.
