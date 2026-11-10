//! `muructl debug report` (design specification section 18.6): a short
//! summary of an installation to attach to a bug report, as text,
//! Markdown or JSON. It may be posted in public, so it holds no log
//! lines, request data, environment or secrets, and what identifies the
//! system is replaced by placeholders (see [`super::redact`]).
//!
//! The report is built once, already with its placeholders, and every
//! format is written from that, so the three say the same.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Args;
use muru_controller::diagnostics::facts::{self, Platform};
use muru_controller::diagnostics::stored::Names;
use muru_controller::diagnostics::{NotCollected, ReleaseFacts, Setting};
use muru_proto::admin::v1 as pb;
use serde::Serialize;

use super::redact::{Redactor, Replaced};
use super::{Installation, Runs, markdown};
use crate::admin::{DaemonFileArgs, SocketArgs, format_time, now_unix_ms};

/// How far back the report counts events.
const EVENT_WINDOW_HOURS: u64 = 24;

/// How many of the most recent warnings and errors the report lists.
const RECENT_EVENTS: usize = 10;

/// The events whose sentence quotes a request, a command or the
/// application's code, which the report leaves out.
const QUOTING_EVENTS: [&str; 5] = [
    "debug.exec",
    "debug.breakpoint",
    "debug.breakpoint_expired",
    "request.",
    "command.",
];

/// What stands for a sentence that is left out.
const LEFT_OUT: &str = "(left out: it quotes a request, a command or the application's code)";

/// The tables of the daemon's configuration file that configure the
/// daemon itself, which the report lists setting by setting. Any other
/// table (virtual hosts, routes, applications, upstreams, secret stores,
/// event sinks and so on) is only counted.
const DAEMON_TABLES: [&str; 10] = [
    "daemon",
    "tools",
    "listener",
    "limits",
    "proxy",
    "compression",
    "logs",
    "shutdown",
    "metrics",
    "dashboard",
];

/// The settings whose values the report shows: numbers, switches, sizes,
/// durations, addresses and paths, by the key less the `run.` or
/// `serve.` of a command's own table. A key ending in a full stop
/// stands for every key under it. Any other setting is shown as set,
/// without its value, so a setting added later is left out until it is
/// listed here.
const SHOWN_SETTINGS: [&str; 42] = [
    "interface",
    "factory",
    "preload",
    "processes",
    "threads",
    "sync_threads",
    "concurrency",
    "max_requests",
    "max_lifetime",
    "build.",
    "bytecode",
    "python",
    "accel_redirect",
    "cgi_local_redirect",
    "timeouts.",
    "memory.",
    "max_body_size",
    "port",
    "https",
    "https_only",
    "https_port",
    "reload",
    "access_log",
    "slow",
    "dashboard",
    "compression",
    "timestamps",
    "listen",
    "admin_listen",
    "admin_status",
    "min_ready",
    "timeout",
    "memory",
    "max_connections",
    "max_connections_per_client",
    "max_requests_per_connection",
    "log_format",
    "run_dir",
    "runtime_dir",
    "spool_quota",
    "shutdown_delay",
    "shutdown_budget",
];

/// The settings that name a Python module, shown with the module
/// replaced.
const MODULE_SETTINGS: [&str; 2] = ["entrypoint", "hooks"];

/// What stands for the value of a setting that is not shown.
const SET: &str = "<set>";

/// How `muructl debug report` writes the report.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ReportFormat {
    /// Plain text.
    Text,
    /// Markdown, to paste into an issue as it is.
    Markdown,
}

/// `muructl debug report`.
#[derive(Args)]
pub struct ReportArgs {
    /// Plain text, or Markdown to paste into an issue as it is.
    #[arg(long, value_enum, default_value_t = ReportFormat::Text, conflicts_with = "json")]
    format: ReportFormat,

    /// Print the report as JSON.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its state and configuration are.
    #[command(flatten)]
    files: DaemonFileArgs,
}

/// A release in the report.
#[derive(Debug, Serialize)]
pub struct ReleaseRow {
    /// The release, as `app1@r3`.
    release: String,
    /// Its state.
    state: String,
    /// Whether traffic goes to it.
    serving: bool,
    /// `workers`, or `http` for a managed HTTP server.
    kind: String,
    /// The interface its host settled on, else as declared.
    interface: String,
    /// The frameworks one of its workers has loaded.
    frameworks: Vec<String>,
    /// The interpreter's version.
    python: String,
    /// `regular` or `free-threaded`.
    build: String,
    /// Each worker's group and state.
    workers: Vec<WorkerRow>,
}

/// A worker in the report.
#[derive(Debug, Serialize)]
pub struct WorkerRow {
    index: u32,
    group: String,
    state: String,
}

/// A setting of the daemon's own configuration file.
#[derive(Debug, Serialize)]
pub struct DaemonSetting {
    key: String,
    value: String,
}

/// How many events of one type and level the window holds.
#[derive(Debug, Serialize)]
pub struct EventCount {
    r#type: String,
    level: String,
    count: usize,
}

/// A warning or an error.
#[derive(Debug, Serialize)]
pub struct EventRow {
    time: String,
    r#type: String,
    level: String,
    subject: String,
    message: String,
}

/// The events of the report.
#[derive(Debug, Serialize)]
pub struct Events {
    /// How far back the counts go, in hours.
    window_hours: u64,
    /// The events in the window by type and level.
    counts: Vec<EventCount>,
    /// The most recent warnings and errors, oldest first.
    recent: Vec<EventRow>,
}

/// The report, with its placeholders in place. A part that could not be
/// collected is null, and `not_collected` says why.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Muru's version.
    muru: String,
    /// The operating system and architecture.
    platform: Platform,
    /// How Muru runs: `muructl run`, `a user daemon` and so on.
    runs: String,
    /// Whether it is running now.
    running: bool,
    /// Whether it runs in a container.
    container: bool,
    /// Every release, with its workers' states.
    releases: Option<Vec<ReleaseRow>>,
    /// The effective settings of the serving releases.
    settings: Option<Vec<Setting>>,
    /// What the daemon's own configuration file sets; null where there is
    /// no daemon.
    daemon_settings: Option<Vec<DaemonSetting>>,
    /// Recent events.
    events: Option<Events>,
    /// The parts that could not be collected, each with why.
    not_collected: Vec<NotCollected>,
    /// How many values of each kind were replaced by placeholders.
    replaced: Replaced,
}

/// Print the report.
pub async fn report(args: ReportArgs) -> anyhow::Result<()> {
    let mut installation = Installation::reach(&args.socket, &args.files).await?;
    let report = Report::build(Collected::from(&mut installation).await);

    if args.json {
        return crate::admin::print_answer(&report);
    }

    match args.format {
        ReportFormat::Text => print!("{}", report.text()),
        ReportFormat::Markdown => print!("{}", report.markdown()),
    }

    Ok(())
}

/// What was collected for a report, as the installation gave it: the
/// facts before any placeholder, and what names its system.
struct Collected {
    /// How Muru runs.
    runs: Runs,
    /// Whether it is running now.
    running: bool,
    /// Whether it runs in a container.
    container: bool,
    /// The operating system.
    platform: Platform,
    /// Muru's own directories, each with the label that stands for it.
    roots: Vec<(PathBuf, &'static str)>,
    /// The home directory of whoever runs the command.
    home: Option<PathBuf>,
    /// The machine's name.
    node: String,
    /// The applications, virtual hosts and users the installation knows.
    names: Names,
    /// The user running the command.
    user: Option<String>,
    /// Every release.
    releases: Result<Vec<ReleaseFacts>, String>,
    /// The effective settings of the serving releases.
    settings: Result<Vec<Setting>, String>,
    /// The daemon's own configuration file, none without one, and
    /// nothing at all where there is no daemon.
    daemon_file: Option<Result<Option<toml::Table>, String>>,
    /// Every event kept.
    events: Result<Vec<pb::Event>, String>,
    /// What could not be collected on the way.
    notes: Vec<NotCollected>,
    /// The time now, in Unix milliseconds.
    now_unix_ms: u64,
}

impl Collected {
    /// Collect from an installation.
    async fn from(installation: &mut Installation) -> Self {
        let names = installation.names().await;
        let releases = installation.releases().await;
        let serving: Vec<ReleaseFacts> = releases
            .iter()
            .flatten()
            .filter(|release| release.serving)
            .cloned()
            .collect();

        let settings = match &releases {
            Ok(_) => installation.settings(&serving).await,
            Err(_) => Err("the serving releases are not known".to_owned()),
        };

        let events = installation.events().await;

        let roots = installation.locations().map_or_else(Vec::new, |locations| {
            vec![
                (locations.state.path.clone(), "<state-dir>"),
                (locations.run.path.clone(), "<run-dir>"),
                (locations.logs.path.clone(), "<log-dir>"),
                (locations.cache.path.clone(), "<cache-dir>"),
                (locations.config.path.clone(), "<daemon-config>"),
            ]
        });

        Self {
            runs: installation.runs(),
            running: installation.running(),
            container: facts::in_container(),
            platform: Platform::read(),
            roots,
            home: super::home(),
            node: muru_controller::config::hostname(),
            names,
            user: super::user(),
            releases,
            settings,
            daemon_file: installation.runs().is_daemon().then(|| {
                installation
                    .daemon_file()
                    .map(|file| file.map(|(_, table)| table))
            }),
            events,
            notes: installation.take_notes(),
            now_unix_ms: now_unix_ms(),
        }
    }
}

impl Report {
    /// The report of what was collected, with its placeholders in place.
    fn build(collected: Collected) -> Self {
        let mut redactor = Redactor::default();

        // Muru's own directories read as what they are for, and the
        // rest of what identifies the system as a placeholder.

        for (path, label) in &collected.roots {
            redactor.root(path, label);
        }

        if let Some(home) = &collected.home {
            redactor.home(home);
        }

        let names = &collected.names;
        names.apps.iter().for_each(|app| redactor.app(app));
        names.hosts.iter().for_each(|host| redactor.host(host));
        redactor.node(&collected.node);
        redactor.node(collected.node.split('.').next().unwrap_or_default());
        names.users.iter().for_each(|user| redactor.user(user));
        collected.user.iter().for_each(|user| redactor.user(user));

        // A name the installation did not list, as a run's application,
        // is known from the releases.

        collected
            .releases
            .iter()
            .flatten()
            .for_each(|release| redactor.app(&release.app));

        // What was noted on the way comes first, then each part that is
        // missing whole.

        let mut not_collected = collected.notes;
        let mut part = |name: &str, why: Option<&String>| {
            if let Some(why) = why {
                not_collected.push(NotCollected::new(name, why));
            }
        };
        part("releases", collected.releases.as_ref().err());
        part("settings", collected.settings.as_ref().err());
        part("events", collected.events.as_ref().err());
        part(
            "daemon settings",
            collected
                .daemon_file
                .as_ref()
                .and_then(|found| found.as_ref().err()),
        );

        for note in &mut not_collected {
            note.why = redactor.text(&note.why);
        }

        let releases = collected.releases.ok().map(|releases| {
            releases
                .iter()
                .map(|release| release_row(release, &mut redactor))
                .collect()
        });
        let settings = collected.settings.ok().map(|settings| {
            settings
                .iter()
                .map(|setting| shown_setting(setting, &mut redactor))
                .collect()
        });
        let daemon_settings = collected
            .daemon_file
            .and_then(Result::ok)
            .map(|table| daemon_settings(table, &mut redactor));
        let events = collected
            .events
            .ok()
            .map(|events| summarise(&events, collected.now_unix_ms, &mut redactor));

        Self {
            muru: env!("CARGO_PKG_VERSION").to_owned(),
            platform: collected.platform,
            runs: collected.runs.describe().to_owned(),
            running: collected.running,
            container: collected.container,
            releases,
            settings,
            daemon_settings,
            events,
            not_collected,
            replaced: redactor.replaced(),
        }
    }

    /// How Muru runs, as one phrase.
    fn runs_phrase(&self) -> String {
        format!(
            "{}, {}, {}",
            self.runs,
            if self.running {
                "running"
            } else {
                "not running"
            },
            if self.container {
                "in a container"
            } else {
                "not in a container"
            }
        )
    }

    /// What the placeholders stand for, as one sentence, or nothing when
    /// none was needed.
    fn replaced_sentence(&self) -> Option<String> {
        let replaced = &self.replaced;
        let kinds: Vec<String> = [
            (replaced.applications, "application name"),
            (replaced.hosts, "host name"),
            (replaced.users, "user name"),
            (replaced.paths, "path"),
            (replaced.addresses, "address"),
            (replaced.modules, "module"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, kind)| crate::admin::counted(*count, kind, &plural(kind)))
        .collect();

        (!kinds.is_empty()).then(|| {
            format!(
                "Placeholders stand for {}. Read the report before posting it.",
                kinds.join(", ")
            )
        })
    }

    /// The report as plain text.
    pub fn text(&self) -> String {
        let mut out = format!(
            "Muru {} on {} {} ({})\nRuns as {}\n",
            self.muru,
            self.platform.os,
            self.platform.arch,
            if self.platform.kernel.is_empty() {
                "kernel not known".to_owned()
            } else {
                format!("kernel {}", self.platform.kernel)
            },
            self.runs_phrase()
        );

        if let Some(releases) = &self.releases {
            out.push_str("\nRELEASES\n");

            if releases.is_empty() {
                out.push_str("  none\n");
            }

            let rows: Vec<Vec<String>> = releases
                .iter()
                .map(|release| {
                    vec![
                        release.release.clone(),
                        release.state.clone(),
                        yes_no(release.serving).to_owned(),
                        or_dash(&release.interface),
                        or_dash(&release.frameworks.join(", ")),
                        or_dash(&python(release)),
                        or_dash(&worker_states(&release.workers)),
                    ]
                })
                .collect();

            if !rows.is_empty() {
                out.push_str(&columns(
                    &[
                        "RELEASE",
                        "STATE",
                        "SERVING",
                        "INTERFACE",
                        "FRAMEWORK",
                        "PYTHON",
                        "WORKERS",
                    ],
                    &rows,
                ));
            }
        }

        if let Some(settings) = &self.settings {
            out.push_str("\nSETTINGS OF THE SERVING RELEASES\n");

            if settings.is_empty() {
                out.push_str("  none\n");
            }

            for (scope, settings) in by_scope(settings) {
                out.push_str(&format!("  {scope}\n"));
                let (defaults, set) = split_defaults(&settings);
                let width = set
                    .iter()
                    .map(|setting| setting.key.len() + setting.value.len() + 3)
                    .filter(|length| *length <= 48)
                    .max()
                    .unwrap_or(0);

                for setting in set {
                    let line = format!("{} = {}", setting.key, setting.value);
                    out.push_str(&format!("    {line:<width$}  ({})\n", setting.source));
                }

                if !defaults.is_empty() {
                    out.push_str("    at their defaults:\n");
                    out.push_str(&wrapped(&defaults, "      ", 78));
                }
            }
        }

        if let Some(settings) = &self.daemon_settings {
            out.push_str("\nDAEMON SETTINGS\n");

            if settings.is_empty() {
                out.push_str("  none: the daemon runs on its defaults\n");
            }

            for setting in settings {
                out.push_str(&format!("  {} = {}\n", setting.key, setting.value));
            }
        }

        if let Some(events) = &self.events {
            out.push_str(&format!(
                "\nEVENTS IN THE LAST {} HOURS\n",
                events.window_hours
            ));

            if events.counts.is_empty() {
                out.push_str("  none\n");
            }

            for count in &events.counts {
                out.push_str(&format!(
                    "  {:>5}  {:<8} {}\n",
                    count.count, count.level, count.r#type
                ));
            }

            out.push_str("\nMOST RECENT WARNINGS AND ERRORS\n");

            if events.recent.is_empty() {
                out.push_str("  none\n");
            }

            for event in &events.recent {
                out.push_str(&format!(
                    "  {} {} {} {}: {}\n",
                    event.time, event.level, event.r#type, event.subject, event.message
                ));
            }
        }

        if !self.not_collected.is_empty() {
            out.push_str("\nNOT COLLECTED\n");

            for note in &self.not_collected {
                out.push_str(&format!("  {}: {}\n", note.part, note.why));
            }
        }

        if let Some(sentence) = self.replaced_sentence() {
            out.push_str(&format!("\n{sentence}\n"));
        }

        out
    }

    /// The report as Markdown. Every value that came from the system is
    /// in a code span, so nothing in it is read as markup.
    pub fn markdown(&self) -> String {
        use markdown::{cell, code, table};

        let mut out = String::from("### Muru diagnostic report\n\n");
        out.push_str(&table(
            &["", ""],
            &[
                vec!["Muru".to_owned(), cell(&self.muru)],
                vec![
                    "Operating system".to_owned(),
                    format!(
                        "{} {}, kernel {}",
                        cell(&self.platform.os),
                        cell(&self.platform.arch),
                        cell(&self.platform.kernel)
                    ),
                ],
                vec!["Runs as".to_owned(), self.runs_phrase()],
            ],
        ));

        if let Some(releases) = &self.releases {
            out.push_str("\n#### Releases\n\n");

            if releases.is_empty() {
                out.push_str("None.\n");
            } else {
                let rows: Vec<Vec<String>> = releases
                    .iter()
                    .map(|release| {
                        vec![
                            cell(&release.release),
                            cell(&release.state),
                            yes_no(release.serving).to_owned(),
                            cell(&release.interface),
                            cell(&release.frameworks.join(", ")),
                            cell(&python(release)),
                            cell(&worker_states(&release.workers)),
                        ]
                    })
                    .collect();
                out.push_str(&table(
                    &[
                        "Release",
                        "State",
                        "Serving",
                        "Interface",
                        "Framework",
                        "Python",
                        "Workers",
                    ],
                    &rows,
                ));
            }
        }

        if let Some(settings) = &self.settings {
            out.push_str("\n#### Settings of the serving releases\n\n");

            // The settings something set are a table; those left at
            // their defaults follow it, a line for each release.

            let all: Vec<&Setting> = settings.iter().collect();
            let (_, set) = split_defaults(&all);

            if settings.is_empty() {
                out.push_str("None.\n");
            } else if !set.is_empty() {
                let rows: Vec<Vec<String>> = set
                    .iter()
                    .map(|setting| {
                        vec![
                            cell(&setting.scope),
                            cell(&setting.key),
                            cell(&setting.value),
                            cell(&setting.source),
                        ]
                    })
                    .collect();
                out.push_str(&table(&["Of", "Setting", "Value", "From"], &rows));
            }

            for (scope, settings) in by_scope(settings) {
                let (defaults, _) = split_defaults(&settings);

                if !defaults.is_empty() {
                    let spans: Vec<String> = defaults.iter().map(|pair| code(pair)).collect();
                    out.push_str(&format!(
                        "\nAt their defaults for {}: {}\n",
                        code(scope),
                        spans.join(", ")
                    ));
                }
            }
        }

        if let Some(settings) = &self.daemon_settings {
            out.push_str("\n#### Daemon settings\n\n");

            if settings.is_empty() {
                out.push_str("None: the daemon runs on its defaults.\n");
            } else {
                let rows: Vec<Vec<String>> = settings
                    .iter()
                    .map(|setting| vec![cell(&setting.key), cell(&setting.value)])
                    .collect();
                out.push_str(&table(&["Setting", "Value"], &rows));
            }
        }

        if let Some(events) = &self.events {
            out.push_str(&format!(
                "\n#### Events in the last {} hours\n\n",
                events.window_hours
            ));

            if events.counts.is_empty() {
                out.push_str("None.\n");
            } else {
                let rows: Vec<Vec<String>> = events
                    .counts
                    .iter()
                    .map(|count| {
                        vec![
                            cell(&count.r#type),
                            cell(&count.level),
                            count.count.to_string(),
                        ]
                    })
                    .collect();
                out.push_str(&table(&["Type", "Level", "Count"], &rows));
            }

            out.push_str("\n#### Most recent warnings and errors\n\n");

            // A sentence is long for a table cell, so each event is a
            // line of a fenced block.

            if events.recent.is_empty() {
                out.push_str("None.\n");
            } else {
                let lines: Vec<String> = events
                    .recent
                    .iter()
                    .map(|event| {
                        format!(
                            "{} {} {} {}: {}",
                            event.time, event.level, event.r#type, event.subject, event.message
                        )
                    })
                    .collect();
                out.push_str(&markdown::fenced(&lines));
            }
        }

        if !self.not_collected.is_empty() {
            out.push_str("\n#### Not collected\n\n");
            let rows: Vec<Vec<String>> = self
                .not_collected
                .iter()
                .map(|note| vec![code(&note.part), cell(&note.why)])
                .collect();
            out.push_str(&table(&["Part", "Why"], &rows));
        }

        if let Some(sentence) = self.replaced_sentence() {
            out.push_str(&format!("\n{sentence}\n"));
        }

        out
    }
}

/// A release as the report shows it.
fn release_row(release: &ReleaseFacts, redactor: &mut Redactor) -> ReleaseRow {
    ReleaseRow {
        release: redactor.text(&release.name()),
        state: release.state.clone(),
        serving: release.serving,
        kind: release.kind.clone(),
        interface: release.interface_shown().to_owned(),
        frameworks: release.frameworks.clone(),
        python: release.python_version.clone(),
        build: release.python_build.clone(),
        workers: release
            .workers
            .iter()
            .map(|worker| WorkerRow {
                index: worker.index,
                group: worker.group.clone(),
                state: worker.state.clone(),
            })
            .collect(),
    }
}

/// A setting as the report shows it: its value when the key is one
/// whose values are shown, a module replaced, and otherwise only that it
/// is set.
fn shown_setting(setting: &Setting, redactor: &mut Redactor) -> Setting {
    let key = setting
        .key
        .strip_prefix("run.")
        .or_else(|| setting.key.strip_prefix("serve."))
        .unwrap_or(&setting.key);

    let shown = SHOWN_SETTINGS.iter().any(|shown| {
        if shown.ends_with('.') {
            key.starts_with(shown)
        } else {
            key == *shown
        }
    });

    let value = if MODULE_SETTINGS.contains(&key) {
        redactor.module(&setting.value)
    } else if shown {
        redactor.value(key, &setting.value)
    } else {
        SET.to_owned()
    };

    Setting {
        scope: redactor.text(&setting.scope),
        key: setting.key.clone(),
        value,
        source: redactor.text(&setting.source),
    }
}

/// What the daemon's configuration file sets: the tables that configure
/// the daemon itself setting by setting, and any other only by how much
/// it holds.
fn daemon_settings(table: Option<toml::Table>, redactor: &mut Redactor) -> Vec<DaemonSetting> {
    let mut settings = Vec::new();

    for (name, value) in table.unwrap_or_default() {
        if DAEMON_TABLES.contains(&name.as_str()) {
            flatten(&name, &value, redactor, &mut settings);
            continue;
        }

        let held = match &value {
            toml::Value::Table(table) => {
                crate::admin::counted(table.len(), "entry", "entries") + ", left out"
            }
            toml::Value::Array(items) => {
                crate::admin::counted(items.len(), "entry", "entries") + ", left out"
            }
            _ => "set, left out".to_owned(),
        };

        settings.push(DaemonSetting {
            key: name,
            value: held,
        });
    }

    settings
}

/// One value of the daemon's file as settings, by dotted key: a table's
/// values under its name, an array of tables' under its name and their
/// place, and anything else as one setting.
fn flatten(key: &str, value: &toml::Value, redactor: &mut Redactor, out: &mut Vec<DaemonSetting>) {
    match value {
        toml::Value::Table(table) => {
            for (name, value) in table {
                flatten(&format!("{key}.{name}"), value, redactor, out);
            }
        }
        toml::Value::Array(items) if items.iter().all(toml::Value::is_table) => {
            for (at, item) in items.iter().enumerate() {
                flatten(&format!("{key}[{at}]"), item, redactor, out);
            }
        }
        value => {
            let last = key.rsplit('.').next().unwrap_or(key);

            // Variable names say what an installation is configured
            // with, and a user's or a group's name whose it is.

            let shown = match (last, value) {
                ("pass_env", toml::Value::Array(items)) => {
                    crate::admin::counted(items.len(), "name", "names") + ", left out"
                }
                ("user" | "group", toml::Value::String(name)) => {
                    redactor.user(name);
                    redactor.text(name)
                }
                (_, toml::Value::String(text)) => redactor.value(key, text),
                (_, other) => redactor.value(key, &other.to_string()),
            };

            out.push(DaemonSetting {
                key: key.to_owned(),
                value: shown,
            });
        }
    }
}

/// The events of the window by type and level, and the most recent
/// warnings and errors of all that are kept.
fn summarise(events: &[pb::Event], now_unix_ms: u64, redactor: &mut Redactor) -> Events {
    let since = now_unix_ms.saturating_sub(EVENT_WINDOW_HOURS * 3_600_000);
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();

    for event in events.iter().filter(|event| event.time_unix_ms >= since) {
        *counts
            .entry((event.r#type.clone(), event.level.clone()))
            .or_default() += 1;
    }

    let serious: Vec<&pb::Event> = events
        .iter()
        .filter(|event| event.level == "warning" || event.level == "error")
        .collect();
    let first = serious.len().saturating_sub(RECENT_EVENTS);

    Events {
        window_hours: EVENT_WINDOW_HOURS,
        counts: counts
            .into_iter()
            .map(|((r#type, level), count)| EventCount {
                r#type,
                level,
                count,
            })
            .collect(),
        recent: serious[first..]
            .iter()
            .map(|event| EventRow {
                time: format_time(event.time_unix_ms),
                r#type: event.r#type.clone(),
                level: event.level.clone(),
                subject: redactor.text(&event.subject),
                message: if QUOTING_EVENTS
                    .iter()
                    .any(|quoting| event.r#type.starts_with(quoting))
                {
                    LEFT_OUT.to_owned()
                } else {
                    redactor.text(&event.message)
                },
            })
            .collect(),
    }
}

/// The settings grouped by what they are settings of, in the order
/// given.
fn by_scope(settings: &[Setting]) -> Vec<(&str, Vec<&Setting>)> {
    let mut scopes: Vec<(&str, Vec<&Setting>)> = Vec::new();

    for setting in settings {
        match scopes.iter_mut().find(|(scope, _)| *scope == setting.scope) {
            Some((_, found)) => found.push(setting),
            None => scopes.push((&setting.scope, vec![setting])),
        }
    }

    scopes
}

/// The settings of one scope as those left at their defaults, each as
/// `key = value`, and those something set.
fn split_defaults<'a>(settings: &[&'a Setting]) -> (Vec<String>, Vec<&'a Setting>) {
    let (defaults, set): (Vec<&Setting>, Vec<&Setting>) = settings
        .iter()
        .partition(|setting| setting.source == crate::print_config::DEFAULT);

    (
        defaults
            .iter()
            .map(|setting| format!("{} = {}", setting.key, setting.value))
            .collect(),
        set,
    )
}

/// Items separated by commas on lines of at most `width` characters,
/// each line after `indent`.
fn wrapped(items: &[String], indent: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = String::new();

    for (at, item) in items.iter().enumerate() {
        let item = if at + 1 < items.len() {
            format!("{item}, ")
        } else {
            item.clone()
        };

        if !line.is_empty() && indent.len() + line.len() + item.trim_end().len() > width {
            out.push_str(&format!("{indent}{}\n", line.trim_end()));
            line.clear();
        }

        line.push_str(&item);
    }

    if !line.is_empty() {
        out.push_str(&format!("{indent}{}\n", line.trim_end()));
    }

    out
}

/// A release's interpreter as one phrase: its version and build.
fn python(release: &ReleaseRow) -> String {
    [release.python.as_str(), release.build.as_str()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The workers' states as one phrase: how many are in each.
fn worker_states(workers: &[WorkerRow]) -> String {
    let mut states: BTreeMap<&str, usize> = BTreeMap::new();

    for worker in workers {
        *states.entry(worker.state.as_str()).or_default() += 1;
    }

    states
        .iter()
        .map(|(state, count)| format!("{count} {state}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rows under a header in columns as wide as their widest cell.
fn columns(header: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = header
        .iter()
        .enumerate()
        .map(|(at, name)| {
            rows.iter()
                .map(|row| row[at].chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();

    let line = |cells: Vec<&str>| {
        let mut line = String::from(" ");

        for (cell, width) in cells.iter().zip(&widths) {
            line.push_str(&format!(" {cell:<width$}"));
        }

        line.trim_end().to_owned() + "\n"
    };

    let mut out = line(header.to_vec());

    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }

    out
}

fn yes_no(yes: bool) -> &'static str {
    if yes { "yes" } else { "no" }
}

fn or_dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text.to_owned()
    }
}

/// The plural of a kind of thing the placeholders stand for.
fn plural(kind: &str) -> String {
    match kind {
        "address" => "addresses".to_owned(),
        kind => format!("{kind}s"),
    }
}

#[cfg(test)]
mod tests {
    use muru_controller::diagnostics::WorkerFacts;

    use super::*;
    use crate::diagnostics::markdown::tests::{cells, outside_code};

    /// Text that Markdown would read as markup, and a tracker as a
    /// mention and an issue.
    const HOSTILE: &str = "a | b `c` ``` d @alice #123 **e** <b>";

    fn release(id: &str, state: &str, serving: bool) -> ReleaseFacts {
        ReleaseFacts {
            app: "acme-shop".to_owned(),
            id: id.to_owned(),
            state: state.to_owned(),
            message: String::new(),
            serving,
            kind: "workers".to_owned(),
            interface: "auto".to_owned(),
            detected_interface: if serving { "wsgi" } else { "" }.to_owned(),
            frameworks: if serving {
                vec!["flask 3.0.3".to_owned()]
            } else {
                Vec::new()
            },
            python_version: "3.13.1".to_owned(),
            python_build: "regular".to_owned(),
            workers: (0..if serving { 2 } else { 0 })
                .map(|index| WorkerFacts {
                    index,
                    group: "default".to_owned(),
                    state: "ready".to_owned(),
                })
                .collect(),
        }
    }

    fn setting(key: &str, value: &str, source: &str) -> Setting {
        Setting {
            scope: "acme-shop@r3".to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
            source: source.to_owned(),
        }
    }

    fn event(sequence: u64, kind: &str, level: &str, message: &str) -> pb::Event {
        pb::Event {
            sequence,
            time_unix_ms: 1_800_000_000_000 + sequence * 1000,
            r#type: kind.to_owned(),
            subject: "acme-shop@r3".to_owned(),
            level: level.to_owned(),
            message: message.to_owned(),
        }
    }

    /// An installation whose every identifying value holds `acme`, an
    /// address of its own or a secret, with text hostile to Markdown in
    /// what it reports.
    fn collected() -> Collected {
        let daemon_file: toml::Table = toml::from_str(
            r#"
            [daemon]
            user = "acmesvc"
            pass_env = ["ACME_TOKEN", "ACME_REGION"]

            [[listener]]
            address = "203.0.113.9:443"
            tls_key = "/etc/acme/tls/key.pem"

            [limits]
            max_body_size = "2MB"

            [dashboard]
            token = "s3cret"

            [events.webhook]
            url = "https://hooks.acme.example/t/s3cret"

            [host."shop.acme.example"]
            names = ["shop.acme.example"]
            "#,
        )
        .unwrap();

        Collected {
            runs: Runs::UserDaemon,
            running: true,
            container: false,
            platform: Platform {
                os: "linux".to_owned(),
                kernel: "6.8.0".to_owned(),
                arch: "x86_64".to_owned(),
            },
            roots: vec![
                (PathBuf::from("/var/acme/state"), "<state-dir>"),
                (PathBuf::from("/var/acme/muru.toml"), "<daemon-config>"),
            ],
            home: Some(PathBuf::from("/home/acmeops")),
            node: "web1.acme.example".to_owned(),
            names: Names {
                apps: vec!["acme-shop".to_owned()],
                hosts: vec!["shop.acme.example".to_owned()],
                users: vec!["acmesvc".to_owned()],
            },
            user: Some("acmeops".to_owned()),
            releases: Ok(vec![
                release("r3", "ready", true),
                release("r2", "stopped", false),
            ]),
            settings: Ok(vec![
                setting(
                    "entrypoint",
                    "acme.shop.wsgi:application",
                    "[tool.muru] in /home/acmeops/work/acme-shop/pyproject.toml",
                ),
                setting(
                    "python",
                    "/home/acmeops/work/acme-shop/.venv/bin/python",
                    "the deploy command",
                ),
                setting("processes", "2", HOSTILE),
                setting("run.hosts", "[\"shop.acme.example\"]", "command line"),
                setting(
                    "bytecode",
                    "redirect, under /var/acme/state/bytecode/acme-shop/r3",
                    "default",
                ),
                setting("threads", "4", "default"),
            ]),
            daemon_file: Some(Ok(Some(daemon_file))),
            events: Ok(vec![
                event(1, "release.ready", "info", "release acme-shop@r3 is ready"),
                event(2, "release.ready", "info", "release acme-shop@r3 is ready"),
                event(
                    3,
                    "release.failed",
                    "error",
                    &format!(
                        "release acme-shop@r4 failed on web1: /srv/acme/shop/app.py from \
                         203.0.113.9 said {HOSTILE}"
                    ),
                ),
                event(
                    4,
                    "debug.exec",
                    "warning",
                    "ran code in worker 0: print(acme_secret)",
                ),
            ]),
            notes: vec![NotCollected::new(
                "frameworks",
                "worker 0 of acme-shop@r3 was asked: no answer within 5 seconds",
            )],
            now_unix_ms: 1_800_000_000_000 + 60_000,
        }
    }

    #[test]
    fn nothing_that_identifies_the_system_is_in_any_format() {
        let report = Report::build(collected());
        let json = serde_json::to_string_pretty(&report).unwrap();

        for (format, output) in [
            ("text", report.text()),
            ("markdown", report.markdown()),
            ("json", json),
        ] {
            for identifying in [
                "acme",
                "ACME",
                "203.0.113",
                "/srv",
                "/home",
                "/etc",
                "/var",
                "web1",
                "s3cret",
                "hooks",
            ] {
                assert!(
                    !output.contains(identifying),
                    "{identifying} is in the {format} report:\n{output}"
                );
            }

            // The placeholders that stand for them are.

            for placeholder in [
                "app1@r3",
                "<module1>:application",
                "~/<path",
                "<state-dir>/bytecode/app1/r3",
                "<address1>:443",
                "<redacted>",
                "user1",
            ] {
                assert!(
                    output.contains(placeholder),
                    "{placeholder} is not in the {format} report:\n{output}"
                );
            }
        }
    }

    #[test]
    fn the_same_value_reads_the_same_wherever_it_appears() {
        let report = Report::build(collected());

        let releases = report.releases.as_ref().unwrap();
        assert_eq!(releases[0].release, "app1@r3");
        assert_eq!(releases[1].release, "app1@r2");

        let settings = report.settings.as_ref().unwrap();
        assert!(settings.iter().all(|setting| setting.scope == "app1@r3"));

        // The interpreter and the project's file are under one home, and
        // each path keeps its own number.

        assert_eq!(settings[0].source, "[tool.muru] in ~/<path1>");
        assert_eq!(settings[1].value, "~/<path2>");

        let events = report.events.as_ref().unwrap();
        assert_eq!(events.recent[0].subject, "app1@r3");
        assert!(
            events.recent[0]
                .message
                .starts_with("release app1@r4 failed on node: <path3> ")
        );
        assert!(events.recent[0].message.contains("from <address1> said"));

        // The listener's address is the one the event named.

        let daemon = report.daemon_settings.as_ref().unwrap();
        let of = |key: &str| {
            daemon
                .iter()
                .find(|setting| setting.key == key)
                .map(|setting| setting.value.as_str())
        };
        assert_eq!(of("listener[0].address"), Some("<address1>:443"));
        assert_eq!(of("listener[0].tls_key"), Some("<redacted>"));
        assert_eq!(of("daemon.user"), Some("user1"));
        assert_eq!(of("daemon.pass_env"), Some("2 names, left out"));
        assert_eq!(of("dashboard.token"), Some("<redacted>"));
        assert_eq!(of("limits.max_body_size"), Some("2MB"));
        assert_eq!(of("events"), Some("1 entry, left out"));
        assert_eq!(of("host"), Some("1 entry, left out"));

        assert_eq!(
            report.replaced,
            Replaced {
                applications: 1,
                hosts: 1,
                users: 1,
                paths: 3,
                addresses: 1,
                modules: 1,
            }
        );
    }

    #[test]
    fn a_setting_is_shown_only_when_its_value_is_known_to_be_safe() {
        let report = Report::build(collected());
        let settings = report.settings.as_ref().unwrap();
        let of = |key: &str| settings.iter().find(|setting| setting.key == key).unwrap();

        assert_eq!(of("processes").value, "2");
        assert_eq!(of("run.hosts").value, "<set>");
        assert_eq!(of("entrypoint").value, "<module1>:application");
    }

    #[test]
    fn events_are_counted_and_the_serious_ones_listed() {
        let report = Report::build(collected());
        let events = report.events.as_ref().unwrap();
        let counts: Vec<(&str, &str, usize)> = events
            .counts
            .iter()
            .map(|count| (count.r#type.as_str(), count.level.as_str(), count.count))
            .collect();

        assert_eq!(
            counts,
            [
                ("debug.exec", "warning", 1),
                ("release.failed", "error", 1),
                ("release.ready", "info", 2),
            ]
        );

        // Oldest first, and a sentence that quotes code is left out.

        assert_eq!(events.recent.len(), 2);
        assert_eq!(events.recent[0].r#type, "release.failed");
        assert_eq!(events.recent[1].r#type, "debug.exec");
        assert_eq!(events.recent[1].message, LEFT_OUT);
        assert_eq!(events.recent[0].time, "2027-01-15T08:00:03Z");

        // An event before the window is not counted, and still listed
        // when it is among the most recent warnings and errors.

        let mut later = collected();
        later.now_unix_ms += 25 * 3_600_000;
        let report = Report::build(later);
        let events = report.events.as_ref().unwrap();
        assert!(events.counts.is_empty());
        assert_eq!(events.recent.len(), 2);
    }

    #[test]
    fn markdown_keeps_what_came_from_the_system_inside_code() {
        let markdown = Report::build(collected()).markdown();
        let outside = outside_code(&markdown);

        // Nothing a tracker would take for a mention, an issue, emphasis
        // or HTML is left outside a code span or a fenced block.

        for markup in ["@", "#1", "*", "<", "`"] {
            assert!(
                !outside.contains(markup),
                "{markup} is outside code in:\n{outside}"
            );
        }

        assert!(markdown.contains("@alice #123"));

        // Every row of a table has as many cells as the table's header.

        let mut columns = None;

        for line in markdown.lines() {
            if !line.starts_with('|') {
                columns = None;
                continue;
            }

            let found = cells(line).len();
            assert_eq!(
                *columns.get_or_insert(found),
                found,
                "a row of another width: {line}"
            );
        }
    }

    #[test]
    fn the_three_formats_say_the_same() {
        let report = Report::build(collected());
        let text = report.text();
        let markdown = report.markdown();
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();

        for release in json["releases"].as_array().unwrap() {
            for field in ["release", "state", "python"] {
                let value = release[field].as_str().unwrap();
                assert!(text.contains(value), "{value} in the text");
                assert!(markdown.contains(value), "{value} in the Markdown");
            }
        }

        for setting in json["settings"].as_array().unwrap() {
            let pair = format!(
                "{} = {}",
                setting["key"].as_str().unwrap(),
                setting["value"].as_str().unwrap()
            );
            assert!(text.contains(&pair), "{pair} in the text");
        }

        for setting in json["daemon_settings"].as_array().unwrap() {
            let key = setting["key"].as_str().unwrap();
            assert!(text.contains(key), "{key} in the text");
            assert!(markdown.contains(key), "{key} in the Markdown");
        }

        for format in [&text, &markdown] {
            assert!(format.contains("a user daemon, running, not in a container"));
            assert!(format.contains("flask 3.0.3"));
            assert!(format.contains("2 ready"));
            assert!(format.contains("worker 0 of app1@r3 was asked"));
        }

        // Settings at their defaults are folded onto a line of their own.

        assert!(text.contains("    at their defaults:\n      bytecode = redirect, under"));
        assert!(markdown.contains("At their defaults for `app1@r3`: `bytecode = redirect, under"));
        assert!(markdown.contains("`threads = 4`"));
    }

    #[test]
    fn a_part_that_could_not_be_collected_says_so_in_its_place() {
        let mut missing = collected();
        missing.releases = Err("no daemon is running, and there is no state store at \
                                /var/acme/state/muru.sqlite"
            .to_owned());
        missing.settings = Err("the serving releases are not known".to_owned());
        missing.events = Err("the store is locked".to_owned());
        missing.daemon_file = Some(Err("/var/acme/muru.toml could not be parsed".to_owned()));
        missing.running = false;

        let report = Report::build(missing);
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();

        for part in ["releases", "settings", "events", "daemon_settings"] {
            assert!(json[part].is_null(), "{part}");
        }

        let parts: Vec<&str> = report
            .not_collected
            .iter()
            .map(|note| note.part.as_str())
            .collect();
        assert_eq!(
            parts,
            [
                "frameworks",
                "releases",
                "settings",
                "events",
                "daemon settings"
            ]
        );

        let text = report.text();
        assert!(text.contains("Runs as a user daemon, not running"));
        assert!(!text.contains("RELEASES"));
        assert!(text.contains(
            "  releases: no daemon is running, and there is no state store at \
             <state-dir>/muru.sqlite\n"
        ));
        assert!(text.contains("  daemon settings: <daemon-config> could not be parsed\n"));

        let markdown = report.markdown();
        assert!(markdown.contains("#### Not collected"));
        assert!(!markdown.contains("#### Releases"));

        // A development run has no daemon settings to miss.

        let mut run = collected();
        run.runs = Runs::Run;
        run.daemon_file = None;
        let report = Report::build(run);
        assert!(report.daemon_settings.is_none());
        assert!(!report.text().contains("DAEMON SETTINGS"));
        assert!(report.text().starts_with(&format!(
            "Muru {} on linux x86_64 (kernel 6.8.0)\nRuns as muructl run, running",
            env!("CARGO_PKG_VERSION")
        )));
    }
}
