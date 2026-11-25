---
status: accepted
date: 2026-11-24
---

# ADR 0076: Application settings in place of the previous-release layer

## Context and problem statement

A deploy takes every setting its flags and the source's `[tool.muru]`
do not give from the release before it (section 3.3). That layer is how
an operator keeps what belongs to a deployment rather than the code, such
as a database URL, a secret's reference or this node's worker count,
without giving it again on every deploy. It is also hidden state. A flag
given for one trial release stays in force through every later deploy,
and after a rollback the next deploy takes from the release rolled back
to, so a flag given on the bad release disappears without a word. A
carried setting that names a path points into the previous release's
source: a release served static files from another release's source
directory (issue 861), and an interpreter can point into another source's
`.venv`. Layer 4 of section 3.3, application settings in the controller,
is where deployment settings belong, and it held none. The question is
where a deployment's settings live once a deploy takes nothing from the
release before it (issues 861 and 899).

## Decision drivers

- A release's settings come from what is in front of the operator: its
  own flags or table, the source, and state they set on purpose and can
  list.

- A bare redeploy keeps the variables and secrets earlier deploys were
  given, without a script that repeats every flag.

- muru-hosting speaks only the admin API, and a document applied with
  `muructl apply` describes the same state as commands typed at a
  terminal.

- Nothing that names a path in one source is carried to another.

## Considered options

- Keep the previous-release layer, and fix the paths it carries one by
  one.

- Keep deployment settings in `[tool.muru]`.

- Give the flags again on every deploy.

- Settings on the application, set by command or by document, which
  every release deployed after they are set takes; the previous-release
  layer removed.

## Decision outcome

Chosen option: settings on the application, because they keep what a
deployment needs across deploys as state that is set, shown and exported
on purpose, and let the previous-release layer go with its faults.
`[tool.muru]` is the source, the same in every mode and on every node,
so it is the wrong place for a database URL, a secret or one node's
capacity; giving every flag on every deploy makes a bare deploy at a
terminal lose a variable without a word.

- **What an application holds.** Environment variables and secrets by
  reference (`STORE:KEY`, never a value), processes, threads, sync
  threads, concurrency, `max_requests`, `max_lifetime`, the host's
  timeouts (soft, hard, hard drain grace, shutdown, WebSocket drain
  grace, exit grace, loop stall, GIL stall) and the memory policy. Not
  the entrypoint, interface, factory, interpreter, preload mode, hooks,
  build, bytecode, handoff, reflection, gRPC-Web origins, redirect
  settings, file hint roots or static entries: they say what the code is
  or name a path in one source, so they stay with the source and the
  release. Tables (worker groups, mounts, static entries, logging,
  scaling, error pages, commands, instrumentation) stay with the source
  and the release too. The limits on requests the edge serves (the
  request timeouts and the body limit) stay with releases, routes and
  hosts.

- **How they are set.** `muructl app set APP` takes the deploy's flags
  for them (`--env`, `--secret`, `--processes`, `--timeout`, `--memory`
  and the rest), `--replace` to drop the others first; `muructl app
  unset APP KEY...` removes them by the key a release records its layer
  under (`processes`, `timeouts.hard_timeout`, `memory.soft_limit`,
  `env.NAME`, `env` for every variable) or all with `--all`; `muructl app
  show APP` lists them. They go through `SetAppSettings`, and `GetApp`
  returns them, so muru-hosting needs nothing else. A document's
  `[app.NAME]` takes the same keys (`env`, `processes`, `timeouts`,
  `memory`), and `muructl config export` writes them. A document states
  the table in full, as it does an application's identity and priority,
  so applying one without them removes them. Setting them creates the
  application when it does not exist, so they can be in place before the
  first deploy.

- **Precedence**, highest first: the release's own settings (deploy
  flags, or a declared release's table); the application's; the source's
  `[tool.muru]`; the daemon's configuration, then the defaults.
  Variables and secrets layer by name across all of them. A declared
  release takes the document's settings for its application, so a file
  still means the same wherever it is applied. A managed HTTP release
  takes only the environment, having no workers.

- **When they apply.** A release records its settings when it is
  created, as before; changing the application's settings changes no
  release, and takes effect at the next deploy, which `muructl app set`
  says. The layer a release records is `the application`, which
  `deploy --dry-run` and `config show --effective` show.

- **The previous-release layer goes.** A deploy takes nothing from the
  release before it: not the settings a flag can set, not the
  environment, not the static entries, not the interpreter, not the
  handoff. `muructl deploy --fresh`, `DeployRequest.fresh` and the layer
  name `the previous release` go with it.

### Consequences

- Good: a release's settings are what its flags, its application, its
  source and the defaults say, each named in `--dry-run`; a rollback and
  a trial flag leave nothing behind; no release reaches into another
  release's source.

- Good: muru-hosting can keep a site's environment and secrets on its
  application and deploy each release with only what changed.

- Bad: an operator who relied on a flag being carried must set it on the
  application, once. With no users in alpha, there is no migration:
  existing releases keep the settings they recorded, and the next deploy
  takes only what the application, the source and its flags give.

- Bad: two ways to keep a deployment setting, the application and
  `[tool.muru]`, which the daemon guide has to tell apart: on the
  application when it belongs to the deployment, in `[tool.muru]` when it
  follows the code.

## More information

Section 3.3 (inheritance and precedence), section 10.4 (declarative
configuration), issues 861 and 899, and muruhq/muru-hosting#31, which
deploys every release fresh until this lands. Issue 862 asks whether the
daemon's own configuration file takes `[app.NAME]` tables; these settings
are applied through `muructl apply` and kept in the state store.
