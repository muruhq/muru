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
