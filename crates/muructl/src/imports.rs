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
        "targets": report["targets"],
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

    // Each worker's loads with the load each was nested in, for a private
    // module's nearest public ancestor (issue 763).

    let mut parents: BTreeMap<(String, String), String> = BTreeMap::new();

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
            parents.insert(
                (name.clone(), module.to_owned()),
                load["parent"].as_str().unwrap_or_default().to_owned(),
            );

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

    // Issue 793: the imports the host's load deferred, each of which the
    // section says how many workers have loaded, so a deferral that pays
    // off shows as well as one that does not.

    let host_deferred: BTreeMap<String, String> = host
        .and_then(|host| host["deferred_unused"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row["module"].as_str()?.to_owned(),
                row["importer"].as_str().unwrap_or_default().to_owned(),
            ))
        })
        .collect();

    let mut rows: Vec<Value> = modules
        .into_iter()
        .filter(|(module, seen)| {
            let ms = seen.ms.iter().copied().fold(0.0, f64::max);

            // A module loaded only inside another's load comes with it,
            // unless the host deferred it, and Muru's own modules are not
            // the application's to move.

            (seen.depth == 0 || filters.module.is_some() || host_deferred.contains_key(module))
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
            let (at, request, worker) = seen.first.unwrap_or_default();
            let listed = public_module(&module, &worker, &parents);

            let advice = if host_deferred.contains_key(&module) {
                deferral_advice(seen.workers.len(), asked, &module, listed.as_deref())
            } else {
                worker_advice(
                    seen.workers.len(),
                    asked,
                    seen.deferred,
                    &module,
                    listed.as_deref(),
                )
            };

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

    // A deferral the host's load made that no worker has loaded is paying
    // off, so far.

    let loaded: Vec<String> = rows
        .iter()
        .filter_map(|row| row["module"].as_str().map(str::to_owned))
        .collect();
    let never: Vec<Value> = host_deferred
        .iter()
        .filter(|(module, _)| !loaded.contains(module) && filters.under(module))
        .map(|(module, importer)| {
            json!({
                "module": module,
                "importer": importer,
                "advice": deferral_advice(0, asked, module, Some(module)),
            })
        })
        .collect();

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

/// Whether a module is private: a part of its name starts with an
/// underscore, as `_osx_support` or `_sysconfigdata_t_darwin_darwin`,
/// whose names often change with the platform and the build.
fn private(module: &str) -> bool {
    module.split('.').any(|part| part.starts_with('_'))
}

/// The module to list in `preload_modules` for one a worker loaded: the
/// module itself, or for a private one the nearest public module whose
/// load it was part of in that worker; none when there is none (issue
/// 763).
fn public_module(
    module: &str,
    worker: &str,
    parents: &BTreeMap<(String, String), String>,
) -> Option<String> {
    let mut current = module.to_owned();

    // A chain of loads is as deep as the imports nest, which a bound
    // keeps finite whatever the answers say.

    for _ in 0..64 {
        if !private(&current) {
            return Some(current);
        }

        current = parents
            .get(&(worker.to_owned(), current))
            .filter(|parent| !parent.is_empty())?
            .clone();
    }

    None
}

/// What to do about a module `loaded_by` of `asked` workers loaded on
/// first use, with `listed` the module to name in `preload_modules`: the
/// module itself, the public module that loaded a private one, or none.
fn worker_advice(
    loaded_by: usize,
    asked: usize,
    deferred: bool,
    module: &str,
    listed: Option<&str>,
) -> String {
    let preload = match listed {
        Some(listed) if listed == module => {
            format!("load it before the fork with preload_modules = [\"{listed}\"]")
        }
        Some(listed) => format!(
            "load it before the fork with preload_modules = [\"{listed}\"], whose load loads it"
        ),
        None => String::new(),
    };

    if loaded_by >= asked && asked > 1 {
        return match (deferred, preload.is_empty()) {
            (_, true) => {
                "every worker loads it; a private module is not one to list in preload_modules"
                    .to_owned()
            }
            (true, false) => {
                format!("every worker loads it, so deferring it gains nothing: {preload}")
            }
            (false, false) => format!("every worker loads it: {preload}"),
        };
    }

    if loaded_by >= asked {
        if preload.is_empty() {
            return "the worker loaded it on first use; a private module is not one to list in preload_modules".to_owned();
        }

        return format!("the worker loaded it on first use: {preload} if most requests need it");
    }

    if deferred {
        "deferral pays off: only some workers load it".to_owned()
    } else {
        "loading on first use pays off: only some workers load it".to_owned()
    }
}

/// What to say of an import the host's load deferred that `loaded_by`
/// of `asked` workers have loaded (issue 793): none, paying off; some,
/// loading on first use pays off; every one, better loaded before the
/// fork, naming `listed` in `preload_modules` when there is one.
fn deferral_advice(loaded_by: usize, asked: usize, module: &str, listed: Option<&str>) -> String {
    let noun = if asked == 1 { "worker" } else { "workers" };
    let counted = format!("deferred in the host; loaded by {loaded_by} of {asked} {noun}");

    if loaded_by == 0 {
        return format!("{counted}: the deferral is paying off");
    }

    if loaded_by < asked {
        return format!("{counted}: loading on first use pays off: only some workers load it");
    }

    match listed {
        Some(listed) if listed == module => {
            format!("{counted}: load it before the fork with preload_modules = [\"{listed}\"]")
        }
        Some(listed) => format!(
            "{counted}: load it before the fork with preload_modules = [\"{listed}\"], whose load loads it"
        ),
        None => format!("{counted}, so deferring it gains nothing"),
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

    // Issue 785: each of the preload_modules' own cost, as Muru timed
    // its load, for someone weighing it against the reload time.

    for target in report["targets"].as_array().into_iter().flatten() {
        if let Some(module) = target["target"]
            .as_str()
            .and_then(|label| label.strip_prefix("module:"))
        {
            let _ = writeln!(
                out,
                "  preload_modules entry {module}: {} ms in {of}",
                ms(&target["ms"])
            );
        }
    }

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
                    notes(row),
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

/// What a row of the imports table notes: an import already deferred,
/// and one made while the host loaded one of the `preload_modules`,
/// which is not advised to be deferred (issue 785).
fn notes(row: &Value) -> String {
    let mut notes = Vec::new();

    if row["already_lazy"].as_bool() == Some(true) {
        notes.push("already lazy");
    }

    if row["target"]
        .as_str()
        .is_some_and(|target| target.starts_with("module:"))
    {
        notes.push("preload_modules");
    }

    notes.join(", ")
}

/// Text with its first letter a capital.
fn capitalised(text: &str) -> String {
    let mut characters = text.chars();

    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
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

    let answered = if asked == 1 {
        "the worker that answered".to_owned()
    } else {
        format!("the {asked} {noun} that answered")
    };

    if modules.is_empty() && never.is_empty() {
        let have = if asked == 1 { "has" } else { "have" };
        let _ = writeln!(
            out,
            "\n{} {have} loaded no module for the first time since the fork.",
            capitalised(&answered)
        );
        return;
    }

    let _ = writeln!(
        out,
        "\nModules loaded for the first time since the fork, by {answered}"
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
            "  {}, imported by {}: {}",
            text(&row["module"]),
            text(&row["importer"]),
            text(&row["advice"])
        );
    }

    // The counts grow as the workers serve.

    let deferrals = !never.is_empty()
        || modules.iter().any(|row| {
            row["advice"]
                .as_str()
                .is_some_and(|advice| advice.starts_with("deferred in the host"))
        });

    if deferrals {
        let _ = writeln!(
            out,
            "  The counts cover the requests served so far; read them after representative traffic."
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
                 "cum_ms": 0.5, "saving_ms": 0.5, "native": true, "unwrapt": false, "already_lazy": false},
                {"edge": 5, "imported": "pandas", "importer": "shop.stats", "target": "module:shop.stats",
                 "cum_ms": 178.0, "saving_ms": 178.0, "native": true, "unwrapt": true, "already_lazy": false}
            ],
            "targets": [{"target": "entrypoint", "ms": 40.0}, {"target": "module:shop.stats", "ms": 178.8}],
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
    fn a_preload_modules_entry_has_its_cost_and_its_imports_are_noted() {
        let found = document(&response(&[]), None, &Filters::default()).unwrap();
        let text = render(&found);

        // Issue 785: the entry's own time on a line of its own, and the
        // imports made while loading it noted as the list's.

        assert!(
            text.contains("\n  preload_modules entry shop.stats: 178.8 ms in the host\n"),
            "{text}"
        );
        let row = text.lines().find(|line| line.contains(" pandas ")).unwrap();
        assert!(
            row.ends_with("module:shop.stats yes  yes     preload_modules"),
            "{row}"
        );
    }

    #[test]
    fn a_private_module_is_suggested_through_its_public_ancestor_or_not_at_all() {
        let worker = json!({
            "modules": {
                "loads": [
                    {"module": "_sysconfigdata_t_darwin_darwin", "request": "R1", "ms": 0.5, "depth": 0,
                     "deferred": false, "at_unix_ms": 1000, "parent": ""},
                    {"module": "urllib.request", "request": "R1", "ms": 5.0, "depth": 0,
                     "deferred": false, "at_unix_ms": 1001, "parent": ""},
                    {"module": "_osx_support", "request": "R1", "ms": 0.1, "depth": 1,
                     "deferred": false, "at_unix_ms": 1002, "parent": "urllib.request"}
                ],
                "loaded": 3,
            },
            "report": null,
        });
        let answered = response(&[(0, worker.clone()), (1, worker)]);
        let advice = |module: &str| {
            let filters = Filters {
                module: Some(module.to_owned()),
                ..Filters::default()
            };
            let found = document(&answered, None, &filters).unwrap();
            found["workers"]["modules"][0]["advice"]
                .as_str()
                .unwrap()
                .to_owned()
        };

        // Issue 763: a private module is never listed; the public module
        // whose load it was part of is, and with none there is no entry.

        assert_eq!(
            advice("_osx_support"),
            "every worker loads it: load it before the fork with preload_modules = [\"urllib.request\"], whose load loads it"
        );
        assert_eq!(
            advice("_sysconfigdata_t_darwin_darwin"),
            "every worker loads it; a private module is not one to list in preload_modules"
        );

        // The heading says whose loads they are.

        let text = render(&document(&answered, None, &Filters::default()).unwrap());
        assert!(
            text.contains(
                "\nModules loaded for the first time since the fork, by the 2 workers that answered\n"
            ),
            "{text}"
        );
        let one = response(&[(0, loads(&[]))]);
        let nothing = Filters {
            packages: vec!["absent".to_owned()],
            ..Filters::default()
        };
        let text = render(&document(&one, None, &nothing).unwrap());
        assert!(
            text.contains(
                "The worker that answered has loaded no module for the first time since the fork."
            ),
            "{text}"
        );
    }

    #[test]
    fn a_deferral_the_host_made_says_how_many_workers_loaded_it() {
        let mut answered = response(&[
            (0, loads(&[("xlsx", "R1", true), ("charts", "R2", true)])),
            (1, loads(&[("xlsx", "R3", true)])),
        ]);
        let mut host = host();
        host["deferred_unused"] = json!([
            {"module": "pandas", "importer": "shop.views"},
            {"module": "xlsx", "importer": "shop.export"},
            {"module": "charts", "importer": "shop.views"}
        ]);
        answered.host_json = host.to_string();
        let found = document(&answered, None, &Filters::default()).unwrap();
        let advice = |module: &str| {
            found["workers"]["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["module"] == module)
                .map(|row| row["advice"].as_str().unwrap().to_owned())
        };

        // Issue 793: every worker, some, or none.

        assert_eq!(
            advice("xlsx").unwrap(),
            "deferred in the host; loaded by 2 of 2 workers: load it before the fork with preload_modules = [\"xlsx\"]"
        );
        assert_eq!(
            advice("charts").unwrap(),
            "deferred in the host; loaded by 1 of 2 workers: loading on first use pays off: only some workers load it"
        );
        assert_eq!(
            found["workers"]["never_used"][0]["advice"],
            "deferred in the host; loaded by 0 of 2 workers: the deferral is paying off"
        );

        let text = render(&found);
        assert!(
            text.contains("  pandas, imported by shop.views: deferred in the host; loaded by 0 of 2 workers: the deferral is paying off\n"),
            "{text}"
        );
        assert!(
            text.contains("The counts cover the requests served so far"),
            "{text}"
        );
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
