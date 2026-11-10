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

use std::io::{self, IsTerminal as _, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use clap::Args;
use muru_controller::diagnostics::archive::{Archive, Entry, FORMAT, Manifest, layout};
use muru_controller::diagnostics::facts::{self, Platform};
use muru_controller::diagnostics::window::Window;
use muru_controller::diagnostics::{ReleaseFacts, period};
use muru_proto::admin::v1 as pb;
use serde::Serialize;

use super::{Installation, within};
use crate::admin::{DaemonFileArgs, SocketArgs, format_time, human_size};

/// What a planned archive says of a part a running daemon is asked for.
const ASKED: &str = "asked for when the bundle is made";

/// What a bundle never holds, as its manifest says.
const NEVER_HELD: [&str; 3] = [
    "secret values",
    "environment variable values beyond those muructl debug inspect shows, which masks the \
     ones that look like secrets",
    "request and response bodies",
];

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
        match found {
            Ok(found) => {
                let mut text = serde_json::to_vec_pretty(&found).map_err(io::Error::other)?;
                text.push(b'\n');
                self.archive.bytes(path, &text, None)
            }
            Err(why) => {
                self.archive.missing(path, why);
                Ok(())
            }
        }
    }

    /// Collect every part. Only a failure to write the archive itself is
    /// an error: a part that cannot be collected is recorded and the
    /// rest carries on.
    async fn collect(mut self) -> io::Result<Archive<W>> {
        self.summary().await?;
        self.events().await?;

        // What was noted while collecting, such as a worker that did not
        // answer, is recorded as what it left out.

        for note in self.installation.take_notes() {
            self.archive.missing(&note.part, note.why);
        }

        Ok(self.archive)
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
            let mut lines = Vec::new();

            for event in events {
                let (app, release) = event
                    .subject
                    .split_once('@')
                    .unwrap_or((event.subject.as_str(), ""));
                let another = others.iter().any(|other| other == app)
                    || (app == target.app && !release.is_empty() && !target.holds(app, release));

                if window.holds(event.time_unix_ms) && !another {
                    lines.extend(serde_json::to_vec(&event).unwrap_or_default());
                    lines.push(b'\n');
                }
            }

            lines
        });

        match found {
            Ok(lines) => self.archive.bytes(layout::EVENTS, &lines, None),
            Err(why) => {
                self.archive.missing(layout::EVENTS, why);
                Ok(())
            }
        }
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
        };
        Some(plan.collect().await?)
    } else {
        None
    };

    if args.dry_run {
        let planned = planned.expect("a dry run is planned");
        print_plan(&planned, &target, window, &output);
        return Ok(());
    }

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

            let recorded = record(&mut installation, &target, window).await;
            let making = Making {
                root: &root,
                window,
                target: &target,
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
            let recorded = record(&mut installation, &target, window).await;
            let making = Making {
                root: &root,
                window,
                target: &target,
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
        };
        let archive = collecting.collect().await?;
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
            options: serde_json::json!({}),
            configuration_changed: None,
            recorded: self.recorded,
            left_out: NEVER_HELD.iter().map(|held| (*held).to_owned()).collect(),
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
async fn record(installation: &mut Installation, target: &Target, window: Window) -> String {
    let Some(live) = &mut installation.live else {
        return "no event: nothing is running to record one".to_owned();
    };

    let noted = within(live.client.note_bundle(pb::NoteBundleRequest {
        app: target.app.clone(),
        release: target.release.clone(),
        since_unix_ms: window.since_unix_ms,
        until_unix_ms: window.until_unix_ms,
        options: Vec::new(),
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
