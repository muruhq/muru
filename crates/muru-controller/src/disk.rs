//! What files take on disk and what removing them gives back, which are
//! not the same when a file is hard-linked from elsewhere, as uv's cache
//! links files into each environment (design specification section 8.2).

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// What a set of files takes on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUse {
    /// The bytes allocated, each hard-linked file counted once.
    pub used: u64,
    /// The bytes removing all of them would give back: a file with a
    /// link outside the set gives back nothing.
    pub freed: u64,
}

/// One file's allocation, and how many of its links have been seen.
#[derive(Debug, Clone, Copy)]
struct Inode {
    bytes: u64,
    links: u64,
    seen: u64,
}

/// A running measure of what some paths take.
#[derive(Debug, Default)]
pub struct Measure {
    inodes: HashMap<(u64, u64), Inode>,
}

impl Measure {
    /// Count a file, or a directory and everything under it, without
    /// following symbolic links. What cannot be read is passed over, and
    /// a path that does not exist adds nothing.
    pub fn add(&mut self, path: &Path) {
        let mut pending = vec![path.to_path_buf()];

        while let Some(path) = pending.pop() {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };

            self.count(&metadata);

            if metadata.is_dir()
                && let Ok(entries) = std::fs::read_dir(&path)
            {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }

    fn count(&mut self, metadata: &std::fs::Metadata) {
        let inode = self
            .inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert(Inode {
                bytes: metadata.blocks() * 512,
                links: metadata.nlink(),
                seen: 0,
            });

        inode.seen += 1;
    }

    /// Add what another measure saw, so that links shared between the
    /// two count as seen.
    pub fn merge(&mut self, other: &Measure) {
        for (key, theirs) in &other.inodes {
            self.inodes
                .entry(*key)
                .and_modify(|ours| ours.seen += theirs.seen)
                .or_insert(*theirs);
        }
    }

    /// What the paths counted so far take, and what removing them gives
    /// back.
    pub fn total(&self) -> DiskUse {
        let mut total = DiskUse::default();

        for inode in self.inodes.values() {
            total.used += inode.bytes;

            if inode.seen >= inode.links {
                total.freed += inode.bytes;
            }
        }

        total
    }
}

/// What one path takes.
pub fn measure(path: &Path) -> io::Result<DiskUse> {
    std::fs::symlink_metadata(path)?;
    let mut measure = Measure::default();
    measure.add(path);
    Ok(measure.total())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, size: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; size]).unwrap();
    }

    #[test]
    fn a_hard_link_outside_the_set_gives_back_nothing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let outside = root.join("cache/shared");
        let inside = root.join("env/shared");

        write(&outside, 64 * 1024);
        write(&root.join("env/own"), 16 * 1024);
        std::fs::hard_link(&outside, &inside).unwrap();

        let env = measure(&root.join("env")).unwrap();
        assert!(env.used >= (64 + 16) * 1024, "{env:?}");
        assert!(env.freed >= 16 * 1024, "{env:?}");
        assert!(env.freed < 64 * 1024, "{env:?}");

        // Linked twice inside what is removed and nowhere else, it goes.

        std::fs::remove_file(&outside).unwrap();
        std::fs::hard_link(&inside, root.join("env/again")).unwrap();
        let env = measure(&root.join("env")).unwrap();
        assert!(env.freed >= (64 + 16) * 1024, "{env:?}");
        assert_eq!(env.used, env.freed);
    }

    #[test]
    fn links_shared_by_two_measures_count_when_merged() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();

        write(&root.join("r1/file"), 64 * 1024);
        std::fs::create_dir(root.join("r2")).unwrap();
        std::fs::hard_link(root.join("r1/file"), root.join("r2/file")).unwrap();

        let mut first = Measure::default();
        first.add(&root.join("r1"));
        let mut second = Measure::default();
        second.add(&root.join("r2"));
        assert!(first.total().freed < 64 * 1024);

        first.merge(&second);
        assert!(first.total().freed >= 64 * 1024);
    }

    #[test]
    fn a_symbolic_link_is_not_followed_and_a_missing_path_adds_nothing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();

        write(&root.join("big"), 64 * 1024);
        std::fs::create_dir(root.join("env")).unwrap();
        std::os::unix::fs::symlink(root.join("big"), root.join("env/link")).unwrap();

        assert!(measure(&root.join("env")).unwrap().used < 64 * 1024);
        assert!(measure(&root.join("absent")).is_err());
        let mut nothing = Measure::default();
        nothing.add(&root.join("absent"));
        assert_eq!(nothing.total(), DiskUse::default());
    }
}
