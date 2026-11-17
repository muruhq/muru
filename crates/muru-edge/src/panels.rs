//! Panels: a request record's data drawn as HTML (design specification
//! section 11.7, ADR 0063). A panel is data in the record, made of spans
//! on the request's clock, values and tables. The edge draws each panel
//! from a minijinja template when it is asked for, and passes the output
//! through the ammonia sanitiser, whose allowlist the built-in panels
//! (Timing, Where, Response and Connection, embedded here) meet as any
//! other panel must. A text panel's text is drawn here too, markdown
//! included (issue 680). No panel carries script.

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
    ("hints", include_str!("../panels/hints.html")),
    (GENERIC, include_str!("../panels/panel.html")),
    (SPANS, include_str!("../panels/spans.html")),
];

/// The spans table the Timing and generic templates import (issue 737),
/// under a name no panel can take.
const SPANS: &str = "_spans";

/// The most events whose dashed lines a spans table draws through its
/// tracks, which the stylesheet has a class for each of.
const EVENTS_DRAWN: usize = 32;

/// The script that shows a spans table's tooltips (issue 737): one
/// floating element for every bar, segment and event, from its `data-tip`.
/// The toolbar's script carries it, and the dashboard serves it.
pub const SCRIPT: &str = include_str!("../panels/tips.js");

/// The template of a panel with none of its own, such as one Muru's
/// extensions add in the worker (issue 677).
const GENERIC: &str = "panel";

/// The elements a panel may use: text, quotes, rules, tables, lists,
/// code, details, meters, progress bars, times and abbreviations.
const TAGS: &[&str] = &[
    "p",
    "div",
    "span",
    "br",
    "strong",
    "em",
    "b",
    "i",
    "del",
    "small",
    "sub",
    "sup",
    "h3",
    "h4",
    "a",
    "blockquote",
    "hr",
    "table",
    "caption",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "th",
    "td",
    "colgroup",
    "col",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "code",
    "pre",
    "details",
    "summary",
    "meter",
    "progress",
    "time",
    "abbr",
];

/// The attributes elements keep besides `class`, where they take them:
/// a span's tooltip text and its place in the keyboard's order, for the
/// spans table's bars, segments and events (issue 737).
const ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "title"]),
    ("abbr", &["title"]),
    ("span", &["title", "data-tip", "tabindex"]),
    ("td", &["title", "aria-label"]),
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
    markdown: ammonia::Builder<'static>,
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

        Self {
            templates,
            sanitiser: sanitiser(dashboard, false),
            markdown: sanitiser(dashboard, true),
        }
    }

    /// Draw a panel, sanitised: by its own template, else the generic
    /// one, with a text panel's text first. A panel whose template fails
    /// is drawn as a sentence saying so.
    pub fn render(&self, panel: &admin::Panel) -> String {
        let drawn = self
            .templates
            .get_template(&format!("{}.html", panel.name))
            .or_else(|_| self.templates.get_template(&format!("{GENERIC}.html")))
            .and_then(|template| {
                template.render(context! {
                    panel => minijinja::value::Serde(panel),
                    numeric => numeric_columns(panel),
                    events => events(&panel.spans),
                    extent => extent(&panel.spans),
                })
            });

        let html = match drawn {
            Ok(html) => html,
            Err(error) => format!(
                "<p class=\"error\">The {} panel could not be drawn: {}</p>",
                escape(&panel.name),
                escape(&error.to_string())
            ),
        };

        let mut drawn = self.text(panel);
        drawn.push_str(&self.sanitise(&html));
        drawn
    }

    /// HTML as the sanitiser leaves it.
    pub fn sanitise(&self, html: &str) -> String {
        self.sanitiser.clean(html).to_string()
    }

    /// A text panel's text (issue 680): plain text with its line breaks
    /// kept, or markdown drawn as HTML and sanitised on its own, the one
    /// place a panel may link outside Muru.
    fn text(&self, panel: &admin::Panel) -> String {
        if panel.text.is_empty() {
            return String::new();
        }

        if panel.markdown {
            return self.markdown.clean(&markdown(&panel.text)).to_string();
        }

        self.sanitise(&format!(
            "<div class=\"text\">{}</div>",
            escape(&panel.text)
        ))
    }
}

/// The sanitiser for what panels draw. Links go only to the reserved
/// prefix or the dashboard (section 11.7), except in markdown, whose
/// links may also go to outside `http` and `https` addresses and open in
/// a new tab, telling the page opened nothing of the one it came from.
fn sanitiser(dashboard: Option<&str>, markdown: bool) -> ammonia::Builder<'static> {
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
        .strip_comments(true);

    if markdown {
        sanitiser
            .link_rel(Some("noopener noreferrer"))
            .set_tag_attribute_value("a", "target", "_blank");
    } else {
        sanitiser.link_rel(Some("noopener"));
    }

    let dashboard = dashboard.map(str::to_owned);
    sanitiser.attribute_filter(move |element, attribute, value| {
        if element == "a" && attribute == "href" {
            let allowed = value.starts_with(RESERVED_PREFIX)
                || dashboard
                    .as_deref()
                    .is_some_and(|dashboard| value.starts_with(dashboard))
                || (markdown && (value.starts_with("http://") || value.starts_with("https://")));
            return allowed.then(|| value.into());
        }

        Some(value.into())
    });

    sanitiser
}

/// Markdown as HTML, before it is sanitised: CommonMark with tables and
/// strikethrough, headings moved to the levels panels use, a table
/// column's alignment as a class, and HTML written into the markdown
/// shown as the text it is.
fn markdown(text: &str) -> String {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};

    let moved = |level: HeadingLevel| {
        if level == HeadingLevel::H1 {
            HeadingLevel::H3
        } else {
            HeadingLevel::H4
        }
    };

    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let events = Parser::new_ext(text, options).map(|event| match event {
        Event::Start(Tag::Heading {
            level,
            id,
            classes,
            attrs,
        }) => Event::Start(Tag::Heading {
            level: moved(level),
            id,
            classes,
            attrs,
        }),
        Event::End(TagEnd::Heading(level)) => Event::End(TagEnd::Heading(moved(level))),
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });

    let mut drawn = String::from("<div class=\"markdown\">");
    html::push_html(&mut drawn, events);
    drawn.push_str("</div>");

    // pulldown-cmark aligns a column with a style attribute, which the
    // sanitiser removes. Only it can write these exact tags, since text,
    // code and HTML in the markdown come out escaped.

    for cell in ["th", "td"] {
        for align in ["left", "center", "right"] {
            drawn = drawn.replace(
                &format!("<{cell} style=\"text-align: {align}\">"),
                &format!("<{cell} class=\"align-{align}\">"),
            );
        }
    }

    drawn
}

/// For each of a panel's tables, which of its columns hold only numbers,
/// so the generic template keeps them on one line, aligned right. A
/// column with no rows is not numeric.
fn numeric_columns(panel: &admin::Panel) -> Vec<Vec<bool>> {
    panel
        .tables
        .iter()
        .map(|table| {
            (0..table.columns.len())
                .map(|column| {
                    !table.rows.is_empty()
                        && table.rows.iter().all(|row| {
                            row.cells
                                .get(column)
                                .is_some_and(|cell| cell.trim().parse::<f64>().is_ok())
                        })
                })
                .collect()
        })
        .collect()
}

/// The moments of the events among spans, in the order the table draws
/// them, the first `EVENTS_DRAWN`: each has a dashed line through every
/// row's track.
fn events(spans: &[admin::PanelSpan]) -> Vec<f64> {
    fn gather(spans: &[admin::PanelSpan], found: &mut Vec<f64>) {
        for span in spans {
            if let Some(at) = span.at_ms
                && found.len() < EVENTS_DRAWN
            {
                found.push(at);
            }

            gather(&span.children, found);
        }
    }

    let mut found = Vec::new();
    gather(spans, &mut found);
    found
}

/// Where the last of the spans ends, an event at its moment and a span
/// of segments at its last, for the generic template's clock.
fn extent(spans: &[admin::PanelSpan]) -> f64 {
    spans
        .iter()
        .map(|span| {
            let own = span.at_ms.unwrap_or(span.start_ms + span.duration_ms);
            let segments = span
                .segments
                .iter()
                .map(|segment| segment.start_ms + segment.duration_ms)
                .fold(0.0, f64::max);
            own.max(segments).max(extent(&span.children))
        })
        .fold(0.0, f64::max)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A record's panels, in the order they are shown: Hints (issue 709),
/// read from all the others, then the built-in ones, then those code in
/// the worker added (issue 677), whose spans move from the worker's clock
/// to the request's. One that takes a built-in panel's name is left out.
pub fn all(record: &admin::RequestRecord) -> Vec<admin::Panel> {
    let mut panels = builtin(record);

    let added = record
        .from_worker
        .as_ref()
        .map(|worker| worker.panels.as_slice())
        .unwrap_or_default();

    for panel in added {
        if panel.name == "hints" || panels.iter().any(|shown| shown.name == panel.name) {
            continue;
        }

        let mut panel = panel.clone();
        shift(&mut panel.spans, record.queue_ms);
        panels.push(panel);
    }

    let hints = crate::hints::panel(record, &panels);
    panels.insert(0, hints);
    panels
}

/// Spans moved from the worker's clock, which starts as the queue ends,
/// to the request's.
fn shift(spans: &mut [admin::PanelSpan], by: f64) {
    for span in spans {
        span.start_ms += by;

        if let Some(at) = span.at_ms.as_mut() {
            *at += by;
        }

        for segment in &mut span.segments {
            segment.start_ms += by;
        }

        shift(&mut span.children, by);
    }
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

/// What the panels say of a worker's part that did not arrive.
const WORKER_MISSING: &str = "did not arrive: the worker stopped, its connection failed, \
     or the edge stopped waiting for it at the request's deadline";

fn value(name: &str, value: impl Into<String>) -> admin::PanelValue {
    admin::PanelValue {
        name: name.to_owned(),
        value: value.into(),
    }
}

fn millis(ms: f64) -> String {
    format_duration(std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0))
}

/// The request's phases on its clock: waiting for a worker, the worker's
/// own phases, the response head reaching the edge, and sending the
/// response.
fn timing(record: &admin::RequestRecord) -> admin::Panel {
    let span = |name: &str, start_ms: f64, duration_ms: f64, category: &str| admin::PanelSpan {
        name: name.to_owned(),
        start_ms,
        duration_ms: duration_ms.max(0.0),
        category: category.to_owned(),
        ..Default::default()
    };

    // A moment rather than a period (issue 737).

    let event = |name: &str, at_ms: f64, category: &str| admin::PanelSpan {
        name: name.to_owned(),
        start_ms: at_ms,
        at_ms: Some(at_ms),
        category: category.to_owned(),
        ..Default::default()
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
                event("response head at the edge", answered, "edge"),
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

            // The application runs once the waits it started with are
            // over: for a thread, then for the GIL.

            if worker.to_start_response > 0.0 {
                let running = worker.thread_wait + worker.gil_wait_first;
                phases.push(span(
                    "running the application",
                    at(running),
                    worker.to_start_response - running,
                    "worker",
                ));
                phases.push(event(
                    "response started",
                    at(worker.to_start_response),
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
            // for a thread, every wait for the GIL and every stretch with
            // the event loop held.

            if worker.thread_wait > 0.0 {
                phases.push(span(
                    "waiting for a thread",
                    at(0.0),
                    worker.thread_wait,
                    "contention",
                ));
            }

            let waits = worker.gil_waits as usize;
            phases.extend(stretches(
                "waiting for the GIL",
                &worker.gil_wait_stretches,
                record.queue_ms,
                |index| format!("wait {} of {}", index + 1, waits.max(index + 1)),
            ));

            let held = worker.loop_held_stretches.len();
            phases.extend(stretches(
                "event loop held",
                &worker.loop_held_stretches,
                record.queue_ms,
                |index| format!("stretch {} of {held}", index + 1),
            ));

            // Issue 738: the modules loaded for the first time in the
            // worker, each outermost load a segment named by its module,
            // with the loads nested in it inside its segment.

            phases.extend(importing(worker, record.queue_ms));

            // A wait goes before the phase that starts with it, and a
            // moment before a period that starts then.

            let rank = |span: &admin::PanelSpan| match (span.category.as_str(), span.at_ms) {
                ("contention", _) => 0,
                (_, Some(_)) => 1,
                _ => 2,
            };
            phases.sort_by(|one, other| {
                one.start_ms
                    .total_cmp(&other.start_ms)
                    .then_with(|| rank(one).cmp(&rank(other)))
            });
            let mut whole = span("in the worker", at(0.0), worker.to_close, "worker");
            whole.children = phases;

            // It goes beside the edge's view of the same time, after the
            // queue.

            let at = request.children.len().min(1);
            request.children.insert(at, whole);
        }

        spans.push(request);
    }

    match record.worker_part.as_str() {
        crate::recorder::WORKER_EXPECTED => {
            values.push(value("worker's part", "still to come"));
        }
        crate::recorder::WORKER_MISSING => values.push(value("worker's part", WORKER_MISSING)),
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
            .map(|worker| [during(worker), modules(worker, record.queue_ms)])
            .into_iter()
            .flatten()
            .filter(|table| !table.rows.is_empty())
            .collect(),
        ..Default::default()
    }
}

/// The row of the modules a request loaded for the first time in the
/// worker, moved onto the request's clock by `by`: a segment for each
/// outermost load, named by its module, the loads nested in it inside
/// it. None when it loaded none.
fn importing(worker: &admin::WorkerPart, by: f64) -> Option<admin::PanelSpan> {
    let outermost: Vec<&admin::ModuleLoad> = worker
        .module_loads
        .iter()
        .filter(|load| load.depth == 0)
        .collect();
    let first = outermost.first()?;
    let start_ms = by + first.at_ms;
    let end_ms = outermost
        .iter()
        .map(|load| by + load.at_ms + load.ms)
        .fold(start_ms, f64::max);

    Some(admin::PanelSpan {
        name: "importing modules".to_owned(),
        start_ms,
        duration_ms: end_ms - start_ms,
        category: "worker".to_owned(),
        segments: outermost
            .iter()
            .map(|load| admin::PanelSegment {
                start_ms: by + load.at_ms,
                duration_ms: load.ms.max(0.0),
                label: load.module.clone(),
                detail: loaded_by(load),
            })
            .collect(),
        ..Default::default()
    })
}

/// What asked for a module, for its tooltip and its row, and whether it
/// resolved an import `lazy_imports` deferred.
fn loaded_by(load: &admin::ModuleLoad) -> String {
    match (load.deferred, load.trigger.is_empty()) {
        (false, true) => String::new(),
        (false, false) => format!("loaded by {}", load.trigger),
        (true, true) => "a deferred import".to_owned(),
        (true, false) => format!("a deferred import, used by {}", load.trigger),
    }
}

/// The modules a request loaded for the first time in the worker, one
/// row each in the order they began, moved onto the request's clock by
/// `by`, a nested one indented beneath the one it is nested in, with how
/// many were past the record's limit.
fn modules(worker: &admin::WorkerPart, by: f64) -> admin::PanelTable {
    let mut rows: Vec<admin::PanelRow> = worker
        .module_loads
        .iter()
        .map(|load| admin::PanelRow {
            cells: vec![
                format!(
                    "{}{}{}",
                    "  ".repeat(load.depth as usize),
                    load.module,
                    if load.deferred { " (deferred)" } else { "" }
                ),
                millis(by + load.at_ms),
                millis(load.ms),
                load.trigger.clone(),
            ],
        })
        .collect();
    let past = (worker.modules_loaded as usize).saturating_sub(rows.len());

    if past > 0 {
        rows.push(admin::PanelRow {
            cells: vec![
                format!("{past} more, past the record's limit"),
                String::new(),
                String::new(),
                String::new(),
            ],
        });
    }

    admin::PanelTable {
        caption: "Modules loaded on first use".to_owned(),
        columns: ["Module", "Starts", "Takes", "Loaded by"]
            .map(str::to_owned)
            .to_vec(),
        rows,
    }
}

/// A row of the worker's stretches, moved onto the request's clock by
/// `by`: one stretch as an ordinary bar, several as the segments of one
/// span from the first's start to the last's end, each of them described
/// by `detail` from its index. None with no stretch.
fn stretches(
    name: &str,
    stretches: &[admin::Stretch],
    by: f64,
    detail: impl Fn(usize) -> String,
) -> Option<admin::PanelSpan> {
    let first = stretches.first()?;
    let last = stretches.last()?;
    let start_ms = by + first.at_ms;

    Some(admin::PanelSpan {
        name: name.to_owned(),
        start_ms,
        duration_ms: (by + last.at_ms + last.ms - start_ms).max(0.0),
        category: "contention".to_owned(),
        segments: stretches
            .iter()
            .enumerate()
            .map(|(index, stretch)| admin::PanelSegment {
                start_ms: by + stretch.at_ms,
                duration_ms: stretch.ms.max(0.0),
                label: name.to_owned(),
                detail: if stretches.len() > 1 {
                    detail(index)
                } else {
                    String::new()
                },
            })
            .collect(),
        ..Default::default()
    })
}

/// `count` of `noun`, in the plural unless it is one.
fn counted(count: u32, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else if noun.ends_with("ch") {
        format!("{count} {noun}es")
    } else {
        format!("{count} {noun}s")
    }
}

/// What held a request up while the worker had it, as a table of what
/// and how long, with what the timeline cannot place: the GIL waits in
/// all and the garbage collections; and the modules loaded on first use
/// in all. Empty when nothing did.
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

    if worker.modules_loaded > 0 {
        row(
            "modules loaded on first use",
            format!(
                "{} in {}",
                millis(worker.modules_ms),
                counted(worker.modules_loaded, "module")
            ),
        );
    }

    let held = &worker.loop_held_stretches;

    if !held.is_empty() {
        row(
            "event loop held",
            format!(
                "{} in {}, the longest {} from {} on the worker's clock",
                millis(held.iter().map(|stretch| stretch.ms).sum()),
                counted(u32::try_from(held.len()).unwrap_or(u32::MAX), "stretch"),
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

    if record.worker_part == crate::recorder::WORKER_MISSING {
        values.push(value(
            "worker's part",
            format!("The worker's part of the record {WORKER_MISSING}, so the request's log records are not here"),
        ));
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
/// error it answered itself, and the evidence Muru captured of a timeout
/// or a killed worker (issue 706), each capture a table of its own.
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

    // A framework that catches the exception logs it and answers 500
    // itself, so the worker saw the application complete (issue 722):
    // the traceback is the first error the request logged with one.

    if let Some(worker) = &record.from_worker
        && values.is_empty()
        && record.status >= 500
        && record.reason.is_empty()
    {
        values.push(value(
            "answered",
            format!("the application answered {} itself", record.status),
        ));

        let logged = worker.logs.iter().find(|line| {
            matches!(line.level.as_str(), "ERROR" | "CRITICAL")
                && line.message.contains("Traceback (most recent call last):")
        });

        if let Some(line) = logged {
            values.push(value(
                "logged",
                format!("{}: {}", line.logger, line.message.trim_end()),
            ));
        }
    }

    if record.worker_part == crate::recorder::WORKER_MISSING {
        values.push(value(
            "worker's part",
            format!("{WORKER_MISSING}, so an exception the application raised is not shown"),
        ));
    }

    admin::Panel {
        name: "errors".to_owned(),
        title: "Errors".to_owned(),
        values,
        tables: record.captures.iter().map(captured).collect(),
        ..Default::default()
    }
}

/// One capture as a table of names and values: what captured it and
/// when, the worker, the request's stack, the worker's other threads and
/// the other records the capture reached.
fn captured(capture: &admin::RequestCapture) -> admin::PanelTable {
    let at = muru_common::log::clock(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(capture.at_unix_ms),
    );
    let row = |name: &str, value: String| admin::PanelRow {
        cells: vec![name.to_owned(), value],
    };
    let mut rows = vec![row("message", capture.message.clone())];

    if !capture.worker.is_empty() {
        let pid = if capture.pid > 0 {
            format!(" (pid {})", capture.pid)
        } else {
            String::new()
        };
        rows.push(row("worker", format!("{}{pid}", capture.worker)));
    }

    if !capture.reason.is_empty() {
        rows.push(row("why", capture.reason.clone()));
    }

    if !capture.stack.is_empty() {
        rows.push(row("stack", capture.stack.clone()));
    }

    for thread in &capture.threads {
        let name = if thread.request_id.is_empty() {
            format!("thread {}", thread.name)
        } else {
            format!("thread {}, request {}", thread.name, thread.request_id)
        };
        rows.push(row(&name, thread.stack.clone()));
    }

    for other in &capture.related {
        rows.push(row("also reached", other.clone()));
    }

    admin::PanelTable {
        caption: format!("{} at {at}", capture.kind),
        columns: vec!["name".to_owned(), "value".to_owned()],
        rows,
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
            html.contains("<td class=\"depth-2\"><em>response head at the edge</em></td>"),
            "{html}"
        );
        assert!(html.contains("What the phases are"), "{html}");

        // Each bar starts where its phase does, in whole percent of the
        // request: the request's own across all of it. The head reached
        // the edge at 18.1 ms of 18.4, a moment drawn as a diamond.

        assert!(html.contains("class=\"bar from-0 for-100\""), "{html}");
        assert!(html.contains("<td class=\"num\">18.1 ms</td>"), "{html}");
        assert!(html.contains("class=\"diamond from-98 for-0\""), "{html}");
    }

    #[test]
    fn the_timing_panel_places_the_workers_phases_after_the_queue() {
        let html = drawn("timing");
        assert!(
            html.contains("<td class=\"depth-2\">in the worker</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-3\">running the application</td>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"depth-3\"><em>response started</em></td>"),
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
        let stretch = |at_ms: f64, ms: f64| admin::Stretch { at_ms, ms };
        worker.gil_wait_stretches = vec![stretch(0.5, 0.3), stretch(6.0, 0.05), stretch(9.0, 0.05)];
        worker.loop_held_stretches = vec![stretch(4.0, 9.0)];
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

        // Every wait for the GIL is a segment on the one line, and the
        // application runs from the end of the first; the one stretch
        // with the loop held is an ordinary bar.

        assert!(html.contains("<td class=\"num\">3 segments</td>"), "{html}");
        assert!(
            html.contains("waiting for the GIL\nstarts at 6.2 ms\ntakes 0.05 ms\nwait 2 of 3"),
            "{html}"
        );
        assert!(
            html.contains("running the application\nstarts at 1.0 ms\ntakes 1.2 ms"),
            "{html}"
        );
        assert!(
            html.contains("event loop held\nstarts at 4.2 ms\ntakes 9.0 ms\""),
            "{html}"
        );

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
        assert!(
            html.contains("9.0ms in 1 stretch, the longest 9.0ms from 4.0ms"),
            "{html}"
        );

        // Nothing held up, no table.

        assert!(timing(&fixture()).tables.is_empty());
    }

    #[test]
    fn the_timing_panel_shows_the_modules_loaded_on_first_use() {
        let mut record = fixture();
        let worker = record.from_worker.as_mut().unwrap();
        let load = |module: &str, at_ms: f64, ms: f64, depth: u32| admin::ModuleLoad {
            module: module.to_owned(),
            at_ms,
            ms,
            depth,
            trigger: "shop/views.py:12".to_owned(),
            deferred: false,
        };
        worker.module_loads = vec![
            load("shop.reports", 0.5, 0.25, 0),
            admin::ModuleLoad {
                deferred: true,
                ..load("openpyxl", 0.75, 1.0, 0)
            },
            load("openpyxl.cell", 0.8, 0.5, 1),
        ];
        worker.modules_loaded = 4;
        worker.modules_ms = 1.5;
        let html = Renderer::new(None).render(&timing(&record));

        // A segment for each outermost load, named by its module, the
        // nested one inside its segment.

        assert!(html.contains(">importing modules</td>"), "{html}");
        assert!(html.contains("<td class=\"num\">2 segments</td>"), "{html}");
        assert!(
            html.contains(
                "openpyxl\nstarts at 0.95 ms\ntakes 1.0 ms\na deferred import, used by shop/views.py:12"
            ),
            "{html}"
        );

        // The table counts them, and each is a row below it, the nested
        // one indented and the one past the limit counted.

        assert!(html.contains("1.5ms in 4 modules"), "{html}");
        assert!(
            html.contains("<summary>Modules loaded on first use</summary>"),
            "{html}"
        );
        assert!(
            html.contains("<td class=\"module\">  openpyxl.cell</td>"),
            "{html}"
        );
        assert!(html.contains("1 more, past the record's limit"), "{html}");

        // Issue 740: a load that resolved a deferred import says so.

        assert!(
            html.contains("<td class=\"module\">openpyxl (deferred)</td>"),
            "{html}"
        );
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

        // A 500 a framework answered after catching the exception, with
        // the error it logged; a 200 with an error logged is not one.

        let mut caught = fixture();
        caught.status = 500;
        caught.reason.clear();
        let worker = caught.from_worker.as_mut().unwrap();
        worker.outcome = "completed".to_owned();
        worker.logs.push(admin::RequestLog {
            level: "ERROR".to_owned(),
            logger: "shop".to_owned(),
            message: "Exception on /authors/4 [GET]\nTraceback (most recent call last):\n  ...\nZeroDivisionError: division by zero".to_owned(),
            ..Default::default()
        });
        let html = Renderer::new(None).render(&errors(&caught));
        assert!(
            html.contains("<th>answered</th><td>the application answered 500 itself</td>"),
            "{html}"
        );
        assert!(
            html.contains("<pre class=\"error\">shop: Exception on /authors/4 [GET]"),
            "{html}"
        );

        caught.status = 200;
        let html = Renderer::new(None).render(&errors(&caught));
        assert!(html.contains("No errors."), "{html}");
    }

    #[test]
    fn the_errors_and_logs_panels_give_a_capture_and_a_part_that_never_came() {
        let mut killed = fixture();
        killed.from_worker = None;
        killed.worker_part = crate::recorder::WORKER_MISSING.to_owned();
        killed.captures = vec![admin::RequestCapture {
            kind: "worker.timeout_killed".to_owned(),
            at_unix_ms: 1_700_000_000_000,
            message: "worker 0 (pid 7) replaced by pid 8: past the hard timeout".to_owned(),
            worker: "0".to_owned(),
            pid: 7,
            reason: "hard_timeout".to_owned(),
            stack: "  File \"shop.py\", line 3, in <view>\n    time.sleep(60)".to_owned(),
            threads: vec![admin::CaptureThread {
                name: "muru-app-1".to_owned(),
                request_id: "B 2".to_owned(),
                stack: "  File \"shop.py\", line 9, in list".to_owned(),
            }],
            related: vec!["B 2".to_owned()],
        }];

        let html = Renderer::new(None).render(&errors(&killed));
        assert!(
            html.contains("<caption>worker.timeout_killed at "),
            "{html}"
        );
        assert!(html.contains("<th>worker</th><td>0 (pid 7)</td>"), "{html}");
        assert!(html.contains("<th>why</th><td>hard_timeout</td>"), "{html}");
        assert!(
            html.contains("<pre class=\"error\">  File \"shop.py\", line 3, in &lt;view&gt;"),
            "{html}"
        );
        assert!(
            html.contains("<th>thread muru-app-1, request B 2</th>"),
            "{html}"
        );
        assert!(
            html.contains(
                "<a href=\"/_muru/requests/B%202/panels/errors.html\" rel=\"noopener\">B 2</a>"
            ),
            "{html}"
        );
        assert!(html.contains("the edge stopped waiting for it"), "{html}");
        assert!(!html.contains("No errors."), "{html}");

        let html = Renderer::new(None).render(&logs(&killed));
        assert!(
            html.contains("so the request's log records are not here."),
            "{html}"
        );
        assert!(!html.contains("No log records"), "{html}");
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
    fn a_panel_with_no_template_of_its_own_is_drawn_by_the_generic_one() {
        let renderer = Renderer::new(None);

        let empty = renderer.render(&admin::Panel {
            name: "nothing".to_owned(),
            ..Default::default()
        });
        assert!(empty.contains("Nothing was added to this panel"), "{empty}");

        let html = renderer.render(&admin::Panel {
            name: "library".to_owned(),
            title: "Library".to_owned(),
            values: vec![value("books", "7")],
            tables: vec![admin::PanelTable {
                caption: "Queries".to_owned(),
                columns: vec!["statement".to_owned(), "times".to_owned()],
                rows: vec![
                    admin::PanelRow {
                        cells: vec![
                            "SELECT name FROM author WHERE id = ?".to_owned(),
                            "7".to_owned(),
                        ],
                    },
                    admin::PanelRow {
                        cells: vec!["SELECT 1".to_owned(), "0.125".to_owned()],
                    },
                ],
            }],
            spans: vec![admin::PanelSpan {
                name: "books()".to_owned(),
                start_ms: 1.0,
                duration_ms: 3.0,
                category: "view".to_owned(),
                children: vec![
                    admin::PanelSpan {
                        name: "<script>".to_owned(),
                        start_ms: 2.0,
                        duration_ms: 0.5,
                        category: "database".to_owned(),
                        failed: true,
                        ..Default::default()
                    },
                    admin::PanelSpan {
                        name: "committed".to_owned(),
                        start_ms: 3.5,
                        at_ms: Some(3.5),
                        category: "database".to_owned(),
                        ..Default::default()
                    },
                    admin::PanelSpan {
                        name: "imports".to_owned(),
                        start_ms: 1.0,
                        duration_ms: 3.0,
                        segments: vec![
                            admin::PanelSegment {
                                start_ms: 1.0,
                                duration_ms: 1.0,
                                label: "json".to_owned(),
                                detail: "from the standard library".to_owned(),
                            },
                            admin::PanelSegment {
                                start_ms: 3.0,
                                duration_ms: 1.0,
                                label: "decimal".to_owned(),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    },
                    admin::PanelSpan {
                        name: "one".to_owned(),
                        start_ms: 1.0,
                        duration_ms: 3.0,
                        segments: vec![admin::PanelSegment {
                            start_ms: 2.0,
                            duration_ms: 2.0,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            dropped: 2,
            ..Default::default()
        });

        assert!(html.contains("<th>books</th>"), "{html}");
        assert!(html.contains("<caption>Queries</caption>"), "{html}");
        assert!(html.contains("WHERE id = ?"), "{html}");
        assert!(
            html.contains("<th>statement</th><th class=\"num\">times</th>"),
            "{html}"
        );
        assert!(html.contains("<td class=\"mono num\">0.125</td>"), "{html}");
        assert!(html.contains("<td class=\"mono\">SELECT 1</td>"), "{html}");
        assert!(
            html.contains("books()") && html.contains("depth-2"),
            "{html}"
        );

        // A failed span is marked beside its name and in its tooltip, and
        // keeps its category's class.

        assert!(html.contains("<tr class=\"view\">"), "{html}");
        assert!(html.contains("<tr class=\"database failed\">"), "{html}");
        assert_eq!(html.matches("class=\"failed-mark\"").count(), 1, "{html}");
        assert!(
            html.contains("data-tip=\"&lt;script&gt;\nstarts at 2.0 ms\ntakes 0.5 ms\nfailed\""),
            "{html}"
        );

        // Issue 737: an event is a diamond, named in italics, its time
        // under Starts and a dash for its length, and its dashed line goes
        // through every row's track, 3.5 ms of the clock's 4.

        assert!(
            html.contains("<tr class=\"database event event-0\">"),
            "{html}"
        );
        assert!(html.contains("<em>committed</em>"), "{html}");
        assert!(
            html.contains("<td class=\"num\" title=\"event\" aria-label=\"event\">–</td>"),
            "{html}"
        );
        assert!(
            html.contains(
                "class=\"diamond from-88 for-0\" tabindex=\"0\" data-tip=\"committed\nat 3.5 ms\""
            ),
            "{html}"
        );
        assert_eq!(
            html.matches("class=\"rule event-0 from-88 for-0\"").count(),
            5,
            "{html}"
        );

        // Several segments on one line, counted and summed, each with its
        // own tooltip; a single segment is an ordinary bar where it is.

        assert!(html.contains("<td class=\"num\">2 segments</td>"), "{html}");
        assert!(html.contains("<td class=\"num\">2.0 ms</td>"), "{html}");
        assert!(
            html.contains("class=\"bar segment from-25 for-25\" tabindex=\"0\" data-tip=\"json\nstarts at 1.0 ms\ntakes 1.0 ms\nfrom the standard library\""),
            "{html}"
        );
        assert!(
            html.contains(
                "class=\"bar from-50 for-50\" tabindex=\"0\" data-tip=\"one\nstarts at 2.0 ms"
            ),
            "{html}"
        );
        assert!(
            html.contains("&lt;script&gt;") && !html.contains("<script>"),
            "{html}"
        );
        assert!(
            html.contains("2 entries were past the panel's limits"),
            "{html}"
        );
    }

    #[test]
    fn a_text_panel_is_drawn_plain_or_from_markdown_with_its_own_links() {
        let renderer = Renderer::new(None);

        let plain = renderer.render(&admin::Panel {
            name: "notes".to_owned(),
            text: "one <b>\ntwo".to_owned(),
            ..Default::default()
        });
        assert!(
            plain.contains("<div class=\"text\">one &lt;b&gt;\ntwo</div>"),
            "{plain}"
        );
        assert!(!plain.contains("Nothing was added"), "{plain}");

        let markdown = renderer.render(&admin::Panel {
            name: "analysis".to_owned(),
            text:
                "# Queries\n\n| statement | times |\n|---|---:|\n| SELECT 1 | 7 |\n\n\
                   > ~~slow~~ quoted\n\n## Detail\n\n**3** ran. See [the guide](https://example.com/guide) \
                   or [a record](/_muru/requests/A1.json).\n\n<script>alert(1)</script>\n\n\
                   [bad](javascript:alert(1))"
                    .to_owned(),
            markdown: true,
            ..Default::default()
        });

        assert!(markdown.contains("<h3>Queries</h3>"), "{markdown}");
        assert!(markdown.contains("<h4>Detail</h4>"), "{markdown}");
        assert!(markdown.contains("<strong>3</strong>"), "{markdown}");
        assert!(
            markdown.contains("<table>")
                && markdown.contains("<th>statement</th>")
                && markdown.contains("<th class=\"align-right\">times</th>")
                && markdown.contains("<td class=\"align-right\">7</td>"),
            "{markdown}"
        );
        assert!(
            markdown.contains("<blockquote>") && markdown.contains("<del>slow</del>"),
            "{markdown}"
        );
        assert!(
            markdown.contains(
                "<a href=\"https://example.com/guide\" target=\"_blank\" rel=\"noopener noreferrer\">"
            ),
            "{markdown}"
        );
        assert!(
            markdown.contains("href=\"/_muru/requests/A1.json\""),
            "{markdown}"
        );
        assert!(
            markdown.contains("&lt;script&gt;") && !markdown.contains("<script>"),
            "{markdown}"
        );
        assert!(!markdown.contains("javascript"), "{markdown}");

        // Outside markdown, an outside link is not kept.

        let elsewhere = renderer.sanitise("<a href=\"https://example.com/guide\">guide</a>");
        assert_eq!(elsewhere, "<a rel=\"noopener\">guide</a>");
    }

    #[test]
    fn an_extensions_panels_follow_the_built_in_ones_on_the_requests_clock() {
        let mut record = admin::RequestRecord {
            status: 200,
            queue_ms: 4.0,
            total_ms: 10.0,
            ..Default::default()
        };
        record.from_worker = Some(admin::WorkerPart {
            panels: vec![
                admin::Panel {
                    name: "trace".to_owned(),
                    title: "Trace".to_owned(),
                    spans: vec![admin::PanelSpan {
                        name: "view".to_owned(),
                        start_ms: 1.0,
                        duration_ms: 2.0,
                        children: vec![
                            admin::PanelSpan {
                                name: "query".to_owned(),
                                start_ms: 1.5,
                                segments: vec![admin::PanelSegment {
                                    start_ms: 1.5,
                                    duration_ms: 0.5,
                                    ..Default::default()
                                }],
                                ..Default::default()
                            },
                            admin::PanelSpan {
                                name: "committed".to_owned(),
                                start_ms: 2.5,
                                at_ms: Some(2.5),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                admin::Panel {
                    name: "timing".to_owned(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let panels = all(&record);
        let names: Vec<&str> = panels.iter().map(|panel| panel.name.as_str()).collect();

        assert_eq!(
            names,
            [
                "hints",
                "timing",
                "where",
                "response",
                "logs",
                "errors",
                "connection",
                "trace"
            ]
        );

        let trace = &panels[7];
        assert_eq!(trace.spans[0].start_ms, 5.0);
        assert_eq!(trace.spans[0].children[0].start_ms, 5.5);

        // An event's moment and a span's segments move with it.

        assert_eq!(trace.spans[0].children[0].segments[0].start_ms, 5.5);
        assert_eq!(trace.spans[0].children[1].at_ms, Some(6.5));
    }
}
