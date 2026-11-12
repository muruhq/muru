---
status: accepted
date: 2026-11-12
---

# ADR 0060: The dashboard shows and never changes, in every mode

## Context and problem statement

Section 10.7 gave the daemon's dashboard actions: maintenance mode on and
off, promote, roll back, drain, pin and unpin a worker group's count, and
restart a worker. ADR 0030 put them behind a session asked for with
`muructl dashboard login --actions`, which the `[dashboard]` table's
`actions = true` had to allow, and protected each form with the session's
own token and a same-origin check. A development run's dashboard never
had them. Every action was already a `muructl` command, so the forms were
a second path for control, with its own checks, events and tests, beside
the admin API's.

Issue 546, revised with the maintainer on 2026-11-11, settled what the
dashboard is: one thing in every mode, a read-only view of what Muru is
doing, with control in `muructl` and, for production with a UI, in a
separate package that uses the admin API (issue 549). The question here
is what becomes of the actions, the switch and the sign-in.

## Decision drivers

- Control should have one path, the admin API, which `muructl` and any
  later UI use, so its checks, its events and its audit trail are the
  same however a change is asked for.

- The dashboard is what is left to look at when the application is a
  JSON API, or too broken to render (issue 546), so it is wanted in every
  mode, a container's included (issue 286).

- A view that cannot change anything needs no form protection, and a
  page with no forms cannot be tricked into posting one.

- Whoever may use the admin socket may look, and no one else (ADR 0030).

## Considered options

- Keep the actions behind the existing switch, off by default.

- Remove the actions, the switch and the form handling, and keep the
  sign-in and sessions for looking.

- Remove the dashboard from the daemon and from `muructl serve`, leaving
  it to `muructl run` (issue 548, closed as not planned).

## Decision outcome

Chosen option: remove the actions and keep the sign-in. The dashboard
shows and never changes, under `muructl run`, `muructl serve` and the
daemon alike. It serves GET (and HEAD) alone, so a POST to any path,
`/actions/*` included, is answered 405. The pages have no forms, and
their content security policy says `form-action 'none'`. Its JSON paths
under `/v1/` read and never change. A view added later brings no form
with it: a change belongs in a `muructl` command, and whatever the
dashboard shows is reachable from `muructl` with `--json` as well.

Signing in and sessions stay, permanently, as ADR 0030 set them out:
`muructl dashboard login` gives a single-use link, `--token` a bearer
token for scripts, and every path under the daemon but `/login`,
`/metrics` and `/static/` needs a session. `muructl serve`'s dashboard
uses the same sign-in. A session carries no powers of its own, since
there is only looking.

Removed with the actions: the `Action` enum and the sink that carried an
action out, the per-session form token and the same-origin check, the
`--actions` option of `muructl dashboard login` and the `actions` field
of the DashboardLogin request, the `actions` key of the `[dashboard]`
table and its handling in `muructl daemon reload`, and the
`dashboard.action` event.

The on-request memory count on a worker page stays: it reads and changes
nothing, though it holds the worker's interpreter while it counts.

### Consequences

- Good: one path for control, whose checks (an overlay's freeze, a
  release's state) and events are the admin API's alone.

- Good: the dashboard can be offered in every mode with nothing to
  switch on or off, and a stolen session cookie can only look.

- Good: less code to keep safe: no form tokens, no origin checks, no
  redirects back to a page.

- Bad: whoever used the buttons runs the `muructl` command instead:
  `maintenance on` and `off`, `rollout bluegreen`, `rollback`, `drain`,
  `scale --processes` and `--unpin`, and `restart --worker` with
  `--force`. A daemon file that still sets `actions` is refused as
  holding an unknown key, since the project is in alpha and keeps no
  compatibility path.

## More information

Section 10.7 of the design specification; issues 546 and 547; ADR 0030,
whose actions this record supersedes, while its sessions, its
relative addresses and its login links stand.
