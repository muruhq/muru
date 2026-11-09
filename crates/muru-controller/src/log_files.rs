//! The log directory as logs and their archives, for `muructl logs list`
//! (design specification section 11.3). The daemon and a muructl working
//! with no daemon running share this, so both see the same files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use muru_proto::admin::v1 as pb;

use crate::store::{Store, StoreError};

/// What the state store says exists, to tell which logs are orphaned.
#[derive(Debug, Default, Clone)]
pub struct Existing {
    releases: HashSet<(String, String)>,
    hosts: HashMap<String, Vec<String>>,
}

impl Existing {
    /// The releases and the virtual hosts' files the store records.
    pub fn read(store: &Store) -> Result<Self, StoreError> {
        let releases = store
            .releases(None)?
            .into_iter()
            .map(|release| (release.app, release.id))
            .collect();
        let mut hosts: HashMap<String, Vec<String>> = HashMap::new();

        for (name, host) in store.hosts()? {
            if let Some(file) = host.access_log_file {
                hosts.entry(file).or_default().push(name);
            }
        }

        Ok(Self { releases, hosts })
    }
}

/// One archive of a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// Its path relative to the log directory.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// When it was last changed.
    pub modified: SystemTime,
}

/// What writes a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The access log.
    Access,
    /// The edge's own log.
    Edge,
    /// The daemon's own log.
    Daemon,
    /// A file virtual hosts name for their records.
    Host,
    /// A release's output.
    Release,
}

impl Kind {
    /// The kind as the admin API names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Access => "access",
            Kind::Edge => "edge",
            Kind::Daemon => "daemon",
            Kind::Host => "host",
            Kind::Release => "release",
        }
    }
}

/// A log and its archives. The log itself may be gone with its archives
/// left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    /// Its path relative to the log directory.
    pub name: String,
    /// What writes it.
    pub kind: Kind,
    /// The application of a release's log.
    pub app: String,
    /// The release of a release's log.
    pub release: String,
    /// The virtual hosts that name this file.
    pub hosts: Vec<String>,
    /// Its size in bytes, 0 when it is gone.
    pub size: u64,
    /// Whether the file exists.
    pub present: bool,
    /// Its archives, oldest first.
    pub archives: Vec<Archive>,
    /// Its release or host no longer exists.
    pub orphaned: bool,
}

impl LogFile {
    /// The bytes held by its archives.
    pub fn archives_size(&self) -> u64 {
        self.archives.iter().map(|archive| archive.size).sum()
    }

    /// The bytes held by the log and its archives.
    pub fn total(&self) -> u64 {
        self.size + self.archives_size()
    }
}

/// The name of the log a file is an archive of, if it is one: the name
/// less a stamp or a number, and the compression's suffix after it.
pub fn archive_base(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".zst")
        .or_else(|| name.strip_suffix(".gz"))
        .unwrap_or(name);
    let (base, tail) = stem.rsplit_once('.')?;
    let archived = tail.starts_with(|c: char| c.is_ascii_digit())
        && tail.chars().all(|c| c.is_ascii_digit() || c == '-');

    (archived && !base.is_empty()).then_some(base)
}

/// Every log in the directory, by name. A file is taken for a log only
/// when its place and its name say it is one, so anything else put in
/// the directory is left out. With no record of what exists nothing is
/// flagged as orphaned.
pub fn list(directory: &Path, existing: Option<&Existing>) -> io::Result<Vec<LogFile>> {
    let mut logs: BTreeMap<String, LogFile> = BTreeMap::new();

    for (name, metadata) in regular_files(directory)? {
        add(&mut logs, &name, &metadata, existing, root_log);
    }

    let apps = directory.join("apps");

    for app in subdirectories(&apps)? {
        let app_directory = apps.join(&app);

        for (name, metadata) in regular_files(&app_directory)? {
            let qualified = format!("apps/{app}/{name}");
            let classify =
                |base: &str, existing: Option<&Existing>| release_log(&app, base, existing);
            add(&mut logs, &qualified, &metadata, existing, classify);
        }
    }

    Ok(logs.into_values().collect())
}

/// The regular files directly in a directory, with their metadata, none
/// for a directory that is absent. A symbolic link is not followed.
fn regular_files(directory: &Path) -> io::Result<Vec<(String, std::fs::Metadata)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut files = Vec::new();

    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        if metadata.file_type().is_file() {
            files.push((name, metadata));
        }
    }

    Ok(files)
}

/// The names of the directories directly in a directory.
fn subdirectories(directory: &Path) -> io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    Ok(entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect())
}

/// What a log is, from its name alone.
struct Classified {
    kind: Kind,
    app: String,
    release: String,
    hosts: Vec<String>,
    orphaned: bool,
}

/// A file in the log directory's top level: the three files the daemon's
/// processes write, a file virtual hosts name, or a `.log` file that was
/// one and whose hosts have gone.
fn root_log(base: &str, existing: Option<&Existing>) -> Option<Classified> {
    let fixed = |kind| Classified {
        kind,
        app: String::new(),
        release: String::new(),
        hosts: Vec::new(),
        orphaned: false,
    };

    match base {
        "access.log" => return Some(fixed(Kind::Access)),
        "edge.log" => return Some(fixed(Kind::Edge)),
        "daemon.log" => return Some(fixed(Kind::Daemon)),
        _ => {}
    }

    let mut classified = fixed(Kind::Host);

    if let Some(hosts) = existing.and_then(|existing| existing.hosts.get(base)) {
        classified.hosts = hosts.clone();
        return Some(classified);
    }

    classified.orphaned = existing.is_some();
    base.ends_with(".log").then_some(classified)
}

/// A file in `apps/APP`: a release's log, orphaned when the store has no
/// such release.
fn release_log(app: &str, base: &str, existing: Option<&Existing>) -> Option<Classified> {
    let release = base.strip_suffix(".log")?;

    Some(Classified {
        kind: Kind::Release,
        app: app.to_owned(),
        release: release.to_owned(),
        hosts: Vec::new(),
        orphaned: existing.is_some_and(|existing| {
            !existing
                .releases
                .contains(&(app.to_owned(), release.to_owned()))
        }),
    })
}

/// Put a file into its log, as the log itself or one of its archives.
/// `name` is relative to the log directory.
fn add(
    logs: &mut BTreeMap<String, LogFile>,
    name: &str,
    metadata: &std::fs::Metadata,
    existing: Option<&Existing>,
    classify: impl Fn(&str, Option<&Existing>) -> Option<Classified>,
) {
    let (directory, file) = name
        .rsplit_once('/')
        .map_or((None, name), |(directory, file)| (Some(directory), file));

    let archived = archive_base(file);
    let base = archived.unwrap_or(file);

    let Some(classified) = classify(base, existing) else {
        return;
    };

    let log_name = directory.map_or_else(
        || base.to_owned(),
        |directory| format!("{directory}/{base}"),
    );
    let log = logs.entry(log_name.clone()).or_insert_with(|| LogFile {
        name: log_name,
        kind: classified.kind,
        app: classified.app,
        release: classified.release,
        hosts: classified.hosts,
        size: 0,
        present: false,
        archives: Vec::new(),
        orphaned: classified.orphaned,
    });

    if archived.is_some() {
        log.archives.push(Archive {
            name: name.to_owned(),
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
        });
        log.archives.sort_by(|a, b| a.name.cmp(&b.name));
    } else {
        log.size = metadata.len();
        log.present = true;
    }
}

/// What `ListLogs` answers: every log in the directory, with what the
/// store says about each.
pub fn report(
    directory: &PathBuf,
    existing: Option<&Existing>,
) -> io::Result<pb::ListLogsResponse> {
    let logs = list(directory, existing)?;
    let total = logs.iter().map(LogFile::total).sum();

    Ok(pb::ListLogsResponse {
        directory: directory.display().to_string(),
        total,
        orphans_known: existing.is_some(),
        logs: logs.iter().map(log_info).collect(),
    })
}

/// A log as the admin API gives it.
fn log_info(log: &LogFile) -> pb::LogInfo {
    pb::LogInfo {
        name: log.name.clone(),
        kind: log.kind.as_str().to_owned(),
        app: log.app.clone(),
        release: log.release.clone(),
        hosts: log.hosts.clone(),
        size: log.size,
        present: log.present,
        archives: log
            .archives
            .iter()
            .map(|archive| pb::LogArchive {
                name: archive.name.clone(),
                size: archive.size,
                modified_unix_ms: archive
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as u64),
            })
            .collect(),
        archives_size: log.archives_size(),
        total: log.total(),
        orphaned: log.orphaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(directory: &Path, name: &str, bytes: usize) {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x".repeat(bytes)).unwrap();
    }

    fn existing() -> Existing {
        Existing {
            releases: HashSet::from([("shop".to_owned(), "r1".to_owned())]),
            hosts: HashMap::from([("own.log".to_owned(), vec!["shop.example".to_owned()])]),
        }
    }

    #[test]
    fn archive_names_give_the_log_they_belong_to() {
        assert_eq!(
            archive_base("access.log.20261025-000000"),
            Some("access.log")
        );
        assert_eq!(
            archive_base("access.log.20261025-000000.zst"),
            Some("access.log")
        );
        assert_eq!(archive_base("access.log.1.gz"), Some("access.log"));
        assert_eq!(archive_base("access.log"), None);
        assert_eq!(archive_base("shop.2.log"), None);
        assert_eq!(archive_base("access.log.truncating"), None);
    }

    #[test]
    fn a_directory_lists_as_logs_with_their_archives_and_orphans() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();

        write(directory, "access.log", 10);
        write(directory, "access.log.20261025-000000.zst", 3);
        write(directory, "access.log.20261024-000000.zst", 2);
        write(directory, "edge.log", 1);
        write(directory, "own.log", 4);
        write(directory, "gone.log", 5);
        write(directory, "notes.txt", 7);
        write(directory, "apps/shop/r1.log", 6);
        write(directory, "apps/shop/r2.log.20261025-000000", 8);
        write(directory, "apps/shop/other", 9);

        let logs = list(directory, Some(&existing())).unwrap();
        let names: Vec<&str> = logs.iter().map(|log| log.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "access.log",
                "apps/shop/r1.log",
                "apps/shop/r2.log",
                "edge.log",
                "gone.log",
                "own.log"
            ]
        );

        let access = &logs[0];
        assert_eq!(
            (access.size, access.archives_size(), access.total()),
            (10, 5, 15)
        );
        assert_eq!(access.archives[0].name, "access.log.20261024-000000.zst");
        assert!(!access.orphaned);

        let orphan = &logs[2];
        assert!(orphan.orphaned && !orphan.present && orphan.size == 0);
        assert_eq!(orphan.archives.len(), 1);
        assert_eq!(
            (orphan.app.as_str(), orphan.release.as_str()),
            ("shop", "r2")
        );

        assert!(logs[4].orphaned, "a .log no host names");
        assert_eq!(logs[5].hosts, ["shop.example"]);
        assert!(!logs[5].orphaned);

        // With no record of what exists, nothing is called orphaned.

        let logs = list(directory, None).unwrap();
        assert!(logs.iter().all(|log| !log.orphaned));
    }

    #[test]
    fn a_missing_directory_has_no_logs() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(
            list(&temporary.path().join("none"), None)
                .unwrap()
                .is_empty()
        );
    }
}
