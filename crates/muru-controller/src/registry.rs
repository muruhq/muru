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

/// The caller's own registry: `registry` in the default runtime location
/// for whoever runs, whatever `--run-dir` or `MURU_RUN_DIR` says, so it
/// is always in the same place. Root's is the system registry.
pub fn user_registry() -> PathBuf {
    crate::locations::default_runtime().join("registry")
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
        std::fs::write(path, format!("{id}\n"))?;
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
}

impl Kind {
    /// The kind as the record writes it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Run => "run",
            Self::Serve => "serve",
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
    /// Its run directory: a daemon's or a serve's, or a run's own
    /// directory under the run directory it was started with.
    pub run_dir: PathBuf,
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
    /// which is known before anything starts, `serve` for a serve, or the
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

/// How many times a lock held only for a moment, by a reader testing it,
/// is tried again before it counts as held by an owner.
const LOCK_TRIES: u32 = 20;

/// Take an exclusive lock, trying again for a moment, since a reader's
/// test holds a shared lock for an instant.
fn lock_exclusive(file: &File) -> std::io::Result<bool> {
    for _ in 0..LOCK_TRIES {
        if fdpass::try_lock(file.as_raw_fd(), true)? {
            return Ok(true);
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    Ok(false)
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

            // An entry with no lock file is a broken one: it is put aside
            // and published afresh.
            Err(_) => {
                remove_entry(registry, &directory, record.id)?;
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
/// it. True when it was removed; an entry whose lock is held is never
/// touched.
pub fn remove_stale(registry: &Path, id: InstanceId) -> std::io::Result<bool> {
    let directory = registry.join(id.to_string());

    let lock = match open_lock(&directory, false, 0) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };

    if !fdpass::try_lock(lock.as_raw_fd(), true)? {
        return Ok(false);
    }

    remove_entry(registry, &directory, id)?;
    drop(lock);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let found = read(&registry);
        assert_eq!(found.len(), 1);
        assert!(!found[0].live);

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
    fn publishing_and_pruning_at_once_never_removes_a_live_entry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry = directory.path().join("registry");
        prepare(&registry).expect("made");

        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let registry = registry.clone();

                std::thread::spawn(move || {
                    for round in 0..25 {
                        let entry = Entry::publish(&registry, record(Kind::Run, 9000 + worker))
                            .expect("published");
                        let id = entry.record().id;

                        // Every other process prunes what it can see while
                        // this one's entry is live.

                        for found in read(&registry) {
                            let _ = remove_stale(&registry, found.record.id);
                        }

                        assert!(
                            read(&registry)
                                .iter()
                                .any(|found| found.record.id == id && found.live),
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
            assert!(!found.live);
            assert!(remove_stale(&registry, found.record.id).expect("pruned"));
        }

        assert!(read(&registry).is_empty());
    }
}
