---
status: accepted
date: 2026-11-14
---

# ADR 0064: Instrumentation is a layer apart from the hooks

## Context and problem statement

A release has one hooks module (section 6.3), and it is imported as part
of loading the application, so importing it may already import the
application. Tools such as an import timer or an APM agent have to be
in place before anything of the application is imported, are often set
up by someone who does not own the application or its hooks, and
commonly run together. Issue 500 asks how such tools are added to a
release, and issue 501 settles the setting, its configuration and what
happens when a tool fails (section 6.15).

## Decision drivers

- Nothing of the application, its hooks module included, may be
  imported before the tools are in place, so that an import timer
  measures every import and an agent's import hooks see every module.

- An operator adds a tool without changing the application's code or
  its hooks module.

- Several tools run together, in an order the operator chooses, and
  each has settings of its own.

- A problem in a tool must not stop workers from starting, but a tool
  that is missing altogether is a configuration error worth stopping
  for.

- The callbacks a tool needs (around the load, a worker's life and each
  entry point) can be added one by one without changing the
  configuration.

## Considered options

- A second setting shaped like `hooks`, naming one more module.

- A table of tools keyed by module name, each with its settings.

- An ordered list of entries, each a module and its settings, kept
  apart from `hooks`.

## Decision outcome

Chosen option: "An ordered list of entries", because it is the only one
that carries both the order the callbacks need and each tool's
settings. A second hooks-like setting has nowhere to put settings and
says nothing about order, and a table keyed by module name loses the
order.

- The list is `instrumentation`: `[[tool.muru.instrumentation]]`,
  `[[instrumentation]]` in `muru.toml` and `muru.local.toml`, and a key
  of a declared release. Each entry has `module` and `settings`. The
  local file's entries come first, so a tool one developer adds measures
  the project's tools as well. A deploy takes the list from the source,
  as it takes the other tables.

- Callbacks are optional module-level functions found by name, as the
  hooks module's are. `configure(settings)` is the first, called once
  straight after the import with that entry's settings alone.

- Each module is imported, configured and started before the next is
  imported. Starting callbacks run in listed order and stopping ones in
  reverse.

- The modules are imported where the application is: in the host under
  preload, before the import check's baseline and the hooks module, and
  in each worker with per-worker imports.

- A module that cannot be imported stops the release. An exception from
  any callback is logged and the release carries on.

- The host receives the list in `MURU_INSTRUMENTATION`, which it removes
  from its environment before importing anything, so the settings reach
  neither the application's environment nor the host's command line.

### Consequences

- Good: an import timer or an agent is a configuration change, and the
  application and its hooks module stay as they are.

- Good: later callbacks (issues 502, 503 and 504) and a built-in module
  of Muru's own fit the same list without a new setting.

- Good: the import check does not report what a tool does at import,
  since its baseline is taken after the tools are imported.

- Bad: the same reason means the check cannot see a thread or socket a
  tool opens in the host and leaves open across the fork; that is the
  tool's to close before the fork.

- Bad: a tool that raises in a callback is only logged, so a broken
  agent can go unnoticed until its log line is read.

## More information

Section 6.15 of the specification, the how-to guide on instrumentation
modules, and issues 500 to 504.
