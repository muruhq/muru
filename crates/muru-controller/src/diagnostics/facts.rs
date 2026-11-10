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
    // SAFETY: utsname is plain data that all-zero bytes are a valid value
    // of, and uname fills the one this frame owns.

    let name = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();

        if libc::uname(&mut name) != 0 {
            return String::new();
        }

        name
    };

    name.release
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8 as char)
        .collect()
}

/// How many bytes an unprivileged process may still write to the file
/// system that holds `path`, or none when the system does not say.
pub fn free_bytes(path: &Path) -> Option<u64> {
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
    Some(found.f_bavail as u64 * found.f_frsize as u64)
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
