//! Named conditions (design specification section 9.10, issue 123, ADR
//! 0061): the daemon's evaluation, once a second, of each condition over
//! its own figures, and each condition's state.
//!
//! Each second the evaluator reads what each condition's terms need from
//! the edge's samples and the daemon's own figures, and keeps those
//! readings for the longest window any term has, at most an hour. A term
//! over a window compares the newest reading with the oldest in the
//! window: counters by their difference, gauges by their average. A term
//! with no data, or with too few requests for a share, an error rate or a
//! latency, is unknown, and an expression is true, false or unknown as
//! Kleene's three-valued logic says.
//!
//! A condition fires once `when` has been true at every evaluation for
//! `for`, and resolves once `clear_when`, or `when` being false, has held
//! for `clear_for`; one evaluation that is not resets the wait. While its
//! evaluation is unknown a condition neither fires nor resolves, holds
//! its state, and reads `unknown`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use muru_config::condition::{Comparison, Expr, Kind};
use muru_edge::metrics::Sample;

/// What the daemon knows each second beside the edge's samples.
#[derive(Debug, Clone, Default)]
pub struct Figures {
    /// Each autoscaled group: its application, group, whether it is at its
    /// most workers and wants more, and whether the node's budget refuses
    /// a scale-out it wants.
    pub scaling: Vec<(String, String, bool, bool)>,
    /// Bytes left under the node's memory limit less the headroom; none
    /// without a limit.
    pub node_memory_available: Option<f64>,
}

/// A condition's state as the admin API gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Its evaluation is unknown, or it has not yet had a figure.
    Unknown,
    /// It fired, and has not resolved.
    Firing,
    /// It is not firing.
    Resolved,
}

impl State {
    /// The state's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Firing => "firing",
            Self::Resolved => "resolved",
        }
    }
}

/// A term's figure now.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Its value, in base units.
    Known(f64),
    /// Why it has none.
    Unknown(String),
}

/// A condition as the daemon has it, and its state.
#[derive(Debug, Clone)]
pub struct Status {
    /// Its name.
    pub name: String,
    /// The table it was declared with.
    pub table: muru_config::Condition,
    /// Where it was declared: `file`, the daemon's own, or `applied`.
    pub source: &'static str,
    /// Its state now.
    pub state: State,
    /// The state it holds while its evaluation is unknown.
    pub held: State,
    /// Since when it has been in its held state, Unix milliseconds; 0 for
    /// one that has not had a figure yet.
    pub since_unix_ms: u64,
    /// Why it is unknown, when it is.
    pub unknown: Option<String>,
    /// Each term of `when`, then of `clear_when`, with its figure now.
    pub terms: Vec<(String, Value)>,
}

/// One condition, read and with its state.
struct Tracked {
    table: muru_config::Condition,
    source: &'static str,
    when: Expr,
    clear: Option<Expr>,
    wait: Duration,
    clear_wait: Duration,
    min_requests: u64,
    /// Whether it is firing; none until it has had a figure.
    firing: Option<bool>,
    since_unix_ms: u64,
    /// Since when the expression it waits on has held, Unix seconds.
    pending: Option<u64>,
    unknown: Option<String>,
    terms: Vec<(String, Value)>,
}

/// What a term reads each second, kept for its window.
#[derive(Debug, Clone, PartialEq)]
enum Reading {
    /// Requests counted, in all: those in scope, and those the term picks
    /// out of them. None in scope means no data.
    Counters { scope: f64, picked: f64 },
    /// The duration histogram's cumulative buckets, summed over the series
    /// in scope, and its count.
    Buckets {
        buckets: Vec<(f64, f64)>,
        count: f64,
    },
    /// A gauge's value, none with no data.
    Gauge(Option<f64>),
    /// No series in scope.
    Nothing,
}

/// The daemon's conditions and the figures they read.
#[derive(Default)]
pub struct Engine {
    conditions: BTreeMap<String, Tracked>,
    /// Each second's readings, by the term's selector, the newest last.
    history: VecDeque<(u64, HashMap<String, Reading>)>,
}

impl Engine {
    /// Take the conditions declared now, each with where it came from. One
    /// whose table is unchanged keeps its state; one that cannot be read is
    /// left out and named in the errors given back.
    pub fn set(
        &mut self,
        declared: Vec<(String, muru_config::Condition, &'static str)>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let mut kept = BTreeMap::new();

        for (name, table, source) in declared {
            if let Some(mut tracked) = self.conditions.remove(&name)
                && tracked.table == table
            {
                tracked.source = source;
                kept.insert(name, tracked);
                continue;
            }

            match compile(&name, table, source) {
                Ok(tracked) => {
                    kept.insert(name, tracked);
                }
                Err(error) => errors.push(error),
            }
        }

        self.conditions = kept;
        errors
    }

    /// Whether any condition is declared.
    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }

    /// Evaluate every condition at `now_unix_ms` over this second's
    /// samples and figures. Gives the events their changes raise: each a
    /// kind, the condition's name as the subject, a level and a message.
    pub fn observe(
        &mut self,
        now_unix_ms: u64,
        samples: &[Sample],
        figures: &Figures,
    ) -> Vec<(&'static str, String, &'static str, String)> {
        let now = now_unix_ms / 1000;

        // This second's readings, of every term any condition has.

        let mut readings = HashMap::new();
        let mut longest = Duration::ZERO;

        for tracked in self.conditions.values() {
            for term in all_terms(tracked) {
                longest = longest.max(term.effective_window().unwrap_or_default());
                readings
                    .entry(term.selector())
                    .or_insert_with(|| read(term, samples, figures));
            }
        }

        self.history.push_back((now, readings));
        let oldest = now.saturating_sub(longest.as_secs() + 1);

        while self.history.front().is_some_and(|(at, _)| *at < oldest) {
            self.history.pop_front();
        }

        let mut events = Vec::new();

        for (name, tracked) in &mut self.conditions {
            if let Some(event) = step(name, tracked, &self.history, now_unix_ms) {
                events.push(event);
            }
        }

        events
    }

    /// Every condition and its state, by name.
    pub fn statuses(&self) -> Vec<Status> {
        self.conditions
            .iter()
            .map(|(name, tracked)| {
                let held = match tracked.firing {
                    Some(true) => State::Firing,
                    Some(false) => State::Resolved,
                    None => State::Unknown,
                };

                Status {
                    name: name.clone(),
                    table: tracked.table.clone(),
                    source: tracked.source,
                    state: if tracked.unknown.is_some() {
                        State::Unknown
                    } else {
                        held
                    },
                    held,
                    since_unix_ms: tracked.since_unix_ms,
                    unknown: tracked.unknown.clone(),
                    terms: tracked.terms.clone(),
                }
            })
            .collect()
    }
}

/// A condition's table, read.
fn compile(
    name: &str,
    table: muru_config::Condition,
    source: &'static str,
) -> Result<Tracked, String> {
    muru_config::condition::check(name, &table)?;

    let when = muru_config::condition::parse(&table.when)
        .map_err(|error| format!("condition {name}: when: {error}"))?;
    let clear = table
        .clear_when
        .as_deref()
        .map(muru_config::condition::parse)
        .transpose()
        .map_err(|error| format!("condition {name}: clear_when: {error}"))?;
    let duration = |text: &Option<String>| {
        text.as_deref()
            .map(muru_config::parse_duration)
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let wait = duration(&table.for_).map_err(|error| format!("condition {name}: for: {error}"))?;
    let clear_wait = duration(&table.clear_for)
        .map_err(|error| format!("condition {name}: clear_for: {error}"))?;

    Ok(Tracked {
        min_requests: table
            .min_requests
            .unwrap_or(muru_config::condition::DEFAULT_MIN_REQUESTS),
        table,
        source,
        when,
        clear,
        wait,
        clear_wait,
        firing: None,
        since_unix_ms: 0,
        pending: None,
        unknown: None,
        terms: Vec::new(),
    })
}

/// The terms of `when`, then of `clear_when`.
fn all_terms(tracked: &Tracked) -> Vec<&Comparison> {
    let mut terms = tracked.when.terms();

    if let Some(clear) = &tracked.clear {
        terms.extend(clear.terms());
    }

    terms
}

/// Whether a sample's labels pass every matcher of `term` for the labels
/// in `labels`.
fn matches(sample: &Sample, term: &Comparison, labels: &[&str]) -> bool {
    term.matchers
        .iter()
        .filter(|matcher| labels.contains(&matcher.label.as_str()))
        .all(|matcher| {
            let value = sample
                .labels
                .iter()
                .find(|(label, _)| *label == matcher.label)
                .map_or("", |(_, value)| value.as_str());
            (value == matcher.value) == matcher.equal
        })
}

/// What `term` reads this second.
fn read(term: &Comparison, samples: &[Sample], figures: &Figures) -> Reading {
    use muru_config::condition::SCOPE_LABELS;

    let name = term.signal.name;

    match term.signal.kind {
        Kind::Rate | Kind::Share => {
            let in_scope: Vec<&Sample> = samples
                .iter()
                .filter(|sample| sample.name == "muru_requests_total")
                .filter(|sample| matches(sample, term, SCOPE_LABELS))
                .collect();

            if in_scope.is_empty() {
                return Reading::Nothing;
            }

            // An error rate picks out the 5xx responses, or those with the
            // reason it names; the other request signals pick out what
            // their filter labels say.

            let error_rate = name == "error_rate";
            let reason = term.label("reason").is_some();
            let picked = in_scope
                .iter()
                .filter(|sample| matches(sample, term, &["status", "reason", "client_class"]))
                .filter(|sample| {
                    !error_rate
                        || reason
                        || sample
                            .labels
                            .iter()
                            .any(|(label, value)| label == "status" && value == "5xx")
                })
                .map(|sample| sample.value)
                .sum();

            Reading::Counters {
                scope: in_scope.iter().map(|sample| sample.value).sum(),
                picked,
            }
        }
        Kind::Latency => {
            let phase = term.label("phase").unwrap_or("total");
            let in_scope = |sample: &&Sample, suffix: &str| {
                sample.name == format!("muru_request_duration_seconds{suffix}")
                    && matches(sample, term, SCOPE_LABELS)
                    && sample
                        .labels
                        .iter()
                        .any(|(label, value)| label == "phase" && value == phase)
            };

            let mut buckets: BTreeMap<u64, (f64, f64)> = BTreeMap::new();

            for sample in samples.iter().filter(|sample| in_scope(sample, "_bucket")) {
                let le = sample
                    .labels
                    .iter()
                    .find(|(label, _)| label == "le")
                    .and_then(|(_, value)| {
                        if value == "+Inf" {
                            Some(f64::INFINITY)
                        } else {
                            value.parse::<f64>().ok()
                        }
                    });

                if let Some(le) = le {
                    buckets.entry(le.to_bits()).or_insert((le, 0.0)).1 += sample.value;
                }
            }

            if buckets.is_empty() {
                return Reading::Nothing;
            }

            let mut buckets: Vec<(f64, f64)> = buckets.into_values().collect();
            buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
            let count = samples
                .iter()
                .filter(|sample| in_scope(sample, "_count"))
                .map(|sample| sample.value)
                .sum();

            Reading::Buckets { buckets, count }
        }
        Kind::Gauge => Reading::Gauge(gauge(term, samples, figures)),
    }
}

/// A gauge's value this second, none with no data.
fn gauge(term: &Comparison, samples: &[Sample], figures: &Figures) -> Option<f64> {
    let labels = term.signal.labels;
    let series = |metric: &str| -> Vec<f64> {
        samples
            .iter()
            .filter(|sample| sample.name == metric && matches(sample, term, labels))
            .map(|sample| sample.value)
            .collect()
    };

    match term.signal.name {
        "queue_depth" => {
            let values = series("muru_queue_depth");
            (!values.is_empty()).then(|| values.iter().sum())
        }

        // The busiest of the groups in scope.
        "capacity_utilisation" => series("muru_capacity_utilisation")
            .into_iter()
            .reduce(f64::max),

        // The share of the upstream's addresses in service.
        "upstream_healthy" => {
            let values = series("muru_upstream_health");
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
        }
        "scaling_at_max" | "scaling_denied" => {
            let at_max = term.signal.name == "scaling_at_max";
            let groups: Vec<bool> = figures
                .scaling
                .iter()
                .filter(|(app, group, _, _)| {
                    term.matchers.iter().all(|matcher| {
                        let value = if matcher.label == "app" { app } else { group };
                        (*value == matcher.value) == matcher.equal
                    })
                })
                .map(|(_, _, max, denied)| if at_max { *max } else { *denied })
                .collect();

            (!groups.is_empty()).then(|| f64::from(u8::from(groups.iter().any(|on| *on))))
        }
        "node_memory_available" => figures.node_memory_available,
        _ => None,
    }
}

/// A term's figure now, from the readings kept.
fn value(
    term: &Comparison,
    history: &VecDeque<(u64, HashMap<String, Reading>)>,
    min_requests: u64,
) -> Value {
    let key = term.selector();
    let Some((now, latest)) = history
        .back()
        .and_then(|(at, readings)| readings.get(&key).map(|reading| (*at, reading)))
    else {
        return Value::Unknown("no data".to_owned());
    };

    let window = term.effective_window();
    let start = window.map_or(now, |window| now.saturating_sub(window.as_secs()));

    // The readings in the window, the oldest first.

    let within: Vec<(u64, &Reading)> = history
        .iter()
        .filter(|(at, _)| *at >= start)
        .filter_map(|(at, readings)| readings.get(&key).map(|reading| (*at, reading)))
        .collect();

    let too_few = |requests: f64| {
        Value::Unknown(format!(
            "{} requests in its window, fewer than min_requests {min_requests}",
            requests.round()
        ))
    };

    match latest {
        Reading::Nothing => Value::Unknown("no data".to_owned()),
        Reading::Gauge(None) => Value::Unknown("no data".to_owned()),
        Reading::Gauge(Some(latest)) => {
            if window.is_none() {
                return Value::Known(*latest);
            }

            let values: Vec<f64> = within
                .iter()
                .filter_map(|(_, reading)| match reading {
                    Reading::Gauge(value) => *value,
                    _ => None,
                })
                .collect();
            Value::Known(values.iter().sum::<f64>() / values.len().max(1) as f64)
        }
        Reading::Counters { scope, picked } => {
            let Some((
                then,
                Reading::Counters {
                    scope: was,
                    picked: had,
                },
            )) = within
                .iter()
                .find(|(_, reading)| matches!(reading, Reading::Counters { .. }))
            else {
                return Value::Unknown("no data".to_owned());
            };

            if *then >= now {
                return Value::Unknown("not enough history yet".to_owned());
            }

            // Counters that went back were reset, as by a new edge: what
            // they count now is all since then.

            let delta = |now: f64, was: f64| if now >= was { now - was } else { now };
            let scope = delta(*scope, *was);
            let picked = delta(*picked, *had);

            if term.signal.kind == Kind::Rate {
                return Value::Known(picked / (now - then) as f64);
            }

            if scope < min_requests as f64 || scope <= 0.0 {
                return too_few(scope);
            }

            Value::Known(picked / scope)
        }
        Reading::Buckets { buckets, count } => {
            let Some((
                then,
                Reading::Buckets {
                    buckets: were,
                    count: was,
                },
            )) = within
                .iter()
                .find(|(_, reading)| matches!(reading, Reading::Buckets { .. }))
            else {
                return Value::Unknown("no data".to_owned());
            };

            if *then >= now {
                return Value::Unknown("not enough history yet".to_owned());
            }

            let reset = count < was;
            let requests = if reset { *count } else { count - was };

            if requests < min_requests as f64 || requests <= 0.0 {
                return too_few(requests);
            }

            let deltas: Vec<(f64, f64)> = buckets
                .iter()
                .map(|(le, now)| {
                    let before = if reset {
                        0.0
                    } else {
                        were.iter()
                            .find(|(bound, _)| bound == le)
                            .map_or(0.0, |(_, count)| *count)
                    };
                    (*le, (now - before).max(0.0))
                })
                .collect();
            let quantile = term
                .label("quantile")
                .and_then(|q| q.parse().ok())
                .unwrap_or(0.99);

            Value::Known(muru_dashboard::history::quantile(&deltas, quantile))
        }
    }
}

/// True, false or unknown, with the first unknown term's reason.
fn evaluate(
    expr: &Expr,
    history: &VecDeque<(u64, HashMap<String, Reading>)>,
    min_requests: u64,
) -> Result<bool, String> {
    match expr {
        Expr::Term(term) => match value(term, history, min_requests) {
            Value::Known(value) => Ok(term.op.holds(value, term.threshold)),
            Value::Unknown(why) => Err(format!("{}: {why}", term.selector())),
        },
        Expr::And(left, right) => {
            match (
                evaluate(left, history, min_requests),
                evaluate(right, history, min_requests),
            ) {
                (Ok(false), _) | (_, Ok(false)) => Ok(false),
                (Err(why), _) | (_, Err(why)) => Err(why),
                _ => Ok(true),
            }
        }
        Expr::Or(left, right) => {
            match (
                evaluate(left, history, min_requests),
                evaluate(right, history, min_requests),
            ) {
                (Ok(true), _) | (_, Ok(true)) => Ok(true),
                (Err(why), _) | (_, Err(why)) => Err(why),
                _ => Ok(false),
            }
        }
    }
}

/// One second of a condition: its terms' figures, the wait it is in, and
/// a change of state, given back as an event.
fn step(
    name: &str,
    tracked: &mut Tracked,
    history: &VecDeque<(u64, HashMap<String, Reading>)>,
    now_unix_ms: u64,
) -> Option<(&'static str, String, &'static str, String)> {
    let now = now_unix_ms / 1000;
    let min = tracked.min_requests;

    tracked.terms = all_terms(tracked)
        .into_iter()
        .map(|term| (term.to_string(), value(term, history, min)))
        .collect();

    let firing = tracked.firing == Some(true);

    // Firing, it waits on clear_when, or on when being false; otherwise on
    // when.

    let holds = if firing {
        match &tracked.clear {
            Some(clear) => evaluate(clear, history, min),
            None => evaluate(&tracked.when, history, min).map(|holds| !holds),
        }
    } else {
        evaluate(&tracked.when, history, min)
    };

    let holds = match holds {
        Ok(holds) => {
            tracked.unknown = None;
            holds
        }
        Err(why) => {
            tracked.unknown = Some(why);
            tracked.pending = None;
            return None;
        }
    };

    if tracked.firing.is_none() && !holds {
        tracked.firing = Some(false);
        tracked.since_unix_ms = now_unix_ms;
    }

    if !holds {
        tracked.pending = None;
        return None;
    }

    let since = *tracked.pending.get_or_insert(now);

    let wait = if firing {
        tracked.clear_wait
    } else {
        tracked.wait
    };

    if now.saturating_sub(since) < wait.as_secs() {
        return None;
    }

    tracked.pending = None;
    tracked.firing = Some(!firing);
    tracked.since_unix_ms = now_unix_ms;

    let figures: Vec<String> = tracked
        .terms
        .iter()
        .map(|(term, value)| match value {
            Value::Known(value) => format!("{term}: {}", round(*value)),
            Value::Unknown(why) => format!("{term}: unknown, {why}"),
        })
        .collect();
    let held = |wait: Duration| {
        if wait.is_zero() {
            String::new()
        } else {
            format!(" for {}", muru_config::format_duration(wait))
        }
    };

    Some(if firing {
        (
            "condition.resolved",
            name.to_owned(),
            "info",
            format!("resolved, clear{}; {}", held(wait), figures.join("; ")),
        )
    } else {
        (
            "condition.firing",
            name.to_owned(),
            "warning",
            format!("firing, when held{}; {}", held(wait), figures.join("; ")),
        )
    })
}

/// A figure as an event gives it: four significant digits are plenty.
fn round(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }

    let scale = 10f64.powi(3 - value.abs().log10().floor() as i32);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests(app: &str, status: &str, reason: &str, count: f64) -> Sample {
        Sample::new(
            "muru_requests_total",
            &[
                ("app", app),
                ("release", "r1"),
                ("group", "default"),
                ("mount", ""),
                ("status", status),
                ("reason", reason),
                ("client_class", ""),
            ],
            count,
        )
    }

    fn engine(table: &str) -> Engine {
        let table: muru_config::Condition = toml::from_str(table).unwrap();
        let mut engine = Engine::default();
        assert!(
            engine
                .set(vec![("shop-errors".to_owned(), table, "applied")])
                .is_empty()
        );
        engine
    }

    /// Feed the engine `seconds` seconds from `start`, the counters
    /// growing by `ok` and `errors` requests a second from `base`; gives
    /// the events and the counters reached.
    fn feed(
        engine: &mut Engine,
        start: u64,
        seconds: u64,
        base: (f64, f64),
        ok: f64,
        errors: f64,
    ) -> (Vec<String>, (f64, f64)) {
        let mut events = Vec::new();
        let (mut good, mut bad) = base;

        for second in start..start + seconds {
            good += ok;
            bad += errors;
            let samples = [
                requests("shop", "2xx", "", good),
                requests("shop", "5xx", "app_error", bad),
            ];

            for (kind, _, _, message) in
                engine.observe(second * 1000, &samples, &Figures::default())
            {
                events.push(format!("{second} {kind} {message}"));
            }
        }

        (events, (good, bad))
    }

    fn state(engine: &Engine) -> State {
        engine.statuses()[0].state
    }

    #[test]
    fn a_condition_fires_once_when_has_held_for_its_wait_and_resolves_on_clear_when() {
        let mut engine = engine(
            "when = \"error_rate{app='shop'}[10s] > 0.2\"\nfor = '5s'\n\
             clear_when = \"error_rate{app='shop'}[10s] < 0.05\"\nclear_for = '3s'\nmin_requests = 10",
        );

        // Healthy traffic: resolved once there is a figure.

        let (events, counters) = feed(&mut engine, 1000, 15, (0.0, 0.0), 10.0, 0.0);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(state(&engine), State::Resolved);

        // Half the requests fail. The window's share passes 0.2 after a
        // few seconds, and the condition fires 5 s after that.

        let (events, counters) = feed(&mut engine, 1015, 15, counters, 5.0, 5.0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].contains("condition.firing firing, when held for 5s"),
            "{events:?}"
        );
        assert!(
            events[0].contains("error_rate{app='shop'}[10s] > 0.2: 0.5"),
            "{events:?}"
        );
        assert_eq!(state(&engine), State::Firing);

        // Errors stop; between 0.05 and 0.2 it still fires, and once below
        // 0.05 for 3 s it resolves.

        let (events, _) = feed(&mut engine, 1030, 20, counters, 10.0, 0.0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].contains("condition.resolved resolved, clear for 3s"),
            "{events:?}"
        );
        assert_eq!(state(&engine), State::Resolved);
    }

    #[test]
    fn one_evaluation_that_does_not_hold_resets_the_wait() {
        let mut engine = engine("when = \"request_rate{app='shop'}[1s] > 5\"\nfor = '3s'");
        let (events, counters) = feed(&mut engine, 0, 3, (0.0, 0.0), 10.0, 0.0);
        assert!(events.is_empty());

        // A quiet second, then three busy ones: only the third fires it.

        let (events, counters) = feed(&mut engine, 3, 1, counters, 0.0, 0.0);
        assert!(events.is_empty());
        let (events, _) = feed(&mut engine, 4, 4, counters, 10.0, 0.0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].starts_with("7 condition.firing"), "{events:?}");
    }

    #[test]
    fn too_few_requests_or_no_data_leave_a_condition_unknown_holding_its_state() {
        let mut engine = engine("when = \"error_rate{app='shop'} > 0.2\"");

        // No series for the application at all.

        engine.observe(
            1_000,
            &[requests("blog", "2xx", "", 5.0)],
            &Figures::default(),
        );
        let status = &engine.statuses()[0];
        assert_eq!(
            (status.state, status.held),
            (State::Unknown, State::Unknown)
        );
        assert!(
            status.unknown.as_deref().unwrap().contains("no data"),
            "{status:?}"
        );

        // Traffic below min_requests, all failing, does not fire it.

        let (events, _) = feed(&mut engine, 2, 30, (0.0, 0.0), 1.0, 1.0);
        assert!(events.is_empty(), "{events:?}");
        let status = &engine.statuses()[0];
        assert_eq!(status.state, State::Unknown);
        assert!(
            status
                .unknown
                .as_deref()
                .unwrap()
                .contains("fewer than min_requests 100"),
            "{status:?}"
        );
    }

    #[test]
    fn and_with_a_false_term_is_false_and_or_with_a_true_term_true_whatever_is_unknown() {
        let history: VecDeque<(u64, HashMap<String, Reading>)> = VecDeque::from([(
            10,
            HashMap::from([
                ("queue_depth".to_owned(), Reading::Gauge(Some(5.0))),
                ("node_memory_available".to_owned(), Reading::Gauge(None)),
            ]),
        )]);
        let holds =
            |text: &str| evaluate(&muru_config::condition::parse(text).unwrap(), &history, 100);

        assert_eq!(
            holds("queue_depth < 1 and node_memory_available < 1"),
            Ok(false)
        );
        assert_eq!(
            holds("queue_depth > 1 or node_memory_available < 1"),
            Ok(true)
        );
        assert!(holds("queue_depth > 1 and node_memory_available < 1").is_err());
        assert!(holds("queue_depth < 1 or node_memory_available < 1").is_err());
    }

    #[test]
    fn a_latency_is_the_quantile_of_the_requests_in_its_window() {
        let durations = |count: f64, slow: f64| {
            let labels = |le: &str| {
                vec![
                    ("app".to_owned(), "shop".to_owned()),
                    ("release".to_owned(), "r1".to_owned()),
                    ("group".to_owned(), "default".to_owned()),
                    ("phase".to_owned(), "total".to_owned()),
                    ("le".to_owned(), le.to_owned()),
                ]
            };
            let sample = |name: &str, labels: Vec<(String, String)>, value: f64| Sample {
                name: name.to_owned(),
                labels,
                value,
            };
            vec![
                sample(
                    "muru_request_duration_seconds_bucket",
                    labels("0.1"),
                    count - slow,
                ),
                sample("muru_request_duration_seconds_bucket", labels("1"), count),
                sample(
                    "muru_request_duration_seconds_bucket",
                    labels("+Inf"),
                    count,
                ),
                sample(
                    "muru_request_duration_seconds_count",
                    labels("")
                        .into_iter()
                        .filter(|(label, _)| label != "le")
                        .collect(),
                    count,
                ),
            ]
        };

        let table: muru_config::Condition = toml::from_str(
            "when = \"latency{app='shop',quantile='0.5'}[5s] > 0.5\"\nmin_requests = 10",
        )
        .unwrap();
        let mut engine = Engine::default();
        engine.set(vec![("slow".to_owned(), table, "file")]);

        // Before the window, fast requests; within it, all slow ones.

        engine.observe(1_000, &durations(100.0, 0.0), &Figures::default());
        let events = engine.observe(2_000, &durations(200.0, 100.0), &Figures::default());
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].3.contains(": 0.55"), "{events:?}");
    }
}
