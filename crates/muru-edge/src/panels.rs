//! Panels: a request record's data drawn as HTML (design specification
//! section 11.7, ADR 0063). A panel is data in the record, made of spans
//! on the request's clock, values and tables. The edge draws each panel
//! from a minijinja template when it is asked for, and passes the output
//! through the ammonia sanitiser, whose allowlist the built-in panels
//! (Timing, Where, Response and Connection, embedded here) meet as any
//! other panel must. No panel carries script.

use std::collections::{HashMap, HashSet};

use minijinja::{Environment, context};
use muru_proto::admin::v1 as admin;

use crate::access::{format_bytes, format_duration};

/// The classes panels use, for the toolbar and the dashboard alike.
pub const STYLESHEET: &str = include_str!("../panels/panels.css");

/// The built-in panels' templates, by panel name.
const TEMPLATES: &[(&str, &str)] = &[
    ("timing", include_str!("../panels/timing.html")),
    ("where", include_str!("../panels/where.html")),
    ("response", include_str!("../panels/response.html")),
    ("logs", include_str!("../panels/logs.html")),
    ("errors", include_str!("../panels/errors.html")),
    ("connection", include_str!("../panels/connection.html")),
];

/// The elements a panel may use: text, tables, lists, code, details,
/// meters, progress bars, times and abbreviations.
const TAGS: &[&str] = &[
    "p", "div", "span", "br", "strong", "em", "b", "i", "small", "sub", "sup", "h3", "h4", "a",
    "table", "caption", "thead", "tbody", "tfoot", "tr", "th", "td", "colgroup", "col", "ul", "ol",
    "li", "dl", "dt", "dd", "code", "pre", "details", "summary", "meter", "progress", "time",
    "abbr",
];

/// The attributes elements keep besides `class`, where they take them.
const ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "title"]),
    ("abbr", &["title"]),
    ("span", &["title"]),
    ("td", &["title"]),
    ("th", &["title"]),
    ("time", &["datetime", "title"]),
    ("meter", &["value", "min", "max", "title"]),
    ("progress", &["value", "max", "title"]),
];

/// The prefix the edge answers in a development run (section 4.20),
/// which a panel's links may name.
pub const RESERVED_PREFIX: &str = "/_muru/";

/// Draws panels from their templates and sanitises what they produce.
pub struct Renderer {
    templates: Environment<'static>,
    sanitiser: ammonia::Builder<'static>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Renderer {
    /// A renderer whose panels may link to `/_muru/` paths and, when it
    /// is named, under the dashboard's address.
    pub fn new(dashboard: Option<&str>) -> Self {
        let mut templates = Environment::new();

        for (name, source) in TEMPLATES {
            templates
                .add_template_owned(format!("{name}.html"), *source)
                .expect("a built-in panel template parses");
        }

        let mut sanitiser = ammonia::Builder::empty();
        sanitiser
            .tags(TAGS.iter().copied().collect())
            .generic_attributes(HashSet::from(["class"]))
            .tag_attributes(
                ATTRIBUTES
                    .iter()
                    .map(|(tag, attributes)| (*tag, attributes.iter().copied().collect()))
                    .collect::<HashMap<_, _>>(),
            )
            .url_schemes(HashSet::from(["http", "https"]))
            .url_relative(ammonia::UrlRelative::PassThrough)
            .link_rel(Some("noopener"))
            .strip_comments(true);

        // Section 11.7: links only to the reserved prefix or the
        // dashboard.

        let dashboard = dashboard.map(str::to_owned);
        sanitiser.attribute_filter(move |element, attribute, value| {
            if element == "a" && attribute == "href" {
                let allowed = value.starts_with(RESERVED_PREFIX)
                    || dashboard
                        .as_deref()
                        .is_some_and(|dashboard| value.starts_with(dashboard));
                return allowed.then(|| value.into());
            }

            Some(value.into())
        });

        Self {
            templates,
            sanitiser,
        }
    }

    /// Draw a panel, sanitised. A panel without a template, or whose
    /// template fails, is drawn as a sentence saying so.
    pub fn render(&self, panel: &admin::Panel) -> String {
        let drawn = self
            .templates
            .get_template(&format!("{}.html", panel.name))
            .and_then(|template| {
                template.render(context! { panel => minijinja::value::Serde(panel) })
            });

        let html = match drawn {
            Ok(html) => html,
            Err(error) => format!(
                "<p class=\"error\">The {} panel could not be drawn: {}</p>",
                escape(&panel.name),
                escape(&error.to_string())
            ),
        };

        self.sanitise(&html)
    }

    /// HTML as the sanitiser leaves it.
    pub fn sanitise(&self, html: &str) -> String {
        self.sanitiser.clean(html).to_string()
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The built-in panels of a record, in the order they are shown.
pub fn builtin(record: &admin::RequestRecord) -> Vec<admin::Panel> {
    vec![
        timing(record),
        place(record),
        response(record),
        logs(record),
        errors(record),
        connection(record),
    ]
}

fn value(name: &str, value: impl Into<String>) -> admin::PanelValue {
    admin::PanelValue {
        name: name.to_owned(),
        value: value.into(),
    }
}

fn millis(ms: f64) -> String {
    format_duration(std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0))
}

/// The request's phases on its clock: waiting for a worker, the
/// application until the response head, and sending the response.
fn timing(record: &admin::RequestRecord) -> admin::Panel {
    let span = |name: &str, start_ms: f64, duration_ms: f64, category: &str| admin::PanelSpan {
        name: name.to_owned(),
        start_ms,
        duration_ms: duration_ms.max(0.0),
        category: category.to_owned(),
        children: Vec::new(),
    };

    let mut spans = Vec::new();
    let mut values = Vec::new();

    // The access log's record gives the request's own times, once the
    // response has ended.

    if record.status != 0 {
        let mut request = span("request", 0.0, record.total_ms, "request");

        // A worker or an upstream answered: its time, and the queue's.

        if record.app_ms > 0.0 || record.queue_ms > 0.0 {
            let answered = record.queue_ms + record.app_ms;
            request.children = vec![
                span("waiting for a worker", 0.0, record.queue_ms, "edge"),
                span(
                    "until the response head",
                    record.queue_ms,
                    record.app_ms,
                    "application",
                ),
                span(
                    "sending the response",
                    answered,
                    record.total_ms - answered,
                    "edge",
                ),
            ];
        }

        // The worker's phases, on its clock, which starts as the queue
        // ends.

        if let Some(worker) = &record.from_worker {
            let at = |ms: f64| record.queue_ms + ms;
            let mut phases = Vec::new();

            if worker.to_start_response > 0.0 {
                phases.push(span(
                    "until the response starts",
                    at(0.0),
                    worker.to_start_response,
                    "worker",
                ));
            }

            if worker.to_first_byte > 0.0 {
                phases.push(span(
                    "producing the body",
                    at(worker.to_first_byte),
                    worker.to_last_byte - worker.to_first_byte,
                    "worker",
                ));
            }

            // Finishing follows the response; an application that failed
            // before starting one has only its whole time.

            if worker.to_close > 0.0
                && (worker.to_start_response > 0.0 || worker.to_first_byte > 0.0)
            {
                let from = worker.to_last_byte.max(worker.to_start_response);
                phases.push(span(
                    "finishing",
                    at(from),
                    worker.to_close - from,
                    "worker",
                ));
            }

            // What held the request up, where it can be placed: the wait
            // for a thread, the first wait for the GIL after it, and the
            // longest the event loop was held.

            if worker.thread_wait > 0.0 {
                phases.push(span(
                    "waiting for a thread",
                    at(0.0),
                    worker.thread_wait,
                    "contention",
                ));
            }

            if worker.gil_wait_first > 0.0 {
                phases.push(span(
                    "waiting for the GIL",
                    at(worker.thread_wait),
                    worker.gil_wait_first,
                    "contention",
                ));
            }

            if worker.loop_lag > 0.0 {
                phases.push(span(
                    "event loop held",
                    at(worker.loop_lag_at),
                    worker.loop_lag,
                    "contention",
                ));
            }

            // A wait goes before the phase that starts with it.

            phases.sort_by(|one, other| {
                one.start_ms.total_cmp(&other.start_ms).then_with(|| {
                    (one.category != "contention").cmp(&(other.category != "contention"))
                })
            });
            let mut whole = span("in the worker", at(0.0), worker.to_close, "worker");
            whole.children = phases;

            // It goes beside the edge's view of the same time, before the
            // sending that follows it.

            let at = request.children.len().min(2);
            request.children.insert(at, whole);
        }

        spans.push(request);
    }

    match record.worker_part.as_str() {
        crate::recorder::WORKER_EXPECTED => {
            values.push(value("worker's part", "still to come"));
        }
        crate::recorder::WORKER_MISSING => values.push(value(
            "worker's part",
            "did not arrive: the worker stopped, or its connection failed",
        )),
        _ => {}
    }

    admin::Panel {
        name: "timing".to_owned(),
        title: "Timing".to_owned(),
        spans,
        values,
        tables: record
            .from_worker
            .as_ref()
            .map(during)
            .filter(|table| !table.rows.is_empty())
            .into_iter()
            .collect(),
    }
}

/// `count` of `noun`, in the plural unless it is one.
fn counted(count: u32, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// What held a request up while the worker had it, as a table of what
/// and how long, with what the timeline cannot place: the GIL waits in
/// all and the garbage collections. Empty when nothing did.
fn during(worker: &admin::WorkerPart) -> admin::PanelTable {
    let mut rows = Vec::new();
    let mut row = |what: &str, took: String| {
        rows.push(admin::PanelRow {
            cells: vec![what.to_owned(), took],
        });
    };

    if worker.thread_wait > 0.0 {
        row("waiting for a thread", millis(worker.thread_wait));
    }

    if worker.gil_waits > 0 {
        row(
            "waiting for the GIL",
            format!(
                "{} in {}",
                millis(worker.gil_wait),
                counted(worker.gil_waits, "wait")
            ),
        );
    }

    if !worker.gc.is_empty() {
        let collections: u32 = worker
            .gc
            .iter()
            .map(|generation| generation.collections)
            .sum();
        let ms: f64 = worker.gc.iter().map(|generation| generation.ms).sum();
        let generations: Vec<String> = worker
            .gc
            .iter()
            .map(|generation| {
                let name = if generation.generation == "full" {
                    "the whole heap".to_owned()
                } else {
                    format!("generation {}", generation.generation)
                };
                format!(
                    "{} of {name} in {}",
                    generation.collections,
                    millis(generation.ms)
                )
            })
            .collect();
        row(
            "garbage collection",
            format!(
                "{} in {}: {}",
                millis(ms),
                counted(collections, "collection"),
                generations.join(", ")
            ),
        );
    }

    if worker.loop_lag > 0.0 {
        row(
            "event loop held",
            format!(
                "{} at the longest, from {} on the worker's clock",
                millis(worker.loop_lag),
                millis(worker.loop_lag_at)
            ),
        );
    }

    admin::PanelTable {
        caption: "During the request".to_owned(),
        columns: vec!["What".to_owned(), "Took".to_owned()],
        rows,
    }
}

/// The request's log records and warnings, as the worker kept them.
fn logs(record: &admin::RequestRecord) -> admin::Panel {
    let mut values = Vec::new();
    let mut rows = Vec::new();

    if let Some(worker) = &record.from_worker {
        rows = worker
            .logs
            .iter()
            .map(|line| admin::PanelRow {
                cells: vec![
                    millis(record.queue_ms + line.at_ms),
                    line.level.clone(),
                    line.logger.clone(),
                    line.message.clone(),
                ],
            })
            .collect();

        if worker.logs_dropped > 0 {
            values.push(value(
                "not kept",
                format!(
                    "{} more records were not kept, past the limit",
                    worker.logs_dropped
                ),
            ));
        }
    }

    admin::Panel {
        name: "logs".to_owned(),
        title: "Logs".to_owned(),
        values,
        tables: vec![admin::PanelTable {
            caption: "Log records".to_owned(),
            columns: ["At", "Level", "Logger", "Message"]
                .map(str::to_owned)
                .to_vec(),
            rows,
        }],
        ..Default::default()
    }
}

/// The application's unhandled exception, or the edge's reason for an
/// error it answered itself.
fn errors(record: &admin::RequestRecord) -> admin::Panel {
    let mut values = Vec::new();

    if let Some(worker) = &record.from_worker
        && !worker.outcome.is_empty()
        && worker.outcome != "completed"
    {
        values.push(value("outcome", worker.outcome.replace('_', " ")));

        if !worker.reason.is_empty() {
            values.push(value("reason", worker.reason.clone()));
        }

        if !worker.error_phase.is_empty() {
            values.push(value("raised in", worker.error_phase.clone()));
        }

        if !worker.error.is_empty() {
            values.push(value("error", worker.error.clone()));
        }

        if !worker.traceback.is_empty() {
            values.push(value("traceback", worker.traceback.trim_end().to_owned()));
        }
    }

    if values.is_empty() && !record.reason.is_empty() && record.status >= 400 {
        values.push(value("edge's reason", record.reason.clone()));
    }

    admin::Panel {
        name: "errors".to_owned(),
        title: "Errors".to_owned(),
        values,
        ..Default::default()
    }
}

/// Where routing sent the request.
fn place(record: &admin::RequestRecord) -> admin::Panel {
    let or_dash = |text: &str| {
        if text.is_empty() {
            "-".to_owned()
        } else {
            text.to_owned()
        }
    };

    let application = if record.app.is_empty() {
        "-".to_owned()
    } else if record.release.is_empty() {
        record.app.clone()
    } else {
        format!("{}@{}", record.app, record.release)
    };

    let mut values = vec![
        value("application", application),
        value("host", or_dash(&record.host)),
    ];

    if !record.virtual_host.is_empty() {
        values.push(value("virtual host", record.virtual_host.clone()));
    }

    values.push(value("mount", or_dash(&record.mount)));
    values.push(value("group", or_dash(&record.group)));
    values.push(value(
        "worker",
        record
            .worker
            .map_or_else(|| "-".to_owned(), |worker| worker.to_string()),
    ));
    if let Some(worker) = record
        .from_worker
        .as_ref()
        .filter(|worker| !worker.thread.is_empty())
    {
        values.push(value("thread", worker.thread.clone()));
    }

    values.push(value("client", or_dash(&record.client)));
    values.push(value("client class", or_dash(&record.client_class)));
    values.push(value("listener", or_dash(&record.listener)));

    if !record.redirects.is_empty() {
        values.push(value("redirected to", record.redirects.join(", ")));
    }

    if !record.handoff.is_empty() {
        values.push(value("handed to", record.handoff.clone()));
    }

    values.push(value(
        "sent again",
        if record.retried { "yes" } else { "no" },
    ));
    values.push(value("routing snapshot", record.snapshot.to_string()));

    admin::Panel {
        name: "where".to_owned(),
        title: "Where".to_owned(),
        values,
        ..Default::default()
    }
}

/// What was sent, whether it was compressed, and both sets of headers.
fn response(record: &admin::RequestRecord) -> admin::Panel {
    let status = if record.in_progress && record.status == 0 {
        "in progress".to_owned()
    } else if record.reason.is_empty() {
        record.status.to_string()
    } else {
        format!("{} {}", record.status, record.reason)
    };

    let compressed = match &record.compression {
        Some(compression) if compression.compressed => compression.encoding.clone(),
        Some(compression) if compression.skipped == "encoded" => {
            format!(
                "no: {} ({})",
                crate::compression::describe_skipped(&compression.skipped),
                compression.encoding
            )
        }
        Some(compression) => format!(
            "no: {}",
            crate::compression::describe_skipped(&compression.skipped)
        ),
        None => "-".to_owned(),
    };

    let headers = |caption: &str, headers: &[admin::RecordedHeader]| admin::PanelTable {
        caption: caption.to_owned(),
        columns: vec!["Name".to_owned(), "Value".to_owned()],
        rows: headers
            .iter()
            .map(|header| admin::PanelRow {
                cells: vec![header.name.clone(), header.value.clone()],
            })
            .collect(),
    };

    admin::Panel {
        name: "response".to_owned(),
        title: "Response".to_owned(),
        values: [
            vec![
                value("status", status),
                value("sent", format_bytes(record.bytes)),
                value("received", format_bytes(record.received)),
                value("compressed", compressed),
            ],
            body(record),
        ]
        .concat(),
        tables: vec![
            headers("Request headers", &record.request_headers),
            headers("Response headers", &record.response_headers),
        ],
        ..Default::default()
    }
}

/// What the application produced, from the worker's part: its bytes
/// before compression, where they came from, and whether the edge sent a
/// file itself.
fn body(record: &admin::RequestRecord) -> Vec<admin::PanelValue> {
    let Some(worker) = record
        .from_worker
        .as_ref()
        .filter(|worker| !worker.body_source.is_empty())
    else {
        return Vec::new();
    };

    let mut produced = format!(
        "{} from {}",
        format_bytes(worker.body_bytes),
        worker.body_source.replace('_', " ")
    );

    if worker.sent_by_edge {
        produced.push_str(", the file sent by the edge");
    }

    vec![value("produced", produced)]
}

/// The connection the request arrived on.
fn connection(record: &admin::RequestRecord) -> admin::Panel {
    let mut values = Vec::new();

    if let Some(connection) = &record.connection {
        values.push(value("id", connection.id.clone()));
        values.push(value("position", connection.position.to_string()));
        values.push(value("idle before", millis(connection.idle_ms)));

        if let Some(handshake) = connection.tls_handshake_ms {
            values.push(value("TLS handshake", millis(handshake)));
        }

        values.push(value(
            "closed after",
            if connection.closed_after {
                "yes: it had served its share of requests"
            } else {
                "no"
            },
        ));
    }

    admin::Panel {
        name: "connection".to_owned(),
        title: "Connection".to_owned(),
        values,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as a development run keeps it, for the templates.
    fn fixture() -> admin::RequestRecord {
        admin::RequestRecord {
            request_id: "01JCZ8X4R3T5V6W7Y8Z9A0B1C2".to_owned(),
            client: "127.0.0.1".to_owned(),
            app: "shop".to_owned(),
            release: "r3".to_owned(),
            host: "localhost".to_owned(),
            group: "default".to_owned(),
            mount: "default".to_owned(),
            method: "GET".to_owned(),
            target: "/orders/42?<b>".to_owned(),
            version: "HTTP/1.1".to_owned(),
            listener: "127.0.0.1:8080".to_owned(),
            status: 200,
            bytes: 1234,
            worker: Some(0),
            queue_ms: 0.2,
            app_ms: 17.9,
            total_ms: 18.4,
            redirects: vec!["/internal/orders".to_owned()],
            client_class: "default".to_owned(),
            connection: Some(admin::RequestConnection {
                id: "01JCZ8X4R3T5V6W7Y8Z9A0B1C0".to_owned(),
                position: 1,
                idle_ms: 1.5,
                tls_handshake_ms: Some(4.2),
                closed_after: true,
            }),
            request_headers: vec![
                admin::RecordedHeader {
                    name: "accept".to_owned(),
                    value: "text/html".to_owned(),
                },
                admin::RecordedHeader {
                    name: "cookie".to_owned(),
                    value: "***".to_owned(),
                },
            ],
            response_headers: vec![admin::RecordedHeader {
                name: "content-type".to_owned(),
                value: "<script>alert(1)</script>".to_owned(),
            }],
            compression: Some(admin::RequestCompression {
                compressed: false,
                encoding: String::new(),
                skipped: "small".to_owned(),
            }),
            worker_part: crate::recorder::WORKER_ARRIVED.to_owned(),
            from_worker: Some(admin::WorkerPart {
                thread: "muru-wsgi-1".to_owned(),
                to_start_response: 2.0,
                to_first_byte: 15.0,
                to_last_byte: 17.0,
                to_close: 17.5,
                body_bytes: 1234,
                body_source: "iterable".to_owned(),
                outcome: "failed".to_owned(),
                reason: "app_error".to_owned(),
                error_phase: "iteration".to_owned(),
                error: "the application raised an exception".to_owned(),
                traceback: "Traceback (most recent call last):\n  ...\nKeyError: '<id>'\n"
                    .to_owned(),
                logs: vec![admin::RequestLog {
                    at_ms: 3.0,
                    level: "WARNING".to_owned(),
                    logger: "shop.orders".to_owned(),
                    message: "low stock for <b>42</b>".to_owned(),
                }],
                logs_dropped: 2,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn drawn(name: &str) -> String {
        let renderer = Renderer::new(Some("http://127.0.0.1:8081/"));
        let panel = builtin(&fixture())
            .into_iter()
            .find(|panel| panel.name == name)
            .unwrap();
        let html = renderer.render(&panel);
        assert!(!html.contains("could not be drawn"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        html
    }

    #[test]
    fn the_timing_panel_draws_the_phases_as_spans() {
        let html = drawn("timing");
        assert!(
            html.contains("<td class=\"depth-1\">request</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-2\">waiting for a worker</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-2\">until the response head</td>"),
            "{html}"
        );
        assert!(html.contains("17.9 ms"), "{html}");
        assert!(html.contains("What the phases are"), "{html}");

        // Each bar starts where its phase does, in whole percent of the
        // request: the request's own across all of it, the application's
        // from 0.2 ms of 18.4 for 17.9 ms.

        assert!(html.contains("class=\"bar from-0 for-100\""), "{html}");
        assert!(html.contains("class=\"bar from-1 for-97\""), "{html}");
    }

    #[test]
    fn the_timing_panel_places_the_workers_phases_after_the_queue() {
        let html = drawn("timing");
        assert!(
            html.contains("<td class=\"depth-2\">in the worker</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-3\">until the response starts</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-3\">producing the body</td>"),
            "{html}"
        );

        // The body starts 15 ms into the worker's clock, after 0.2 ms of
        // queue.

        assert!(html.contains("15.2 ms"), "{html}");
        assert!(html.contains("class=\"bar from-83 for-11\""), "{html}");

        let mut missing = fixture();
        missing.from_worker = None;
        missing.worker_part = crate::recorder::WORKER_MISSING.to_owned();
        let html = Renderer::new(None).render(&timing(&missing));
        assert!(html.contains("The worker's part did not arrive"), "{html}");
    }

    #[test]
    fn the_timing_panel_marks_what_held_the_request_up() {
        let mut record = fixture();
        let worker = record.from_worker.as_mut().unwrap();
        worker.thread_wait = 0.5;
        worker.gil_wait = 0.4;
        worker.gil_waits = 3;
        worker.gil_wait_first = 0.3;
        worker.gc = vec![admin::RequestGc {
            generation: "2".to_owned(),
            collections: 1,
            ms: 1.5,
        }];
        worker.loop_lag = 9.0;
        worker.loop_lag_at = 4.0;
        let html = Renderer::new(None).render(&timing(&record));

        // The waits that can be placed are spans in the worker's, in the
        // order they began.

        let thread = html.find(">waiting for a thread</td>").unwrap();
        let gil = html.find(">waiting for the GIL</td>").unwrap();
        let held = html.find(">event loop held</td>").unwrap();
        let body = html.find(">producing the body</td>").unwrap();
        assert!(thread < gil && gil < held && held < body, "{html}");
        assert!(html.contains("<tr class=\"contention\">"), "{html}");

        // The table has all of it.

        assert!(
            html.contains("<caption>During the request</caption>"),
            "{html}"
        );
        assert!(html.contains("0.4ms in 3 waits"), "{html}");
        assert!(
            html.contains("1.5ms in 1 collection: 1 of generation 2 in 1.5ms"),
            "{html}"
        );
        assert!(html.contains("9.0ms at the longest, from 4.0ms"), "{html}");

        // Nothing held up, no table.

        assert!(timing(&fixture()).tables.is_empty());
    }

    #[test]
    fn the_logs_panel_lists_the_records_escaped_and_counts_the_rest() {
        let html = drawn("logs");
        assert!(
            html.contains("<span class=\"warning\">WARNING</span>"),
            "{html}"
        );
        assert!(
            html.contains("low stock for &lt;b&gt;42&lt;/b&gt;"),
            "{html}"
        );
        assert!(html.contains("3.2ms"), "{html}");
        assert!(html.contains("2 more records were not kept"), "{html}");

        let mut quiet = fixture();
        quiet.from_worker = None;
        let html = Renderer::new(None).render(&logs(&quiet));
        assert!(html.contains("No log records or warnings"), "{html}");
    }

    #[test]
    fn the_errors_panel_gives_the_exception_and_its_traceback() {
        let html = drawn("errors");
        assert!(
            html.contains("<th>raised in</th><td>iteration</td>"),
            "{html}"
        );
        assert!(html.contains("<pre class=\"error\">Traceback"), "{html}");
        assert!(html.contains("KeyError: '&lt;id&gt;'"), "{html}");

        let mut fine = fixture();
        fine.from_worker = None;
        let html = Renderer::new(None).render(&errors(&fine));
        assert!(html.contains("No errors."), "{html}");
    }

    #[test]
    fn the_where_panel_names_the_release_and_the_redirects() {
        let html = drawn("where");
        assert!(html.contains("muru-wsgi-1"), "{html}");
        assert!(
            html.contains("<th>application</th><td class=\"mono\">shop@r3</td>"),
            "{html}"
        );
        assert!(html.contains("/internal/orders"), "{html}");
        assert!(
            html.contains("<th>worker</th><td class=\"mono\">0</td>"),
            "{html}"
        );
    }

    #[test]
    fn the_response_panel_escapes_and_masks_the_headers() {
        let html = drawn("response");
        assert!(html.contains("no: the body is under min_size"), "{html}");
        assert!(html.contains("1234B from iterable"), "{html}");
        assert!(
            html.contains("<summary>Request headers <span class=\"badge\">2</span></summary>"),
            "{html}"
        );
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"muted\" title=\"masked\">***</td>"),
            "{html}"
        );
    }

    #[test]
    fn the_connection_panel_gives_the_handshake_of_the_first_request() {
        let html = drawn("connection");
        assert!(html.contains("4.2ms"), "{html}");
        assert!(html.contains("its share of requests"), "{html}");
    }

    #[test]
    fn the_sanitiser_keeps_the_allowlist_and_drops_the_rest() {
        let renderer = Renderer::new(Some("http://127.0.0.1:8081/"));
        let html = renderer.sanitise(
            "<table onclick=\"x()\" class=\"values\"><tr><td style=\"color:red\">a</td></tr></table>\
             <script>alert(1)</script><img src=\"x\"><abbr title=\"t\">ab</abbr>\
             <meter value=\"1\" min=\"0\" max=\"2\"></meter><iframe></iframe>\
             <a href=\"/_muru/requests/A1.json\">ok</a><a href=\"https://example.com/\">no</a>\
             <a href=\"http://127.0.0.1:8081/request/A1\">dashboard</a>\
             <a href=\"javascript:alert(1)\">bad</a>",
        );

        assert!(html.contains("<table class=\"values\">"), "{html}");
        assert!(html.contains("<td>a</td>"), "{html}");
        assert!(!html.contains("script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("iframe"), "{html}");
        assert!(html.contains("<abbr title=\"t\">ab</abbr>"), "{html}");
        assert!(
            html.contains("<meter value=\"1\" min=\"0\" max=\"2\">"),
            "{html}"
        );
        assert!(html.contains("href=\"/_muru/requests/A1.json\""), "{html}");
        assert!(
            html.contains("href=\"http://127.0.0.1:8081/request/A1\""),
            "{html}"
        );
        assert!(!html.contains("example.com"), "{html}");
        assert!(!html.contains("javascript"), "{html}");
    }

    #[test]
    fn a_panel_with_no_template_says_so() {
        let renderer = Renderer::new(None);
        let html = renderer.render(&admin::Panel {
            name: "<nothing>".to_owned(),
            ..Default::default()
        });
        assert!(
            html.contains("The &lt;nothing&gt; panel could not be drawn"),
            "{html}"
        );
    }
}
