# Conditions

A condition says when something holds over Muru's own figures: an error
rate that has climbed, a share of requests from crawlers, a queue that
will not drain. The daemon evaluates every condition each second, keeps
its state, `firing`, `resolved` or `unknown`, and raises an event as it
changes. A condition does nothing else by itself, so its events and
`muructl condition show` are how you tune its thresholds against real
traffic.

## Declaring a condition

Declare conditions in the daemon's own file or in a file you apply with
`muructl apply`:

```toml
[condition.shop-errors]
when = "error_rate{app='shop',reason='app_error'} > 0.2"
for = "1m"
clear_when = "error_rate{app='shop',reason='app_error'} < 0.02"
clear_for = "5m"

[condition.crawler-surge]
when = "request_share{app='shop',client_class='ai-crawler'} > 0.3"
for = "5m"
clear_when = "request_share{app='shop',client_class='ai-crawler'} < 0.1"
clear_for = "15m"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `when` | | The expression that fires the condition |
| `for` | `0s` | How long `when` must be true at every evaluation before the condition fires |
| `clear_when` | none | The expression that resolves it. Without one, it resolves once `when` has been false for `clear_for` |
| `clear_for` | `0s` | How long the condition must be clear at every evaluation before it resolves |
| `min_requests` | `100` | The requests a share, an error rate or a latency needs in its window to be a figure |

A condition's name is lowercase letters, digits and hyphens. A name the
daemon's file declares stays the file's: an applied condition with the
same name is left out, with a warning in the daemon's log. `muructl
apply` refuses a condition that does not read, naming the condition and
the term, and `muructl config export` writes the applied ones back.

## The expression

An expression compares signals with numbers, and joins comparisons with
`and` and `or`, `and` binding tighter, with parentheses to group them:

```
error_rate{app='shop'} > 0.2 and request_rate{app='shop'}[5m] > 10
(queue_depth{app='shop'} > 50 or latency{app='shop',phase='queue'} > 2) and scaling_at_max{app='shop'} >= 1
```

- A signal takes labels in braces, each `=` or `!=` a quoted value. Only
  the labels the signal takes are allowed.

- A window in brackets, such as `[5m]`, says how much of the past the
  figure is made from, up to `1h`. A rate, a share, an error rate or a
  latency is over the last minute without one. A gauge is its latest
  value without one, and its average over the window with one.

- The comparisons are `>`, `>=`, `<` and `<=`. Numbers are in base units:
  a ratio from 0 to 1 is `0.2`, not `20%`, and a time is in seconds,
  `0.5`, not `500ms`. A threshold outside 0 to 1 for a ratio is refused.

There is no arithmetic, no function and no regular expression.

## Signals

A request signal's labels are of two kinds. `app`, `release` and `group`
say which requests are counted; the others pick some of them out. A
share or an error rate is the requests picked out over all the requests
counted.

| Signal | Labels | Value |
| --- | --- | --- |
| `request_rate` | `app`, `release`, `group`, `status`, `reason`, `client_class` | requests a second over the window |
| `request_share` | `app`, `release`, `group`, `status`, `reason`, `client_class` | the requests picked out over all the requests counted, 0 to 1 |
| `error_rate` | `app`, `release`, `group`, `reason` | 5xx responses, or those with `reason`, over all the requests counted, 0 to 1 |
| `latency` | `app`, `release`, `group`, `quantile` (`'0.99'` by default), `phase` (`'total'` by default, `'queue'` or `'application'`) | seconds; the wait for a worker is `latency{phase='queue'}` |
| `queue_depth` | `app`, `release`, `group` | requests waiting for a worker, summed over the groups counted |
| `capacity_utilisation` | `app`, `release`, `group` | busy worker slots over all, 0 to 1, of the busiest group counted, itself a 30-second average |
| `scaling_at_max` | `app`, `group` | 1 while a group is at its most workers and its signals ask for more, else 0 |
| `scaling_denied` | `app`, `group` | 1 while the node's budget refuses a scale-out a group wants, else 0 |
| `node_memory_available` | none | bytes left under the node's memory limit less its headroom |
| `upstream_healthy` | `upstream` | the share of the upstream's addresses in service, 0 to 1 |

`status` is a status class, such as `'5xx'`, and `reason` the reason the
edge gave a response it generated, as the [metrics](metrics.md) have
them.

## How a condition is evaluated

Each second the daemon works out every term's figure and the condition's
expression:

- A condition fires once `when` has been true at every evaluation for
  `for`. One evaluation that is not resets the wait.

- A firing condition resolves once `clear_when` has been true at every
  evaluation for `clear_for`, or, without `clear_when`, once `when` has
  been false that long. A `clear_when` with a threshold apart from
  `when`'s keeps a condition from flapping around one value, and
  `muructl apply` warns about one that reads the same figure with no gap
  between the two.

- A term is unknown when its signal has no figures, as for an
  application with no traffic yet, an upstream that is gone, or a group
  with no workers, and when a share, an error rate or a latency has fewer
  than `min_requests` requests in its window. `and` with a false term is
  false and `or` with a true term is true; anything else with an unknown
  term is unknown.

- A condition whose evaluation is unknown neither fires nor resolves: it
  holds the state it had, its wait starts again, and it reads `unknown`,
  naming the term. A condition starts `unknown` until it has a figure,
  including after the daemon starts again, since the figures are kept
  only while the daemon runs.

A quiet application lowers `min_requests` on its conditions, so a
handful of requests can be a figure.

## Watching conditions

`muructl condition list` gives each condition's state and since when:

```console
$ muructl condition list
CONDITION     STATE     SINCE                 WHEN
crawler-surge resolved  2026-11-02T05:40:12Z  request_share{app='shop',client_class='ai-crawler'} > 0.3
shop-errors   firing    2026-11-02T05:42:20Z  error_rate{app='shop',reason='app_error'} > 0.2
```

`muructl condition show NAME` gives each term's figure now, or why it has
none:

```console
$ muructl condition show shop-errors
condition shop-errors (applied)
  state       firing since 2026-11-02T05:42:20Z
  when        error_rate{app='shop',reason='app_error'} > 0.2
  for         1m
  clear_when  error_rate{app='shop',reason='app_error'} < 0.02
  clear_for   5m
  min         100 requests
  term        error_rate{app='shop',reason='app_error'} > 0.2 is 0.31
  term        error_rate{app='shop',reason='app_error'} < 0.02 is 0.31
```

Both take `--json`. Each change of state is an event, `condition.firing`
or `condition.resolved`, with the condition's name as its subject and the
figure of each term:

```console
$ muructl events
    51 05:42:20 warning  condition.firing     shop-errors              firing, when held for 1m; error_rate{app='shop',reason='app_error'} > 0.2: 0.31; error_rate{app='shop',reason='app_error'} < 0.02: 0.31
```

The MCP server's `conditions` tool gives the same, and the events reach
the dashboard and event sinks as every event does.
