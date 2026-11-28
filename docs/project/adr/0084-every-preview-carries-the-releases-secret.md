---
status: accepted
date: 2026-11-27
---

# ADR 0084: Every preview carries the release's secret

## Context and problem statement

A release was previewed by its ID alone: `Muru-Preview: r2`, or a
`muru_preview=r2` cookie, sent a request to `r2` whatever the route's
policy said, for any release the edge had for the route, one deployed
with `--hold` among them (section 4.5, issue 928). Release IDs are easy
to guess (`r1`, `r2`, a date), so anyone who could reach the listener
could see a release that was held so that it could be tried before
clients saw it. A wrong guess was answered with `muru-preview: ignored`,
which told the client it had missed, and so which IDs exist.

A detour was not open this way. Since ADR 0075 a detour is previewed
only as `RELEASE.SECRET`, the secret made by the daemon when the detour
opens, and a browser gets the cookie from a one-time entry link under
`/_muru/preview/` that the daemon mints.

The question is what a client must hold to see a release that takes no
public traffic, and what a response may say about a preview that was not
honoured.

## Decision drivers

- Seeing a release that takes no public traffic needs something its
  operator gave out, not only its name.

- A release's ID stays a name. It is in `MURU_RELEASE`, the `release`
  label on metrics, logs and the dashboard, and files name releases in
  `serving` keys and traffic splits, a declared release's ID being
  written by a person.

- A response tells nothing about which releases exist.

- One mechanism for detours and other releases, so that the edge, the
  commands and the documentation describe one thing.

- What is given out can be taken back.

## Considered options

- A preview secret apart from the ID, as detours have, for every
  release.

- A suffix on the release ID, from a hash of the configuration or a
  commit.

- Previews only for a named client class or trusted addresses.

## Decision outcome

Chosen option: "A preview secret apart from the ID, for every release",
because it is the only one that leaves the ID a name and needs nothing
of the network a client comes from. A commit hash is not secret, a short
hash can be searched with the response as the oracle, and a suffix would
have to be spelled in every file that names a release. Limiting previews
to addresses or a class would rule out the commonest use, a person
opening a held release from wherever they are, and is still there for
those who want it, as a client class given the release in a policy.

### Details fixed by the implementation

- **The secret** is 128 random bits in hexadecimal, as a detour's is,
  made when the release is first recorded and kept in the state store,
  in a column of its own that the release's record is not read from. It
  outlasts daemon restarts and applies that leave the release in place.
  A store from before the change gets one for each release when a
  daemon opens it. A detour keeps its secret in memory as before.

- **The value** is `RELEASE.SECRET` for every release, the serving one
  included, split at the last dot. A bare ID is honoured for nothing.
  The secret is compared in constant time, and against a fixed value
  when the route has no such release.

- **One answer for every miss.** A wrong secret, an unknown release, a
  bare ID and any other value are routed as usual with `muru-preview:
  ignored`, and a `muru_preview` cookie that is no release's on the node
  is expired by the response, whichever of them it was. The header is
  kept, since a person whose link has gone stale needs to see why they
  are not looking at the release, and it now says the same thing in
  every case.

- **Upstream previews are dropped.** `upstream:NAME` was a preview
  value. Keeping it would need a secret for each upstream name in the
  store, a target form for the command, and a rule for upstreams that
  several routes name, all for trying an upstream before it has a share,
  which a client class given `upstream:NAME` in the policy's `classes`
  already does, and which the upstream's own address does without Muru.
  The cost is out of proportion to that, so an upstream is not previewed.
  Issue 955's rule, that a preview reaches an upstream only through a
  route whose policy names it, goes with it.

- **The secret is taken off every request** the edge passes on: the
  `Muru-Preview` header, and the `muru_preview` cookie from the Cookie
  headers, for workers, upstreams, managed servers, detours' runs and
  authentication handlers, in every mode. Before, an application saw the
  header, and only a detour's run was kept from seeing its secret. An
  application that read `Muru-Preview` to learn that it was being
  previewed no longer can; `MURU_RELEASE` says which release it is.

- **The secret leaves the daemon** only in the answer to
  `PreviewRelease` and in the snapshot its own edges are given. It is
  not in a release as the API and the commands show it, in events, logs
  or the access log, or in a debug bundle, and `muructl doctor store
  --copy` writes its copy without the column's values, rebuilt so that
  no free page holds one.

- **`/_muru/preview/` is reserved on every daemon's edge**, whether or
  not detours are enabled, since any release may have an entry link. A
  system daemon, which takes no detours, reserves it too. `muructl run`
  and `muructl serve` have one release and reserve nothing.

- **`muructl preview [TARGET]`** replaces `muructl detour preview`, with
  no alias: there are no users to carry, and a hidden command that
  prints a pointer would be one more thing to remove. TARGET is
  `APP@RELEASE`, anything `detour preview` took, or an application, for
  which the daemon chooses the open detour, else the one held release.
  The daemon chooses, so that the command and the MCP tool cannot differ.
  Ambiguity is refused with the candidates.

- **`--valid-for`** makes entry links that last up to an hour, a minute
  by default, still single use. A link sent to somebody else needs
  longer than a minute; an hour bounds how long a forgotten link is
  good for.

- **`--new-secret`** gives a release a new secret, the snapshot is
  pushed and then the command is answered, so nothing given out before
  works afterwards. It works for a detour too, whose secret only the
  daemon holds. It is the event `release.preview_secret_changed`.

- **`muructl deploy --hold --open-preview`** opens a link once the
  release is ready. `muructl apply --hold` has no such flag: an apply
  may hold several releases of several applications, and opening a tab
  for each, or choosing one, would both surprise. Both commands point at
  `muructl preview APP@RELEASE`, as a held release's message does.

- **The daemon's probes**, a rollout's and an overlay's, send the full
  value, read from the store when the probe is sent.

- **The API** call is `PreviewRelease`, in place of `PreviewDetour`, and
  the MCP server has a `preview` tool, with the `detours` tool left to
  list. The tool takes no `new_secret`, so it stays one that reads.

### Consequences

- Good: a held release, and any release a policy gives nothing, is seen
  only by those given a link or the header, and what was given out is
  taken back with one command.

- Good: a response to a preview that missed says nothing of which
  releases exist.

- Good: one preview mechanism, for detours and every other release.

- Bad: `curl -H 'Muru-Preview: r2'` no longer works; the header is
  `"$(muructl preview shop@r2 --header)"`, which needs the admin socket.
  Tests and scripts that sent the bare ID change.

- Bad: the header value is a credential for the release for as long as
  the secret stands, unlike a link, which is used once. `--new-secret`
  is the remedy, and nothing expires it on its own.

- Bad: an upstream cannot be previewed.

- Bad: the state store's schema changes (version 23), and a copy of the
  store is no longer byte for byte what the daemon holds.

## More information

Issue 928, and issue 955 for the sticky cookie and upstream previews.
Sections 4.5, 4.21, 4.22, 9.2, 9.4, 10.12 and 13.1 of the specification,
and appendices A and B. ADR 0075, whose detour preview this extends, and
ADR 0077, whose rule for a previewed request outside a detour's paths
stands.
