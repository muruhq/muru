//! Managed HTTP server releases (design specification section 9.9): the
//! instances of a release that runs an ordinary HTTP server, which the
//! daemon starts, probes, supervises and stops, and which the edge
//! reaches as an upstream named after the release. Each instance runs
//! under a log relay of its own, its parent (issue 492, ADR 0059).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use muru_config::{ServerBind, ServerProfile};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::Instant;

use crate::host::Identity;
use crate::locations::HostDirs;
use crate::model::HttpServerSpec;
use crate::supervisor::{self, EventNames, InstanceSpec, Report, Supervisor};

/// How long one readiness probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the instances are probed while they start.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the instances get to be ready when the release says nothing.
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(60);

/// The event of an instance started again that answers its readiness
/// probe, or is taken into service without (issue 917).
const READY_EVENT: &str = "server.ready";

/// Hears how the probe of an instance started again ended: its index, the
/// pid probed and whether it answered in time.
pub type Probed = Arc<dyn Fn(u32, u32, bool) + Send + Sync>;

/// The events the instances of a managed server are reported under.
const EVENTS: EventNames = EventNames {
    exited: "server.exited",
    started: "server.started",
    failed: "server.failed",
    crash_looping: "server.crash_looping",
    gave_up: "server.gave_up",
    stopped: "server.stopped",
    killed: "server.killed",
    daemonised: "server.daemonised",
};

/// Where one instance listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// A Unix socket.
    Unix(PathBuf),
    /// A loopback port.
    Port(u16),
}

impl Bind {
    /// The address as an upstream names it: `unix:/path` or
    /// `127.0.0.1:port`.
    pub fn address(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix:{}", path.display()),
            Self::Port(port) => format!("127.0.0.1:{port}"),
        }
    }

    /// The address parsed back from a record.
    fn parse(text: &str) -> Option<Self> {
        if let Some(path) = text.strip_prefix("unix:") {
            return Some(Self::Unix(PathBuf::from(path)));
        }

        text.strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse().ok())
            .map(Self::Port)
    }
}

/// What a previous daemon recorded about the instances, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ServersRecord {
    /// Each instance's pid and address.
    pub instances: Vec<InstanceRecord>,
}

/// One instance as recorded: what the supervisor records of its process,
/// and its address.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InstanceRecord {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid; 0 while it waits to be started again.
    pub pid: u32,
    /// Where it listens, as an upstream names it.
    pub address: String,
    /// When its process was started, Unix milliseconds; 0 when not known.
    #[serde(default)]
    pub started_unix_ms: u64,
    /// Its quick exits and failed starts in a row.
    #[serde(default)]
    pub quick_exits: u32,
    /// How it was left, for one that exited and is not started again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<supervisor::Left>,
    /// The start time the kernel gives its process, which with the pid
    /// names the process; 0 when not known.
    #[serde(default)]
    pub start_time: u64,
    /// The pid of its log relay; 0 for an instance started without one.
    #[serde(default)]
    pub relay_pid: u32,
    /// The relay's start time, which with its pid names it.
    #[serde(default)]
    pub relay_start_time: u64,
}

impl InstanceRecord {
    /// The part of the record that is the supervisor's.
    fn process(&self) -> supervisor::InstanceRecord {
        supervisor::InstanceRecord {
            index: self.index,
            pid: self.pid,
            started_unix_ms: self.started_unix_ms,
            quick_exits: self.quick_exits,
            left: self.left,
            start_time: self.start_time,
            relay_pid: self.relay_pid,
            relay_start_time: self.relay_start_time,
        }
    }
}

/// One instance as `release show` gives it: what is recorded of it, and
/// where it stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceStatus {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid; 0 when it has no process.
    pub pid: u32,
    /// Where it listens, as an upstream names it.
    pub address: String,
    /// When its process was started, Unix milliseconds; 0 when it has
    /// none or it is not known.
    pub started_unix_ms: u64,
    /// `running`, `starting` (its process has not answered the readiness
    /// probe since it was started again), `backoff` (it exited and its
    /// next start is due), `exited` or `failed` (it is not started
    /// again).
    pub state: &'static str,
    /// For an instance in `backoff`, when its next start is due, Unix
    /// milliseconds; 0 otherwise.
    pub next_start_unix_ms: u64,
    /// Its quick exits and failed starts in a row.
    pub quick_exits: u32,
}

/// How the daemon has each instance's log relay started: from `executable`,
/// with its counts and events going to `watcher`.
#[derive(Clone)]
pub struct Relays {
    /// The executable to start relays from: this program.
    pub executable: PathBuf,
    /// What each relay queues and does when full.
    pub settings: crate::host::RelaySettings,
    /// The release, as app@release.
    pub release: String,
    /// Hears what the relays report.
    pub watcher: crate::host::RelayWatcher,
}

/// Section 9.9 (issue 917): whether an instance is out of service at the
/// edge as starting, from when its relay reports its end (issue 922) or
/// its exit is seen, or it is adopted as gone, until its process answers
/// the readiness probe or the readiness timeout passes.
#[derive(Default)]
struct Starting {
    marked: bool,
    /// The process being probed, once the instance has one; or the one
    /// whose relay reported its end, with no probe, while the supervisor
    /// still counts it as running.
    pid: Option<u32>,
    probe: Option<tokio::task::JoinHandle<()>>,
}

impl Starting {
    /// Marked, with no process to probe yet.
    fn marked() -> Self {
        Self {
            marked: true,
            pid: None,
            probe: None,
        }
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.take() {
            probe.abort();
        }
    }
}

/// A restart of every instance by hand (issue 393), one at a time: each
/// is taken out of service, stopped, started again and waited for until
/// it answers its readiness probe before the next is touched.
struct Rolling {
    /// The instances still to restart, by their place.
    queue: std::collections::VecDeque<usize>,
    /// The one being restarted, and how far it has got.
    current: Option<(usize, Phase)>,
    /// How many the restart began with.
    total: u32,
    /// When it began, Unix milliseconds.
    started_unix_ms: u64,
    /// How long an instance asked to stop has before it is killed.
    grace: Duration,
}

/// How far the restart of one instance has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// It is marked starting, out of service, and the edge is being
    /// told; the next check stops it.
    Marked,
    /// Its process was asked to stop, or it had none: the supervisor
    /// starts it.
    Stopping,
    /// The process with this pid was started in its place and is being
    /// probed.
    Probing(u32),
}

/// How far a restart of every instance has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartProgress {
    /// The instances it began with.
    pub total: u32,
    /// Those not yet restarted and answering.
    pub left: u32,
    /// When it began, Unix milliseconds.
    pub started_unix_ms: u64,
}

/// The events of a restart of every instance.
const RESTART_STARTED: &str = "restart.started";
const RESTART_FINISHED: &str = "restart.finished";

/// The instances of one managed HTTP release: their processes, which a
/// supervisor runs, and where each listens.
pub struct Servers {
    readiness_path: Option<String>,
    /// How long an instance started again gets to answer its probe.
    readiness_timeout: Duration,
    /// Each instance's starting mark, in the supervisor's order.
    starting: Vec<Starting>,
    dirs: HostDirs,
    /// Each instance's address, in the supervisor's order.
    binds: Vec<Bind>,
    /// Boxed so that what holds a release's servers is of a size with
    /// what holds a host.
    supervisor: Box<Supervisor>,
    /// Lines the instances' relays have dropped, across every relay.
    dropped: Arc<AtomicU64>,
    /// The program the command runs, as the process view names it.
    program: String,
    /// The instances' exits this daemon has seen, by outcome.
    exits: std::collections::BTreeMap<supervisor::Outcome, u64>,
    /// The restart of every instance under way, if one is.
    rolling: Option<Rolling>,
    /// Why the last restart of every instance stopped early, if it did.
    restart_failure: Option<String>,
    /// What a restart has to report, for the next check to hand on.
    pending: Vec<Report>,
}

impl Servers {
    /// Start every instance in `dirs`, its sockets in one directory and
    /// its files in the other (section 13.8), each with its own address, running `spec.command` in `cwd` with `env`, as
    /// `identity` when one is set, its output going to `log` through a
    /// relay of its own with `relays`, or appended by the instance itself
    /// without. When an instance cannot be started, those started before
    /// it are stopped, with `grace` to stop in before they are killed.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        spec: &HttpServerSpec,
        dirs: &HostDirs,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
        relays: Option<Relays>,
        grace: Duration,
    ) -> anyhow::Result<Self> {
        dirs.create().with_context(|| {
            format!(
                "creating {} and {}",
                dirs.sockets.display(),
                dirs.files.display()
            )
        })?;
        let binds = allocate(spec.bind, &dirs.sockets, spec.instances.max(1))?;
        let instances = (0..)
            .zip(&binds)
            .map(|(index, bind)| instance_spec(spec, index, bind))
            .collect();
        let dropped = Arc::new(AtomicU64::new(0));
        let started = Supervisor::start(
            process_spec(spec, cwd, env, identity, log, dirs, relays, &dropped),
            spec.restart.policy(),
            instances,
            grace,
        )
        .await;

        let supervisor = match started {
            Ok(supervisor) => supervisor,
            Err(error) => {
                dirs.remove();
                return Err(error);
            }
        };

        Ok(Self {
            readiness_path: spec.readiness_path.clone(),
            readiness_timeout: readiness_timeout(spec),
            starting: binds.iter().map(|_| Starting::default()).collect(),
            dirs: dirs.clone(),
            binds,
            supervisor: Box::new(supervisor),
            dropped,
            program: crate::processes::program_of(&spec.command).unwrap_or_default(),
            exits: std::collections::BTreeMap::new(),
            rolling: None,
            restart_failure: None,
            pending: Vec::new(),
        })
    }

    /// Take over the instances a previous daemon recorded, each one
    /// that is still there, with those that are not due to be started
    /// by the next `check`. Only a record with no instances, or with an
    /// address that cannot be read, is refused.
    ///
    /// An instance is there when the process with its pid has the start
    /// time it was recorded with, which the supervisor checks. A record
    /// with no start time for an instance, as one written before the
    /// start time was kept, leaves only the pid, which any process may
    /// have by now. Such an instance is taken over only if something
    /// answers HTTP at its address as well, and is otherwise started
    /// again, with the process that has its pid left alone.
    ///
    /// An instance not taken over is marked starting, out of service
    /// until it is started again and answers (issue 917); one taken over
    /// is not, having been ready under the daemon before.
    #[allow(clippy::too_many_arguments)]
    pub async fn adopt(
        spec: &HttpServerSpec,
        dirs: &HostDirs,
        cwd: &Path,
        env: Vec<(String, String)>,
        identity: Option<Identity>,
        log: &Path,
        relays: Option<Relays>,
        record: &ServersRecord,
    ) -> anyhow::Result<Self> {
        let mut binds = Vec::new();

        for instance in &record.instances {
            let bind = Bind::parse(&instance.address).with_context(|| {
                format!(
                    "instance {}: {:?} is not an address",
                    instance.index, instance.address
                )
            })?;
            binds.push(bind);
        }

        let instances = record
            .instances
            .iter()
            .zip(&binds)
            .map(|(instance, bind)| instance_spec(spec, instance.index, bind))
            .collect();
        let mut processes = supervisor::Record {
            instances: record
                .instances
                .iter()
                .map(InstanceRecord::process)
                .collect(),
        };

        for (process, bind) in processes.instances.iter_mut().zip(&binds) {
            let unproven = process.pid != 0 && process.start_time == 0 && process.relay_pid == 0;

            if unproven
                && answer(bind, spec.readiness_path.as_deref(), PROBE_TIMEOUT)
                    .await
                    .is_none()
            {
                process.pid = 0;
            }
        }

        let dropped = Arc::new(AtomicU64::new(0));
        let supervisor = Supervisor::adopt(
            process_spec(spec, cwd, env, identity, log, dirs, relays, &dropped),
            spec.restart.policy(),
            instances,
            &processes,
        )?;

        let starting = supervisor
            .instances()
            .map(|instance| match instance.pid() {
                Some(_) => Starting::default(),
                None => Starting::marked(),
            })
            .collect();

        Ok(Self {
            readiness_path: spec.readiness_path.clone(),
            readiness_timeout: readiness_timeout(spec),
            starting,
            dirs: dirs.clone(),
            binds,
            supervisor: Box::new(supervisor),
            dropped,
            program: crate::processes::program_of(&spec.command).unwrap_or_default(),
            exits: std::collections::BTreeMap::new(),
            rolling: None,
            restart_failure: None,
            pending: Vec::new(),
        })
    }

    /// Wait until every instance answers its readiness probe, or one
    /// exits, or the time is up.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut ready = vec![false; self.binds.len()];
        let path = self.readiness_path.clone();
        let indexes: Vec<u32> = self
            .supervisor
            .instances()
            .map(supervisor::Instance::index)
            .collect();

        loop {
            for (position, bind) in self.binds.iter().enumerate() {
                if ready[position] {
                    continue;
                }

                if let Some(exit) = self.supervisor.try_exited(position)? {
                    anyhow::bail!("instance {} {exit} before it was ready", indexes[position]);
                }

                if probe(bind, path.as_deref(), PROBE_TIMEOUT).await {
                    ready[position] = true;
                }
            }

            if ready.iter().all(|ready| *ready) {
                return Ok(());
            }

            if Instant::now() >= deadline {
                let waiting = indexes
                    .iter()
                    .zip(&ready)
                    .find(|(_, ready)| !**ready)
                    .map(|(index, _)| *index)
                    .unwrap_or(0);
                anyhow::bail!(
                    "instance {waiting} was not ready within {} s{}",
                    timeout.as_secs(),
                    path.as_deref()
                        .map(|path| format!(": {path} did not answer 2xx or 3xx"))
                        .unwrap_or_default()
                );
            }

            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }

    /// What adoption found of each instance, by index; empty when this
    /// daemon started them.
    pub fn adopted(&self) -> Vec<(u32, supervisor::Adopted)> {
        self.supervisor
            .instances()
            .map(supervisor::Instance::index)
            .zip(self.supervisor.adopted().iter().cloned())
            .collect()
    }

    /// Reap instances that exited and start again those whose wait has
    /// passed, with what happened for the event stream.
    ///
    /// An instance whose exit is seen is marked starting, and one
    /// started again is probed beside the daemon's loop as the release's
    /// start probes its instances, every 100 ms until it answers or the
    /// readiness timeout passes; `probed` hears how that ended, and
    /// `probed` here takes the answer (issue 917).
    pub async fn check(&mut self, probed: &Probed) -> Vec<Report> {
        self.roll_on();
        let mut reports = self.supervisor.check().await;

        for outcome in reports.iter().filter_map(|report| report.outcome) {
            *self.exits.entry(outcome).or_default() += 1;
        }

        let pids: Vec<Option<u32>> = self
            .supervisor
            .instances()
            .map(supervisor::Instance::pid)
            .collect();
        let indexes: Vec<u32> = self
            .supervisor
            .instances()
            .map(supervisor::Instance::index)
            .collect();

        for (position, pid) in pids.into_iter().enumerate() {
            let starting = &mut self.starting[position];

            match pid {
                None if !starting.marked || starting.pid.is_some() => {
                    *starting = Starting::marked();
                }
                Some(pid) if starting.marked && starting.pid != Some(pid) => {
                    let bind = self.binds[position].clone();
                    let path = self.readiness_path.clone();
                    let timeout = self.readiness_timeout;
                    let probed = Arc::clone(probed);
                    let index = indexes[position];

                    *starting = Starting {
                        marked: true,
                        pid: Some(pid),
                        probe: Some(tokio::spawn(async move {
                            let answered = answers_within(&bind, path.as_deref(), timeout).await;
                            probed(index, pid, answered);
                        })),
                    };
                }
                _ => {}
            }
        }

        self.roll_after();
        reports.append(&mut self.pending);
        reports
    }

    /// Restart every instance by hand, one at a time (issue 393), with
    /// `grace` for each to stop in before it is killed. Gives each
    /// instance's index and pid, 0 for one with no process, which is
    /// started all the same. The next checks carry it out; `restart`
    /// says how far it has got, and `restart_failure` why it stopped
    /// early if it did. Refused while one is under way.
    ///
    /// With `only`, the one instance with that index is restarted, the
    /// same way (issue 1002).
    pub fn restart_all(
        &mut self,
        only: Option<u32>,
        grace: Duration,
    ) -> Result<Vec<(u32, u32)>, String> {
        if self.rolling.is_some() {
            return Err("a restart of its instances is already under way".to_owned());
        }

        let chosen: Vec<(usize, u32, u32)> = self
            .supervisor
            .instances()
            .enumerate()
            .filter(|(_, instance)| only.is_none_or(|index| instance.index() == index))
            .map(|(position, instance)| (position, instance.index(), instance.pid().unwrap_or(0)))
            .collect();

        if chosen.is_empty() {
            return Err(format!(
                "it has no instance {}; it has {}",
                only.unwrap_or(0),
                counted(self.supervisor.instances().count())
            ));
        }

        let instances: Vec<(u32, u32)> = chosen
            .iter()
            .map(|(_, index, pid)| (*index, *pid))
            .collect();

        self.restart_failure = None;
        self.rolling = Some(Rolling {
            queue: chosen.iter().map(|(position, _, _)| *position).collect(),
            current: None,
            total: instances.len() as u32,
            started_unix_ms: crate::model::now_unix_ms(),
            grace,
        });
        self.pending.push(Report {
            kind: RESTART_STARTED,
            level: "info",
            message: match only {
                Some(index) => format!("restarting instance {index}"),
                None => format!(
                    "restarting {} one at a time, each to answer its readiness probe before \
                     the next is stopped",
                    counted(instances.len())
                ),
            },
            outcome: None,
            index: None,
            change: supervisor::Change::None,
        });

        Ok(instances)
    }

    /// End the restart of every instance under way: the instances it
    /// has not reached stay as they are, and the one it is at is still
    /// started again. Gives the index and pid of each left; refused when
    /// no restart is under way.
    pub fn cancel_restart(&mut self) -> Result<Vec<(u32, u32)>, String> {
        let Some(rolling) = self.rolling.take() else {
            return Err("no restart of its instances is under way".to_owned());
        };

        let left: Vec<(u32, u32)> = self
            .supervisor
            .instances()
            .enumerate()
            .filter(|(position, _)| rolling.queue.contains(position))
            .map(|(_, instance)| (instance.index(), instance.pid().unwrap_or(0)))
            .collect();

        // One only marked, not yet stopped, goes back into service.

        if let Some((position, Phase::Marked)) = rolling.current {
            self.starting[position] = Starting::default();
        }

        self.pending.push(Report {
            kind: RESTART_FINISHED,
            level: "warning",
            message: format!(
                "the restart was cancelled with {} left as {}",
                counted(left.len()),
                if left.len() == 1 {
                    "it was"
                } else {
                    "they were"
                }
            ),
            outcome: None,
            index: None,
            change: supervisor::Change::None,
        });

        Ok(left)
    }

    /// How far the restart of every instance under way has got; none
    /// when there is none.
    pub fn restart(&self) -> Option<RestartProgress> {
        self.rolling.as_ref().map(|rolling| RestartProgress {
            total: rolling.total,
            left: rolling.queue.len() as u32 + u32::from(rolling.current.is_some()),
            started_unix_ms: rolling.started_unix_ms,
        })
    }

    /// Why the last restart of every instance stopped before it had
    /// restarted them all, if it did.
    pub fn restart_failure(&self) -> Option<&str> {
        self.restart_failure.as_deref()
    }

    /// Take the restart of every instance a step on, before the
    /// supervisor's check. The next instance is first marked starting,
    /// which takes it out of service at the edge with the snapshot the
    /// daemon pushes after this check, and only the check after that
    /// asks its process to stop, so that no request is sent to an
    /// instance that has been told to go.
    fn roll_on(&mut self) {
        let Some(rolling) = &mut self.rolling else {
            return;
        };

        match rolling.current {
            None => {
                let Some(position) = rolling.queue.pop_front() else {
                    let whole = rolling.total as usize == self.binds.len();
                    self.rolling = None;
                    self.pending.push(Report {
                        kind: RESTART_FINISHED,
                        level: "info",
                        message: match whole {
                            true => "every instance has been restarted and answers".to_owned(),
                            false => "the instance has been restarted and answers".to_owned(),
                        },
                        outcome: None,
                        index: None,
                        change: supervisor::Change::None,
                    });
                    return;
                };

                let pid = self
                    .supervisor
                    .instances()
                    .nth(position)
                    .and_then(supervisor::Instance::pid);

                self.starting[position] = Starting {
                    marked: true,
                    pid,
                    probe: None,
                };

                // One with no process has nothing to take out of
                // service, and is started at once.

                if pid.is_none() {
                    self.supervisor.restart(position, Instant::now());
                    rolling.current = Some((position, Phase::Stopping));
                } else {
                    rolling.current = Some((position, Phase::Marked));
                }
            }
            Some((position, Phase::Marked)) => {
                self.supervisor
                    .restart(position, Instant::now() + rolling.grace);
                rolling.current = Some((position, Phase::Stopping));
            }
            Some((_, Phase::Stopping | Phase::Probing(_))) => {}
        }
    }

    /// Look at the instance being restarted after the supervisor's
    /// check: its new process is the one to wait for, and a start that
    /// failed, or a new process that exited again, ends the restart,
    /// since stopping the next instance would leave the release with
    /// one fewer that serves.
    fn roll_after(&mut self) {
        let Some((position, phase)) = self.rolling.as_ref().and_then(|rolling| rolling.current)
        else {
            return;
        };

        let Some(instance) = self.supervisor.instances().nth(position) else {
            return;
        };

        let index = instance.index();

        match (phase, instance.pid()) {
            (Phase::Stopping, Some(pid)) if !instance.stopping() => {
                if let Some(rolling) = &mut self.rolling {
                    rolling.current = Some((position, Phase::Probing(pid)));
                }
            }
            (Phase::Stopping, None) if instance.quick_exits() > 0 => {
                self.stop_rolling(format!("instance {index} could not be started again"));
            }
            (Phase::Probing(pid), now) if now != Some(pid) => {
                self.stop_rolling(format!("instance {index} exited again before it answered"));
            }
            _ => {}
        }
    }

    /// End the restart of every instance early, with why.
    fn stop_rolling(&mut self, why: String) {
        let Some(rolling) = self.rolling.take() else {
            return;
        };

        self.pending.push(Report {
            kind: RESTART_FINISHED,
            level: "error",
            message: format!(
                "the restart stopped: {why}; {} left as {}",
                counted(rolling.queue.len()),
                if rolling.queue.len() == 1 {
                    "it was"
                } else {
                    "they were"
                }
            ),
            outcome: None,
            index: None,
            change: supervisor::Change::None,
        });
        self.restart_failure = Some(why);
    }

    /// Take the outcome of the probe of the instance at `index`, whose
    /// process was `pid`: the mark is cleared, whether it answered or the
    /// readiness timeout passed, which is reported as a warning since
    /// the instance is then left to the edge's ejection. A probe of a
    /// process the instance no longer has changes nothing.
    pub fn probed(&mut self, index: u32, pid: u32, answered: bool) -> Option<Report> {
        let position = self
            .supervisor
            .instances()
            .position(|instance| instance.index() == index)?;
        let starting = &mut self.starting[position];

        // A probe the relay's report of the process's end stopped, whose
        // outcome was already on its way, says nothing now.

        if !starting.marked || starting.pid != Some(pid) || starting.probe.is_none() {
            return None;
        }

        *starting = Starting::default();

        // The restart of every instance waits on this probe: an answer
        // lets it go on to the next, and none ends it.

        if let Some(rolling) = &mut self.rolling
            && rolling.current == Some((position, Phase::Probing(pid)))
        {
            if answered {
                rolling.current = None;
            } else {
                self.stop_rolling(format!(
                    "instance {index} did not answer its readiness probe within {}",
                    muru_config::format_duration(self.readiness_timeout)
                ));
            }
        }

        if answered {
            return Some(Report {
                kind: READY_EVENT,
                level: "info",
                message: format!("instance {index} answers and takes requests again"),
                outcome: None,
                index: None,
                change: supervisor::Change::None,
            });
        }

        Some(Report {
            kind: READY_EVENT,
            level: "warning",
            message: format!(
                "instance {index} did not answer within {}{}; it takes requests again, \
                 and is set aside if they fail",
                muru_config::format_duration(self.readiness_timeout),
                self.readiness_path
                    .as_deref()
                    .map(|path| format!(" ({path} did not answer 2xx or 3xx)"))
                    .unwrap_or_default()
            ),
            outcome: None,
            index: None,
            change: supervisor::Change::None,
        })
    }

    /// Issue 922: the relay of the instance at `index` reports that its
    /// process `pid` has ended. The instance is marked starting at once,
    /// and its probe, if one ran, is stopped; the exit itself, its wait
    /// and the start in its place are left to the check that sees the
    /// relay gone, since a new relay for the index takes the old one's
    /// control socket and record. Says whether the mark is new. A report
    /// about a process the instance no longer has changes nothing.
    pub fn ended(&mut self, index: u32, pid: u32) -> bool {
        let Some(position) = self
            .supervisor
            .instances()
            .position(|instance| instance.index() == index && instance.pid() == Some(pid))
        else {
            return false;
        };

        let starting = &mut self.starting[position];
        let new = !starting.marked;

        *starting = Starting {
            marked: true,
            pid: Some(pid),
            probe: None,
        };

        new
    }

    /// The addresses of the instances marked starting, which the edge
    /// keeps out of service without ejecting them.
    pub fn starting(&self) -> Vec<String> {
        self.binds
            .iter()
            .zip(&self.starting)
            .filter(|(_, starting)| starting.marked)
            .map(|(bind, _)| bind.address())
            .collect()
    }

    /// Have each instance's relay open the release log again by name;
    /// gives, for each relay told, what changes once it says it has.
    pub fn reopen_log(&self) -> Vec<tokio::sync::watch::Receiver<u64>> {
        self.supervisor.reopen()
    }

    /// Lines of the instances' output their relays have dropped so far.
    pub fn output_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Every instance's address, as the snapshot names them.
    pub fn addresses(&self) -> Vec<String> {
        self.binds.iter().map(Bind::address).collect()
    }

    /// The program the instances' command runs, as the process view
    /// names it (issue 841): `gunicorn` for `gunicorn --workers 2 ...`.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// How many instances have a process right now.
    pub fn running(&self) -> usize {
        self.supervisor.running()
    }

    /// The instances' exits this daemon has seen, by outcome: `clean`
    /// or `failed`.
    pub fn exits(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.exits
            .iter()
            .map(|(outcome, count)| (outcome.as_str(), *count))
    }

    /// Each instance as `release show` gives it.
    pub fn status(&self) -> Vec<InstanceStatus> {
        let now_unix_ms = crate::model::now_unix_ms();

        self.supervisor
            .instances()
            .zip(&self.binds)
            .zip(&self.starting)
            .map(|((instance, bind), starting)| {
                let (state, next_start_unix_ms) = match instance.state() {
                    supervisor::State::Running | supervisor::State::Stopping if starting.marked => {
                        ("starting", 0)
                    }
                    supervisor::State::Running | supervisor::State::Stopping => ("running", 0),
                    supervisor::State::Stopped => ("failed", 0),
                    supervisor::State::Backoff { wait } => {
                        ("backoff", now_unix_ms + wait.as_millis() as u64)
                    }
                    supervisor::State::Exited => ("exited", 0),
                    supervisor::State::Failed => ("failed", 0),
                };

                InstanceStatus {
                    index: instance.index(),
                    pid: instance.pid().unwrap_or(0),
                    address: bind.address(),
                    started_unix_ms: match instance.pid() {
                        Some(_) => instance.started_unix_ms(),
                        None => 0,
                    },
                    state,
                    next_start_unix_ms,
                    quick_exits: instance.quick_exits(),
                }
            })
            .collect()
    }

    /// The instances as a record for the next daemon.
    pub fn record(&self) -> ServersRecord {
        ServersRecord {
            instances: self
                .supervisor
                .record()
                .instances
                .into_iter()
                .zip(&self.binds)
                .map(|(process, bind)| InstanceRecord {
                    index: process.index,
                    pid: process.pid,
                    address: bind.address(),
                    started_unix_ms: process.started_unix_ms,
                    quick_exits: process.quick_exits,
                    left: process.left,
                    start_time: process.start_time,
                    relay_pid: process.relay_pid,
                    relay_start_time: process.relay_start_time,
                })
                .collect(),
        }
    }

    /// The directories the instances' sockets and files are in.
    pub fn dirs(&self) -> &HostDirs {
        &self.dirs
    }

    /// Stop every instance: the graceful signal, then SIGKILL for one
    /// still there after `grace`, and the sockets and run directory
    /// removed.
    pub async fn stop(self, grace: Duration) {
        self.stop_until(std::time::Instant::now() + grace).await;
    }

    /// Stop the instances as `stop` does, killing what is still there at
    /// `kill_at`.
    pub async fn stop_until(self, kill_at: std::time::Instant) {
        self.supervisor.stop_until(Instant::from_std(kill_at)).await;
        self.dirs.remove();
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        self.supervisor.release();
    }
}

/// A count of instances as a phrase: `1 instance`, `3 instances`.
fn counted(count: usize) -> String {
    format!("{count} instance{}", if count == 1 { "" } else { "s" })
}

/// How long the release's instances get to be ready.
fn readiness_timeout(spec: &HttpServerSpec) -> Duration {
    Duration::from_millis(spec.readiness_timeout_ms)
}

/// Whether an instance answers its readiness probe within `timeout`,
/// probed every 100 ms as the release's start probes it.
async fn answers_within(bind: &Bind, path: Option<&str>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    loop {
        let left = deadline.saturating_duration_since(Instant::now());

        if probe(bind, path, PROBE_TIMEOUT.min(left)).await {
            return true;
        }

        if Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(PROBE_INTERVAL.min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

/// A socket path or a free loopback port for each of `count` instances.
/// A free port is one the system gives to a listener that asks for any.
/// Every such listener is held open until the last port is known, so
/// that no two instances of a release are given the same one.
fn allocate(bind: ServerBind, run_dir: &Path, count: u32) -> anyhow::Result<Vec<Bind>> {
    match bind {
        ServerBind::Unix => Ok((0..count)
            .map(|index| Bind::Unix(run_dir.join(format!("server-{index}.sock"))))
            .collect()),
        ServerBind::Port => {
            let listeners = (0..count)
                .map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
                .collect::<std::io::Result<Vec<_>>>()
                .context("finding a free loopback port")?;
            listeners
                .iter()
                .map(|listener| Ok(Bind::Port(listener.local_addr()?.port())))
                .collect()
        }
    }
}

/// What the supervisor needs to know of every instance of a release.
#[allow(clippy::too_many_arguments)]
fn process_spec(
    spec: &HttpServerSpec,
    cwd: &Path,
    env: Vec<(String, String)>,
    identity: Option<Identity>,
    log: &Path,
    dirs: &HostDirs,
    relays: Option<Relays>,
    dropped: &Arc<AtomicU64>,
) -> supervisor::Spec {
    supervisor::Spec {
        cwd: cwd.to_path_buf(),
        env,
        identity,
        log: log.to_path_buf(),
        relay: relays.map(|relays| supervisor::RelaySpec {
            executable: relays.executable,
            dirs: dirs.clone(),
            settings: relays.settings,
            release: relays.release,
            kind: "instance",
            watcher: relays.watcher,
            dropped: Arc::clone(dropped),
        }),
        sink: None,
        stop_signal: stop_signal(spec),
        stop_group: false,
        events: EVENTS,
    }
}

/// One instance for the supervisor: the command with the instance's
/// address filled in, the variables that tell the server where to
/// listen, and the socket file a crashed instance leaves behind.
fn instance_spec(spec: &HttpServerSpec, index: u32, bind: &Bind) -> InstanceSpec {
    let mut env = vec![
        ("MURU_HTTP_BIND".to_owned(), bind.address()),
        ("MURU_SERVER_INDEX".to_owned(), index.to_string()),
    ];

    let stale = match bind {
        Bind::Unix(path) => vec![path.clone()],
        Bind::Port(port) => {
            env.push(("PORT".to_owned(), port.to_string()));
            Vec::new()
        }
    };

    InstanceSpec {
        command: arguments_for(spec.profile, &spec.command, bind),
        env,
        stale,
    }
}

/// The signal that stops an instance gracefully: the profile's, or the
/// one the release names.
fn stop_signal(spec: &HttpServerSpec) -> i32 {
    let name = spec.stop_signal.as_deref().unwrap_or(match spec.profile {
        ServerProfile::Gunicorn
        | ServerProfile::Uvicorn
        | ServerProfile::Granian
        | ServerProfile::ModWsgiExpress
        | ServerProfile::Uwsgi
        | ServerProfile::Generic => "TERM",
    });

    match name {
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "HUP" => libc::SIGHUP,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "WINCH" => libc::SIGWINCH,
        _ => libc::SIGTERM,
    }
}

/// The command with its address filled in: `{bind}` becomes the
/// address in the server's own form, `{socket}` the socket path and
/// `{port}` the port, and a command that names none of them gets the
/// profile's own flags appended.
fn arguments_for(profile: ServerProfile, command: &[String], bind: &Bind) -> Vec<String> {
    let address = match (profile, bind) {
        (
            ServerProfile::Uvicorn | ServerProfile::Granian | ServerProfile::Uwsgi,
            Bind::Unix(path),
        ) => path.display().to_string(),
        (_, bind) => bind.address(),
    };

    let socket = match bind {
        Bind::Unix(path) => path.display().to_string(),
        Bind::Port(_) => String::new(),
    };

    let port = match bind {
        Bind::Port(port) => port.to_string(),
        Bind::Unix(_) => String::new(),
    };

    let mentioned = command.iter().any(|argument| {
        argument.contains("{bind}") || argument.contains("{socket}") || argument.contains("{port}")
    });
    let mut arguments: Vec<String> = command
        .iter()
        .map(|argument| {
            argument
                .replace("{bind}", &address)
                .replace("{socket}", &socket)
                .replace("{port}", &port)
        })
        .collect();

    // uWSGI 2.0 takes TERM as a brutal reload and drops the requests in
    // flight, so its profile has a master whose hook makes TERM a
    // graceful stop instead, whatever the command binds with. Both flags
    // are harmless given twice.

    if profile == ServerProfile::Uwsgi {
        arguments.extend([
            "--master".to_owned(),
            "--hook-master-start".to_owned(),
            "unix_signal:15 gracefully_kill_them_all".to_owned(),
        ]);
    }

    if mentioned {
        return arguments;
    }

    match (profile, bind) {
        (ServerProfile::Gunicorn, _) => arguments.extend(["--bind".to_owned(), address]),
        (ServerProfile::Uwsgi, _) => arguments.extend(["--http-socket".to_owned(), address]),
        (ServerProfile::Uvicorn | ServerProfile::Granian, Bind::Unix(_)) => {
            arguments.extend(["--uds".to_owned(), socket]);
        }
        (
            ServerProfile::Uvicorn | ServerProfile::Granian | ServerProfile::ModWsgiExpress,
            Bind::Port(_),
        ) => {
            arguments.extend([
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                port,
            ]);
        }
        (ServerProfile::ModWsgiExpress, Bind::Unix(_)) | (ServerProfile::Generic, _) => {}
    }

    arguments
}

/// Whether an instance answers: with a path, a GET of it answered 2xx
/// or 3xx; without one, any HTTP response at all.
async fn probe(bind: &Bind, path: Option<&str>, timeout: Duration) -> bool {
    match answer(bind, path, timeout).await {
        Some(status) => path.is_none() || (200..400).contains(&status),
        None => false,
    }
}

/// The status an instance answers a GET of the path with, `/` when
/// there is none, or nothing when no HTTP response comes in time.
async fn answer(bind: &Bind, path: Option<&str>, timeout: Duration) -> Option<u16> {
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: localhost\r\nUser-Agent: muru-readiness\r\n\r\n",
        path.unwrap_or("/")
    );
    let answered = tokio::time::timeout(timeout, async {
        match bind {
            Bind::Unix(socket) => {
                let mut stream = UnixStream::connect(socket).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
            Bind::Port(port) => {
                let mut stream = TcpStream::connect(("127.0.0.1", *port)).await.ok()?;
                stream.write_all(request.as_bytes()).await.ok()?;
                status_of(&mut stream).await
            }
        }
    })
    .await;

    answered.ok().flatten()
}

/// The status of the response a stream carries, from its status line.
async fn status_of<S: AsyncRead + Unpin>(stream: &mut S) -> Option<u16> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];

    while head.len() < 256 {
        if stream.read(&mut byte).await.ok()? == 0 {
            break;
        }

        if byte[0] == b'\n' {
            break;
        }

        head.push(byte[0]);
    }

    let line = String::from_utf8_lossy(&head);
    let mut words = line.split_whitespace();
    let version = words.next()?;

    if !version.starts_with("HTTP/") {
        return None;
    }

    words.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn placeholders_take_the_address_in_the_servers_form() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "--bind", "{bind}", "app:app"]),
                &unix
            ),
            command(&["gunicorn", "--bind", "unix:/run/s.sock", "app:app"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app", "--uds", "{bind}"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Generic,
                &command(&["server", "--listen={bind}", "--port", "{port}"]),
                &port
            ),
            command(&["server", "--listen=127.0.0.1:8001", "--port", "8001"])
        );
    }

    #[test]
    fn a_command_without_placeholders_gets_the_profiles_flags() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        assert_eq!(
            arguments_for(
                ServerProfile::Gunicorn,
                &command(&["gunicorn", "app:app"]),
                &port
            ),
            command(&["gunicorn", "app:app", "--bind", "127.0.0.1:8001"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uvicorn,
                &command(&["uvicorn", "app:app"]),
                &unix
            ),
            command(&["uvicorn", "app:app", "--uds", "/run/s.sock"])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Granian,
                &command(&["granian", "app:app"]),
                &port
            ),
            command(&[
                "granian",
                "app:app",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(
                ServerProfile::ModWsgiExpress,
                &command(&["mod_wsgi-express", "start-server", "app.wsgi"]),
                &port
            ),
            command(&[
                "mod_wsgi-express",
                "start-server",
                "app.wsgi",
                "--host",
                "127.0.0.1",
                "--port",
                "8001"
            ])
        );
        assert_eq!(
            arguments_for(ServerProfile::Generic, &command(&["server"]), &port),
            command(&["server"])
        );
    }

    #[test]
    fn uwsgi_binds_an_http_socket_and_stops_gracefully_on_term() {
        let unix = Bind::Unix(PathBuf::from("/run/s.sock"));
        let port = Bind::Port(8001);
        let graceful = [
            "--master",
            "--hook-master-start",
            "unix_signal:15 gracefully_kill_them_all",
        ];
        assert_eq!(
            arguments_for(
                ServerProfile::Uwsgi,
                &command(&["uwsgi", "--module", "app:application"]),
                &unix
            ),
            command(
                &[
                    &["uwsgi", "--module", "app:application"][..],
                    &graceful,
                    &["--http-socket", "/run/s.sock"],
                ]
                .concat()
            )
        );
        assert_eq!(
            arguments_for(
                ServerProfile::Uwsgi,
                &command(&["uwsgi", "--http-socket", "{bind}", "app.ini"]),
                &port
            ),
            command(
                &[
                    &["uwsgi", "--http-socket", "127.0.0.1:8001", "app.ini"][..],
                    &graceful
                ]
                .concat()
            )
        );
    }

    #[test]
    fn addresses_round_trip_through_records() {
        for bind in [Bind::Unix(PathBuf::from("/run/s.sock")), Bind::Port(8001)] {
            assert_eq!(Bind::parse(&bind.address()), Some(bind));
        }

        assert_eq!(Bind::parse("10.0.0.1:80"), None);
    }

    #[test]
    fn the_instances_of_a_release_get_addresses_of_their_own() {
        let run_dir = Path::new("/run/r1");
        let ports = allocate(ServerBind::Port, run_dir, 64).unwrap();
        let distinct: std::collections::BTreeSet<String> =
            ports.iter().map(Bind::address).collect();
        assert_eq!(distinct.len(), 64);
        assert!(
            ports
                .iter()
                .all(|bind| matches!(bind, Bind::Port(port) if *port != 0))
        );

        assert_eq!(
            allocate(ServerBind::Unix, run_dir, 2).unwrap(),
            [
                Bind::Unix(PathBuf::from("/run/r1/server-0.sock")),
                Bind::Unix(PathBuf::from("/run/r1/server-1.sock")),
            ]
        );
    }

    #[tokio::test]
    async fn an_instance_recorded_with_no_start_time_is_adopted_only_if_its_address_answers() {
        // The record names a process that is running, a child of the
        // test, and has no start time for it, so the pid is all there
        // is to say the process is the instance.

        let dir = tempfile::tempdir().unwrap();
        let mut bystander = std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .unwrap();
        let spec = HttpServerSpec {
            profile: ServerProfile::Generic,
            command: command(&["/bin/sleep", "600"]),
            bind: ServerBind::Port,
            instances: 1,
            readiness_path: None,
            readiness_timeout_ms: 1000,
            stop_signal: None,
            restart: crate::model::RestartSpec::default(),
        };

        // Something answers HTTP at one address, and nothing listens at
        // the other.

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let answering = Bind::Port(listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await;
            }
        });
        let silent = Bind::Unix(dir.path().join("nothing.sock"));

        for (bind, adopted) in [(answering, true), (silent, false)] {
            let record = ServersRecord {
                instances: vec![InstanceRecord {
                    index: 0,
                    pid: bystander.id(),
                    address: bind.address(),
                    started_unix_ms: 0,
                    quick_exits: 0,
                    left: None,
                    start_time: 0,
                    relay_pid: 0,
                    relay_start_time: 0,
                }],
            };
            let servers = Servers::adopt(
                &spec,
                &HostDirs::together(dir.path()),
                dir.path(),
                Vec::new(),
                None,
                &dir.path().join("output.log"),
                None,
                &record,
            )
            .await
            .unwrap();
            assert_eq!(
                servers.record().instances[0].pid,
                if adopted { bystander.id() } else { 0 }
            );
            assert_eq!(servers.running(), usize::from(adopted));
            servers.release();
        }

        // Neither adoption signalled the process.

        assert_eq!(bystander.try_wait().unwrap(), None);
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[tokio::test]
    async fn the_probe_reads_a_status_line() {
        let mut stream = std::io::Cursor::new(b"HTTP/1.1 503 Service Unavailable\r\n\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, Some(503));
        let mut stream = std::io::Cursor::new(b"nonsense\r\n".to_vec());
        assert_eq!(status_of(&mut stream).await, None);
    }
}
