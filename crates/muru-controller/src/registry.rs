//! The per-user registry of the Muru instances running on this machine
//! (design specification section 13.8, issue 805, ADR 0069).
//!
//! The registry records presence and nothing else: an entry exists
//! because a local process is alive and holds its lock. The instances a
//! user knows by name are a separate catalogue of definitions (section
//! 13.8, named instances), which `muructl instance list` merges with this.
//!
//! There is no shared index. Each instance owns one directory,
//! `REGISTRY/ID/`, holding `lock`, on which the owner keeps an exclusive
//! `flock` for as long as it lives, and `instance.json`, the record, which
//! the owner alone writes, replacing it whole by a rename. A free lock
//! means the owner is gone, however it went, and the pid is never used to
//! decide that. A new entry is made under `.new-ID/`, locked, written,
//! and only then renamed to its final name, so an entry under that name
//! is always already locked; an entry is removed by renaming it to
//! `.gone-ID-RANDOM/` first, so no reader sees one half deleted. Readers
//! take no lock to read, skip names beginning with `.` and anything that
//! vanishes or does not parse, and test liveness with a shared lock they
//! release at once.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub use crate::client::Endpoint;
use crate::fdpass;

/// The record's format, written in every record.
pub const FORMAT: u32 = 1;

/// An entry's lock file.
const LOCK: &str = "lock";

/// The record inside an entry.
const RECORD: &str = "instance.json";

/// The variable a test session, or anyone, sets to mark the instances it
/// starts; the registry records it as the instance's label.
pub const LABEL_VARIABLE: &str = "MURU_INSTANCE_LABEL";

/// The system daemon's registry, which any user may read.
pub const SYSTEM_REGISTRY: &str = "/run/muru/registry";

/// The caller's own registry: `registry` in the account's state
/// location, whatever `--state-dir`, `MURU_STATE_DIR`, `XDG_STATE_HOME`
/// or `HOME` says, so it is always in the same place for a user. It holds regular files, so it is never in
/// the runtime location, which on macOS is under `/tmp`, where files are
/// deleted by age while their owner still runs (issue 948). Root's is
/// the system registry, which stays in `/run/muru`: that is not cleaned
/// by age, it is emptied at boot as the instances it lists are, and
/// every user must be able to read it, which the system state directory
/// does not allow. A user with no home directory, as a container given
/// an arbitrary user ID has, keeps it in the default runtime location,
/// where nothing cleans by age inside a container.
pub fn user_registry() -> PathBuf {
    if crate::locations::caller_is_root() {
        return PathBuf::from(SYSTEM_REGISTRY);
    }

    crate::locations::account_state()
        .unwrap_or_else(crate::locations::default_user_runtime)
        .join("registry")
}

/// An instance's ID: 128 random bits, written as 32 hex digits and shown
/// short as the first 8, so an ID recorded on one machine names one
/// instance anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId([u8; 16]);

impl InstanceId {
    /// A new random ID.
    pub fn generate() -> Self {
        use ring::rand::SecureRandom as _;

        let mut bytes = [0u8; 16];

        // The system's random source does not fail on any platform Muru
        // runs on; were it to, the time and pid still keep IDs apart.

        if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
            let now = crate::model::now_unix_ms().to_be_bytes();
            bytes[..8].copy_from_slice(&now);
            bytes[8..12].copy_from_slice(&std::process::id().to_be_bytes());
        }

        Self(bytes)
    }

    /// The ID of an instance that has none yet, as a named instance that
    /// has never run.
    pub fn nil() -> Self {
        Self([0; 16])
    }

    /// The ID kept in `path`, when the file holds one.
    pub fn read_from(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// The first 8 hex digits, as titles and listings show it.
    pub fn short(&self) -> String {
        self.to_string()[..8].to_owned()
    }

    /// The ID kept in `path`, or a new one written there when the file is
    /// missing or does not hold one: a daemon's, in its state directory.
    pub fn kept_in(path: &Path) -> std::io::Result<Self> {
        if let Some(id) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return Ok(id);
        }

        let id = Self::generate();
        muru_common::files::write_whole(path, format!("{id}\n"))?;
        Ok(id)
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }

        Ok(())
    }
}

impl std::str::FromStr for InstanceId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("{text:?} is not an instance ID of 32 hex digits");

        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid());
        }

        let mut bytes = [0u8; 16];

        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }

        Ok(Self(bytes))
    }
}

impl Serialize for InstanceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What kind of instance a record is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A daemon.
    Daemon,
    /// A development run, `muructl run`.
    Run,
    /// `muructl serve`.
    Serve,
    /// `muructl detour run`, a development run behind a daemon's edge
    /// (section 13.12).
    Detour,
}

impl Kind {
    /// The kind as the record writes it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Run => "run",
            Self::Serve => "serve",
            Self::Detour => "detour",
        }
    }
}

/// One listener in the configuration, as configured and as bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    /// What it is for: `http` or `https`.
    pub name: String,
    /// The address as written, which may have port 0 or `auto`.
    pub configured: String,
    /// The address actually bound; none until bound, or when the bind failed.
    #[serde(default)]
    pub bound: Option<String>,
    /// Whether it carries TLS.
    #[serde(default)]
    pub tls: bool,
}

/// An instance's record, which its owner alone writes. Fields that do not
/// apply to the kind are absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The record's format, [`FORMAT`].
    pub format: u32,
    /// The instance's ID.
    pub id: InstanceId,
    /// The machine's hostname.
    pub host: String,
    /// What kind of instance it is.
    pub kind: Kind,
    /// Its name: `user` or `system` for the default daemons, a named
    /// instance's name; none for a run, a serve, or a daemon whose
    /// locations were given by flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `user` or `system`.
    pub scope: String,
    /// The label from `MURU_INSTANCE_LABEL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The owner's pid.
    pub pid: u32,
    /// When the operating system says `pid` started, so a command about
    /// to signal it can check it has not been reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_started_unix_ms: Option<u64>,
    /// When the instance started.
    pub started_unix_ms: u64,
    /// The version of Muru it runs.
    pub muru_version: String,
    /// The user it runs as.
    pub uid: u32,
    /// How to reach its admin API.
    pub admin: Endpoint,
    /// Its run directory, where its sockets are: a daemon's or a serve's,
    /// or a run's own directory under the run directory it was started
    /// with.
    pub run_dir: PathBuf,
    /// Where it keeps its runtime files (section 13.8): `run` in a
    /// daemon's state directory, or a run's own directory of them; none
    /// for a serve, which keeps them with its sockets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_dir: Option<PathBuf>,
    /// A daemon's state directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    /// A daemon's configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// A daemon's log directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<PathBuf>,
    /// One entry for each listener in the configuration.
    #[serde(default)]
    pub listeners: Vec<Listener>,
    /// The dashboard's URL, when one is served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<String>,
    /// A daemon's edge process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_pid: Option<u32>,
    /// A run's `--host` names, in the order given; empty for a run with
    /// none, and for a daemon, whose names belong to its virtual hosts.
    #[serde(default)]
    pub names: Vec<String>,
    /// A run's or a serve's application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// A run's or a serve's project directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    /// The directory a run was started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<PathBuf>,
    /// The runtime library directories in use.
    #[serde(default)]
    pub runtime: Vec<PathBuf>,
}

impl Record {
    /// A record for this process, of `kind`, reached at `admin`, with its
    /// run directory; the rest is filled by the caller.
    pub fn new(id: InstanceId, kind: Kind, admin: Endpoint, run_dir: PathBuf) -> Self {
        let pid = std::process::id();

        // SAFETY: getuid and geteuid have no preconditions and cannot fail.

        let (uid, root) = unsafe { (libc::getuid(), libc::geteuid() == 0) };

        Self {
            format: FORMAT,
            id,
            host: crate::processes::hostname(),
            kind,
            name: None,
            scope: if root { "system" } else { "user" }.to_owned(),
            label: std::env::var(LABEL_VARIABLE)
                .ok()
                .filter(|label| !label.is_empty()),
            pid,
            process_started_unix_ms: crate::processes::read(pid)
                .and_then(|found| found.started_unix_ms),
            started_unix_ms: crate::model::now_unix_ms(),
            muru_version: env!("CARGO_PKG_VERSION").to_owned(),
            uid,
            admin,
            run_dir,
            files_dir: None,
            state_dir: None,
            config: None,
            log_dir: None,
            listeners: Vec::new(),
            dashboard: None,
            edge_pid: None,
            names: Vec::new(),
            app: None,
            project: None,
            started_from: None,
            runtime: Vec::new(),
        }
    }

    /// The instance's tag, which its processes' titles carry: its name,
    /// `run:PORT` for a run, with the port it is configured to serve on,
    /// which is known before anything starts, `serve` for a serve,
    /// `detour:` and the short ID for a detour, which has no port, or the
    /// short ID; a run on a port picked when it binds, `--port auto`, is
    /// `run:` and its short ID.
    pub fn tag(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }

        match self.kind {
            Kind::Run => match self.configured_port() {
                Some(port) => format!("run:{port}"),
                None => format!("run:{}", self.id.short()),
            },
            Kind::Serve => "serve".to_owned(),
            Kind::Detour => format!("detour:{}", self.id.short()),
            Kind::Daemon => self.id.short(),
        }
    }

    /// The port the first listener is configured with, when it is not one
    /// picked at bind time.
    pub fn configured_port(&self) -> Option<u16> {
        let listener = self.listeners.first()?;
        let port: u16 = listener.configured.rsplit(':').next()?.parse().ok()?;
        (port != 0).then_some(port)
    }

    /// The instance, as a sentence names it: `the development run of shop
    /// in ~/src/shop (pid 4123, dashboard http://127.0.0.1:8001/)`, or
    /// `daemon instance dev (pid 5120)`.
    pub fn describe(&self) -> String {
        let dashboard = match (self.kind, self.dashboard.as_deref()) {
            (Kind::Run | Kind::Detour, Some(dashboard)) => format!(", dashboard {dashboard}"),
            _ => String::new(),
        };

        format!("{} (pid {}{dashboard})", self.summary(), self.pid)
    }

    /// The instance as [`Record::describe`] names it, without what only a
    /// running instance has: `the development run of shop in ~/src/shop`,
    /// for an instance that is no longer running.
    pub fn summary(&self) -> String {
        let app = self.app.as_deref().unwrap_or("an application");

        match self.kind {
            Kind::Run => {
                let project = self
                    .project
                    .as_deref()
                    .map(|project| format!(" in {}", home_shortened(project)))
                    .unwrap_or_default();
                format!("the development run of {app}{project}")
            }
            Kind::Detour => {
                let project = self
                    .project
                    .as_deref()
                    .map(|project| format!(" in {}", home_shortened(project)))
                    .unwrap_or_default();
                format!("the detour of {app}{project}")
            }
            Kind::Serve => format!("muructl serve of {app}"),
            Kind::Daemon => match &self.name {
                Some(name) => format!("daemon instance {name}"),
                None => format!(
                    "the daemon with the state directory {}",
                    self.state_dir
                        .as_deref()
                        .map_or_else(|| self.run_dir.display().to_string(), home_shortened)
                ),
            },
        }
    }
}

/// A live instance that holds an address, as a bind that failed names
/// it (section 13.8, issue 808).
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    /// The instance's record.
    pub record: Record,
    /// What of it holds the address: `http`, `https` or `dashboard`.
    pub what: String,
}

impl Holder {
    /// The instance, as a sentence names it: what [`Record::describe`]
    /// says.
    pub fn describe(&self) -> String {
        self.record.describe()
    }

    /// How to stop it: `stop it`, or for a daemon the command that does.
    pub fn stop_hint(&self) -> String {
        match (self.record.kind, self.record.name.as_deref()) {
            (Kind::Daemon, Some("user" | "system")) => {
                "stop it with muructl daemon stop".to_owned()
            }
            (Kind::Daemon, Some(name)) => {
                format!("stop it with muructl daemon stop --instance {name}")
            }
            (Kind::Daemon, None) => format!(
                "stop it with muructl daemon stop --socket {}",
                self.record.admin
            ),
            _ => "stop it".to_owned(),
        }
    }
}

/// Whether two addresses as written, `HOST:PORT`, would clash: the same
/// port on addresses that overlap, a wildcard overlapping every address
/// of its family.
pub fn clashes(one: &str, other: &str) -> bool {
    let (Ok(one), Ok(other)) = (
        one.parse::<std::net::SocketAddr>(),
        other.parse::<std::net::SocketAddr>(),
    ) else {
        return false;
    };

    one.port() != 0
        && one.port() == other.port()
        && (one.ip() == other.ip()
            || (one.is_ipv4() == other.is_ipv4()
                && (one.ip().is_unspecified() || other.ip().is_unspecified())))
}

/// The live instance in `registries` that holds `address`, by a bound
/// listener or its dashboard, other than this process.
pub fn holder_of(registries: &[PathBuf], address: std::net::SocketAddr) -> Option<Holder> {
    let wanted = address.to_string();
    let me = std::process::id();

    registries
        .iter()
        .flat_map(|registry| live(registry))
        .filter(|record| record.pid != me)
        .find_map(|record| {
            let listener = record
                .listeners
                .iter()
                .find(|listener| {
                    listener
                        .bound
                        .as_deref()
                        .is_some_and(|bound| clashes(bound, &wanted))
                })
                .map(|listener| listener.name.clone());

            let dashboard = record
                .dashboard
                .as_deref()
                .and_then(|url| url.strip_prefix("http://"))
                .map(|rest| rest.trim_end_matches('/'))
                .filter(|bound| clashes(bound, &wanted))
                .map(|_| "dashboard".to_owned());

            listener.or(dashboard).map(|what| Holder { record, what })
        })
}

/// What to say of a bind refused because `address` is in use, when a
/// live instance holds it: `port 8000 on 127.0.0.1 is in use by the
/// development run of shop in ~/src/shop (pid 4123); CHOOSE, or stop it`.
pub fn in_use_by_instance(address: std::net::SocketAddr, choose: &str) -> Option<String> {
    let holder = holder_of(&readable_registries(), address)?;

    Some(format!(
        "port {} on {} is in use by {}; {choose}, or {}",
        address.port(),
        address.ip(),
        holder.describe(),
        holder.stop_hint()
    ))
}

/// What to say of a bind refused because `address` is in use: the live
/// instance that holds it, as `in_use_by_instance` says, or else the
/// program lsof or ss name, `port 8000 on 127.0.0.1 is in use by
/// python3.14 (pid 4123); CHOOSE, or stop it`. None when neither is
/// found.
pub fn in_use_by(address: std::net::SocketAddr, choose: &str) -> Option<String> {
    if let Some(message) = in_use_by_instance(address, choose) {
        return Some(message);
    }

    let holder = port_holder(address.port())?;

    Some(format!(
        "port {} on {} is in use by {holder}; {choose}, or stop it",
        address.port(),
        address.ip()
    ))
}

/// The process listening on a TCP port, as "command (pid N)", found with
/// lsof or ss when either is installed.
pub fn port_holder(port: u16) -> Option<String> {
    let lsof = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .output()
        .ok()
        .filter(|output| output.status.success());

    if let Some(output) = lsof {
        let text = String::from_utf8_lossy(&output.stdout);
        let pid = text.lines().find_map(|line| line.strip_prefix('p'));
        let command = text.lines().find_map(|line| line.strip_prefix('c'));

        if let (Some(pid), Some(command)) = (pid, command) {
            return Some(format!("{command} (pid {pid})"));
        }
    }

    let ss = std::process::Command::new("ss")
        .args(["-lptnH", &format!("sport = :{port}")])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&ss.stdout);

    // users:(("python3",pid=1234,fd=5))

    let users = text.split("users:((\"").nth(1)?;
    let (command, rest) = users.split_once('"')?;
    let pid = rest.split("pid=").nth(1)?.split(',').next()?;
    Some(format!("{command} (pid {pid})"))
}

/// The registries every user may hold an address in that the caller can
/// read: its own and the system daemon's.
pub fn readable_registries() -> Vec<PathBuf> {
    let mut found = vec![user_registry()];
    let system = PathBuf::from(SYSTEM_REGISTRY);

    if !found.contains(&system) {
        found.push(system);
    }

    found
}

/// `path` with the home directory shortened to `~`.
pub fn home_shortened(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);

    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Make the registry directory, private to its user, or readable by
/// every user for the system registry.
fn prepare(registry: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let created = !registry.exists();
    std::fs::create_dir_all(registry)?;

    if created {
        let mode = if registry == Path::new(SYSTEM_REGISTRY) {
            0o755
        } else {
            0o700
        };
        std::fs::set_permissions(registry, std::fs::Permissions::from_mode(mode))?;
    }

    Ok(())
}

/// The mode an entry's files have: readable by every user in the system
/// registry, private elsewhere.
fn file_mode(registry: &Path) -> u32 {
    if registry == Path::new(SYSTEM_REGISTRY) {
        0o644
    } else {
        0o600
    }
}

/// A short random suffix, for the names an entry has on its way in or out.
fn random_suffix() -> String {
    InstanceId::generate().short()
}

/// Write `record` to `instance.json` in `directory`, by writing it beside
/// and renaming it over, so a reader sees the old record or the new.
fn write_record(directory: &Path, record: &Record, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let temporary = directory.join(format!("{RECORD}.tmp"));
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    drop(file);
    std::fs::rename(&temporary, directory.join(RECORD))
}

/// Open an entry's lock file, creating it when asked.
fn open_lock(directory: &Path, create: bool, mode: u32) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(mode)
        .open(directory.join(LOCK))
}

/// How many times a lock held only for a moment is tried again before it
/// counts as held by an owner. A reader testing an entry holds a shared
/// lock for an instant, and a process forked by the lock's holder keeps
/// a copy of it until it runs its program, since a `flock` belongs to the
/// open file and close-on-exec descriptors stay open until the exec.
const LOCK_TRIES: u32 = 20;

/// Take an exclusive lock, trying again for a moment, so that a hold
/// that lasts an instant (see [`LOCK_TRIES`]) is waited out.
fn lock_exclusive(file: &File) -> std::io::Result<bool> {
    for _ in 0..LOCK_TRIES {
        if fdpass::try_lock(file.as_raw_fd(), true)? {
            return Ok(true);
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    Ok(false)
}

/// Whether `file` is the file at `path`, rather than one renamed away
/// from it since it was opened.
fn is_at(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(open), Ok(there)) => open.dev() == there.dev() && open.ino() == there.ino(),
        _ => false,
    }
}

/// Remove an entry's directory by renaming it aside first, so a reader
/// never sees it half deleted.
fn remove_entry(registry: &Path, directory: &Path, id: InstanceId) -> std::io::Result<()> {
    let gone = registry.join(format!(".gone-{id}-{}", random_suffix()));
    std::fs::rename(directory, &gone)?;
    std::fs::remove_dir_all(&gone)
}

/// An entry this process owns: its lock held, its record written by it
/// alone. Dropped, it is removed, unless [`Entry::keep`] said to leave it.
#[derive(Debug)]
pub struct Entry {
    registry: PathBuf,
    directory: PathBuf,
    mode: u32,
    /// Held open, and so locked, for as long as the entry is owned.
    _lock: File,
    record: Mutex<Record>,
    keep: std::sync::atomic::AtomicBool,
}

impl Entry {
    /// Publish `record` in `registry`. An entry under the record's ID that
    /// its owner has left, as a daemon that detached or was killed leaves
    /// its own, is taken over by taking its free lock and writing the
    /// record anew; one whose lock is held is refused.
    pub fn publish(registry: &Path, record: Record) -> std::io::Result<Self> {
        prepare(registry)?;

        let mode = file_mode(registry);
        let id = record.id;
        let directory = registry.join(id.to_string());

        if directory.exists() {
            return Self::take_over(registry, directory, mode, record);
        }

        // Made under a name readers skip, locked, written, and only then
        // given its own name.

        let new = registry.join(format!(".new-{id}"));
        let _ = std::fs::remove_dir_all(&new);
        std::fs::create_dir(&new)?;

        let lock = open_lock(&new, true, mode)?;

        if !fdpass::try_lock(lock.as_raw_fd(), true)? {
            return Err(std::io::Error::other(format!(
                "the new registry entry {} is locked by another process",
                new.display()
            )));
        }

        write_record(&new, &record, mode)?;

        if let Err(error) = std::fs::rename(&new, &directory) {
            let _ = std::fs::remove_dir_all(&new);

            // Another process published the same ID in the meantime, which
            // only a daemon restarted twice at once can do.

            if directory.exists() {
                return Self::take_over(registry, directory, mode, record);
            }

            return Err(error);
        }

        Ok(Self::owned(registry, directory, mode, lock, record))
    }

    /// Take over the entry at `directory`, left by an owner that has gone.
    fn take_over(
        registry: &Path,
        directory: PathBuf,
        mode: u32,
        record: Record,
    ) -> std::io::Result<Self> {
        let lock = match open_lock(&directory, true, mode) {
            Ok(lock) => lock,

            // A pruner removed the entry since it was seen, so it is
            // published afresh (issue 819).
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::publish(registry, record);
            }

            // An entry with no lock file is a broken one: it is put aside
            // and published afresh.
            Err(_) => {
                match remove_entry(registry, &directory, record.id) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }

                return Self::publish(registry, record);
            }
        };

        if !lock_exclusive(&lock)? {
            return Err(std::io::Error::other(format!(
                "instance {} is already registered by a live process ({})",
                record.id,
                directory.display()
            )));
        }

        // A pruner that held the lock first has renamed the entry aside, so
        // the lock taken is on a file no longer at the entry's path: the
        // entry is published afresh rather than owned where no reader
        // looks (issue 819).

        if !is_at(&lock, &directory.join(LOCK)) {
            drop(lock);
            return Self::publish(registry, record);
        }

        write_record(&directory, &record, mode)?;
        Ok(Self::owned(registry, directory, mode, lock, record))
    }

    fn owned(registry: &Path, directory: PathBuf, mode: u32, lock: File, record: Record) -> Self {
        Self {
            registry: registry.to_path_buf(),
            directory,
            mode,
            _lock: lock,
            record: Mutex::new(record),
            keep: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The record as last written.
    pub fn record(&self) -> Record {
        self.record
            .lock()
            .expect("registry record poisoned")
            .clone()
    }

    /// Change the record and write it again, when the change changed it.
    pub fn update(&self, change: impl FnOnce(&mut Record)) {
        let mut record = self.record.lock().expect("registry record poisoned");
        let before = record.clone();
        change(&mut record);

        if *record == before {
            return;
        }

        if let Err(error) = write_record(&self.directory, &record, self.mode) {
            tracing::warn!(%error, entry = %self.directory.display(), "rewriting the registry record failed");
        }
    }

    /// Leave the entry in place when this process exits, its lock freed by
    /// the exit: what a daemon that detaches does, so its edge and hosts
    /// read as a detached instance's and the next daemon takes it over.
    pub fn keep(&self) {
        self.keep.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.keep.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let id = self.record.get_mut().map(|record| record.id);

        if let Ok(id) = id {
            let _ = remove_entry(&self.registry, &self.directory, id);
        }
    }
}

/// An entry as a reader finds it.
#[derive(Debug, Clone)]
pub struct Found {
    /// The record.
    pub record: Record,
    /// Whether its owner holds the lock.
    pub live: bool,
    /// The entry's directory.
    pub directory: PathBuf,
}

/// Every entry in `registry`, each with whether its owner is alive. What
/// vanishes while it is read, has no record, or has one that does not
/// parse is skipped, as is every name beginning with `.`.
pub fn read(registry: &Path) -> Vec<Found> {
    let Ok(entries) = std::fs::read_dir(registry) else {
        return Vec::new();
    };

    let mut found: Vec<Found> = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let directory = entry.path();
            let record: Record =
                serde_json::from_slice(&std::fs::read(directory.join(RECORD)).ok()?).ok()?;

            Some(Found {
                live: is_live(&directory),
                record,
                directory,
            })
        })
        .collect();

    found.sort_by_key(|found| (found.record.started_unix_ms, found.record.id));
    found
}

/// The live entries of `registry` alone.
pub fn live(registry: &Path) -> Vec<Record> {
    read(registry)
        .into_iter()
        .filter(|found| found.live)
        .map(|found| found.record)
        .collect()
}

/// Whether `id` has an entry in `registry` whose owner holds its lock.
pub fn entry_is_live(registry: &Path, id: InstanceId) -> bool {
    let directory = registry.join(id.to_string());
    directory.join(RECORD).exists() && is_live(&directory)
}

/// Whether the entry at `directory` has a live owner: its lock cannot be
/// shared. A lock that cannot be opened counts as live, since only its
/// owner could say otherwise.
fn is_live(directory: &Path) -> bool {
    let Ok(lock) = open_lock(directory, false, 0) else {
        return directory.join(LOCK).exists();
    };

    match fdpass::try_lock(lock.as_raw_fd(), false) {
        Ok(true) => {
            fdpass::unlock(lock.as_raw_fd());
            false
        }
        Ok(false) | Err(_) => true,
    }
}

/// Remove the entry of `id` when its owner is gone: take its lock, which
/// succeeds only then, and rename it aside and delete it while holding
/// it. A lock held for an instant, by a reader or a forked process, is
/// waited out as a takeover waits it out (issue 818). True when it was
/// removed; an entry whose lock stays held is never touched.
pub fn remove_stale(registry: &Path, id: InstanceId) -> std::io::Result<bool> {
    let directory = registry.join(id.to_string());

    let lock = match open_lock(&directory, false, 0) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };

    if !lock_exclusive(&lock)? {
        return Ok(false);
    }

    // The lock may be on a file renamed away while this waited for it, its
    // entry removed by its owner or another pruner, and the path may now
    // hold the owner's entry published afresh: only the entry whose lock
    // this holds is removed (issue 819).

    if !is_at(&lock, &directory.join(LOCK)) {
        return Ok(false);
    }

    remove_entry(registry, &directory, id)?;
    drop(lock);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the entry of `id` reads as stale within the moment a
    /// takeover or a prune waits (see [`LOCK_TRIES`]). Its lock can be
    /// held for an instant after its owner let it go, by a process another
    /// test forked before it ran its program (issue 818), so a single read
    /// is not enough.
    fn stale_soon(registry: &Path, id: InstanceId) -> bool {
        for _ in 0..LOCK_TRIES {
            if read(registry)
                .iter()
                .any(|found| found.record.id == id && !found.live)
            {
                return true;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        false
    }

    fn record(kind: Kind, port: u16) -> Record {
        let mut record = Record::new(
            InstanceId::generate(),
            kind,
            Endpoint::unix("/tmp/admin.sock"),
            PathBuf::from("/tmp/run"),
        );
        record.listeners.push(Listener {
            name: "http".to_owned(),
            configured: format!("127.0.0.1:{port}"),
            bound: Some(format!("127.0.0.1:{port}")),
            tls: false,
        });
        record
    }

    #[test]
    fn ids_are_written_as_hex_and_read_back() {
        let id = InstanceId::generate();
        let text = id.to_string();

        assert_eq!(text.len(), 32);
        assert_eq!(text.parse::<InstanceId>(), Ok(id));
        assert_eq!(id.short(), text[..8]);
        assert!("7f3a".parse::<InstanceId>().is_err());
        assert_ne!(InstanceId::generate(), id);
    }

    #[test]
    fn a_daemons_id_is_kept_across_starts() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("instance-id");
        let first = InstanceId::kept_in(&path).expect("made");

        assert_eq!(InstanceId::kept_in(&path).expect("read"), first);
    }

    #[test]
    fn an_entry_is_published_rewritten_and_removed_whole() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        let entry = Entry::publish(&registry, record(Kind::Run, 8000)).expect("published");
        let id = entry.record().id;

        let found = read(&registry);
        assert_eq!(found.len(), 1);
        assert!(found[0].live);
        assert_eq!(found[0].record.tag(), "run:8000");

        entry.update(|record| record.dashboard = Some("http://127.0.0.1:8001/".to_owned()));
        assert_eq!(
            live(&registry)[0].dashboard.as_deref(),
            Some("http://127.0.0.1:8001/")
        );

        // A live entry is never pruned.

        assert!(!remove_stale(&registry, id).expect("checked"));
        drop(entry);
        assert!(read(&registry).is_empty());
        assert_eq!(std::fs::read_dir(&registry).expect("listed").count(), 0);
    }

    #[test]
    fn a_stale_entry_held_for_a_moment_is_still_pruned() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        let entry = Entry::publish(&registry, record(Kind::Run, 8000)).expect("published");
        let id = entry.record().id;
        entry.keep();
        drop(entry);

        // A shared lock held for a moment, as a reader's test or a forked
        // child's copy holds one, is waited out rather than taken for an
        // owner (issue 818).

        let held = open_lock(&registry.join(id.to_string()), false, 0).expect("opened");
        assert!(fdpass::try_lock(held.as_raw_fd(), false).expect("locked"));

        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });

        assert!(remove_stale(&registry, id).expect("pruned"));
        releasing.join().expect("released");
        assert!(read(&registry).is_empty());
    }

    #[test]
    fn a_reader_skips_what_is_half_written_or_on_its_way_in_or_out() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        let _entry = Entry::publish(&registry, record(Kind::Run, 8000)).expect("published");

        for name in [".new-x", ".gone-y-z"] {
            std::fs::create_dir_all(registry.join(name)).expect("made");
        }

        let broken = registry.join(InstanceId::generate().to_string());
        std::fs::create_dir(&broken).expect("made");
        std::fs::write(broken.join(RECORD), b"{\"format\": 1, \"id\":").expect("written");

        let empty = registry.join(InstanceId::generate().to_string());
        std::fs::create_dir(&empty).expect("made");

        assert_eq!(read(&registry).len(), 1);
    }

    #[test]
    fn an_owner_gone_leaves_a_stale_entry_that_is_pruned_or_taken_over() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        let mine = record(Kind::Daemon, 8080);
        let id = mine.id;
        let entry = Entry::publish(&registry, mine.clone()).expect("published");

        // A daemon that detaches leaves its entry, its lock freed.

        entry.keep();
        drop(entry);
        assert_eq!(read(&registry).len(), 1);
        assert!(stale_soon(&registry, id));

        // The daemon started again takes its own entry over.

        let again = Entry::publish(&registry, mine.clone()).expect("taken over");
        assert!(read(&registry)[0].live);
        assert!(Entry::publish(&registry, mine).is_err());
        again.keep();
        drop(again);

        assert!(remove_stale(&registry, id).expect("pruned"));
        assert!(read(&registry).is_empty());
    }

    #[test]
    fn a_bound_address_names_the_instance_holding_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        let mut run = record(Kind::Run, 18_000);
        run.pid = std::process::id() + 1;
        run.app = Some("shop".to_owned());
        run.project = Some(PathBuf::from("/srv/shop"));
        run.dashboard = Some("http://127.0.0.1:18001/".to_owned());
        let _entry = Entry::publish(&registry, run).expect("published");
        let registries = [registry];

        let holder = holder_of(&registries, "127.0.0.1:18000".parse().expect("address"))
            .expect("held by the run");
        assert_eq!(holder.what, "http");
        assert!(
            holder
                .describe()
                .starts_with("the development run of shop in /srv/shop (pid "),
            "{}",
            holder.describe()
        );
        assert!(
            holder
                .describe()
                .ends_with(", dashboard http://127.0.0.1:18001/)")
        );
        assert_eq!(holder.stop_hint(), "stop it");

        let dashboard = holder_of(&registries, "0.0.0.0:18001".parse().expect("address"));
        assert_eq!(
            dashboard.map(|holder| holder.what).as_deref(),
            Some("dashboard")
        );
        assert!(holder_of(&registries, "127.0.0.1:18002".parse().expect("address")).is_none());
    }

    #[test]
    fn addresses_clash_on_one_port_when_they_overlap() {
        assert!(clashes("127.0.0.1:9000", "127.0.0.1:9000"));
        assert!(clashes("0.0.0.0:9000", "127.0.0.1:9000"));
        assert!(clashes("127.0.0.1:9000", "0.0.0.0:9000"));
        assert!(!clashes("127.0.0.1:9000", "127.0.0.2:9000"));
        assert!(!clashes("127.0.0.1:9000", "127.0.0.1:9001"));
        assert!(!clashes("[::]:9000", "127.0.0.1:9000"));
        assert!(!clashes("127.0.0.1:0", "127.0.0.1:0"));
    }

    #[test]
    fn publishing_and_pruning_at_once_never_removes_a_live_entry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        prepare(&registry).expect("made");

        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let registry = registry.clone();

                std::thread::spawn(move || {
                    // One instance per worker, so that an entry it left
                    // stale is taken over again, as a restarted daemon
                    // takes over its own, while the others prune it.

                    let mine = record(Kind::Daemon, 9000 + worker);

                    for round in 0..25 {
                        let entry =
                            Entry::publish(&registry, mine.clone()).unwrap_or_else(|error| {
                                panic!("worker {worker} round {round}: {error}")
                            });

                        // Every other process prunes what it reads as
                        // stale, as instance prune does, while this one's
                        // entry is live.

                        for found in read(&registry).into_iter().filter(|found| !found.live) {
                            let _ = remove_stale(&registry, found.record.id);
                        }

                        assert!(
                            read(&registry)
                                .iter()
                                .any(|found| found.record.id == mine.id && found.live),
                            "worker {worker} round {round} lost its live entry"
                        );

                        if round % 2 == 0 {
                            entry.keep();
                        }

                        drop(entry);
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("no worker failed");
        }

        for found in read(&registry) {
            assert!(stale_soon(&registry, found.record.id));
            assert!(remove_stale(&registry, found.record.id).expect("pruned"));
        }

        assert!(read(&registry).is_empty());
    }
}
