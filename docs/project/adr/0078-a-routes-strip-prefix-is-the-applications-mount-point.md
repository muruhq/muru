---
status: accepted
date: 2026-11-25
---

# ADR 0078: A route's strip_prefix is the application's mount point

## Context and problem statement

A route's path selects which requests reach an application or an
upstream, and nothing else (section 4.5, issue 906): an application
gets `SCRIPT_NAME=""` unless a mount gives it a path, and an upstream
gets the whole path. That is the right default, since the most common
sub-path route to an application is one for policy, whose application
expects the whole path. But there was no way to say in the
configuration that part of a route's path is where the application
lives. The only ways were a trusted proxy sending `X-Forwarded-Prefix`,
or a mount of the application's own entrypoint at the sub path, which
ties the release to one URL and must be repeated in every checkout a
detour runs from (issue 907). Nothing worked for an upstream. The
question is how a route says which part of its path is the
application's mount point, and what that part does for each kind of
target (issue 910).

## Decision drivers

- An application sees the part the WSGI way, moved into `SCRIPT_NAME`
  rather than lost; an upstream sees it the gateway way, taken off the
  path and sent as `X-Forwarded-Prefix`.

- A route narrower than the mount point, such as `/sub/path/api` to an
  application whose `/api` is a mount, or a policy route at
  `/sub/path/admin`, must give the same base as the route at
  `/sub/path`.

- The setting says what `SCRIPT_NAME` becomes, and fails loudly when the
  route's path is edited so that it no longer fits.

- No change for a route without it, and none that a second route can
  cause by being added or removed.

- Stripping cannot be tricked by how a client spells the path.

## Considered options

- `strip_prefix`, a path that `muructl apply` checks is a leading part
  of the route's path on segment boundaries.

- A boolean that strips the route's whole path. It cannot express a
  route narrower than the mount point.

- A segment count, `strip_segments = 2`. As precise, but it says
  nothing of what `SCRIPT_NAME` becomes and silently changes meaning
  when the route's path is edited.

- Working the base out from the shortest route to the application on
  the host. Adding or removing a route would change what the others'
  requests see.

- Stripping by default. It would break the policy routes that expect
  the whole path.

## Decision outcome

Chosen option: `strip_prefix`, a path, because it says what it does,
expresses routes narrower than the mount point, and fails loudly when
the route moves.

- It is the route's `path` or a leading part of it whole segments long,
  checked by `muructl apply`; a trailing slash on either makes no
  difference. It can never reach past the route's path.

- The route matches as before, on segment boundaries, so the request's
  first segments are the stripped part's and stripping is a split after
  them, on the path the router matched. An encoded slash, a `..` or a
  doubled slash cannot move the boundary, and the rest keeps the
  client's spelling.

- For an application's release, `SCRIPT_NAME` (ASGI `root_path`) is the
  trusted `X-Forwarded-Prefix`, then the stripped part, then the mount's
  path; ASGI `path` stays the whole path. The release's mounts, worker
  groups and directories of its own are matched against the rest of
  the path, so behind a stripping route a mount's path is relative to
  the stripped part. Without `strip_prefix` the rest is the whole path,
  and mounts behave exactly as before.

- For an upstream, a managed HTTP release and a detour, the stripped
  part is taken off the path sent, `/` when nothing is left since a
  request names a path, and appended to the trusted
  `X-Forwarded-Prefix`, which is sent. A detour's run trusts that
  header, so it sees the `SCRIPT_NAME` of the release it stands in for
  with nothing specific to detours.

- A static route refuses it unless it has a fallback, which takes the
  request with the stripped part; the directory is served relative to
  the route's path already. An internal redirect is routed afresh and
  takes the `strip_prefix` of the route it lands on. Overlays do not
  change it.

### Consequences

- Good: an application or service can be put under a sub path of a
  shared host by configuration alone, and the release does not need to
  know its URL.

- Good: several routes to one application can give it the same base
  whatever they select.

- Bad: how a release's mount paths are read now depends on the route
  that reaches it: from the host's root, or from the stripped part.

- Bad: an upstream or a detour sent the bare stripped part sees the
  path `/`, where an application's release sees `PATH_INFO` `""`.

## More information

Issue 910, and issue 906, which corrected section 4.9's examples with
it; sections 4.5, 4.13, 6.7 and appendix D.
