//! `muructl instance` (design specification sections 13.8 and 13.10):
//! the Muru instances on this machine, from the instance registry, with
//! the processes they left behind.
//!
//! What a row's state is depends on the kind of entry it comes from. A
//! local instance's is decided by its registry lock and the processes
//! carrying its ID; an instance on another machine would be decided by a
//! call to it instead, so the state is worked out per kind, in one place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use muru_controller::lookup::MuruProcess;
use muru_controller::registry::{self, Kind, Record};
use muru_proto::admin::v1 as pb;
use serde::Serialize;

/// The instance subcommands.
#[derive(Subcommand)]
pub enum InstanceCommand {
    /// List the Muru instances on this machine: daemons, development
    /// runs and serves, running, detached or stale, and with --orphans
    /// the Muru processes no live instance accounts for.
    List(ListArgs),
}

/// A kind of instance, for --kind.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    /// Daemons.
    Daemon,
    /// Development runs.
    Run,
    /// `muructl serve`.
    Serve,
}

impl KindArg {
    fn kind(self) -> Kind {
        match self {
            Self::Daemon => Kind::Daemon,
            Self::Run => Kind::Run,
            Self::Serve => Kind::Serve,
        }
    }
}

/// What `instance list` takes.
#[derive(Args)]
pub struct ListArgs {
    /// Also list the Muru processes that no running or detached instance
    /// accounts for, exiting 1 when there are any.
    #[arg(long)]
    pub orphans: bool,

    /// Only the instances, and orphans, with this label, which an
    /// instance takes from MURU_INSTANCE_LABEL.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,

    /// Only instances of this kind.
    #[arg(long, value_enum, value_name = "KIND")]
    pub kind: Option<KindArg>,

    /// Also other users' registries (root only).
    #[arg(long)]
    pub all_users: bool,

    /// One row per port the instances use or are configured to use, with
    /// the clashes between them.
    #[arg(long, conflicts_with = "orphans")]
    pub ports: bool,

    /// Print one JSON document: the instances, each record with its
    /// state and location, and with --orphans the orphans.
    #[arg(long)]
    pub json: bool,
}

/// What a row is: an instance's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Its owner holds its registry lock.
    Running,
    /// Its lock is free, but processes carrying its ID still run, as a
    /// daemon that detached leaves its edge and hosts.
    Detached,
    /// Its lock is free and nothing of it runs.
    Stale,
}

impl State {
    /// The state as the table shows it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Detached => "detached",
            Self::Stale => "stale",
        }
    }
}

/// One instance on the machine.
#[derive(Debug, Clone)]
pub struct Row {
    /// Its registry record.
    pub record: Record,
    /// Its state, worked out for its kind.
    pub state: State,
}

impl Row {
    /// The name the table and titles give it.
    pub fn tag(&self) -> String {
        self.record.tag()
    }

    /// Whether the instance runs, or its processes do.
    pub fn alive(&self) -> bool {
        matches!(self.state, State::Running | State::Detached)
    }

    /// Whether `process` says it belongs to this instance.
    pub fn owns(&self, process: &MuruProcess) -> bool {
        match process.id {
            Some(id) => id == self.record.id,
            None => process.tag.as_deref() == Some(self.tag().as_str()),
        }
    }
}

/// The registries to read: the caller's, the system daemon's when it is
/// another, and with `all_users` every user's.
pub fn registries(all_users: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = vec![registry::user_registry()];
    let system = PathBuf::from(registry::SYSTEM_REGISTRY);

    if !found.contains(&system) {
        found.push(system);
    }

    if all_users {
        // SAFETY: geteuid has no preconditions and cannot fail.

        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!("--all-users reads other users' registries, which needs root");
        }

        let pattern: (&str, &str) = if cfg!(target_os = "linux") {
            ("/run/user", "muru/registry")
        } else {
            ("/tmp", "registry")
        };

        if let Ok(entries) = std::fs::read_dir(pattern.0) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let ours = cfg!(target_os = "linux") || name.starts_with("muru-");
                let path = entry.path().join(pattern.1);

                if ours && path.is_dir() && !found.contains(&path) {
                    found.push(path);
                }
            }
        }
    }

    Ok(found)
}

/// The state of a local instance: running while its lock is held, else
/// detached while a process carrying its ID or tag runs, else stale.
fn local_state(found: &registry::Found, processes: &[MuruProcess]) -> State {
    if found.live {
        return State::Running;
    }

    let tag = found.record.tag();
    let carries = |process: &MuruProcess| match process.id {
        Some(id) => id == found.record.id,
        None => process.role != "run" && process.tag.as_deref() == Some(tag.as_str()),
    };

    if processes.iter().any(carries) {
        State::Detached
    } else {
        State::Stale
    }
}

/// Every instance in the registries, with its state.
pub fn rows(registries: &[PathBuf], processes: &[MuruProcess]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();

    for path in registries {
        for found in registry::read(path) {
            if !seen.insert(found.record.id) {
                continue;
            }

            rows.push(Row {
                state: local_state(&found, processes),
                record: found.record,
            });
        }
    }

    rows
}

/// A Muru process no live instance accounts for.
#[derive(Debug, Clone, Serialize)]
pub struct Orphan {
    /// Its pid.
    pub pid: u32,
    /// Its role, from its title.
    pub role: String,
    /// Its title, or command line.
    pub title: String,
    /// The user it runs as.
    pub user: String,
    /// When it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_unix_ms: Option<u64>,
    /// Its parent.
    pub parent_pid: u32,
    /// The instance it says it belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// That instance's ID, when it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Its instance's label, from its environment or its instance's entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Why it is an orphan.
    pub why: String,
}

/// How long one instance may take to answer ListProcesses.
const ASK: Duration = Duration::from_secs(3);

/// What the running instances supervise: the pids each one's
/// ListProcesses answer lists, with what descends from them, and the
/// pids of the instances that did not answer.
struct Supervised {
    pids: HashSet<u32>,
    unanswered: HashSet<u32>,
}

/// Ask each running instance what it supervises.
async fn supervised(rows: &[Row]) -> Supervised {
    let mut supervised = Supervised {
        pids: HashSet::new(),
        unanswered: HashSet::new(),
    };

    for row in rows.iter().filter(|row| row.state == State::Running) {
        supervised.pids.insert(row.record.pid);

        let asked = tokio::time::timeout(ASK, async {
            let mut client = muru_controller::client::connect(&row.record.admin)
                .await
                .ok()?;
            client
                .list_processes(pb::ListProcessesRequest {
                    all: true,
                    ..pb::ListProcessesRequest::default()
                })
                .await
                .ok()
                .map(tonic::Response::into_inner)
        })
        .await;

        let Ok(Some(answer)) = asked else {
            supervised.unanswered.insert(row.record.pid);
            continue;
        };

        let mut stack: Vec<pb::Process> = answer.root.into_iter().collect();

        while let Some(process) = stack.pop() {
            supervised.pids.insert(process.pid);
            stack.extend(process.children);
        }
    }

    supervised
}

/// The orphans among `processes`: one whose instance is neither running
/// nor detached, one with a Muru title and no instance, from a version
/// before tags, and one of a running instance that the instance does not
/// supervise. The processes of a running instance that did not answer
/// are not judged, rather than taken for orphans.
pub async fn orphans(rows: &[Row], processes: &[MuruProcess]) -> Vec<Orphan> {
    let supervised = supervised(rows).await;
    let mut found = Vec::new();

    for process in processes {
        if supervised.pids.contains(&process.pid) {
            continue;
        }

        let owner = rows.iter().find(|row| row.owns(process));

        let why = match owner {
            Some(row) if row.state == State::Detached => continue,
            Some(row)
                if row.state == State::Running
                    && supervised.unanswered.contains(&row.record.pid) =>
            {
                continue;
            }
            Some(row) if row.state == State::Running => {
                format!("{} does not supervise it", row.tag())
            }
            Some(row) => format!("its instance is {}", row.state.word()),
            None if process.id.is_none() && process.tag.is_none() => {
                "it names no instance, as Muru's processes did before instance tags".to_owned()
            }
            None => "its instance is gone".to_owned(),
        };

        found.push(Orphan {
            pid: process.pid,
            role: process.role.clone(),
            title: process.title.clone(),
            user: process
                .uid
                .map(muru_controller::processes::user_name)
                .unwrap_or_default(),
            started_unix_ms: process.started_unix_ms,
            parent_pid: process.parent,
            instance: process.tag.clone(),
            instance_id: process.id.map(|id| id.to_string()),
            label: process
                .label
                .clone()
                .or_else(|| owner.and_then(|row| row.record.label.clone())),
            why,
        });
    }

    found.sort_by_key(|orphan| orphan.pid);
    found
}

/// One port an instance uses or is configured to use.
#[derive(Debug, Clone, Serialize)]
pub struct Port {
    /// The port.
    pub port: u16,
    /// The address it is on.
    pub address: String,
    /// The instance, by its tag.
    pub instance: String,
    /// The instance's kind.
    pub kind: String,
    /// What it is for: `http`, `https` or `dashboard`.
    pub r#use: String,
    /// `bound`, or `configured` when the instance has not bound it.
    pub state: String,
    /// The instance it would clash with, for a configured port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clashes_with: Option<String>,
}

/// The ports of the instances that run or were left running, and the
/// configured ones of those that have not bound them, each configured
/// one with the instance it would clash with.
pub fn ports(rows: &[Row]) -> Vec<Port> {
    let mut found: Vec<(Port, String)> = Vec::new();

    for row in rows.iter().filter(|row| row.alive()) {
        let mut addresses: Vec<(String, String, bool)> = row
            .record
            .listeners
            .iter()
            .map(|listener| match &listener.bound {
                Some(bound) => (bound.clone(), listener.name.clone(), true),
                None => (listener.configured.clone(), listener.name.clone(), false),
            })
            .collect();

        if let Some(dashboard) = row
            .record
            .dashboard
            .as_deref()
            .and_then(|url| url.strip_prefix("http://"))
        {
            addresses.push((
                dashboard.trim_end_matches('/').to_owned(),
                "dashboard".to_owned(),
                true,
            ));
        }

        for (address, what, bound) in addresses {
            let Some((host, port)) = address.rsplit_once(':') else {
                continue;
            };
            let Ok(port) = port.parse::<u16>() else {
                continue;
            };

            found.push((
                Port {
                    port,
                    address: host
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_owned(),
                    instance: row.tag(),
                    kind: row.record.kind.word().to_owned(),
                    r#use: what,
                    state: if bound { "bound" } else { "configured" }.to_owned(),
                    clashes_with: None,
                },
                address,
            ));
        }
    }

    // A configured port clashes with another instance's bound or
    // configured one on the same port and an overlapping address.

    let snapshot = found.clone();

    for (port, address) in &mut found {
        if port.state != "configured" {
            continue;
        }

        port.clashes_with = snapshot
            .iter()
            .find(|(other, other_address)| {
                other.instance != port.instance && registry::clashes(address, other_address)
            })
            .map(|(other, _)| other.instance.clone());
    }

    let mut ports: Vec<Port> = found.into_iter().map(|(port, _)| port).collect();
    ports.sort_by(|a, b| (a.port, &a.address, &a.instance).cmp(&(b.port, &b.address, &b.instance)));
    ports
}

/// The ports table.
fn print_ports(ports: &[Port]) {
    if ports.is_empty() {
        println!("muru: no instance uses a port");
        return;
    }

    let mut table = vec![
        ["PORT", "ADDRESS", "INSTANCE", "KIND", "USE", "STATE", ""]
            .map(str::to_owned)
            .to_vec(),
    ];

    for port in ports {
        table.push(vec![
            port.port.to_string(),
            port.address.clone(),
            port.instance.clone(),
            port.kind.clone(),
            port.r#use.clone(),
            port.state.clone(),
            port.clashes_with
                .as_deref()
                .map(|other| format!("clashes with {other}"))
                .unwrap_or_default(),
        ]);
    }

    print_table(&table);
}

/// A row as `--json` prints it: the record with its state and location.
fn row_json(row: &Row) -> serde_json::Value {
    let mut value = serde_json::to_value(&row.record).unwrap_or_default();

    if let Some(object) = value.as_object_mut() {
        object.insert("tag".to_owned(), row.tag().into());
        object.insert("state".to_owned(), row.state.word().into());
        object.insert("location".to_owned(), "local".into());
    }

    value
}

/// A path with the home directory shortened to `~`.
pub fn shortened(path: &Path) -> String {
    registry::home_shortened(path)
}

/// How long ago `unix_ms` was, as the table shows it: `3d 2h`, `2h 4m`,
/// `14m`, `40s`.
pub fn uptime(unix_ms: u64) -> String {
    let seconds = crate::admin::now_unix_ms().saturating_sub(unix_ms) / 1000;
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);

    match (days, seconds / 3600, seconds / 60) {
        (1.., _, _) if hours > 0 => format!("{days}d {hours}h"),
        (1.., _, _) => format!("{days}d"),
        (_, 1.., _) => format!("{}h {minutes}m", seconds / 3600),
        (_, _, 1..) => format!("{minutes}m"),
        _ => format!("{seconds}s"),
    }
}

/// The LISTEN column: the bound addresses, a later one on the same
/// address shortened to its port, each marked `(tls)` when it is, or the
/// configured ones marked `(configured)` when nothing is bound.
pub fn listen_column(listeners: &[registry::Listener]) -> String {
    let bound: Vec<(&str, bool)> = listeners
        .iter()
        .filter_map(|listener| Some((listener.bound.as_deref()?, listener.tls)))
        .collect();
    let configured = bound.is_empty();

    let addresses: Vec<(&str, bool)> = if configured {
        listeners
            .iter()
            .map(|listener| (listener.configured.as_str(), listener.tls))
            .collect()
    } else {
        bound
    };

    if addresses.is_empty() {
        return "-".to_owned();
    }

    let mut previous_host: Option<&str> = None;
    let mut parts = Vec::new();

    for (address, tls) in addresses {
        let (host, port) = address.rsplit_once(':').unwrap_or((address, ""));

        let shown = if previous_host == Some(host) {
            format!(":{port}")
        } else {
            address.to_owned()
        };

        previous_host = Some(host);
        parts.push(if tls { format!("{shown} (tls)") } else { shown });
    }

    let joined = parts.join(", ");

    if configured {
        format!("{joined} (configured)")
    } else {
        joined
    }
}

/// The WHERE column: a run's or serve's project and application, or a
/// daemon's state directory.
fn where_column(record: &Record) -> String {
    match (record.kind, &record.project, &record.app) {
        (Kind::Run | Kind::Serve, Some(project), Some(app)) => {
            format!("{} ({app})", shortened(project))
        }
        _ => record
            .state_dir
            .as_deref()
            .map_or_else(|| shortened(&record.run_dir), shortened),
    }
}

/// Print rows of cells as columns two spaces apart.
pub fn print_table(rows: &[Vec<String>]) {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| format!("{cell:width$}", width = widths[column]))
            .collect();
        println!("{}", line.join("   ").trim_end());
    }
}

/// The rows `instance list` keeps, after its filters.
fn kept(rows: Vec<Row>, args: &ListArgs) -> Vec<Row> {
    rows.into_iter()
        .filter(|row| args.kind.is_none_or(|kind| row.record.kind == kind.kind()))
        .filter(|row| args.label.is_none() || row.record.label == args.label)
        .collect()
}

/// `muructl instance list`: exits 0 when listed, and with --orphans 1
/// when there are orphans.
pub async fn list(args: ListArgs) -> anyhow::Result<i32> {
    let registries = registries(args.all_users)?;
    let processes = muru_controller::lookup::muru_processes();
    let all = rows(&registries, &processes);

    let orphans = if args.orphans {
        orphans(&all, &processes)
            .await
            .into_iter()
            .filter(|orphan| args.label.is_none() || orphan.label == args.label)
            .collect()
    } else {
        Vec::new()
    };

    let rows = kept(all, &args);

    if args.ports {
        let ports = ports(&rows);

        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "ports": ports }))?
            );
        } else {
            print_ports(&ports);
        }

        return Ok(0);
    }

    if args.json {
        let mut answer = serde_json::json!({
            "instances": rows.iter().map(row_json).collect::<Vec<_>>(),
        });

        if args.orphans {
            answer["orphans"] = serde_json::to_value(&orphans)?;
        }

        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        print_rows(&rows);

        if args.orphans {
            print_orphans(&orphans);
        }
    }

    Ok(i32::from(args.orphans && !orphans.is_empty()))
}

/// The instance table.
fn print_rows(rows: &[Row]) {
    if rows.is_empty() {
        println!("muru: no instances are registered on this machine");
        return;
    }

    let labelled = rows.iter().any(|row| row.record.label.is_some());
    let mut table = vec![
        [
            "NAME",
            "KIND",
            "STATE",
            "PID",
            "UPTIME",
            "LISTEN",
            "DASHBOARD",
            "WHERE",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];

    if labelled {
        table[0].push("LABEL".to_owned());
    }

    for row in rows {
        let running = row.state == State::Running;
        let mut cells = vec![
            row.tag(),
            row.record.kind.word().to_owned(),
            row.state.word().to_owned(),
            if running {
                row.record.pid.to_string()
            } else {
                "-".to_owned()
            },
            if running {
                uptime(row.record.started_unix_ms)
            } else {
                "-".to_owned()
            },
            listen_column(&row.record.listeners),
            match (&row.record.dashboard, running) {
                (Some(dashboard), true) => dashboard.clone(),
                _ => "-".to_owned(),
            },
            where_column(&row.record),
        ];

        if labelled {
            cells.push(row.record.label.clone().unwrap_or_else(|| "-".to_owned()));
        }

        table.push(cells);
    }

    print_table(&table);
}

/// The orphans, after the instances.
fn print_orphans(orphans: &[Orphan]) {
    println!();

    if orphans.is_empty() {
        println!("muru: no orphaned Muru processes");
        return;
    }

    let mut table = vec![
        ["PID", "ROLE", "USER", "UPTIME", "PPID", "INSTANCE", "TITLE"]
            .map(str::to_owned)
            .to_vec(),
    ];

    for orphan in orphans {
        table.push(vec![
            orphan.pid.to_string(),
            orphan.role.clone(),
            orphan.user.clone(),
            orphan
                .started_unix_ms
                .map_or_else(|| "-".to_owned(), uptime),
            orphan.parent_pid.to_string(),
            orphan.instance.clone().unwrap_or_else(|| "-".to_owned()),
            orphan.title.clone(),
        ]);
    }

    print_table(&table);
}

/// How many instances `instance list` shows that run or were left
/// running, for `muructl status` to say when it finds nothing listening.
pub fn count_alive() -> usize {
    let Ok(registries) = registries(false) else {
        return 0;
    };

    let processes = muru_controller::lookup::muru_processes();

    rows(&registries, &processes)
        .iter()
        .filter(|row| row.alive())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(configured: &str, bound: Option<&str>, tls: bool) -> registry::Listener {
        registry::Listener {
            name: if tls { "https" } else { "http" }.to_owned(),
            configured: configured.to_owned(),
            bound: bound.map(str::to_owned),
            tls,
        }
    }

    #[test]
    fn the_listen_column_shortens_and_marks_addresses() {
        assert_eq!(
            listen_column(&[
                listener("127.0.0.1:9000", Some("127.0.0.1:9000"), false),
                listener("127.0.0.1:9443", Some("127.0.0.1:9443"), true),
            ]),
            "127.0.0.1:9000, :9443 (tls)"
        );
        assert_eq!(
            listen_column(&[listener("127.0.0.1:9100", None, false)]),
            "127.0.0.1:9100 (configured)"
        );
        assert_eq!(listen_column(&[]), "-");
    }

    #[test]
    fn a_configured_port_clashes_with_another_instances_bound_one() {
        let row = |name: &str, configured: &str, bound: Option<&str>| {
            let mut record = Record::new(
                registry::InstanceId::generate(),
                Kind::Daemon,
                muru_controller::client::Endpoint::unix("/tmp/admin.sock"),
                PathBuf::from("/tmp/run"),
            );
            record.name = Some(name.to_owned());
            record.listeners = vec![listener(configured, bound, false)];
            Row {
                record,
                state: State::Running,
            }
        };

        let listed = ports(&[
            row("dev", "127.0.0.1:9000", Some("127.0.0.1:9000")),
            row("staging", "0.0.0.0:9000", None),
            row("other", "127.0.0.1:9100", None),
        ]);
        let shown: Vec<(u16, &str, &str, Option<&str>)> = listed
            .iter()
            .map(|port| {
                (
                    port.port,
                    port.instance.as_str(),
                    port.state.as_str(),
                    port.clashes_with.as_deref(),
                )
            })
            .collect();

        assert_eq!(
            shown,
            [
                (9000, "staging", "configured", Some("dev")),
                (9000, "dev", "bound", None),
                (9100, "other", "configured", None),
            ]
        );
    }

    #[test]
    fn uptimes_are_shown_in_their_two_largest_units() {
        let ago = |seconds: u64| uptime(crate::admin::now_unix_ms() - seconds * 1000);

        assert_eq!(ago(40), "40s");
        assert_eq!(ago(14 * 60 + 5), "14m");
        assert_eq!(ago(2 * 3600 + 4 * 60), "2h 4m");
        assert_eq!(ago(3 * 86_400 + 2 * 3600), "3d 2h");
    }

    #[test]
    fn a_row_owns_the_processes_of_its_id_or_its_tag() {
        let id = registry::InstanceId::generate();
        let mut record = Record::new(
            id,
            Kind::Daemon,
            muru_controller::client::Endpoint::unix("/tmp/admin.sock"),
            PathBuf::from("/tmp/run"),
        );
        record.name = Some("dev".to_owned());
        let row = Row {
            record,
            state: State::Running,
        };
        let process = |id: Option<registry::InstanceId>, tag: Option<&str>| MuruProcess {
            pid: 1,
            parent: 0,
            uid: None,
            started_unix_ms: None,
            role: "host".to_owned(),
            title: String::new(),
            tag: tag.map(str::to_owned),
            id,
            label: None,
        };

        assert!(row.owns(&process(Some(id), None)));
        assert!(row.owns(&process(None, Some("dev"))));
        assert!(!row.owns(&process(
            Some(registry::InstanceId::generate()),
            Some("dev")
        )));
    }
}
