---
status: accepted
date: 2026-11-26
---

# ADR 0079: The daemon's configuration and deployments are separate documents

## Context and problem statement

The daemon's configuration file and the files given to `muructl apply`
share one format, one parser, one reference page and one JSON Schema
(section 10.3), and each reader takes its own tables from it without
saying what it left. A file with `[app.shop] data_dir`, a host or a
route starts a daemon cleanly and does nothing with them (issue 862),
while the generated reference says those tables "may be in either" file.
An applied file with `[limits]` succeeds having skipped it, some tables
with a warning and some without. The words do not help: `muructl apply`
"applies declarative configuration", `muructl config` means the desired
state in the store, and `muructl daemon config` (issue 894) is to mean
the daemon's own file. A file kept beside a project's code is named
`muru.daemon.toml` and holds releases, not the daemon's settings. The
question is how many kinds of configuration there are, what each holds,
and how each is named and changed.

## Decision drivers

- Input that is accepted is used. A table in the wrong file is an error
  that says where it goes, never a silence.

- A reader can tell from a command's name and help which kind of
  configuration it changes.

- The daemon's file can always be loaded by itself, before anything is
  deployed.

- A setting has one place to go, by what kind of setting it is.

## Considered options

- Keep one format, and have each reader warn about the tables it skips.

- Let the daemon's file hold applications, hosts and routes too, applied
  at start, as muru-hosting first wanted for a site's data directory.

- Three kinds of configuration, each with one home and one way to
  change it, each reader refusing the others' tables.

## Decision outcome

Chosen option: three kinds of configuration, because a warning still
lets an apply or a start succeed having ignored part of its input, and a
daemon's file that holds a deployment makes the file a second copy of
what the state store holds, with no rule for which wins after a deploy
by command.

- **The three kinds.** *Project settings* say what a release of a source
  is, live in the source (`[tool.muru]`, or the project's `muru.toml`),
  and take effect at the next deploy or apply that names the source. The
  *daemon's configuration* says how this daemon runs, lives in the one
  file `muructl daemon status` shows, and takes effect on a reload, or a
  restart for the few keys read at start. *Deployments* say what the
  daemon serves, live in the state store, and are changed by commands
  or by files given to `muructl apply`.

- **Each table belongs to one document.** The daemon's file takes
  `[daemon]`, `[tools]`, `[[listener]]`, `[limits]`, `[proxy]`,
  `[compression]`, `[secrets]`, `[logs]`, `[logging]`, `[shutdown]`,
  `[metrics]`, `[events]`, `[dashboard]`, `[overlay_webhook]` and the
  node-wide `[error_pages]` and `[error_override]`. Applied files take
  `[app.NAME]` and its releases, `[host.NAME]`, `[[route]]`,
  `[upstream.NAME]` and `[overlay.NAME]`. The reference and the schema
  split in two to match.

- **Each reader refuses the other's tables.** The daemon does not start
  or reload with a deployment table in its file, and says to apply it.
  An apply fails on any daemon table, and says to use `muructl daemon
  config`. An apply with any error applies nothing, as before.

- **Node-wide definitions are the one stated overlap.** The daemon's
  file may define client classes, rate limits and conditions for the
  whole node. A deployment may use them and define its own. A name is
  defined in one place: a collision is one uniform error for all three
  kinds, where a class or limit failed the apply and a condition was
  dropped with a line in the log.

- **References point one way.** A deployment may name what the daemon's
  file defines (secret stores, classes, limits, conditions). The
  daemon's file never needs anything a deployment defines in order to
  load or to run. A condition there may still name an application or an
  upstream in a signal's labels: a label filters figures and resolves
  nothing, and a condition whose object is gone reads `unknown` as it
  does now. Nothing checks or warns for it. A condition belongs beside
  what it watches, in that deployment, and other deployments use it by
  name; the daemon's file is for one with no deployment to own it.

- **The store is the only truth for a deployment.** Files given to
  apply are input to a reconciliation, and the daemon never rereads
  them by itself.

- **A start and a reload read the daemon's file the same way.** A start
  that adopts a running edge and hosts applies the file it was given as
  a reload would, replacing the edge when the listeners differ. Run on
  0.1.0a36, a start after `muructl daemon stop` with a file naming
  another listener kept serving on the old one until a reload, while
  status showed the new file.

- **A release's settings have one place each.** With the code, in the
  project's settings; for this node, on the application (`[app.NAME]`,
  or `muructl app set`, ADR 0076); for one release, on the release.
  A deployment file is therefore written for one node, and nothing
  moves to the daemon's file. No layer of overrides is added for values
  that differ between nodes: nodes that share most of a deployment
  split it across files, a common file and one for the node. Section
  10.3 said applied files support `${VAR}` interpolation, and section
  10.4 gave export a `--param`; neither was built, and both leave the
  specification. A value from outside reaches a release through a
  secret store, the `env` kind among them. The idea is kept in issue
  939.

- **The commands for the daemon's file** are `muructl daemon config
  path`, `show`, `check`, `edit`, `set`, `unset` and `replace` (issue
  894). `check FILE` resolves the file's relative paths against its own
  directory, since it answers whether a daemon started with `--config
  FILE` would take it. `replace FILE` resolves them against the
  directory of the daemon's file, since that is where the file is
  going, and `replace FILE --check` tests a replacement. Their check
  refuses deployment tables as a start does. `set`, `edit` and
  `replace` warn when a flag the daemon was started with stands over a
  key they changed, and the full form of `show` is a valid file. There
  is no `daemon config apply`: a file changed by other means is checked
  with `check` and applied with `muructl daemon reload`.

- **The commands for deployments** are `muructl apply` and the group
  `muructl deployment`, which replaces `muructl config` with no alias:
  `show`, `diff`, `export`, and with ADR 0080 `list` and `remove`.
  `config show --effective` becomes `deployment show --effective`. A
  deployment always means a named set, `default` included (ADR 0080);
  the whole is "what the daemon serves", never "the deployment".
  `muructl deploy` keeps its name: it is a whole prefix of the group's,
  so completion stops at it, and subcommand prefixes are not inferred.

- **`muructl apply -f` may be given more than once**, and a directory
  holding a `pyproject.toml` or a project's `muru.toml` is refused with
  a message naming the file to give, where it failed on the project
  file's first key.

- **An applied file kept beside a project's code is `muru.deploy.toml`**
  by convention, in place of `muru.daemon.toml` (ADR 0051), in
  `muructl render`'s output, the guides and the MCP server's text.

- **`muructl render` stays a command of its own** that contacts no
  daemon, and is not folded into `muructl deploy --dry-run`, which
  needs one. `--release ID` is required: the `r1` default made two
  renders into one file a release declared twice, and an id for a file
  in version control should be a date or a commit, not one node's
  counter. `--deployment NAME` writes the `deployment` key (ADR 0080)
  at the top, for a file that holds one release, and is left off when
  appending to a file. `render` takes `--metadata` and fills the git
  keys as `muructl deploy` does (section 9.14), so a release made
  through a file records what one made by command does.

- **`muructl run` and `muructl serve` sit outside the three kinds.**
  They take their settings from the project, their flags and the
  environment, and have no daemon configuration file and no
  deployments. `muructl apply` is refused against them, as now.

### Consequences

- Good: no table is accepted and ignored, in either direction, and the
  error names the command that takes it.

- Good: the daemon's file is small, loads by itself, and has commands
  that check it before the daemon reads it.

- Good: `apply`, `deployment` and `daemon config` each name one thing.

- Bad: one file can no longer be given to both `daemon start --config`
  and `apply -f`, each taking its half. That worked only until the file
  defined a class or a limit, when the apply failed.

- Bad: `muructl config`, `muru.daemon.toml` and `render`'s default id
  go without a compatible form. There are no users in alpha.

- Bad: a node whose daemon should come up already serving needs the
  files applied at start, which ADR 0081 adds.

## More information

Sections 10.3, 10.4, 10.13 and 13.8, and appendices A and C. Issues 862
(the table that was ignored), 894 (the daemon's commands), 450 (the
guide that states the model), 925 and 927. ADR 0051 named
`muru.daemon.toml`, which this record renames. ADR 0052 defined
`muructl render`, whose default release id this record removes. ADR 0076
put release settings on the application. ADRs 0080, 0081 and 0082
continue this one: named deployments, files applied at start, and files
and commands on the same application.
