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
/// A managed uv.
pub const UV: &str = "uv";
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

/// What the user cache holds: the runtime library by version and tag, the
/// development certificates and a managed uv.
pub fn user_entries(root: &Path) -> Vec<pb::CacheEntry> {
    let mut entries = runtime_entries(root);
    entries.extend(whole(root, "certs", CERTS));
    entries.extend(whole(root, "tools", UV));
    entries
}

/// What a daemon's state directory holds that can be re-created: the
/// runtime library by version and tag, uv's build cache for each user,
/// the Pythons it downloaded and a managed uv.
pub fn state_entries(root: &Path) -> Vec<pb::CacheEntry> {
    let mut entries = runtime_entries(root);
    entries.extend(children(root, "uv-cache", UV_CACHE));
    entries.extend(children(root, "python", PYTHON));
    entries.extend(whole(root, "tools", UV));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: PathBuf) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"content").unwrap();
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
        write(root.join("tools/uv"));

        let entries = user_entries(root);
        let names: Vec<&str> = entries.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            ["0.1.0a15/cp313", "0.1.0a16/cp315t", "certs", "tools"]
        );
        assert_eq!(entries[0].version, "0.1.0a15");
        assert_eq!(entries[1].tag, "cp315t");
        assert_eq!(entries[2].kind, CERTS);
        assert_eq!(entries[3].kind, UV);

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
