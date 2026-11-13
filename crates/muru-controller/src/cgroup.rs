//! A cgroup v2's limits (design specification sections 7.7 and 13.9):
//! the CPU quota and CPUs, reported and warned against, and the memory
//! limit, which the memory guard watches. They are read from the
//! process's own cgroup, which in a container with its own cgroup
//! namespace is the root of the mount, and read again whenever they are
//! used, since `docker update` and an in-place pod resize rewrite them.

use std::path::{Path, PathBuf};

use muru_config::format_size_approx as size;

/// Where cgroup v2 is mounted.
const MOUNT: &str = "/sys/fs/cgroup";

/// What is kept free under the memory limit for muructl, the edge and
/// the page cache: this share of it, and at least `HEADROOM_MIN`.
const HEADROOM_PERCENT: u64 = 10;

const HEADROOM_MIN: u64 = 128 * 1024 * 1024;

/// Where the containers guide explains the limits, for the warnings.
pub const GUIDE: &str = "https://docs.muruhq.dev/en/latest/howto/containers.html#container-limits";

/// The limits of one cgroup, each absent when it is not set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The cgroup's directory.
    pub dir: PathBuf,
    /// The quota and period of `cpu.max`, in microseconds.
    pub cpu_max: Option<(u64, u64)>,
    /// The CPUs of `cpuset.cpus.effective`.
    pub cpus: Option<usize>,
    /// `memory.max`, in bytes.
    pub memory_max: Option<u64>,
}

impl Limits {
    /// This process's cgroup v2 limits, or none where there is no cgroup
    /// v2 (macOS, cgroup v1).
    pub fn read() -> Option<Self> {
        let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        Self::read_from(Path::new(MOUNT), &membership)
    }

    /// The cgroup v2 limits of another process, by its pid, or none
    /// where its membership cannot be read.
    pub fn read_of(pid: u32) -> Option<Self> {
        let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        Self::read_from(Path::new(MOUNT), &membership)
    }

    /// The limits of the cgroup `membership` (the text of
    /// `/proc/self/cgroup`) names under `mount`. A path the mount does not
    /// hold, as when the container shares the host's cgroup namespace, is
    /// read as the mount's root, which is then the container's.
    pub fn read_from(mount: &Path, membership: &str) -> Option<Self> {
        let path = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim_start_matches('/');
        let nested = mount.join(path);

        let dir = if nested.join("cgroup.controllers").is_file() {
            nested
        } else if mount.join("cgroup.controllers").is_file() {
            mount.to_path_buf()
        } else {
            return None;
        };

        Some(Self::read_dir(dir))
    }

    /// The limits as the files of `dir` give them now.
    pub fn read_dir(dir: PathBuf) -> Self {
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();

        Self {
            cpu_max: read("cpu.max").and_then(|text| parse_cpu_max(&text)),
            cpus: read("cpuset.cpus.effective").and_then(|text| count_cpus(&text)),
            memory_max: read("memory.max").and_then(|text| text.trim().parse().ok()),
            dir,
        }
    }

    /// The same cgroup's limits read again.
    pub fn again(&self) -> Self {
        Self::read_dir(self.dir.clone())
    }

    /// The CPUs the quota allows, rounded up and held to the CPUs the
    /// cgroup may run on, with the file that holds it back; none without
    /// a quota.
    pub fn cpus_allowed(&self) -> Option<(usize, &'static str)> {
        let (quota, period) = self.cpu_max?;
        let count = usize::try_from(quota.div_ceil(period.max(1)))
            .unwrap_or(usize::MAX)
            .max(1);

        match self.cpus {
            Some(cpus) if cpus < count => Some((cpus, "cpuset.cpus.effective")),
            _ => Some((count, "cpu.max")),
        }
    }

    /// The headroom kept under `memory.max`, without a limit none.
    pub fn headroom(&self) -> Option<u64> {
        self.memory_max.map(headroom)
    }

    /// The limits in a line: `2 CPUs from cpu.max 200000 100000,
    /// memory.max 1.0GB with 128MB of headroom`.
    pub fn describe(&self) -> String {
        let cpu = match (self.cpu_max, self.cpus) {
            (Some((quota, period)), _) => format!(
                "{} from cpu.max {quota} {period}",
                cpus_text(quota as f64 / period.max(1) as f64)
            ),
            (None, Some(cpus)) => format!(
                "no CPU quota, {} from cpuset.cpus.effective",
                cpus_text(cpus as f64)
            ),
            (None, None) => "no CPU quota".to_owned(),
        };

        let memory = match self.memory_max {
            Some(limit) => format!(
                "memory.max {} with {} of headroom",
                size(limit),
                size(headroom(limit))
            ),
            None => "no memory.max".to_owned(),
        };

        format!("{cpu}, {memory}")
    }

    /// What changed since `before`, a phrase for each limit, such as
    /// `memory.max from 1.0GB to 2.0GB`.
    pub fn changes(&self, before: &Self) -> Vec<String> {
        let mut changes = Vec::new();

        if self.cpu_max != before.cpu_max {
            let text = |cpu_max: Option<(u64, u64)>| match cpu_max {
                Some((quota, period)) => format!("{quota} {period}"),
                None => "max".to_owned(),
            };
            changes.push(format!(
                "cpu.max from {} to {}",
                text(before.cpu_max),
                text(self.cpu_max)
            ));
        }

        if self.cpus != before.cpus {
            let text = |cpus: Option<usize>| cpus.map_or_else(|| "none".to_owned(), cpus_text_int);
            changes.push(format!(
                "cpuset.cpus.effective from {} to {}",
                text(before.cpus),
                text(self.cpus)
            ));
        }

        if self.memory_max != before.memory_max {
            let text = |limit: Option<u64>| limit.map_or_else(|| "max".to_owned(), size);
            changes.push(format!(
                "memory.max from {} to {}",
                text(before.memory_max),
                text(self.memory_max)
            ));
        }

        changes
    }

    /// What the cgroup uses of its memory limit now, without a limit
    /// none.
    pub fn memory(&self) -> Option<Memory> {
        let limit = self.memory_max?;
        let used = memory_used(&self.dir)?;
        Some(Memory { limit, used })
    }
}

/// The headroom kept under a memory limit: 10 % of it, at least 128 MiB.
pub fn headroom(limit: u64) -> u64 {
    (limit / 100 * HEADROOM_PERCENT).max(HEADROOM_MIN)
}

/// A cgroup's memory limit and what it uses of it, inactive file pages
/// counted as free, since the kernel reclaims those before it kills
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// `memory.max`.
    pub limit: u64,
    /// `memory.current` less `inactive_file` of `memory.stat`.
    pub used: u64,
}

impl Memory {
    /// What is left under the limit.
    pub fn left(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// The headroom kept under the limit.
    pub fn headroom(&self) -> u64 {
        headroom(self.limit)
    }
}

/// What the cgroup at `dir` uses of memory, inactive file pages left
/// out.
pub fn memory_used(dir: &Path) -> Option<u64> {
    let current: u64 = std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let inactive = std::fs::read_to_string(dir.join("memory.stat"))
        .ok()
        .and_then(|stat| {
            stat.lines()
                .find_map(|line| line.strip_prefix("inactive_file "))
                .and_then(|count| count.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);

    Some(current.saturating_sub(inactive))
}

/// A count of CPUs as a person says it: `2 CPUs`, `1.5 CPUs`, `1 CPU`.
fn cpus_text(cpus: f64) -> String {
    if (cpus - 1.0).abs() < f64::EPSILON {
        return "1 CPU".to_owned();
    }

    if cpus.fract().abs() < 0.005 {
        format!("{cpus:.0} CPUs")
    } else {
        format!("{} CPUs", format!("{cpus:.2}").trim_end_matches('0'))
    }
}

fn cpus_text_int(cpus: usize) -> String {
    cpus_text(cpus as f64)
}

/// `cpu.max`'s quota and period, or none for `max`, no quota.
fn parse_cpu_max(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?.parse().ok()?;
    let period = fields
        .next()
        .map_or(Some(100_000), |field| field.parse().ok())?;
    Some((quota, period))
}

/// The CPUs a list such as `0-3,6,8-9` names.
fn count_cpus(text: &str) -> Option<usize> {
    let mut count = 0;

    for range in text.trim().split(',').filter(|range| !range.is_empty()) {
        count += match range.split_once('-') {
            Some((first, last)) => {
                let first: usize = first.parse().ok()?;
                let last: usize = last.parse().ok()?;
                last.checked_sub(first)? + 1
            }
            None => {
                range.parse::<usize>().ok()?;
                1
            }
        };
    }

    (count > 0).then_some(count)
}

/// The kernel's count of processes in the cgroup killed for memory, from
/// `memory.events`.
pub fn oom_kills(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.events"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cgroup(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::write(dir.path().join("cgroup.controllers"), "cpu memory\n").expect("write");

        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).expect("write");
        }

        dir
    }

    #[test]
    fn the_cpus_allowed_are_the_quota_rounded_up_and_held_to_the_cpus() {
        let dir = cgroup(&[
            ("cpu.max", "150000 100000\n"),
            ("cpuset.cpus.effective", "0-63\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.cpus_allowed(), Some((2, "cpu.max")));
        assert_eq!(
            limits.describe(),
            "1.5 CPUs from cpu.max 150000 100000, no memory.max"
        );

        let dir = cgroup(&[
            ("cpu.max", "800000 100000\n"),
            ("cpuset.cpus.effective", "0-1,4\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.cpus_allowed(), Some((3, "cpuset.cpus.effective")));

        let dir = cgroup(&[
            ("cpu.max", "max 100000\n"),
            ("cpuset.cpus.effective", "2-5\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.cpus_allowed(), None);

        let dir = cgroup(&[("cpu.max", "1000 100000\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.cpus_allowed(), Some((1, "cpu.max")));
    }

    #[test]
    fn the_headroom_is_a_tenth_of_the_limit_and_at_least_128_mib() {
        assert_eq!(headroom(4 << 30), (4u64 << 30) / 100 * 10);
        assert_eq!(headroom(1 << 30), 128 << 20);

        let dir = cgroup(&[
            ("cpu.max", "200000 100000\n"),
            ("memory.max", "1073741824\n"),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.headroom(), Some(128 << 20));
        assert_eq!(
            limits.describe(),
            "2 CPUs from cpu.max 200000 100000, memory.max 1.0GB with 128MB of headroom"
        );

        let dir = cgroup(&[("memory.max", "max\n")]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert_eq!(limits.headroom(), None);
        assert_eq!(limits.memory(), None);
    }

    #[test]
    fn what_is_used_leaves_out_inactive_file_pages() {
        let dir = cgroup(&[
            ("memory.max", "1073741824\n"),
            ("memory.current", "943718400\n"),
            (
                "memory.stat",
                "anon 600000000\nfile 300000000\ninactive_file 104857600\n",
            ),
        ]);
        let limits = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        let memory = limits.memory().expect("memory");
        assert_eq!(memory.used, 943_718_400 - 104_857_600);
        assert_eq!(memory.left(), (1 << 30) - (943_718_400 - 104_857_600));
        assert_eq!(memory.headroom(), 128 << 20);
    }

    #[test]
    fn a_change_is_said_limit_by_limit() {
        let dir = cgroup(&[
            ("cpu.max", "200000 100000\n"),
            ("memory.max", "1073741824\n"),
        ]);
        let before = Limits::read_from(dir.path(), "0::/\n").expect("limits");
        assert!(before.again().changes(&before).is_empty());

        std::fs::write(dir.path().join("memory.max"), "2147483648\n").expect("write");
        std::fs::write(dir.path().join("cpu.max"), "max 100000\n").expect("write");
        assert_eq!(
            before.again().changes(&before),
            [
                "cpu.max from 200000 100000 to max",
                "memory.max from 1.0GB to 2.0GB"
            ]
        );
    }

    #[test]
    fn a_path_the_mount_does_not_hold_is_read_at_its_root() {
        let dir = cgroup(&[("memory.max", "1073741824\n")]);
        let limits = Limits::read_from(dir.path(), "0::/kubepods/pod1/abc\n").expect("limits");
        assert_eq!(limits.dir, dir.path());
        assert_eq!(limits.memory_max, Some(1 << 30));

        let empty = tempfile::tempdir().expect("temporary directory");
        assert_eq!(Limits::read_from(empty.path(), "0::/\n"), None);
        assert_eq!(Limits::read_from(dir.path(), "1:cpu:/\n"), None);
    }

    #[test]
    fn oom_kills_are_counted_from_memory_events() {
        let dir = cgroup(&[(
            "memory.events",
            "low 0\nhigh 0\nmax 12\noom 2\noom_kill 2\noom_group_kill 0\n",
        )]);
        assert_eq!(oom_kills(dir.path()), Some(2));
    }
}
