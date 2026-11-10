//! What can be re-created: the user cache and the daemon's re-creatable
//! data, and what of it is in use (design specification sections 8.2,
//! 8.3 and 13.8). The same functions work on the files for the daemon and
//! for `muructl cache` with no daemon running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use muru_proto::admin::v1 as pb;

use crate::disk::Measure;
use crate::locations::Locations;
use crate::model::{Release, ReleaseState};
use crate::provision::Provisioning;

/// A runtime library directory, of one version and interpreter tag.
pub const RUNTIME: &str = "runtime";
/// The development certificates.
pub const CERTS: &str = "certs";
/// uv's build cache, of one user.
pub const UV_CACHE: &str = "uv_cache";
/// A Python uv downloaded for the daemon.
pub const PYTHON: &str = "python";

/// One thing an area holds.
fn entry(kind: &str, name: String, path: &Path) -> pb::CacheEntry {
    pb::CacheEntry {
        kind: kind.to_owned(),
        name,
        path: path.display().to_string(),
        ..Default::default()
    }
}

/// The names in a directory, sorted, leaving out the hidden ones uv keeps
/// for itself.
fn names(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Each runtime library directory under `root`/runtime, by version and tag.
fn runtime_entries(root: &Path) -> Vec<pb::CacheEntry> {
    let runtime = root.join("runtime");
    let mut found = Vec::new();

    for version in names(&runtime) {
        let directory = runtime.join(&version);

        for tag in names(&directory) {
            let path = directory.join(&tag);
            let mut item = entry(RUNTIME, format!("{version}/{tag}"), &path);
            item.version = version.clone();
            item.tag = tag;
            found.push(item);
        }
    }

    found
}

/// The directory `name` under `root` as one entry, when it exists.
fn whole(root: &Path, name: &str, kind: &str) -> Option<pb::CacheEntry> {
    let path = root.join(name);

    path.symlink_metadata()
        .ok()
        .map(|_| entry(kind, name.to_owned(), &path))
}

/// Each child of the directory `name` under `root` as an entry.
fn children(root: &Path, name: &str, kind: &str) -> Vec<pb::CacheEntry> {
    let directory = root.join(name);

    names(&directory)
        .into_iter()
        .map(|child| {
            let path = directory.join(&child);
            entry(kind, format!("{name}/{child}"), &path)
        })
        .collect()
}

/// What the user cache holds: the runtime library by version and tag and
/// the development certificates.
pub fn user_entries(root: &Path) -> Vec<pb::CacheEntry> {
    let mut entries = runtime_entries(root);
    entries.extend(whole(root, "certs", CERTS));
    entries
}

/// What a daemon's state directory holds that can be re-created: the
/// runtime library by version and tag, uv's build cache for each user
/// and the Pythons it downloaded.
pub fn state_entries(root: &Path) -> Vec<pb::CacheEntry> {
    let mut entries = runtime_entries(root);
    entries.extend(children(root, "uv-cache", UV_CACHE));
    entries.extend(children(root, "python", PYTHON));
    entries
}

/// What uses an area's entries now.
#[derive(Debug, Default)]
pub struct InUse {
    /// Runtime library directories, canonical, each with what uses it.
    pub runtime: BTreeMap<PathBuf, String>,
    /// Interpreters found under the area, canonical, each with what uses
    /// it.
    pub pythons: BTreeMap<PathBuf, String>,
    /// A build or start under way, which the current runtime library,
    /// the uv cache and the Pythons must stay for.
    pub building: Option<String>,
}

/// The canonical path, or the path as it is when it cannot be resolved.
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What a running host has in the runtime directory: its pid and the
/// runtime library directory it runs with.
#[derive(serde::Deserialize)]
struct HostRuntime {
    pid: u32,
    #[serde(default)]
    runtime: Option<PathBuf>,
}

/// What the daemon's releases use of its state directory: the runtime
/// libraries of the hosts still running, which a detached daemon's hosts
/// count among, the Pythons any release was built with or names, and
/// whether a release is being built or started.
pub fn daemon_in_use(locations: &Locations, releases: &[Release]) -> InUse {
    let mut in_use = InUse::default();
    let apps_dir = locations.run.path.join("apps");
    let python_root = real(&locations.state.path.join("python"));

    for app in names(&apps_dir) {
        for id in names(&apps_dir.join(&app)) {
            let record = apps_dir.join(&app).join(&id).join("host.toml");
            let Some(host) = std::fs::read_to_string(record)
                .ok()
                .and_then(|text| toml::from_str::<HostRuntime>(&text).ok())
            else {
                continue;
            };

            if let Some(runtime) = host.runtime
                && crate::host::alive(host.pid)
            {
                in_use
                    .runtime
                    .insert(real(&runtime), format!("the release {app}@{id}"));
            }
        }
    }

    for release in releases {
        let subject = format!("{}@{}", release.app, release.id);

        for python in [
            release.spec.python.clone(),
            Provisioning::environment_dir(&locations.state.path, &release.app, &release.id)
                .join("bin/python"),
        ] {
            let resolved = real(&python);

            if resolved.starts_with(&python_root) {
                in_use
                    .pythons
                    .entry(resolved)
                    .or_insert_with(|| format!("the release {subject}"));
            }
        }

        if matches!(
            release.state,
            ReleaseState::Pending
                | ReleaseState::Building
                | ReleaseState::Starting
                | ReleaseState::Warming
        ) && in_use.building.is_none()
        {
            in_use.building = Some(format!("the release {subject} being built or started"));
        }
    }

    in_use
}

/// Say what uses each entry. `own_version` is the version the area's
/// owner runs now, whose runtime library a build under way would need.
pub fn mark_in_use(entries: &mut [pb::CacheEntry], in_use: &InUse, own_version: &str) {
    for item in entries.iter_mut() {
        let path = real(Path::new(&item.path));

        item.in_use = match item.kind.as_str() {
            RUNTIME => in_use.runtime.get(&path).cloned().or_else(|| {
                (item.version == own_version)
                    .then(|| in_use.building.clone())
                    .flatten()
            }),
            UV_CACHE => in_use.building.clone(),
            PYTHON => in_use
                .pythons
                .iter()
                .find(|(interpreter, _)| interpreter.starts_with(&path))
                .map(|(_, what)| what.clone())
                .or_else(|| in_use.building.clone()),
            _ => None,
        }
        .unwrap_or_default();
    }
}

/// Measure each entry and return the report on an area.
pub fn report(directory: &Path, mut entries: Vec<pb::CacheEntry>) -> pb::ListCacheResponse {
    let mut together = Measure::default();

    for item in &mut entries {
        let mut measure = Measure::default();
        measure.add(Path::new(&item.path));
        item.size = measure.total().used;
        together.merge(&measure);
    }

    pb::ListCacheResponse {
        directory: directory.display().to_string(),
        total: together.total().used,
        entries,
    }
}

/// The report on a daemon's state directory, marking what its releases
/// use.
pub fn state_report(locations: &Locations, releases: &[Release]) -> pb::ListCacheResponse {
    let root = &locations.state.path;
    let mut entries = state_entries(root);
    let own = muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION"));

    mark_in_use(&mut entries, &daemon_in_use(locations, releases), &own);
    report(root, entries)
}

/// What `prune` removes.
#[derive(Debug, Default, Clone)]
pub struct PruneRules {
    /// The version the area's owner runs now, whose runtime library stays.
    pub own_version: String,
    /// Only runtime libraries of this version, when set.
    pub version: Option<String>,
    /// Only runtime libraries of this interpreter tag, when set.
    pub tag: Option<String>,
}

/// Split entries into what would be removed and what is left, because
/// it is in use. `chosen` says whether an entry would otherwise go.
fn choose(
    directory: &str,
    entries: Vec<pb::CacheEntry>,
    chosen: impl Fn(&pb::CacheEntry) -> bool,
    dry_run: bool,
) -> pb::RemoveCacheResponse {
    let mut answer = pb::RemoveCacheResponse {
        directory: directory.to_owned(),
        dry_run,
        ..Default::default()
    };
    let mut together = Measure::default();

    for mut item in entries {
        if !chosen(&item) {
            continue;
        }

        if !item.in_use.is_empty() {
            answer.left.push(item);
            continue;
        }

        let mut measure = Measure::default();
        measure.add(Path::new(&item.path));
        item.freed = measure.total().freed;
        together.merge(&measure);
        answer.removed.push(item);
    }

    answer.freed = together.total().freed;
    answer
}

/// What a prune removes of a report: runtime libraries of other versions,
/// or of the version or tag named, and, when neither is named, downloaded
/// Pythons nothing uses. What is in use of those named is left and
/// reported; a Python in use is simply not stale.
pub fn plan_prune(
    report: pb::ListCacheResponse,
    rules: &PruneRules,
    dry_run: bool,
) -> pb::RemoveCacheResponse {
    let named = rules.version.is_some() || rules.tag.is_some();
    let entries: Vec<pb::CacheEntry> = report
        .entries
        .into_iter()
        .filter(|item| named || item.kind != PYTHON || item.in_use.is_empty())
        .collect();

    choose(
        &report.directory,
        entries,
        |item| match item.kind.as_str() {
            RUNTIME if named => {
                rules
                    .version
                    .as_ref()
                    .is_none_or(|version| *version == item.version)
                    && rules.tag.as_ref().is_none_or(|tag| *tag == item.tag)
            }
            RUNTIME => item.version != rules.own_version,
            PYTHON => !named,
            _ => false,
        },
        dry_run,
    )
}

/// What a clear removes of a report: everything, except the development
/// certificates unless `certs`, since a browser or system may trust them.
pub fn plan_clear(
    report: pb::ListCacheResponse,
    certs: bool,
    dry_run: bool,
) -> pb::RemoveCacheResponse {
    choose(
        &report.directory,
        report.entries,
        |item| item.kind != CERTS || certs,
        dry_run,
    )
}

/// Remove what the plan says, filling in why anything could not be, and
/// removing a runtime version's directory once it is empty.
pub fn carry_out(answer: &mut pb::RemoveCacheResponse) {
    for item in &mut answer.removed {
        let path = Path::new(&item.path);
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };

        let removed = if metadata.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };

        match removed {
            Ok(()) => {
                if item.kind == RUNTIME
                    && let Some(version) = path.parent()
                {
                    let _ = std::fs::remove_dir(version);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => item.error = error.to_string(),
        }
    }

    // A release that was not removed gave nothing back, so with any left
    // the total is what those that went gave back.

    if answer.removed.iter().any(|item| !item.error.is_empty()) {
        answer.freed = answer
            .removed
            .iter()
            .filter(|item| item.error.is_empty())
            .map(|item| item.freed)
            .sum();
    }
}

/// A prune of a daemon's state directory.
pub fn prune_state(
    locations: &Locations,
    releases: &[Release],
    version: Option<String>,
    tag: Option<String>,
    dry_run: bool,
) -> pb::RemoveCacheResponse {
    let rules = PruneRules {
        own_version: muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION")),
        version,
        tag,
    };
    let mut answer = plan_prune(state_report(locations, releases), &rules, dry_run);

    if !dry_run {
        carry_out(&mut answer);
    }

    answer
}

/// A clear of a daemon's state directory.
pub fn clear_state(
    locations: &Locations,
    releases: &[Release],
    dry_run: bool,
) -> pb::RemoveCacheResponse {
    let mut answer = plan_clear(state_report(locations, releases), false, dry_run);

    if !dry_run {
        carry_out(&mut answer);
    }

    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: PathBuf) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"content").unwrap();
    }

    fn runtime(version: &str, tag: &str, in_use: &str) -> pb::CacheEntry {
        pb::CacheEntry {
            kind: RUNTIME.to_owned(),
            name: format!("{version}/{tag}"),
            version: version.to_owned(),
            tag: tag.to_owned(),
            in_use: in_use.to_owned(),
            ..Default::default()
        }
    }

    fn other(kind: &str, name: &str, in_use: &str) -> pb::CacheEntry {
        pb::CacheEntry {
            kind: kind.to_owned(),
            name: name.to_owned(),
            in_use: in_use.to_owned(),
            ..Default::default()
        }
    }

    fn named(items: &[pb::CacheEntry]) -> Vec<&str> {
        items.iter().map(|item| item.name.as_str()).collect()
    }

    fn report_of(entries: Vec<pb::CacheEntry>) -> pb::ListCacheResponse {
        pb::ListCacheResponse {
            entries,
            ..Default::default()
        }
    }

    #[test]
    fn prune_takes_other_versions_and_unused_pythons_and_leaves_what_is_in_use() {
        let rules = PruneRules {
            own_version: "1.1.0".to_owned(),
            ..Default::default()
        };
        let answer = plan_prune(
            report_of(vec![
                runtime("1.0.0", "cp313", ""),
                runtime("1.0.0", "cp314", "the release shop@r1"),
                runtime("1.1.0", "cp313", ""),
                other(CERTS, "certs", ""),
                other(UV_CACHE, "uv-cache/a", ""),
                other(PYTHON, "python/a", ""),
                other(PYTHON, "python/b", "the release shop@r1"),
            ]),
            &rules,
            true,
        );

        assert_eq!(named(&answer.removed), ["1.0.0/cp313", "python/a"]);
        assert_eq!(
            named(&answer.left),
            ["1.0.0/cp314"],
            "a Python in use is not stale"
        );

        // Naming a version or tag takes only those runtime libraries,
        // muructl's own included, and no Pythons.

        let rules = PruneRules {
            own_version: "1.1.0".to_owned(),
            tag: Some("cp313".to_owned()),
            ..Default::default()
        };
        let answer = plan_prune(
            report_of(vec![
                runtime("1.0.0", "cp313", ""),
                runtime("1.1.0", "cp313", ""),
                runtime("1.1.0", "cp314", ""),
                other(PYTHON, "python/a", ""),
            ]),
            &rules,
            true,
        );

        assert_eq!(named(&answer.removed), ["1.0.0/cp313", "1.1.0/cp313"]);
    }

    #[test]
    fn clear_takes_everything_but_certificates_unless_named_and_leaves_what_is_in_use() {
        let entries = vec![
            runtime("1.0.0", "cp313", ""),
            runtime("1.0.0", "cp314", "the development run of shop"),
            other(CERTS, "certs", ""),
            other(UV_CACHE, "uv-cache/a", ""),
        ];

        let answer = plan_clear(report_of(entries.clone()), false, true);
        assert_eq!(named(&answer.removed), ["1.0.0/cp313", "uv-cache/a"]);
        assert_eq!(named(&answer.left), ["1.0.0/cp314"]);

        let answer = plan_clear(report_of(entries), true, true);
        assert_eq!(
            named(&answer.removed),
            ["1.0.0/cp313", "certs", "uv-cache/a"]
        );
    }

    #[test]
    fn carrying_out_removes_the_plan_and_says_what_it_freed() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();

        write(root.join("runtime/1.0.0/cp313/muru/__init__.py"));
        write(root.join("runtime/1.1.0/cp313/muru/__init__.py"));

        let entries = user_entries(root);
        let rules = PruneRules {
            own_version: "1.1.0".to_owned(),
            ..Default::default()
        };
        let mut answer = plan_prune(report(root, entries), &rules, false);
        carry_out(&mut answer);

        assert_eq!(named(&answer.removed), ["1.0.0/cp313"]);
        assert!(answer.removed[0].error.is_empty());
        assert!(answer.freed > 0);
        assert!(
            !root.join("runtime/1.0.0").exists(),
            "the empty version goes"
        );
        assert!(root.join("runtime/1.1.0/cp313").exists());
    }

    #[test]
    fn a_running_hosts_runtime_library_counts_as_in_use() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let locations = Locations::resolve(&crate::locations::Options {
            state_dir: Some(root.join("state")),
            run_dir: Some(root.join("run")),
            config: Some(root.join("muru.toml")),
            ..Default::default()
        });
        let library = root.join("state/runtime/1.0.0/cp313");
        let gone = root.join("state/runtime/1.0.0/cp314");

        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&gone).unwrap();

        let record = |id: &str, pid: u32, runtime: &Path| {
            let directory = root.join("run/apps/shop").join(id);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("host.toml"),
                format!(
                    "pid = {pid}\nsockets = []\nruntime = {:?}\n",
                    runtime.display().to_string()
                ),
            )
            .unwrap();
        };

        // This process is alive, and no process has the other pid.

        record("r1", std::process::id(), &library);
        record("r2", u32::MAX / 2, &gone);

        let in_use = daemon_in_use(&locations, &[]);
        let library = std::fs::canonicalize(&library).unwrap();
        assert_eq!(
            in_use.runtime.get(&library).map(String::as_str),
            Some("the release shop@r1")
        );
        assert_eq!(in_use.runtime.len(), 1, "{in_use:?}");
    }

    #[test]
    fn the_user_cache_lists_each_runtime_version_and_tag_and_the_rest() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();

        write(root.join("runtime/0.1.0a15/cp313/muru/__init__.py"));
        write(root.join("runtime/0.1.0a16/cp315t/muru/__init__.py"));
        write(root.join("certs/dev.pem"));

        let entries = user_entries(root);
        let names: Vec<&str> = entries.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["0.1.0a15/cp313", "0.1.0a16/cp315t", "certs"]);
        assert_eq!(entries[0].version, "0.1.0a15");
        assert_eq!(entries[1].tag, "cp315t");
        assert_eq!(entries[2].kind, CERTS);

        let answer = report(root, entries);
        assert!(answer.total > 0);
        assert_eq!(answer.directory, root.display().to_string());
    }

    #[test]
    fn the_daemon_area_lists_uv_caches_and_pythons_and_marks_what_is_used() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();

        write(root.join("runtime/9.9.9/cp313/muru/__init__.py"));
        write(root.join("uv-cache/alice/wheel"));
        write(root.join("python/cpython-3.13/bin/python3.13"));
        write(root.join("python/cpython-3.14/bin/python3.14"));
        write(root.join("python/.temp/partial"));

        let mut entries = state_entries(root);
        assert_eq!(
            entries
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            [
                "9.9.9/cp313",
                "uv-cache/alice",
                "python/cpython-3.13",
                "python/cpython-3.14"
            ],
            "hidden directories uv keeps are left alone"
        );

        let python = std::fs::canonicalize(root.join("python/cpython-3.13/bin/python3.13"));
        let runtime = std::fs::canonicalize(root.join("runtime/9.9.9/cp313"));
        let mut in_use = InUse::default();
        in_use.pythons.insert(python.unwrap(), "shop@r1".to_owned());
        in_use
            .runtime
            .insert(runtime.unwrap(), "shop@r1".to_owned());

        mark_in_use(&mut entries, &in_use, "9.9.9");
        assert_eq!(entries[0].in_use, "shop@r1");
        assert_eq!(entries[1].in_use, "", "no build is under way");
        assert_eq!(entries[2].in_use, "shop@r1");
        assert_eq!(entries[3].in_use, "");

        // A build under way needs the current runtime, uv's cache and
        // every Python.

        let in_use = InUse {
            building: Some("a build".to_owned()),
            ..Default::default()
        };
        mark_in_use(&mut entries, &in_use, "9.9.9");
        assert_eq!(entries[0].in_use, "a build");
        assert_eq!(entries[1].in_use, "a build");
        assert_eq!(entries[3].in_use, "a build");

        mark_in_use(&mut entries, &in_use, "other");
        assert_eq!(entries[0].in_use, "", "another version is free");
    }
}
