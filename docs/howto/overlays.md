# Overlays

An overlay is a named piece of configuration laid over the routes and
applications while it is active. The configuration underneath never
changes: when the overlay ends, the routes are as they were, including
any change made to them in the meantime. One mechanism covers a timed
switch to a new release, a read-only period while a database is backed
up, or a maintenance window.

```toml
[[route]]
name = 'shop-root'
host = 'shop'
path = '/'
app = 'shop'

[overlay.nightly-backup]
routes.shop-root = { mode = 'read-only', deny = ['POST', 'PUT', 'PATCH', 'DELETE'] }
trigger = { schedule = { cron = '0 2 * * *', duration = '1h', timezone = 'Australia/Sydney' } }

[overlay.cutover-r43]
routes.shop-root.traffic = { pin = 'r43' }
trigger = { schedule = { start = '2026-11-02T09:00', timezone = 'Australia/Sydney' } }
warm_up = { start_release = 'r43', lead = '30m' }
preconditions = { release_ready = 'r43' }
```

Overlays are declared beside routes, in the files `muructl apply` reads,
and reach routes by the route's `name`.

## What an overlay changes

| Key | Effect |
| --- | --- |
| `routes.NAME.mode` | The [site mode](client-classes.md#method-restrictions-and-site-modes) the application sees as `muru.mode` |
| `routes.NAME.deny`, `answer` | Methods refused, with 503 `read_only` or, with `answer = 'method_not_allowed'`, 405 |
| `routes.NAME.traffic` | `{ pin = 'r43' }` sends every request to one release of the route's application; `{ split = { r43 = 20 } }` replaces the route's shares |
| `routes.NAME.target` | What serves the route instead: `{ app = 'NAME' }`, `{ static = '/srv/snapshot' }` or `{ upstream = 'NAME' }` |
| `routes.NAME.rate_limits` | The [rate limits](client-classes.md) in place of the route's |
| `routes.NAME.request_headers`, `response_headers` | The [header rules](headers.md), each table in place of the route's |
| `routes.NAME.error_pages` | [Error pages](error-pages.md) by status or `status:reason`, each in place of the route's page for the same key; the route's other pages stay |
| `routes.NAME.client_routes` | Routes for [client classes](client-classes.md), by class: each takes the keys above (except `client_routes`) for that class's requests at the route's host and path |
| `apps.NAME.maintenance` | Maintenance mode on (`true`) or off (`false`) for the application |
| `apps.NAME.mode`, `deny`, `answer` | The site mode and refused methods on each of the application's routes, as `routes.NAME` sets them on one |
| `apps.NAME.min_workers`, `max_workers`, `scale_in`, `group` | [Scheduled capacity](scaling.md#scheduled-capacity) for the application's default worker group, or `group` |
| `apps.NAME.cap_workers` | At most this many workers for the group, whatever its policy and the autoscaler choose (see [capping a group](scaling.md#capping-a-group)) |
| `apps.NAME.freeze_deploys` | Refuse the application's [deploys and rollouts](rollouts.md#deploy-freezes) |

An overlay can choose the page people see while it is active, such as a
notice for a planned upgrade; a page's file must be an absolute path:

```toml
[overlay.db-upgrade]
apps.shop = { maintenance = true }
routes.shop-root.error_pages = { '503:maintenance' = { file = '/srv/shop/upgrade.html' } }
```

An overlay can also give a client class a route of its own while it is
active, such as sending crawlers to a static snapshot when they become a
burden. Where the route has a route for that class alone at the same
path, the overlay changes it; otherwise the class gets a copy of the
route with the overlay's changes, and other requests carry on as before:

```toml
[overlay.crawler-surge]
routes.shop-root.client_routes = { ai-crawler = { target = { static = '/srv/shop/snapshot' } } }
```

An application's mode goes on every route of the application at once,
such as for a backup that must not see writes:

```toml
[overlay.nightly-backup]
apps.shop = { mode = 'read-only', deny = ['POST', 'PUT', 'PATCH', 'DELETE'] }
```

The application's routes take the mode and methods as if each route
named them, in the overlays' order, so a later overlay setting a route's
mode replaces the application's on that route. An application reached
with no route, on a daemon with no host declared, has no mode.

While an overlay pins or splits a route's traffic, or a class's, a
rollout of that route's application waits, and carries on when the
overlay ends.

Two overlays that set the same field cannot both be applied unless each
has a different `priority` (capacity is the exception: the highest floor
and ceiling apply, and the lowest cap); the higher one's value stands while both are active.
Each error page key and each class in `client_routes` is a field of its
own, so overlays choosing pages for different statuses, or routes for
different classes, do not conflict. `muructl apply` refuses overlays
that would conflict, or that name a route, application, upstream,
directory, rate limit or client class that does not exist.

## Activating

By hand:

```
muructl overlay activate nightly-backup
muructl overlay activate nightly-backup --for 2h
muructl overlay activate nightly-backup --until 2026-11-02T06:00 --timezone Australia/Sydney
muructl overlay end nightly-backup
```

By schedule: `trigger.schedule` gives either a `start` and an optional
`end`, for one window, or a `cron` expression and a `duration`, for a
window that recurs. Times are local times in `timezone` (an IANA name
such as `Australia/Sydney`, the daemon's own zone without one); a `start`
or `end` may instead be a time with an offset. Daylight saving is
handled: a local time the clocks skip is read as the time after the
change, and one they repeat as the first of the two. An overlay with a
start and no end stays active until it is ended or promoted. Ending a
scheduled overlay by hand keeps it ended until its window is over, or
until an apply changes its trigger.

A cron expression has five fields, minute, hour, day of month, month and
day of week, each a number, `*`, a range such as `1-5`, a step such as
`*/15` and lists of these, with month and day names (`jan`, `mon`) and
Sunday as 0 or 7. `@hourly`, `@daily`, `@weekly`, `@monthly` and
`@yearly` stand for the usual expressions. When both the day of month
and the day of week are given, a day matching either matches, as in
cron. When recurring windows overlap, the overlay stays active from the
first to the end of the last.

The daemon works out which overlays should be active from the time and
what it has recorded, every second, so one restarted in the middle of a
window carries on with it. Schedules follow the system clock, so while
any are declared the daemon checks every ten minutes that the clock is
synchronised, and raises a `clock.unsynchronised` warning when it is not
(and `clock.synchronised` once it is again).

## By a condition

`trigger.condition` names a [condition](conditions.md), and the overlay
follows it: active while the condition is firing, and ended once it has
resolved.

```toml
[overlay.db-outage]
routes.shop-root = { target = { static = '/srv/shop/snapshot' }, mode = 'degraded' }
trigger = { condition = 'shop-errors' }
latch = true

[overlay.crawler-limits]
routes.shop-root.client_routes.ai-crawler = { rate_limits = ['crawlers'] }
trigger = { condition = 'crawler-surge' }
```

The overlay follows the condition's state, not its events. While the
condition is `unknown` the overlay is held as it is, active or not. After
the daemon starts again the overlay is worked out again from the
condition, and an active one stays active until the condition has a
figure that says otherwise. An overlay names one condition, and has no
schedule beside it; combine signals in the condition's expression. It may
also take `trigger.external = true`, for the webhook.

Three settings on the overlay keep it from flapping:

| Key | Default | Meaning |
| --- | --- | --- |
| `min_active` | `5m` | The least the overlay stays active, however soon its condition resolves |
| `cooldown` | `10m` | How long after it ends its condition cannot activate it again; `0s` for none |
| `max_per_hour` | `4` | The most activations by its condition in an hour |

An activation the cooldown or the cap holds back is an `overlay.skipped`
event saying which, once while the condition keeps firing. So is one its
[preconditions](#preconditions) hold back, which is tried again each
second while the condition fires. With `latch = true` the overlay stays
active after its condition resolves, until a person ends it, which suits
an overlay that answers errors. An overlay activated by hand or by the
webhook is ended by hand or by the webhook, whatever its condition does.
Ending by hand an overlay its condition activated holds it through its
cooldown.

The activation and the end are `overlay.activated` and `overlay.ended`
events, naming the condition, with each of its terms' figures. `muructl
overlay list` shows the condition's state beside the trigger, and
`muructl overlay preview NAME` the condition's terms and their figures
now:

```console
$ muructl overlay preview db-outage
condition shop-errors is firing
  error_rate{app='shop',reason='app_error'} > 0.2 is 0.31
  error_rate{app='shop',reason='app_error'} < 0.02 is 0.31
route shop-root: mode (none) -> degraded
route shop-root: target app shop -> static /srv/shop/snapshot
```

An overlay can take away the traffic its own condition reads: a static
snapshot in place of the application whose error rate fired it leaves
that application with no requests, its error rate unknown, and the
overlay can never end by it. A client class's route to a snapshot does
the same to a condition on that class's share of the application's
requests, since a request answered from a static directory is not the
application's: the share falls while the class keeps coming, and the
overlay ends too soon. A rate limit keeps the requests it refuses in the
application's figures. `muructl apply` warns about such an overlay.
Resolve its condition on another signal, such as `upstream_healthy` for a
database's health check upstream, or set `latch = true`, which apply does
not warn about, and end the overlay by hand once `muructl overlay
preview` shows the application healthy again.

An overlay on a condition can set capacity too, as in [capacity on a
condition](scaling.md#capacity-on-a-condition).

## The webhook

Monitoring and alerting systems can activate and end overlays that
declare `trigger = { external = true }`, beside a schedule or alone,
through a signed webhook. It is served on its own address, given in the
daemon's configuration file:

```toml
[overlay_webhook]
listen = '0.0.0.0:9466'
secret_file = '/etc/muru/overlay-webhook.key'
```

A request is `POST /v1/overlays/NAME:trigger` with a JSON body:

```json
{"action": "activate", "time": "2026-11-02T09:00:00Z", "for": "2h"}
```

`action` is `activate` or `end`; an activation may take `for` (a
duration) or `until` (an RFC 3339 time), and without either lasts until
it is ended. `time` is when the request was made, as an RFC 3339 time,
and must be within five minutes of the daemon's clock, so a captured
request cannot be replayed later. The `X-Muru-Signature` header is
`sha256=` and the hex HMAC-SHA256 of the body with the secret, as the
[event sinks](events.md) sign what they send:

```console
$ body='{"action": "end", "time": "2026-11-02T11:00:00Z"}'
$ signature=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$(cat /etc/muru/overlay-webhook.key)" | sed 's/^.* //')
$ curl -H "X-Muru-Signature: sha256=$signature" -d "$body" http://shop.internal:9466/v1/overlays/db-outage:trigger
{"overlay":"db-outage","message":"ended by the webhook"}
```

The answer is JSON: 200 with what happened; 401 `bad_signature` or
`stale`; 404 for an overlay that does not take the trigger; 409
`refused`, with why, when the overlay is already in that state or its
preconditions do not hold; and 400 for a body that cannot be read. The
webhook carries no secrets, but put it on an address only monitoring
systems reach, or behind a proxy that adds TLS.

## Advisory and latched schedules

With `advisory = true`, the schedule only raises an
`overlay.would_activate` event as each window begins, saying whether the
preconditions held, and the overlay is activated by hand only. It suits
trying out a schedule before letting it act.

With `latch = true`, an overlay its schedule activates stays active after
the window ends, until a person ends it.

## Warm-up

`warm_up` starts a release ahead of each window of the schedule, so a
switch to it lands on warm workers:

```toml
warm_up = { start_release = 'r43', lead = '30m' }
```

For `lead` before the window begins, the release runs, started again if
it was stopped or retained, without taking traffic, and the overlay's
capacity floors already apply; the event `overlay.warming` says so. A
`start_release` is optional. The release is named as for `release_ready`.
If the overlay does not activate, nothing else keeps the release
running, and it is retained as a superseded release is.

## Preconditions

`preconditions.release_ready` names a release that must be ready, or
retained and warm, for the overlay to activate: `app@release`, or just
the release when the overlay changes routes of one application. When it
does not hold, an activation by hand is refused, and a scheduled one is
skipped for the rest of its window with the event `overlay.skipped`, so a
timed switch never moves traffic to something broken.

A skip lasts until the window ends, which for a `start` with no `end` is
until a person acts. Once the release is fixed, either activate the
overlay by hand, or apply a change to it, such as a new `start` or the
release it names: an apply that changes a skipped overlay clears the
skip, so the overlay is judged again at its next window, or at once if
its window has begun. `muructl apply` reports `updated; its skip is
cleared`, and `muructl overlay list` shows a skip with no end as
`skipped (activate it by hand, or apply a change, to try again)`.

`preconditions.probe` is a path that release must answer with a 2xx or
3xx response as well, asked through the edge as a preview of the release,
at the host of the first route the overlay changes:

```toml
preconditions = { release_ready = 'r43', probe = '/healthz' }
```

A probe that fails names the release it asked and the host, in the
refusal of an activation by hand and in `overlay.skipped`:

```console
$ muructl overlay activate cutover-r43
muru: error: overlay cutover-r43 was not activated: its probe of shop@r43 failed: /healthz (as a preview, at host shop) answered 503 Service Unavailable
```

## Previewing and promoting

```
muructl overlay list
muructl overlay preview cutover-r43
muructl overlay promote cutover-r43
```

`list` shows each overlay's state, its trigger and what it changes.
`preview` shows what activating it now would change, field by field.
`muructl config show --effective`, for every application or one, writes
the configuration with the active overlays' changes in the routes,
headed by what each active overlay sets that is in force, and then by
where each release setting came from (see the
[daemon guide](daemon.md#declaring-applications-in-files)):

```console
$ muructl config show shop --effective
# Muru effective state: muructl 0.1.0 on web1, 2026-11-02T02:10:00Z, state version 812.
# The active overlays set, and the routes below include:
#   route shop-root: methods.deny = POST, PUT, PATCH, DELETE (overlay nightly-backup)
#   route shop-root: mode = read-only (overlay nightly-backup)
#   app shop: group default: at least 6 workers (schedule shop.weekday-mornings)
```

`promote` folds an active overlay's changes into the base configuration
and ends it for good, which is how a timed switch becomes the new normal:

- A `pin` makes the pinned release the application's serving release, as
  a deploy would. The release that served before drains and is retained
  for `muructl rollback`, and later deploys replace the pinned release as
  usual. Since the switch moves every route of the application, the
  overlay must pin all of them to the same release, and no route of a
  client class alone; otherwise promote refuses and names the routes it
  leaves out. A pin with a `target` is refused too, and so is a pin while
  a rollout of the application is under way.

- A `split` is not promoted. Shares between releases are a route's
  traffic policy, which belongs in the applied files, or a rollout moves
  the traffic. Promote refuses, showing the `[route.traffic]` table to
  add to the route.

- Everything else the overlay sets on routes (mode, methods, target, rate
  limits, headers, error pages and the routes it adds for client classes)
  is written into the routes, and its maintenance settings into the
  applications.

The applied files still declare the routes as they were, and may name
the serving release with `serving`, so the next `muructl apply` would
undo the promotion. Promote prints what to put in the files to keep it:

```console
$ muructl overlay promote cutover-r43
muru: overlay cutover-r43 promoted: its changes are now the base configuration; shop@r43 serves in place of shop@r42, which is retained
muru: the next apply undoes this unless the applied files say the same: put this in them, and remove overlay cutover-r43

# shop: where the files name its serving release
[app.shop]
serving = "r43"
```

An overlay that changes a route's mode, say, prints the route as it now
is, under `# route shop-root, in place of its table in the files`. Put
the blocks in the files and remove the overlay from them.

While a route's traffic is split, by its own policy or an overlay's,
`muructl release list` shows each release's share of it under the
release, and `muructl scale status` the same for the release it shows:

```console
$ muructl release list shop
RELEASE                  STATE      SERVING  WORKERS               DISK SOURCE
shop@r43                 ready               2 (2x4)             48.0MB /srv/shop/r43
                         traffic: shop-root 100% (overlay cutover-r43)
shop@r42                 ready      yes      2 (2x4)             48.0MB /srv/shop/r42
                         traffic: none, shop-root sends 100% to r43 (overlay cutover-r43)
```

Activations, ends, skips, advisory windows, warm-ups and promotions are
the events `overlay.activated`, `overlay.ended`, `overlay.skipped`,
`overlay.would_activate`, `overlay.warming` and `overlay.promoted`.
