---
status: accepted
date: 2026-11-15
---

# ADR 0061: A condition is a named, PromQL-shaped expression over Muru's own figures

## Context and problem statement

Section 9.10 lists a condition over Muru's own signals as a trigger for
overlays, and other parts of Muru want the same: incidents on a
sustained condition (issue 545) and a rollout waiting for a quiet time
(issue 321). Issue 123 builds the conditions themselves, with a state
and events and nothing acting on them yet. What a condition is written
in, which figures it reads, and how it behaves over time, with too little
traffic and with no data, had to be decided once for every user of it.

## Decision drivers

- Operators already write Prometheus alerting rules; a condition should
  read like one, and move to one.

- Muru keeps no time-series database, only its own per-second figures
  in the daemon's memory.

- One form for every user of a condition, each showing the same state.

- A condition must not fire on a handful of requests, nor act when its
  figures are missing.

## Considered options

- PromQL itself, over a store of Muru's metrics.

- A small language shaped like PromQL, over the daemon's own figures.

- A structured table (`{ metric, above, sustained, clears }`).

- A condition written inline in each overlay's trigger, rather than
  named.

## Decision outcome

**A named condition, `[condition.NAME]`, in the daemon's file or an
applied file**, with `when`, `for`, `clear_when`, `clear_for` and
`min_requests`, which overlays, incidents and later triggers name rather
than write. What acts on a condition owns its own guards (an overlay's
minimum active time, cooldown, cap and latch); the condition owns the
evaluation.

**The language is a PromQL-shaped subset**: a comparison of a signal,
with `=` and `!=` label matchers and a window in brackets, against a
number in base units, joined by `and` (binding tighter) and `or` with
parentheses. No arithmetic, no functions, no regular expressions.
PromQL would be a large thing to build and to document over figures
Muru does not store; the subset reads like the rules operators write,
and maps to one with each signal mapped to its metric. A table cannot
combine terms.

**Signals are named after Muru's metrics**, without the `muru_` prefix
and the Prometheus suffixes, taking the same labels: `request_rate`,
`request_share`, `error_rate`, `latency`, `queue_depth`,
`capacity_utilisation`, `upstream_healthy`, and from the daemon's own
state `scaling_at_max`, `scaling_denied` and `node_memory_available`. A
request signal's `app`, `release` and `group` say which requests are
counted and its other labels pick some out, a share being those picked
out over those counted.

**Windows and evaluation follow Prometheus.** A rate, share, error rate
or quantile is over its window, a minute by default and at most the hour
of figures kept; a window on a gauge averages it, and without one a
gauge is its latest value. The daemon evaluates every second; `when`
must be true at every evaluation through `for` to fire, one false
evaluation resetting the wait, as a pending Prometheus alert is reset,
and `clear_when` through `clear_for` likewise to resolve. `clear_when`
is a whole expression, so what it reads is on the page; without it the
condition resolves when `when` has been false for `clear_for`.

**Too little traffic, and no data, are unknown.** A share, error rate or
latency over fewer than `min_requests` requests (100, as the canary
gates' minimum) is unknown, and so is a signal with no figures.
Expressions take three values, as Kleene's logic. An unknown condition
neither fires nor resolves: it resets its wait and holds its state, as
the Kubernetes HorizontalPodAutoscaler holds its scale when its metrics
are missing.

### Consequences

- Good: one evaluation and one state for every user of a condition,
  defined in one place and watched before anything acts on it.

- Good: a condition reads like a Prometheus rule and needs no store of
  metrics beyond an hour of the terms' own readings.

- Bad: the figures are lost when the daemon restarts, so every condition
  starts unknown again until its windows have figures.

- Bad: only what Muru measures can be read: no `route` label until the
  edge counts by route (issues 690 and 513), no `client_class` on
  `latency` until it keeps durations by client class (issue 695).

## More information

- Specification section 9.10.

- Issue 123, which builds conditions; issue 689, overlays triggered by
  one; issues 545 and 321, later users.

- Prometheus alerting rules, the Kubernetes HorizontalPodAutoscaler's
  stabilisation and KEDA's activation thresholds, compared in issue 123.
