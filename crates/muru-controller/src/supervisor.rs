//! Process supervision: starting the instances of a program, starting
//! again one that exits, recording them for the next daemon, taking them
//! over from such a record, and stopping them.
//!
//! Nothing here knows what the processes do. The instances of a managed
//! HTTP server (design specification section 9.9) are the one user, and
//! what is HTTP's (addresses, profiles, readiness probes) stays with
//! them in `servers.rs`. Time is tokio's throughout, so the tests run
//! under a paused clock and advance it instead of sleeping.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::host::{Identity, alive};

/// How often a process that is not a child of this daemon is looked at
/// while it is waited for.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The event types a supervisor reports under. Its user names them, so
/// that an event says what kind of thing the instances are.
#[derive(Debug, Clone, Copy)]
pub struct EventNames {
    /// An instance exited and waits to be started again.
    pub exited: &'static str,
    /// An instance was started again.
    pub started: &'static str,
    /// An instance could not be started.
    pub failed: &'static str,
}

/// What every instance of a supervised program shares.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The directory the instances run in. A command whose program is a
    /// relative path with a directory in it is taken from here.
    pub cwd: PathBuf,
    /// The environment of every instance, which starts from nothing else.
    pub env: Vec<(String, String)>,
    /// Whom the instances run as, when not as the daemon.
    pub identity: Option<Identity>,
    /// The file each instance's standard output and error are appended
    /// to, by the instance itself: the daemon is not in the path.
    pub log: PathBuf,
    /// The signal that asks an instance to stop.
    pub stop_signal: i32,
    /// The event types to report under.
    pub events: EventNames,
}

/// What differs from one instance to the next.
#[derive(Debug, Clone)]
pub struct InstanceSpec {
    /// The command line, complete: the program and its arguments.
    pub command: Vec<String>,
    /// The instance's own environment variables, set over the shared
    /// ones.
    pub env: Vec<(String, String)>,
    /// Files the process leaves behind that the next one could not start
    /// beside, such as a socket it binds: removed before every start and
    /// once the instance has stopped.
    pub stale: Vec<PathBuf>,
}

/// When an instance that exited is started again. The defaults are what
/// a managed HTTP server does. The decisions still to be made about
/// supervision (when not to start again, when to give up) belong here
/// and in `restart_after`, which is the one place an exit is judged.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    /// The wait before the start that follows an exit, doubled with each
    /// exit in a row.
    pub delay: Duration,
    /// The longest that wait gets.
    pub max_delay: Duration,
    /// A run this long counts as a recovery: the exit that ends it
    /// starts the waits over.
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        }
    }
}

impl RestartPolicy {
    /// How long an instance waits to be started again after `exit`, the
    /// latest of `quick_exits` in a row. Every exit is treated alike: a
    /// clean one, a failure, and an adopted process that is simply gone
    /// are all started again, without limit.
    fn restart_after(&self, _exit: &Exit, quick_exits: u32) -> Duration {
        self.backoff(quick_exits)
    }

    /// The wait before the next start after this many exits or failed
    /// starts in a row.
    fn backoff(&self, in_a_row: u32) -> Duration {
        self.delay
            .saturating_mul(1 << in_a_row.saturating_sub(1).min(31))
            .min(self.max_delay)
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// A child of this daemon, reaped with this status.
    Status(ExitStatus),
    /// A process adopted from a previous daemon, which has no status to
    /// give: it can only be seen to be gone.
    Gone,
}

impl fmt::Display for Exit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => write!(formatter, "exited with {status}"),
            Self::Gone => formatter.write_str("is gone"),
        }
    }
}

/// A process this daemon started, or one it adopted from a previous
/// daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

impl Process {
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Child(child) => child.id(),
            Self::Adopted(pid) => Some(*pid),
        }
    }

    /// Whether the process has exited, without waiting.
    fn try_exited(&mut self) -> std::io::Result<Option<Exit>> {
        match self {
            Self::Child(child) => Ok(child.try_wait()?.map(Exit::Status)),
            Self::Adopted(pid) => Ok((!alive(*pid)).then_some(Exit::Gone)),
        }
    }

    fn signal(&self, signal: i32) {
        if let Some(pid) = self.pid() {
            // SAFETY: kill(2) with a pid this daemon started or adopted.

            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    /// Wait for the process to exit until the deadline, then kill it.
    async fn finish(self, deadline: Instant) {
        match self {
            Self::Child(mut child) => {
                let remaining = deadline.saturating_duration_since(Instant::now());

                if tokio::time::timeout(remaining, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
            Self::Adopted(pid) => {
                while alive(pid) && Instant::now() < deadline {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }

                if alive(pid) {
                    // SAFETY: as in signal.

                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

/// One supervised instance.
pub struct Instance {
    index: u32,
    spec: InstanceSpec,
    process: Option<Process>,
    /// When the process was started, for the backoff reset.
    started: Instant,
    /// When the process was started, Unix milliseconds; 0 for one adopted
    /// from a record that did not say.
    started_unix_ms: u64,
    /// Exits and failed starts in a row, each doubling the wait before
    /// the next start.
    quick_exits: u32,
    /// When the next start is due, after an exit.
    due: Option<Instant>,
}

impl Instance {
    fn new(index: u32, spec: InstanceSpec) -> Self {
        Self {
            index,
            spec,
            process: None,
            started: Instant::now(),
            started_unix_ms: 0,
            quick_exits: 0,
            due: None,
        }
    }

    /// The instance's index, from 0.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The pid of the instance's process, when it has one.
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().and_then(Process::pid)
    }

    /// Remove what the process left behind.
    fn clear_stale(&self) {
        for path in &self.spec.stale {
            let _ = std::fs::remove_file(path);
        }
    }

    /// As a record for the next daemon: pid 0 and no start time while
    /// the instance waits to be started.
    fn record(&self) -> InstanceRecord {
        InstanceRecord {
            index: self.index,
            pid: self.pid().unwrap_or(0),
            started_unix_ms: if self.process.is_some() {
                self.started_unix_ms
            } else {
                0
            },
            quick_exits: self.quick_exits,
        }
    }
}

/// A change the supervisor reports, for the event stream.
pub struct Report {
    /// The event type.
    pub kind: &'static str,
    /// Its level.
    pub level: &'static str,
    /// What happened.
    pub message: String,
}

/// The instances as recorded for the next daemon.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// Each instance, in the order they were started in.
    pub instances: Vec<InstanceRecord>,
}

/// One instance as recorded.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceRecord {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid; 0 while it waits to be started.
    pub pid: u32,
    /// When its process was started, Unix milliseconds; 0 when not known.
    #[serde(default)]
    pub started_unix_ms: u64,
    /// Its exits and failed starts in a row, so that the waits between
    /// starts carry on under the next daemon.
    #[serde(default)]
    pub quick_exits: u32,
}

/// The instances of one supervised program.
pub struct Supervisor {
    spec: Spec,
    policy: RestartPolicy,
    instances: Vec<Instance>,
}

impl Supervisor {
    /// Start an instance for each of `instances`, indexed from 0 in that
    /// order.
    pub fn start(
        spec: Spec,
        policy: RestartPolicy,
        instances: Vec<InstanceSpec>,
    ) -> anyhow::Result<Self> {
        let instances = (0..)
            .zip(instances)
            .map(|(index, instance)| Instance::new(index, instance))
            .collect();
        let mut supervisor = Self {
            spec,
            policy,
            instances,
        };

        for position in 0..supervisor.instances.len() {
            supervisor.spawn(position)?;
        }

        Ok(supervisor)
    }

    /// Take over the instances a previous daemon recorded, each of
    /// `instances` matching the record's instance at the same place.
    ///
    /// Adoption is instance by instance. One whose process is still
    /// there is taken over: it is stopped with signals rather than
    /// waited for, and when it exits it is started again as a child of
    /// this daemon. One that is not there, because it exited while no
    /// daemon was looking or was waiting to be started when the record
    /// was written (pid 0), is an instance that exited: it is due at
    /// once, so the next `check` starts it, and its exits in a row are
    /// as recorded. Only a record that names no instance is refused.
    pub fn adopt(
        spec: Spec,
        policy: RestartPolicy,
        instances: Vec<InstanceSpec>,
        record: &Record,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!record.instances.is_empty(), "no instances were recorded");
        anyhow::ensure!(
            instances.len() == record.instances.len(),
            "{} instances were recorded, not {}",
            record.instances.len(),
            instances.len()
        );
        let now = Instant::now();
        let instances = record
            .instances
            .iter()
            .zip(instances)
            .map(|(recorded, instance)| {
                let instance = Instance {
                    quick_exits: recorded.quick_exits,
                    ..Instance::new(recorded.index, instance)
                };

                if still_there(recorded) {
                    Instance {
                        process: Some(Process::Adopted(recorded.pid)),
                        started_unix_ms: recorded.started_unix_ms,
                        ..instance
                    }
                } else {
                    Instance {
                        due: Some(now),
                        ..instance
                    }
                }
            })
            .collect();

        Ok(Self {
            spec,
            policy,
            instances,
        })
    }

    /// Start one instance's process.
    fn spawn(&mut self, position: usize) -> anyhow::Result<()> {
        let instance = &self.instances[position];

        // An instance that crashed leaves its files behind, a socket it
        // bound above all, which the next one could not start beside.

        instance.clear_stale();

        let arguments = &instance.spec.command;
        let program = Path::new(&arguments[0]);

        // A relative path with a directory in it is taken from the
        // working directory, which is what an operator writing
        // `.venv/bin/gunicorn` means; a bare name is found on PATH.

        let program = if program.is_relative() && program.components().count() > 1 {
            self.spec.cwd.join(program)
        } else {
            program.to_path_buf()
        };

        // Output goes to the log file itself, not through this daemon,
        // so a daemon that detaches or upgrades leaves the instance
        // writing; the file is rotated in place (section 11.3).

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.spec.log)
            .with_context(|| format!("opening {}", self.spec.log.display()))?;
        let mut command = Command::new(&program);
        command
            .args(&arguments[1..])
            .current_dir(&self.spec.cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .env_clear()
            .envs(self.spec.env.iter().cloned())
            .envs(instance.spec.env.iter().cloned());
        crate::host::own_session(&mut command, self.spec.identity);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let instance = &mut self.instances[position];
        instance.process = Some(Process::Child(child));
        instance.started = Instant::now();
        instance.started_unix_ms = crate::model::now_unix_ms();
        instance.due = None;
        Ok(())
    }

    /// Whether the process of the instance at `position` has exited,
    /// without waiting, and how. An instance with no process has not.
    pub fn try_exited(&mut self, position: usize) -> std::io::Result<Option<Exit>> {
        match &mut self.instances[position].process {
            Some(process) => process.try_exited(),
            None => Ok(None),
        }
    }

    /// Reap instances that exited and start again those whose wait has
    /// passed, with what happened for the event stream.
    pub fn check(&mut self) -> Vec<Report> {
        let now = Instant::now();
        let mut reports = Vec::new();

        for position in 0..self.instances.len() {
            if let Ok(Some(exit)) = self.try_exited(position) {
                let instance = &mut self.instances[position];
                instance.process = None;

                if now.saturating_duration_since(instance.started) >= self.policy.stable_after {
                    instance.quick_exits = 0;
                }

                instance.quick_exits += 1;
                let wait = self.policy.restart_after(&exit, instance.quick_exits);
                instance.due = Some(now + wait);
                reports.push(Report {
                    kind: self.spec.events.exited,
                    level: "warning",
                    message: format!(
                        "instance {} {exit}; starting it again in {}",
                        instance.index,
                        muru_config::format_duration(wait)
                    ),
                });
            }

            let instance = &self.instances[position];

            if instance.process.is_some() || !instance.due.is_some_and(|due| due <= now) {
                continue;
            }

            let index = instance.index;

            match self.spawn(position) {
                Ok(()) => reports.push(Report {
                    kind: self.spec.events.started,
                    level: "info",
                    message: format!(
                        "instance {index} started again (pid {})",
                        self.instances[position].pid().unwrap_or(0)
                    ),
                }),
                Err(error) => {
                    let instance = &mut self.instances[position];
                    instance.quick_exits += 1;
                    let wait = self.policy.backoff(instance.quick_exits);
                    instance.due = Some(now + wait);
                    reports.push(Report {
                        kind: self.spec.events.failed,
                        level: "error",
                        message: format!(
                            "instance {index} could not be started: {error:#}; trying again in {}",
                            muru_config::format_duration(wait)
                        ),
                    });
                }
            }
        }

        reports
    }

    /// The instances, in the order they were started in.
    pub fn instances(&self) -> impl Iterator<Item = &Instance> {
        self.instances.iter()
    }

    /// How many instances have a process right now.
    pub fn running(&self) -> usize {
        self.instances
            .iter()
            .filter(|instance| instance.process.is_some())
            .count()
    }

    /// The instances as a record for the next daemon.
    pub fn record(&self) -> Record {
        Record {
            instances: self.instances.iter().map(Instance::record).collect(),
        }
    }

    /// Stop every instance: the stop signal to each, then SIGKILL for
    /// one still there at `kill_at`, and what they left behind removed.
    pub async fn stop_until(mut self, kill_at: Instant) {
        for instance in &self.instances {
            if let Some(process) = &instance.process {
                process.signal(self.spec.stop_signal);
            }
        }

        for instance in &mut self.instances {
            if let Some(process) = instance.process.take() {
                process.finish(kill_at).await;
            }

            instance.clear_stale();
        }
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        for instance in self.instances {
            if let Some(Process::Child(mut child)) = instance.process {
                // The child must not be killed when its handle drops.

                let _ = child.try_wait();
                std::mem::forget(child);
            }
        }
    }
}

/// Whether the process an instance was recorded with is still there.
/// Pid 0 is how the record shows an instance that had no process, and
/// is never looked for: as a pid it would name this daemon's own process
/// group.
fn still_there(recorded: &InstanceRecord) -> bool {
    recorded.pid != 0 && alive(recorded.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: EventNames = EventNames {
        exited: "thing.exited",
        started: "thing.started",
        failed: "thing.failed",
    };

    /// A specification whose instances run in `dir`.
    fn spec(dir: &Path) -> Spec {
        Spec {
            cwd: dir.to_path_buf(),
            env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            identity: None,
            log: dir.join("output.log"),
            stop_signal: libc::SIGTERM,
            events: EVENTS,
        }
    }

    fn instance(command: &[&str]) -> InstanceSpec {
        InstanceSpec {
            command: command.iter().map(|word| (*word).to_owned()).collect(),
            env: Vec::new(),
            stale: Vec::new(),
        }
    }

    /// Wait, in real time, for something that depends on a process, which
    /// no clock of tokio's governs.
    fn wait_for<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);

        loop {
            if let Some(value) = ready() {
                return value;
            }

            assert!(std::time::Instant::now() < deadline, "{what} never came");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Check until there is something to report.
    fn reports(supervisor: &mut Supervisor) -> Vec<String> {
        wait_for("a report", || {
            let reports = supervisor.check();
            (!reports.is_empty()).then_some(reports)
        })
        .into_iter()
        .map(|report| format!("{} {}: {}", report.kind, report.level, report.message))
        .collect()
    }

    fn pids(supervisor: &Supervisor) -> Vec<u32> {
        supervisor
            .instances()
            .map(|instance| instance.pid().unwrap_or(0))
            .collect()
    }

    /// Kill a process a test's supervisor started.
    fn kill(pid: u32) {
        assert_ne!(pid, 0);

        // SAFETY: kill(2) with the pid of a child of this test.

        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }

    /// Whether a process is there, without reaping it as `alive` would.
    fn exists(pid: u32) -> bool {
        // SAFETY: kill(2) with signal 0 sends nothing.

        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn the_wait_doubles_to_the_longest() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.backoff(1), Duration::from_secs(1));
        assert_eq!(policy.backoff(2), Duration::from_secs(2));
        assert_eq!(policy.backoff(4), Duration::from_secs(8));
        assert_eq!(policy.backoff(5), Duration::from_secs(16));
        assert_eq!(policy.backoff(6), Duration::from_secs(30));
        assert_eq!(policy.backoff(9), Duration::from_secs(30));
        assert_eq!(policy.backoff(500), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn an_instance_that_exits_is_started_again_after_a_wait_that_doubles() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sh", "-c", "exit 3"])],
        )
        .unwrap();

        for wait in [1, 2, 4] {
            assert_eq!(
                reports(&mut supervisor),
                [format!(
                    "thing.exited warning: instance 0 exited with exit status: 3; \
                     starting it again in {wait}s"
                )]
            );
            assert_eq!(supervisor.running(), 0);

            // Nothing happens until the whole wait has passed.

            tokio::time::advance(Duration::from_secs(wait) - Duration::from_millis(1)).await;
            assert!(supervisor.check().is_empty());
            tokio::time::advance(Duration::from_millis(1)).await;
            let started = reports(&mut supervisor);
            assert_eq!(
                started,
                [format!(
                    "thing.started info: instance 0 started again (pid {})",
                    pids(&supervisor)[0]
                )]
            );
        }

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_lasted_starts_the_waits_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
        )
        .unwrap();

        // Two exits in a row double the wait.

        for wait in [1, 2] {
            kill(pids(&supervisor)[0]);
            let exited = reports(&mut supervisor);
            assert!(
                exited[0].ends_with(&format!("starting it again in {wait}s")),
                "{exited:?}"
            );
            tokio::time::advance(Duration::from_secs(wait)).await;
            assert!(reports(&mut supervisor)[0].starts_with("thing.started"));
        }

        // A run of a minute is a recovery, and the exit after it waits
        // as the first did.

        tokio::time::advance(Duration::from_secs(60)).await;
        kill(pids(&supervisor)[0]);
        let exited = reports(&mut supervisor);
        assert!(exited[0].ends_with("starting it again in 1s"), "{exited:?}");

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_that_fails_is_tried_again_after_a_longer_wait() {
        // The program is a relative path, taken from the working
        // directory, so that removing it there makes the next start fail.

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::os::unix::fs::symlink("/bin/sleep", dir.path().join("bin/sleep")).unwrap();
        let mut supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["bin/sleep", "600"])],
        )
        .unwrap();

        std::fs::remove_file(dir.path().join("bin/sleep")).unwrap();
        kill(pids(&supervisor)[0]);
        assert!(reports(&mut supervisor)[0].starts_with("thing.exited"));
        tokio::time::advance(Duration::from_secs(1)).await;
        let failed = reports(&mut supervisor);
        assert!(
            failed[0].starts_with("thing.failed error: instance 0 could not be started: starting "),
            "{failed:?}"
        );
        assert!(failed[0].ends_with("; trying again in 2s"), "{failed:?}");

        // Once the program is back the start goes through.

        std::os::unix::fs::symlink("/bin/sleep", dir.path().join("bin/sleep")).unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(reports(&mut supervisor)[0].starts_with("thing.started"));

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_record_names_each_process_and_none_for_one_that_waits() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![
                instance(&["/bin/sleep", "600"]),
                instance(&["/bin/sleep", "601"]),
            ],
        )
        .unwrap();

        let started = pids(&supervisor);
        let record = supervisor.record();
        assert_eq!(
            record
                .instances
                .iter()
                .map(|instance| (instance.index, instance.pid))
                .collect::<Vec<_>>(),
            [(0, started[0]), (1, started[1])]
        );
        assert!(
            record
                .instances
                .iter()
                .all(|instance| instance.started_unix_ms > 0)
        );
        assert_eq!(supervisor.running(), 2);

        // An instance that waits to be started has no pid and no start
        // time, and the other is as it was.

        kill(started[1]);
        reports(&mut supervisor);
        let record = supervisor.record();
        assert_eq!(
            (record.instances[0].index, record.instances[0].pid),
            (0, started[0])
        );
        assert_eq!(
            (
                record.instances[1].index,
                record.instances[1].pid,
                record.instances[1].started_unix_ms
            ),
            (1, 0, 0)
        );
        assert_eq!(supervisor.running(), 1);

        // The record reads back from the form it is written in.

        let text = toml::to_string(&record).unwrap();
        let read: Record = toml::from_str(&text).unwrap();
        assert_eq!(read.instances.len(), 2);
        assert_eq!(read.instances[0].pid, started[0]);

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test]
    async fn stop_lets_an_instance_that_takes_the_signal_exit() {
        let dir = tempfile::tempdir().unwrap();
        let left_behind = dir.path().join("left-behind");
        let supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![InstanceSpec {
                stale: vec![left_behind.clone()],
                ..instance(&["/bin/sleep", "600"])
            }],
        )
        .unwrap();
        let pid = pids(&supervisor)[0];
        std::fs::write(&left_behind, "").unwrap();

        // The deadline is far off, so the stop returning at all shows
        // the signal was enough.

        tokio::time::timeout(
            Duration::from_secs(60),
            supervisor.stop_until(Instant::now() + Duration::from_secs(3600)),
        )
        .await
        .expect("the instance exits on the stop signal");
        assert!(!exists(pid));
        assert!(!left_behind.exists());
    }

    #[tokio::test]
    async fn stop_kills_an_instance_still_there_at_the_deadline() {
        // The shell ignores TERM and so does the sleep it becomes, and
        // says so in a file once the signal is ignored.

        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&[
                "/bin/sh",
                "-c",
                "trap '' TERM; : > ignoring; exec /bin/sleep 600",
            ])],
        )
        .unwrap();
        let pid = pids(&supervisor)[0];
        wait_for("the trap", || {
            dir.path().join("ignoring").exists().then_some(())
        });

        // The stop takes the whole of the time allowed, since the signal
        // does nothing, and the instance is gone after it.

        let grace = Duration::from_millis(300);
        let began = std::time::Instant::now();
        supervisor.stop_until(Instant::now() + grace).await;
        assert!(began.elapsed() >= grace);
        assert!(!exists(pid));
    }

    /// A record of processes with these pids, each with two exits in a
    /// row behind it.
    fn recorded(pids: &[u32]) -> Record {
        Record {
            instances: (0..)
                .zip(pids)
                .map(|(index, pid)| InstanceRecord {
                    index,
                    pid: *pid,
                    started_unix_ms: if *pid == 0 { 0 } else { 1234 },
                    quick_exits: 2,
                })
                .collect(),
        }
    }

    /// A process that stands in for one a previous daemon started: a
    /// child of the test, which no supervisor of the test started.
    fn previous() -> std::process::Child {
        std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .unwrap()
    }

    /// A pid that names no process: that of a child that was reaped.
    fn dead_pid() -> u32 {
        let mut child = previous();
        child.kill().unwrap();
        child.wait().unwrap();
        child.id()
    }

    #[tokio::test(start_paused = true)]
    async fn adoption_takes_over_what_is_there_and_starts_the_rest() {
        // Three instances were recorded: one still runs, one exited
        // while no daemon was there, and one was waiting to be started,
        // which the record shows as pid 0.

        let dir = tempfile::tempdir().unwrap();
        let mut survivor = previous();
        let record = recorded(&[survivor.id(), dead_pid(), 0]);
        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![
                instance(&["/bin/sleep", "600"]),
                instance(&["/bin/sleep", "601"]),
                instance(&["/bin/sleep", "602"]),
            ],
            &record,
        )
        .unwrap();

        // The survivor is taken over as it was recorded, and the others
        // have no process yet.

        assert_eq!(pids(&supervisor), [survivor.id(), 0, 0]);
        assert_eq!(supervisor.running(), 1);
        assert_eq!(supervisor.record().instances[0].started_unix_ms, 1234);

        // The first check starts the two that were not there, and
        // leaves the survivor alone.

        let started = supervisor.check();
        let started: Vec<&str> = started
            .iter()
            .map(|report| report.message.as_str())
            .collect();
        let now = pids(&supervisor);
        assert_eq!(
            started,
            [
                format!("instance 1 started again (pid {})", now[1]),
                format!("instance 2 started again (pid {})", now[2]),
            ]
        );
        assert_eq!(now[0], survivor.id());
        assert_eq!(supervisor.running(), 3);

        // The exits in a row carry on from the record: with two behind
        // it, an instance's next exit waits as the third does.

        kill(now[1]);
        assert_eq!(
            reports(&mut supervisor),
            [
                "thing.exited warning: instance 1 exited with signal: 9 (SIGKILL); \
              starting it again in 4s"
            ]
        );

        // An adopted process that exits is seen to be gone and started
        // again as a child.

        survivor.kill().unwrap();
        survivor.wait().unwrap();
        assert_eq!(
            reports(&mut supervisor),
            ["thing.exited warning: instance 0 is gone; starting it again in 4s"]
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(supervisor.check().len(), 2);
        assert_eq!(supervisor.running(), 3);
        assert!(!pids(&supervisor).contains(&survivor.id()));

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn adoption_refuses_only_a_record_with_no_instances() {
        let dir = tempfile::tempdir().unwrap();
        let refused = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            Vec::new(),
            &recorded(&[]),
        );
        assert_eq!(
            refused.err().map(|error| error.to_string()),
            Some("no instances were recorded".to_owned())
        );

        // A record whose every instance is gone is taken over, and its
        // instances are started by the first check.

        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &recorded(&[dead_pid()]),
        )
        .unwrap();
        assert_eq!(supervisor.running(), 0);
        assert_eq!(supervisor.check().len(), 1);
        assert_eq!(supervisor.running(), 1);

        supervisor.stop_until(Instant::now()).await;
    }
}
