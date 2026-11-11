//! Section 11.3: `reopen_on_rename`. The daemon notices that something,
//! such as logrotate, has renamed a log file away and has its writer open
//! the file again by name, as `muructl logs reopen` does, so no
//! `postrotate` command is needed.
//!
//! The check is a `stat` of each path every few seconds, the same on every
//! platform: a path that no longer leads to the file last seen there, a
//! different device and inode or nothing at all, has been renamed. What
//! was seen is kept in the state store, so a daemon that starts after a
//! rename made while it was stopped notices it at its first check.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// A file as its device and inode.
type Identity = (u64, u64);

/// The file last seen at each path watched.
#[derive(Debug, Default)]
pub struct Watch {
    seen: HashMap<PathBuf, Identity>,
    /// Whether `seen` changed since it was last taken to be kept.
    changed: bool,
}

impl Watch {
    /// A watch that starts from what the state store kept.
    pub fn from_seen(seen: Vec<(String, u64, u64)>) -> Self {
        Self {
            seen: seen
                .into_iter()
                .map(|(path, device, inode)| (PathBuf::from(path), (device, inode)))
                .collect(),
            changed: false,
        }
    }

    /// Check each path against the file last seen there, and say which
    /// were renamed away. A path seen for the first time is only noted,
    /// and one no longer watched is forgotten.
    pub fn check(&mut self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let mut renamed = Vec::new();

        for path in paths {
            let now = identity(path);

            match (self.seen.get(path).copied(), now) {
                (Some(was), Some(now)) if was == now => {}
                (Some(_), now) => {
                    renamed.push(path.clone());
                    self.note(path, now);
                }
                (None, Some(_)) => self.note(path, now),
                (None, None) => {}
            }
        }

        let watched = self.seen.len();
        self.seen.retain(|path, _| paths.contains(path));
        self.changed |= self.seen.len() != watched;
        renamed
    }

    /// Forget every file seen, after the writers have been told to reopen
    /// theirs: the files they open are noted at the next check, so a
    /// reopen this daemon caused, by its own rotation say, causes no
    /// second one.
    pub fn forget(&mut self) {
        if !self.seen.is_empty() {
            self.seen.clear();
            self.changed = true;
        }
    }

    /// What was seen, to be kept in the state store, when it changed since
    /// it was last taken.
    pub fn take_changed(&mut self) -> Option<Vec<(String, u64, u64)>> {
        if !std::mem::take(&mut self.changed) {
            return None;
        }

        Some(
            self.seen
                .iter()
                .map(|(path, (device, inode))| (path.display().to_string(), *device, *inode))
                .collect(),
        )
    }

    fn note(&mut self, path: &Path, now: Option<Identity>) {
        match now {
            Some(now) => self.seen.insert(path.to_path_buf(), now),
            None => self.seen.remove(path),
        };

        self.changed = true;
    }
}

/// The file at a path, when it is a file.
fn identity(path: &Path) -> Option<Identity> {
    std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_renamed_away_is_noticed_once_and_a_new_one_only_noted() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("access.log");
        let paths = [log.clone()];
        let mut watch = Watch::default();

        // Nothing there yet, then a first file: neither is a rename.

        assert!(watch.check(&paths).is_empty());
        std::fs::write(&log, "one\n").unwrap();
        assert!(watch.check(&paths).is_empty());
        assert!(watch.take_changed().is_some());
        assert!(watch.check(&paths).is_empty());
        assert!(watch.take_changed().is_none(), "nothing changed");

        // Renamed away with nothing in its place, then with a new file.

        std::fs::rename(&log, directory.path().join("access.log.1")).unwrap();
        assert_eq!(watch.check(&paths), paths);
        assert!(watch.check(&paths).is_empty(), "noticed once");
        std::fs::write(&log, "two\n").unwrap();
        assert!(
            watch.check(&paths).is_empty(),
            "a new file after nothing is only noted"
        );

        std::fs::rename(&log, directory.path().join("access.log.2")).unwrap();
        std::fs::write(&log, "three\n").unwrap();
        assert_eq!(watch.check(&paths), paths);
    }

    #[test]
    fn what_was_kept_is_checked_against_and_a_reopen_forgets_it() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("edge.log");
        std::fs::write(&log, "one\n").unwrap();
        let paths = [log.clone()];

        let mut first = Watch::default();
        first.check(&paths);
        let kept = first.take_changed().unwrap();

        // A daemon that starts after the file was renamed notices it.

        std::fs::rename(&log, directory.path().join("edge.log.1")).unwrap();
        std::fs::write(&log, "two\n").unwrap();
        let mut second = Watch::from_seen(kept);
        assert_eq!(second.check(&paths), paths);

        // After a reopen this daemon caused, the next rename is all that
        // counts, not the one already handled.

        second.forget();
        assert!(second.check(&paths).is_empty());
        assert_eq!(second.take_changed().unwrap().len(), 1);
    }
}
