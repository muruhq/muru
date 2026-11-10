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

use crate::host::{Identity, alive, leads_group, signal_group, signal_process, start_time};

/// How often a process that is not a child of this daemon is looked at
/// while it is waited for.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often a process group is looked at while it empties.
const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The least that processes an instance left behind in its group get
/// between the stop signal and SIGKILL, when the time allowed for the
/// stop has less than this left; also how long a process that was
/// killed is given to go.
const LEFTOVER_GRACE: Duration = Duration::from_millis(500);

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
    /// Whether the stop signal goes to the instance's whole process
    /// group. It goes to the instance's own process alone otherwise,
    /// which is whom a server's stop signal is meant for.
    pub stop_group: bool,
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
/// daemon. Either way it was started in a session of its own, so its
/// pid is also the id of its process group, which holds whatever it
/// started that did not leave.
///
/// Each carries the start time the kernel gives the process, 0 when it
/// could not be read. A pid is used again once its process has gone, so
/// the pid of a process this daemon does not hold as a child names it
/// only together with that time.
enum Process {
    /// A child, with the pid it was started with: its handle forgets the
    /// pid once the child has been reaped, and the group is still looked
    /// for under it then.
    Child {
        child: Child,
        pid: u32,
        start_time: u64,
    },
    /// A process a previous daemon started.
    Adopted { pid: u32, start_time: u64 },
}

/// Whether the process recorded with this pid and start time is still
/// running: something has the pid, and it started when the recorded
/// process did. A pid whose process has another start time, or none
/// that can be read, has passed to another process, and the recorded
/// one is gone. A record with no start time has only the pid to go by.
/// Pid 0 is how a record shows an instance that had no process, and is
/// never looked for: as a pid it would name this daemon's own group.
fn still_running(pid: u32, started: u64) -> bool {
    pid != 0 && alive(pid) && (started == 0 || start_time(pid) == Some(started))
}

impl Process {
    /// The pid the process was started with, and so its group's id.
    fn pid(&self) -> u32 {
        match self {
            Self::Child { pid, .. } | Self::Adopted { pid, .. } => *pid,
        }
    }

    /// The start time the kernel gave the process; 0 when not known.
    fn start_time(&self) -> u64 {
        match self {
            Self::Child { start_time, .. } | Self::Adopted { start_time, .. } => *start_time,
        }
    }

    /// The pid to signal the process itself by. There is none for a
    /// child that has been reaped, nor for an adopted process that is no
    /// longer the one with its pid: the pid may be another process's by
    /// now, which must never be signalled.
    fn own_pid(&self) -> Option<u32> {
        match self {
            Self::Child { child, .. } => child.id(),
            Self::Adopted { pid, start_time } => still_running(*pid, *start_time).then_some(*pid),
        }
    }

    /// Whether the process has exited, without waiting.
    fn try_exited(&mut self) -> std::io::Result<Option<Exit>> {
        match self {
            Self::Child { child, .. } => Ok(child.try_wait()?.map(Exit::Status)),
            Self::Adopted { pid, start_time } => {
                Ok((!still_running(*pid, *start_time)).then_some(Exit::Gone))
            }
        }
    }

    /// Ask the process to stop: the signal to the process alone, or to
    /// its whole group.
    fn stop(&self, signal: i32, group: bool) {
        if group && self.group_is_own() {
            signal_group(self.pid(), signal);
        } else if let Some(pid) = self.own_pid() {
            signal_process(pid, signal);
        }
    }

    /// Whether the process exits by the deadline, waiting for it until
    /// then.
    async fn exits_by(&mut self, deadline: Instant) -> bool {
        match self {
            Self::Child { child, .. } => tokio::time::timeout_at(deadline, child.wait())
                .await
                .is_ok(),
            Self::Adopted { pid, start_time } => {
                while still_running(*pid, *start_time) && Instant::now() < deadline {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }

                !still_running(*pid, *start_time)
            }
        }
    }

    /// Kill the process and everything in its group, and wait for the
    /// process to go. It is still there, so its pid, and the group under
    /// it, are its own.
    async fn kill(&mut self) {
        match self {
            Self::Child { child, pid, .. } => {
                signal_group(*pid, libc::SIGKILL);
                let _ = child.kill().await;
            }
            Self::Adopted { pid, start_time } => {
                // A process this daemon did not start is looked at once
                // more, and checked to lead its group, before anything
                // is signalled.

                if !still_running(*pid, *start_time) {
                    return;
                }

                if leads_group(*pid) {
                    signal_group(*pid, libc::SIGKILL);
                }

                signal_process(*pid, libc::SIGKILL);
                let gone_by = Instant::now() + LEFTOVER_GRACE;

                while still_running(*pid, *start_time) && Instant::now() < gone_by {
                    tokio::time::sleep(GROUP_POLL_INTERVAL).await;
                }
            }
        }
    }

    /// The process group of a process that has exited, if anything is
    /// still running in it.
    fn left_behind(&self) -> Option<u32> {
        (self.group_is_own() && signal_group(self.pid(), 0)).then(|| self.pid())
    }

    /// Whether the process group under the process's pid is the
    /// process's own. A child's is. An adopted process's is unless
    /// another process has its pid now, when a group under that id is
    /// the other's: an id is not used again while anything is left of
    /// its group.
    fn group_is_own(&self) -> bool {
        match self {
            Self::Child { .. } => true,
            Self::Adopted { pid, start_time } => !alive(*pid) || still_running(*pid, *start_time),
        }
    }

    /// Finish a stop that has been asked for: wait for the process to
    /// exit until `kill_at`, kill its whole group if it has not, and
    /// leave nothing of the group behind either way.
    async fn finish(mut self, kill_at: Instant, stop_signal: i32) {
        let group = self.pid();

        if !self.exits_by(kill_at).await {
            self.kill().await;
        }

        // The process is gone, and nothing is to be left of its group;
        // after a kill what is found there is on its way out.

        if self.group_is_own() {
            clear_group(group, kill_at, stop_signal).await;
        }
    }
}

/// Leave nothing of a process group whose leader has gone. Whatever is
/// still in it, workers that outlive their master say, is sent the stop
/// signal, and killed at `kill_at`, or after a short grace when that
/// has all but passed. A group id is not given to another group while
/// any member is left, so what answers under it is the instance's.
async fn clear_group(group: u32, kill_at: Instant, stop_signal: i32) {
    if !signal_group(group, 0) {
        return;
    }

    signal_group(group, stop_signal);
    let grace_ends = kill_at.max(Instant::now() + LEFTOVER_GRACE);

    while signal_group(group, 0) && Instant::now() < grace_ends {
        tokio::time::sleep(GROUP_POLL_INTERVAL).await;
    }

    if signal_group(group, 0) {
        signal_group(group, libc::SIGKILL);
    }
}

/// Processes an instance left running in its process group when it
/// exited, which are gone before the instance is started again.
struct Leftover {
    /// The group's id: the pid the instance's process had.
    group: u32,
    /// Whether the group has been sent SIGKILL.
    killed: bool,
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
    /// What the process that exited left running, until it has gone.
    leftover: Option<Leftover>,
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
            leftover: None,
        }
    }

    /// The instance's index, from 0.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The pid of the instance's process, when it has one.
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Process::pid)
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
            start_time: self.process.as_ref().map_or(0, Process::start_time),
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
    /// The start time the kernel gives its process, which with the pid
    /// names the process: a number to compare for equality and nothing
    /// else (clock ticks since boot on Linux, microseconds on macOS).
    /// 0 when it is not known: there is no process, it could not be
    /// read, or the record was written before it was kept.
    #[serde(default)]
    pub start_time: u64,
}

/// The instances of one supervised program.
pub struct Supervisor {
    spec: Spec,
    policy: RestartPolicy,
    instances: Vec<Instance>,
}

impl Supervisor {
    /// Start an instance for each of `instances`, indexed from 0 in that
    /// order. When one cannot be started, those started before it are
    /// stopped, with `stop_within` to stop in before they are killed,
    /// so a start that fails leaves nothing running.
    pub async fn start(
        spec: Spec,
        policy: RestartPolicy,
        instances: Vec<InstanceSpec>,
        stop_within: Duration,
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
            if let Err(error) = supervisor.spawn(position) {
                supervisor.stop_until(Instant::now() + stop_within).await;
                return Err(error);
            }
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
    /// once, so the next `check` starts it. Only a record that names no
    /// instance is refused.
    ///
    /// The waits between starts carry on as if the daemon had not
    /// changed: an instance's exits in a row are as recorded, and one
    /// that is taken over has been running since its recorded start, so
    /// a run that has lasted is still a recovery.
    ///
    /// A process is there when something has the recorded pid and its
    /// start time is the recorded one. A pid whose process started at
    /// another time, or whose start time cannot be read, has passed to
    /// another process: the instance is not there, and that process is
    /// never signalled. A record with no start time for an instance has
    /// only the pid to go by, so its user should have other grounds for
    /// believing it, and clear the pid of one it does not believe.
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
        let now_unix_ms = crate::model::now_unix_ms();
        let instances = record
            .instances
            .iter()
            .zip(instances)
            .map(|(recorded, instance)| {
                let instance = Instance {
                    quick_exits: recorded.quick_exits,
                    ..Instance::new(recorded.index, instance)
                };

                if still_running(recorded.pid, recorded.start_time) {
                    // How long it has run, by the wall clock; a record
                    // that does not say when it started gives none.

                    let run = match recorded.started_unix_ms {
                        0 => Duration::ZERO,
                        started => Duration::from_millis(now_unix_ms.saturating_sub(started)),
                    };

                    Instance {
                        process: Some(Process::Adopted {
                            pid: recorded.pid,
                            start_time: recorded.start_time,
                        }),
                        started: now.checked_sub(run).unwrap_or(now),
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
        let pid = child
            .id()
            .with_context(|| format!("{} started with no pid", program.display()))?;
        let instance = &mut self.instances[position];
        instance.process = Some(Process::Child {
            child,
            pid,
            start_time: start_time(pid).unwrap_or(0),
        });
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
    ///
    /// An instance is not started beside what the one before it left
    /// running. Processes still in the group of an instance that exited
    /// are sent the stop signal when the exit is seen, and have the wait
    /// before the next start to go in. Any still there when the wait
    /// ends are killed, and the start is put off to the next check.
    pub fn check(&mut self) -> Vec<Report> {
        let now = Instant::now();
        let mut reports = Vec::new();

        for position in 0..self.instances.len() {
            if let Ok(Some(exit)) = self.try_exited(position) {
                let instance = &mut self.instances[position];
                let left_behind = instance
                    .process
                    .take()
                    .and_then(|process| process.left_behind());

                if let Some(group) = left_behind {
                    signal_group(group, self.spec.stop_signal);
                }

                instance.leftover = left_behind.map(|group| Leftover {
                    group,
                    killed: false,
                });

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
                        "instance {} {exit}{}; starting it again in {}",
                        instance.index,
                        if left_behind.is_some() {
                            ", leaving processes behind that are being stopped"
                        } else {
                            ""
                        },
                        muru_config::format_duration(wait)
                    ),
                });
            }

            let instance = &mut self.instances[position];

            if instance.process.is_some() {
                continue;
            }

            // A group that has emptied is forgotten at once: its id may
            // be given to another group from then on.

            if instance
                .leftover
                .as_ref()
                .is_some_and(|leftover| !signal_group(leftover.group, 0))
            {
                instance.leftover = None;
            }

            if !instance.due.is_some_and(|due| due <= now) {
                continue;
            }

            // What is left after the wait is killed, once, and the start
            // waits for the next check, by when it has gone.

            if let Some(leftover) = &mut instance.leftover
                && !leftover.killed
            {
                signal_group(leftover.group, libc::SIGKILL);
                leftover.killed = true;
                tracing::warn!(
                    instance = instance.index,
                    group = leftover.group,
                    "processes an instance left behind did not stop and were killed"
                );
                continue;
            }

            instance.leftover = None;
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

    /// Stop every instance. Each is sent the stop signal, and one still
    /// there at `kill_at` is killed with everything in its process
    /// group. Processes an instance left in its group when it exited are
    /// sent the stop signal too and killed at `kill_at`, or a moment
    /// after it when it has all but passed. What the instances left
    /// behind in files is removed.
    pub async fn stop_until(mut self, kill_at: Instant) {
        let stop_signal = self.spec.stop_signal;

        for instance in &self.instances {
            if let Some(process) = &instance.process {
                process.stop(stop_signal, self.spec.stop_group);
            }
        }

        let stops = self.instances.iter_mut().map(|instance| async move {
            if let Some(process) = instance.process.take() {
                process.finish(kill_at, stop_signal).await;
            }

            // An instance that was waiting to be started may have left
            // processes behind when it exited.

            if let Some(leftover) = instance.leftover.take() {
                clear_group(leftover.group, kill_at, stop_signal).await;
            }

            instance.clear_stale();
        });
        futures::future::join_all(stops).await;
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        for instance in self.instances {
            if let Some(Process::Child { mut child, .. }) = instance.process {
                // The child must not be killed when its handle drops.

                let _ = child.try_wait();
                std::mem::forget(child);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a start that fails gives the instances it started to stop in.
    const STOP_WITHIN: Duration = Duration::from_secs(5);

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
            stop_group: false,
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
        assert!(signal_process(pid, libc::SIGKILL));
    }

    /// Whether a process is there, without reaping it as `alive` would.
    fn exists(pid: u32) -> bool {
        signal_process(pid, 0)
    }

    /// Whether anything is left of the process group a test's instance
    /// led.
    fn group_exists(pid: u32) -> bool {
        assert_ne!(pid, 0);
        signal_group(pid, 0)
    }

    /// Start one instance that is a shell running `script`, and wait
    /// for the script to say it is set up by writing the file `ready`.
    async fn start_script(dir: &Path, spec: Spec, script: &str) -> (Supervisor, u32) {
        let supervisor = Supervisor::start(
            spec,
            RestartPolicy::default(),
            vec![instance(&["/bin/sh", "-c", script])],
            STOP_WITHIN,
        )
        .await
        .unwrap();
        let pid = pids(&supervisor)[0];
        wait_for("the script to be set up", || {
            dir.join("ready").exists().then_some(())
        });
        (supervisor, pid)
    }

    /// The pids of the processes whose command line holds the text: a
    /// path under a test's own directory, which names them alone.
    fn running_under(text: &str) -> Vec<u32> {
        let listing = std::process::Command::new("ps")
            .args(["-ax", "-o", "pid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.contains(text))
            .filter_map(|line| line.split_whitespace().next()?.parse().ok())
            .collect()
    }

    #[tokio::test]
    async fn a_start_that_fails_part_way_stops_the_instances_it_started() {
        // The first instance runs a program from the test's directory,
        // so its command line names it, and the second has no program.

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::os::unix::fs::symlink("/bin/sleep", dir.path().join("bin/sleep")).unwrap();
        let program = dir.path().join("bin/sleep").display().to_string();
        let started = Supervisor::start(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![
                instance(&["bin/sleep", "600"]),
                instance(&["bin/missing", "600"]),
            ],
            STOP_WITHIN,
        )
        .await;
        let error = started.err().expect("the second instance cannot start");
        assert!(error.to_string().starts_with("starting "), "{error}");

        // Nothing is left of the first. Whatever is found is killed
        // before the test fails, so that the test leaves nothing either.

        let left = running_under(&program);

        for pid in &left {
            kill(*pid);
        }

        assert!(left.is_empty(), "left running: {left:?}");
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
            STOP_WITHIN,
        )
        .await
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
            STOP_WITHIN,
        )
        .await
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
            STOP_WITHIN,
        )
        .await
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
            STOP_WITHIN,
        )
        .await
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
        assert_eq!(
            record
                .instances
                .iter()
                .map(|instance| Some(instance.start_time))
                .collect::<Vec<_>>(),
            [start_time(started[0]), start_time(started[1])]
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
                record.instances[1].started_unix_ms,
                record.instances[1].start_time
            ),
            (1, 0, 0, 0)
        );
        assert_eq!(supervisor.running(), 1);

        // The record reads back from the form it is written in, and so
        // does one written before the later fields were kept.

        let text = toml::to_string(&record).unwrap();
        let read: Record = toml::from_str(&text).unwrap();
        assert_eq!(read.instances.len(), 2);
        assert_eq!(read.instances[0].pid, started[0]);
        assert_eq!(read.instances[0].start_time, record.instances[0].start_time);
        let older: Record = toml::from_str("[[instances]]\nindex = 0\npid = 7\n").unwrap();
        assert_eq!(
            (
                older.instances[0].start_time,
                older.instances[0].quick_exits
            ),
            (0, 0)
        );

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
            STOP_WITHIN,
        )
        .await
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
    async fn stop_kills_the_whole_group_of_an_instance_still_there_at_the_deadline() {
        // The shell ignores TERM and so does the child it waits for, as
        // a master and its worker might.

        let dir = tempfile::tempdir().unwrap();
        let (supervisor, pid) = start_script(
            dir.path(),
            spec(dir.path()),
            "trap '' TERM; /bin/sleep 600 & : > ready; wait",
        )
        .await;

        // The stop takes the whole of the time allowed, since the signal
        // does nothing, and neither the shell nor its child outlives it.

        let grace = Duration::from_millis(300);
        let began = std::time::Instant::now();
        supervisor.stop_until(Instant::now() + grace).await;
        assert!(began.elapsed() >= grace);
        assert!(!exists(pid));
        wait_for("the group to empty", || (!group_exists(pid)).then_some(()));
    }

    #[tokio::test]
    async fn stop_leaves_nothing_behind_an_instance_that_exits() {
        // The shell takes TERM and exits, and the child it started
        // ignores it.

        let dir = tempfile::tempdir().unwrap();
        let (supervisor, pid) = start_script(
            dir.path(),
            spec(dir.path()),
            "(trap '' TERM; : > ready; exec /bin/sleep 600) & wait",
        )
        .await;

        // The child is asked to stop once the shell has gone, and killed
        // when the time allowed ends.

        supervisor
            .stop_until(Instant::now() + Duration::from_millis(300))
            .await;
        assert!(!exists(pid));
        wait_for("the group to empty", || (!group_exists(pid)).then_some(()));
    }

    #[tokio::test(start_paused = true)]
    async fn what_an_instance_leaves_behind_is_gone_before_it_starts_again() {
        // The shell starts a child that ignores TERM, and waits for it.

        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, pid) = start_script(
            dir.path(),
            spec(dir.path()),
            "(trap '' TERM; : > ready; exec /bin/sleep 600) & wait",
        )
        .await;

        // The shell is killed and its child runs on in its group, which
        // is sent the stop signal.

        kill(pid);
        assert_eq!(
            reports(&mut supervisor),
            [
                "thing.exited warning: instance 0 exited with signal: 9 (SIGKILL), \
              leaving processes behind that are being stopped; starting it again in 1s"
            ]
        );
        assert!(group_exists(pid));

        // The child ignored the signal, so when the wait ends it is
        // killed, and the instance is not started beside it.

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(supervisor.check().is_empty());
        assert_eq!(supervisor.running(), 0);
        wait_for("the group to empty", || (!group_exists(pid)).then_some(()));

        // With nothing left, the next check starts the instance.

        let started = supervisor.check();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].kind, "thing.started");
        assert_eq!(supervisor.running(), 1);

        supervisor.stop_until(Instant::now()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn what_an_instance_leaves_behind_is_stopped_with_it() {
        // As above, and the stop comes while the instance waits to be
        // started, with its child still running.

        let dir = tempfile::tempdir().unwrap();
        let (mut supervisor, pid) = start_script(
            dir.path(),
            spec(dir.path()),
            "(trap '' TERM; : > ready; exec /bin/sleep 600) & wait",
        )
        .await;
        kill(pid);
        reports(&mut supervisor);
        assert!(group_exists(pid));

        supervisor.stop_until(Instant::now()).await;
        wait_for("the group to empty", || (!group_exists(pid)).then_some(()));
    }

    #[tokio::test]
    async fn the_stop_signal_goes_to_the_group_when_asked() {
        // The shell lives through TERM, which only interrupts its first
        // wait, and then waits for its child. Only the child's exit lets
        // it go, and the child is signalled only when the group is.

        let dir = tempfile::tempdir().unwrap();
        let (supervisor, pid) = start_script(
            dir.path(),
            Spec {
                stop_group: true,
                ..spec(dir.path())
            },
            "trap : TERM; /bin/sleep 600 & : > ready; wait; wait",
        )
        .await;

        // The deadline is far off, so the stop returning at all shows
        // the signal reached the child.

        tokio::time::timeout(
            Duration::from_secs(60),
            supervisor.stop_until(Instant::now() + Duration::from_secs(3600)),
        )
        .await
        .expect("the group exits on the stop signal");
        assert!(!exists(pid));
        wait_for("the group to empty", || (!group_exists(pid)).then_some(()));
    }

    /// A record of processes with these pids, each started a moment ago
    /// with two exits in a row behind it.
    fn recorded(pids: &[u32]) -> Record {
        let now = crate::model::now_unix_ms();

        Record {
            instances: (0..)
                .zip(pids)
                .map(|(index, pid)| InstanceRecord {
                    index,
                    pid: *pid,
                    started_unix_ms: if *pid == 0 { 0 } else { now },
                    quick_exits: 2,
                    start_time: start_time(*pid).unwrap_or(0),
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
        assert_eq!(
            supervisor.record().instances[0].started_unix_ms,
            record.instances[0].started_unix_ms
        );

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
    async fn an_adopted_instance_has_run_since_its_recorded_start() {
        // The record is of a process started two minutes ago, with exits
        // in a row from before that.

        let dir = tempfile::tempdir().unwrap();
        let mut lasting = previous();
        let mut record = recorded(&[lasting.id()]);
        record.instances[0].started_unix_ms -= 120_000;
        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &record,
        )
        .unwrap();

        // Its run has lasted, so its exit is a first one, whatever the
        // daemon that sees it has been running for.

        lasting.kill().unwrap();
        lasting.wait().unwrap();
        assert_eq!(
            reports(&mut supervisor),
            ["thing.exited warning: instance 0 is gone; starting it again in 1s"]
        );

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

    #[tokio::test(start_paused = true)]
    async fn a_pid_that_another_process_has_is_not_adopted_and_never_signalled() {
        // The record names a process that is running, a child of the
        // test, with a start time that is not that process's: the pid
        // has passed to it since the record was written.

        let dir = tempfile::tempdir().unwrap();
        let mut bystander = previous();
        let mut record = recorded(&[bystander.id()]);
        assert_ne!(record.instances[0].start_time, 0);
        record.instances[0].start_time += 1;
        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &record,
        )
        .unwrap();

        // The instance has no process, and the first check starts one.

        assert_eq!(pids(&supervisor), [0]);
        assert_eq!(supervisor.check().len(), 1);
        assert_ne!(pids(&supervisor), [bystander.id()]);

        // Stopping the instance leaves the bystander running.

        supervisor.stop_until(Instant::now()).await;
        assert_eq!(bystander.try_wait().unwrap(), None);
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_adopted_process_whose_pid_passes_to_another_is_gone() {
        // What the supervisor adopted is described by a pid and a start
        // time. Here they stop matching, as when the adopted process
        // exits and its pid is given out again within a check: the
        // process that has the pid now is a child of the test.

        let dir = tempfile::tempdir().unwrap();
        let mut bystander = previous();
        let record = recorded(&[bystander.id()]);
        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &record,
        )
        .unwrap();
        assert_eq!(pids(&supervisor), [bystander.id()]);

        let Some(Process::Adopted { start_time, .. }) = &mut supervisor.instances[0].process else {
            panic!("the instance was adopted");
        };
        *start_time += 1;

        // The instance is seen to be gone, and a stop while it waits
        // signals nothing: the bystander runs on.

        assert_eq!(
            reports(&mut supervisor),
            ["thing.exited warning: instance 0 is gone; starting it again in 4s"]
        );
        supervisor.stop_until(Instant::now()).await;
        assert_eq!(bystander.try_wait().unwrap(), None);

        // Nor is it signalled when the stop comes before the check has
        // seen that the instance is gone.

        let mut supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &record,
        )
        .unwrap();

        if let Some(Process::Adopted { start_time, .. }) = &mut supervisor.instances[0].process {
            *start_time += 1;
        }

        supervisor.stop_until(Instant::now()).await;
        assert_eq!(bystander.try_wait().unwrap(), None);
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_with_no_start_time_is_adopted_on_the_pid_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut running = previous();
        let mut record = recorded(&[running.id()]);
        record.instances[0].start_time = 0;
        let supervisor = Supervisor::adopt(
            spec(dir.path()),
            RestartPolicy::default(),
            vec![instance(&["/bin/sleep", "600"])],
            &record,
        )
        .unwrap();
        assert_eq!(pids(&supervisor), [running.id()]);

        // The process is the one recorded, so the stop is its to take.
        // The supervisor reaps it, being its parent here, so there is
        // no status left for the test to wait for.

        supervisor.stop_until(Instant::now()).await;
        assert!(!exists(running.id()));
        let _ = running.wait();
    }
}
