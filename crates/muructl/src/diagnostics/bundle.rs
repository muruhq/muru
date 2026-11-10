//! `muructl debug bundle` (design specification section 11.9): an archive
//! of a whole system for a time window, made on request for the people
//! who own the system to triage with. Unlike the report it holds real log
//! lines and addresses, so it is not for posting.
//!
//! It is made on systems that are in trouble. It reads the daemon's files
//! where it can instead of asking the daemon, gives each question it does
//! ask a time limit of its own, and records a part it cannot collect in
//! the manifest and carries on. The archive is written as a stream at a
//! low priority, readable by its owner alone.

use std::io::{self, BufRead, BufReader, IsTerminal as _, Read, Seek as _, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use clap::Args;
use muru_controller::diagnostics::archive::{Archive, Entry, FORMAT, Manifest, layout};
use muru_controller::diagnostics::facts::{self, Platform};
use muru_controller::diagnostics::logs::{Packed, Stretch, stretches};
use muru_controller::diagnostics::stored;
use muru_controller::diagnostics::window::{Window, line_time};
use muru_controller::diagnostics::{ReleaseFacts, changes_configuration, mask, period};
use muru_controller::log_files::{self, Kind, LogFile};
use muru_proto::admin::v1 as pb;
use serde::Serialize;
use tokio_stream::StreamExt as _;

use super::scrub::Scrubber;
use super::{ASK, Installation, within};
use crate::admin::{DaemonFileArgs, SocketArgs, format_time, human_size};

/// What a planned archive says of a part a running daemon is asked for.
const ASKED: &str = "asked for when the bundle is made";

/// Why a part that only what runs can answer for is missing when
/// nothing runs.
const NOTHING_RUNS: &str = "nothing is running to ask";

/// How many lines a development run or `muructl serve` is asked for:
/// more than it keeps in memory.
const EVERY_LINE: u32 = 1_000_000;

/// What a bundle never holds, as its manifest says.
const NEVER_HELD: [&str; 3] = [
    "secret values",
    "environment variable values beyond those muructl debug inspect shows, which masks the \
     ones that look like secrets",
    "request and response bodies",
];

/// What --scrub takes out of a bundle, as its manifest says.
const SCRUBBED: &str = "client addresses, which read as <address1> and so on, the same one for the \
                        same address, and the query strings of request targets (--scrub); a line \
                        of an application's own output is free text, where only what has the \
                        shape of either is found";

/// `muructl debug bundle`.
#[derive(Args)]
pub struct BundleArgs {
    /// Only this application, or one release of it as app@release; the
    /// whole node when left out.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    /// How far back the bundle reaches, as a duration such as 30m or 2d.
    #[arg(long, value_name = "DURATION", default_value = "1h", value_parser = muru_config::parse_duration)]
    since: Duration,

    /// Where the archive is written, or - for standard output; a file
    /// named for the time, in the current directory, when left out.
    #[arg(long, short, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Print what would be collected and about how large it would be,
    /// and write nothing.
    #[arg(long)]
    dry_run: bool,

    /// Replace client addresses with placeholders and cut query strings,
    /// for a bundle that goes to someone with a narrower need.
    #[arg(long)]
    scrub: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// Where the daemon's state, logs and configuration are, when they
    /// are not where the daemon in reach says or the defaults put them.
    #[command(flatten)]
    files: DaemonFileArgs,
}

/// What a bundle is limited to: an application, one release of it, or
/// nothing, which is the whole node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Target {
    app: String,
    release: String,
}

impl Target {
    /// The target a command line names.
    fn parse(target: Option<&str>) -> anyhow::Result<Self> {
        match target.map(|target| target.split_once('@')) {
            None => Ok(Self::default()),
            Some(None) => Ok(Self {
                app: target.unwrap_or_default().to_owned(),
                release: String::new(),
            }),
            Some(Some((app, release))) if !app.is_empty() && !release.is_empty() => Ok(Self {
                app: app.to_owned(),
                release: release.to_owned(),
            }),
            Some(Some(_)) => anyhow::bail!(
                "{:?} is not a release: expected app@release",
                target.unwrap_or_default()
            ),
        }
    }

    /// The target as it was written, empty for the whole node.
    fn name(&self) -> String {
        match (self.app.as_str(), self.release.as_str()) {
            ("", _) => String::new(),
            (app, "") => app.to_owned(),
            (app, release) => format!("{app}@{release}"),
        }
    }

    /// What the bundle is of, as a phrase.
    fn describe(&self) -> String {
        if self.app.is_empty() {
            "the whole node".to_owned()
        } else {
            self.name()
        }
    }

    /// Whether a release is within the target.
    fn holds(&self, app: &str, release: &str) -> bool {
        (self.app.is_empty() || self.app == app)
            && (self.release.is_empty() || self.release == release)
    }
}

/// What `installation.json` holds: how Muru runs, where, and its
/// releases, as the report summarises them and without its placeholders.
#[derive(Serialize)]
struct Summary<'a> {
    muru: &'static str,
    platform: Platform,
    node: String,
    runs: &'static str,
    running: bool,
    container: bool,
    /// What answered on the admin socket said of itself.
    status: Option<&'a pb::Status>,
    /// The daemon's locations, each with the rule that chose it.
    locations: Option<serde_json::Value>,
    releases: &'a [ReleaseFacts],
}

/// A bundle on its way into an archive, written or only planned.
struct Bundle<'a, W: Write> {
    installation: &'a mut Installation,
    archive: Archive<W>,
    window: Window,
    target: &'a Target,
    /// What takes client addresses and query strings out, with --scrub.
    scrubber: Option<Scrubber>,
    /// The events of the window, once they have been read.
    events: Option<Vec<pb::Event>>,
    /// The metrics as the daemon gave them, once it has been asked.
    metrics: Option<Result<String, String>>,
}

/// A bundle collected: its archive, and what collecting it found out.
struct Collected<W: Write> {
    archive: Archive<W>,
    /// Whether the configuration changed during the window, when the
    /// events could be read to tell.
    configuration_changed: Option<bool>,
}

impl<W: Write> Bundle<'_, W> {
    /// Whether a part that a running daemon answers for is to be asked
    /// for now. A planned archive only lists such a part, since a plan
    /// asks nothing of a system that may be in trouble.
    fn asks(&mut self, path: &str) -> bool {
        if self.archive.is_planned() && self.installation.running() {
            self.archive.would_ask(path, ASKED);
            return false;
        }

        true
    }

    /// Add a part as JSON, or record why there is none.
    fn put<T: Serialize>(&mut self, path: &str, found: Result<T, String>) -> io::Result<()> {
        put_into(&mut self.archive, path, found)
    }

    /// Collect every part. Only a failure to write the archive itself is
    /// an error: a part that cannot be collected is recorded and the
    /// rest carries on.
    async fn collect(mut self) -> io::Result<Collected<W>> {
        self.summary().await?;
        self.events().await?;
        let configuration_changed = self.configuration().await?;
        self.logs().await?;

        // What was noted while collecting, such as a worker that did not
        // answer, is recorded as what it left out.

        for note in self.installation.take_notes() {
            self.archive.missing(&note.part, note.why);
        }

        Ok(Collected {
            archive: self.archive,
            configuration_changed,
        })
    }

    /// Add a part as text, or record why there is none.
    fn put_text(&mut self, path: &str, found: Result<String, String>) -> io::Result<()> {
        match found {
            Ok(text) => self.archive.bytes(path, text.as_bytes(), None),
            Err(why) => {
                self.archive.missing(path, why);
                Ok(())
            }
        }
    }

    /// The metrics of what runs, asked for once.
    async fn metrics(&mut self) -> Result<String, String> {
        if self.metrics.is_none() {
            let asked = match &mut self.installation.live {
                Some(live) => within(live.client.get_metrics(pb::GetMetricsRequest {}))
                    .await
                    .map(|answer| answer.text),
                None => Err(NOTHING_RUNS.to_owned()),
            };

            self.metrics = Some(asked);
        }

        self.metrics
            .clone()
            .expect("the metrics were just asked for")
    }

    /// `config/`: the configuration in force on the system. With the
    /// state store in reach, every release's effective settings and its
    /// specification, the whole effective configuration and what is in
    /// force outside it; without, what `muructl config show --effective`
    /// prints. Beside them the daemon's own file, what only the running
    /// daemon knows, and every change the window's events record.
    ///
    /// The answer is whether the configuration changed in the window.
    async fn configuration(&mut self) -> io::Result<Option<bool>> {
        let config = layout::CONFIG;
        let target = self.target;
        let app = (!target.app.is_empty()).then_some(target.app.as_str());

        if let Ok(store) = &self.installation.store {
            let releases = stored::releases(store).map_err(|error| error.to_string());

            match releases {
                Ok(releases) => {
                    for release in releases
                        .iter()
                        .filter(|release| target.holds(&release.app, &release.id))
                    {
                        let record = stored::release_record(store, release)
                            .map_err(|error| error.to_string());
                        let path = format!("{config}/releases/{}.json", release.name());
                        put_into(&mut self.archive, &path, record)?;
                    }
                }
                Err(why) => self.archive.missing(&format!("{config}/releases"), why),
            }

            let effective = stored::effective(store, app);
            let in_force = stored::in_force(store).map_err(|error| error.to_string());
            self.put_text(&format!("{config}/effective.toml"), effective)?;
            self.put(&format!("{config}/in-force.json"), in_force)?;
        } else {
            let path = format!("{config}/effective.toml");

            if self.asks(&path) {
                let shown = self.effective_as_shown().await;
                self.put_text(&path, shown)?;
            }

            let why = self.installation.store.as_ref().err().cloned();
            self.archive
                .missing(&format!("{config}/in-force.json"), why.unwrap_or_default());
        }

        // The daemon's own file, with what looks like a secret masked.

        if self.installation.runs().is_daemon() {
            let path = format!("{config}/daemon.toml");

            match self.installation.daemon_file() {
                Ok(Some((_, mut table))) => {
                    mask::toml_table(&mut table);
                    let text = toml::to_string(&table).map_err(|error| error.to_string());
                    self.put_text(&path, text)?;
                }
                Ok(None) => self.archive.missing(
                    &path,
                    format!(
                        "there is no configuration file at {}",
                        self.installation
                            .locations()
                            .map(|locations| locations.config.path.display().to_string())
                            .unwrap_or_default()
                    ),
                ),
                Err(why) => self.archive.missing(&path, why),
            }
        }

        // What only the running daemon knows: the changes to the access
        // log in force, and the routing snapshot the edge routes by,
        // which an access log record names.

        let path = format!("{config}/access-log-overrides.json");

        if self.asks(&path) {
            let asked = match &mut self.installation.live {
                Some(live) => within(live.client.override_access_log(
                    pb::OverrideAccessLogRequest {
                        mode: String::new(),
                        app: String::new(),
                        paths: Vec::new(),
                        duration_ms: 0,
                        reset: false,
                    },
                ))
                .await
                .map(|answer| answer.overrides),
                None => Err(NOTHING_RUNS.to_owned()),
            };

            self.put(&path, asked)?;
        }

        let path = format!("{config}/routing-snapshot.json");

        if self.asks(&path) {
            let version = self.metrics().await.and_then(|text| {
                crate::admin::metric_samples(&text)
                    .iter()
                    .find(|sample| sample["name"] == "muru_snapshot_version")
                    .map(|sample| serde_json::json!({"version": sample["value"]}))
                    .ok_or_else(|| "the metrics name no routing snapshot".to_owned())
            });
            self.put(&path, version)?;
        }

        // What Muru keeps no record of.

        self.archive.missing(
            &format!("{config}/routing-snapshots"),
            "only the routing snapshot in force is kept, not those before it: the events in              changes.jsonl say what changed, and when",
        );
        self.archive.missing(
            &format!("{config}/logger-levels.json"),
            "the levels muructl logging level changed are held by each worker, and nothing              lists them",
        );

        // Every change the window's events record.

        let Some(events) = &self.events else {
            self.archive.missing(
                &format!("{config}/changes.jsonl"),
                "the events of the window were not read",
            );
            return Ok(None);
        };

        let mut lines = Vec::new();
        let mut changes = 0;

        for event in events
            .iter()
            .filter(|event| changes_configuration(&event.r#type))
        {
            lines.extend(event_line(&mut self.scrubber, event));
            changes += 1;
        }

        let note = match changes {
            0 => "the configuration did not change during the window".to_owned(),
            changes => format!(
                "the configuration changed {} during the window: the rest of config/ is as it \
                 was when the bundle was made",
                crate::admin::counted(changes, "time", "times")
            ),
        };

        self.archive
            .bytes(&format!("{config}/changes.jsonl"), &lines, Some(note))?;
        Ok(Some(changes > 0))
    }

    /// The effective configuration as `muructl config show --effective`
    /// prints it, asked of what runs: what each overlay sets, each
    /// release's settings with where each came from, then the document,
    /// its variables masked as a worker's environment is.
    async fn effective_as_shown(&mut self) -> Result<String, String> {
        let Some(live) = &mut self.installation.live else {
            return Err(NOTHING_RUNS.to_owned());
        };

        let export = within(live.client.export_config(pb::ExportConfigRequest {
            app: self.target.app.clone(),
            effective: true,
            resolve_rollouts: false,
        }))
        .await?;
        let mut text = String::new();

        for (heading, lines) in [
            (
                "# The active overlays set, and the routes below include:",
                &export.attributions,
            ),
            (
                "# The releases' settings and the routes' limits, and the layer each came from:",
                &export.layers,
            ),
        ] {
            if !lines.is_empty() {
                text.push_str(heading);
                text.push('\n');

                for line in lines {
                    text.push_str(&format!("#   {line}\n"));
                }

                text.push('\n');
            }
        }

        text.push_str(&mask::document_env(&export.document)?);
        Ok(text)
    }

    /// `installation.json`: how Muru runs and its releases.
    async fn summary(&mut self) -> io::Result<()> {
        if !self.asks(layout::INSTALLATION) {
            return Ok(());
        }

        let releases = self.installation.releases().await;
        let target = self.target;

        let found = releases.map(|releases| {
            let releases: Vec<ReleaseFacts> = releases
                .into_iter()
                .filter(|release| target.holds(&release.app, &release.id))
                .collect();
            let installation = &*self.installation;

            let locations = installation.locations().map(|locations| {
                let place = |location: &muru_controller::locations::Location| {
                    serde_json::json!({
                        "path": location.path.display().to_string(),
                        "rule": location.rule,
                    })
                };

                serde_json::json!({
                    "config": place(&locations.config),
                    "state": place(&locations.state),
                    "run": place(&locations.run),
                    "logs": place(&locations.logs),
                    "cache": place(&locations.cache),
                })
            });

            serde_json::to_value(Summary {
                muru: env!("CARGO_PKG_VERSION"),
                platform: Platform::read(),
                node: muru_controller::config::hostname(),
                runs: installation.runs().describe(),
                running: installation.running(),
                container: facts::in_container(),
                status: installation.status(),
                locations,
                releases: &releases,
            })
            .unwrap_or_default()
        });

        self.put(layout::INSTALLATION, found)
    }

    /// `events.jsonl`: every event of the window, in full, a JSON object
    /// a line. With a target, the events of other applications are left
    /// out, and those of the node stay.
    async fn events(&mut self) -> io::Result<()> {
        // The events are read from the state store where there is one,
        // and what runs is asked only without it.

        if !self.installation.reads_store() && !self.asks(layout::EVENTS) {
            return Ok(());
        }

        let others = self.other_applications().await;
        let window = self.window;
        let target = self.target;

        let found = self.installation.events().await.map(|events| {
            events
                .into_iter()
                .filter(|event| {
                    let (app, release) = event
                        .subject
                        .split_once('@')
                        .unwrap_or((event.subject.as_str(), ""));
                    let another = others.iter().any(|other| other == app)
                        || (app == target.app
                            && !release.is_empty()
                            && !target.holds(app, release));

                    window.holds(event.time_unix_ms) && !another
                })
                .collect::<Vec<_>>()
        });

        match found {
            Ok(events) => {
                let mut lines = Vec::new();

                for event in &events {
                    lines.extend(self.event_line(event));
                }

                self.events = Some(events);
                self.archive.bytes(layout::EVENTS, &lines, None)
            }
            Err(why) => {
                self.archive.missing(layout::EVENTS, why);
                Ok(())
            }
        }
    }

    /// An event as a line of JSON, its sentence scrubbed when asked.
    fn event_line(&mut self, event: &pb::Event) -> Vec<u8> {
        event_line(&mut self.scrubber, event)
    }

    /// `logs/`: the logs of the window, by their names in the log
    /// directory. A daemon's are read from its files, each from where the
    /// window starts in it, with the archives the rotation left in the
    /// window; a development run and `muructl serve` write none, and are
    /// asked for the lines they keep in memory.
    async fn logs(&mut self) -> io::Result<()> {
        let Some(directory) = self
            .installation
            .locations()
            .map(|locations| locations.logs.path.clone())
        else {
            return self.kept_lines().await;
        };

        let existing = self
            .installation
            .store
            .as_ref()
            .ok()
            .and_then(|store| log_files::Existing::read(store).ok());

        let listed = match log_files::list(&directory, existing.as_ref()) {
            Ok(listed) => listed,
            Err(error) => {
                self.archive.missing(
                    layout::LOGS,
                    format!("{} could not be read: {error}", directory.display()),
                );
                return Ok(());
            }
        };

        for log in &listed {
            // Another application's output is not part of a bundle of one.

            if log.kind == Kind::Release && !self.target.holds(&log.app, &log.release) {
                continue;
            }

            for found in stretches(&directory, log, self.window) {
                match found {
                    Ok(stretch) => self.stretch(log, stretch)?,
                    Err((name, why)) => self
                        .archive
                        .missing(&format!("{}/{name}", layout::LOGS), why),
                }
            }
        }

        Ok(())
    }

    /// Add one stretch of a log. It is copied as it is, unless its lines
    /// are to be scrubbed or, for the records of an access log, limited
    /// to the bundle's application: then each line is passed through,
    /// and a compressed archive is decompressed to do it.
    fn stretch(&mut self, log: &LogFile, mut stretch: Stretch) -> io::Result<()> {
        let records = matches!(log.kind, Kind::Access | Kind::Host);
        let wanted = (records && !self.target.app.is_empty()).then(|| self.target.clone());
        let path = format!("{}/{}", layout::LOGS, stretch.name);

        let mut note = match (stretch.packed, stretch.archive) {
            (Some(_), _) => "the whole archive, which was last written in the window".to_owned(),
            (None, true) => "the lines of the window, from an archive".to_owned(),
            (None, false) => "the lines of the window".to_owned(),
        };

        if wanted.is_none() && self.scrubber.is_none() {
            stretch.file.seek(SeekFrom::Start(stretch.start))?;
            return self
                .archive
                .stream(&path, stretch.length, &stretch.file, Some(note));
        }

        // Each line is passed through, so the size of what comes out is
        // not known until it has been. A plan gives the size going in.

        if let Some(target) = &wanted {
            note.push_str(&format!(", the records of {} alone", target.name()));
        }

        if self.scrubber.is_some() {
            note.push_str(", scrubbed");
        }

        let path = Packed::stem(&path).to_owned();

        if self.archive.is_planned() {
            note.push_str(": its size before that");
            return self
                .archive
                .stream(&path, stretch.length, io::empty(), Some(note));
        }

        // Once to learn the size, and again to write: what comes out is
        // the same both times, since the stretch is.

        let mut passing = Passing {
            wanted: wanted.as_ref(),
            scrubber: self.scrubber.as_mut(),
        };

        let size = match passed(&mut stretch, &mut passing) {
            Ok(mut lines) => io::copy(&mut lines, &mut io::sink()),
            Err(error) => Err(error),
        };

        let size = match size {
            Ok(size) => size,
            Err(error) => {
                self.archive.missing(&path, format!("reading it: {error}"));
                return Ok(());
            }
        };

        match passed(&mut stretch, &mut passing) {
            Ok(lines) => self.archive.stream(&path, size, lines, Some(note)),
            Err(error) => {
                self.archive.missing(&path, format!("reading it: {error}"));
                Ok(())
            }
        }
    }

    /// The logs of a development run or `muructl serve`, which writes no
    /// files: the lines of the window among those it keeps in memory.
    async fn kept_lines(&mut self) -> io::Result<()> {
        let Some(status) = self.installation.status().cloned() else {
            return Ok(());
        };

        let output = format!(
            "{}/apps/{}/{}.log",
            layout::LOGS,
            status.app,
            status.release
        );
        let access = format!("{}/access.log", layout::LOGS);

        for (path, records) in [(output, false), (access, true)] {
            if !self.asks(&path) {
                continue;
            }

            match self.tailed(records).await {
                Ok(lines) => {
                    let mut text = Vec::new();
                    let mut within_window = false;

                    // A line without a time goes with the one before it.

                    for line in &lines {
                        // A record is kept with its line end, and a line
                        // of output without.

                        let line = line.trim_end_matches('\n');

                        if let Some(time) = line_time(line.as_bytes()) {
                            within_window = self.window.holds(time);
                        }

                        if within_window {
                            let line = match &mut self.scrubber {
                                Some(scrubber) => scrubber.line(line),
                                None => line.to_owned(),
                            };

                            text.extend(line.into_bytes());
                            text.push(b'\n');
                        }
                    }

                    let note = format!(
                        "the lines of the window among those {} keeps in memory",
                        self.installation.runs().describe()
                    );
                    self.archive.bytes(&path, &text, Some(note))?;
                }
                Err(why) => self.archive.missing(&path, why),
            }
        }

        Ok(())
    }

    /// The lines a development run or `muructl serve` keeps: its output,
    /// or with `records` its access log's.
    async fn tailed(&mut self, records: bool) -> Result<Vec<String>, String> {
        let Some(live) = &mut self.installation.live else {
            return Err(NOTHING_RUNS.to_owned());
        };

        let asked = async {
            let mut stream = live
                .client
                .tail_logs(pb::TailLogsRequest {
                    app: String::new(),
                    release: String::new(),
                    follow: false,
                    lines: EVERY_LINE,
                    access: records,
                    host: String::new(),
                })
                .await
                .map_err(|status| status.message().to_owned())?
                .into_inner();
            let mut lines = Vec::new();

            while let Some(line) = stream.next().await {
                lines.push(line.map_err(|status| status.message().to_owned())?.line);
            }

            Ok(lines)
        };

        tokio::time::timeout(ASK, asked)
            .await
            .map_err(|_| format!("no answer within {} seconds", ASK.as_secs()))?
    }

    /// The applications the bundle is not of, when it is limited to one.
    async fn other_applications(&mut self) -> Vec<String> {
        if self.target.app.is_empty() {
            return Vec::new();
        }

        let mut others: Vec<String> = self
            .installation
            .releases_known()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(app, _)| app)
            .filter(|app| *app != self.target.app)
            .collect();
        others.dedup();
        others
    }
}

/// An event as a line of JSON, its subject and sentence scrubbed when a
/// scrubber is given.
fn event_line(scrubber: &mut Option<Scrubber>, event: &pb::Event) -> Vec<u8> {
    let mut line = match scrubber {
        Some(scrubber) => {
            let mut event = event.clone();
            event.subject = scrubber.text(&event.subject);
            event.message = scrubber.text(&event.message);
            serde_json::to_vec(&event)
        }
        None => serde_json::to_vec(event),
    }
    .unwrap_or_default();
    line.push(b'\n');
    line
}

/// What each line of a log is passed through on its way into a bundle.
struct Passing<'a> {
    /// The application, or the release, whose access records alone are
    /// kept, when the bundle is limited to one.
    wanted: Option<&'a Target>,
    /// What scrubs each line, with --scrub.
    scrubber: Option<&'a mut Scrubber>,
}

impl Passing<'_> {
    /// Pass one line, with its line end, into `out`: nothing for a
    /// record of another application.
    fn pass(&mut self, line: &[u8], out: &mut Vec<u8>) {
        // A record names its application and release as JSON strings,
        // and a quote inside a string is escaped, so the field is found
        // as written. A line that is no JSON record names neither, and
        // is kept.

        if let Some(target) = self.wanted
            && line.starts_with(b"{")
        {
            let named = |field: &str, value: &str| {
                let written = format!("\"{field}\":\"{value}\"");
                line.windows(written.len())
                    .any(|found| found == written.as_bytes())
            };

            if !named("app", &target.app)
                || (!target.release.is_empty() && !named("release", &target.release))
            {
                return;
            }
        }

        let Some(scrubber) = &mut self.scrubber else {
            out.extend_from_slice(line);
            return;
        };

        let text = String::from_utf8_lossy(line);

        let (text, ending) = match text.strip_suffix('\n') {
            Some(text) => (text, "\n"),
            None => (&*text, ""),
        };

        out.extend_from_slice(scrubber.line(text).as_bytes());
        out.extend_from_slice(ending.as_bytes());
    }
}

/// The lines of a source, each passed through, as a reader.
struct Passed<'a, 'b, R> {
    source: R,
    passing: &'a mut Passing<'b>,
    /// What has been passed and not yet read.
    ready: Vec<u8>,
    /// How much of `ready` has been read.
    read: usize,
}

impl<R: BufRead> Read for Passed<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut line = Vec::new();

        while self.read == self.ready.len() {
            self.ready.clear();
            self.read = 0;
            line.clear();

            if self.source.read_until(b'\n', &mut line)? == 0 {
                return Ok(0);
            }

            self.passing.pass(&line, &mut self.ready);
        }

        let most = buffer.len().min(self.ready.len() - self.read);
        buffer[..most].copy_from_slice(&self.ready[self.read..self.read + most]);
        self.read += most;
        Ok(most)
    }
}

/// A reader of a stretch's lines, each passed through, from its start: a
/// compressed archive decompressed on the way.
fn passed<'a, 'b, 's>(
    stretch: &'s mut Stretch,
    passing: &'a mut Passing<'b>,
) -> io::Result<Passed<'a, 'b, BufReader<Box<dyn Read + 's>>>> {
    stretch.file.seek(SeekFrom::Start(stretch.start))?;
    let bytes = (&stretch.file).take(stretch.length);

    let source: Box<dyn Read + 's> = match stretch.packed {
        Some(packed) => packed.reader(bytes)?,
        None => Box::new(bytes),
    };

    Ok(Passed {
        source: BufReader::with_capacity(64 * 1024, source),
        passing,
        ready: Vec::new(),
        read: 0,
    })
}

/// Add a part as JSON to an archive, or record why there is none.
fn put_into<W: Write, T: Serialize>(
    archive: &mut Archive<W>,
    path: &str,
    found: Result<T, String>,
) -> io::Result<()> {
    match found {
        Ok(found) => {
            let mut text = serde_json::to_vec_pretty(&found).map_err(io::Error::other)?;
            text.push(b'\n');
            archive.bytes(path, &text, None)
        }
        Err(why) => {
            archive.missing(path, why);
            Ok(())
        }
    }
}

/// Where an archive goes.
enum Output {
    /// A file, which must not exist yet.
    File(PathBuf),
    /// Standard output.
    Stdout,
}

impl Output {
    /// Where the options put the archive: `-` is standard output, and
    /// with no option it is a file named for the bundle, here.
    fn of(output: Option<&Path>, root: &str) -> Self {
        match output {
            Some(path) if path == Path::new("-") => Output::Stdout,
            Some(path) => Output::File(path.to_path_buf()),
            None => Output::File(PathBuf::from(format!("{root}.tar.gz"))),
        }
    }
}

/// The directory a file is made in.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// A time as the name of a bundle carries it, `20261102-054222`, in UTC.
fn stamp(time: SystemTime) -> String {
    format_time(
        time.duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64),
    )
    .chars()
    .filter(char::is_ascii_digit)
    .enumerate()
    .flat_map(|(at, digit)| (at == 8).then_some('-').into_iter().chain([digit]))
    .collect()
}

/// Make the bundle, or with `--dry-run` say what it would hold.
pub async fn bundle(args: BundleArgs) -> anyhow::Result<()> {
    let now = SystemTime::now();
    let window = Window::last(args.since, now);
    let target = Target::parse(args.target.as_deref())?;
    let root = format!("muru-bundle-{}", stamp(now));
    let mut installation = Installation::reach(&args.socket, &args.files).await?;
    check_target(&mut installation, &target).await?;

    // What would be collected, with the size of what is on disk: the
    // answer to a dry run, and what the room for a file is checked by.

    let output = Output::of(args.output.as_deref(), &root);

    let planned = if args.dry_run || matches!(output, Output::File(_)) {
        let plan = Bundle {
            installation: &mut installation,
            archive: Archive::planned(&root),
            window,
            target: &target,
            scrubber: args.scrub.then(Scrubber::default),
            events: None,
            metrics: None,
        };
        Some(plan.collect().await?.archive)
    } else {
        None
    };

    if args.dry_run {
        let planned = planned.expect("a dry run is planned");
        print_plan(&planned, &target, window, &output);
        return Ok(());
    }

    // What was asked for beyond the default, for the event to say.

    let options: Vec<String> = [(args.scrub, "scrub")]
        .iter()
        .filter(|(asked, _)| *asked)
        .map(|(_, option)| (*option).to_owned())
        .collect();

    // The collection competes with nothing that serves.

    facts::lower_priority();

    match output {
        Output::Stdout => {
            let stdout = io::stdout();

            if stdout.is_terminal() {
                anyhow::bail!(
                    "standard output is a terminal, and a bundle is a compressed archive: send \
                     it to a file or a pipe, or name a file with --output"
                );
            }

            let recorded = record(&mut installation, &target, window, options).await;
            let making = Making {
                root: &root,
                window,
                target: &target,
                scrub: args.scrub,
                recorded,
            };
            let (_, entries) = making
                .write(&mut installation, stdout.lock())
                .await
                .context("writing the bundle")?;

            // Standard output is the archive, so what is said of it goes
            // to standard error.

            eprint!("{}", summary("standard output", None, &entries));
        }
        Output::File(path) => {
            let planned = planned.expect("a file's room is checked against a plan");
            check_room(&path, &planned)?;
            let file = create(&path)?;
            let recorded = record(&mut installation, &target, window, options).await;
            let making = Making {
                root: &root,
                window,
                target: &target,
                scrub: args.scrub,
                recorded,
            };

            // A bundle that could not be finished is not left behind as
            // if it were one.

            let entries = match making
                .write(&mut installation, io::BufWriter::new(file))
                .await
            {
                Ok((_, entries)) => entries,
                Err(error) => {
                    let _ = std::fs::remove_file(&path);
                    return Err(error)
                        .with_context(|| format!("writing the bundle to {}", path.display()));
                }
            };

            let size = std::fs::metadata(&path).map(|found| found.len()).ok();
            print!("{}", summary(&path.display().to_string(), size, &entries));
        }
    }

    Ok(())
}

/// What a bundle being written is: its name, its window, what it is
/// limited to, and how its making was recorded.
struct Making<'a> {
    root: &'a str,
    window: Window,
    target: &'a Target,
    /// Whether client addresses and query strings are taken out.
    scrub: bool,
    recorded: String,
}

impl Making<'_> {
    /// Collect the bundle into `writer`, ending with its manifest, and
    /// give the writer back, flushed, with the parts.
    async fn write<W: Write>(
        self,
        installation: &mut Installation,
        writer: W,
    ) -> io::Result<(W, Vec<Entry>)> {
        let collecting = Bundle {
            installation: &mut *installation,
            archive: Archive::new(writer, self.root, self.window.until_unix_ms / 1000),
            window: self.window,
            target: self.target,
            scrubber: self.scrub.then(Scrubber::default),
            events: None,
            metrics: None,
        };
        let Collected {
            archive,
            configuration_changed,
        } = collecting.collect().await?;
        let entries = archive.entries().to_vec();

        let manifest = Manifest {
            format: FORMAT,
            kind: "bundle".to_owned(),
            created: format_time(self.window.until_unix_ms),
            muru: env!("CARGO_PKG_VERSION").to_owned(),
            node: muru_controller::config::hostname(),
            target: self.target.name(),
            window: self.window,
            runs: installation.runs().describe().to_owned(),
            running: installation.running(),
            options: serde_json::json!({"scrub": self.scrub}),
            configuration_changed,
            recorded: self.recorded,
            left_out: NEVER_HELD
                .iter()
                .chain(self.scrub.then_some(&SCRUBBED))
                .map(|held| (*held).to_owned())
                .collect(),
            parts: Vec::new(),
        };

        let mut writer = archive
            .finish(manifest)?
            .expect("an archive that is written");
        writer.flush()?;
        Ok((writer, entries))
    }
}

/// Refuse a target that names nothing the installation has, since a
/// bundle of nothing would otherwise be made without a word.
async fn check_target(installation: &mut Installation, target: &Target) -> anyhow::Result<()> {
    if target.app.is_empty() {
        return Ok(());
    }

    // What cannot be read cannot rule a target out.

    let Ok(releases) = installation.releases_known().await else {
        return Ok(());
    };

    if !releases.iter().any(|(app, _)| *app == target.app) {
        anyhow::bail!("no application named {}", target.app);
    }

    if !releases
        .iter()
        .any(|(app, release)| target.holds(app, release))
    {
        anyhow::bail!("{} has no release {}", target.app, target.release);
    }

    Ok(())
}

/// Have what runs record that a bundle is being made of it, and say for
/// the manifest how it was recorded, or why it was not.
async fn record(
    installation: &mut Installation,
    target: &Target,
    window: Window,
    options: Vec<String>,
) -> String {
    let Some(live) = &mut installation.live else {
        return "no event: nothing is running to record one".to_owned();
    };

    let noted = within(live.client.note_bundle(pb::NoteBundleRequest {
        app: target.app.clone(),
        release: target.release.clone(),
        since_unix_ms: window.since_unix_ms,
        until_unix_ms: window.until_unix_ms,
        options,
    }))
    .await;

    match noted {
        Ok(_) if installation.runs.is_daemon() => "as the event debug.bundle".to_owned(),
        Ok(_) => format!("as a line of {}'s output", installation.runs.describe()),
        Err(why) => format!("no event: {why}"),
    }
}

/// Refuse to start an archive there is no room for, saying how much it
/// needs, since a full disk is a common reason to make one.
fn check_room<W: Write>(path: &Path, planned: &Archive<W>) -> anyhow::Result<()> {
    let directory = directory_of(path);
    let (_, about) = planned.sizes();

    if let Some(free) = facts::free_bytes(directory)
        && free < about
    {
        anyhow::bail!(
            "the bundle needs about {}, and {} has {} free: write it elsewhere with --output, \
             or to standard output with --output -",
            human_size(about),
            directory.display(),
            human_size(free)
        );
    }

    Ok(())
}

/// Create the file an archive is written to, readable and writable by
/// its owner alone from the start. A file that exists is left alone: it
/// would keep the mode it has.
fn create(path: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => anyhow::anyhow!(
                "{} exists: name another file with --output, or remove it",
                path.display()
            ),
            _ => anyhow::anyhow!("creating {}: {error}", path.display()),
        })
}

/// The parts that are in an archive, and those that are not.
fn split(entries: &[Entry]) -> (Vec<&Entry>, Vec<&Entry>) {
    entries.iter().partition(|entry| entry.collected)
}

/// What is said once a bundle is written: where it is, what it holds
/// and what it could not, and that it is to be handled with care.
fn summary(place: &str, size: Option<u64>, entries: &[Entry]) -> String {
    let (collected, missing) = split(entries);
    let size = size.map_or_else(String::new, |size| format!(" ({})", human_size(size)));
    let mut out = format!(
        "muru: wrote the bundle to {place}{size}: {}, {} not collected\n",
        crate::admin::counted(collected.len(), "part", "parts"),
        missing.len()
    );

    for entry in missing {
        out.push_str(&format!(
            "  {}: {}\n",
            entry.path,
            entry.why.as_deref().unwrap_or_default()
        ));
    }

    out.push_str(
        "muru: it holds production data (logs, client addresses and configuration) and only \
         its owner can read it: handle it as such\n",
    );
    out
}

/// Print what a bundle would hold.
fn print_plan<W: Write>(planned: &Archive<W>, target: &Target, window: Window, output: &Output) {
    let (collected, missing) = split(planned.entries());
    let reach = period((window.until_unix_ms - window.since_unix_ms) / 1000);
    println!(
        "A bundle of {}, reaching back {reach} to {}, would hold:",
        target.describe(),
        format_time(window.since_unix_ms)
    );

    let width = collected
        .iter()
        .map(|entry| entry.path.len())
        .max()
        .unwrap_or(0);

    for entry in &collected {
        let held = match (entry.bytes, &entry.note) {
            (Some(bytes), Some(note)) => format!("{}, {note}", human_size(bytes)),
            (Some(bytes), None) => human_size(bytes),
            (None, note) => note.clone().unwrap_or_default(),
        };

        println!("  {:<width$}  {held}", entry.path);
    }

    if !missing.is_empty() {
        println!("It would not hold:");
    }

    for entry in &missing {
        println!(
            "  {}: {}",
            entry.path,
            entry.why.as_deref().unwrap_or_default()
        );
    }

    let (raw, about) = planned.sizes();
    println!(
        "About {} once compressed, from {} on disk and what is asked for when it is made.",
        human_size(about),
        human_size(raw)
    );

    if let Output::File(path) = output {
        let directory = directory_of(path);

        if let Some(free) = facts::free_bytes(directory) {
            let named = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
            println!("{} has {} free.", named.display(), human_size(free));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_an_application_a_release_or_the_whole_node() {
        let whole = Target::parse(None).unwrap();
        assert_eq!(whole.describe(), "the whole node");
        assert_eq!(whole.name(), "");
        assert!(whole.holds("shop", "r3"));

        let app = Target::parse(Some("shop")).unwrap();
        assert_eq!(app.name(), "shop");
        assert!(app.holds("shop", "r3") && !app.holds("api", "r3"));

        let release = Target::parse(Some("shop@r3")).unwrap();
        assert_eq!(release.describe(), "shop@r3");
        assert!(release.holds("shop", "r3") && !release.holds("shop", "r2"));

        for wrong in ["shop@", "@r3"] {
            assert_eq!(
                Target::parse(Some(wrong)).unwrap_err().to_string(),
                format!("{wrong:?} is not a release: expected app@release")
            );
        }
    }

    #[test]
    fn a_bundle_is_named_for_when_it_was_made() {
        let time = UNIX_EPOCH + Duration::from_secs(1_793_598_142);
        assert_eq!(stamp(time), "20261102-054222");

        assert!(matches!(
            Output::of(None, "muru-bundle-20261102-054222"),
            Output::File(path) if path == Path::new("muru-bundle-20261102-054222.tar.gz")
        ));
        assert!(matches!(
            Output::of(Some(Path::new("-")), "b"),
            Output::Stdout
        ));
        assert_eq!(directory_of(Path::new("b.tar.gz")), Path::new("."));
        assert_eq!(
            directory_of(Path::new("/var/tmp/b.tar.gz")),
            Path::new("/var/tmp")
        );
    }

    #[test]
    fn what_is_said_of_a_bundle_names_what_it_lacks() {
        let mut archive = Archive::planned("b");
        archive.bytes("installation.json", b"{}", None).unwrap();
        archive.missing("figures/metrics.txt", "no daemon is running");

        assert_eq!(
            summary("b.tar.gz", Some(2048), archive.entries()),
            "muru: wrote the bundle to b.tar.gz (2.0KB): 1 part, 1 not collected\n\
             \x20 figures/metrics.txt: no daemon is running\n\
             muru: it holds production data (logs, client addresses and configuration) and \
             only its owner can read it: handle it as such\n"
        );
    }

    #[test]
    fn a_bundle_there_is_no_room_for_is_refused_with_what_it_needs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("b.tar.gz");

        let mut small = Archive::planned("b");
        small.bytes("installation.json", b"{}", None).unwrap();
        check_room(&path, &small).unwrap();

        // More than any file system has free.

        let mut vast = Archive::planned("b");
        vast.stream("logs/access.log", u64::MAX / 2, io::empty(), None)
            .unwrap();
        let refused = check_room(&path, &vast).unwrap_err().to_string();
        assert!(refused.starts_with("the bundle needs about "), "{refused}");
        assert!(refused.ends_with(
            "free: write it elsewhere with --output, or to standard output with --output -"
        ));

        // The file is made for its owner alone, and one that exists is
        // left as it is.

        use std::os::unix::fs::PermissionsExt as _;

        drop(create(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            create(&path)
                .unwrap_err()
                .to_string()
                .ends_with("exists: name another file with --output, or remove it")
        );
    }
}
