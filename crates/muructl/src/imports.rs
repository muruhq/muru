//! `muructl debug imports` and the MCP server's `imports` tool (section
//! 6.18): the import report of a release's load, and what its workers have
//! loaded since the fork.
//!
//! The host sends the report of its load, unwrapt's facts with Muru's
//! analysis of them, which the daemon keeps in its store beside the
//! release and a run in memory. Each worker answers with the modules it
//! has loaded for the first time since the fork, and with the report of
//! its own load when it loaded the application itself. This module turns
//! the two into one document, filtered as asked, and renders it as text.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use muru_proto::admin::v1 as pb;
use serde_json::{Value, json};

/// What `--target`, `--top`, `--min-ms`, `--package` and `--module` keep.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Only the imports made while this target loaded: `hooks`,
    /// `entrypoint`, `mount:NAME` or `module:NAME`, or `none` for those
    /// outside any.
    pub target: Option<String>,
    /// The most rows of each table.
    pub top: usize,
    /// The least a row's time must be, in milliseconds.
    pub min_ms: f64,
    /// Only modules, and importers, under these top-level packages.
    pub packages: Vec<String>,
    /// The chain of imports that brought this module in, in place of the
    /// tables.
    pub module: Option<String>,
}

/// The rows each table keeps unless `--top` says otherwise.
pub const DEFAULT_TOP: usize = 20;

impl Filters {
    /// Whether a module, or the importer of one, is under the packages
    /// asked for.
    fn under(&self, name: &str) -> bool {
        let package = name.split('.').next().unwrap_or(name);
        self.packages.is_empty() || self.packages.iter().any(|wanted| wanted == package)
    }

    /// Whether a row of the host's report is kept: its target, its time
    /// and its packages.
    fn keeps(&self, row: &Value, ms_key: &str) -> bool {
        let target = row["target"].as_str().unwrap_or_default();

        let wanted_target = match self.target.as_deref() {
            None => true,
            Some("none") => target.is_empty(),
            Some(asked) => asked == target,
        };

        let module = row["module"]
            .as_str()
            .or_else(|| row["imported"].as_str())
            .unwrap_or_default();
        let importer = row["importer"].as_str().unwrap_or_default();

        wanted_target
            && row[ms_key].as_f64().unwrap_or(0.0) >= self.min_ms
            && (self.under(module) || self.under(importer))
    }
}

/// The document `debug imports` prints and the MCP tool answers: the
/// report asked for, the host's or one worker's, with its rows filtered,
/// and the section on the workers.
pub fn document(
    response: &pb::DebugResponse,
    worker: Option<&str>,
    filters: &Filters,
) -> anyhow::Result<Value> {
    let workers: Vec<(String, Value)> = response
        .workers
        .iter()
        .filter(|reply| reply.ok)
        .filter_map(|reply| {
            let name = if reply.group.is_empty() || reply.group == "default" {
                reply.index.to_string()
            } else {
                format!("{}/{}", reply.group, reply.index)
            };

            serde_json::from_str(&reply.result_json)
                .ok()
                .map(|result| (name, result))
        })
        .collect();

    let failed: Vec<Value> = response
        .workers
        .iter()
        .filter(|reply| !reply.ok)
        .map(|reply| json!({"worker": reply.index, "group": reply.group, "error": reply.error}))
        .collect();

    // The host's report, or the worker's own when one is asked for; a
    // forked worker has none of its own.

    let host: Option<Value> = if response.host_json.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&response.host_json)?)
    };

    let report = match worker {
        Some(_) => workers
            .first()
            .and_then(|(_, result)| result.get("report"))
            .filter(|report| !report.is_null())
            .cloned(),
        None => host.clone(),
    };

    let of = match worker {
        Some(name) => format!("worker {name}"),
        None => "the host".to_owned(),
    };

    let mut found = json!({
        "release": response.release,
        "of": of,
        "report": report.as_ref().map(|report| analysis(report, filters)),
        "workers": workers_section(&workers, host.as_ref(), filters),
    });

    if !failed.is_empty() {
        found["unanswered"] = Value::Array(failed);
    }

    Ok(found)
}

/// A report's analysis, its rows filtered, without unwrapt's raw edges
/// except as the chain `--module` asks for.
fn analysis(report: &Value, filters: &Filters) -> Value {
    let top = if filters.top == 0 {
        DEFAULT_TOP
    } else {
        filters.top
    };

    let rows = |key: &str, ms_key: &str| -> Value {
        Value::Array(
            report[key]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter(|row| filters.keeps(row, ms_key))
                        .take(top)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        )
    };

    // A deferred import never used has no time, so only the packages
    // apply to it.

    let mut found = json!({
        "labels": report["labels"],
        "workers": report["workers"],
        "summary": report["summary"],
        "findings": rows("findings", "ms"),
        "candidates": rows("candidates", "cum_ms"),
        "deferred_used": rows("deferred_used", "load_ms"),
        "deferred_unused": Value::Array(
            report["deferred_unused"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter(|row| {
                            filters.under(row["module"].as_str().unwrap_or_default())
                                || filters.under(row["importer"].as_str().unwrap_or_default())
                        })
                        .take(top)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        ),
    });

    if let Some(module) = &filters.module {
        found["chain"] = chain(report, module);
    }

    found
}

/// The import calls that brought `module` in, outermost first, from the
/// raw edges: the first call that loaded it, then each call it ran
/// inside. Matched by its full name, else by its top-level package, as
/// unwrapt's own chains are; the best cut is unwrapt's when its chains
/// include the package.
fn chain(report: &Value, module: &str) -> Value {
    let edges = report["report"]["edges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let by_id: BTreeMap<i64, &Value> = edges
        .iter()
        .filter_map(|edge| Some((edge["id"].as_i64()?, edge)))
        .collect();
    let label_sets = report["report"]["label_sets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let package = module.split('.').next().unwrap_or(module);

    let loading = |exact: bool| {
        edges
            .iter()
            .filter(|edge| edge["loaded"].as_bool() == Some(true))
            .filter(|edge| {
                let imported = edge["imported"].as_str().unwrap_or_default();

                if exact {
                    imported == module
                } else {
                    imported.split('.').next() == Some(package)
                }
            })
            .min_by_key(|edge| edge["id"].as_i64().unwrap_or(i64::MAX))
    };

    let Some(first) = loading(true).or_else(|| loading(false)) else {
        return json!({"module": module, "links": null, "best_cut": null});
    };

    let mut links = Vec::new();
    let mut current = Some(first);

    while let Some(edge) = current {
        let index = edge["labels"].as_u64().unwrap_or(0) as usize;
        let target = label_sets
            .get(index)
            .and_then(|labels| labels["target"].as_str())
            .unwrap_or_default();

        links.push(json!({
            "edge": edge["id"],
            "importer": edge["importer"],
            "imported": edge["imported"],
            "file": edge["file"],
            "line": edge["line"],
            "cum_ms": edge["cum_ms"],
            "self_ms": edge["self_ms"],
            "kind": edge["kind"],
            "native": edge["defer"]["native"],
            "unwrapt": edge["defer"]["unwrapt"],
            "target": target,
        }));

        current = edge["parent_id"]
            .as_i64()
            .and_then(|parent| by_id.get(&parent).copied());
    }

    links.reverse();

    let best_cut = report["chains"]
        .as_array()
        .and_then(|chains| {
            chains
                .iter()
                .find(|found| found["module"].as_str() == Some(package))
        })
        .map_or(Value::Null, |found| found["best_cut"].clone());

    json!({"module": module, "links": links, "best_cut": best_cut})
}

/// The section on the workers: each module they loaded for the first
/// time since the fork, how many of the workers loaded it, the first
/// request to, its time and what to do about it; and the deferred
/// imports of the host's load that no worker has used.
fn workers_section(workers: &[(String, Value)], host: Option<&Value>, filters: &Filters) -> Value {
    #[derive(Default)]
    struct Seen {
        workers: Vec<String>,
        first: Option<(u64, String, String)>,
        ms: Vec<f64>,
        deferred: bool,
        depth: u64,
    }

    let mut modules: BTreeMap<String, Seen> = BTreeMap::new();

    for (name, result) in workers {
        let loads = result["modules"]["loads"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        for load in &loads {
            let Some(module) = load["module"].as_str() else {
                continue;
            };

            let seen = modules.entry(module.to_owned()).or_insert_with(|| Seen {
                depth: u64::MAX,
                ..Seen::default()
            });
            let at = load["at_unix_ms"].as_u64().unwrap_or(u64::MAX);
            let request = load["request"].as_str().unwrap_or_default().to_owned();

            if seen
                .first
                .as_ref()
                .is_none_or(|(earliest, _, _)| at < *earliest)
            {
                seen.first = Some((at, request, name.clone()));
            }

            seen.workers.push(name.clone());
            seen.ms.push(load["ms"].as_f64().unwrap_or(0.0));
            seen.deferred |= load["deferred"].as_bool().unwrap_or(false);
            seen.depth = seen.depth.min(load["depth"].as_u64().unwrap_or(0));
        }
    }

    let asked = workers.len();
    let mut rows: Vec<Value> = modules
        .into_iter()
        .filter(|(module, seen)| {
            let ms = seen.ms.iter().copied().fold(0.0, f64::max);

            // A module loaded only inside another's load comes with it,
            // and Muru's own modules are not the application's to move.

            (seen.depth == 0 || filters.module.is_some())
                && !module.starts_with("muru.")
                && filters.under(module)
                && ms >= filters.min_ms
                && filters
                    .module
                    .as_ref()
                    .is_none_or(|wanted| wanted == module)
        })
        .map(|(module, seen)| {
            let ms = seen.ms.iter().sum::<f64>() / seen.ms.len().max(1) as f64;
            let advice = worker_advice(seen.workers.len(), asked, seen.deferred, &module);
            let (at, request, worker) = seen.first.unwrap_or_default();

            json!({
                "module": module,
                "loaded_by": seen.workers.len(),
                "of": asked,
                "first_request": request,
                "first_worker": worker,
                "first_at_unix_ms": at,
                "ms": ms,
                "outermost": seen.depth == 0,
                "deferred": seen.deferred,
                "advice": advice,
            })
        })
        .collect();

    rows.sort_by(|left, right| {
        right["ms"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&left["ms"].as_f64().unwrap_or(0.0))
    });

    // A deferral the host's load made that no worker has used saves its
    // module in every worker.

    let loaded: Vec<String> = rows
        .iter()
        .filter_map(|row| row["module"].as_str().map(str::to_owned))
        .collect();
    let never: Vec<Value> = host
        .and_then(|host| host["deferred_unused"].as_array())
        .map(|unused| {
            unused
                .iter()
                .filter(|row| {
                    let module = row["module"].as_str().unwrap_or_default();
                    !loaded.iter().any(|seen| seen == module) && filters.under(module)
                })
                .map(|row| {
                    json!({
                        "module": row["module"],
                        "importer": row["importer"],
                        "advice": "never needed: deferring it saves it in every worker",
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let top = if filters.top == 0 {
        DEFAULT_TOP
    } else {
        filters.top
    };
    rows.truncate(top);

    json!({
        "asked": asked,
        "modules": rows,
        "never_used": never,
    })
}

/// What to do about a module `loaded_by` of `asked` workers loaded on
/// first use.
fn worker_advice(loaded_by: usize, asked: usize, deferred: bool, module: &str) -> String {
    if loaded_by >= asked && asked > 1 {
        if deferred {
            return format!(
                "every worker loads it, so deferring it gains nothing: load it before the fork with preload_modules = [\"{module}\"]"
            );
        }

        return format!(
            "every worker loads it: load it before the fork with preload_modules = [\"{module}\"]"
        );
    }

    if loaded_by >= asked {
        return format!(
            "the worker loaded it on first use: load it before the fork with preload_modules = [\"{module}\"] if most requests need it"
        );
    }

    if deferred {
        "deferral pays off: only some workers load it".to_owned()
    } else {
        "loading on first use pays off: only some workers load it".to_owned()
    }
}

/// The document as text.
pub fn render(document: &Value) -> String {
    let mut out = String::new();
    let release = document["release"].as_str().unwrap_or_default();
    let of = document["of"].as_str().unwrap_or_default();

    match document.get("report").filter(|report| !report.is_null()) {
        Some(report) => render_report(&mut out, report, release, of),
        None => {
            let _ = writeln!(
                out,
                "No import report of {of}'s load of release {release}: turn on import_report, \
                 or run muructl check --imports"
            );
        }
    }

    render_workers(&mut out, &document["workers"]);

    if let Some(unanswered) = document["unanswered"].as_array() {
        for worker in unanswered {
            let _ = writeln!(
                out,
                "worker {} did not answer: {}",
                worker["worker"],
                worker["error"].as_str().unwrap_or_default()
            );
        }
    }

    out
}

/// A millisecond figure, to one decimal place.
fn ms(value: &Value) -> String {
    format!("{:.1}", value.as_f64().unwrap_or(0.0))
}

/// A text field, or `-` when empty.
fn text(value: &Value) -> String {
    match value.as_str() {
        Some(found) if !found.is_empty() => found.to_owned(),
        _ => "-".to_owned(),
    }
}

/// Rows set out in columns, the numbers right-aligned.
fn table(out: &mut String, header: &[&str], rows: &[Vec<String>], numeric: &[bool]) {
    let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let line = |cells: Vec<String>| -> String {
        let mut line = String::from(" ");

        for (index, cell) in cells.iter().enumerate() {
            let pad = widths[index].saturating_sub(cell.chars().count());

            if numeric[index] {
                line.push_str(&" ".repeat(pad + 1));
                line.push_str(cell);
            } else {
                line.push(' ');
                line.push_str(cell);
                line.push_str(&" ".repeat(pad));
            }
        }

        line.trim_end().to_owned()
    };

    let _ = writeln!(
        out,
        "{}",
        line(header.iter().map(|title| (*title).to_owned()).collect())
    );

    for row in rows {
        let _ = writeln!(out, "{}", line(row.clone()));
    }
}

fn render_report(out: &mut String, report: &Value, release: &str, of: &str) {
    let labels = &report["labels"];
    let summary = &report["summary"];

    let workers = report["workers"].as_u64().unwrap_or(1);

    let _ = writeln!(
        out,
        "Import report of {of}'s load of {}@{release}: outcome {}, Python {}, {workers} {}",
        text(&labels["app"]),
        text(&labels["outcome"]),
        text(&labels["python"]),
        if workers == 1 { "worker" } else { "workers" },
    );
    let _ = writeln!(
        out,
        "  {} ms importing, {} modules loaded, {} import calls; deferring what can be deferred would save about {} ms",
        ms(&summary["total_import_ms"]),
        summary["modules_loaded"].as_u64().unwrap_or(0),
        summary["import_calls"].as_u64().unwrap_or(0),
        ms(&summary["estimated_saving_ms"]),
    );

    // --module: the chain that brought the module in.

    if let Some(chain) = report.get("chain") {
        render_chain(out, chain);
        return;
    }

    if let Some(findings) = report["findings"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(out, "\nAdvice");

        for finding in findings {
            let kind = match finding["kind"].as_str().unwrap_or_default() {
                "fork_safety" => "fork safety",
                "used_before_fork" => "used before the fork",
                _ => "defer",
            };

            let target = match finding["target"].as_str() {
                Some(target) if !target.is_empty() => format!(", loading {target}"),
                _ => String::new(),
            };

            let _ = writeln!(
                out,
                "  {kind}: {} imported by {}, {} ms{target}",
                text(&finding["module"]),
                text(&finding["importer"]),
                ms(&finding["ms"]),
            );
            let _ = writeln!(out, "    paid by {}", text(&finding["pays"]));
            let _ = writeln!(out, "    {}", text(&finding["advice"]));

            if let Some(config) = finding["config"]
                .as_str()
                .filter(|config| !config.is_empty())
            {
                for line in config.lines() {
                    let _ = writeln!(out, "      {line}");
                }
            }
        }
    }

    if let Some(candidates) = report["candidates"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(out, "\nImports that loaded a module, by cumulative time");
        let rows: Vec<Vec<String>> = candidates
            .iter()
            .map(|row| {
                vec![
                    ms(&row["cum_ms"]),
                    ms(&row["saving_ms"]),
                    text(&row["imported"]),
                    text(&row["importer"]),
                    text(&row["target"]),
                    yes(&row["native"]),
                    yes(&row["unwrapt"]),
                    if row["already_lazy"].as_bool() == Some(true) {
                        "already lazy".to_owned()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect();

        table(
            out,
            &[
                "CUM MS",
                "SAVES MS",
                "IMPORTED",
                "IMPORTED BY",
                "LOADING",
                "3.15",
                "UNWRAPT",
                "NOTES",
            ],
            &rows,
            &[true, true, false, false, false, false, false, false],
        );
    }

    if let Some(used) = report["deferred_used"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(out, "\nDeferred but used during the load");
        let rows: Vec<Vec<String>> = used
            .iter()
            .map(|row| {
                vec![
                    ms(&row["load_ms"]),
                    text(&row["module"]),
                    text(&row["importer"]),
                    text(&row["used_in"]),
                    row["line"]
                        .as_u64()
                        .map_or("-".to_owned(), |line| line.to_string()),
                ]
            })
            .collect();

        table(
            out,
            &["LOAD MS", "MODULE", "DEFERRED IN", "USED IN", "LINE"],
            &rows,
            &[true, false, false, false, true],
        );
    }

    if let Some(unused) = report["deferred_unused"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(out, "\nDeferred and not used during the load");
        let rows: Vec<Vec<String>> = unused
            .iter()
            .map(|row| vec![text(&row["module"]), text(&row["importer"])])
            .collect();

        table(out, &["MODULE", "DEFERRED IN"], &rows, &[false, false]);
    }
}

fn yes(value: &Value) -> String {
    if value.as_bool() == Some(true) {
        "yes".to_owned()
    } else {
        "no".to_owned()
    }
}

fn render_chain(out: &mut String, chain: &Value) {
    let module = text(&chain["module"]);

    let Some(links) = chain["links"].as_array() else {
        let _ = writeln!(out, "\nNothing in the load loaded {module}.");
        return;
    };

    let _ = writeln!(out, "\nThe imports that loaded {module}, outermost first");
    let best = chain["best_cut"].as_i64();
    let rows: Vec<Vec<String>> = links
        .iter()
        .map(|link| {
            let place = match (link["file"].as_str(), link["line"].as_u64()) {
                (Some(file), Some(line)) => format!("{file}:{line}"),
                (Some(file), None) => file.to_owned(),
                _ => "-".to_owned(),
            };

            vec![
                ms(&link["cum_ms"]),
                format!("{} -> {}", text(&link["importer"]), text(&link["imported"])),
                text(&link["target"]),
                place,
                if link["edge"].as_i64() == best && best.is_some() {
                    "best cut".to_owned()
                } else {
                    String::new()
                },
            ]
        })
        .collect();

    table(
        out,
        &["CUM MS", "IMPORT", "LOADING", "AT", "NOTES"],
        &rows,
        &[true, false, false, false, false],
    );
}

fn render_workers(out: &mut String, workers: &Value) {
    let asked = workers["asked"].as_u64().unwrap_or(0);

    if asked == 0 {
        return;
    }

    let modules = workers["modules"].as_array().cloned().unwrap_or_default();
    let never = workers["never_used"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let noun = if asked == 1 { "worker" } else { "workers" };

    if modules.is_empty() && never.is_empty() {
        let _ = writeln!(
            out,
            "\nThe {asked} {noun} asked have loaded no module for the first time since the fork."
        );
        return;
    }

    let _ = writeln!(
        out,
        "\nModules the {asked} {noun} asked loaded for the first time since the fork"
    );
    let rows: Vec<Vec<String>> = modules
        .iter()
        .map(|row| {
            vec![
                format!(
                    "{}{}",
                    text(&row["module"]),
                    if row["deferred"].as_bool() == Some(true) {
                        " (deferred)"
                    } else {
                        ""
                    }
                ),
                format!(
                    "{} of {}",
                    row["loaded_by"].as_u64().unwrap_or(0),
                    row["of"].as_u64().unwrap_or(0)
                ),
                text(&row["first_request"]),
                ms(&row["ms"]),
                text(&row["advice"]),
            ]
        })
        .collect();

    table(
        out,
        &["MODULE", "LOADED BY", "FIRST REQUEST", "MS", "ADVICE"],
        &rows,
        &[false, false, false, true, false],
    );

    for row in &never {
        let _ = writeln!(
            out,
            "  {} (deferred in {}): {}",
            text(&row["module"]),
            text(&row["importer"]),
            text(&row["advice"])
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Value {
        json!({
            "labels": {"app": "shop", "outcome": "fork", "python": "3.15.0", "role": "host"},
            "workers": 2,
            "summary": {"total_import_ms": 50.0, "modules_loaded": 9, "import_calls": 40, "estimated_saving_ms": 30.0},
            "findings": [{"kind": "defer", "module": "openpyxl", "importer": "shop.reports", "target": "entrypoint",
                          "ms": 30.0, "saving_ms": 30.0, "pays": "the host", "advice": "add it",
                          "config": "__lazy_modules__ = ['openpyxl']"}],
            "candidates": [
                {"edge": 2, "imported": "openpyxl", "importer": "shop.reports", "target": "entrypoint",
                 "cum_ms": 30.0, "saving_ms": 30.0, "native": true, "unwrapt": true, "already_lazy": false},
                {"edge": 4, "imported": "yaml", "importer": "shop.hooks", "target": "hooks",
                 "cum_ms": 0.5, "saving_ms": 0.5, "native": true, "unwrapt": false, "already_lazy": false}
            ],
            "deferred_used": [],
            "deferred_unused": [{"module": "pandas", "importer": "shop.views"}],
            "chains": [{"module": "openpyxl", "best_cut": 2}],
            "report": {
                "label_sets": [{}, {"target": "entrypoint"}],
                "edges": [
                    {"id": 1, "parent_id": null, "importer": "shop", "imported": "shop.reports", "loaded": true,
                     "cum_ms": 31.0, "self_ms": 1.0, "labels": 1, "file": "shop/__init__.py", "line": 1,
                     "defer": {"native": true, "unwrapt": true}},
                    {"id": 2, "parent_id": 1, "importer": "shop.reports", "imported": "openpyxl", "loaded": true,
                     "cum_ms": 30.0, "self_ms": 30.0, "labels": 1, "file": "shop/reports.py", "line": 3,
                     "defer": {"native": true, "unwrapt": true}}
                ]
            }
        })
    }

    fn response(workers: &[(u32, Value)]) -> pb::DebugResponse {
        pb::DebugResponse {
            workers: workers
                .iter()
                .map(|(index, result)| pb::WorkerDebug {
                    index: *index,
                    pid: 100 + index,
                    ok: true,
                    error: String::new(),
                    result_json: result.to_string(),
                    group: "default".to_owned(),
                })
                .collect(),
            release: "r1".to_owned(),
            host_json: host().to_string(),
        }
    }

    fn loads(modules: &[(&str, &str, bool)]) -> Value {
        json!({
            "modules": {
                "loads": modules
                    .iter()
                    .map(|(module, request, deferred)| json!({
                        "module": module, "request": request, "ms": 12.0, "depth": 0,
                        "deferred": deferred, "at_unix_ms": 1000
                    }))
                    .collect::<Vec<_>>(),
                "loaded": modules.len(),
            },
            "report": null,
        })
    }

    #[test]
    fn the_host_report_is_filtered_and_the_workers_loads_are_counted() {
        let answered = response(&[
            (
                0,
                loads(&[("shop.export", "R1", false), ("pandas.io", "R2", true)]),
            ),
            (1, loads(&[("shop.export", "R3", false)])),
        ]);
        let filters = Filters {
            target: Some("entrypoint".to_owned()),
            ..Filters::default()
        };
        let found = document(&answered, None, &filters).unwrap();

        // --target keeps the entrypoint's imports alone.

        let candidates = found["report"]["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["imported"], "openpyxl");

        // Both workers loaded shop.export, one of them the deferred
        // pandas.io, and no worker the deferred pandas.

        let modules = found["workers"]["modules"].as_array().unwrap();
        let export = modules
            .iter()
            .find(|row| row["module"] == "shop.export")
            .unwrap();
        assert_eq!(
            (export["loaded_by"].as_u64(), export["of"].as_u64()),
            (Some(2), Some(2))
        );
        assert!(
            export["advice"]
                .as_str()
                .unwrap()
                .contains("preload_modules = [\"shop.export\"]")
        );
        let pandas_io = modules
            .iter()
            .find(|row| row["module"] == "pandas.io")
            .unwrap();
        assert_eq!(
            pandas_io["advice"],
            "deferral pays off: only some workers load it"
        );
        assert_eq!(found["workers"]["never_used"][0]["module"], "pandas");

        let text = render(&found);
        assert!(
            text.contains("Import report of the host's load of shop@r1"),
            "{text}"
        );
        assert!(text.contains("__lazy_modules__ = ['openpyxl']"), "{text}");
        assert!(text.contains("pandas.io (deferred)"), "{text}");
    }

    #[test]
    fn the_chain_of_a_module_is_walked_from_the_edges() {
        let filters = Filters {
            module: Some("openpyxl".to_owned()),
            ..Filters::default()
        };
        let found = document(&response(&[]), None, &filters).unwrap();
        let chain = &found["report"]["chain"];

        assert_eq!(chain["links"][0]["imported"], "shop.reports");
        assert_eq!(chain["links"][1]["imported"], "openpyxl");
        assert_eq!(chain["best_cut"], 2);

        let text = render(&found);
        assert!(text.contains("shop.reports -> openpyxl"), "{text}");
        assert!(text.contains("best cut"), "{text}");
    }

    #[test]
    fn without_a_report_it_says_how_to_get_one() {
        let mut answered = response(&[]);
        answered.host_json.clear();
        let text = render(&document(&answered, None, &Filters::default()).unwrap());

        assert!(text.contains("turn on import_report"), "{text}");
    }
}
