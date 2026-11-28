//! A daemon owns its state directory and its run directory for as long as
//! it runs (design specification section 13.3, issue 804).
//!
//! The daemon takes an exclusive `flock` on `STATE/daemon.lock` and on
//! the run directory itself as the first thing it does once its
//! locations are resolved, before it re-adopts an edge or a host, writes
//! its pid file, or removes and binds its admin socket. A second daemon
//! on either directory cannot take the lock and is refused before it
//! touches anything, so it can never take over a running daemon's socket
//! and edge. The kernel releases the locks when the process ends,
//! however it ends, so a daemon that died leaves nothing that stops the
//! next.
//!
//! The run directory holds sockets and no regular file (section 13.8,
//! issue 948), so its lock is on the directory, opened for reading, and
//! not on a file in it: a lock file under `/tmp` is deleted by age on
//! macOS while its daemon runs, after which a second daemon would find a
//! new file of that name free.
//!
//! The state directory's lock file holds the owner's pid and admin
//! socket, so a refusal names them; a refusal over the run directory
//! names the admin socket, which is in it, and the pid when the instance
//! registry has it. The descriptors are close-on-exec, so no edge, host
//! or command inherits them; an upgrade, which runs the daemon's binary
//! again in the same process, clears the flag for that exec alone and
//! names the descriptors on the new command line, so the locks are never
//! released along the way.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::fdpass;
use crate::locations::Locations;

/// The lock file in the state directory.
pub const LOCK_FILE: &str = "daemon.lock";

/// The directories a running daemon owns, held by their locks until it
/// exits.
#[derive(Debug)]
pub struct Ownership {
    held: Vec<Held>,
}

/// One lock held, by its open lock file.
#[derive(Debug)]
struct Held {
    file: File,
}

/// Which of the daemon's directories a lock is for, as a refusal names it.
const WHAT: [&str; 2] = ["state directory", "run directory"];

impl Ownership {
    /// Take the locks on the state and run directories of `locations`,
    /// creating the directories when they do not exist: the state
    /// directory's on its lock file, the run directory's on the directory. `inherited` are
    /// descriptors an upgraded daemon was started with, which already
    /// hold the locks and are used in place of new ones. The error says
    /// which directory another daemon uses, with its pid and socket.
    pub fn take(locations: &Locations, inherited: &[RawFd]) -> Result<Self, String> {
        let socket = locations.admin_socket();
        let mut inherited: Vec<OwnedFd> = inherited
            .iter()
            .filter_map(|fd| fdpass::adopt_inherited(*fd))
            .collect();
        let mut held = Vec::new();

        for (what, location) in WHAT.iter().zip([&locations.state, &locations.run]) {
            std::fs::create_dir_all(&location.path)
                .map_err(|error| format!("creating {}: {error}", location.path.display()))?;

            // One directory given as both is locked twice, on its lock
            // file and on itself, which are two open files and do not
            // conflict, so a daemon with another state directory is
            // still refused the run directory.

            let on_directory = *what == WHAT[1];

            let path = if on_directory {
                location.path.clone()
            } else {
                location.path.join(LOCK_FILE)
            };

            // An upgraded daemon's own descriptor for this file, when it
            // was given one, already holds the lock.

            if let Some(at) = inherited.iter().position(|fd| same_file(fd, &path)) {
                let file = File::from(inherited.swap_remove(at));
                let _ = fdpass::set_close_on_exec(file.as_raw_fd(), true);
                held.push(Held { file });
                continue;
            }

            let file = if on_directory {
                File::open(&path)
            } else {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)
            }
            .map_err(|error| format!("opening {}: {error}", path.display()))?;

            let locked = fdpass::try_lock(file.as_raw_fd(), true)
                .map_err(|error| format!("locking {}: {error}", path.display()))?;

            if !locked && on_directory {
                return Err(run_refusal(&location.path));
            }

            if !locked {
                return Err(refusal(what, &location.path, &path));
            }

            if !on_directory {
                write_owner(&file, &socket);
            }

            held.push(Held { file });
        }

        Ok(Self { held })
    }

    /// Clear close-on-exec on the lock descriptors, for the exec of an
    /// upgrade alone, and give their numbers for the new command line.
    pub fn keep_across_exec(&self) -> Vec<RawFd> {
        self.held
            .iter()
            .map(|held| held.file.as_raw_fd())
            .filter(|fd| fdpass::set_close_on_exec(*fd, false).is_ok())
            .collect()
    }

    /// Set close-on-exec on the lock descriptors again, after an exec
    /// that failed.
    pub fn close_on_exec(&self) {
        for held in &self.held {
            let _ = fdpass::set_close_on_exec(held.file.as_raw_fd(), true);
        }
    }
}

/// The flag an upgraded daemon is started with, naming the descriptors
/// that hold its locks.
pub const HELD_LOCKS_FLAG: &str = "--held-locks";

/// Parse the descriptor numbers of `--held-locks`: a comma-separated list.
pub fn parse_held(text: &str) -> Result<Vec<RawFd>, String> {
    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<RawFd>()
                .map_err(|_| format!("{part:?} is not a descriptor number"))
        })
        .collect()
}

/// Whether the open descriptor `fd` is the file at `path`.
fn same_file(fd: &OwnedFd, path: &Path) -> bool {
    let Ok(duplicate) = fd.try_clone() else {
        return false;
    };

    let open = File::from(duplicate).metadata();
    let named = std::fs::metadata(path);

    matches!((open, named), (Ok(open), Ok(named)) if open.dev() == named.dev() && open.ino() == named.ino())
}

/// Write the owner's pid and admin socket into its lock file, which a
/// daemon refused reads to name them.
fn write_owner(file: &File, socket: &Path) {
    let mut file = file;
    let _ = file.set_len(0);
    let _ = writeln!(
        file,
        "pid={}\nsocket={}",
        std::process::id(),
        socket.display()
    );
}

/// What a daemon refused the lock at `path` is told: the directory, and
/// the pid and socket of the daemon that holds it when its file says.
fn refusal(what: &str, directory: &Path, path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };

    let who = match field("pid") {
        Some(pid) => format!("another daemon (pid {pid})"),
        None => "another daemon".to_owned(),
    };

    match field("socket") {
        Some(socket) => format!(
            "{who} is using the {what} {}; its admin socket is {socket}",
            directory.display()
        ),
        None => format!("{who} is using the {what} {}", directory.display()),
    }
}

/// What a daemon refused the lock on the run directory is told. The
/// directory has no file to say who holds it, so the admin socket is the
/// one in it, and the pid is the registry's for the daemon with that run
/// directory, when it is there.
fn run_refusal(directory: &Path) -> String {
    let owner = crate::registry::live(&crate::registry::user_registry())
        .into_iter()
        .find(|record| record.kind == crate::registry::Kind::Daemon && record.run_dir == directory);

    let who = match owner {
        Some(record) => format!("another daemon (pid {})", record.pid),
        None => "another daemon".to_owned(),
    };

    format!(
        "{who} is using the run directory {}; its admin socket is {}",
        directory.display(),
        directory.join("admin.sock").display()
    )
}

/// Whether `message` is a refusal from [`Ownership::take`], for `muructl
/// daemon start` to report as itself.
pub fn is_refusal(message: &str) -> bool {
    message.contains("another daemon") && message.contains(" is using the ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locations::Options;

    fn locations(state: &Path, run: &Path) -> Locations {
        Locations::resolve(&Options {
            state_dir: Some(state.to_path_buf()),
            run_dir: Some(run.to_path_buf()),
            ..Options::default()
        })
    }

    #[test]
    fn a_second_owner_of_either_directory_is_refused_and_told_who_has_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path();
        let first = locations(&path.join("state"), &path.join("run"));
        let _held = Ownership::take(&first, &[]).expect("the first daemon owns both");

        // The same run directory, another state directory.

        let second = locations(&path.join("other"), &path.join("run"));
        let refused = Ownership::take(&second, &[]).expect_err("the run directory is owned");
        assert_eq!(
            refused,
            format!(
                "another daemon is using the run directory {}; its admin socket is {}",
                path.join("run").display(),
                path.join("run/admin.sock").display()
            )
        );
        assert!(is_refusal(&refused));

        // The same state directory, another run directory.

        let third = locations(&path.join("state"), &path.join("elsewhere"));
        let refused = Ownership::take(&third, &[]).expect_err("the state directory is owned");
        assert!(
            refused.contains("is using the state directory"),
            "{refused}"
        );
    }

    #[test]
    fn the_locks_are_free_once_the_owner_lets_go_and_one_directory_can_be_both() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("both");
        let both = locations(&path, &path);

        let held = Ownership::take(&both, &[]).expect("one directory as both");
        assert_eq!(held.held.len(), 2);

        // Another state directory with the same run directory is refused.

        let other = locations(&directory.path().join("other"), &path);
        let refused = Ownership::take(&other, &[]).expect_err("the run directory is owned");
        assert!(refused.contains("is using the run directory"), "{refused}");
        drop(held);

        Ownership::take(&both, &[]).expect("free again once dropped");
    }

    #[test]
    fn an_upgraded_daemon_keeps_the_locks_it_was_given() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path();
        let here = locations(&path.join("state"), &path.join("run"));
        let held = Ownership::take(&here, &[]).expect("owned");

        // What the exec passes on: the same open files under their
        // numbers, which the new image takes as its own.

        let numbers = held.keep_across_exec();
        assert_eq!(numbers.len(), 2);
        let duplicates: Vec<RawFd> = held
            .held
            .iter()
            .map(|held| {
                use std::os::fd::IntoRawFd as _;

                held.file.try_clone().expect("duplicate").into_raw_fd()
            })
            .collect();

        let again = Ownership::take(&here, &duplicates).expect("the inherited locks are used");
        assert_eq!(again.held.len(), 2);
        assert_eq!(parse_held("3,4").expect("parsed"), vec![3, 4]);
    }
}
