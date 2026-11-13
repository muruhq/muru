//! The filesystem a path is on, from the kernel's list of this process's
//! mounts, `/proc/self/mountinfo` (Linux only; elsewhere nothing is
//! known). Used to find request bodies spooled to memory (design
//! specification section 13.9) and directories shared from another
//! machine, where file events do not arrive (section 13.1).

use std::path::{Path, PathBuf};

/// One mount: where it is and its filesystem type, such as `tmpfs`,
/// `ext4` or `fuse.grpcfuse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// The mount point.
    pub point: PathBuf,
    /// The filesystem type, with a FUSE filesystem's subtype after a dot.
    pub kind: String,
}

/// The mount `path` is on, or none where the mounts cannot be read.
pub fn mount_of(path: &Path) -> Option<Mount> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    mount_in(&table, &path)
}

/// Whether `path` is on a filesystem held in memory, tmpfs or ramfs,
/// whose files count against a cgroup's memory limit.
pub fn in_memory(path: &Path) -> bool {
    mount_of(path).is_some_and(|mount| matches!(mount.kind.as_str(), "tmpfs" | "ramfs"))
}

/// The mount of `table`, the text of a mountinfo file, that holds
/// `path`: the one with the longest mount point above it, the last
/// listed winning, as a later mount hides an earlier one at the same
/// point.
pub fn mount_in(table: &str, path: &Path) -> Option<Mount> {
    let mut found: Option<Mount> = None;

    for line in table.lines() {
        let Some(mount) = parse(line) else {
            continue;
        };

        if !path.starts_with(&mount.point) {
            continue;
        }

        let longer = found
            .as_ref()
            .is_none_or(|best| mount.point.as_os_str().len() >= best.point.as_os_str().len());

        if longer {
            found = Some(mount);
        }
    }

    found
}

/// One line of mountinfo: the mount point is the fifth field, and the
/// filesystem type the first after the ` - ` separator.
fn parse(line: &str) -> Option<Mount> {
    let (before, after) = line.split_once(" - ")?;
    let point = before.split(' ').nth(4)?;
    let kind = after.split(' ').next()?;

    Some(Mount {
        point: PathBuf::from(unescape(point)),
        kind: kind.to_owned(),
    })
}

/// A mount point as the kernel writes it, with a space, a tab, a newline
/// and a backslash as three octal digits after a backslash.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;

    while at < bytes.len() {
        let octal = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });

        match octal {
            Some(digits) => {
                out.push(
                    digits
                        .iter()
                        .fold(0u8, |value, digit| value * 8 + (digit - b'0')),
                );
                at += 4;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
22 1 0:21 / / rw,relatime shared:1 - overlay overlay rw,lowerdir=/l
23 22 0:22 / /run/muru rw,nosuid,nodev - tmpfs tmpfs rw,size=65536k
24 22 0:23 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw
25 22 0:24 / /app rw,relatime - fuse.grpcfuse grpcfuse rw
26 22 0:25 / /with\\040space rw - virtiofs mount0 rw
27 22 0:26 / /run/muru rw - ext4 /dev/vda1 rw
";

    #[test]
    fn the_longest_mount_point_above_a_path_holds_it() {
        let mount = mount_in(TABLE, Path::new("/tmp/uploads")).expect("a mount");
        assert_eq!(mount.point, Path::new("/tmp"));
        assert_eq!(mount.kind, "tmpfs");

        let mount = mount_in(TABLE, Path::new("/app/src/shop.py")).expect("a mount");
        assert_eq!(mount.kind, "fuse.grpcfuse");

        let mount = mount_in(TABLE, Path::new("/srv")).expect("a mount");
        assert_eq!(mount.kind, "overlay");
    }

    #[test]
    fn a_later_mount_hides_an_earlier_one_at_the_same_point() {
        let mount = mount_in(TABLE, Path::new("/run/muru/spool")).expect("a mount");
        assert_eq!(mount.kind, "ext4");
    }

    #[test]
    fn a_mount_point_with_a_space_is_unescaped() {
        let mount = mount_in(TABLE, Path::new("/with space/x")).expect("a mount");
        assert_eq!(mount.point, Path::new("/with space"));
        assert_eq!(mount.kind, "virtiofs");
    }

    #[test]
    fn a_path_is_held_by_whole_components_only() {
        let mount = mount_in(TABLE, Path::new("/tmpfiles")).expect("a mount");
        assert_eq!(mount.kind, "overlay");
    }
}
