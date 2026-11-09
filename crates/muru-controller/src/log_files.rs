//! The log directory as logs and their archives, and the cutting of a
//! live log, for the `muructl logs` commands on the files (design
//! specification section 11.3). The daemon and a muructl working with no
//! daemon running share this, so both see the same files and cut them
//! the same way.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
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
pub fn report(directory: &Path, existing: Option<&Existing>) -> io::Result<pb::ListLogsResponse> {
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

/// The logs a request names, which must exist. No target at all is
/// every log, as `all` is.
pub fn select<'a>(logs: &'a [LogFile], target: &pb::LogTarget) -> Result<Vec<&'a LogFile>, String> {
    if target.access {
        return logs
            .iter()
            .find(|log| log.kind == Kind::Access && log.present)
            .map(|log| vec![log])
            .ok_or_else(|| "there is no access log".to_owned());
    }

    if !target.host.is_empty() {
        return logs
            .iter()
            .find(|log| log.present && log.hosts.contains(&target.host))
            .map(|log| vec![log])
            .ok_or_else(|| {
                format!(
                    "no log file for virtual host {}: its records are in access.log unless it \
                     names an access_log_file",
                    target.host
                )
            });
    }

    if !target.app.is_empty() {
        return logs
            .iter()
            .find(|log| {
                log.kind == Kind::Release
                    && log.present
                    && log.app == target.app
                    && log.release == target.release
            })
            .map(|log| vec![log])
            .ok_or_else(|| format!("no log for {}@{}", target.app, target.release));
    }

    Ok(logs.iter().filter(|log| log.present).collect())
}

/// The start of what is kept, when `keep` bytes of a file's newest
/// content are kept: just after a line ends, at or after `size - keep`.
/// With no line ending there to cut at, nothing is kept.
pub fn cut_point(path: &Path, size: u64, keep: u64) -> io::Result<u64> {
    if keep >= size {
        return Ok(0);
    }

    if keep == 0 {
        return Ok(size);
    }

    let start = size - keep;
    let mut file = std::fs::File::open(path)?;

    // The byte before the window decides whether the window opens on a
    // line start, so the search begins there.

    file.seek(SeekFrom::Start(start - 1))?;
    let mut position = start - 1;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;

        if read == 0 {
            return Ok(size);
        }

        if let Some(at) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            return Ok((position + at as u64 + 1).min(size));
        }

        position += read as u64;
    }
}

/// What a truncate of the named logs would do, or did.
fn truncated(log: &LogFile, cut: u64, in_place: bool) -> pb::TruncatedLog {
    pb::TruncatedLog {
        name: log.name.clone(),
        size: log.size,
        kept: log.size - cut,
        freed: cut,
        in_place,
        ..Default::default()
    }
}

/// What truncating the named logs to `keep` bytes each would free.
pub fn plan_truncate(
    directory: &Path,
    existing: Option<&Existing>,
    target: &pb::LogTarget,
    keep: u64,
) -> Result<pb::TruncateLogsResponse, String> {
    let logs =
        list(directory, existing).map_err(|error| format!("reading the log directory: {error}"))?;
    let selected = select(&logs, target)?;
    let mut planned = Vec::new();

    for log in selected {
        let cut = cut_point(&directory.join(&log.name), log.size, keep)
            .map_err(|error| format!("reading {}: {error}", log.name))?;
        planned.push(truncated(log, cut, false));
    }

    Ok(pb::TruncateLogsResponse {
        freed: planned.iter().map(|log| log.freed).sum(),
        logs: planned,
        dry_run: true,
    })
}

/// A log file being cut, by the plan the daemon carries out.
#[derive(Debug, Clone)]
pub struct Target {
    /// Its path.
    pub path: PathBuf,
    /// Its name, relative to the log directory.
    pub name: String,
    /// How many of its bytes to keep.
    pub keep: u64,
    /// Whether its writers cannot be told to reopen it.
    pub in_place: bool,
}

/// A file replaced by a shorter copy, whose old file is held open so
/// that what writers added before they reopened can be carried over.
#[derive(Debug)]
pub struct Swapped {
    path: PathBuf,
    name: String,
    old: std::fs::File,
    copied_to: u64,
}

impl Swapped {
    /// The size of the old file now.
    fn len(&self) -> io::Result<u64> {
        Ok(self.old.metadata()?.len())
    }
}

/// How long the old file's size must stay still for its writers to have
/// moved on, and the longest to wait for that.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Where the kept part of a log is written before it replaces the log.
fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".truncating");
    PathBuf::from(name)
}

/// Write the part of a file from `cut` to its end to the temporary file,
/// with the file's mode and owner, returning the position copied to.
fn write_kept(path: &Path, cut: u64) -> io::Result<(PathBuf, std::fs::File, u64)> {
    let temporary = temporary_path(path);
    let mut source = std::fs::File::open(path)?;
    let metadata = source.metadata()?;

    let copied = (|| -> io::Result<u64> {
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        crate::rotation::match_mode_and_owner(&output, &temporary, &metadata)?;
        source.seek(SeekFrom::Start(cut))?;
        let copied = io::copy(&mut source, &mut output)?;
        output.sync_all()?;
        Ok(cut + copied)
    })();

    match copied {
        Ok(copied) => Ok((temporary, source, copied)),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Replace a log with a copy holding only what is kept. The old file is
/// held open until `finish_swap` has carried over what writers added
/// while they moved to the new file.
pub fn swap_in(target: &Target) -> io::Result<Option<Swapped>> {
    let size = std::fs::metadata(&target.path)?.len();
    let cut = cut_point(&target.path, size, target.keep)?;

    if cut == 0 {
        return Ok(None);
    }

    let (temporary, old, copied_to) = write_kept(&target.path, cut)?;

    if let Err(error) = std::fs::rename(&temporary, &target.path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    Ok(Some(Swapped {
        path: target.path.clone(),
        name: target.name.clone(),
        old,
        copied_to,
    }))
}

/// Wait for writers to have moved off the old files, then add what they
/// wrote there during the swap to the new ones. Returns the bytes added
/// to each, by the log's name.
pub async fn finish_swaps(swapped: Vec<Swapped>) -> Vec<(String, io::Result<u64>)> {
    let started = tokio::time::Instant::now();
    let mut sizes: Vec<Option<u64>> = swapped.iter().map(|one| one.len().ok()).collect();
    let mut still_since = tokio::time::Instant::now();

    while started.elapsed() < SETTLE_LIMIT {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let now: Vec<Option<u64>> = swapped.iter().map(|one| one.len().ok()).collect();

        if now == sizes {
            if still_since.elapsed() >= SETTLE {
                break;
            }
        } else {
            sizes = now;
            still_since = tokio::time::Instant::now();
        }
    }

    tokio::task::spawn_blocking(move || {
        swapped
            .into_iter()
            .map(|one| {
                let name = one.name.clone();
                (name, append_tail(one))
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Add to the new file what the old one gained after the copy, as one
/// write, so a concurrent line does not land in the middle of it.
fn append_tail(mut swapped: Swapped) -> io::Result<u64> {
    let mut tail = Vec::new();
    swapped.old.seek(SeekFrom::Start(swapped.copied_to))?;
    swapped.old.read_to_end(&mut tail)?;

    if tail.is_empty() {
        return Ok(0);
    }

    let mut new = std::fs::OpenOptions::new()
        .append(true)
        .open(&swapped.path)?;
    new.write_all(&tail)?;
    Ok(tail.len() as u64)
}

/// Cut a log whose writers cannot reopen it, in place: the kept part is
/// written aside, the file truncated and the kept part appended. Lines
/// written while the kept part is copied are lost. `keep` of 0 truncates
/// the file and needs no free disk space.
pub fn cut_in_place(target: &Target) -> io::Result<u64> {
    let size = std::fs::metadata(&target.path)?.len();
    let cut = cut_point(&target.path, size, target.keep)?;

    if cut == 0 {
        return Ok(0);
    }

    if target.keep == 0 {
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&target.path)?;
        return Ok(size);
    }

    let (temporary, _, _) = write_kept(&target.path, cut)?;
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&target.path)?;
    let mut kept = std::fs::File::open(&temporary)?;
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&target.path)?;
    io::copy(&mut kept, &mut appended)?;
    std::fs::remove_file(&temporary)?;
    Ok(cut)
}

/// Truncate the named logs directly, with no daemon running. Everything
/// is cut in place, since an edge or a host left running by a detached
/// daemon may still have the files open.
pub fn truncate_directly(
    directory: &Path,
    existing: Option<&Existing>,
    target: &pb::LogTarget,
    keep: u64,
) -> Result<pb::TruncateLogsResponse, String> {
    let logs =
        list(directory, existing).map_err(|error| format!("reading the log directory: {error}"))?;
    let selected = select(&logs, target)?;
    let mut done = Vec::new();

    for log in selected {
        let one = Target {
            path: directory.join(&log.name),
            name: log.name.clone(),
            keep,
            in_place: true,
        };
        let cut = cut_point(&one.path, log.size, keep).unwrap_or(0);
        let mut result = truncated(log, cut, keep > 0);

        if let Err(error) = cut_in_place(&one) {
            result.error = error.to_string();
            result.freed = 0;
            result.kept = log.size;
        }

        done.push(result);
    }

    Ok(pb::TruncateLogsResponse {
        freed: done.iter().map(|log| log.freed).sum(),
        logs: done,
        dry_run: false,
    })
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
    fn a_cut_is_at_the_start_of_a_line_and_keeps_at_most_the_size() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("a.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let size = std::fs::metadata(&path).unwrap().len();

        assert_eq!(cut_point(&path, size, 0).unwrap(), size);
        assert_eq!(cut_point(&path, size, size).unwrap(), 0);
        assert_eq!(cut_point(&path, size, 100).unwrap(), 0);
        assert_eq!(cut_point(&path, size, 6).unwrap(), 13, "third is six bytes");
        assert_eq!(
            cut_point(&path, size, 7).unwrap(),
            13,
            "starts inside second"
        );
        assert_eq!(
            cut_point(&path, size, 5).unwrap(),
            size,
            "inside the last line"
        );
    }

    #[test]
    fn cutting_in_place_keeps_whole_newest_lines() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("a.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let target = Target {
            path: path.clone(),
            name: "a.log".to_owned(),
            keep: 6,
            in_place: true,
        };

        assert_eq!(cut_in_place(&target).unwrap(), 13);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third\n");

        let empty = Target { keep: 0, ..target };
        assert_eq!(cut_in_place(&empty).unwrap(), 6);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[tokio::test]
    async fn a_swap_carries_over_what_a_writer_added_before_reopening() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("a.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let target = Target {
            path: path.clone(),
            name: "a.log".to_owned(),
            keep: 6,
            in_place: false,
        };

        // A writer holds the file open across the swap, as the edge and
        // a host do until they are told to reopen.

        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let swapped = swap_in(&target).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third\n");

        writeln!(writer, "late").unwrap();
        let added = finish_swaps(vec![swapped]).await;
        assert_eq!(added[0].1.as_ref().unwrap(), &5);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third\nlate\n");
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
