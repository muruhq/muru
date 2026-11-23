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

    /// Remove stale registry entries and stop orphaned Muru processes,
    /// after listing them and asking.
    Prune(PruneArgs),

    /// Define a named daemon instance: its definition and its daemon
    /// configuration file, with the listeners given.
    Create(CreateArgs),

    /// Remove a named instance's definition, and with --state its state
    /// and logs, after asking. Refused while the instance runs.
    Remove(RemoveArgs),
}

/// What `instance create` takes.
#[derive(Args)]
pub struct CreateArgs {
    /// The instance's name: 1 to 24 lowercase letters, digits and hyphens,
    /// starting with a letter; not user or system.
    #[arg(value_parser = new_name)]
    pub name: String,

    /// An address to serve plain HTTP on, as IP:PORT; give it once for
    /// each listener.
    #[arg(long = "listen", value_name = "ADDRESS", required = true)]
    pub listen: Vec<std::net::SocketAddr>,

    /// The loopback address to serve the dashboard on.
    #[arg(long, value_name = "ADDRESS")]
    pub dashboard: Option<std::net::SocketAddr>,

    /// What the instance is for, kept in its definition.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Define it in the system configuration area (/etc/muru), for a
    /// daemon that runs as root.
    #[arg(long)]
    pub system: bool,
}

/// What `instance remove` takes.
#[derive(Args)]
pub struct RemoveArgs {
    /// The instance's name.
    pub name: String,

    /// Also remove its state directory, run directory and logs.
    #[arg(long)]
    pub state: bool,

    /// The instance in the system configuration area.
    #[arg(long)]
    pub system: bool,

    /// Do it without asking.
    #[arg(long)]
    pub yes: bool,
}

/// Check a new instance's name: a valid name and not a default daemon's.
fn new_name(text: &str) -> Result<String, String> {
    muru_controller::locations::check_name(text)?;

    if muru_controller::locations::RESERVED_NAMES.contains(&text) {
        return Err(format!(
            "{text} names the default daemon of that name; choose another"
        ));
    }

    Ok(text.to_owned())
}

/// The label an instance is started with, beside its other options.
#[derive(Args, Clone, Debug, Default)]
pub struct LabelArgs {
    /// A label for the instance, which muructl instance list shows and
    /// --label filters on, and --instance selects it by when no other
    /// running instance has it (default: MURU_INSTANCE_LABEL).
    #[arg(long, value_name = "LABEL", value_parser = parse_label)]
    pub label: Option<String>,
}

/// Check a label: 1 to 64 letters, digits, hyphens, underscores and dots,
/// so it is one word in a table and on a command line.
fn parse_label(text: &str) -> Result<String, String> {
    let valid = !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));

    if !valid {
        return Err(format!(
            "{text:?} is not a label: 1 to 64 letters, digits, hyphens, underscores and dots"
        ));
    }

    Ok(text.to_owned())
}

/// What `instance prune` takes.
#[derive(Args)]
pub struct PruneArgs {
    /// Only what carries this label.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,

    /// Only the leftovers of one instance, by its name, tag or ID (in
    /// full, or its first 8 hex digits).
    #[arg(long = "instance", value_name = "NAME|ID")]
    pub only: Option<String>,

    /// Also stop the processes of detached instances, such as the edge
    /// and hosts a daemon that detached left serving, and remove their
    /// entries.
    #[arg(long)]
    pub detached: bool,

    /// Remove stale entries and stop nothing.
    #[arg(long)]
    pub entries_only: bool,

    /// How long a process has after SIGTERM before it is killed.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = muru_config::parse_duration)]
    pub grace: Duration,

    /// Also other users' registries and processes (root only).
    #[arg(long)]
    pub all_users: bool,

    /// Only list what would be removed and stopped.
    #[arg(long)]
    pub dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    pub yes: bool,

    /// Report what was removed, stopped and kept as JSON.
    #[arg(long)]
    pub json: bool,
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
    /// `muructl detour run`.
    Detour,
}

impl KindArg {
    fn kind(self) -> Kind {
        match self {
            Self::Daemon => Kind::Daemon,
            Self::Run => Kind::Run,
            Self::Serve => Kind::Serve,
            Self::Detour => Kind::Detour,
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
    /// A named instance that is defined and has no entry.
    Stopped,
}

impl State {
    /// The state as the table shows it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Detached => "detached",
            Self::Stale => "stale",
            Self::Stopped => "stopped",
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
    /// Its definition, for a named instance.
    pub definition: Option<muru_controller::catalogue::Definition>,
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
                definition: None,
            });
        }
    }

    // The named instances defined, which are stopped when they have no
    // entry; a definition and an entry of the same name are one row.

    for system in [false, true] {
        let (definitions, warnings) = muru_controller::catalogue::read(
            &muru_controller::locations::caller_config_area(system),
        );

        for warning in warnings {
            muru_common::muru_eprintln!("warning: {warning}");
        }

        for definition in definitions {
            let scope = if system { "system" } else { "user" };
            let entry = rows.iter().position(|row| {
                row.record.name.as_deref() == Some(definition.name.as_str())
                    && row.record.scope == scope
            });

            match entry {
                Some(at) => rows[at].definition = Some(definition),
                None => rows.push(stopped_row(definition, system)),
            }
        }

        // The system area is the caller's own for root.

        // SAFETY: geteuid has no preconditions and cannot fail.

        if unsafe { libc::geteuid() } == 0 {
            break;
        }
    }

    rows
}

/// The locations a named instance's name gives it.
pub fn named_locations(name: &str, system: bool) -> muru_controller::locations::Locations {
    muru_controller::locations::Locations::resolve(&muru_controller::locations::Options {
        instance: Some(name.to_owned()),
        system,
        ..muru_controller::locations::Options::default()
    })
}

/// What a named instance's daemon file configures it to listen on, with
/// its dashboard; nothing when the file cannot be read.
pub fn configured(locations: &muru_controller::locations::Locations) -> Vec<registry::Listener> {
    let Ok(settings) = muru_controller::settings::NodeSettings::read(
        &locations.config.path,
        &locations.state.path.join("secrets"),
        &muru_controller::settings::Overrides::default(),
    ) else {
        return Vec::new();
    };

    settings
        .listeners
        .iter()
        .map(|(address, tls)| registry::Listener {
            name: if *tls { "https" } else { "http" }.to_owned(),
            configured: address.to_string(),
            bound: None,
            tls: *tls,
        })
        .chain(settings.dashboard.map(|address| registry::Listener {
            name: "dashboard".to_owned(),
            configured: address.to_string(),
            bound: None,
            tls: false,
        }))
        .collect()
}

/// The row of a named instance that is defined and has no entry, with
/// what its file configures.
fn stopped_row(definition: muru_controller::catalogue::Definition, system: bool) -> Row {
    let locations = named_locations(&definition.name, system);
    let id = registry::InstanceId::read_from(
        &locations
            .state
            .path
            .join(muru_controller::daemon::INSTANCE_ID_FILE),
    )
    .unwrap_or_else(registry::InstanceId::nil);
    let mut record = Record::new(
        id,
        Kind::Daemon,
        muru_controller::client::Endpoint::unix(locations.admin_socket()),
        locations.run.path.clone(),
    );

    record.name = Some(definition.name.clone());
    record.scope = if system { "system" } else { "user" }.to_owned();
    record.label = None;
    record.pid = 0;
    record.process_started_unix_ms = None;
    record.started_unix_ms = 0;
    record.state_dir = Some(locations.state.path.clone());
    record.config = Some(locations.config.path.clone());
    record.log_dir = Some(locations.logs.path.clone());
    let (dashboard, listeners): (Vec<_>, Vec<_>) = configured(&locations)
        .into_iter()
        .partition(|listener| listener.name == "dashboard");
    record.listeners = listeners;
    record.dashboard = dashboard
        .first()
        .map(|listener| format!("http://{}/", listener.configured));

    Row {
        record,
        state: State::Stopped,
        definition: Some(definition),
    }
}

/// The addresses named instances other than the one at `locations` are
/// configured with and do not run on, by instance: what a daemon about to
/// start would clash with only once they start.
pub fn configured_elsewhere(
    locations: &muru_controller::locations::Locations,
) -> Vec<(String, String)> {
    let processes = muru_controller::lookup::muru_processes();
    let Ok(registries) = registries(false) else {
        return Vec::new();
    };

    rows(&registries, &processes)
        .into_iter()
        .filter(|row| row.state == State::Stopped)
        .filter(|row| row.record.name != locations.instance)
        .flat_map(|row| {
            let system = row.record.scope == "system";
            let name = row.tag();
            configured(&named_locations(&name, system))
                .into_iter()
                .map(move |listener| (name.clone(), listener.configured))
        })
        .collect()
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

    for row in rows
        .iter()
        .filter(|row| row.alive() || row.state == State::Stopped)
    {
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
                row.alive(),
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

/// A row as `--json` prints it: the record with its state and location,
/// and for an instance that can be reached what `--instance` takes for it.
fn row_json(row: &Row, running: &[Record]) -> serde_json::Value {
    let mut value = serde_json::to_value(&row.record).unwrap_or_default();

    if let Some(object) = value.as_object_mut() {
        // A stopped instance has a definition and no record of its own,
        // so it has no process or start time to give.

        if row.state == State::Stopped {
            for field in ["pid", "started_unix_ms", "uid", "muru_version", "host"] {
                object.remove(field);
            }

            if row.record.id == registry::InstanceId::nil() {
                object.remove("id");
            }
        }

        if let Some(definition) = &row.definition {
            object.insert(
                "definition".to_owned(),
                serde_json::json!({
                    "kind": definition.file.kind,
                    "description": definition.file.description,
                    "directory": definition.directory,
                }),
            );
        }

        object.insert("tag".to_owned(), row.tag().into());

        if let Some(selector) = selector(row, running) {
            object.insert("selector".to_owned(), selector.into());
        }

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

/// The WHERE column: a run's or serve's project and application, a named
/// instance's `instances/NAME`, or a daemon's state directory.
fn where_column(row: &Row) -> String {
    let record = &row.record;

    if let Some(definition) = &row.definition {
        return format!("instances/{}", definition.name);
    }

    match (record.kind, &record.project, &record.app) {
        (Kind::Run | Kind::Serve | Kind::Detour, Some(project), Some(app)) => {
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

    // What selects each running instance is worked out among all of
    // them, whatever the filters keep.

    let running: Vec<Record> = all
        .iter()
        .filter(|row| row.state == State::Running)
        .map(|row| row.record.clone())
        .collect();
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
            "instances": rows.iter().map(|row| row_json(row, &running)).collect::<Vec<_>>(),
        });

        if args.orphans {
            answer["orphans"] = serde_json::to_value(&orphans)?;
        }

        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        print_rows(&rows, &running);

        if args.orphans {
            print_orphans(&orphans);
        }
    }

    Ok(i32::from(args.orphans && !orphans.is_empty()))
}

/// What `--instance` takes to reach a row's instance (issue 868): for a
/// running instance the value that selects it among `running`, for a
/// stopped named instance its name; none for an instance that cannot be
/// reached.
fn selector(row: &Row, running: &[Record]) -> Option<String> {
    match row.state {
        State::Running => Some(muru_controller::selector::handle(&row.record, running)),
        State::Stopped => row.record.name.clone(),
        State::Detached | State::Stale => None,
    }
}

/// The instance table. The NAME of an instance that can be reached is
/// what `--instance` takes for it; of one that cannot, its tag.
fn print_rows(rows: &[Row], reachable: &[Record]) {
    if rows.is_empty() {
        println!("muru: no instances to list");
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
            selector(row, reachable).unwrap_or_else(|| row.tag()),
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
            where_column(row),
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

/// A process prune stops, with what it was found as.
#[derive(Debug, Clone, Serialize)]
pub struct Stopping {
    /// Its pid.
    pub pid: u32,
    /// Its role.
    pub role: String,
    /// Its title.
    pub title: String,
    /// Its instance's tag, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Why it is stopped.
    pub why: String,
    /// When it started, so a reused pid is never signalled.
    #[serde(skip)]
    started_unix_ms: Option<u64>,
    /// Its parent, so a worker goes with its host.
    #[serde(skip)]
    parent: u32,
}

/// An entry prune removes.
#[derive(Debug, Clone, Serialize)]
pub struct Removing {
    /// The instance, by its tag.
    pub instance: String,
    /// Its kind.
    pub kind: String,
    /// Its ID.
    pub id: String,
    /// Its state.
    pub state: String,
    #[serde(skip)]
    registry: PathBuf,
    #[serde(skip)]
    run_dir: Option<PathBuf>,
}

/// Something prune leaves, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Kept {
    /// What it is.
    pub what: String,
    /// Why it is left.
    pub why: String,
}

/// What prune would do, and then what it did.
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    /// The entries removed.
    pub removed: Vec<Removing>,
    /// The processes stopped.
    pub stopped: Vec<Stopping>,
    /// What was left, with why.
    pub kept: Vec<Kept>,
    /// What could not be done, with why.
    pub failed: Vec<Kept>,
}

/// Whether `row` is the instance `wanted` names: by its name, tag, full ID
/// or the first hex digits of it.
fn named(row: &Row, wanted: &str) -> bool {
    let id = row.record.id.to_string();

    row.record.name.as_deref() == Some(wanted)
        || row.tag() == wanted
        || (wanted.len() >= 8 && id.starts_with(wanted))
}

/// Work out what prune removes and stops.
pub async fn plan(args: &PruneArgs) -> anyhow::Result<Plan> {
    crate::admin::require_local("instance prune", args.only.as_deref())?;
    let registries = registries(args.all_users)?;
    let processes = muru_controller::lookup::muru_processes();
    let all = rows(&registries, &processes);
    let mut plan = Plan::default();

    let wanted_row = |row: &Row| {
        args.label
            .as_ref()
            .is_none_or(|label| row.record.label.as_ref() == Some(label))
            && args.only.as_deref().is_none_or(|only| named(row, only))
    };

    // The entries whose owners are gone; a detached one only with
    // --detached, once its processes are stopped.

    for (row, path) in all.iter().zip(registry_paths(&all, &registries)) {
        if !wanted_row(row) {
            continue;
        }

        let removing = Removing {
            instance: row.tag(),
            kind: row.record.kind.word().to_owned(),
            id: row.record.id.to_string(),
            state: row.state.word().to_owned(),
            registry: path,
            run_dir: matches!(row.record.kind, Kind::Run | Kind::Detour)
                .then(|| row.record.run_dir.clone()),
        };

        match row.state {
            State::Stale => plan.removed.push(removing),
            State::Detached if args.detached && !args.entries_only => plan.removed.push(removing),
            State::Detached => plan.kept.push(Kept {
                what: format!("{} {}", row.record.kind.word(), row.tag()),
                why: "it is detached and its processes keep serving; --detached stops them"
                    .to_owned(),
            }),
            State::Running | State::Stopped => {}
        }
    }

    if args.entries_only {
        return Ok(plan);
    }

    // The orphans, and with --detached the processes of the detached
    // instances asked for.

    let orphans = orphans(&all, &processes).await;

    for orphan in orphans {
        let owner = processes.iter().find(|process| process.pid == orphan.pid);
        let row = owner.and_then(|process| all.iter().find(|row| row.owns(process)));

        let wanted = match row {
            Some(row) => wanted_row(row),
            None => {
                args.only.is_none()
                    && args
                        .label
                        .as_ref()
                        .is_none_or(|label| orphan.label.as_ref() == Some(label))
            }
        };

        if wanted {
            plan.stopped.push(Stopping {
                pid: orphan.pid,
                role: orphan.role.clone(),
                title: orphan.title.clone(),
                instance: orphan.instance.clone(),
                why: orphan.why.clone(),
                started_unix_ms: orphan.started_unix_ms,
                parent: orphan.parent_pid,
            });
        }
    }

    for row in all
        .iter()
        .filter(|row| row.state == State::Detached && wanted_row(row))
    {
        for process in processes.iter().filter(|process| row.owns(process)) {
            if !args.detached {
                plan.kept.push(Kept {
                    what: format!("{} {} ({})", process.role, process.pid, process.title),
                    why: format!("it serves for the detached instance {}", row.tag()),
                });
                continue;
            }

            plan.stopped.push(Stopping {
                pid: process.pid,
                role: process.role.clone(),
                title: process.title.clone(),
                instance: process.tag.clone(),
                why: format!("its instance {} is detached", row.tag()),
                started_unix_ms: process.started_unix_ms,
                parent: process.parent,
            });
        }
    }

    Ok(plan)
}

/// The registry each row came from, by where its entry is.
fn registry_paths(rows: &[Row], registries: &[PathBuf]) -> Vec<PathBuf> {
    rows.iter()
        .map(|row| {
            registries
                .iter()
                .find(|registry| registry.join(row.record.id.to_string()).is_dir())
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

/// The order processes are stopped in: hosts and everything else before
/// relays, which carry the hosts' last output, and workers whose host is
/// being stopped are left to end with it.
fn stage(stopping: &Stopping, pids: &HashSet<u32>) -> u8 {
    match stopping.role.as_str() {
        "worker" | "process" if pids.contains(&stopping.parent) => 1,
        "relay" => 1,
        _ => 0,
    }
}

/// Signal `stopping` when it is still the process found.
fn signal(stopping: &Stopping, signal: i32) -> Result<bool, String> {
    if !muru_controller::lookup::still_the_same(stopping.pid, stopping.started_unix_ms) {
        return Ok(false);
    }

    // SAFETY: kill sends a signal and touches no memory; the pid was just
    // checked to be the process found, by its start time.

    let sent = unsafe { libc::kill(stopping.pid as libc::pid_t, signal) };

    if sent == 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();

    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        _ => Err(error.to_string()),
    }
}

/// Stop the processes: SIGTERM, the grace period, then SIGKILL, a stage
/// at a time; each pid checked against its start time before every
/// signal. Those the caller may not signal are reported and left.
async fn stop(plan: &mut Plan, grace: Duration) {
    let pids: HashSet<u32> = plan.stopped.iter().map(|stopping| stopping.pid).collect();
    let mut refused: Vec<(usize, String)> = Vec::new();

    for wanted in [0, 1] {
        let staged: Vec<usize> = (0..plan.stopped.len())
            .filter(|index| stage(&plan.stopped[*index], &pids) == wanted)
            .collect();

        for index in &staged {
            if let Err(error) = signal(&plan.stopped[*index], libc::SIGTERM) {
                refused.push((*index, error));
            }
        }

        // The stage's processes get the grace period to exit.

        let deadline = std::time::Instant::now() + grace;

        while std::time::Instant::now() < deadline
            && staged.iter().any(|index| {
                let stopping = &plan.stopped[*index];
                muru_controller::lookup::still_the_same(stopping.pid, stopping.started_unix_ms)
            })
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Whatever outlived its grace period is killed.

    for stopping in &plan.stopped {
        let _ = signal(stopping, libc::SIGKILL);
    }

    for (index, why) in refused.iter().rev() {
        let stopping = plan.stopped.remove(*index);
        plan.failed.push(Kept {
            what: format!("{} {} ({})", stopping.role, stopping.pid, stopping.title),
            why: why.clone(),
        });
    }
}

/// Remove the entries, each only while its lock can be taken, and a
/// run's own directory with it.
fn remove(plan: &mut Plan) {
    let mut removed = Vec::new();

    for removing in plan.removed.drain(..) {
        let Ok(id) = removing.id.parse() else {
            continue;
        };

        match registry::remove_stale(&removing.registry, id) {
            Ok(true) => {
                if let Some(run_dir) = &removing.run_dir
                    && run_dir
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
                {
                    let _ = std::fs::remove_dir_all(run_dir);
                }

                removed.push(removing);
            }
            Ok(false) => plan.kept.push(Kept {
                what: format!("{} {}", removing.kind, removing.instance),
                why: "its owner holds its lock again".to_owned(),
            }),
            Err(error) => plan.failed.push(Kept {
                what: format!("{} {}", removing.kind, removing.instance),
                why: error.to_string(),
            }),
        }
    }

    plan.removed = removed;
}

/// What prune is about to do, for the question and --dry-run.
fn print_plan(plan: &Plan) {
    for removing in &plan.removed {
        println!(
            "remove   {} {} ({}, {})",
            removing.kind,
            removing.instance,
            removing.state,
            &removing.id[..8]
        );
    }

    for stopping in &plan.stopped {
        println!(
            "stop     {} {}: {} ({})",
            stopping.role, stopping.pid, stopping.title, stopping.why
        );
    }

    for kept in &plan.kept {
        println!("keep     {}: {}", kept.what, kept.why);
    }
}

/// `muructl instance prune`.
pub async fn prune(args: PruneArgs) -> anyhow::Result<()> {
    let mut plan = plan(&args).await?;

    if plan.removed.is_empty() && plan.stopped.is_empty() {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan);
            println!("muru: nothing to prune");
        }

        return Ok(());
    }

    if args.dry_run {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan);
            println!("muru: nothing was changed (--dry-run)");
        }

        return Ok(());
    }

    if !args.json {
        print_plan(&plan);
    }

    let question = format!(
        "remove {} and stop {}?",
        counted(plan.removed.len(), "entry", "entries"),
        counted(plan.stopped.len(), "process", "processes")
    );

    if !crate::admin::confirm(&question, args.yes)? {
        println!("muru: nothing was changed");
        return Ok(());
    }

    stop(&mut plan, args.grace).await;
    remove(&mut plan);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "muru: removed {} and stopped {}",
            counted(plan.removed.len(), "entry", "entries"),
            counted(plan.stopped.len(), "process", "processes")
        );

        for failed in &plan.failed {
            println!("muru: left {}: {}", failed.what, failed.why);
        }
    }

    Ok(())
}

/// `count` with the singular or plural noun.
fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// `muructl instance create`: write the definition and the daemon's
/// file, refusing a name that exists and an address another defined or
/// running instance uses.
pub fn create(args: CreateArgs) -> anyhow::Result<()> {
    let area = muru_controller::locations::caller_config_area(args.system);
    let directory = area
        .join(muru_controller::locations::INSTANCES_DIR)
        .join(&args.name);

    if directory.exists() {
        anyhow::bail!(
            "instance {} is already defined, in {}",
            args.name,
            directory.display()
        );
    }

    // Every address is checked against the instances defined and those
    // running, so two never clash once both are started.

    let processes = muru_controller::lookup::muru_processes();
    let all = rows(&registries(false)?, &processes);
    let addresses: Vec<(std::net::SocketAddr, &str)> = args
        .listen
        .iter()
        .map(|address| (*address, "listener"))
        .chain(args.dashboard.map(|address| (address, "dashboard address")))
        .collect();

    for (address, what) in &addresses {
        if let Some(message) = muru_controller::registry::in_use_by_instance(
            *address,
            &format!("choose another {what}"),
        ) {
            anyhow::bail!(message);
        }

        for row in all.iter().filter(|row| row.definition.is_some()) {
            let system = row.record.scope == "system";
            let clash = configured(&named_locations(&row.tag(), system))
                .into_iter()
                .any(|listener| registry::clashes(&listener.configured, &address.to_string()));

            if clash {
                anyhow::bail!(
                    "{address} is configured for instance {} already; choose another {what}",
                    row.tag()
                );
            }
        }
    }

    let mut config = format!(
        "# The daemon configuration of instance {}, which muructl instance create wrote.\n",
        args.name
    );

    for address in &args.listen {
        config.push_str(&format!("\n[[listener]]\naddress = '{address}'\n"));
    }

    if let Some(dashboard) = args.dashboard {
        config.push_str(&format!("\n[dashboard]\nlisten = '{dashboard}'\n"));
    }

    let definition =
        muru_controller::catalogue::create(&area, &args.name, args.description.as_deref(), &config)
            .map_err(|error| anyhow::anyhow!("defining instance {}: {error}", args.name))?;

    println!(
        "muru: instance {} is defined in {}; start it with muructl daemon start --instance {}",
        args.name,
        definition.directory.display(),
        args.name
    );
    Ok(())
}

/// `muructl instance remove`: the definition, and with --state the state,
/// run and log directories, after asking; refused while the instance runs.
pub fn remove_instance(args: RemoveArgs) -> anyhow::Result<()> {
    let area = muru_controller::locations::caller_config_area(args.system);
    let Some(definition) = muru_controller::catalogue::find(&area, &args.name) else {
        let (defined, _) = muru_controller::catalogue::read(&area);

        anyhow::bail!(
            "no instance {} is defined{}",
            args.name,
            if defined.is_empty() {
                ""
            } else {
                "; muructl instance list shows the ones that are"
            }
        );
    };

    let processes = muru_controller::lookup::muru_processes();
    let row = rows(&registries(false)?, &processes)
        .into_iter()
        .find(|row| row.definition.as_ref() == Some(&definition));

    if let Some(row) = row.as_ref().filter(|row| row.alive()) {
        anyhow::bail!(
            "instance {} is {}; stop it first with muructl daemon stop --instance {} --shutdown",
            args.name,
            row.state.word(),
            args.name
        );
    }

    // The definition is its two files; on macOS the instance's state is a
    // directory beside them, which goes only with --state.

    let locations = named_locations(&args.name, args.system);
    let mut removed: Vec<PathBuf> = vec![
        definition
            .directory
            .join(muru_controller::catalogue::DEFINITION_FILE),
        definition.config(),
    ];

    if args.state {
        removed.extend([
            locations.state.path.clone(),
            locations.run.path.clone(),
            locations.logs.path.clone(),
        ]);
    }

    let removed: Vec<PathBuf> = removed.into_iter().filter(|path| path.exists()).collect();

    for path in &removed {
        println!("remove   {}", path.display());
    }

    if !crate::admin::confirm(&format!("remove instance {}?", args.name), args.yes)? {
        println!("muru: nothing was changed");
        return Ok(());
    }

    // A stale entry the instance left goes with it.

    if let Some(row) = row.filter(|row| row.state == State::Stale) {
        for registry in registries(false)? {
            let _ = registry::remove_stale(&registry, row.record.id);
        }
    }

    for path in &removed {
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        gone.map_err(|error| anyhow::anyhow!("removing {}: {error}", path.display()))?;
    }

    // The definition's directory goes once nothing is left in it.

    let _ = std::fs::remove_dir(&definition.directory);

    if !args.state && definition.directory.exists() {
        println!(
            "muru: instance {} is removed; its state is kept in {}, which --state removes",
            args.name,
            locations.state.path.display()
        );
    } else {
        println!("muru: instance {} is removed", args.name);
    }
    Ok(())
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
                definition: None,
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
    fn a_pid_with_another_start_time_is_never_signalled() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("sleep started");
        let started = muru_controller::processes::read(child.id())
            .and_then(|found| found.started_unix_ms)
            .expect("its start time");
        let pid = child.id();
        let stopping = |started_unix_ms| Stopping {
            pid,
            role: "host".to_owned(),
            title: String::new(),
            instance: None,
            why: String::new(),
            started_unix_ms,
            parent: 1,
        };

        // A start time a minute out is another process that has the pid.

        assert_eq!(
            signal(&stopping(Some(started + 60_000)), libc::SIGTERM),
            Ok(false)
        );
        assert!(child.try_wait().expect("waited").is_none());

        assert_eq!(signal(&stopping(Some(started)), libc::SIGTERM), Ok(true));
        assert!(child.wait().expect("waited").code().is_none());
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
            definition: None,
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
