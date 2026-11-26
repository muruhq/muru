---
status: accepted
date: 2026-11-26
---

# ADR 0080: Named deployments

## Context and problem statement

`muructl apply --prune` treats the files it is given as the whole node
(section 10.4). One operator with two applications, each with its files
in its own repository, can prune neither, since each set of files
describes half the node. Policy both applications use belongs in neither
set, and a host several applications serve has no stated owner (issue
766). Once what the daemon serves is its own kind of configuration (ADR
0079), the question is how it is divided, so that a set of files can be
applied, pruned and removed without regard to the rest.

## Decision drivers

- A node with one set of files, or none, never has to name anything and
  behaves as it does now.

- A set of files can be pruned and removed as a unit.

- Nobody's objects are removed by someone else's apply.

- One operator runs the node. The division organises it and is not a
  boundary of permission.

## Considered options

- Leave apply without a scope, and have every set of files declare the
  whole node.

- A namespace per set of files, with names private to it.

- Named deployments: ownership recorded on each object, names shared
  across the node.

## Decision outcome

Chosen option: named deployments, because a private namespace would
stop a route in one set of files from naming a host or an application
in another, which is the case of one site served by several
applications, and no scope at all is the fault being fixed.

- **A deployment is a named set of objects applied together.** A
  `deployment` key at the top of a file names it, so the name travels
  with the files. Each object an apply creates records the deployment
  that owns it: applications with their settings and releases, hosts,
  routes, upstreams, overlays, and the deployment's own classes, limits
  and conditions.

- **There is always a `default` deployment.** Files with no key belong
  to it, and so does what commands make, except that a release made by
  `muructl deploy` belongs to its application's deployment.

- **Names stay node-wide.** A route in one deployment may name a host,
  an application or a class in another, or one the daemon's file
  defines. Ownership says who may change a thing, not who may refer to
  it. Any deployment may put a route on any host: there is no list of
  who is allowed, and one is revisited only if deployments become a
  boundary of permission.

- **Only the owner changes or removes its objects.** An apply that
  declares an object another named deployment owns is an error naming
  that deployment. A named deployment may take an object from `default`
  by declaring it, which is how a node run by command moves to files.
  Nothing moves an object between two named deployments or renames a
  deployment; that idea is kept in issue 929.

- **Nothing is removed while another deployment names it.** A removal,
  by `muructl deployment remove` or by a prune, is refused while a
  route or other object in another deployment refers to what it would
  remove, and names them. `muructl app remove` is already refused while
  a route sends the application traffic. The way out is for the other
  deployment to drop its references first, or for both sets of files to
  go in one apply. Removing the other deployment's routes with the
  host, or leaving them recorded and matching nothing, were not taken.

- **Two deployments may not declare a route for the same host, path and
  class.** Apply already refuses two such routes in the files it is
  given; the refusal extends to a route another deployment holds.

- **One apply may carry several deployments.** The files given, by a
  directory or by repeated `-f`, are grouped by their key. Files with
  the same name merge into that deployment. They are planned together
  and applied all or nothing, so a route in one may name an application
  another adds in the same apply.

- **Prune has two levels.** `--prune` reaches only the deployments the
  files name: it removes what they own and the files no longer declare.
  `--prune=all` reaches every deployment, `default` included, so a
  named deployment the files do not mention is removed, and so is an
  application made by command and in no file. It is what `--prune`
  meant before, under a name that says how much, and it is how a set of
  files is made the whole truth for a node. Asked for by name, it is
  not refused when an application the files do not declare is serving;
  the plan lists what is drained. It is exempt from the refusal above,
  since it removes what refers and what is referred to together. A node
  run by command alone uses neither. What a prune does with a release
  is in ADR 0082.

- **A deployment is removed as a unit.** `muructl deployment remove
  NAME` drains what it runs, after listing what goes and asking, as
  `muructl app remove` does: `--yes` without a terminal, `--dry-run`,
  and application data directories kept unless `--data`. `default` can
  be removed too, which empties it and leaves it in place, and is the
  way to clear a running node of what commands and keyless files made.
  There is no `remove --all`: the offline reset of ADR 0081 covers the
  rest.

- **`muructl deployment list`** shows each deployment with its
  applications, hosts and routes.

- **Export follows the same division.** `muructl deployment export
  NAME` writes one deployment to standard output with its key;
  `default` is written without one, so a node that has only `default`
  exports as `muructl config export` did. With no name it writes
  `default` when no named deployment exists and is refused when one
  does. `export --dir DIR` writes one file per deployment into an empty
  directory, which applies back unchanged, and replaces the `--split`
  of section 10.4, which was never built. `export --app APP` writes one
  application with the routes that reach it and their hosts, as before,
  and `--as NAME` writes a `deployment` key at the top, for moving an
  application out of `default`. Definitions the daemon's file holds are
  not exported. A file format that nests each table under its
  deployment's name was not taken: it would be a second way to write
  every table.

- **`muructl app set` and `muructl app unset` are refused on an
  application a named deployment owns**, naming the deployment. A file
  states an application's table in full (ADR 0076), so a setting made
  by command there would be removed at the next apply, without the
  release that needed it failing until later. A one-off value goes on
  the deploy. In `default` the commands work as before, and `app set`
  warns when the application's settings were last written by an apply.

- **What shares a deployment** is what would be removed together. A
  host that separately released applications share goes in a deployment
  of its own, and each application's deployment holds its routes on
  that host.

### Consequences

- Good: two sets of files on one node can each be pruned, and removed,
  without touching the other.

- Good: a node that names nothing sees no difference, apart from
  `--prune=all` being the name for the prune that removes everything.

- Good: `default` gives commands and files one model, and a path from
  the first to the second.

- Bad: the store records an owner for every object, and every removal
  has to look for references from other deployments.

- Bad: moving an application between named deployments, or renaming a
  deployment, means running a copy alongside under another name and
  switching the routes.

- Bad: a user has to understand ownership to choose a layout for a
  shared host.

## More information

Sections 10.4 and 10.13. ADR 0079 separates the daemon's configuration
from deployments; ADR 0081 applies files at start; ADR 0082 covers
files and commands on the same application. Issues 766 (objects that
belong to a site), 929 (renaming and moving, kept for later) and 450.
