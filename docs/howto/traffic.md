# Traffic between releases

A route sends every request to its application's serving release unless
it has a `traffic` table. With one, the route shares its requests between
the releases the table names and the serving release, keeps a client on
the release it first got when told to, and lets a preview reach a release
that takes no share. This is the mechanism canaries, A/B splits and dark
launches are built on.

## Splitting requests

```toml
[[route]]
host = "shop"
app = "shop"

[route.traffic]
split = { r2 = 10 }
```

`split` gives each release a percentage of the route's requests, by
release id. The application's serving release takes whatever is left, so
this route sends one request in ten to `r2` and the rest to the serving
release. The percentages are whole numbers and may add up to at most 100.
Applying a policy that names a release which does not exist is refused.

Clients are not picked at random request by request. Each client is
hashed to one of 10,000 buckets, and each release in `split` owns a
contiguous range of them, the newest release's at the top. A client that
lands in `r2` at 10% is still in `r2` when the share grows to 25%, and
the same client gets the same release in every edge process and across
edge upgrades.

A release named in `split` is kept running. If it was retained after a
deploy it comes back to ready; if it was stopped it is started. When a
policy stops naming a release that is not the serving one, the release
drains and is retained as it would be after a deploy. A deploy or rollback
that would drain a release some policy still names leaves it running.

`muructl release list` shows each release's share of a route that splits
its traffic on a `traffic:` line below the release, such as `traffic:
shop / 10%` for `r2` here and `traffic: shop / 90%` for the serving
release (a route with a `name` goes by it), and `muructl scale status` the same for the release it shows.

A share can go to an [HTTP upstream](upstreams.md) instead of a release,
named `upstream:NAME` in `split`. Its buckets sit below the releases'
ranges, and the sticky cookie names it the same way, reaching it only
through a route whose policy names it.

## The key

What a client is hashed by is the `hash_key`:

| `hash_key` | The key |
| --- | --- |
| `cookie` (the default) | An anonymous id the edge issues as the `muru_id` cookie on a client's first request, kept for a year |
| `header:NAME` | The value of that request header, for example a user id set by a trusted proxy or a gateway |
| `cookie:NAME` | The value of that cookie, for example a session cookie the application sets |
| `client` | The client's address |

A header or cookie key that a request does not carry falls back to the
client's address.

A `salt` is mixed into the hash, so two routes with the same key spread
the same clients differently. Without one the route's host and path are
the salt, so two routes of one application make independent splits
unless they are given the same salt.

## Sticky clients

```toml
[route.traffic]
split = { r2 = 25 }
sticky = true
sticky_grace = "10m"
```

With `sticky = true` the edge sets a `muru_release` cookie naming the
release a client was given, and honours it on later requests as long as
that release serves the route: it is the serving release, or one the
policy names in `split` or `classes`. The cookie is ignored once the
release stops, and a cookie naming anything else is ignored too, a
release deployed with `--hold` and an upstream the policy does not name
among them.

When a sticky release leaves the route, because the policy no longer
names it or a deploy supersedes it, it stays reachable through the cookie
for `sticky_grace` (10 minutes by default) and then drains. New clients
stop landing on it at once. Its state shows as draining during the grace,
with a message saying so.

## Previews

A release with a share of `0` takes no ordinary traffic but can still be
previewed, which is how a release is checked before it gets a share:

```toml
[route.traffic]
split = { r2 = 0 }
```

`muructl preview` opens a release in the browser:

```console
$ muructl preview shop@r2
muru: opened https://shop.example.com/ in preview of shop@r2
```

It opens a one-time entry link. Following the link gives the browser
the `muru_preview` cookie and sends it on to the application's address,
and from then on that browser's requests go to `r2` whatever the policy
says, while every other client is routed as before. The cookie lasts
until the browser closes or you leave preview, at `/_muru/preview/leave`
on any of the application's hosts.

```console
$ muructl preview shop@r2 --print             # print a link, open nothing
$ muructl preview shop@r2 --host admin.example.com
$ muructl preview shop@r2 --list              # a link for each of the application's addresses
$ muructl preview shop@r2 --name alice.shop.example.com
$ muructl preview shop@r2 --valid-for 30m --print
$ muructl preview shop@r2 --header
Muru-Preview: r2.3f9c0e7a51b2d4c68e1f0a9b7c5d3e21
```

A link works once, within a minute, or within the time `--valid-for`
gives, up to an hour, which is for a link sent to somebody else. The
links are for the addresses `muructl open` gives the application, and
only for names that reach this machine unless `--any-name` keeps the
others; `--name` builds the link from a name of your choosing, which
must reach a host that routes the application.

`--header` prints the `Muru-Preview` header, which previews the release
without a cookie, for curl or a test client:

```console
$ curl -H "$(muructl preview shop@r2 --header)" https://shop.example.com/
```

The target is a release as `APP@RELEASE`. An application alone means its
open [detour](detours.md), or else its one release deployed with
`--hold`, and with no target the application is the project's, as its
`pyproject.toml` or `muru.toml` names it. When that leaves more than one
release, or none, the command lists the releases to choose from.

### The preview secret

A preview is the release's ID and its preview secret together, as
`r2.3f9c0e7a51b2d4c68e1f0a9b7c5d3e21` above, in the header or the
cookie. The daemon makes the secret, 128 random bits, when the release
is first recorded, and keeps it for as long as the release exists,
across restarts and applies. Knowing the release's ID is not enough to
see it: `Muru-Preview: r2` previews nothing.

A preview that is not honoured, because the secret is wrong or missing
or the route has no such release, is answered in one way whatever was
wrong with it: the request is routed as usual, the response carries the
header `muru-preview: ignored`, and a `muru_preview` cookie that is no
release's is removed by the response.

The secret is shown only by `muructl preview`. It is not in `muructl
release list` or `release show`, in events, logs or the access log, in a
[debug bundle](debugging.md) or in a copy of the state store. The edge
takes the `Muru-Preview` header and the `muru_preview` cookie off every
request before it reaches the application or an upstream, so the
application never sees them.

Treat a `--header` value, and a link until it is used, as you would a
password for the release. `--new-secret` gives the release a new secret,
after which every link, cookie and header given out before previews
nothing:

```console
$ muructl preview shop@r2 --new-secret --print
muru: shop@r2 has a new preview secret: links, cookies and headers given out before no longer preview it
https://shop.example.com/_muru/preview/8c1f5e0a9b7d4c63f2e1a0b9c8d7e6f5
```

### What can be previewed

Only releases the edge routes can be previewed: the serving release, a
release named in the route's policy and a release deployed with `--hold`,
which is the way to try one before it takes traffic. The serving release
can be previewed too, which is useful while a split sends most clients
elsewhere. A retained release keeps its workers for a rollback and takes
no requests, so it cannot be previewed, and `muructl preview` says so.
An upstream cannot be previewed: give it a client class of your own in
the policy's `classes` to try it before it has a share (see
[Cohorts](#cohorts)).

The daemon's edge answers paths under `/_muru/preview/` itself, on every
host: an entry link, and `leave`. An application's own route under that
path is not reached.

## Cohorts

A cohort is a set of clients that gets a release of its own whatever
the split says: testers with a cookie, say, or users a proxy marks with
a header. Define the cohort as a [client class](client-classes.md),
which matches headers and cookies by regular expression, and give the
class a release in the policy's `classes`:

```toml
[[client_class]]
name = 'beta'
cookies = { plan = '^beta$' }

[[route]]
host = 'shop'
app = 'shop'
traffic = { classes = { beta = 'r2' } }
```

A request with `plan=beta` goes to `r2`, ahead of a sticky cookie and
the split, and everyone else is routed as before. For an experiment
that gives a share of all clients a release, use `split` with a `salt`
of its own, so the experiment's clients are not the same ones another
route's split picks.

## Routes on different releases

Each route has a policy of its own, so routes of one application can be
on different releases. A route whose split gives one release all of its
requests stays on that release while the others follow the serving one:

```toml
[[route]]
host = 'shop'
app = 'shop'

[[route]]
host = 'shop'
path = '/legacy'
app = 'shop'
traffic = { split = { r1 = 100 } }
```

Deploys move `/` to each new release, and `/legacy` stays on `r1`,
which is kept running for it.

## Seeing the policy

`muructl route list` shows each route's shares, stickiness and key:

```text
HOST             PATH                     TARGET
shop             /                        shop (r2 10%, sticky, by header:x-user)
```

Applying a changed policy records a `traffic_policy.changed` event naming
the route and the new policy. A release that a policy names but that
cannot start records `traffic_policy.release_unavailable`.

The response to every request names the release that served it in the
access log, and an application can read its own release id from
`muru.context().release`.

A canary rollout (see [rollouts](rollouts.md)) applies a growing share of
the application's requests to a candidate release on top of whatever
policy a route declares, judged by gates at each step.
