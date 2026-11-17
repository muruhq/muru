---
status: accepted
date: 2026-11-17
---

# ADR 0068: Promoting an overlay's pin switches the serving release

## Context and problem statement

`muructl overlay promote` folded an active overlay into the base by
writing the routes as the overlay left them (section 9.10). An overlay's
`pin` is a 100% share of one release in the route's traffic policy, so a
promoted pin stayed on the route as a split of its own. Later deploys
never took traffic, `release list` and `scale status` called the old
release serving, and the next apply of the files that declare the route
removed the pin (issue 759). What should promoting a pin, and a split,
write?

## Decision drivers

- The overlays guide sells promote as how a timed switch becomes the new
  normal, so a promoted cutover must behave as a deploy of that release
  would: later deploys replace it, and rollback goes to the one before.

- Applied files are the source of the declared configuration. A promote
  that writes state the files contradict is undone by the next apply,
  so the operator needs to be told what to put in them.

- The reports must not call a release serving that takes no traffic
  without saying where the traffic goes.

## Considered options

- Keep writing the pin and split into the route, and report it.

- Switch the serving release for a pin of every route of the
  application, refuse a partial pin or one for a client class, and write
  a split into the route.

- Switch the serving release for a pin of every route of the
  application, and refuse to promote a split, refusing a partial pin or
  one for a client class as well.

## Decision outcome

Chosen option: the third, decided by the maintainer on the issue. A pin
of every route of an application to one release makes that release the
application's serving release, as `muructl rollout bluegreen` would, the
release that served before drained and retained as after a deploy, and
leaves the routes' traffic as the base declares it. A pin of only some
of the application's routes, of a client class's route, with a target,
or while a rollout of the application is under way, is refused, since a
switch moves every route. A split is refused with the `[route.traffic]`
table to put in the applied files, because shares between releases are
a route's traffic policy, or a rollout's work. Everything else the
overlay sets is written into the routes and applications as before.
Promote's answer carries, as TOML, what the applied files must say for
the next apply to keep the promotion: the `serving` key, for files that
name one, and each route written. `release list` and `scale status`
keep SERVING meaning the application's serving release and add a
`traffic:` line with a release's share of each route that splits.

### Consequences

- Good: a promoted cutover is an ordinary serving release, and later
  deploys, rollbacks and capacity schedules treat it so.

- Good: the operator is told what the files must say, rather than
  finding the promotion undone by the next apply.

- Bad: an overlay that pins one route of an application with several
  cannot be promoted; the switch is done with a rollout or by pinning
  every route.

- Bad: a split cannot be promoted at all; it is declared in the files.

## More information

Issue 759, and section 9.10 of the specification.
