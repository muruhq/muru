//! The Muru processes on this machine (design specification section
//! 13.10, issue 806): every process of the caller's, or every process
//! when root, whose title says it is one of Muru's or whose environment
//! carries an instance's ID, with the instance it says it belongs to.
//! `muructl instance list --orphans`, `muructl instance prune` and the
//! test suite's sweep are built on it.
//!
//! The title is the source for Muru's own processes, since one that made
//! room for a long title shows the title's bytes where its environment
//! was; the environment is what attributes a process with no title of
//! Muru's, such as a subprocess a worker started.

use crate::registry::InstanceId;

/// The roles a Muru process title names, after `muru `.
pub const ROLES: [&str; 7] = ["daemon", "edge", "host", "relay", "worker", "run", "serve"];

/// The role given to a process with no title of Muru's that carries an
/// instance's ID in its environment.
pub const UNTITLED: &str = "process";

/// One of Muru's processes, as the operating system shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuruProcess {
    /// Its pid.
    pub pid: u32,
    /// The parent the operating system records.
    pub parent: u32,
    /// The user it runs as, where it can be read.
    pub uid: Option<u32>,
    /// When it started, Unix milliseconds, so a pid is never signalled
    /// once another process has it.
    pub started_unix_ms: Option<u64>,
    /// Its role, from its title, or [`UNTITLED`].
    pub role: String,
    /// Its title, or its command line.
    pub title: String,
    /// The tag of the instance it belongs to, when it says.
    pub tag: Option<String>,
    /// The ID of the instance it belongs to, when its environment says.
    pub id: Option<InstanceId>,
    /// The label of the instance it belongs to, when its environment says.
    pub label: Option<String>,
}

/// Every Muru process of the caller's, or of every user when root, other
/// than the caller itself.
pub fn muru_processes() -> Vec<MuruProcess> {
    // SAFETY: geteuid has no preconditions and cannot fail.

    let uid = unsafe { libc::geteuid() };
    let me = std::process::id();

    crate::processes::all()
        .into_iter()
        .filter(|found| found.pid != me && (uid == 0 || found.uid == Some(uid)))
        .filter_map(|found| {
            let (title, environment) = crate::processes::command_and_environment(found.pid)?;
            let variable = |name: &str| {
                environment
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            };
            let id = variable(crate::instance::ID_VARIABLE).and_then(|text| text.parse().ok());

            let (role, tag) = match parse_title(&title) {
                Some((role, tag)) => (
                    role,
                    tag.or_else(|| variable(crate::instance::TAG_VARIABLE)),
                ),
                None if id.is_some() => {
                    (UNTITLED.to_owned(), variable(crate::instance::TAG_VARIABLE))
                }
                None => return None,
            };

            Some(MuruProcess {
                pid: found.pid,
                parent: found.parent,
                uid: found.uid,
                started_unix_ms: found.started_unix_ms,
                role,
                title,
                tag,
                id,
                label: variable(crate::registry::LABEL_VARIABLE).filter(|label| !label.is_empty()),
            })
        })
        .collect()
}

/// The role and instance tag a Muru title names: `muru ROLE ...`, with
/// the tag in parentheses at the end, or for a daemon or an edge the
/// scope after the role when it is a name rather than a state directory,
/// for a run `run:PORT` from its address, and for a serve `serve`. None
/// for a title that is not Muru's.
pub fn parse_title(title: &str) -> Option<(String, Option<String>)> {
    let rest = title.strip_prefix("muru ")?;
    let (role, after) = rest.split_once(' ').unwrap_or((rest, ""));

    if !ROLES.contains(&role) {
        return None;
    }

    let after = after.trim();

    let tagged = after
        .strip_suffix(')')
        .and_then(|inner| inner.rfind(" (").map(|open| inner[open + 2..].to_owned()))
        .or_else(|| {
            after
                .strip_prefix('(')
                .and_then(|inner| inner.strip_suffix(')'))
                .map(str::to_owned)
        });

    let tag = tagged.or_else(|| match role {
        "daemon" | "edge" if !after.is_empty() && !after.starts_with('/') => {
            Some(after.split(' ').next().unwrap_or(after).to_owned())
        }
        "run" => after
            .split(' ')
            .nth(1)
            .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
            .map(|address| format!("run:{}", address.port())),
        "serve" => Some("serve".to_owned()),
        _ => None,
    });

    Some((role.to_owned(), tag))
}

/// Whether the process `pid` still is the one found, by its start time:
/// a pid the operating system has given to another process since is
/// never signalled.
pub fn still_the_same(pid: u32, started_unix_ms: Option<u64>) -> bool {
    let Some(found) = crate::processes::read(pid) else {
        return false;
    };

    match (started_unix_ms, found.started_unix_ms) {
        (Some(expected), Some(now)) => expected.abs_diff(now) <= START_TOLERANCE_MS,
        _ => false,
    }
}

/// How far two readings of one process's start time may differ: Linux
/// counts it in clock ticks from a boot time read in whole seconds.
const START_TOLERANCE_MS: u64 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_name_their_role_and_instance() {
        let parsed = |title: &str| parse_title(title);
        let some = |role: &str, tag: &str| Some((role.to_owned(), Some(tag.to_owned())));

        assert_eq!(parsed("muru host shop@r3 (dev)"), some("host", "dev"));
        assert_eq!(
            parsed("muru worker shop@r3 web/0 (run:8000)"),
            some("worker", "run:8000")
        );
        assert_eq!(
            parsed("muru relay shop@r3 (7f3a9c2e)"),
            some("relay", "7f3a9c2e")
        );
        assert_eq!(parsed("muru daemon user"), some("daemon", "user"));
        assert_eq!(
            parsed("muru edge /srv/state (7f3a9c2e)"),
            some("edge", "7f3a9c2e")
        );
        assert_eq!(
            parsed("muru run shop 127.0.0.1:8000 dashboard 127.0.0.1:8001"),
            some("run", "run:8000")
        );
        assert_eq!(parsed("muru serve shop"), some("serve", "serve"));

        // A title from before tags, and titles that are not Muru's.

        assert_eq!(parsed("muru host shop@r3"), Some(("host".to_owned(), None)));
        assert_eq!(
            parsed("muru edge /srv/state"),
            Some(("edge".to_owned(), None))
        );
        assert_eq!(parsed("muructl run"), None);
        assert_eq!(parsed("muru unknown thing"), None);
    }

    #[test]
    fn this_process_is_the_same_and_a_changed_start_time_is_not() {
        let me = std::process::id();
        let started = crate::processes::read(me).and_then(|found| found.started_unix_ms);

        assert!(still_the_same(me, started));
        assert!(!still_the_same(me, started.map(|ms| ms + 60_000)));
        assert!(!still_the_same(me, None));
    }

    /// The variable that makes [`sleeper`] sleep.
    const SLEEPER: &str = "MURU_LOOKUP_SLEEPER";

    /// A process to find, run by the test below.
    #[test]
    #[ignore = "run by a_child_is_found_with_the_instance_its_environment_names"]
    fn sleeper() {
        if std::env::var_os(SLEEPER).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[test]
    fn a_child_is_found_with_the_instance_its_environment_names() {
        // This test's own executable, running the sleeper below: macOS
        // withholds the environment of its own system binaries, such as
        // /bin/sleep, though not of the interpreters workers run.

        let id = InstanceId::generate();
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "lookup::tests::sleeper", "--ignored"])
            .env(SLEEPER, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env(crate::instance::ID_VARIABLE, id.to_string())
            .env(crate::instance::TAG_VARIABLE, "run:8000")
            .spawn()
            .expect("sleep started");

        // The operating system lists a new process at once, but its
        // arguments only once it has run its exec.

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = None;

        while found.is_none() && std::time::Instant::now() < deadline {
            found = muru_processes()
                .into_iter()
                .find(|process| process.pid == child.id() && process.id.is_some());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();

        let found = found.expect("the child is one of Muru's by its environment");
        assert_eq!(found.role, UNTITLED);
        assert_eq!(found.id, Some(id));
        assert_eq!(found.tag.as_deref(), Some("run:8000"));
    }
}
