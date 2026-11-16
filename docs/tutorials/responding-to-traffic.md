# Responding to traffic

A condition watches Muru's own figures and says when something holds, such
as crawlers taking most of a site's requests. An overlay is a piece of
configuration laid over the routes and applications while it is active.
Put together, Muru notices something, responds to it, and stops
responding once it is over, without anyone running a command.

In about twenty minutes you will declare a condition and watch it against
real traffic before it acts, give crawlers tighter rate limits while it
fires, see why a response must not take away what its condition reads,
and hold capacity in reserve while the condition lasts.

This tutorial assumes you have done [scaling under
load](scaling-under-load.md).

## A shop with crawlers

```
mkdir crawlers && cd crawlers
uv init --bare
uv venv
```

Save this as `app.py`:

```{literalinclude} code/crawlers/app.py
:language: python
```

and add Muru's settings and a scaling policy to `pyproject.toml`:

```{literalinclude} code/crawlers/pyproject.toml
:language: toml
:start-at: "[tool.muru]"
```

Deploy it:

```
muructl daemon start
muructl deploy shop .
```

```
muru: release shop@r1 is ready and serving
```

Conditions and overlays are declared in files that `muructl apply`
reads, beside the routes they change. Save this as `site.toml`:

```{literalinclude} code/crawlers/site.toml
:language: toml
```

It puts the shop behind a route named `shop`, which an overlay can reach
by name, gives requests from three crawlers' user agents the client class
`ai-crawler`, and declares a rate limit for that class, two requests a
second, which no route uses yet.

```
muructl apply -f site.toml
```

```
muru: applied
  create-client-class client_class ai-crawler  created
  create-rate-limit rate_limit crawlers      created
  create-host     host shop                created
  create-route    route shop /             created
```

Save the load script as `load.py`. Two people's browsers ask for the page
five times a second each for the whole run, and four crawlers do the same
for the first part of it; every five seconds it prints the answers so
far, by status:

```{literalinclude} code/crawlers/load.py
:language: python
```

## Watch a condition before it acts

Save this as `watch.toml`:

```{literalinclude} code/crawlers/watch.toml
:language: toml
```

The condition fires once crawlers have been more than half of the shop's
requests, over a ten second window, for ten seconds, and resolves once
they have been under a fifth of them for ten seconds. Two thresholds with
a gap between them keep it from flapping around one value. `min_requests`
is how many requests the window needs before its share counts as a
figure. The windows are short so you can watch it in a minute; real ones
are minutes long.

```
muructl apply -f watch.toml
muructl condition list
```

```
CONDITION     STATE     SINCE                 WHEN
crawler-surge unknown   -                     request_share{app='shop',client_class='ai-crawler'}[10s] > 0.5
```

With no traffic the share has no figure, so the condition is `unknown`.
Run a minute of load, the crawlers for its first thirty seconds:

```
uv run load.py 60 30
```

and in another terminal, follow the condition every few seconds:

```
muructl condition show crawler-surge
```

```
condition crawler-surge (applied)
  state       pending: when has held for 4s of 10s
  when        request_share{app='shop',client_class='ai-crawler'}[10s] > 0.5
  for         10s
  clear_when  request_share{app='shop',client_class='ai-crawler'}[10s] < 0.2
  clear_for   10s
  min         20 requests
  term        request_share{app='shop',client_class='ai-crawler'}[10s] > 0.5 is 0.6667
  term        request_share{app='shop',client_class='ai-crawler'}[10s] < 0.2 is 0.6667
```

The crawlers are two thirds of the requests, over the threshold, and the
condition is pending while it waits out `for`. Ten seconds later it
fires:

```
  state       firing since 2026-11-02T05:40:12Z
```

When the crawlers stop, their share falls. Once it has been under a fifth
for `clear_for`, the condition resolves:

```
  state       resolved since 2026-11-02T05:40:50Z
```

Each change is an event:

```
muructl events
```

```
     8 05:40:12 warning  condition.firing     crawler-surge            firing, when held for 10s; request_share{app='shop',client_class='ai-crawler'}[10s] > 0.5: 0.6667; ...
     9 05:40:50 info     condition.resolved   crawler-surge            resolved, clear for 10s; request_share{app='shop',client_class='ai-crawler'}[10s] > 0.5: 0; ...
```

Nothing else happened: the crawlers were served throughout. A condition
does nothing by itself, so you can declare one and watch it against real
traffic until its thresholds and waits fire when you would want them to,
before anything acts on it.

## Respond with an overlay

Save this as `respond.toml`:

```{literalinclude} code/crawlers/respond.toml
:language: toml
```

While `crawler-surge` fires, the overlay gives the `ai-crawler` class a
route of its own at the shop's route, the same route with the `crawlers`
rate limit; everyone else's requests carry on as before. `min_active`
keeps it active for at least twenty seconds however soon its condition
resolves, and `cooldown` keeps the condition from activating it again for
a minute after it ends.

```
muructl apply -f respond.toml
muructl overlay list
```

```
OVERLAY                  STATE      TRIGGER                            CHANGES
crawler-response         idle       condition crawler-surge, resolved  route shop
```

Run the load again:

```
uv run load.py 60 30
```

Once the condition fires, the overlay is active, and the crawlers' answers
turn to 429s while the people's stay 200s:

```
muructl overlay list
```

```
OVERLAY                  STATE              TRIGGER                            CHANGES
crawler-response         active (condition) condition crawler-surge, firing    route shop
```

```
  5s  people 50 x 200  crawlers 100 x 200
 10s  people 98 x 200  crawlers 196 x 200
 15s  people 148 x 200  crawlers 232 x 200, 63 x 429
 20s  people 196 x 200  crawlers 242 x 200, 150 x 429
 25s  people 244 x 200  crawlers 252 x 200, 237 x 429
 30s  people 294 x 200  crawlers 262 x 200, 325 x 429
```

A request the rate limit refuses is still one of the shop's, so the
crawlers are still most of its requests and the condition keeps firing
for as long as they keep coming. Once they stop, the condition resolves,
and the overlay ends with it, since it has been active for longer than
`min_active`:

```
OVERLAY                  STATE                               TRIGGER                            CHANGES
crawler-response         cooldown until 2026-11-02T05:43:38Z condition crawler-surge, resolved  route shop
```

The events tell the story:

```
muructl events
```

```
    10 05:42:00 warning  condition.firing     crawler-surge            firing, when held for 10s; ...
    11 05:42:00 notice   overlay.activated    overlay crawler-response activated: its condition crawler-surge is firing; ...
    12 05:42:01 warning  ratelimit.enforcing  rate_limit crawlers      began refusing or holding requests: 15 so far
    13 05:42:38 info     condition.resolved   crawler-surge            resolved, clear for 10s; ...
    14 05:42:38 notice   overlay.ended        overlay crawler-response ended: its condition crawler-surge resolved; ...
```

Run a short burst straight away, inside the cooldown:

```
uv run load.py 50 20
```

The condition fires again, but the overlay stays as it is, and the
crawlers are served:

```
    16 05:43:04 warning  overlay.skipped      overlay crawler-response not activated though its condition crawler-surge is firing: its cooldown after it last ended lasts until 2026-11-02T05:43:38.132Z
```

`min_active`, `cooldown` and `max_per_hour`, four activations an hour by
default, keep a condition that hovers around its threshold from turning
an overlay on and off all day.

## A response that cannot see itself recover

Rather than refusing crawlers, you could answer them from a static
snapshot of the shop, which costs the application nothing. Save this as
`snapshot.toml`, which changes the same overlay:

```{literalinclude} code/crawlers/snapshot.toml
:language: toml
```

Without its last line, applying it warns:

```
muru: warning: overlay crawler-response takes shop's ai-crawler requests away from what condition crawler-surge reads, so the condition sees none of them and may resolve while they keep coming; rate-limit the class instead, or read the class's share of every request, without app
```

A request answered from a static directory is not the application's, so
while the overlay is active the crawlers disappear from the shop's
figures. Their share falls to nothing although they are still coming,
the condition resolves, the overlay ends, and the crawlers are back on
the shop. An overlay must not take away the traffic its own condition
reads. `latch = true` is one way out: the overlay stays active after its
condition resolves, until a person ends it. Wait for the cooldown to pass,
then apply it and run load with the crawlers throughout:

```
muructl apply -f snapshot.toml
uv run load.py 60 60
```

The crawlers' answers stay 200s, from the snapshot. The condition fires,
the overlay activates and the condition resolves about fifteen seconds
later while the crawlers keep coming, but the overlay stays:

```
muructl overlay list
```

```
OVERLAY                  STATE              TRIGGER                                    CHANGES
crawler-response         active (condition) condition crawler-surge, latched, resolved route shop
```

It is for you to decide when the crawlers have gone. `muructl overlay
preview` shows what the overlay changes and its condition's figures, and
`muructl overlay end` ends it:

```
muructl overlay end crawler-response
```

```
muru: overlay crawler-response ended by hand
```

The other way out is a condition on a figure the overlay does not change.
Here, `request_share{client_class='ai-crawler'}` without `app` counts every
request the node takes, the snapshot's among them. A rate limit, as in
the step before, needs neither.

## Capacity on a condition

An overlay can set capacity as well as routes. Put the rate limit back,
and save this as `capacity.toml`:

```{literalinclude} code/crawlers/capacity.toml
:language: toml
```

While the crawlers surge, the shop keeps at least three workers and does
not scale in, so a lull in the crawl does not drain the workers the
people need when it resumes.

```
muructl apply -f respond.toml
muructl apply -f capacity.toml
uv run load.py 60 40
```

Once the condition fires, both overlays are active, and the group rises
to the floor:

```
muructl scale status shop
```

```
shop@r1
GROUP        WORKERS      SLOTS      IDLE    QUEUE-P95     REAPS      POLICY
default      3            1/12       2.75    under 1 ms    0/6        spare 1-4 workers, 0-2 idle
  scheduled: 3-4 workers, no scale-in from crawler-capacity
  workers: cpu 3%, gc 0%, gil wait p95 under 0.1 ms, loop lag p95 -
  diagnosis: within capacity
  last: scaled out to 3: raising toward the floor of 3 that crawler-capacity sets (7s ago)
```

The `scheduled` line names the overlay that sets the floor. The
autoscaler still decides the count every second within it.

A cap goes the other way. While the floor is in force, cap the group by
hand for a minute:

```
muructl scale shop --cap 2 --for 1m
```

```
muru: shop default capped at 2 workers until 2026-11-02T05:51:55Z
muru: shop default is at 2 workers
```

The cap wins over the floor, and the excess worker drains at once:

```
  scheduled: 2-2 workers, no scale-in from crawler-capacity
  capped: at most 2 workers from shop:cap
```

A cap suits a dependency that more workers would only overload, and it
can follow a condition too. End it early:

```
muructl overlay end shop:cap
```

After the crawlers stop and the condition resolves, both overlays end,
and the group scales in by the autoscaler's usual slow rules.

## Clean up

```
muructl daemon stop --shutdown
```

## From here

- [Conditions](../howto/conditions.md) covers every signal a condition
  can read, windows and how unknown figures are treated.

- [Overlays](../howto/overlays.md) covers everything an overlay can change,
  schedules, preconditions, the webhook for monitoring systems, and
  promoting an overlay into the base configuration.

- [Capacity on a condition](../howto/scaling.md#capacity-on-a-condition)
  in the scaling guide has more examples, and [capping a
  group](../howto/scaling.md#capping-a-group) the rules of a cap.

- [Scaling under load](scaling-under-load.md) is how the autoscaler
  decides the count within those bounds.
