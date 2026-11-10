//! What the machine says about itself: the operating system, and whether
//! Muru runs in a container.

use std::path::Path;

use serde::Serialize;

/// The operating system Muru runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Platform {
    /// The operating system, as `linux` or `macos`.
    pub os: String,
    /// The kernel's release, as `uname -r` gives it; empty when it is not
    /// known.
    pub kernel: String,
    /// The architecture, as `x86_64` or `aarch64`.
    pub arch: String,
}

impl Platform {
    /// The platform this process runs on.
    pub fn read() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            kernel: kernel_release(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }
}

/// The kernel's release, or nothing when the system does not say.
fn kernel_release() -> String {
    uname().into_iter().nth(1).unwrap_or_default()
}

/// How many bytes an unprivileged process may still write to the file
/// system that holds `path`, or none when the system does not say.
pub fn free_bytes(path: &Path) -> Option<u64> {
    space(path).map(|(_, free)| free)
}

/// The size of the file system that holds `path` and how many bytes of
/// it an unprivileged process may still write, or none when the system
/// does not say.
fn space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;

    // SAFETY: statvfs is plain data that all-zero bytes are a valid value
    // of, the path is a NUL-terminated string this frame owns, and statvfs
    // fills the structure this frame owns.

    let found = unsafe {
        let mut found: libc::statvfs = std::mem::zeroed();

        if libc::statvfs(path.as_ptr(), &mut found) != 0 {
            return None;
        }

        found
    };

    // The types of the fields differ between systems.

    #[allow(clippy::unnecessary_cast)]
    let block = found.f_frsize as u64;

    #[allow(clippy::unnecessary_cast)]
    Some((found.f_blocks as u64 * block, found.f_bavail as u64 * block))
}

/// The room on the file system that holds one of Muru's directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Disk {
    /// What the directory is for: `state`, `logs` and so on.
    pub of: String,
    /// The directory.
    pub path: String,
    /// The size of its file system in bytes, when the system says.
    pub total_bytes: Option<u64>,
    /// How many bytes an unprivileged process may still write to it.
    pub free_bytes: Option<u64>,
}

/// The limits of a container's cgroup, each absent when it is not set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cgroup {
    /// The process the cgroup is of: `daemon`, or `muructl` when the
    /// daemon's could not be read.
    pub of: String,
    /// The cgroup's directory.
    pub directory: String,
    /// The CPU quota and its period, in microseconds.
    pub cpu_max: Option<(u64, u64)>,
    /// The CPUs it may run on.
    pub cpus: Option<usize>,
    /// The memory limit in bytes.
    pub memory_max: Option<u64>,
}

/// What the machine has, and what its processes are allowed.
#[derive(Debug, Clone, Serialize)]
pub struct Host {
    /// The kernel: its name, release, version and the machine's
    /// architecture, as `uname` gives them.
    pub kernel: Vec<String>,
    /// How many CPUs this process may run on.
    pub cpus: usize,
    /// The load averages over 1, 5 and 15 minutes.
    pub load: Option<[f64; 3]>,
    /// The machine's memory in bytes.
    pub memory_bytes: Option<u64>,
    /// How much of it is available, where the system says (Linux).
    pub memory_available_bytes: Option<u64>,
    /// The soft and hard limits on open files of the command that made
    /// this, which a daemon started the same way shares; none for a
    /// limit that is not set.
    pub open_files: Option<(Option<u64>, Option<u64>)>,
    /// The room on the file system of each of Muru's directories.
    pub disks: Vec<Disk>,
    /// The cgroup limits, where there is cgroup v2 (Linux).
    pub cgroup: Option<Cgroup>,
    /// The limits the kernel holds each of Muru's processes to, as
    /// `/proc/PID/limits` gives them (Linux), by what the process is.
    pub process_limits: Vec<(String, String)>,
}

impl Host {
    /// Read the machine: `directories` are Muru's own, each with what it
    /// is for, and `processes` Muru's own processes by pid, each with
    /// what it is, the daemon first.
    pub fn read(directories: &[(&str, &Path)], processes: &[(&str, u32)]) -> Self {
        let daemon = processes.first().map(|(_, pid)| *pid);

        let cgroup = daemon
            .and_then(crate::cgroup::Limits::read_of)
            .map(|limits| ("daemon", limits))
            .or_else(|| crate::cgroup::Limits::read().map(|limits| ("muructl", limits)))
            .map(|(of, limits)| Cgroup {
                of: of.to_owned(),
                directory: limits.dir.display().to_string(),
                cpu_max: limits.cpu_max,
                cpus: limits.cpus,
                memory_max: limits.memory_max,
            });

        Self {
            kernel: uname(),
            cpus: std::thread::available_parallelism().map_or(1, usize::from),
            load: load_averages(),
            memory_bytes: memory_bytes(),
            memory_available_bytes: memory_available_bytes(),
            open_files: open_files(),
            disks: directories
                .iter()
                .map(|(of, path)| {
                    let space = space(path);

                    Disk {
                        of: (*of).to_owned(),
                        path: path.display().to_string(),
                        total_bytes: space.map(|(total, _)| total),
                        free_bytes: space.map(|(_, free)| free),
                    }
                })
                .collect(),
            cgroup,
            process_limits: processes
                .iter()
                .filter_map(|(of, pid)| {
                    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
                    Some(((*of).to_owned(), limits))
                })
                .collect(),
        }
    }
}

/// The fields of `uname`: the system's name, the kernel's release and
/// version, and the machine's architecture.
fn uname() -> Vec<String> {
    // SAFETY: utsname is plain data that all-zero bytes are a valid value
    // of, and uname fills the one this frame owns.

    let name = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();

        if libc::uname(&mut name) != 0 {
            return Vec::new();
        }

        name
    };

    // A C character is signed on some systems and not on others.

    #[allow(clippy::unnecessary_cast)]
    let text = |field: &[libc::c_char]| -> String {
        field
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8 as char)
            .collect()
    };

    [
        &name.sysname[..],
        &name.release,
        &name.version,
        &name.machine,
    ]
    .iter()
    .map(|field| text(field))
    .collect()
}

/// The load averages over 1, 5 and 15 minutes.
fn load_averages() -> Option<[f64; 3]> {
    let mut load = [0.0; 3];

    // SAFETY: getloadavg writes at most the three values asked for into
    // the array this frame owns.

    let read = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    (read == 3).then_some(load)
}

/// The machine's memory in bytes.
fn memory_bytes() -> Option<u64> {
    // SAFETY: sysconf reads a system value and has no memory effects.

    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };

    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

/// How much memory is available, as Linux estimates it.
fn memory_available_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    let kilobytes: u64 = line.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kilobytes * 1024)
}

/// The soft and hard limits on this process's open files, none for one
/// that is not set.
fn open_files() -> Option<(Option<u64>, Option<u64>)> {
    // SAFETY: rlimit is plain data that all-zero bytes are a valid value
    // of, and getrlimit fills the one this frame owns.

    let limit = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();

        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return None;
        }

        limit
    };

    // The type of the fields differs between systems.

    #[allow(clippy::unnecessary_cast)]
    let set = |value: libc::rlim_t| (value != libc::RLIM_INFINITY).then_some(value as u64);

    Some((set(limit.rlim_cur), set(limit.rlim_max)))
}

/// Give this process's work a lower priority than anything serving, so
/// that collecting from a system short of CPU takes nothing from it.
/// Left as it is when the system refuses.
pub fn lower_priority() {
    // SAFETY: setpriority changes only this process's own scheduling
    // priority and has no memory effects; its result is not needed.

    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// Whether this process runs in a container, as far as the usual signs
/// tell: the files a container runtime leaves at the root, the variables
/// Kubernetes and systemd's container managers set, or the cgroup of
/// process 1.
pub fn in_container() -> bool {
    in_container_at(Path::new("/"), |name| std::env::var_os(name).is_some())
}

/// The same, for a file system rooted at `root` and an environment `set`
/// answers for.
fn in_container_at(root: &Path, set: impl Fn(&str) -> bool) -> bool {
    if root.join(".dockerenv").exists() || root.join("run/.containerenv").exists() {
        return true;
    }

    if set("KUBERNETES_SERVICE_HOST") || set("container") {
        return true;
    }

    std::fs::read_to_string(root.join("proc/1/cgroup")).is_ok_and(|membership| {
        ["docker", "kubepods", "containerd", "libpod", "lxc"]
            .iter()
            .any(|sign| membership.contains(sign))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_names_the_system_and_its_architecture() {
        let platform = Platform::read();

        assert_eq!(platform.os, std::env::consts::OS);
        assert!(!platform.arch.is_empty());
        assert!(!platform.kernel.is_empty());
    }

    #[test]
    fn the_machine_says_what_it_has() {
        let directory = tempfile::tempdir().unwrap();
        let host = Host::read(
            &[
                ("state", directory.path()),
                ("gone", Path::new("/no/such/place")),
            ],
            &[("daemon", std::process::id())],
        );

        assert_eq!(host.kernel.len(), 4);
        assert_eq!(host.kernel[1], Platform::read().kernel);
        assert!(host.cpus >= 1);
        assert!(host.load.is_some());
        assert!(
            host.memory_bytes
                .is_some_and(|bytes| bytes > 64 * 1024 * 1024)
        );
        assert!(
            host.open_files
                .is_some_and(|(soft, _)| soft.is_none_or(|soft| soft > 0))
        );

        assert_eq!(host.disks[0].of, "state");
        assert!(host.disks[0].total_bytes >= host.disks[0].free_bytes);
        assert!(host.disks[0].free_bytes.is_some_and(|free| free > 0));
        assert_eq!(host.disks[1].total_bytes, None);

        // What only Linux tells: the memory available, and the limits the
        // kernel holds a process to.

        if cfg!(target_os = "linux") {
            assert!(host.memory_available_bytes.is_some_and(|bytes| bytes > 0));
            let (of, limits) = &host.process_limits[0];
            assert_eq!(of, "daemon");
            assert!(limits.contains("Max open files"), "{limits}");
        } else {
            assert_eq!(host.memory_available_bytes, None);
            assert!(host.process_limits.is_empty());
            assert!(host.cgroup.is_none());
        }
    }

    #[test]
    fn a_file_system_says_what_room_it_has() {
        let directory = tempfile::tempdir().unwrap();

        assert!(free_bytes(directory.path()).is_some_and(|free| free > 0));
        assert_eq!(free_bytes(&directory.path().join("no/such/place")), None);
    }

    #[test]
    fn a_container_is_known_by_its_runtimes_signs() {
        let root = tempfile::tempdir().unwrap();
        let unset = |_: &str| false;
        assert!(!in_container_at(root.path(), unset));

        assert!(in_container_at(root.path(), |name| name == "KUBERNETES_SERVICE_HOST"));

        std::fs::create_dir_all(root.path().join("proc/1")).unwrap();
        std::fs::write(root.path().join("proc/1/cgroup"), "0::/init.scope\n").unwrap();
        assert!(!in_container_at(root.path(), unset));

        std::fs::write(
            root.path().join("proc/1/cgroup"),
            "0::/kubepods/besteffort/pod1\n",
        )
        .unwrap();
        assert!(in_container_at(root.path(), unset));

        std::fs::remove_file(root.path().join("proc/1/cgroup")).unwrap();
        std::fs::write(root.path().join(".dockerenv"), "").unwrap();
        assert!(in_container_at(root.path(), unset));
    }
}
