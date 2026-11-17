//! The Hints panel (issue 709): what stands out in a request's record,
//! each hint naming the panel and the figure it comes from. Hints read
//! the record and its other panels and change nothing; each threshold is
//! a constant here, documented in the toolbar guide.

use muru_proto::admin::v1 as admin;

use crate::access::{format_bytes, format_duration};

/// A statement run, or an endpoint called, this many times in one
/// request is a hint: the N+1 pattern.
pub const REPEATED: usize = 5;

/// A single query or outbound call taking this share of the request's
/// total time is a hint...
pub const SLOW_SHARE: f64 = 0.25;

/// ...when it took at least this long, in milliseconds.
pub const SLOW_MS: f64 = 10.0;

/// An HTML or JSON response of at least this many bytes that went out
/// uncompressed is a hint, the default `min_size`.
pub const UNCOMPRESSED_BYTES: u64 = 1024;

/// The event loop held for this long during the request, in
/// milliseconds, is a hint.
pub const LOOP_HELD_MS: f64 = 100.0;

/// A wait for a worker, or for a thread of the worker, this long, in
/// milliseconds, is a hint.
pub const WAIT_MS: f64 = 100.0;

/// Garbage collections during the request taking this long in all, in
/// milliseconds, are a hint...
pub const GC_MS: f64 = 50.0;

/// ...as are this many of them.
pub const GC_COLLECTIONS: u32 = 20;

/// Modules loaded for the first time in the worker taking this share of
/// the request's total time are a hint...
pub const IMPORT_SHARE: f64 = 0.25;

/// ...when they took at least this long, in milliseconds.
pub const IMPORT_MS: f64 = 50.0;

/// The most hints of one kind, the strongest first.
const PER_KIND: usize = 3;

/// Why a response was not compressed that a hint names; the others say
/// there was nothing to compress, or that it already was.
const UNCOMPRESSED_REASONS: &[&str] = &[
    "off",
    "opted_out",
    "no_transform",
    "type",
    "not_accepted",
    "failed",
];

/// The edge's own reasons a hint names, with the setting behind each and
/// how a run sets it.
const OWN_REASONS: &[(&str, &str, &str)] = &[
    ("deadline", "request_deadline", "--timeout deadline="),
    ("soft_timeout", "soft_timeout", "--timeout soft="),
    ("hard_timeout", "hard_timeout", "--timeout hard="),
    ("queue_full", "queue_timeout", "--timeout queue="),
    ("cold_start_timeout", "queue_timeout", "--timeout queue="),
    (
        "client_timeout",
        "body_idle_timeout",
        "--timeout body_idle=",
    ),
    (
        "worker_stalled",
        "loop_stall_timeout or gil_stall_timeout",
        "--timeout loop_stall= or gil_stall=",
    ),
    ("body_too_large", "max_body_size", "--max-body-size"),
    ("upload_too_large", "max_file_size", ""),
    ("too_many_files", "max_files", ""),
];

/// One hint: what stands out, the panel it comes from and the figure.
struct Hint {
    text: String,
    panel: &'static str,
    figure: String,
}

/// The Hints panel of a record, from the record and its other panels,
/// those from the worker already on the request's clock.
pub fn panel(record: &admin::RequestRecord, panels: &[admin::Panel]) -> admin::Panel {
    let mut hints = Vec::new();
    let named = |name: &str| panels.iter().find(|panel| panel.name == name);

    if let Some(sql) = named("sql") {
        repeated(
            &mut hints,
            sql,
            "By statement",
            0,
            "SQL",
            "the same statement ran",
        );
        slow(
            &mut hints,
            record,
            sql,
            "Each query",
            &[2],
            "SQL",
            "one query",
        );
    }

    if let Some(outbound) = named("outbound") {
        repeated(
            &mut hints,
            outbound,
            "By endpoint",
            usize::MAX,
            "Outbound",
            "the same endpoint was called",
        );
        slow(
            &mut hints,
            record,
            outbound,
            "Each call",
            &[2, 3],
            "Outbound",
            "one call to another service",
        );
    }

    uncompressed(&mut hints, record);

    if let Some(worker) = &record.from_worker {
        held(&mut hints, record, worker, named("trace"));
        waited(&mut hints, record, worker);
        collected(&mut hints, worker);
        imported(&mut hints, record, worker);
    }

    answered(&mut hints, record);

    admin::Panel {
        name: "hints".to_owned(),
        title: "Hints".to_owned(),
        tables: vec![admin::PanelTable {
            caption: "What stands out".to_owned(),
            columns: ["hint", "panel", "figure"].map(str::to_owned).to_vec(),
            rows: hints
                .into_iter()
                .map(|hint| admin::PanelRow {
                    cells: vec![hint.text, hint.panel.to_owned(), hint.figure],
                })
                .collect(),
        }],
        ..Default::default()
    }
}

/// The rows of one of a panel's tables, by its caption.
fn rows<'a>(panel: &'a admin::Panel, caption: &str) -> &'a [admin::PanelRow] {
    panel
        .tables
        .iter()
        .find(|table| table.caption == caption)
        .map(|table| table.rows.as_slice())
        .unwrap_or_default()
}

fn number(cell: Option<&String>) -> f64 {
    cell.and_then(|cell| cell.trim().parse().ok())
        .unwrap_or(0.0)
}

fn millis(ms: f64) -> String {
    format_duration(std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0))
}

/// A grouped table's rows run or called at least `REPEATED` times: the
/// statement in column `what`, or for an endpoint (`usize::MAX`) its
/// method, host and path; then how many times, and the total.
fn repeated(
    hints: &mut Vec<Hint>,
    panel: &admin::Panel,
    caption: &str,
    what: usize,
    from: &'static str,
    saying: &str,
) {
    let (times, total) = if what == usize::MAX { (3, 4) } else { (1, 2) };

    let found = rows(panel, caption)
        .iter()
        .filter(|row| number(row.cells.get(times)) >= REPEATED as f64)
        .take(PER_KIND);

    for row in found {
        let named = if what == usize::MAX {
            row.cells
                .get(..3)
                .map(|cells| cells.join(" "))
                .unwrap_or_default()
        } else {
            row.cells.get(what).cloned().unwrap_or_default()
        };
        let count = number(row.cells.get(times)) as u64;

        hints.push(Hint {
            text: format!(
                "{saying} {count} times in one request, which an N+1 pattern does: {named}"
            ),
            panel: from,
            figure: format!(
                "{count} times, {} in all",
                millis(number(row.cells.get(total)))
            ),
        });
    }
}

/// A table's rows, each a query or call, that took at least `SLOW_MS`
/// and `SLOW_SHARE` of the request's time; the time is column 1, and the
/// columns `what` name it.
fn slow(
    hints: &mut Vec<Hint>,
    record: &admin::RequestRecord,
    panel: &admin::Panel,
    caption: &str,
    what: &[usize],
    from: &'static str,
    saying: &str,
) {
    if record.total_ms <= 0.0 {
        return;
    }

    let mut found: Vec<(f64, String)> = rows(panel, caption)
        .iter()
        .map(|row| {
            let named: Vec<&str> = what
                .iter()
                .filter_map(|column| row.cells.get(*column).map(String::as_str))
                .collect();
            (number(row.cells.get(1)), named.join(" "))
        })
        .filter(|(took, _)| *took >= SLOW_MS && *took >= SLOW_SHARE * record.total_ms)
        .collect();
    found.sort_by(|one, other| other.0.total_cmp(&one.0));

    for (took, named) in found.into_iter().take(PER_KIND) {
        hints.push(Hint {
            text: format!(
                "{saying} took {:.0}% of the request: {named}",
                took * 100.0 / record.total_ms
            ),
            panel: from,
            figure: format!("{} of {}", millis(took), millis(record.total_ms)),
        });
    }
}

/// An HTML or JSON response of at least `UNCOMPRESSED_BYTES` that went
/// out uncompressed, for a reason other than having nothing to compress.
fn uncompressed(hints: &mut Vec<Hint>, record: &admin::RequestRecord) {
    let Some(compression) = &record.compression else {
        return;
    };

    let kind = record
        .response_headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| {
            header
                .value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let textual = kind == "text/html" || kind == "application/json" || kind.ends_with("+json");

    if compression.compressed
        || !textual
        || record.bytes < UNCOMPRESSED_BYTES
        || !UNCOMPRESSED_REASONS.contains(&compression.skipped.as_str())
    {
        return;
    }

    hints.push(Hint {
        text: format!(
            "a {} {kind} response went out uncompressed: {}",
            format_bytes(record.bytes),
            crate::compression::describe_skipped(&compression.skipped)
        ),
        panel: "Response",
        figure: format!("{} sent", format_bytes(record.bytes)),
    });
}

/// The event loop held for at least `LOOP_HELD_MS`, with the deepest span
/// of the Trace panel running in the middle of the stretch.
fn held(
    hints: &mut Vec<Hint>,
    record: &admin::RequestRecord,
    worker: &admin::WorkerPart,
    trace: Option<&admin::Panel>,
) {
    if worker.loop_lag < LOOP_HELD_MS {
        return;
    }

    let middle = record.queue_ms + worker.loop_lag_at + worker.loop_lag / 2.0;
    let running = trace.and_then(|trace| deepest(&trace.spans, middle));
    let during = running
        .map(|span| format!(", while {} was running", span.name))
        .unwrap_or_default();

    hints.push(Hint {
        text: format!(
            "the worker's event loop was held for {} during the request{during}, which holds up every request on the loop",
            millis(worker.loop_lag)
        ),
        panel: if running.is_some() { "Timing, Trace" } else { "Timing" },
        figure: format!(
            "{} held, from {}",
            millis(worker.loop_lag),
            millis(record.queue_ms + worker.loop_lag_at)
        ),
    });
}

/// The deepest span running at `at` on the request's clock.
fn deepest(spans: &[admin::PanelSpan], at: f64) -> Option<&admin::PanelSpan> {
    spans
        .iter()
        .rfind(|span| span.start_ms <= at && at <= span.start_ms + span.duration_ms)
        .map(|span| deepest(&span.children, at).unwrap_or(span))
}

/// A long wait for a worker, or for one of its threads.
fn waited(hints: &mut Vec<Hint>, record: &admin::RequestRecord, worker: &admin::WorkerPart) {
    if record.queue_ms >= WAIT_MS {
        hints.push(Hint {
            text: "the request waited for a worker: the run had too few workers for the page's requests; raise processes (--processes)".to_owned(),
            panel: "Timing",
            figure: format!("{} waiting for a worker", millis(record.queue_ms)),
        });
    }

    if worker.thread_wait >= WAIT_MS {
        hints.push(Hint {
            text: "the request waited for a thread of its worker: the worker had too few threads for the page's requests; raise threads (--threads)".to_owned(),
            panel: "Timing",
            figure: format!("{} waiting for a thread", millis(worker.thread_wait)),
        });
    }
}

/// Long or many garbage collections while the request was in the worker.
fn collected(hints: &mut Vec<Hint>, worker: &admin::WorkerPart) {
    let collections: u32 = worker
        .gc
        .iter()
        .map(|generation| generation.collections)
        .sum();
    let ms: f64 = worker.gc.iter().map(|generation| generation.ms).sum();

    if collections == 0 || (ms < GC_MS && collections < GC_COLLECTIONS) {
        return;
    }

    hints.push(Hint {
        text: "garbage collection ran during the request, pausing every thread of the worker; the request may allocate many objects".to_owned(),
        panel: "Timing",
        figure: format!(
            "{collections} {}, {} in all",
            if collections == 1 { "collection" } else { "collections" },
            millis(ms)
        ),
    });
}

/// Modules loaded for the first time in the worker that took at least
/// `IMPORT_MS` and `IMPORT_SHARE` of the request, naming the longest of
/// the outermost loads (issue 738).
fn imported(hints: &mut Vec<Hint>, record: &admin::RequestRecord, worker: &admin::WorkerPart) {
    if record.total_ms <= 0.0
        || worker.modules_ms < IMPORT_MS
        || worker.modules_ms < IMPORT_SHARE * record.total_ms
    {
        return;
    }

    let mut outermost: Vec<&admin::ModuleLoad> = worker
        .module_loads
        .iter()
        .filter(|load| load.depth == 0)
        .collect();
    outermost.sort_by(|one, other| other.ms.total_cmp(&one.ms));
    let longest: Vec<&str> = outermost
        .iter()
        .take(PER_KIND)
        .map(|load| load.module.as_str())
        .collect();

    let modules = if worker.modules_loaded == 1 {
        "1 module".to_owned()
    } else {
        format!("{} modules", worker.modules_loaded)
    };

    hints.push(Hint {
        text: format!(
            "the worker loaded {modules} for the first time during the request, {:.0}% of its time, which each worker does again for the first request that needs them; list the longest in preload_modules to load them before the worker serves: {}",
            worker.modules_ms * 100.0 / record.total_ms,
            longest.join(", ")
        ),
        panel: "Timing",
        figure: format!(
            "{} of {} loading modules",
            millis(worker.modules_ms),
            millis(record.total_ms)
        ),
    });
}

/// A request Muru answered itself, for a reason a setting governs.
fn answered(hints: &mut Vec<Hint>, record: &admin::RequestRecord) {
    let Some((reason, setting, flag)) = OWN_REASONS
        .iter()
        .find(|(reason, _, _)| *reason == record.reason)
    else {
        return;
    };

    let flag = if flag.is_empty() {
        String::new()
    } else {
        format!(" ({flag} for a run)")
    };

    hints.push(Hint {
        text: format!(
            "Muru answered the request itself, {} {reason}; the setting is {setting}{flag}",
            record.status
        ),
        panel: "Errors",
        figure: format!("{} {reason}", record.status),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(caption: &str, rows: &[&[&str]]) -> admin::PanelTable {
        admin::PanelTable {
            caption: caption.to_owned(),
            rows: rows
                .iter()
                .map(|cells| admin::PanelRow {
                    cells: cells.iter().map(|cell| (*cell).to_owned()).collect(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn hinted(record: &admin::RequestRecord, panels: &[admin::Panel]) -> Vec<Vec<String>> {
        let drawn = panel(record, panels);
        assert_eq!(drawn.tables[0].caption, "What stands out");
        drawn.tables[0]
            .rows
            .iter()
            .map(|row| row.cells.clone())
            .collect()
    }

    fn record() -> admin::RequestRecord {
        admin::RequestRecord {
            status: 200,
            total_ms: 100.0,
            from_worker: Some(admin::WorkerPart::default()),
            ..Default::default()
        }
    }

    #[test]
    fn a_statement_or_an_endpoint_repeated_is_a_hint_from_its_threshold() {
        let times = |n: usize| n.to_string();
        let (below, at, above) = (times(REPEATED - 1), times(REPEATED), times(REPEATED + 1));

        let sql = admin::Panel {
            name: "sql".to_owned(),
            tables: vec![table(
                "By statement",
                &[
                    &["SELECT a FROM t WHERE id = ?", &above, "6.0"],
                    &["SELECT b FROM t WHERE id = ?", &at, "5.0"],
                    &["SELECT c FROM t", &below, "4.0"],
                ],
            )],
            ..Default::default()
        };
        let outbound = admin::Panel {
            name: "outbound".to_owned(),
            tables: vec![table(
                "By endpoint",
                &[
                    &["GET", "api.test", "/authors", &at, "50.0"],
                    &["GET", "api.test", "/books", &below, "1.0"],
                ],
            )],
            ..Default::default()
        };

        let found = hinted(&record(), &[sql, outbound]);
        let texts: Vec<&str> = found.iter().map(|row| row[0].as_str()).collect();

        assert_eq!(found.len(), 3, "{found:?}");
        assert!(texts[0].ends_with(
            "6 times in one request, which an N+1 pattern does: SELECT a FROM t WHERE id = ?"
        ));
        assert!(texts[1].contains("SELECT b"));
        assert_eq!(
            found[2][0],
            "the same endpoint was called 5 times in one request, which an N+1 pattern does: GET api.test /authors"
        );
        assert_eq!(
            (found[2][1].as_str(), found[2][2].as_str()),
            ("Outbound", "5 times, 50ms in all")
        );
    }

    #[test]
    fn a_slow_query_or_call_is_a_hint_by_its_share_of_the_request() {
        let share = SLOW_SHARE * 100.0;
        let each = |rows: &[&[&str]]| admin::Panel {
            name: "sql".to_owned(),
            tables: vec![table("Each query", rows)],
            ..Default::default()
        };
        let (below, at) = (format!("{}", share - 0.5), format!("{share}"));

        // Over the share of a 100 ms request and at it; below the share,
        // and over it but under SLOW_MS in a short request.

        let found = hinted(
            &record(),
            &[each(&[
                &["1.0", &below, "SELECT 1"],
                &["2.0", &at, "SELECT 2"],
                &["3.0", "60.0", "SELECT 3"],
            ])],
        );
        let texts: Vec<&str> = found.iter().map(|row| row[0].as_str()).collect();
        assert_eq!(
            texts,
            [
                "one query took 60% of the request: SELECT 3",
                "one query took 25% of the request: SELECT 2"
            ]
        );

        let mut short = record();
        short.total_ms = SLOW_MS;
        let below_ms = format!("{}", SLOW_MS - 1.0);
        assert!(hinted(&short, &[each(&[&["0.0", &below_ms, "SELECT 4"]])]).is_empty());
        assert_eq!(
            hinted(
                &short,
                &[each(&[&["0.0", &format!("{SLOW_MS}"), "SELECT 5"]])]
            )
            .len(),
            1
        );
    }

    #[test]
    fn an_uncompressed_page_is_a_hint_with_the_reason() {
        let page = |bytes: u64, kind: &str, skipped: &str| {
            let mut found = record();
            found.bytes = bytes;
            found.response_headers = vec![admin::RecordedHeader {
                name: "content-type".to_owned(),
                value: kind.to_owned(),
            }];
            found.compression = Some(admin::RequestCompression {
                compressed: skipped.is_empty(),
                skipped: skipped.to_owned(),
                ..Default::default()
            });
            hinted(&found, &[])
        };

        let found = page(UNCOMPRESSED_BYTES, "text/html; charset=utf-8", "off");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0][0],
            "a 1024B text/html response went out uncompressed: compression is off for the route"
        );
        assert_eq!(found[0][1], "Response");
        assert_eq!(
            page(
                UNCOMPRESSED_BYTES + 1,
                "application/problem+json",
                "opted_out"
            )
            .len(),
            1
        );
        assert!(page(UNCOMPRESSED_BYTES - 1, "text/html", "off").is_empty());
        assert!(page(50_000, "image/png", "type").is_empty());
        assert!(page(50_000, "text/html", "small").is_empty());
        assert!(page(50_000, "text/html", "").is_empty(), "compressed");
    }

    #[test]
    fn the_event_loop_held_is_a_hint_naming_the_span_then_running() {
        let held = |ms: f64| {
            let mut found = record();
            found.queue_ms = 2.0;
            let worker = found.from_worker.as_mut().unwrap();
            worker.loop_lag = ms;
            worker.loop_lag_at = 10.0;
            let trace = admin::Panel {
                name: "trace".to_owned(),
                spans: vec![admin::PanelSpan {
                    name: "GET /report".to_owned(),
                    start_ms: 2.0,
                    duration_ms: 400.0,
                    children: vec![admin::PanelSpan {
                        name: "report.totals".to_owned(),
                        start_ms: 11.0,
                        duration_ms: 300.0,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };
            hinted(&found, &[trace])
        };

        assert!(held(LOOP_HELD_MS - 1.0).is_empty());
        let found = held(LOOP_HELD_MS);
        assert_eq!(found.len(), 1);
        assert!(
            found[0][0]
                .contains("held for 100ms during the request, while report.totals was running"),
            "{found:?}"
        );
        assert_eq!(
            (found[0][1].as_str(), found[0][2].as_str()),
            ("Timing, Trace", "100ms held, from 12ms")
        );
        assert_eq!(held(LOOP_HELD_MS + 1.0).len(), 1);
    }

    #[test]
    fn long_waits_and_collections_are_hints() {
        let waits = |queue: f64, thread: f64| {
            let mut found = record();
            found.queue_ms = queue;
            found.from_worker.as_mut().unwrap().thread_wait = thread;
            hinted(&found, &[])
        };

        assert!(waits(WAIT_MS - 1.0, WAIT_MS - 1.0).is_empty());
        let found = waits(WAIT_MS, WAIT_MS + 1.0);
        assert_eq!(found.len(), 2);
        assert!(found[0][0].contains("raise processes (--processes)"));
        assert!(found[1][0].contains("raise threads (--threads)"));

        let collected = |collections: u32, ms: f64| {
            let mut found = record();
            found.from_worker.as_mut().unwrap().gc = vec![admin::RequestGc {
                generation: "0".to_owned(),
                collections,
                ms,
            }];
            hinted(&found, &[])
        };

        assert!(collected(GC_COLLECTIONS - 1, GC_MS - 1.0).is_empty());
        assert_eq!(
            collected(GC_COLLECTIONS, 1.0)[0][2],
            "20 collections, 1.0ms in all"
        );
        assert_eq!(collected(1, GC_MS)[0][2], "1 collection, 50ms in all");
        assert_eq!(collected(GC_COLLECTIONS + 1, GC_MS + 1.0).len(), 1);
    }

    #[test]
    fn first_time_module_loads_are_a_hint_naming_the_longest() {
        let imported = |ms: f64| {
            let mut found = record();
            let worker = found.from_worker.as_mut().unwrap();
            let load = |module: &str, ms: f64, depth: u32| admin::ModuleLoad {
                module: module.to_owned(),
                ms,
                depth,
                ..Default::default()
            };
            worker.module_loads = vec![
                load("shop.reports", ms * 0.3, 0),
                load("openpyxl", ms * 0.7, 0),
                load("openpyxl.cell", ms * 0.5, 1),
            ];
            worker.modules_loaded = 3;
            worker.modules_ms = ms;
            hinted(&found, &[])
        };

        // The request's 100 ms: below either threshold is no hint.

        assert!(imported(IMPORT_MS - 1.0).is_empty());
        let found = imported(IMPORT_MS);
        assert_eq!(found.len(), 1);
        assert!(
            found[0][0]
                .contains("3 modules for the first time during the request, 50% of its time"),
            "{}",
            found[0][0]
        );
        assert!(found[0][0].ends_with(
            "preload_modules to load them before the worker serves: openpyxl, shop.reports"
        ));
        assert_eq!(found[0][2], "50ms of 100ms loading modules");
    }

    #[test]
    fn an_answer_of_muru_s_own_names_the_setting() {
        let answered = |status: u32, reason: &str| {
            let mut found = record();
            found.status = status;
            found.reason = reason.to_owned();
            found.from_worker = None;
            hinted(&found, &[])
        };

        let found = answered(504, "deadline");
        assert_eq!(
            found[0][0],
            "Muru answered the request itself, 504 deadline; the setting is request_deadline (--timeout deadline= for a run)"
        );
        assert_eq!(
            (found[0][1].as_str(), found[0][2].as_str()),
            ("Errors", "504 deadline")
        );
        assert!(answered(413, "too_many_files")[0][0].ends_with("the setting is max_files"));
        assert!(answered(500, "app_error").is_empty());
        assert!(answered(200, "").is_empty());
    }
}
