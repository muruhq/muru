//! `--print-config` (design specification sections 13.1 and 13.9): the
//! settings a `muructl run` or a `muructl serve` would use, each with
//! where it came from, written in the shape of the project's own file so
//! the two can be read side by side.

use std::fmt::Write as _;

use serde::Serialize;

/// What a setting is called when nothing sets it.
pub(crate) const DEFAULT: &str = "default";
/// What a setting is called when a flag sets it.
pub(crate) const COMMAND_LINE: &str = "command line";

/// One setting: its key, its value and where the value came from.
pub struct Row {
    key: &'static str,
    value: toml::Value,
    source: String,
}

/// The settings a run or a serve uses, as its report sets them out: the
/// application's own, then the command's table.
pub struct Settings {
    /// What the report says before the settings.
    notes: Vec<String>,
    /// The application's settings.
    top: Vec<Row>,
    /// The command's table: `run` or `serve`.
    section: &'static str,
    /// The settings of the command's table.
    rows: Vec<Row>,
    /// What the report says after the settings.
    trailer: Vec<String>,
}

impl Settings {
    /// Each setting as a line of `muructl config show --effective`, which
    /// reads `SCOPE: KEY = VALUE (SOURCE)`, with those of the command's
    /// table under its name, as `run.port`.
    pub fn layers(&self, scope: &str) -> Vec<String> {
        let top = self.top.iter().map(|row| (row.key.to_owned(), row));
        let rows = self
            .rows
            .iter()
            .map(|row| (format!("{}.{}", self.section, row.key), row));

        top.chain(rows)
            .map(|(key, row)| {
                let value = match &row.value {
                    toml::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };

                format!("{scope}: {key} = {value} ({})", row.source)
            })
            .collect()
    }
}

impl Row {
    /// A setting, or none for a value TOML cannot write.
    pub fn new(
        key: &'static str,
        value: impl Serialize,
        source: impl Into<String>,
    ) -> Option<Self> {
        Some(Self {
            key,
            value: toml::Value::try_from(value).ok()?,
            source: source.into(),
        })
    }
}

/// The tables as they were read, kept so that which of them set a key
/// can be asked once they have been merged.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    /// The project's settings.
    pub table: serde_json::Value,
    /// The project's run table.
    pub run: serde_json::Value,
    /// The run table of muru.local.toml.
    pub local: serde_json::Value,
}

impl Layers {
    /// The tables as they were read.
    pub fn new(
        table: &muru_config::ToolMuru,
        run: &muru_config::RunSettings,
        local: &muru_config::RunSettings,
    ) -> Self {
        Self {
            table: serde_json::to_value(table).unwrap_or_default(),
            run: serde_json::to_value(run).unwrap_or_default(),
            local: serde_json::to_value(local).unwrap_or_default(),
        }
    }
}

/// Whether a table sets a key: an absent key, an empty list and an empty
/// table set nothing.
fn has(table: &serde_json::Value, key: &str) -> bool {
    match table.get(key) {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(object)) => !object.is_empty(),
        Some(_) => true,
    }
}

/// How the project's settings are named: the table of a pyproject.toml,
/// or the file itself.
pub fn table_name(source: Option<&std::path::Path>, table: &str) -> String {
    match source {
        Some(path)
            if path
                .file_name()
                .is_some_and(|name| name == "pyproject.toml") =>
        {
            let inner = if table.is_empty() {
                "tool.muru".to_owned()
            } else {
                format!("tool.muru.{table}")
            };
            format!("[{inner}] in {}", path.display())
        }
        Some(path) if table.is_empty() => path.display().to_string(),
        Some(path) => format!("[{table}] in {}", path.display()),
        None => "the project".to_owned(),
    }
}

/// The report: a comment saying what it is, the application's settings,
/// then the command's own table.
fn render(settings: &Settings) -> String {
    let Settings {
        notes,
        top,
        section,
        rows,
        trailer,
    } = settings;
    let mut out =
        format!("# The settings this muructl {section} would use, and where each came from.\n");

    for note in notes {
        let _ = writeln!(out, "# {note}");
    }

    let block = |out: &mut String, rows: &[Row]| {
        let lines: Vec<(String, &str)> = rows
            .iter()
            .map(|row| (format!("{} = {}", row.key, row.value), row.source.as_str()))
            .collect();

        // The sources line up, unless one long value would push them
        // all off the screen.

        let width = lines
            .iter()
            .map(|(line, _)| line.len())
            .filter(|length| *length <= 44)
            .max()
            .unwrap_or(0);

        for (line, source) in lines {
            let _ = writeln!(out, "{line:<width$}    # {source}");
        }
    };

    out.push('\n');
    block(&mut out, top);
    let _ = writeln!(out, "\n[{section}]");
    block(&mut out, rows);

    if !trailer.is_empty() {
        out.push('\n');
    }

    for line in trailer {
        let _ = writeln!(out, "# {line}");
    }

    out
}

/// The tables of the project's settings that no flag changes, which are
/// used as the file has them.
fn other_tables(table: &serde_json::Value, shown: &[Row]) -> Vec<String> {
    let Some(object) = table.as_object() else {
        return Vec::new();
    };

    object
        .iter()
        .filter(|(key, value)| {
            !matches!(key.as_str(), "run" | "serve")
                && !shown.iter().any(|row| row.key == key.as_str())
                && has(table, key)
                && (value.is_object() || value.is_array())
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// The note that names those tables.
fn other_tables_note(tables: &[String], source: Option<&std::path::Path>) -> Vec<String> {
    match (tables.is_empty(), source) {
        (false, Some(path)) => vec![format!(
            "Used as {} has them: {}.",
            path.display(),
            tables.join(", ")
        )],
        _ => Vec::new(),
    }
}

/// `muructl run --print-config`: the flags over muru.local.toml over the
/// project's run table over the defaults, and the application's settings
/// from the flags over the project's table.
pub fn for_run(args: &crate::RunArgs, resolved: &crate::Resolved, interpreter: &str) -> String {
    let mut settings = run_settings(args, resolved);
    settings
        .trailer
        .insert(0, format!("The interpreter is {interpreter}."));
    render(&settings)
}

/// The settings a `muructl run` uses, each with where it came from.
pub fn run_settings(args: &crate::RunArgs, resolved: &crate::Resolved) -> Settings {
    let layers = &resolved.layers;
    let serving = &resolved.serving;
    let source = resolved.source.as_deref();
    let project = table_name(source, "");
    let run_table = table_name(source, "run");
    let local = serving
        .local
        .as_ref()
        .map(|path| format!("[run] in {}", path.display()));

    let app = |flag: bool, key: &str| -> String {
        if flag {
            COMMAND_LINE.to_owned()
        } else if has(&layers.table, key) {
            project.clone()
        } else {
            DEFAULT.to_owned()
        }
    };

    // One value: the highest layer that sets it.

    let run = |flag: bool, key: &str| -> String {
        if flag {
            COMMAND_LINE.to_owned()
        } else if let (true, Some(local)) = (has(&layers.local, key), &local) {
            local.clone()
        } else if has(&layers.run, key) {
            run_table.clone()
        } else {
            DEFAULT.to_owned()
        }
    };

    // A list: every layer that adds to it.

    let list = |flag: bool, key: &str| -> String {
        let mut from = Vec::new();

        if has(&layers.run, key) {
            from.push(run_table.clone());
        }

        if let (true, Some(local)) = (has(&layers.local, key), &local) {
            from.push(local.clone());
        }

        if flag {
            from.push(COMMAND_LINE.to_owned());
        }

        from.join(", then ")
    };

    let single = args.single;
    let entrypoint_given = args.app.as_deref() == Some(resolved.entrypoint.as_str());

    let mut top = vec![
        Row::new(
            "entrypoint",
            &resolved.entrypoint,
            app(entrypoint_given, "entrypoint"),
        ),
        Row::new(
            "interface",
            resolved.interface.as_str(),
            app(args.interface.is_some(), "interface"),
        ),
        Row::new(
            "factory",
            resolved.factory.is_some(),
            app(args.factory, "factory"),
        ),
        Row::new(
            "processes",
            resolved.processes,
            app(single || args.processes.is_some(), "processes"),
        ),
        Row::new(
            "threads",
            resolved.threads,
            app(single || args.threads.is_some(), "threads"),
        ),
        Row::new(
            "sync_threads",
            resolved.sync_threads,
            app(args.sync_threads.is_some(), "sync_threads"),
        ),
        Row::new(
            "concurrency",
            resolved.concurrency,
            app(args.concurrency.is_some(), "concurrency"),
        ),
        Row::new(
            "preload",
            resolved.preload,
            app(args.preload.is_some(), "preload"),
        ),
    ];

    if let Some(hooks) = &resolved.hooks {
        top.push(Row::new("hooks", hooks, app(args.hooks.is_some(), "hooks")));
    }

    // Section 6.15: muru.local.toml's instrumentation entries come before
    // the project's, so the row names both when both give some.

    if !resolved.instrumentation.is_empty() {
        let from_table = layers.table["instrumentation"]
            .as_array()
            .map_or(0, Vec::len);

        let source = match (&serving.local, from_table) {
            (Some(path), 0) => format!("[[instrumentation]] in {}", path.display()),
            (Some(path), _) if resolved.instrumentation.len() > from_table => {
                format!("[[instrumentation]] in {}, then {project}", path.display())
            }
            _ => project.clone(),
        };

        top.push(Row::new(
            "instrumentation",
            &resolved.instrumentation,
            source,
        ));
    }

    if let Some(python) = args.python.as_ref().or(resolved.python.as_ref()) {
        top.push(Row::new(
            "python",
            python,
            app(args.python.is_some(), "python"),
        ));
    }

    if let Some(requests) = resolved.max_requests {
        top.push(Row::new(
            "max_requests",
            requests,
            app(args.max_requests.is_some(), "max_requests"),
        ));
    }

    if let Some(lifetime) = resolved.max_lifetime {
        top.push(Row::new(
            "max_lifetime",
            muru_config::format_duration(lifetime),
            app(args.max_lifetime.is_some(), "max_lifetime"),
        ));
    }

    let top: Vec<Row> = top.into_iter().flatten().collect();
    let (http, https) = crate::addresses(args, serving);

    let port = |port: Option<crate::Port>, address: std::net::SocketAddr| match port {
        Some(crate::Port::Auto) => toml::Value::String("auto".to_owned()),
        _ => toml::Value::Integer(i64::from(address.port())),
    };

    let reload = if !args.reload && !args.no_reload && args.allow_public {
        "default under --allow-public".to_owned()
    } else {
        run(args.reload || args.no_reload, "reload")
    };

    let mut rows = vec![
        Row::new(
            "port",
            port(serving.port, http),
            run(args.port.is_some(), "port"),
        ),
        Row::new(
            "https",
            serving.https,
            run(args.https || args.no_https, "https"),
        ),
        Row::new(
            "https_only",
            serving.https_only,
            run(args.https_only || args.no_https_only, "https_only"),
        ),
    ];

    if serving.tls() {
        rows.push(Row::new(
            "https_port",
            port(serving.https_port, https),
            run(args.https_port.is_some(), "https_port"),
        ));
    }

    if let Some(certificate) = &serving.tls_cert {
        rows.push(Row::new(
            "tls_cert",
            certificate.display().to_string(),
            run(args.tls_cert.is_some(), "tls_cert"),
        ));
    }

    if let Some(key) = &serving.tls_key {
        rows.push(Row::new(
            "tls_key",
            key.display().to_string(),
            run(args.tls_key.is_some(), "tls_key"),
        ));
    }

    if !serving.hosts.is_empty() {
        rows.push(Row::new(
            "hosts",
            &serving.hosts,
            list(!args.hosts.is_empty(), "hosts"),
        ));
    }

    if let Some(file) = &serving.env_file {
        rows.push(Row::new(
            "env_file",
            file.display().to_string(),
            run(args.env_file.is_some(), "env_file"),
        ));
    }

    rows.push(Row::new("reload", serving.reload, reload));

    let paths = |paths: &[std::path::PathBuf]| -> Vec<String> {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    };

    if !serving.reload_dirs.is_empty() {
        rows.push(Row::new(
            "reload_dirs",
            paths(&serving.reload_dirs),
            list(!args.reload_dir.is_empty(), "reload_dirs"),
        ));
    }

    if !serving.reload_include.is_empty() {
        rows.push(Row::new(
            "reload_include",
            &serving.reload_include,
            list(!args.reload_include.is_empty(), "reload_include"),
        ));
    }

    if !serving.reload_exclude.is_empty() {
        rows.push(Row::new(
            "reload_exclude",
            &serving.reload_exclude,
            list(!args.reload_exclude.is_empty(), "reload_exclude"),
        ));
    }

    rows.push(Row::new(
        "access_log",
        &serving.access_log,
        run(args.access_log.is_some(), "access_log"),
    ));

    if let Some(file) = &serving.access_log_file {
        rows.push(Row::new(
            "access_log_file",
            file.display().to_string(),
            run(args.access_log_file.is_some(), "access_log_file"),
        ));
    }

    if !serving.access_log_exclude.is_empty() {
        rows.push(Row::new(
            "access_log_exclude",
            &serving.access_log_exclude,
            list(!args.access_log_exclude.is_empty(), "access_log_exclude"),
        ));
    }

    rows.push(Row::new(
        "slow",
        muru_config::format_duration(serving.slow),
        run(args.slow.is_some(), "slow"),
    ));

    if let Some(dashboard) = &serving.dashboard {
        rows.push(Row::new(
            "dashboard",
            dashboard,
            run(args.dashboard.is_some(), "dashboard"),
        ));
    }

    rows.push(Row::new(
        "compression",
        serving.compression,
        run(args.no_compression, "compression"),
    ));
    rows.push(Row::new(
        "toolbar",
        serving.toolbar,
        run(args.toolbar || args.no_toolbar, "toolbar"),
    ));
    rows.push(Row::new(
        "timestamps",
        serving.timestamps,
        run(args.timestamps || args.no_timestamps, "timestamps"),
    ));

    let rows: Vec<Row> = rows.into_iter().flatten().collect();

    let mut notes = match source {
        Some(path) => vec![format!(
            "The project's settings are read from {}.",
            path.display()
        )],
        None => vec!["The project has no settings file.".to_owned()],
    };

    if single {
        notes.push("--single gives one process of one thread.".to_owned());
    }

    let mut trailer = other_tables_note(&other_tables(&layers.table, &top), source);

    if has(&layers.table, "serve") {
        trailer.push("The serve table is muructl serve's, and is not read here.".to_owned());
    }

    // What the run's .env file sets, by name and never by value.

    if let Some(file) = &serving.env_file {
        trailer.push(env_file_note(
            file,
            &resolved.env_file_names,
            layers.table.get("env"),
        ));
    }

    Settings {
        notes,
        top,
        section: "run",
        rows,
        trailer,
    }
}

/// The line of the report for the run's `.env` file: what it sets, and
/// which of those it takes from the project's `[tool.muru.env]`.
fn env_file_note(
    file: &std::path::Path,
    names: &[String],
    table: Option<&serde_json::Value>,
) -> String {
    let declared = |name: &str| table.and_then(|table| table.get(name)).is_some();
    let overrides: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| declared(name))
        .collect();

    let mut note = if names.is_empty() {
        format!("{} sets no variables.", file.display())
    } else {
        format!("{} sets {}.", file.display(), names.join(", "))
    };

    if !overrides.is_empty() {
        let _ = write!(note, " Over [tool.muru.env]: {}.", overrides.join(", "));
    }

    note
}

/// What `muructl serve` adds to the report: its own table's settings,
/// those of the application its layers set, and what it worked out.
pub struct Serve {
    /// Serve's own settings, each given or its default.
    pub rows: Vec<Row>,
    /// The layer that set one of the application's settings, if serve's
    /// table, the environment or a flag did.
    pub set: std::collections::BTreeMap<&'static str, String>,
    /// What serve worked out, as sentences.
    pub trailer: Vec<String>,
}

/// `muructl serve --print-config`: the flags over the environment over
/// the serve table of the project's settings over the defaults.
pub fn for_serve(resolved: &crate::Resolved, serve: Serve) -> String {
    render(&serve_settings(resolved, serve))
}

/// The settings a `muructl serve` uses, each with where it came from.
pub fn serve_settings(resolved: &crate::Resolved, serve: Serve) -> Settings {
    let layers = &resolved.layers;
    let source = resolved.source.as_deref();
    let project = table_name(source, "");

    let app = |key: &'static str| -> String {
        if let Some(layer) = serve.set.get(key) {
            layer.clone()
        } else if has(&layers.table, key) {
            project.clone()
        } else {
            DEFAULT.to_owned()
        }
    };

    let mut top = vec![
        Row::new("entrypoint", &resolved.entrypoint, app("entrypoint")),
        Row::new("interface", resolved.interface.as_str(), app("interface")),
        Row::new("factory", resolved.factory.is_some(), app("factory")),
        Row::new("processes", resolved.processes, app("processes")),
        Row::new("threads", resolved.threads, app("threads")),
        Row::new("sync_threads", resolved.sync_threads, app("sync_threads")),
        Row::new("concurrency", resolved.concurrency, app("concurrency")),
        Row::new("preload", resolved.preload, app("preload")),
    ];

    if let Some(hooks) = &resolved.hooks {
        top.push(Row::new("hooks", hooks, app("hooks")));
    }

    if !resolved.instrumentation.is_empty() {
        top.push(Row::new(
            "instrumentation",
            &resolved.instrumentation,
            project.clone(),
        ));
    }

    if let Some(requests) = resolved.max_requests {
        top.push(Row::new("max_requests", requests, app("max_requests")));
    }

    if let Some(lifetime) = resolved.max_lifetime {
        top.push(Row::new(
            "max_lifetime",
            muru_config::format_duration(lifetime),
            app("max_lifetime"),
        ));
    }

    let top: Vec<Row> = top.into_iter().flatten().collect();

    let notes = match source {
        Some(path) => vec![format!(
            "The project's settings are read from {}.",
            path.display()
        )],
        None => vec!["The project has no settings file.".to_owned()],
    };

    let mut trailer = other_tables_note(&other_tables(&layers.table, &top), source);

    if has(&layers.table, "run") {
        trailer.push("The run table is muructl run's, and is not read here.".to_owned());
    }

    trailer.extend(serve.trailer);

    Settings {
        notes,
        top,
        section: "serve",
        rows: serve.rows,
        trailer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_sets_a_key_it_gives_a_value() {
        let table = serde_json::json!({
            "threads": 3, "hooks": null, "hosts": [], "env": {}, "static": [{"path": "/s"}]
        });

        assert!(has(&table, "threads"));
        assert!(has(&table, "static"));

        for key in ["hooks", "hosts", "env", "processes"] {
            assert!(!has(&table, key), "{key}");
        }
    }

    #[test]
    fn settings_are_named_by_the_file_they_are_in() {
        let pyproject = std::path::Path::new("/app/pyproject.toml");
        let mounted = std::path::Path::new("/etc/shop/settings.toml");

        assert_eq!(
            table_name(Some(pyproject), ""),
            "[tool.muru] in /app/pyproject.toml"
        );
        assert_eq!(
            table_name(Some(pyproject), "run"),
            "[tool.muru.run] in /app/pyproject.toml"
        );
        assert_eq!(table_name(Some(mounted), ""), "/etc/shop/settings.toml");
        assert_eq!(
            table_name(Some(mounted), "serve"),
            "[serve] in /etc/shop/settings.toml"
        );
    }

    #[test]
    fn the_report_reads_as_the_projects_file_with_sources() {
        let top = [
            Row::new("entrypoint", "shop:application", "muru.toml").expect("a row"),
            Row::new("threads", 8, COMMAND_LINE).expect("a row"),
        ];
        let rows = [Row::new("hosts", ["a.test", "b.test"], DEFAULT).expect("a row")];
        let settings = Settings {
            notes: vec!["A note.".to_owned()],
            top: top.into(),
            section: "run",
            rows: rows.into(),
            trailer: vec!["After.".to_owned()],
        };
        let report = render(&settings);

        assert_eq!(
            report,
            "# The settings this muructl run would use, and where each came from.\n\
             # A note.\n\
             \n\
             entrypoint = \"shop:application\"    # muru.toml\n\
             threads = 8                        # command line\n\
             \n\
             [run]\n\
             hosts = [\"a.test\", \"b.test\"]    # default\n\
             \n\
             # After.\n"
        );
        report.parse::<toml::Table>().expect("valid TOML");

        // The same settings as lines of the effective configuration.

        assert_eq!(
            settings.layers("shop@r1"),
            [
                "shop@r1: entrypoint = shop:application (muru.toml)",
                "shop@r1: threads = 8 (command line)",
                "shop@r1: run.hosts = [\"a.test\", \"b.test\"] (default)",
            ]
        );
    }
}
