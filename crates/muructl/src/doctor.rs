//! `muructl doctor` (issue 950, design specification section 11.10): what
//! is wrong with Muru itself, as opposed to an application.
//!
//! The command reads and never changes anything, works with no daemon
//! running, and exits non-zero when a check fails. It lists every socket
//! and every file of one installation, the locations its flags and the
//! environment resolve, from the code's own knowledge of the layout
//! (section 13.8) and not from a walk of the directories: what each is
//! for, who owns it and its condition. Whatever is in a location and was
//! not accounted for is then listed as such. It then checks one part of
//! the installation against another, and each failed check names the
//! command that puts it right.
//!
//! A socket's condition is a fact where connecting to it is harmless: the
//! admin socket, the edge's control socket, a relay's, a worker's and a
//! managed server's. A host's `control.sock` and `output.sock`, and the
//! edge's listener socket, are not connected to, since the host takes a
//! new connection on either for its daemon or its relay and drops the one
//! it had, and the edge hands its listeners to whoever connects; for
//! those the condition is whether the owner is alive.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use muru_controller::locations::{self, Locations};
use muru_controller::lookup::MuruProcess;
use muru_controller::model::Release;
use muru_controller::registry::{self, InstanceId};
use muru_controller::store::{SCHEMA_VERSION, Store};
use serde::Serialize;

use crate::admin::{LocationArgs, human_size};

/// What `muructl doctor` takes.
#[derive(Args)]
pub struct DoctorArgs {
    #[command(subcommand)]
    command: Option<DoctorCommand>,

    /// Print the report as one JSON document.
    #[arg(long)]
    json: bool,

    /// Print what doctor found on the machine a bundle was made on, from
    /// the archive muructl debug bundle wrote, in place of checking this
    /// one.
    #[arg(long, value_name = "FILE")]
    bundle: Option<PathBuf>,

    #[command(flatten)]
    locations: LocationArgs,
}

/// The parts of the installation `muructl doctor` looks into further.
#[derive(Subcommand)]
pub enum DoctorCommand {
    /// Check the state store, opened read-only: SQLite's integrity and
    /// foreign key checks, the schema version, the rows in each table and
    /// Muru's own rules for what the store may hold.
    Store(StoreArgs),

    /// What happened last, in order: the daemon's events, the last lines
    /// of its own log and when each of its processes started.
    Timeline(TimelineArgs),
}

/// What `muructl doctor timeline` takes.
#[derive(Args)]
pub struct TimelineArgs {
    /// How many entries to print, the latest.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print the entries as one JSON document.
    #[arg(long)]
    json: bool,
}

/// What `muructl doctor store` takes.
#[derive(Args)]
pub struct StoreArgs {
    /// Print the store's schema as SQL, and check nothing.
    #[arg(long, conflicts_with = "copy")]
    dump_schema: bool,

    /// Write a consistent copy of the store to FILE, which must not
    /// exist, to inspect elsewhere; the daemon need not be stopped.
    #[arg(long, value_name = "FILE")]
    copy: Option<PathBuf>,

    /// Print the report as one JSON document.
    #[arg(long)]
    json: bool,
}

/// How long one socket may take to answer.
const ASK: Duration = Duration::from_secs(3);

/// A socket or a file of the installation.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct Item {
    /// Where it is.
    path: String,
    /// What it is for.
    what: String,
    /// Who owns it, when something does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    /// Its condition, as a few words.
    condition: String,
    /// Whether that condition is as it should be.
    ok: bool,
}

/// One check of a part of the installation against another.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct Check {
    /// The check's name.
    check: String,
    /// Whether it passed.
    ok: bool,
    /// What was found.
    detail: String,
    /// The command that puts a failure right, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

/// Everything `muructl doctor` reports.
#[derive(Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct Report {
    /// Whether every check passed and every listed thing is in order.
    ok: bool,
    sockets: Vec<Item>,
    files: Vec<Item>,
    checks: Vec<Check>,
}

impl Report {
    fn socket(&mut self, path: &Path, what: &str, owner: Option<String>, condition: Condition) {
        self.sockets.push(Item {
            path: path.display().to_string(),
            what: what.to_owned(),
            owner,
            ok: condition.ok(),
            condition: condition.words(),
        });
    }

    fn file(&mut self, path: &Path, what: &str, condition: &str, ok: bool) {
        self.files.push(Item {
            path: registry::home_shortened(path),
            what: what.to_owned(),
            owner: None,
            condition: condition.to_owned(),
            ok,
        });
    }

    fn pass(&mut self, check: &str, detail: String) {
        self.checks.push(Check {
            check: check.to_owned(),
            ok: true,
            detail,
            fix: None,
        });
    }

    fn fail(&mut self, check: &str, detail: String, fix: Option<&str>) {
        self.checks.push(Check {
            check: check.to_owned(),
            ok: false,
            detail,
            fix: fix.map(str::to_owned),
        });
    }
}

/// A socket's condition.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Condition {
    /// Something accepted a connection.
    Answering,
    /// Not connected to; the process that owns it is alive.
    OwnerAlive,
    /// It is there and nothing is behind it.
    Stale(&'static str),
    /// It should be there and is not.
    Absent,
}

impl Condition {
    fn ok(&self) -> bool {
        matches!(self, Self::Answering | Self::OwnerAlive)
    }

    fn words(&self) -> String {
        match self {
            Self::Answering => "answering".to_owned(),
            Self::OwnerAlive => "owner alive".to_owned(),
            Self::Stale(why) => format!("STALE, {why}"),
            Self::Absent => "ABSENT".to_owned(),
        }
    }
}

/// Whether anything accepts a connection on the socket at `path`. The
/// connection is closed at once, with nothing sent.
fn answers(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// The condition of a socket that may be connected to.
fn connected(path: &Path) -> Condition {
    if !exists(path) {
        Condition::Absent
    } else if answers(path) {
        Condition::Answering
    } else {
        Condition::Stale("nothing answers")
    }
}

/// The condition of a socket that must not be connected to, by whether
/// the process that owns it is alive.
fn owned(path: &Path, owner: Option<u32>) -> Condition {
    if !exists(path) {
        Condition::Absent
    } else if owner.is_some_and(muru_controller::host::alive) {
        Condition::OwnerAlive
    } else {
        Condition::Stale("its owner is gone")
    }
}

/// Whether there is anything at `path`, a socket or a link included.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// A record a daemon wrote for the next, read as TOML.
fn record<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// What the daemon records of its edge.
#[derive(serde::Deserialize)]
struct EdgeRecord {
    pid: u32,
    control: PathBuf,
    listener_socket: PathBuf,
    #[serde(default)]
    generation: u32,
}

/// What the daemon records of a release's host.
#[derive(serde::Deserialize)]
struct HostRecord {
    pid: u32,
    /// The runtime library directory the host runs with, which ends in
    /// the library's version and the interpreter's tag.
    #[serde(default)]
    runtime: Option<PathBuf>,
    #[serde(default)]
    sockets: Vec<PathBuf>,
    #[serde(default)]
    groups: Vec<GroupRecord>,
}

/// A named worker group in a host's record.
#[derive(serde::Deserialize)]
struct GroupRecord {
    name: String,
    #[serde(default)]
    sockets: Vec<PathBuf>,
}

/// What a relay's record says.
#[derive(serde::Deserialize)]
struct RelayRecord {
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    relay_pid: u32,
}

impl RelayRecord {
    /// The relay's own pid: a host's relay records it as `pid`, and a
    /// relay that is a process's parent as `relay_pid` beside the
    /// process's.
    fn relay(&self) -> u32 {
        if self.relay_pid != 0 {
            self.relay_pid
        } else {
            self.pid
        }
    }
}

/// The names in a directory, sorted; none when it cannot be read.
fn names(directory: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// The bytes and the number of regular files under `path`.
fn measure(path: &Path) -> (u64, u64) {
    let Ok(metadata) = path.symlink_metadata() else {
        return (0, 0);
    };

    if !metadata.is_dir() {
        return (metadata.len(), u64::from(metadata.is_file()));
    }

    names(path)
        .iter()
        .map(|name| measure(&path.join(name)))
        .fold((0, 0), |sum, each| (sum.0 + each.0, sum.1 + each.1))
}

/// The pid a lock file or a pid file names.
fn pid_in(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;

    text.lines()
        .find_map(|line| line.strip_prefix("pid="))
        .unwrap_or(text.trim())
        .trim()
        .parse()
        .ok()
}

/// Whether another process holds a `flock` on `path`, by taking a shared
/// one without waiting and letting it go at once, as a reader of the
/// instance registry does. Asked only when the process the file names is
/// gone, so a daemon that is starting is never refused its lock.
fn locked_by_another(path: &Path) -> bool {
    use std::os::fd::AsRawFd as _;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };

    match muru_controller::fdpass::try_lock(file.as_raw_fd(), false) {
        Ok(true) => {
            muru_controller::fdpass::unlock(file.as_raw_fd());
            false
        }
        Ok(false) => true,
        Err(_) => false,
    }
}

/// What is known of the installation before anything is listed.
struct Installation {
    locations: Locations,
    /// The store, when there is one that can be read.
    store: Option<Store>,
    /// Why the store could not be read, when it is there and could not.
    store_error: Option<String>,
    releases: Vec<Release>,
    /// The daemon's instance ID, kept in its state directory.
    id: Option<InstanceId>,
    /// The pid the state directory's lock file names.
    lock_pid: Option<u32>,
    /// The daemon's answer on its admin socket.
    status: Option<muru_proto::admin::v1::Status>,
    /// Each worker's pid by its socket, from the daemon when it answers.
    workers: BTreeMap<String, u32>,
    processes: Vec<MuruProcess>,
    registry: Vec<registry::Found>,
}

impl Installation {
    async fn read(locations: Locations) -> Self {
        let store_path = locations.state_store();

        let (store, store_error) = match store_path.is_file() {
            true => match Store::open_read_only(&store_path) {
                Ok(store) => (Some(store), None),
                Err(error) => (None, Some(error.to_string())),
            },
            false => (None, None),
        };

        let releases = store
            .as_ref()
            .and_then(|store| store.releases(None).ok())
            .unwrap_or_default();

        let id = std::fs::read_to_string(
            locations
                .state
                .path
                .join(muru_controller::daemon::INSTANCE_ID_FILE),
        )
        .ok()
        .and_then(|text| text.trim().parse().ok());

        let socket = locations.admin_socket();

        let status = match exists(&socket) {
            true => crate::admin::status_within(&socket, ASK).await,
            false => None,
        };

        let workers = match &status {
            Some(_) => worker_pids(&socket, &releases).await,
            None => BTreeMap::new(),
        };

        Self {
            lock_pid: pid_in(
                &locations
                    .state
                    .path
                    .join(muru_controller::ownership::LOCK_FILE),
            ),
            store,
            store_error,
            releases,
            id,
            status,
            workers,
            processes: muru_controller::lookup::muru_processes(),
            registry: registry::read(&registry::user_registry()),
            locations,
        }
    }

    /// The release with the number, as `app@release`.
    fn release(&self, number: u64) -> Option<String> {
        self.releases
            .iter()
            .find(|release| release.number == number)
            .map(|release| format!("{}@{}", release.app, release.id))
    }

    /// The daemon's own process, when it runs. A daemon gives its ID to
    /// what it starts and does not carry it itself, so it is the process
    /// titled a daemon that its lock file names, that answers on its
    /// socket, or that its live registry entry names.
    fn daemon_process(&self) -> Option<&MuruProcess> {
        let entry = self.id.and_then(|id| {
            self.registry
                .iter()
                .find(|found| found.live && found.record.id == id)
                .map(|found| found.record.pid)
        });
        let named = [
            self.lock_pid,
            self.status.as_ref().map(|status| status.pid),
            entry,
        ];

        self.processes
            .iter()
            .find(|process| process.role == "daemon" && named.contains(&Some(process.pid)))
    }

    /// The daemon's pid: what answers on its socket, else the process
    /// with its ID, else the holder its lock file names while alive.
    fn daemon_pid(&self) -> Option<u32> {
        self.status
            .as_ref()
            .map(|status| status.pid)
            .or_else(|| self.daemon_process().map(|process| process.pid))
            .or_else(|| {
                self.lock_pid
                    .filter(|pid| muru_controller::host::alive(*pid))
            })
    }
}

/// Each worker's pid by its socket, as the daemon reports its releases.
async fn worker_pids(socket: &Path, releases: &[Release]) -> BTreeMap<String, u32> {
    use muru_controller::client;
    use muru_proto::admin::v1 as pb;

    let mut found = BTreeMap::new();

    let Ok(mut client) = client::connect(&client::Endpoint::unix(socket)).await else {
        return found;
    };

    for release in releases.iter().filter(|release| release.state.is_running()) {
        let asked = tokio::time::timeout(
            ASK,
            client.get_release(pb::GetReleaseRequest {
                app: release.app.clone(),
                release: release.id.clone(),
            }),
        )
        .await;

        if let Ok(Ok(answer)) = asked {
            for worker in answer.into_inner().workers {
                found.insert(worker.socket, worker.pid);
            }
        }
    }

    found
}

/// List the sockets of the installation: the daemon's, its edge's, each
/// host's and each development run's, then what else the run directory
/// holds.
async fn list_sockets(installation: &Installation, report: &mut Report) {
    let locations = &installation.locations;
    let run = &locations.run.path;
    let daemon = installation.daemon_pid();

    // The admin socket, which a daemon that runs must answer on.

    let admin = locations.admin_socket();

    if exists(&admin) || daemon.is_some() {
        let condition = match (&installation.status, exists(&admin)) {
            (Some(_), _) => Condition::Answering,
            (None, true) => Condition::Stale("nothing answers"),
            (None, false) => Condition::Absent,
        };

        report.socket(
            &admin,
            "admin API",
            daemon.map(|pid| format!("daemon pid {pid}")),
            condition,
        );
    }

    // The edge's two sockets, from the daemon's record of it.

    let edge_dir = locations.edge_sockets();
    let mut edge_known = BTreeSet::new();

    if let Some(edge) = record::<EdgeRecord>(&locations.edge_record()) {
        let owner = Some(format!("edge pid {}", edge.pid));
        report.socket(
            &edge.control,
            "daemon to edge control",
            owner.clone(),
            connected(&edge.control),
        );
        report.socket(
            &edge.listener_socket,
            "edge hands its listeners to a new daemon",
            owner,
            owned(&edge.listener_socket, Some(edge.pid)),
        );
        edge_known.extend([edge.control, edge.listener_socket]);
    }

    for name in names(&edge_dir) {
        let path = edge_dir.join(&name);

        if !edge_known.contains(&path) {
            report.socket(
                &path,
                "(not in the daemon's record of its edge)",
                None,
                Condition::Stale("nothing owns it"),
            );
        }
    }

    // Each host, by its release's number.

    let hosts: BTreeSet<String> = names(&run.join(locations::HOSTS_DIR))
        .into_iter()
        .chain(names(&locations.files().join(locations::HOSTS_DIR)))
        .collect();

    for name in hosts {
        let Ok(number) = name.parse::<u64>() else {
            continue;
        };

        list_host(installation, number, report);
    }

    // Each detached command run's relay.

    let runs = installation
        .store
        .as_ref()
        .and_then(|store| store.runs_under_way().ok())
        .unwrap_or_default();
    let commands = run.join(muru_controller::daemon::COMMANDS_DIR);

    for name in names(&commands) {
        let path = commands
            .join(&name)
            .join(muru_controller::relay::CONTROL_SOCKET);
        let run = runs.iter().find(|run| {
            Path::new(&run.secrets)
                .file_name()
                .is_some_and(|own| *own == *name)
        });

        match run {
            Some(run) => report.socket(
                &path,
                &format!(
                    "daemon to the relay of run {} of {} on {}@{}",
                    run.id, run.name, run.app, run.release
                ),
                Some(format!("relay pid {}", run.relay_pid)),
                connected(&path),
            ),
            None => report.socket(
                &path,
                "(no command run under way owns it)",
                None,
                Condition::Stale("nothing owns it"),
            ),
        }
    }

    // Each development run under the run directory, from the registry.

    for name in names(run).iter().filter(|name| name.starts_with("run-")) {
        list_run(installation, &run.join(name), report).await;
    }

    // What else is at the top of the run directory.

    for name in names(run) {
        let known = [
            "admin.sock",
            locations::EDGE_DIR,
            locations::HOSTS_DIR,
            muru_controller::daemon::COMMANDS_DIR,
            locations::INSTANCES_DIR,
            "registry",
        ];

        if known.contains(&name.as_str()) || name.starts_with("run-") {
            continue;
        }

        report.socket(
            &run.join(&name),
            "(not accounted for)",
            None,
            Condition::Stale("nothing owns it"),
        );
    }
}

/// List one host's sockets: those its record names, then what else its
/// directory holds.
fn list_host(installation: &Installation, number: u64, report: &mut Report) {
    let dirs = installation.locations.host_dirs(number);
    let subject = installation
        .release(number)
        .unwrap_or_else(|| format!("release number {number}, which the store does not hold"));
    let host = record::<HostRecord>(&dirs.files.join(muru_controller::daemon::HOST_RECORD));
    let servers = record::<muru_controller::servers::ServersRecord>(
        &dirs.files.join(muru_controller::daemon::SERVERS_RECORD),
    );
    let relay = record::<RelayRecord>(&dirs.files.join(muru_controller::relay::RECORD))
        .map(|relay| relay.relay());
    let mut known: BTreeSet<PathBuf> = BTreeSet::new();

    if let Some(host) = &host {
        let owner = Some(format!("host pid {}", host.pid));
        let control = dirs.sockets.join("control.sock");
        let output = dirs.sockets.join(muru_controller::relay::OUTPUT_SOCKET);
        let relay_control = dirs.sockets.join(muru_controller::relay::CONTROL_SOCKET);

        report.socket(
            &control,
            &format!("daemon to host, {subject}"),
            owner.clone(),
            owned(&control, Some(host.pid)),
        );
        report.socket(
            &output,
            &format!("output of {subject}, to its log relay"),
            owner.clone(),
            owned(&output, Some(host.pid)),
        );
        report.socket(
            &relay_control,
            &format!("daemon to the log relay of {subject}"),
            relay.map(|pid| format!("relay pid {pid}")),
            connected(&relay_control),
        );
        known.extend([control, output, relay_control]);

        // The default group's workers are the sockets no named group has.

        let grouped: BTreeSet<&PathBuf> = host
            .groups
            .iter()
            .flat_map(|group| &group.sockets)
            .collect();

        let groups = std::iter::once((
            None,
            host.sockets
                .iter()
                .filter(|socket| !grouped.contains(socket))
                .collect::<Vec<_>>(),
        ))
        .chain(
            host.groups
                .iter()
                .map(|group| (Some(group.name.as_str()), group.sockets.iter().collect())),
        );

        for (group, sockets) in groups {
            for (index, socket) in sockets.into_iter().enumerate() {
                // A worker's link may be TCP, which is no socket file.

                if !socket.is_absolute() {
                    continue;
                }

                let what = match group {
                    None => format!("requests to {subject} worker {index}"),
                    Some(group) => format!("requests to {subject} worker {group}/{index}"),
                };

                let worker = installation
                    .workers
                    .get(&socket.display().to_string())
                    .map(|pid| format!("pid {pid}"));

                report.socket(
                    socket,
                    &what,
                    worker.or_else(|| owner.clone()),
                    connected(socket),
                );
                known.insert(socket.clone());
            }
        }
    }

    if let Some(servers) = &servers {
        for instance in &servers.instances {
            let relay = dirs.sockets.join(format!("relay-{}.sock", instance.index));

            if let Some(path) = instance.address.strip_prefix("unix:").map(PathBuf::from) {
                report.socket(
                    &path,
                    &format!("requests to {subject} instance {}", instance.index),
                    Some(format!("pid {}", instance.pid)),
                    connected(&path),
                );
                known.insert(path);
            }

            if exists(&relay) {
                report.socket(
                    &relay,
                    &format!(
                        "daemon to the log relay of {subject} instance {}",
                        instance.index
                    ),
                    Some(format!("relay pid {}", instance.relay_pid)),
                    connected(&relay),
                );
                known.insert(relay);
            }
        }
    }

    let recorded = host.is_some() || servers.is_some();

    for name in names(&dirs.sockets) {
        let path = dirs.sockets.join(&name);

        if known.contains(&path) {
            continue;
        }

        // A worker replaced since the record was written has a socket
        // the record does not name yet, which answers.

        let (what, condition) = match recorded {
            true => (
                format!("(not in the record of {subject})"),
                connected(&path),
            ),
            false => (
                "(nothing owns it)".to_owned(),
                Condition::Stale("its host has no record"),
            ),
        };

        report.socket(&path, &what, None, condition);
    }
}

/// List a development run's sockets: its admin socket and its hosts'.
async fn list_run(installation: &Installation, directory: &Path, report: &mut Report) {
    let found = installation
        .registry
        .iter()
        .find(|found| found.record.run_dir == directory);
    let live = found.filter(|found| found.live).map(|found| &found.record);

    let subject = match found.and_then(|found| found.record.app.clone()) {
        Some(app) => format!("the development run of {app}"),
        None => "a development run".to_owned(),
    };

    let owner = live.map(|record| format!("run pid {}", record.pid));
    let admin = directory.join("admin.sock");

    let condition = match live {
        Some(_) if crate::admin::status_within(&admin, ASK).await.is_some() => Condition::Answering,
        Some(_) if exists(&admin) => Condition::Stale("nothing answers"),
        Some(_) => Condition::Absent,
        None => Condition::Stale("its run has no live registry entry"),
    };

    report.socket(
        &admin,
        &format!("admin API of {subject}"),
        owner.clone(),
        condition,
    );

    let hosts = directory.join(locations::HOSTS_DIR);

    for number in names(&hosts) {
        for name in names(&hosts.join(&number)) {
            let path = hosts.join(&number).join(&name);

            let (what, connect) = match name.as_str() {
                "control.sock" => (format!("{subject} to its host, r{number}"), false),
                "output.sock" => (format!("output of r{number} of {subject}"), false),
                "relay.sock" => (format!("{subject} to the log relay of r{number}"), true),
                _ => (format!("requests to r{number} of {subject}, {name}"), true),
            };

            let condition = match (live, connect) {
                (None, _) => Condition::Stale("its run has no live registry entry"),
                (Some(_), true) => connected(&path),
                (Some(record), false) => owned(&path, Some(record.pid)),
            };

            report.socket(&path, &what, owner.clone(), condition);
        }
    }

    let detour = directory.join("detour.sock");

    if exists(&detour) {
        let condition = match live {
            Some(_) => connected(&detour),
            None => Condition::Stale("its run has no live registry entry"),
        };

        report.socket(
            &detour,
            &format!("requests from the daemon's edge to {subject}"),
            owner,
            condition,
        );
    }
}

/// What each entry at the top of a state directory is.
const STATE_ENTRIES: [(&str, &str); 14] = [
    ("muru.toml", "configuration"),
    ("envs", "environments built for releases"),
    ("pycache", "releases' bytecode"),
    ("static", "releases' static directories"),
    ("data", "applications' data"),
    ("runtime", "runtime library, by version and interpreter"),
    ("python", "interpreters uv installed"),
    ("uv-cache", "uv's caches, by user"),
    ("spool", "upload spool"),
    ("runs", "development runs' data and static directories"),
    ("instances", "named instances"),
    ("logs", "logs"),
    ("lists", "lists the configuration names"),
    ("certs", "certificates"),
];

/// List the files of the installation: its configuration, its store, its
/// locks, its registry entry, its runtime files and its logs, then what
/// else the state directory holds.
fn list_files(installation: &Installation, report: &mut Report) {
    let locations = &installation.locations;
    let state = &locations.state.path;
    let mut known: BTreeSet<String> = BTreeSet::new();

    // The configuration file, which need not exist.

    let config = &locations.config.path;

    match config.is_file() {
        true => report.file(config, "configuration", "present", true),
        false => report.file(config, "configuration", "absent, which is allowed", true),
    }

    // The store, with the schema version it records.

    let store_path = locations.state_store();
    known.insert("muru.sqlite".to_owned());

    if store_path.is_file() {
        let size = human_size(measure(&store_path).0);

        let (condition, ok) = match (&installation.store, &installation.store_error) {
            (Some(store), _) => match store.schema_version() {
                Ok(Some(version)) => (format!("schema {version}, {size}"), true),
                Ok(None) => (format!("records no schema version, {size}"), false),
                Err(error) => (format!("cannot be read: {error}"), false),
            },
            (None, Some(error)) => (format!("cannot be opened: {error}"), false),
            (None, None) => (size, true),
        };

        report.file(&store_path, "state store", &condition, ok);
    }

    for suffix in ["-wal", "-shm", "-journal"] {
        let beside = PathBuf::from(format!("{}{suffix}", store_path.display()));
        known.insert(format!("muru.sqlite{suffix}"));

        if beside.is_file() {
            let size = human_size(measure(&beside).0);
            report.file(&beside, "state store's journal", &size, true);
        }
    }

    // The lock on the state directory, and the instance's ID.

    let lock = state.join(muru_controller::ownership::LOCK_FILE);
    known.insert(muru_controller::ownership::LOCK_FILE.to_owned());

    if lock.is_file() {
        let condition = match installation.lock_pid {
            Some(pid) if muru_controller::host::alive(pid) => format!("held by pid {pid}"),
            Some(pid) => format!("free, last held by pid {pid}"),
            None => "free".to_owned(),
        };

        report.file(&lock, "lock on the state directory", &condition, true);
    }

    let id_file = state.join(muru_controller::daemon::INSTANCE_ID_FILE);
    known.insert(muru_controller::daemon::INSTANCE_ID_FILE.to_owned());

    if let Some(id) = installation.id {
        report.file(&id_file, "the daemon's instance ID", &id.short(), true);
    }

    // The daemon's entry in the instance registry.

    if let Some(id) = installation.id {
        let entry = registry::user_registry().join(id.to_string());

        let found = installation
            .registry
            .iter()
            .find(|found| found.record.id == id);

        let (condition, ok) = match found {
            Some(found) if found.live => (format!("locked by pid {}", found.record.pid), true),
            Some(_) => ("lock free".to_owned(), true),
            None if installation.daemon_process().is_some() => ("ABSENT".to_owned(), false),
            None => ("absent, no daemon runs".to_owned(), true),
        };

        if found.is_some() || !ok {
            report.file(&entry, "registry entry", &condition, ok);
        }
    }

    // The runtime files.

    let files = locations.files();
    known.insert(locations::FILES_DIR.to_owned());
    known.insert("registry".to_owned());
    list_runtime_files(installation, &files, report);

    // The logs, as one line.

    let logs = &locations.logs.path;

    if logs.is_dir() {
        let (bytes, count) = measure(logs);
        report.file(
            logs,
            "logs",
            &format!("{count} files, {}", human_size(bytes)),
            true,
        );
    }

    // What else the state directory holds, each by what it is.

    for name in names(state) {
        if known.contains(&name) || state.join(&name) == *logs || state.join(&name) == *config {
            continue;
        }

        let what = STATE_ENTRIES
            .iter()
            .find(|(known, _)| *known == name)
            .map_or("(not accounted for)", |(_, what)| what);
        let (bytes, count) = measure(&state.join(&name));

        report.file(
            &state.join(&name),
            what,
            &format!("{count} files, {}", human_size(bytes)),
            true,
        );
    }
}

/// List the runtime files: the daemon's pid file, its edge's record and
/// buckets, each host's files and each command run's.
fn list_runtime_files(installation: &Installation, files: &Path, report: &mut Report) {
    let locations = &installation.locations;
    let mut known: BTreeSet<String> = BTreeSet::new();
    let pid_file = locations.pid_file();
    known.insert("daemon.pid".to_owned());

    if let Some(pid) = pid_in(&pid_file) {
        let (condition, ok) = match muru_controller::host::alive(pid) {
            true => (format!("pid {pid}"), true),
            false => (format!("STALE, pid {pid} is gone"), false),
        };

        report.file(&pid_file, "the background daemon's pid", &condition, ok);
    }

    let edge_record = locations.edge_record();
    known.insert("edge.toml".to_owned());

    if let Some(edge) = record::<EdgeRecord>(&edge_record) {
        let (condition, ok) = match muru_controller::host::alive(edge.pid) {
            true => (
                format!("edge pid {}, generation {}", edge.pid, edge.generation),
                true,
            ),
            false => (format!("STALE, edge pid {} is gone", edge.pid), false),
        };

        report.file(
            &edge_record,
            "the edge, for the next daemon to take over",
            &condition,
            ok,
        );
    } else if edge_record.is_file() {
        report.file(&edge_record, "the edge's record", "cannot be read", false);
    }

    let buckets = locations
        .edge_files()
        .join(muru_controller::edges::BUCKETS_FILE);
    known.insert("edge".to_owned());

    if buckets.is_file() {
        report.file(
            &buckets,
            "the rate limits' counts",
            &human_size(measure(&buckets).0),
            true,
        );
    }

    known.insert(locations::HOSTS_DIR.to_owned());

    for name in names(&files.join(locations::HOSTS_DIR)) {
        let Ok(number) = name.parse::<u64>() else {
            continue;
        };

        let directory = locations.host_files(number);
        let subject = installation
            .release(number)
            .unwrap_or_else(|| format!("release number {number}"));

        for entry in names(&directory) {
            let path = directory.join(&entry);

            let (what, pid) = match entry.as_str() {
                muru_controller::daemon::HOST_RECORD => (
                    format!("the host of {subject}, for the next daemon"),
                    record::<HostRecord>(&path).map(|host| host.pid),
                ),
                muru_controller::daemon::SERVERS_RECORD => (
                    format!("the instances of {subject}, for the next daemon"),
                    None,
                ),
                "secrets" => (format!("secrets of {subject} delivered as files"), None),
                "gates" => (format!("{subject} waits for its gates"), None),
                "stop-by" => (format!("when the workers of {subject} must be gone"), None),
                "imports.json" => (format!("the import report of {subject}"), None),
                name if name.starts_with("relay") => (
                    format!("a log relay of {subject}"),
                    record::<RelayRecord>(&path).map(|relay| relay.relay()),
                ),
                _ => ("(not accounted for)".to_owned(), None),
            };

            let (condition, ok) = match pid {
                Some(pid) if muru_controller::host::alive(pid) => (format!("pid {pid}"), true),
                Some(pid) => (format!("STALE, pid {pid} is gone"), false),
                None if path.is_dir() => (format!("{} files", measure(&path).1), true),
                None => ("present".to_owned(), true),
            };

            report.file(&path, &what, &condition, ok);
        }
    }

    known.insert(muru_controller::daemon::COMMANDS_DIR.to_owned());
    let commands = files.join(muru_controller::daemon::COMMANDS_DIR);

    for name in names(&commands) {
        let (_, count) = measure(&commands.join(&name));
        report.file(
            &commands.join(&name),
            "a command run's relay record and secret files",
            &format!("{count} files"),
            true,
        );
    }

    // A development run's files are beside the default daemon's.

    for name in names(files) {
        let path = files.join(&name);

        if known.contains(&name) {
            continue;
        }

        if !name.starts_with("run-") {
            report.file(&path, "(not accounted for)", "present", true);
            continue;
        }

        let live = installation
            .registry
            .iter()
            .any(|found| found.live && found.record.files_dir.as_deref() == Some(path.as_path()));

        match live {
            true => report.file(&path, "a development run's files", "its run is live", true),
            false => report.file(
                &path,
                "a development run's files",
                "STALE, no live run names it",
                false,
            ),
        }
    }
}

/// Check the registry against the processes: the daemon's entry is held
/// by the process its record names, and a daemon or a run that runs has
/// an entry.
fn check_registry(installation: &Installation, report: &mut Report) {
    const CHECK: &str = "registry";
    let registry = registry::user_registry();

    match (installation.id, installation.daemon_process()) {
        (Some(id), process) => {
            let found = installation
                .registry
                .iter()
                .find(|found| found.record.id == id);
            let others = installation
                .processes
                .iter()
                .filter(|other| other.id == Some(id) && other.role != "daemon")
                .count();

            match (found, process) {
                (Some(found), Some(process)) if found.live && found.record.pid == process.pid => {
                    report.pass(
                        CHECK,
                        format!(
                            "the daemon (pid {}) holds its entry {}",
                            process.pid,
                            id.short()
                        ),
                    );
                }
                (Some(found), Some(process)) if found.live => report.fail(
                    CHECK,
                    format!(
                        "the entry {} is locked and names pid {}, and the daemon is pid {}",
                        id.short(),
                        found.record.pid,
                        process.pid
                    ),
                    Some("muructl daemon restart"),
                ),
                (_, Some(process)) => report.fail(
                    CHECK,
                    format!(
                        "the daemon (pid {}) runs and has no live entry in {}, so muructl \
                         instance list does not show it",
                        process.pid,
                        registry.display()
                    ),
                    Some("muructl daemon restart"),
                ),
                (Some(found), None) if found.live => report.fail(
                    CHECK,
                    format!(
                        "the entry {} is locked by pid {}, which is not a daemon with that ID",
                        id.short(),
                        found.record.pid
                    ),
                    None,
                ),
                (Some(_), None) if others > 0 => report.pass(
                    CHECK,
                    format!(
                        "the daemon is detached: its entry {} is free and {others} of its \
                         processes run",
                        id.short()
                    ),
                ),
                (Some(_), None) => report.fail(
                    CHECK,
                    format!(
                        "the entry {} is stale: nothing of its instance runs",
                        id.short()
                    ),
                    Some("muructl instance prune"),
                ),
                (None, None) if others > 0 => report.fail(
                    CHECK,
                    format!(
                        "{others} processes of the instance {} run with no daemon and no \
                         entry in {}",
                        id.short(),
                        registry.display()
                    ),
                    Some("muructl daemon start"),
                ),
                (None, None) => {
                    report.pass(CHECK, "no daemon runs, and it has no entry".to_owned())
                }
            }
        }
        (None, _) => report.pass(
            CHECK,
            "no daemon has run from this state directory".to_owned(),
        ),
    }

    // Each development run whose directory is under the run directory.

    let run = &installation.locations.run.path;

    for name in names(run).iter().filter(|name| name.starts_with("run-")) {
        let directory = run.join(name);
        let short = name.trim_start_matches("run-");
        let found = installation
            .registry
            .iter()
            .find(|found| found.record.run_dir == directory);
        let process = installation.processes.iter().find(|process| {
            process.role == "run" && process.id.is_some_and(|id| id.short() == short)
        });

        match (found, process) {
            (Some(found), _) if found.live => report.pass(
                CHECK,
                format!("the run {short} (pid {}) holds its entry", found.record.pid),
            ),
            (_, Some(process)) => report.fail(
                CHECK,
                format!(
                    "the run {short} (pid {}) runs and has no live entry in {}, so muructl \
                     instance list does not show it",
                    process.pid,
                    registry.display()
                ),
                None,
            ),
            (Some(_), None) => report.fail(
                CHECK,
                format!("the entry of the run {short} is stale: its run is gone"),
                Some("muructl instance prune"),
            ),
            (None, None) => report.fail(
                CHECK,
                format!(
                    "{} is the directory of a run that is gone and has no entry",
                    directory.display()
                ),
                None,
            ),
        }
    }
}

/// Check the locks: what the state directory's lock file, the pid file,
/// the admin socket and the registry say of who the daemon is agree, and
/// no other process holds either directory.
fn check_locks(installation: &Installation, report: &mut Report) {
    const CHECK: &str = "locks";
    let locations = &installation.locations;
    let lock = locations
        .state
        .path
        .join(muru_controller::ownership::LOCK_FILE);
    let holder = installation
        .lock_pid
        .filter(|pid| muru_controller::host::alive(*pid));
    let answering = installation.status.as_ref().map(|status| status.pid);
    let process = installation.daemon_process().map(|process| process.pid);

    // Every source that names a daemon names the same one.

    let named: BTreeSet<u32> = [holder, answering, process].into_iter().flatten().collect();

    if named.len() > 1 {
        report.fail(
            CHECK,
            format!(
                "the state directory's lock names pid {}, the admin socket is answered by pid \
                 {} and the daemon's process is pid {}",
                holder.map_or("none".to_owned(), |pid| pid.to_string()),
                answering.map_or("none".to_owned(), |pid| pid.to_string()),
                process.map_or("none".to_owned(), |pid| pid.to_string()),
            ),
            None,
        );
        return;
    }

    let Some(daemon) = named.into_iter().next() else {
        // No daemon: nothing may hold either lock.

        let held: Vec<String> = [(lock, "state"), (locations.run.path.clone(), "run")]
            .into_iter()
            .filter(|(path, _)| locked_by_another(path))
            .map(|(_, what)| format!("the {what} directory"))
            .collect();

        match held.is_empty() {
            true => report.pass(
                CHECK,
                "no daemon runs, and nothing holds its locks".to_owned(),
            ),
            false => report.fail(
                CHECK,
                format!(
                    "no daemon is known, and a process holds the lock on {}",
                    held.join(" and on ")
                ),
                Some("muructl instance list --orphans"),
            ),
        }

        return;
    };

    if let Some(status) = &installation.status
        && Path::new(&status.state_dir) != locations.state.path
    {
        report.fail(
            CHECK,
            format!(
                "the daemon on the run directory {} (pid {daemon}) keeps its state in {}, not \
                 in {}",
                locations.run.path.display(),
                status.state_dir,
                locations.state.path.display()
            ),
            None,
        );
        return;
    }

    let pid_file = pid_in(&locations.pid_file());

    match pid_file {
        Some(recorded) if recorded != daemon => report.fail(
            CHECK,
            format!(
                "the pid file {} names pid {recorded}, and the daemon is pid {daemon}",
                locations.pid_file().display()
            ),
            Some("muructl daemon restart"),
        ),
        _ => report.pass(
            CHECK,
            format!("the daemon (pid {daemon}) holds the state and run directories"),
        ),
    }
}

/// Check the paths: each socket's against the platform's limit, no
/// regular file in the run directory, no location of files where they
/// are deleted by age, and nothing of the layout before this one.
fn check_paths(installation: &Installation, report: &mut Report) {
    const CHECK: &str = "paths";
    let locations = &installation.locations;
    let run = &locations.run.path;
    let limit = locations::SOCKET_PATH_LIMIT - 1;
    let mut failed = false;

    if let Err(message) = locations::check_socket_room(run) {
        report.fail(CHECK, message, Some("muructl daemon start --run-dir DIR"));
        failed = true;
    }

    let longest = report
        .sockets
        .iter()
        .map(|socket| (socket.path.len(), socket.path.clone()))
        .max();

    if let Some((length, path)) = &longest
        && *length > limit
    {
        report.fail(
            CHECK,
            format!("the socket path {path} is {length} bytes, past the {limit} allowed"),
            None,
        );
        failed = true;
    }

    // A daemon before the runtime files moved kept these in the run
    // directory, where this version neither reads nor removes them.

    for (name, what) in [
        ("daemon.lock", "the lock file"),
        ("daemon.pid", "the pid file"),
        ("edge.toml", "the edge's record"),
        ("apps", "the releases' directories"),
    ] {
        let path = run.join(name);

        if exists(&path) {
            report.fail(
                CHECK,
                format!(
                    "{} is {what} of a daemon of an earlier version of Muru, which kept its \
                     files in the run directory",
                    path.display()
                ),
                Some("muructl daemon reset"),
            );
            failed = true;
        }
    }

    for file in regular_files(run, 4) {
        let earlier = ["daemon.lock", "daemon.pid", "edge.toml"]
            .iter()
            .any(|name| file == run.join(name));

        if earlier || file.starts_with(run.join("apps")) || file.starts_with(run.join("registry")) {
            continue;
        }

        report.fail(
            CHECK,
            format!(
                "{} is a regular file in the run directory, which holds sockets only",
                file.display()
            ),
            None,
        );
        failed = true;
    }

    // Files are deleted by age under these, while their owner runs.

    for (what, path) in [
        ("state directory", locations.state.path.clone()),
        ("registry", registry::user_registry()),
    ] {
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        let cleaned = ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"]
            .iter()
            .any(|cleaned| real.starts_with(cleaned));

        // The system registry is in /run, and a user with no home
        // directory keeps theirs in the runtime location.

        if cleaned && !(what == "registry" && locations::account_state().is_none()) {
            report.fail(
                CHECK,
                format!(
                    "the {what} {} is under a temporary directory, where files nothing has \
                     touched for some days are deleted while their owner runs",
                    path.display()
                ),
                match what {
                    "registry" => None,
                    _ => Some("muructl daemon start --state-dir DIR"),
                },
            );
            failed = true;
        }
    }

    if !failed {
        let detail = match longest {
            Some((length, _)) => format!(
                "the longest of {} socket paths is {length} bytes of the {limit} allowed, and \
                 the run directory holds sockets only",
                report.sockets.len()
            ),
            None => format!(
                "the run directory {} leaves room for every socket path, and holds no file",
                run.display()
            ),
        };

        report.pass(CHECK, detail);
    }
}

/// The regular files under `directory`, to `depth` directories down.
fn regular_files(directory: &Path, depth: u32) -> Vec<PathBuf> {
    let mut found = Vec::new();

    for name in names(directory) {
        let path = directory.join(&name);
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };

        if metadata.is_file() {
            found.push(path);
        } else if metadata.is_dir() && depth > 0 {
            found.extend(regular_files(&path, depth - 1));
        }
    }

    found
}

/// Check the store against the processes: each release the store says
/// runs has a host that is alive, and each host that runs is a release
/// the store says runs.
fn check_releases(installation: &Installation, report: &mut Report) {
    const CHECK: &str = "releases";

    if installation.store.is_none() {
        return;
    }

    let locations = &installation.locations;
    let mut failed = false;
    let mut running = 0;

    for release in &installation.releases {
        let subject = format!("{}@{}", release.app, release.id);
        let files = locations.host_files(release.number);
        let host = record::<HostRecord>(&files.join(muru_controller::daemon::HOST_RECORD));
        let servers = files
            .join(muru_controller::daemon::SERVERS_RECORD)
            .is_file();
        let alive = host
            .as_ref()
            .is_some_and(|host| muru_controller::host::alive(host.pid));

        // A release at zero workers with its host stopped has no record
        // and is still ready, so only a record that names a dead host,
        // or a host with no release behind it, is a fault.

        match (release.state.is_running(), &host) {
            (true, Some(host)) if !alive => {
                report.fail(
                    CHECK,
                    format!(
                        "the store says {subject} is {}, and its host (pid {}) is gone",
                        release.state.as_str(),
                        host.pid
                    ),
                    Some("muructl daemon start"),
                );
                failed = true;
            }
            (false, Some(host)) if alive => {
                report.fail(
                    CHECK,
                    format!(
                        "the store says {subject} is {}, and its host (pid {}) runs",
                        release.state.as_str(),
                        host.pid
                    ),
                    Some("muructl instance list --orphans"),
                );
                failed = true;
            }
            (true, _) if alive || servers => running += 1,
            _ => {}
        }
    }

    // A host's files for a number no release has.

    for name in names(&locations.files().join(locations::HOSTS_DIR)) {
        let known = name
            .parse::<u64>()
            .is_ok_and(|number| installation.release(number).is_some());

        if !known {
            report.fail(
                CHECK,
                format!(
                    "{} holds the files of a host whose release the store does not hold",
                    locations
                        .files()
                        .join(locations::HOSTS_DIR)
                        .join(&name)
                        .display()
                ),
                Some("muructl daemon start"),
            );
            failed = true;
        }
    }

    if !failed {
        report.pass(
            CHECK,
            format!(
                "{} releases in the store, {running} with a host or instances that run",
                installation.releases.len()
            ),
        );
    }
}

/// Check that the store can be read by this muructl.
fn check_store(installation: &Installation, report: &mut Report) {
    const CHECK: &str = "store";
    let path = installation.locations.state_store();

    match (&installation.store, &installation.store_error) {
        (None, None) => report.pass(CHECK, format!("no state store at {}", path.display())),
        (None, Some(error)) => report.fail(
            CHECK,
            format!("{} cannot be opened: {error}", path.display()),
            Some("muructl doctor store"),
        ),
        (Some(store), _) => match store.schema_version() {
            Ok(Some(version)) if version == SCHEMA_VERSION => report.pass(
                CHECK,
                format!("schema {version}, as this muructl expects; muructl doctor store checks it in full"),
            ),
            Ok(Some(version)) if version < SCHEMA_VERSION => report.pass(
                CHECK,
                format!(
                    "schema {version}, older than the {SCHEMA_VERSION} this muructl writes: a \
                     daemon of this version brings it up to date when it starts"
                ),
            ),
            Ok(Some(version)) => report.fail(
                CHECK,
                format!(
                    "schema {version}, newer than the {SCHEMA_VERSION} this muructl knows: a \
                     later version of Muru wrote it"
                ),
                None,
            ),
            Ok(None) => report.fail(
                CHECK,
                format!("{} records no schema version", path.display()),
                Some("muructl doctor store"),
            ),
            Err(error) => report.fail(
                CHECK,
                format!("{} cannot be read: {error}", path.display()),
                Some("muructl doctor store"),
            ),
        },
    }
}

/// Check the edge against the daemon: the edge the daemon names is the
/// one its record names and the one that answers, and the routing it
/// serves is the routing the daemon last gave it. Gives the edge's
/// version, for the check of versions.
async fn check_edge(installation: &Installation, report: &mut Report) -> Option<String> {
    const CHECK: &str = "edge";
    let locations = &installation.locations;
    let status = installation.status.as_ref()?;
    let recorded = record::<EdgeRecord>(&locations.edge_record());

    let Some(recorded) = recorded else {
        report.fail(
            CHECK,
            format!(
                "the daemon runs and has no record of its edge at {}",
                locations.edge_record().display()
            ),
            Some("muructl daemon restart"),
        );
        return None;
    };

    let edge = match muru_controller::edges::status_of(&recorded.control, ASK).await {
        Ok(edge) => edge,
        Err(error) => {
            report.fail(
                CHECK,
                format!(
                    "the edge (pid {}) gave no status on {}: {error:#}",
                    recorded.pid,
                    recorded.control.display()
                ),
                Some("muructl daemon restart"),
            );
            return None;
        }
    };

    let version = Some(edge.version.clone()).filter(|version| !version.is_empty());

    if edge.pid != recorded.pid || edge.pid != status.edge_pid {
        report.fail(
            CHECK,
            format!(
                "the edge that answers is pid {}, its record names pid {} and the daemon names \
                 pid {}",
                edge.pid, recorded.pid, status.edge_pid
            ),
            Some("muructl daemon restart"),
        );
        return version;
    }

    // A daemon of a version before this check says nothing of what it
    // sent, which reads as version 0.

    if status.snapshot_version == 0 && status.version != env!("CARGO_PKG_VERSION") {
        report.pass(
            CHECK,
            format!(
                "the edge (pid {}) answers; the daemon, version {}, does not say which routing \
                 it last sent",
                edge.pid, status.version
            ),
        );
        return version;
    }

    // The daemon may send a snapshot between the two answers, so it is
    // asked again after the edge, and either of its answers matches.

    let after = crate::admin::status_within(&locations.admin_socket(), ASK)
        .await
        .map(|status| status.snapshot_version);
    let sent = [Some(status.snapshot_version), after];

    match sent.contains(&Some(edge.snapshot_version)) {
        true => report.pass(
            CHECK,
            format!(
                "the edge (pid {}) serves snapshot {}, the one the daemon last sent",
                edge.pid, edge.snapshot_version
            ),
        ),
        false => report.fail(
            CHECK,
            format!(
                "the edge (pid {}) serves snapshot {}, and the daemon last sent {}",
                edge.pid,
                edge.snapshot_version,
                after.unwrap_or(status.snapshot_version)
            ),
            Some("muructl daemon restart"),
        ),
    }

    version
}

/// The version of the runtime library a host runs with, from the
/// directory its record names: `runtime/VERSION/TAG`.
fn runtime_version(runtime: &Path) -> Option<String> {
    runtime
        .parent()?
        .file_name()
        .map(|version| version.to_string_lossy().into_owned())
}

/// A version as two builds of Muru say it: the package's own form and
/// the one in a directory's name (`0.1.0-alpha.37`, `0.1.0a37`) are the
/// same version.
fn same_version(one: &str, other: &str) -> bool {
    let plain = |version: &str| {
        version
            .replace("-alpha.", "a")
            .replace("-beta.", "b")
            .replace("-rc.", "rc")
    };
    plain(one) == plain(other)
}

/// Check the versions: the daemon and its edge run the version of this
/// muructl, which is the installed one. A half-finished upgrade leaves
/// them apart. Each host keeps the runtime library it started with
/// through an upgrade, so a host on another version is said and is no
/// failure.
fn check_versions(installation: &Installation, edge: Option<String>, report: &mut Report) {
    const CHECK: &str = "versions";
    let own = env!("CARGO_PKG_VERSION");

    let Some(status) = &installation.status else {
        report.pass(
            CHECK,
            format!("no daemon runs; this muructl is version {own}"),
        );
        return;
    };

    if !same_version(&status.version, own) {
        report.fail(
            CHECK,
            format!(
                "the daemon (pid {}) runs version {}, and this muructl is version {own}",
                status.pid, status.version
            ),
            Some("muructl daemon upgrade"),
        );
        return;
    }

    match &edge {
        Some(edge) if !same_version(edge, &status.version) => {
            report.fail(
                CHECK,
                format!(
                    "the edge runs version {edge}, and the daemon version {}: an upgrade \
                     replaces the edge, and this one was not replaced",
                    status.version
                ),
                Some("muructl daemon upgrade"),
            );
            return;
        }
        _ => {}
    }

    // The hosts, by the runtime library each was started with.

    let mut others: Vec<String> = Vec::new();
    let mut hosts = 0;

    for release in &installation.releases {
        let files = installation.locations.host_files(release.number);
        let host = record::<HostRecord>(&files.join(muru_controller::daemon::HOST_RECORD));

        let Some(version) = host
            .filter(|host| muru_controller::host::alive(host.pid))
            .and_then(|host| host.runtime)
            .and_then(|runtime| runtime_version(&runtime))
        else {
            continue;
        };

        hosts += 1;

        if !same_version(&version, own) {
            others.push(format!("{}@{} on {version}", release.app, release.id));
        }
    }

    let edge = match edge {
        Some(_) => "the daemon and its edge run",
        None => "the daemon runs",
    };

    match others.is_empty() {
        true if hosts == 0 => {
            report.pass(CHECK, format!("{edge} version {own}, as this muructl is"));
        }
        true => report.pass(
            CHECK,
            format!(
                "{edge} version {own}, as this muructl is, and so does the runtime library of \
                 {hosts} hosts"
            ),
        ),
        false => report.pass(
            CHECK,
            format!(
                "{edge} version {own}, as this muructl is; {} of {hosts} hosts keep the runtime \
                 library they started with until a deploy ({})",
                others.len(),
                others.join(", ")
            ),
        ),
    }
}

/// The least room a location may have before it is a failure.
const LEAST_FREE: u64 = 64 * 1024 * 1024;

/// Check the disk: each location is a directory its daemon's user owns
/// and may write to, on a file system with room left.
fn check_disk(installation: &Installation, report: &mut Report) {
    use std::os::unix::fs::MetadataExt as _;

    const CHECK: &str = "disk";
    let locations = &installation.locations;

    // The daemon's user when one runs, else whoever asks.

    // SAFETY: geteuid has no preconditions and cannot fail.

    let caller = unsafe { libc::geteuid() };
    let owner = installation
        .daemon_process()
        .and_then(|process| process.uid)
        .unwrap_or(caller);
    let mut failed = false;
    let mut room = Vec::new();

    for (what, path) in [
        ("state", &locations.state.path),
        ("run", &locations.run.path),
        ("log", &locations.logs.path),
        ("cache", &locations.cache.path),
    ] {
        let Ok(metadata) = path.metadata() else {
            continue;
        };

        if !metadata.is_dir() {
            report.fail(
                CHECK,
                format!("the {what} directory {} is not a directory", path.display()),
                None,
            );
            failed = true;
            continue;
        }

        if metadata.uid() != owner {
            report.fail(
                CHECK,
                format!(
                    "the {what} directory {} belongs to {}, and the daemon's user is {}",
                    path.display(),
                    muru_controller::processes::user_name(metadata.uid()),
                    muru_controller::processes::user_name(owner)
                ),
                None,
            );
            failed = true;
        }

        if metadata.mode() & 0o300 != 0o300 {
            report.fail(
                CHECK,
                format!(
                    "the {what} directory {} has mode {:o}, which its owner cannot write to",
                    path.display(),
                    metadata.mode() & 0o7777
                ),
                None,
            );
            failed = true;
        }

        // The run directory admits its owner, and its group for the
        // edge, and nobody else may make a socket in it.

        if what == "run" && metadata.mode() & 0o002 != 0 {
            report.fail(
                CHECK,
                format!(
                    "the run directory {} has mode {:o}, which lets any user write to it",
                    path.display(),
                    metadata.mode() & 0o7777
                ),
                None,
            );
            failed = true;
        }

        match muru_controller::diagnostics::facts::free_bytes(path) {
            Some(free) if free < LEAST_FREE => {
                report.fail(
                    CHECK,
                    format!(
                        "the {what} directory {} has {} free",
                        path.display(),
                        human_size(free)
                    ),
                    Some("muructl release prune"),
                );
                failed = true;
            }
            Some(free) => room.push(format!("{what} {}", human_size(free))),
            None => {}
        }
    }

    if !failed {
        report.pass(
            CHECK,
            match room.is_empty() {
                true => "no location exists yet".to_owned(),
                false => format!(
                    "each location belongs to {} and has room: {} free",
                    muru_controller::processes::user_name(owner),
                    room.join(", ")
                ),
            },
        );
    }
}

/// Print a table of items, the paths lined up.
fn print_items(heading: &str, items: &[Item]) {
    println!("{heading}");

    if items.is_empty() {
        println!("  none");
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.path.clone(),
                item.what.clone(),
                item.owner.clone().unwrap_or_default(),
                item.condition.clone(),
            ]
        })
        .collect();
    crate::instances::print_table(&rows);
}

/// Print the checks, one to a line, a failure with what puts it right.
fn print_checks(checks: &[Check]) {
    println!("CHECKS");

    for check in checks {
        let word = if check.ok { "ok" } else { "FAILED" };

        match &check.fix {
            Some(fix) => println!("{word:<7} {:<13} {}; see {fix}", check.check, check.detail),
            None => println!("{word:<7} {:<13} {}", check.check, check.detail),
        }
    }
}

/// Everything doctor finds of the installation at `locations`: its
/// sockets and files, and each check.
pub(crate) async fn examine(locations: Locations) -> Report {
    let installation = Installation::read(locations).await;
    let mut report = Report::default();

    list_sockets(&installation, &mut report).await;
    list_files(&installation, &mut report);
    check_registry(&installation, &mut report);
    check_locks(&installation, &mut report);
    check_paths(&installation, &mut report);
    check_store(&installation, &mut report);
    check_releases(&installation, &mut report);
    let edge = check_edge(&installation, &mut report).await;
    check_versions(&installation, edge, &mut report);
    check_disk(&installation, &mut report);

    report.ok = report.checks.iter().all(|check| check.ok)
        && report
            .sockets
            .iter()
            .chain(&report.files)
            .all(|item| item.ok);
    report
}

/// Print a report as text.
fn print_report(report: &Report) {
    print_items("SOCKETS", &report.sockets);
    println!();
    print_items("FILES", &report.files);
    println!();
    print_checks(&report.checks);
}

/// A daemon that runs says where its files are. Unless `--state-dir`
/// names the state directory, when a daemon answers on the run
/// directory's admin socket its state, log and cache directories and its
/// configuration file are the ones checked, so that the run directory
/// alone finds a daemon that was started with locations of its own. A
/// state directory given by the flag is kept, and the check of the locks
/// says when the daemon on the run directory is not that one's.
async fn follow_daemon(mut locations: Locations) -> Locations {
    if locations.state.rule == "option" {
        return locations;
    }

    let socket = locations.admin_socket();

    let Some(status) = (match exists(&socket) {
        true => crate::admin::status_within(&socket, ASK).await,
        false => None,
    }) else {
        return locations;
    };

    if status.kind != "daemon" {
        return locations;
    }

    for (location, said) in [
        (&mut locations.state, &status.state_dir),
        (&mut locations.logs, &status.log_dir),
        (&mut locations.cache, &status.cache_dir),
        (&mut locations.config, &status.config),
    ] {
        if !said.is_empty() {
            *location = locations::Location {
                path: PathBuf::from(said),
                rule: "daemon",
            };
        }
    }

    locations
}

/// Run `muructl doctor`: the exit code is 1 when anything failed.
pub async fn doctor(args: DoctorArgs) -> anyhow::Result<i32> {
    if let Some(bundle) = &args.bundle {
        anyhow::ensure!(
            args.command.is_none(),
            "--bundle prints all that a bundle holds of doctor's, and takes no part to look into"
        );
        return from_bundle(bundle, args.json);
    }

    let locations = follow_daemon(args.locations.resolve()?).await;

    match &args.command {
        Some(DoctorCommand::Store(store)) => {
            return doctor_store(&locations, store, args.json || store.json);
        }
        Some(DoctorCommand::Timeline(timeline)) => {
            return doctor_timeline(locations, timeline, args.json || timeline.json).await;
        }
        None => {}
    }

    let report = examine(locations).await;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    Ok(i32::from(!report.ok))
}

/// What `muructl doctor store` reports.
#[derive(Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct StoreReport {
    ok: bool,
    path: String,
    /// The bytes of the database and of its write-ahead log.
    bytes: u64,
    wal_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema_version: Option<i64>,
    expected_schema_version: i64,
    /// Each table with its rows.
    tables: BTreeMap<String, u64>,
    checks: Vec<Check>,
}

/// Check the store at `path`, opened read-only.
pub(crate) fn examine_store(path: &Path) -> anyhow::Result<StoreReport> {
    let store = Store::open_read_only(path)
        .map_err(|error| anyhow::anyhow!("{} cannot be opened: {error}", path.display()))?;
    let mut report = Report::default();
    let version = store.schema_version().ok().flatten();

    // SQLite's own checks.

    match store.integrity() {
        Ok(faults) if faults.is_empty() => {
            report.pass("integrity", "SQLite finds the database sound".to_owned());
        }
        Ok(faults) => report.fail("integrity", faults.join("; "), Some("muructl daemon reset")),
        Err(error) => report.fail("integrity", error.to_string(), None),
    }

    match store.foreign_key_faults() {
        Ok(faults) if faults.is_empty() => {
            report.pass("foreign keys", "every row names rows that exist".to_owned());
        }
        Ok(faults) => report.fail("foreign keys", faults.join("; "), None),
        Err(error) => report.fail("foreign keys", error.to_string(), None),
    }

    // The schema version against what this muructl writes.

    match version {
        Some(version) if version == SCHEMA_VERSION => report.pass(
            "schema",
            format!("version {version}, as this muructl expects"),
        ),
        Some(version) if version < SCHEMA_VERSION => report.fail(
            "schema",
            format!(
                "version {version}, older than the {SCHEMA_VERSION} this muructl writes, so the \
                 rules below may not be checkable; a daemon of this version brings it up to \
                 date when it starts"
            ),
            Some("muructl daemon start"),
        ),
        Some(version) => report.fail(
            "schema",
            format!(
                "version {version}, newer than the {SCHEMA_VERSION} this muructl knows: a later \
                 version of Muru wrote it"
            ),
            None,
        ),
        None => report.fail("schema", "the store records no version".to_owned(), None),
    }

    // Muru's own rules.

    match store.rule_breaks() {
        Ok(rules) => {
            for (rule, breaks) in rules {
                match breaks.is_empty() {
                    true => report.pass("rule", rule.to_owned()),
                    false => report.fail("rule", format!("{rule}: {}", breaks.join("; ")), None),
                }
            }
        }
        Err(error) => report.fail(
            "rule",
            format!("the rules cannot be checked: {error}"),
            None,
        ),
    }

    match muru_controller::references::dangling(&store) {
        Ok(found) if found.is_empty() => report.pass(
            "rule",
            "no route, host, overlay, rate limit, worker group or rollout names something \
             removed"
                .to_owned(),
        ),
        Ok(found) => report.fail("rule", found.join("; "), Some("muructl apply")),
        Err(error) => report.fail(
            "rule",
            format!("what the objects name cannot be checked: {error}"),
            None,
        ),
    }

    let tables: BTreeMap<String, u64> = store
        .table_counts()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let wal = PathBuf::from(format!("{}-wal", path.display()));

    Ok(StoreReport {
        ok: report.checks.iter().all(|check| check.ok),
        path: path.display().to_string(),
        bytes: measure(path).0,
        wal_bytes: measure(&wal).0,
        schema_version: version,
        expected_schema_version: SCHEMA_VERSION,
        tables,
        checks: report.checks,
    })
}

/// Print a store's report as text.
fn print_store(report: &StoreReport) {
    println!(
        "store    {} ({}, write-ahead log {})",
        report.path,
        human_size(report.bytes),
        human_size(report.wal_bytes)
    );
    println!();
    println!("TABLES");

    let rows: Vec<Vec<String>> = report
        .tables
        .iter()
        .map(|(name, rows)| vec![name.clone(), rows.to_string()])
        .collect();
    crate::instances::print_table(&rows);
    println!();
    print_checks(&report.checks);
}

/// Run `muructl doctor store` on the store of `locations`.
fn doctor_store(locations: &Locations, args: &StoreArgs, json: bool) -> anyhow::Result<i32> {
    let path = locations.state_store();

    anyhow::ensure!(
        path.is_file(),
        "there is no state store at {}; --state-dir names another state directory",
        path.display()
    );

    if args.dump_schema || args.copy.is_some() {
        let store = Store::open_read_only(&path)
            .map_err(|error| anyhow::anyhow!("{} cannot be opened: {error}", path.display()))?;

        let Some(copy) = &args.copy else {
            print!("{}", store.schema_sql()?);
            return Ok(0);
        };

        anyhow::ensure!(
            !exists(copy),
            "{} exists; a copy is never written over a file",
            copy.display()
        );
        store.copy_to(copy)?;

        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "copied": path.display().to_string(),
                    "to": copy.display().to_string(),
                    "bytes": measure(copy).0,
                }))?
            );
        } else {
            println!(
                "muru: copied {} to {} ({})",
                path.display(),
                copy.display(),
                human_size(measure(copy).0)
            );
        }

        return Ok(0);
    }

    let report = examine_store(&path)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_store(&report);
    }

    Ok(i32::from(!report.ok))
}

/// The part of a bundle that holds doctor's report, and the one that
/// holds its checks of the store.
pub(crate) const BUNDLE_REPORT: &str = "doctor/report.json";
pub(crate) const BUNDLE_STORE: &str = "doctor/store.json";

/// Run `muructl doctor --bundle`: print what doctor found where and when
/// the bundle was made, which the bundle carries. A socket's condition
/// can only be asked on the machine it is on, so nothing is checked
/// again here, and the exit code is that of the checks as they were.
fn from_bundle(path: &Path, json: bool) -> anyhow::Result<i32> {
    use muru_controller::diagnostics::archive;

    let wanted = [archive::layout::MANIFEST, BUNDLE_REPORT, BUNDLE_STORE];
    let parts = archive::read_parts(path, &wanted)
        .map_err(|error| anyhow::anyhow!("{} cannot be read: {error}", path.display()))?;

    let Some(report) = parts.get(BUNDLE_REPORT) else {
        anyhow::bail!(
            "{} holds no {BUNDLE_REPORT}: it is no bundle of a daemon's, or one made by a \
             version of Muru before doctor's report was part of a bundle",
            path.display()
        );
    };

    let report: Report = serde_json::from_slice(report)
        .map_err(|error| anyhow::anyhow!("{BUNDLE_REPORT} cannot be read: {error}"))?;
    let store: Option<StoreReport> = parts
        .get(BUNDLE_STORE)
        .and_then(|store| serde_json::from_slice(store).ok());
    let manifest: serde_json::Value = parts
        .get(archive::layout::MANIFEST)
        .and_then(|manifest| serde_json::from_slice(manifest).ok())
        .unwrap_or_default();
    let ok = report.ok && store.as_ref().is_none_or(|store| store.ok);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": ok,
                "bundle": path.display().to_string(),
                "created": manifest["created"],
                "node": manifest["node"],
                "muru": manifest["muru"],
                "report": report,
                "store": store,
            }))?
        );
        return Ok(i32::from(!ok));
    }

    let said = |key: &str| manifest[key].as_str().unwrap_or("unknown").to_owned();
    println!(
        "bundle   {}, made {} on {} by Muru {}",
        path.display(),
        said("created"),
        said("node"),
        said("muru")
    );
    println!();
    print_report(&report);

    if let Some(store) = &store {
        println!();
        print_store(store);
    }

    Ok(i32::from(!ok))
}

/// One entry of the timeline.
#[derive(Debug, Clone, Serialize)]
struct Moment {
    /// When, Unix milliseconds.
    time_unix_ms: u64,
    /// The same as text, in UTC.
    time: String,
    /// Where it is from: `event`, `log` or `process`.
    source: &'static str,
    /// What happened.
    text: String,
}

impl Moment {
    fn new(time_unix_ms: u64, source: &'static str, text: String) -> Self {
        let at = std::time::UNIX_EPOCH + Duration::from_millis(time_unix_ms);
        let (date, hours, minutes, seconds, millis) = muru_edge::access::civil(at);

        Self {
            time_unix_ms,
            time: format!("{date}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"),
            source,
            text,
        }
    }
}

/// How much of the end of the daemon's log is read for its last lines.
const LOG_TAIL: u64 = 256 * 1024;

/// The last lines of the log at `path` that begin with a time, each
/// with its time.
fn log_moments(path: &Path) -> Vec<Moment> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let size = file.metadata().map_or(0, |metadata| metadata.len());
    let from = size.saturating_sub(LOG_TAIL);
    let mut tail = Vec::new();

    if file.seek(SeekFrom::Start(from)).is_err() || file.read_to_end(&mut tail).is_err() {
        return Vec::new();
    }

    // The first line of a tail that starts mid-file is a part of one.

    String::from_utf8_lossy(&tail)
        .lines()
        .skip(usize::from(from > 0))
        .filter_map(|line| {
            let time = muru_controller::diagnostics::window::line_time(line.as_bytes())?;
            let text = line.split_once(' ').map_or(line, |(_, rest)| rest);
            Some(Moment::new(time, "log", text.to_owned()))
        })
        .collect()
}

/// Run `muructl doctor timeline`: the events the store keeps, the last
/// lines of the daemon's own log and the start of each of the
/// installation's processes, merged in order, the latest `limit`.
async fn doctor_timeline(
    locations: Locations,
    args: &TimelineArgs,
    json: bool,
) -> anyhow::Result<i32> {
    let log = locations.daemon_log();
    let installation = Installation::read(locations).await;
    let mut moments = log_moments(&log);

    // The last events the store keeps.

    if let Some(store) = &installation.store {
        let last = store.last_event_sequence().unwrap_or(0);
        let after = last.saturating_sub(args.limit as u64);

        for event in store.events_after(after, args.limit).unwrap_or_default() {
            moments.push(Moment::new(
                event.time_unix_ms,
                "event",
                format!("{} {}: {}", event.kind, event.subject, event.message),
            ));
        }
    }

    // The processes of this installation: the daemon's own, and those
    // that carry its instance ID.

    let daemon = installation.daemon_process().map(|process| process.pid);

    for process in &installation.processes {
        let ours = Some(process.pid) == daemon
            || (installation.id.is_some() && process.id == installation.id);

        if let (true, Some(started)) = (ours, process.started_unix_ms) {
            moments.push(Moment::new(
                started,
                "process",
                format!("{} started (pid {})", process.title.trim(), process.pid),
            ));
        }
    }

    moments.sort_by_key(|moment| moment.time_unix_ms);
    let skip = moments.len().saturating_sub(args.limit);
    let moments = &moments[skip..];

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "timeline": moments }))?
        );
        return Ok(0);
    }

    if moments.is_empty() {
        println!("muru: nothing to show: no events, no daemon log and no process");
    }

    for moment in moments {
        println!("{}  {:<7}  {}", moment.time, moment.source, moment.text);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_the_same_however_a_build_writes_them() {
        assert!(same_version("0.1.0-alpha.37", "0.1.0a37"));
        assert!(same_version("1.0.0", "1.0.0"));
        assert!(!same_version("1.0.0", "1.1.0"));
        assert!(!same_version("0.1.0-alpha.36", "0.1.0a37"));

        assert_eq!(
            runtime_version(Path::new("/var/lib/muru/runtime/1.0.0/cp313")).as_deref(),
            Some("1.0.0")
        );
        assert_eq!(runtime_version(Path::new("cp313")), None);
    }

    #[test]
    fn the_last_lines_of_a_log_that_begin_with_a_time_are_its_moments() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let log = directory.path().join("daemon.log");

        std::fs::write(
            &log,
            "2026-11-02T05:40:11.204Z muru: state: /var/lib/muru (default)\n\
             a line with no time\n\
             2026-11-02T05:40:11.209Z muru: error: cannot listen on 0.0.0.0:80\n",
        )
        .expect("written");

        let moments = log_moments(&log);
        assert_eq!(moments.len(), 2);
        assert_eq!(moments[1].time, "2026-11-02T05:40:11.209Z");
        assert_eq!(moments[1].text, "muru: error: cannot listen on 0.0.0.0:80");
        assert_eq!(moments[1].source, "log");
        assert!(moments[0].time_unix_ms < moments[1].time_unix_ms);
        assert!(log_moments(&directory.path().join("none")).is_empty());
    }

    #[test]
    fn a_pid_is_read_from_a_lock_file_and_from_a_pid_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let lock = directory.path().join("daemon.lock");
        let pid = directory.path().join("daemon.pid");

        std::fs::write(&lock, "pid=4242\nsocket=/run/muru/admin.sock\n").expect("written");
        std::fs::write(&pid, "4243").expect("written");

        assert_eq!(pid_in(&lock), Some(4242));
        assert_eq!(pid_in(&pid), Some(4243));
        assert_eq!(pid_in(&directory.path().join("none")), None);
    }

    #[test]
    fn a_socket_nothing_listens_on_is_stale_and_one_that_is_listened_on_answers() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("a.sock");

        assert_eq!(connected(&path), Condition::Absent);

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bound");
        assert_eq!(connected(&path), Condition::Answering);
        assert_eq!(
            owned(&path, Some(std::process::id())),
            Condition::OwnerAlive
        );

        drop(listener);
        assert_eq!(connected(&path), Condition::Stale("nothing answers"));
        assert_eq!(owned(&path, None), Condition::Stale("its owner is gone"));
        assert!(!Condition::Stale("nothing answers").ok());
    }

    #[test]
    fn regular_files_are_found_among_sockets_and_directories() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let run = directory.path();

        std::fs::create_dir_all(run.join("h/1")).expect("made");
        let _listener =
            std::os::unix::net::UnixListener::bind(run.join("h/1/worker-0.sock")).expect("bound");
        assert!(regular_files(run, 4).is_empty());

        std::fs::write(run.join("h/1/host.toml"), "pid = 1\n").expect("written");
        assert_eq!(regular_files(run, 4), [run.join("h/1/host.toml")]);
        assert_eq!(measure(run), (8, 1));
    }
}
