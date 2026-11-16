//! The file watcher behind `muructl run --reload` (design specification
//! section 9.8): it polls the watched directories and reports the files
//! that changed, respecting .gitignore and skipping caches, virtualenvs and
//! editor files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::GitignoreBuilder;
use ignore::{DirEntry, WalkBuilder};
use tokio::sync::mpsc;

/// Directory names never watched, besides what .gitignore excludes.
const SKIPPED_DIRECTORIES: [&str; 2] = ["__pycache__", "node_modules"];

/// Editor leftovers that never count as a change.
const EDITOR_FILES: [&str; 4] = ["*.swp", "*.swx", "*~", ".#*"];

/// What is watched and which files count.
pub struct Watch {
    directories: Vec<PathBuf>,
    /// Issue 723: the directories an enclosing repository ignores as a
    /// whole, walked by their own ignore files alone.
    own_rules: Vec<PathBuf>,
    include: GlobSet,
    exclude: GlobSet,
    /// Files watched whatever the patterns and directories say.
    files: Vec<PathBuf>,
}

type Snapshot = HashMap<PathBuf, (SystemTime, u64)>;

impl Watch {
    /// Watch `directories` for files matching `include` but not `exclude`.
    pub fn new(
        directories: Vec<PathBuf>,
        include: &[String],
        exclude: &[String],
    ) -> anyhow::Result<Self> {
        let own_rules = directories
            .iter()
            .filter(|directory| ignored_by_enclosing(directory))
            .cloned()
            .collect();

        Ok(Self {
            directories,
            own_rules,
            include: globs(include)?,
            exclude: globs(&[exclude, &EDITOR_FILES.map(str::to_owned)].concat())?,
            files: Vec::new(),
        })
    }

    /// Also watch these files, whatever the patterns and directories say
    /// (section 9.8: the lock, the manifest and uv's pin).
    pub fn with_files(mut self, files: Vec<PathBuf>) -> Self {
        self.files = files;
        self
    }

    /// The directories watched.
    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }

    /// The watched directories an enclosing repository's ignore files
    /// ignore as a whole, which are watched by their own alone.
    pub fn ignored_by_enclosing(&self) -> &[PathBuf] {
        &self.own_rules
    }

    /// Whether the watched directories hold no file that counts now,
    /// whatever the files watched by name.
    pub fn holds_no_file(&self) -> bool {
        self.snapshot().keys().all(|path| self.files.contains(path))
    }

    /// Take the baseline now, then return the task that polls every
    /// `interval` and sends each file that changed, appeared or
    /// disappeared since, until the receiver is dropped. The baseline is
    /// taken here rather than when the task first runs, so a change made
    /// once the caller has announced the watch is never folded into it.
    pub fn start(
        self,
        interval: Duration,
        changes: mpsc::Sender<PathBuf>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let previous = self.snapshot();
        self.run(previous, interval, changes)
    }

    async fn run(self, mut previous: Snapshot, interval: Duration, changes: mpsc::Sender<PathBuf>) {
        loop {
            tokio::time::sleep(interval).await;
            let current = self.snapshot();

            for path in changed(&previous, &current) {
                if changes.send(path).await.is_err() {
                    return;
                }
            }

            previous = current;
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut files = Snapshot::new();

        for directory in &self.directories {
            // Issue 723: a directory an enclosing repository ignores as a
            // whole, such as a project kept in another repository's
            // scratch directory, is walked by its own ignore files, as
            // if it were a repository of its own.

            let own = self.own_rules.contains(directory);
            let walker = WalkBuilder::new(directory)
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(true)
                .parents(!own)
                .require_git(!own)
                .filter_entry(|entry| !skipped(entry))
                .build();

            for entry in walker.flatten() {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };

                if !metadata.is_file() {
                    continue;
                }

                let relative = entry.path().strip_prefix(directory).unwrap_or(entry.path());

                if !self.include.is_match(relative) || self.exclude.is_match(relative) {
                    continue;
                }

                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.insert(entry.path().to_path_buf(), (modified, metadata.len()));
            }
        }

        for file in &self.files {
            let Ok(metadata) = std::fs::metadata(file) else {
                continue;
            };

            if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.insert(file.clone(), (modified, metadata.len()));
            }
        }

        files
    }
}

fn globs(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();

    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid pattern {pattern:?}"))?);
    }

    Ok(builder.build()?)
}

/// Whether the repository enclosing `directory`, when it is not one of
/// its own, ignores it as a whole: the ignore files from that
/// repository's root down to the directory's parent, and its
/// `.git/info/exclude`, match the directory or a directory above it.
fn ignored_by_enclosing(directory: &Path) -> bool {
    let Ok(directory) = directory.canonicalize() else {
        return false;
    };

    if directory.join(".git").exists() {
        return false;
    }

    let above: Vec<&Path> = directory.ancestors().skip(1).collect();
    let Some(root) = above.iter().position(|place| place.join(".git").exists()) else {
        return false;
    };

    // From the root down, as git reads them: a directory ignored higher
    // up cannot be brought back by a file below.

    for (depth, place) in above[..=root].iter().rev().enumerate() {
        let mut builder = GitignoreBuilder::new(place);
        builder.add(place.join(".gitignore"));

        if depth == 0 {
            builder.add(place.join(".git").join("info").join("exclude"));
        }

        let Ok(rules) = builder.build() else {
            continue;
        };

        if rules
            .matched_path_or_any_parents(&directory, true)
            .is_ignore()
        {
            return true;
        }
    }

    false
}

fn skipped(entry: &DirEntry) -> bool {
    if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
        return false;
    }

    let name = entry.file_name().to_string_lossy();
    SKIPPED_DIRECTORIES.contains(&name.as_ref()) || is_virtualenv(entry.path())
}

fn is_virtualenv(directory: &Path) -> bool {
    directory.join("pyvenv.cfg").is_file()
}

/// Paths whose entry differs between two snapshots, sorted.
fn changed(previous: &Snapshot, current: &Snapshot) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = current
        .iter()
        .filter(|(path, stamp)| previous.get(*path) != Some(stamp))
        .map(|(path, _)| path.clone())
        .chain(
            previous
                .keys()
                .filter(|path| !current.contains_key(*path))
                .cloned(),
        )
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_and_excludes_apply_to_relative_paths() {
        let directory = std::env::temp_dir().join(format!("muru-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("pkg/__pycache__")).expect("mkdir");
        std::fs::create_dir_all(directory.join("venv")).expect("mkdir");
        std::fs::write(directory.join("app.py"), "x").expect("write");
        std::fs::write(directory.join("pkg/views.py"), "x").expect("write");
        std::fs::write(directory.join("pkg/__pycache__/views.pyc"), "x").expect("write");
        std::fs::write(directory.join("notes.txt"), "x").expect("write");
        std::fs::write(directory.join("app.py~"), "x").expect("write");
        std::fs::write(directory.join("venv/pyvenv.cfg"), "x").expect("write");
        std::fs::write(directory.join("venv/site.py"), "x").expect("write");
        let watch = Watch::new(
            vec![directory.clone()],
            &["**/*.py".to_owned()],
            &["pkg/skip*.py".to_owned()],
        )
        .expect("globs");
        let names: Vec<String> = {
            let mut names: Vec<String> = watch
                .snapshot()
                .keys()
                .map(|path| {
                    path.strip_prefix(&directory)
                        .expect("inside")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        };
        assert_eq!(names, ["app.py", "pkg/views.py"]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn watched_names(watch: &Watch, directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = watch
            .snapshot()
            .keys()
            .map(|path| {
                path.strip_prefix(directory)
                    .expect("inside")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_project_an_enclosing_repository_ignores_is_watched_by_its_own_rules() {
        let root = std::env::temp_dir().join(format!("muru-watch-enclosed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let root = root.canonicalize().expect("canonical");

        // Issue 723: a repository whose scratch directory ignores all that
        // is in it, with a project there that has no repository of its
        // own but has its own ignore file.

        let shop = root.join("scratch/shop");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::create_dir_all(&shop).expect("mkdir");
        std::fs::write(root.join("scratch/.gitignore"), "*\n!.gitignore\n").expect("write");
        std::fs::write(shop.join("shop.py"), "x").expect("write");
        std::fs::write(shop.join("secret.py"), "x").expect("write");
        std::fs::write(shop.join(".gitignore"), "secret.py\n").expect("write");
        let watch = Watch::new(vec![shop.clone()], &["**/*.py".to_owned()], &[]).expect("globs");
        assert_eq!(watch.ignored_by_enclosing(), std::slice::from_ref(&shop));
        assert_eq!(watched_names(&watch, &shop), ["shop.py"]);
        assert!(!watch.holds_no_file());

        // A project in a part of the repository it does not ignore keeps
        // the repository's rules.

        let api = root.join("services/api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(root.join(".gitignore"), "generated_*.py\n").expect("write");
        std::fs::write(api.join("api.py"), "x").expect("write");
        std::fs::write(api.join("generated_models.py"), "x").expect("write");
        let watch = Watch::new(vec![api.clone()], &["**/*.py".to_owned()], &[]).expect("globs");
        assert!(watch.ignored_by_enclosing().is_empty());
        assert_eq!(watched_names(&watch, &api), ["api.py"]);

        // With nothing that matches, the watch says it holds no file.

        let watch = Watch::new(vec![api.clone()], &["**/*.html".to_owned()], &[]).expect("globs");
        assert!(watch.holds_no_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_change_made_before_the_task_first_runs_is_reported() {
        let directory =
            std::env::temp_dir().join(format!("muru-watch-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("mkdir");
        let lock = directory.join("uv.lock");
        std::fs::write(&lock, "version = 1\n").expect("write");
        let watch = Watch::new(vec![directory.clone()], &["**/*.py".to_owned()], &[])
            .expect("globs")
            .with_files(vec![lock.clone()]);
        let (sender, mut receiver) = mpsc::channel(8);

        // The baseline is taken by start, so a change between it and the
        // task's first poll is a change.

        let task = watch.start(Duration::from_millis(20), sender);
        std::fs::write(&lock, "version = 1\n# changed\n").expect("write");
        let polling = tokio::spawn(task);

        let reported = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("a change within the timeout");
        assert_eq!(reported, Some(lock));
        polling.abort();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn changes_are_additions_modifications_and_removals() {
        let stamp =
            |seconds: u64, size: u64| (SystemTime::UNIX_EPOCH + Duration::from_secs(seconds), size);
        let previous = Snapshot::from([
            (PathBuf::from("a.py"), stamp(1, 1)),
            (PathBuf::from("b.py"), stamp(1, 1)),
            (PathBuf::from("c.py"), stamp(1, 1)),
        ]);
        let current = Snapshot::from([
            (PathBuf::from("a.py"), stamp(1, 1)),
            (PathBuf::from("b.py"), stamp(2, 1)),
            (PathBuf::from("d.py"), stamp(1, 1)),
        ]);
        assert_eq!(
            changed(&previous, &current),
            [
                PathBuf::from("b.py"),
                PathBuf::from("c.py"),
                PathBuf::from("d.py")
            ]
        );
    }
}
