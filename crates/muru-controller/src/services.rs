//! Services (design specification section 9.17): named commands the
//! daemon keeps running beside the applications, which take no requests
//! from the edge. Their processes are run by the supervisor managed HTTP
//! servers share (`supervisor.rs`), under the one restart policy (ADR
//! 0090), each instance with a log relay of its own as its parent.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use muru_config::Service as Table;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::time::Instant;

use crate::host::Identity;
use crate::locations::HostDirs;
use crate::supervisor::{
    self, Change, EventNames, InstanceSpec, Left, Report, RestartPolicy, State, Supervisor,
};

/// The events a service's instances are reported under.
const EVENTS: EventNames = EventNames {
    exited: "service.exited",
    started: "service.started",
    failed: "service.failed",
    crash_looping: "service.crash_looping",
    gave_up: "service.gave_up",
    stopped: "service.stopped",
    killed: "service.killed",
    daemonised: "service.daemonised",
};

/// The event of an instance that passed its readiness check.
const READY_EVENT: &str = "service.ready";

/// The event of an instance that did not in time.
const NOT_READY_EVENT: &str = "service.not_ready";

/// The record a service's instances are kept in for the next daemon,
/// among the service's runtime files.
pub const RECORD: &str = "service.toml";

/// How many of an instance's exits are kept for `service status`.
const HISTORY: usize = 5;

/// How long one readiness probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The part of each wait that is added to it at random (section 9.17).
const JITTER: f32 = 0.1;

/// How many instances a table declares.
pub fn instances_of(table: &Table) -> u32 {
    table.instances.unwrap_or(1).max(1)
}

/// Whether a table's service starts with the daemon.
pub fn autostart_of(table: &Table) -> bool {
    table.autostart.unwrap_or(true)
}

/// A count of instances as a phrase: `1 instance`, `3 instances`.
pub fn counted(count: u32) -> String {
    format!("{count} instance{}", if count == 1 { "" } else { "s" })
}

/// What a changed definition does to a service that runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Difference {
    /// The keys whose change stops every instance and starts it again:
    /// what a process is started with cannot be changed under it.
    pub restart: Vec<&'static str>,
    /// The count of instances before and after, when it changed.
    pub instances: Option<(u32, u32)>,
    /// The keys whose change is in force at once, with nothing restarted.
    pub in_place: Vec<&'static str>,
    /// Whether the service starts with the daemon, when that changed.
    pub autostart: Option<bool>,
}

impl Difference {
    /// What differs between two definitions of one service.
    pub fn between(old: &Table, new: &Table) -> Self {
        let mut difference = Self::default();

        for (key, changed) in [
            ("command", old.command != new.command),
            ("cwd", old.cwd != new.cwd),
            ("env", old.env != new.env),
            ("user", old.user != new.user),
            ("group", old.group != new.group),
            ("umask", old.umask != new.umask),
            ("readiness", old.readiness != new.readiness),
            ("stop_signal", old.stop_signal != new.stop_signal),
            ("stop_group", old.stop_group != new.stop_group),
        ] {
            if changed {
                difference.restart.push(key);
            }
        }

        for (key, changed) in [
            ("restart", old.restart != new.restart),
            ("stop_timeout", old.stop_timeout != new.stop_timeout),
            ("reload_signal", old.reload_signal != new.reload_signal),
            ("description", old.description != new.description),
        ] {
            if changed {
                difference.in_place.push(key);
            }
        }

        let (before, after) = (instances_of(old), instances_of(new));

        if before != after {
            difference.instances = Some((before, after));
        }

        if autostart_of(old) != autostart_of(new) {
            difference.autostart = Some(autostart_of(new));
        }

        difference
    }

    /// The difference as a plan says it, with what it does.
    pub fn describe(&self, new: &Table) -> String {
        let mut parts = Vec::new();

        if !self.restart.is_empty() {
            let count = instances_of(new);
            parts.push(format!(
                "{} changed; its {} {} restarted",
                self.restart.join(", "),
                counted(count),
                if count == 1 { "is" } else { "are" }
            ));
        } else if let Some((before, after)) = self.instances {
            let by = before.abs_diff(after);
            parts.push(format!(
                "instances {before} to {after}; {} {} {}",
                counted(by),
                if by == 1 { "is" } else { "are" },
                if after > before { "started" } else { "stopped" }
            ));
        }

        if !self.in_place.is_empty() {
            parts.push(format!(
                "{} changed; nothing is restarted",
                self.in_place.join(", ")
            ));
        }

        match self.autostart {
            Some(true) => parts.push("autostart on; it is started".to_owned()),
            Some(false) => parts.push("autostart off; it keeps running until stopped".to_owned()),
            None => {}
        }

        parts.join("; ")
    }
}

/// A service's definition for a plan: for a new one its command, and
/// for a changed one what the change does to its instances.
pub fn describe_change(recorded: Option<&Table>, table: &Table) -> String {
    let Some(recorded) = recorded else {
        let mut text = table.command.join(" ");
        let count = instances_of(table);

        if count > 1 {
            text.push_str(&format!(", {}", counted(count)));
        }

        if !autostart_of(table) {
            text.push_str(", not started automatically");
        }

        return text;
    };

    Difference::between(recorded, table).describe(table)
}

/// The number of a signal a table names; TERM for none it knows.
pub fn signal_number(name: &str) -> i32 {
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

/// A wait a table gives, already checked, or its default.
fn wait_of(text: &Option<String>, default: Duration) -> Duration {
    text.as_deref()
        .and_then(|text| muru_config::parse_duration(text).ok())
        .unwrap_or(default)
}

/// How long a table's instances have to stop in.
pub fn stop_timeout_of(table: &Table) -> Duration {
    wait_of(
        &table.stop_timeout,
        muru_config::DEFAULT_SERVICE_STOP_TIMEOUT,
    )
}

/// The restart policy a table gives, with the wait lengthened at random
/// and a program that puts itself in the background named.
fn policy_of(table: &Table) -> RestartPolicy {
    RestartPolicy {
        jitter: JITTER,
        foreground: true,
        ..crate::config::restart_spec_from(table.restart.as_ref()).policy()
    }
}

/// How an instance is known to be up, with its placeholders still in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Form {
    After(Duration),
    Tcp(String),
    Unix(String),
    Http(String),
    Command(Vec<String>),
}

/// A table's readiness: the form, how long an instance has, and how
/// often a form that probes tries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Readiness {
    form: Form,
    timeout: Duration,
    interval: Duration,
}

impl Readiness {
    fn of(table: &Table) -> Self {
        let given = table.readiness.clone().unwrap_or_default();

        let form = if let Some(address) = given.tcp {
            Form::Tcp(address)
        } else if let Some(path) = given.unix {
            Form::Unix(path)
        } else if let Some(url) = given.http {
            Form::Http(url)
        } else if let Some(command) = given.command {
            Form::Command(command)
        } else {
            Form::After(wait_of(
                &given.after,
                muru_config::DEFAULT_SERVICE_READY_AFTER,
            ))
        };

        Self {
            form,
            timeout: wait_of(
                &given.timeout,
                muru_config::DEFAULT_SERVICE_READINESS_TIMEOUT,
            ),
            interval: wait_of(
                &given.interval,
                muru_config::DEFAULT_SERVICE_READINESS_INTERVAL,
            ),
        }
    }

    /// The readiness as `service status` words it.
    fn describe(&self) -> String {
        let within = muru_config::format_duration(self.timeout);

        match &self.form {
            Form::After(wait) => format!("after {}", muru_config::format_duration(*wait)),
            Form::Tcp(address) => format!("tcp {address} within {within}"),
            Form::Unix(path) => format!("unix {path} within {within}"),
            Form::Http(url) => format!("http {url} within {within}"),
            Form::Command(command) => format!("command {} within {within}", command.join(" ")),
        }
    }
}

/// What a probe that runs a command needs of the service.
#[derive(Clone)]
struct ProbeContext {
    cwd: PathBuf,
    env: Vec<(String, String)>,
    identity: Option<Identity>,
}

/// How long an instance takes to become ready, when it does within its
/// time: the form tried until it passes or the time is up.
async fn ready_within(readiness: Readiness, fill: Fill, context: ProbeContext) -> Option<Duration> {
    let began = Instant::now();

    probe_until_ready(readiness, fill, context)
        .await
        .then(|| Instant::now().saturating_duration_since(began))
}

/// Whether an instance becomes ready within its time.
async fn probe_until_ready(readiness: Readiness, fill: Fill, context: ProbeContext) -> bool {
    let deadline = Instant::now() + readiness.timeout;

    if let Form::After(wait) = readiness.form {
        tokio::time::sleep(wait).await;
        return true;
    }

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let within = PROBE_TIMEOUT.min(left);

        let passed = match &readiness.form {
            Form::After(_) => true,
            Form::Tcp(address) => {
                let address = fill.text(address);
                tokio::time::timeout(within, tokio::net::TcpStream::connect(address))
                    .await
                    .is_ok_and(|connected| connected.is_ok())
            }
            Form::Unix(path) => {
                let path = fill.text(path);
                tokio::time::timeout(within, tokio::net::UnixStream::connect(path))
                    .await
                    .is_ok_and(|connected| connected.is_ok())
            }
            Form::Http(url) => tokio::time::timeout(within, http_answers(fill.text(url)))
                .await
                .unwrap_or(false),
            Form::Command(command) => {
                let command: Vec<String> = command.iter().map(|word| fill.text(word)).collect();
                tokio::time::timeout(within, command_passes(command, &fill, context.clone()))
                    .await
                    .unwrap_or(false)
            }
        };

        if passed {
            return true;
        }

        if Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(
            readiness
                .interval
                .min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}

/// Whether a GET of an `http://host:port/path` URL answers 2xx or 3xx.
async fn http_answers(url: String) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or(&url);

    let (address, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };

    let Ok(mut stream) = tokio::net::TcpStream::connect(address).await else {
        return false;
    };

    let request = format!("GET {path} HTTP/1.0\r\nHost: {address}\r\nUser-Agent: muru\r\n\r\n");

    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }

    // The status is in the first few bytes: `HTTP/1.1 200`.

    let mut head = [0u8; 12];

    if stream.read_exact(&mut head).await.is_err() {
        return false;
    }

    head.starts_with(b"HTTP/") && matches!(head[9], b'2' | b'3')
}

/// Whether a readiness command, run as the service's user with the
/// instance's environment, exits 0.
async fn command_passes(command: Vec<String>, fill: &Fill, context: ProbeContext) -> bool {
    let mut probe = tokio::process::Command::new(&command[0]);
    probe
        .args(&command[1..])
        .current_dir(&context.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_clear()
        .envs(context.env.iter().cloned())
        .envs(fill.variables())
        .kill_on_drop(true);
    crate::host::own_session(&mut probe, context.identity);

    match probe.status().await {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// What the placeholders of one instance stand for.
#[derive(Debug, Clone)]
struct Fill {
    name: String,
    index: u32,
    run_dir: PathBuf,
}

impl Fill {
    /// A text with `{index}` and `{run_dir}` filled in. A table is
    /// checked before it is recorded, so a text that cannot be filled is
    /// given as it is.
    fn text(&self, text: &str) -> String {
        let index = self.index.to_string();
        let run_dir = self.run_dir.display().to_string();

        muru_config::fill_placeholders(text, &[("index", &index), ("run_dir", &run_dir)])
            .unwrap_or_else(|_| text.to_owned())
    }

    /// The variables every instance is given.
    fn variables(&self) -> Vec<(String, String)> {
        vec![
            ("MURU_SERVICE".to_owned(), self.name.clone()),
            ("MURU_SERVICE_INDEX".to_owned(), self.index.to_string()),
            (
                "MURU_SERVICE_RUN_DIR".to_owned(),
                self.run_dir.display().to_string(),
            ),
        ]
    }
}

/// What the daemon works out for a service before it runs: where it
/// lives, whom it runs as and what its instances start from.
#[derive(Clone)]
pub struct Setting {
    /// The service's name.
    pub name: String,
    /// Its definition.
    pub table: Table,
    /// Its sockets and runtime files.
    pub dirs: HostDirs,
    /// The directory its command runs in.
    pub cwd: PathBuf,
    /// What every instance starts from: the daemon's clean base.
    pub base: Vec<(String, String)>,
    /// The service's own variables, secrets resolved, placeholders not
    /// yet filled.
    pub env: Vec<(String, String)>,
    /// Whom the instances run as, when not as the daemon.
    pub identity: Option<Identity>,
    /// The service's log.
    pub log: PathBuf,
    /// How each instance's log relay is started; none starts the
    /// instances directly, as the tests do.
    pub relays: Option<crate::servers::Relays>,
    /// For a project's service, the application and the release it is
    /// of; none for a node's.
    pub owner: Option<(String, String)>,
}

impl Setting {
    fn fill(&self, index: u32) -> Fill {
        Fill {
            name: self.name.clone(),
            index,
            run_dir: self.dirs.sockets.join(index.to_string()),
        }
    }

    /// One instance for the supervisor: the command and the service's
    /// variables with the instance's own filled in. Its own directory
    /// under the runtime directory is made, and given to its user.
    fn instance(&self, index: u32) -> InstanceSpec {
        let fill = self.fill(index);
        let _ = std::fs::create_dir_all(&fill.run_dir);

        if let Some(identity) = self.identity {
            let _ = identity.own(&fill.run_dir);
        }

        let mut env: Vec<(String, String)> = self
            .env
            .iter()
            .map(|(name, value)| (name.clone(), fill.text(value)))
            .collect();
        env.extend(fill.variables());

        InstanceSpec {
            command: self
                .table
                .command
                .iter()
                .map(|word| fill.text(word))
                .collect(),
            env,
            stale: Vec::new(),
        }
    }

    fn instances(&self) -> Vec<InstanceSpec> {
        (0..instances_of(&self.table))
            .map(|index| self.instance(index))
            .collect()
    }

    /// What the supervisor needs to know of every instance.
    fn spec(&self, dropped: &Arc<AtomicU64>) -> supervisor::Spec {
        supervisor::Spec {
            cwd: self.cwd.clone(),
            env: self.base.clone(),
            identity: self.identity,
            log: self.log.clone(),
            relay: self.relays.clone().map(|relays| supervisor::RelaySpec {
                executable: relays.executable,
                dirs: self.dirs.clone(),
                settings: relays.settings,
                release: relays.release,
                kind: "service",
                watcher: relays.watcher,
                dropped: Arc::clone(dropped),
            }),
            stop_signal: signal_number(self.table.stop_signal.as_deref().unwrap_or("TERM")),
            stop_group: self.table.stop_group.unwrap_or(false),
            events: EVENTS,
        }
    }

    fn probe_context(&self) -> ProbeContext {
        ProbeContext {
            cwd: self.cwd.clone(),
            env: self.base.iter().chain(&self.env).cloned().collect(),
            identity: self.identity,
        }
    }
}

/// How a service comes to be under this daemon.
pub enum Begin<'a> {
    /// Every instance is started.
    Start,
    /// Nothing is started: the service is not started automatically, or
    /// was stopped by hand.
    Idle,
    /// The instances a previous daemon recorded are taken over.
    Adopt(&'a supervisor::Record),
}

/// The readiness of one instance's process.
#[derive(Default)]
struct Probe {
    /// The process it is about.
    pid: Option<u32>,
    /// Whether it has passed.
    ready: bool,
    /// The check under way, which gives how long the instance took to
    /// be ready when it was in time.
    task: Option<tokio::task::JoinHandle<Option<Duration>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// One instance as `service status` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceStatus {
    /// The instance's index, from 0.
    pub index: u32,
    /// Its pid; 0 without a process.
    pub pid: u32,
    /// `stopped`, `starting`, `running`, `backoff`, `stopping`, `exited`
    /// or `failed`.
    pub state: &'static str,
    /// When its process was started, Unix milliseconds; 0 without one.
    pub started_unix_ms: u64,
    /// For one in `backoff`, when its next start is due; 0 otherwise.
    pub next_start_unix_ms: u64,
    /// Its quick exits and failed starts in a row.
    pub quick_exits: u32,
    /// Its last exits, oldest first: when, Unix milliseconds, and what
    /// was said.
    pub exits: Vec<(u64, String)>,
}

/// A service as `service list` and `status` give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Its name: a node's own, or `APP/NAME` for a project's.
    pub name: String,
    /// For a project's service, the application and the release it is
    /// of.
    pub owner: Option<(String, String)>,
    /// Its definition.
    pub table: Table,
    /// The least healthy of its instances' states.
    pub state: &'static str,
    /// When it came to be in that state, Unix milliseconds.
    pub since_unix_ms: u64,
    /// Whether it was stopped by hand.
    pub held: bool,
    /// The starts that followed an exit the program made.
    pub restarts: u64,
    /// The directory its command runs in.
    pub cwd: PathBuf,
    /// Its log.
    pub log: PathBuf,
    /// Its restart policy as a phrase.
    pub restart: String,
    /// Its readiness as a phrase.
    pub readiness: String,
    /// Each instance.
    pub instances: Vec<InstanceStatus>,
}

/// The states from least healthy to healthiest, which is how a service's
/// state is chosen from its instances'.
const STATES: [&str; 7] = [
    "failed", "backoff", "starting", "stopping", "exited", "stopped", "running",
];

/// One service under the daemon: its instances, which a supervisor runs,
/// and what is HTTP's for a managed server done here for a service:
/// readiness in its five forms, the counts and the history.
pub struct Service {
    setting: Setting,
    readiness: Readiness,
    supervisor: Box<Supervisor>,
    /// A supervisor whose instances are being stopped because the
    /// definition changed what they were started with; the one above
    /// starts once this one has nothing left.
    retiring: Option<Box<Supervisor>>,
    /// Whether the instances are to be started once the retiring ones
    /// have gone.
    start_after_retiring: bool,
    /// The retiring instances run on beside the new ones, and are asked
    /// to stop once the new ones are ready or this time has passed: a
    /// changeover that starts first.
    overlap_until: Option<Instant>,
    /// Each instance's readiness, by index.
    probes: BTreeMap<u32, Probe>,
    /// Each instance's last exits, by index.
    history: BTreeMap<u32, VecDeque<(u64, String)>>,
    /// The instances a restart one at a time has still to reach, and the
    /// one it is at.
    rolling: VecDeque<u32>,
    rolling_at: Option<u32>,
    held: bool,
    restarts: u64,
    exits: BTreeMap<supervisor::Outcome, u64>,
    dropped: Arc<AtomicU64>,
    state: &'static str,
    since_unix_ms: u64,
    /// The service is being removed: once nothing of it runs it is
    /// done with.
    removing: bool,
    pending: Vec<Report>,
}

impl Service {
    /// Bring a service under this daemon as `begin` says.
    pub fn begin(setting: Setting, held: bool, begin: Begin<'_>) -> Result<Self, String> {
        setting.dirs.create().map_err(|error| {
            format!(
                "creating {} and {}: {error}",
                setting.dirs.sockets.display(),
                setting.dirs.files.display()
            )
        })?;
        let dropped = Arc::new(AtomicU64::new(0));
        let policy = policy_of(&setting.table);
        let mut probes = BTreeMap::new();

        let supervisor = match begin {
            Begin::Adopt(record) => {
                // The record decides how many instances there are: the
                // definition is brought to its own count afterwards.

                let instances = record
                    .instances
                    .iter()
                    .map(|instance| setting.instance(instance.index))
                    .collect();
                let adopted = Supervisor::adopt(setting.spec(&dropped), policy, instances, record)
                    .map_err(|error| format!("{error:#}"))?;

                // An instance taken over was up under the daemon before.

                for instance in adopted.instances() {
                    if let Some(pid) = instance.pid() {
                        probes.insert(
                            instance.index(),
                            Probe {
                                pid: Some(pid),
                                ready: true,
                                task: None,
                            },
                        );
                    }
                }

                adopted
            }
            Begin::Start | Begin::Idle => {
                let mut idle =
                    Supervisor::idle(setting.spec(&dropped), policy, setting.instances());

                if matches!(begin, Begin::Start) {
                    for position in 0..idle.instances().count() {
                        idle.start_instance(position);
                    }
                }

                idle
            }
        };

        let mut service = Self {
            readiness: Readiness::of(&setting.table),
            setting,
            supervisor: Box::new(supervisor),
            retiring: None,
            start_after_retiring: false,
            overlap_until: None,
            probes,
            history: BTreeMap::new(),
            rolling: VecDeque::new(),
            rolling_at: None,
            held,
            restarts: 0,
            exits: BTreeMap::new(),
            dropped,
            state: "stopped",
            since_unix_ms: crate::model::now_unix_ms(),
            removing: false,
            pending: Vec::new(),
        };
        service.resize();
        service.state = service.worst();
        Ok(service)
    }

    /// The service's name.
    pub fn name(&self) -> &str {
        &self.setting.name
    }

    /// Its definition.
    pub fn table(&self) -> &Table {
        &self.setting.table
    }

    /// Its sockets and runtime files.
    pub fn dirs(&self) -> &HostDirs {
        &self.setting.dirs
    }

    /// Its log.
    pub fn log(&self) -> &Path {
        &self.setting.log
    }

    /// Whether it was stopped by hand.
    pub fn held(&self) -> bool {
        self.held
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

    /// The place of the instance with `index` in the supervisor.
    fn position(&self, index: u32) -> Option<usize> {
        self.supervisor
            .instances()
            .position(|instance| instance.index() == index)
    }

    /// The instances a command is for: the one named, or every one.
    fn chosen(&self, index: Option<u32>) -> Result<Vec<u32>, String> {
        match index {
            Some(index) if self.position(index).is_some() => Ok(vec![index]),
            Some(index) => Err(format!(
                "{} has no instance {index}; it has {}",
                self.setting.name,
                counted(self.supervisor.instances().count() as u32)
            )),
            None => Ok(self
                .supervisor
                .instances()
                .map(supervisor::Instance::index)
                .collect()),
        }
    }

    /// Bring the count of instances to the definition's: the missing are
    /// added after the last, and the highest numbered stopped and
    /// forgotten.
    fn resize(&mut self) {
        let wanted = instances_of(&self.setting.table) as usize;
        let mut have = self.supervisor.instances().count();

        // A service that is stopped stays so, whatever its count: the
        // instances added to it are stopped as the others are.

        let stopped = self.held
            || (have > 0
                && self
                    .supervisor
                    .instances()
                    .all(|instance| instance.state() == State::Stopped));

        while have < wanted {
            let index = self
                .supervisor
                .instances()
                .last()
                .map_or(0, |instance| instance.index() + 1);
            self.supervisor.add(self.setting.instance(index));
            have += 1;

            if stopped {
                self.supervisor.stop(have - 1, Instant::now());
            }
        }

        let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

        while have > wanted && self.supervisor.remove_last(kill_at) {
            have -= 1;
        }
    }

    /// Start the service, or one instance of it, by hand: what is
    /// stopped, exited or failed is put back under its policy with its
    /// count cleared, and what waits for a start is started now. Gives
    /// the indexes started; those already running are left.
    pub fn start(&mut self, index: Option<u32>) -> Result<Vec<u32>, String> {
        let mut started = Vec::new();

        for index in self.chosen(index)? {
            if let Some(position) = self.position(index)
                && self.supervisor.start_instance(position)
            {
                started.push(index);
            }
        }

        self.held = false;
        Ok(started)
    }

    /// Stop the service, or one instance of it, by hand, with `grace`
    /// for each to stop in before it is killed, the definition's
    /// `stop_timeout` when none is given. A service stopped whole is
    /// held: it stays stopped until it is started by hand. Gives the
    /// indexes that had a process.
    pub fn stop(
        &mut self,
        index: Option<u32>,
        grace: Option<Duration>,
    ) -> Result<Vec<u32>, String> {
        let kill_at = Instant::now() + grace.unwrap_or(stop_timeout_of(&self.setting.table));
        let mut stopped = Vec::new();

        for chosen in self.chosen(index)? {
            let Some(position) = self.position(chosen) else {
                continue;
            };

            if self
                .supervisor
                .instances()
                .nth(position)
                .and_then(supervisor::Instance::pid)
                .is_some()
            {
                stopped.push(chosen);
            }

            self.supervisor.stop(position, kill_at);
        }

        self.rolling.clear();
        self.rolling_at = None;

        if index.is_none() {
            self.held = true;
        }

        Ok(stopped)
    }

    /// Restart the service, or one instance of it, by hand: each is
    /// stopped and started at once, with no quick exit counted, and one
    /// that had no process is started. With `rolling`, one at a time,
    /// each ready before the next is stopped.
    pub fn restart(&mut self, index: Option<u32>, rolling: bool) -> Result<Vec<u32>, String> {
        let chosen = self.chosen(index)?;
        self.held = false;

        if rolling {
            self.rolling = chosen.iter().copied().collect();
            self.rolling_at = None;
            return Ok(chosen);
        }

        let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

        for index in &chosen {
            if let Some(position) = self.position(*index) {
                self.supervisor.restart(position, kill_at);
            }
        }

        Ok(chosen)
    }

    /// Send a signal to the service's instances, or to one: gives the
    /// index and pid of each it was sent to.
    pub fn signal(&self, index: Option<u32>, signal: i32) -> Result<Vec<(u32, u32)>, String> {
        Ok(self
            .chosen(index)?
            .into_iter()
            .filter_map(|index| {
                let pid = self.supervisor.signal(self.position(index)?, signal)?;
                Some((index, pid))
            })
            .collect())
    }

    /// Take up a changed definition (section 9.17). What a process is
    /// started with cannot be changed under it, so a change to any of
    /// that stops every instance and starts it again from the new
    /// definition. A changed count starts or stops the difference, and
    /// the rest is in force at once. A service that had failed is
    /// started again, the change being the likeliest fix, and one
    /// stopped by hand stays stopped.
    pub fn update(&mut self, setting: Setting) -> Difference {
        let difference = Difference::between(&self.setting.table, &setting.table);
        let was_running = self
            .supervisor
            .instances()
            .any(|instance| !matches!(instance.state(), State::Stopped));
        let failed: Vec<usize> = self
            .supervisor
            .instances()
            .enumerate()
            .filter(|(_, instance)| instance.state() == State::Failed)
            .map(|(position, _)| position)
            .collect();
        self.setting = setting;
        self.readiness = Readiness::of(&self.setting.table);

        // One declared again while it was being removed is kept.

        self.removing = false;

        if !difference.restart.is_empty() {
            // The instances that run are stopped under the supervisor
            // that started them, and a new one takes their place once
            // they have gone.

            let fresh = Supervisor::idle(
                self.setting.spec(&self.dropped),
                policy_of(&self.setting.table),
                self.setting.instances(),
            );
            let mut old = std::mem::replace(&mut self.supervisor, Box::new(fresh));
            let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

            for position in 0..old.instances().count() {
                old.stop(position, kill_at);
            }

            self.retiring = Some(old);
            self.start_after_retiring = was_running && !self.held;
            self.probes.clear();
            self.rolling.clear();
            self.rolling_at = None;
            return difference;
        }

        self.supervisor.set_policy(policy_of(&self.setting.table));
        self.resize();

        if !self.held {
            for position in failed {
                self.supervisor.start_instance(position);
            }

            if difference.autostart == Some(true) && !was_running {
                for position in 0..self.supervisor.instances().count() {
                    self.supervisor.start_instance(position);
                }
            }
        }

        difference
    }

    /// Take up what the daemon has worked out afresh for the same
    /// definition, its secrets above all, which may have changed in
    /// their stores: the next process of each instance starts with it,
    /// and those that run are left as they are. A start or a restart by
    /// hand does this first, so that it is how a changed secret reaches
    /// a service.
    pub fn refresh(&mut self, setting: Setting) {
        if setting.table != self.setting.table {
            return;
        }

        self.setting = setting;
        self.supervisor.set_env(self.setting.base.clone());
        let indexes: Vec<u32> = self
            .supervisor
            .instances()
            .map(supervisor::Instance::index)
            .collect();

        for (position, index) in indexes.into_iter().enumerate() {
            self.supervisor
                .respecify(position, self.setting.instance(index));
        }
    }

    /// The application and the release a project's service is of.
    pub fn owner(&self) -> Option<&(String, String)> {
        self.setting.owner.as_ref()
    }

    /// Change over to another release's instances (section 9.17): what
    /// runs was started for the release before, and `setting` is the new
    /// release's. With `start_first` the new instances are started at
    /// once and the old ones asked to stop when the new are ready, or
    /// when their readiness timeout has passed if they never are, so
    /// that there is no gap; otherwise the old ones are stopped and the
    /// new started once they have gone, so that there are never two. A
    /// service stopped by hand stays stopped, and one that does not
    /// start automatically is not carried over.
    pub fn replace(&mut self, setting: Setting, start_first: bool) {
        let start = autostart_of(&setting.table) && !self.held;
        self.setting = setting;
        self.readiness = Readiness::of(&self.setting.table);
        self.removing = false;

        let fresh = Supervisor::idle(
            self.setting.spec(&self.dropped),
            policy_of(&self.setting.table),
            self.setting.instances(),
        );
        let mut old = std::mem::replace(&mut self.supervisor, Box::new(fresh));

        // A changeover already under way is ended: what was retiring
        // goes at once, and what was current retires in its turn.

        if let Some(earlier) = self.retiring.take() {
            earlier.kill_all();
            earlier.release();
        }

        self.probes.clear();
        self.rolling.clear();
        self.rolling_at = None;

        if start_first && start && old.running() > 0 {
            for position in 0..self.supervisor.instances().count() {
                self.supervisor.start_instance(position);
            }

            self.start_after_retiring = false;
            self.overlap_until = Some(Instant::now() + self.readiness.timeout);
        } else {
            let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

            for position in 0..old.instances().count() {
                old.stop(position, kill_at);
            }

            self.start_after_retiring = start;
            self.overlap_until = None;
        }

        self.retiring = Some(old);
    }

    /// Stop the instances without holding the service, as its
    /// application's host stopping for idleness does: `resume` starts
    /// them again.
    pub fn pause(&mut self) {
        let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

        for position in 0..self.supervisor.instances().count() {
            self.supervisor.stop(position, kill_at);
        }
    }

    /// Start what `pause` stopped, unless the service is held or does
    /// not start automatically.
    pub fn resume(&mut self) {
        if self.held || !autostart_of(&self.setting.table) {
            return;
        }

        for position in 0..self.supervisor.instances().count() {
            self.supervisor.start_instance(position);
        }
    }

    /// Stop every instance because the service is no longer declared.
    /// `gone` says when nothing of it is left.
    pub fn remove(&mut self) {
        let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

        for position in 0..self.supervisor.instances().count() {
            self.supervisor.stop(position, kill_at);
        }

        self.removing = true;
        self.start_after_retiring = false;
        self.rolling.clear();
        self.rolling_at = None;
    }

    /// Whether the service is being removed.
    pub fn removing(&self) -> bool {
        self.removing
    }

    /// Whether the service was removed and nothing of it runs any more.
    pub fn gone(&self) -> bool {
        self.removing && self.retiring.is_none() && self.supervisor.running() == 0
    }

    /// Reap instances that exited, start those that are due, look at
    /// each instance's readiness and take a restart one at a time a
    /// step on, with what happened for the event stream.
    pub async fn check(&mut self) -> Vec<Report> {
        let mut reports = std::mem::take(&mut self.pending);

        // The instances a changed definition is replacing go first.

        if let Some(retiring) = &mut self.retiring {
            // In a changeover that starts first, the old instances are
            // asked to stop once every new one is ready, or when the new
            // ones have had the time they have to be.

            if let Some(until) = self.overlap_until {
                let ready = self.supervisor.instances().all(|instance| {
                    instance.state() == State::Running
                        && self
                            .probes
                            .get(&instance.index())
                            .is_some_and(|probe| probe.ready && probe.pid == instance.pid())
                });

                if ready || until <= Instant::now() {
                    let kill_at = Instant::now() + stop_timeout_of(&self.setting.table);

                    for position in 0..retiring.instances().count() {
                        retiring.stop(position, kill_at);
                    }

                    self.overlap_until = None;
                }
            }

            reports.extend(retiring.check().await);

            if retiring.running() == 0 {
                self.retiring = None;

                if std::mem::take(&mut self.start_after_retiring) {
                    for position in 0..self.supervisor.instances().count() {
                        self.supervisor.start_instance(position);
                    }
                }
            }
        }

        self.roll();
        let seen = self.supervisor.check().await;
        let now_unix_ms = crate::model::now_unix_ms();

        for report in &seen {
            if let Some(outcome) = report.outcome {
                *self.exits.entry(outcome).or_default() += 1;
            }

            let Some(index) = report.index else {
                continue;
            };

            match report.change {
                Change::Exited(ran) => {
                    let exits = self.history.entry(index).or_default();
                    let said = report.message.split(';').next().unwrap_or_default();
                    let said = said
                        .strip_prefix(&format!("instance {index} "))
                        .unwrap_or(said);
                    exits.push_back((
                        now_unix_ms,
                        format!("{said} after {}", muru_config::format_duration(round(ran))),
                    ));

                    while exits.len() > HISTORY {
                        exits.pop_front();
                    }
                }
                Change::Started { again: true } => self.restarts += 1,
                Change::Started { again: false } | Change::None => {}
            }
        }

        reports.extend(seen);
        reports.extend(self.probe());

        let worst = self.worst();

        if worst != self.state {
            self.state = worst;
            self.since_unix_ms = now_unix_ms;
        }

        reports
    }

    /// Look at each instance's readiness: a new process is given its
    /// check, a check that has ended is taken, and an instance that was
    /// not ready in time is stopped as one that failed.
    fn probe(&mut self) -> Vec<Report> {
        let mut reports = Vec::new();
        let instances: Vec<(usize, u32, Option<u32>, bool)> = self
            .supervisor
            .instances()
            .enumerate()
            .map(|(position, instance)| {
                (
                    position,
                    instance.index(),
                    instance.pid(),
                    instance.stopping(),
                )
            })
            .collect();
        self.probes
            .retain(|index, _| instances.iter().any(|(_, known, _, _)| known == index));

        for (position, index, pid, stopping) in instances {
            let probe = self.probes.entry(index).or_default();

            if probe.pid != pid {
                *probe = Probe {
                    pid,
                    ready: false,
                    task: pid.map(|_| {
                        tokio::spawn(ready_within(
                            self.readiness.clone(),
                            self.setting.fill(index),
                            self.setting.probe_context(),
                        ))
                    }),
                };
            }

            if stopping
                || !probe
                    .task
                    .as_ref()
                    .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                continue;
            }

            // The check has ended, so taking its answer does not wait.

            let Some(task) = probe.task.take() else {
                continue;
            };

            let passed = futures::FutureExt::now_or_never(task)
                .and_then(Result::ok)
                .flatten();

            if let Some(took) = passed {
                probe.ready = true;
                reports.push(Report::new(
                    READY_EVENT,
                    "info",
                    format!(
                        "instance {index} is ready after {}",
                        muru_config::format_duration(round(took))
                    ),
                ));
            } else {
                let within = muru_config::format_duration(self.readiness.timeout);
                reports.push(Report::new(
                    NOT_READY_EVENT,
                    "warning",
                    format!("instance {index} was not ready within {within} and is stopped"),
                ));
                self.supervisor.fail(
                    position,
                    Instant::now() + stop_timeout_of(&self.setting.table),
                    format!("was not ready within {within}"),
                );
            }
        }

        reports
    }

    /// Take a restart one at a time a step on: the instance it is at
    /// must be ready before the next is restarted, and one that does not
    /// come back ends it, with the rest left as they were.
    fn roll(&mut self) {
        if let Some(index) = self.rolling_at {
            let Some(position) = self.position(index) else {
                self.rolling_at = None;
                return;
            };

            let Some(instance) = self.supervisor.instances().nth(position) else {
                return;
            };

            let ready = self.probes.get(&index).is_some_and(|probe| probe.ready);

            match instance.state() {
                State::Running if ready => self.rolling_at = None,
                State::Running | State::Stopping => return,
                State::Backoff { .. } if instance.quick_exits() == 0 => return,
                State::Backoff { .. } | State::Failed | State::Exited | State::Stopped => {
                    let left = self.rolling.len() as u32;
                    self.rolling.clear();
                    self.rolling_at = None;
                    self.pending.push(Report::new(
                        "service.restart_stopped",
                        "error",
                        format!(
                            "the restart one at a time stopped: instance {index} did not come \
                             back; {} left as {}",
                            counted(left),
                            if left == 1 { "it was" } else { "they were" }
                        ),
                    ));
                    return;
                }
            }
        }

        let Some(index) = self.rolling.pop_front() else {
            return;
        };

        if let Some(position) = self.position(index) {
            // The instance's readiness starts over with its new process;
            // until then it is not the one that was ready before.

            if let Some(probe) = self.probes.get_mut(&index) {
                probe.ready = false;
            }

            self.supervisor.restart(
                position,
                Instant::now() + stop_timeout_of(&self.setting.table),
            );
            self.rolling_at = Some(index);
        }
    }

    /// Whether a restart one at a time is under way.
    pub fn rolling(&self) -> bool {
        self.rolling_at.is_some() || !self.rolling.is_empty()
    }

    /// The state of one instance as it is shown.
    fn state_of(&self, instance: &supervisor::Instance) -> &'static str {
        match instance.state() {
            State::Running => {
                let ready = self
                    .probes
                    .get(&instance.index())
                    .is_some_and(|probe| probe.ready && probe.pid == instance.pid());

                if ready { "running" } else { "starting" }
            }
            State::Backoff { .. } => "backoff",
            State::Exited => "exited",
            State::Failed => "failed",
            State::Stopped => "stopped",
            State::Stopping => "stopping",
        }
    }

    /// The least healthy of the instances' states.
    fn worst(&self) -> &'static str {
        // While the old instances of a changeover that stops first are
        // going, nothing of the new runs yet.

        if self.retiring.is_some() && self.overlap_until.is_none() {
            return "stopping";
        }

        let states: Vec<&str> = self
            .supervisor
            .instances()
            .map(|instance| self.state_of(instance))
            .collect();

        STATES
            .into_iter()
            .find(|state| states.contains(state))
            .unwrap_or("stopped")
    }

    /// The service as `service list` and `status` give it.
    pub fn status(&self) -> Status {
        let now_unix_ms = crate::model::now_unix_ms();

        let instances = self
            .supervisor
            .instances()
            .map(|instance| InstanceStatus {
                index: instance.index(),
                pid: instance.pid().unwrap_or(0),
                state: self.state_of(instance),
                started_unix_ms: match instance.pid() {
                    Some(_) => instance.started_unix_ms(),
                    None => 0,
                },
                next_start_unix_ms: match instance.state() {
                    State::Backoff { wait } => now_unix_ms + wait.as_millis() as u64,
                    _ => 0,
                },
                quick_exits: instance.quick_exits(),
                exits: self
                    .history
                    .get(&instance.index())
                    .map(|exits| exits.iter().cloned().collect())
                    .unwrap_or_default(),
            })
            .collect();

        Status {
            name: self.setting.name.clone(),
            owner: self.setting.owner.clone(),
            table: self.setting.table.clone(),
            state: self.worst(),
            since_unix_ms: self.since_unix_ms,
            held: self.held,
            restarts: self.restarts,
            cwd: self.setting.cwd.clone(),
            log: self.setting.log.clone(),
            restart: crate::config::restart_spec_from(self.setting.table.restart.as_ref())
                .describe(),
            readiness: self.readiness.describe(),
            instances,
        }
    }

    /// The exits the instances made themselves that this daemon has
    /// seen, by outcome: `clean` or `failed`.
    pub fn exits(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.exits
            .iter()
            .map(|(outcome, count)| (outcome.as_str(), *count))
    }

    /// Lines of the instances' output their relays have dropped so far.
    pub fn output_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Have each instance's relay open the service's log again by name.
    pub fn reopen_log(&self) -> Vec<tokio::sync::watch::Receiver<u64>> {
        self.supervisor.reopen()
    }

    /// The instances as a record for the next daemon.
    pub fn record(&self) -> supervisor::Record {
        self.supervisor.record()
    }

    /// Whether any instance was left by the policy or by hand, which a
    /// daemon that starts from nothing forgets.
    pub fn left(&self) -> bool {
        self.record()
            .instances
            .iter()
            .any(|instance| matches!(instance.left, Some(Left::Exited | Left::Failed)))
    }

    /// Stop every instance, killing what is still there at `kill_at`,
    /// and remove the service's directories under the runtime location.
    pub async fn stop_until(self, kill_at: std::time::Instant) {
        let kill_at = Instant::from_std(kill_at);

        if let Some(retiring) = self.retiring {
            retiring.stop_until(kill_at).await;
        }

        self.supervisor.stop_until(kill_at).await;
        self.setting.dirs.remove();
    }

    /// Leave the instances running and forget them, for a daemon that
    /// detaches.
    pub fn release(self) {
        // What a changeover is still stopping is in no record, so the
        // next daemon could not find it: it is killed here.

        if let Some(retiring) = self.retiring {
            retiring.kill_all();
            retiring.release();
        }

        self.supervisor.release();
    }
}

/// What adoption found of a service's instances, as its event says it:
/// the pids taken over, how each instance that ended while no daemon ran
/// did and what follows, and those left as they were.
pub fn adoption_message(adopted: &[(u32, supervisor::Adopted)]) -> String {
    use supervisor::Adopted;

    let mut pids = Vec::new();
    let mut clauses = Vec::new();

    for (index, found) in adopted {
        match found {
            Adopted::Kept(pid) => pids.push(pid.to_string()),
            Adopted::Ended {
                exit,
                wait: Some(wait),
            } => clauses.push(format!(
                "instance {index} {exit} while no daemon ran and is started again in {}",
                muru_config::format_duration(*wait)
            )),
            Adopted::Ended { exit, wait: None } => clauses.push(format!(
                "instance {index} {exit} while no daemon ran and is not started again"
            )),
            Adopted::Left(left) => clauses.push(format!(
                "instance {index} stays {}",
                match left {
                    Left::Exited => "exited",
                    Left::Failed => "failed",
                    Left::Stopped => "stopped",
                }
            )),
            Adopted::Gone => {
                clauses.push(format!("instance {index} was gone and is started again"))
            }
        }
    }

    let mut message = match pids.len() {
        0 => "none of its instances was still running".to_owned(),
        _ => format!("took over its instances (pid {})", pids.join(", ")),
    };

    for clause in clauses {
        message.push_str("; ");
        message.push_str(&clause);
    }

    message
}

/// A length of time to a tenth of a second, which is as close as an
/// event or a status needs it.
fn round(duration: Duration) -> Duration {
    Duration::from_millis((duration.as_millis() as u64).div_ceil(100) * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_plan_says_what_a_change_does_to_the_instances() {
        let redis = table("command = ['redis-server', '--port', '6379']");
        assert_eq!(describe_change(None, &redis), "redis-server --port 6379");
        assert_eq!(
            describe_change(
                None,
                &table("command = ['x']\ninstances = 4\nautostart = false")
            ),
            "x, 4 instances, not started automatically"
        );

        // What a process was started with restarts it.

        let moved = table("command = ['redis-server', '--port', '6380']\nenv = { A = 'b' }");
        assert_eq!(
            describe_change(Some(&redis), &moved),
            "command, env changed; its 1 instance is restarted"
        );

        // A count alone starts or stops the difference.

        let more = table("command = ['redis-server', '--port', '6379']\ninstances = 3");
        assert_eq!(
            describe_change(Some(&redis), &more),
            "instances 1 to 3; 2 instances are started"
        );
        assert_eq!(
            describe_change(Some(&more), &redis),
            "instances 3 to 1; 2 instances are stopped"
        );

        // The policy and the rest are in force at once.

        let policy = table(
            "command = ['redis-server', '--port', '6379']\nstop_timeout = '30s'\n\
             restart = { give_up_after = 5 }\nautostart = false",
        );
        assert_eq!(
            describe_change(Some(&redis), &policy),
            "restart, stop_timeout changed; nothing is restarted; \
             autostart off; it keeps running until stopped"
        );
    }

    /// A service whose instances run in `dir`, started directly, with
    /// the lines of `table` after its command.
    fn setting(dir: &Path, name: &str, table: &str) -> Setting {
        Setting {
            name: name.to_owned(),
            table: toml::from_str(table).unwrap(),
            dirs: HostDirs {
                sockets: dir.join("run"),
                files: dir.join("files"),
            },
            cwd: dir.to_path_buf(),
            base: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            env: vec![("WHO".to_owned(), "copy {index} in {run_dir}".to_owned())],
            identity: None,
            log: dir.join("service.log"),
            relays: None,
            owner: None,
        }
    }

    /// Check, in real time, until the service is as `until` wants it,
    /// and give everything reported on the way.
    async fn settle(service: &mut Service, until: impl Fn(&Status) -> bool) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut said = Vec::new();

        loop {
            said.extend(
                service
                    .check()
                    .await
                    .into_iter()
                    .map(|report| format!("{} {}: {}", report.kind, report.level, report.message)),
            );

            if until(&service.status()) {
                return said;
            }

            assert!(
                std::time::Instant::now() < deadline,
                "never settled: {:?}, having said {said:?}",
                service.status()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn states(status: &Status) -> Vec<&'static str> {
        status
            .instances
            .iter()
            .map(|instance| instance.state)
            .collect()
    }

    #[tokio::test]
    async fn a_service_starts_becomes_ready_and_is_stopped_and_started_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let table = "command = ['/bin/sh', '-c', 'echo \"$WHO $MURU_SERVICE\"; exec sleep 600']\n\
                     instances = 2\nreadiness = { after = '200ms' }\n";
        let mut service =
            Service::begin(setting(dir.path(), "copies", table), false, Begin::Start).unwrap();

        // Each instance is starting until its readiness passes.

        let said = settle(&mut service, |status| status.state == "running").await;
        assert!(
            said.iter()
                .any(|line| line.starts_with("service.started info: instance 1 started (pid "))
        );
        assert!(
            said.iter()
                .any(|line| line.starts_with("service.ready info: instance 0 is ready after "))
        );
        let status = service.status();
        assert_eq!(states(&status), ["running", "running"]);
        assert_eq!((status.restarts, status.held), (0, false));
        assert_eq!(status.readiness, "after 200ms");
        assert_eq!(status.restart, "always, 1s to 60s, stable after 60s");

        // Each was given its own index and directory.

        let log = std::fs::read_to_string(dir.path().join("service.log")).unwrap();
        let run = dir.path().join("run");
        assert!(
            log.contains(&format!("copy 0 in {}/0 copies", run.display())),
            "{log}"
        );
        assert!(
            log.contains(&format!("copy 1 in {}/1 copies", run.display())),
            "{log}"
        );
        assert!(run.join("1").is_dir());

        // One instance stopped leaves the other, and the service is not
        // held; the whole service stopped is.

        assert_eq!(service.stop(Some(1), None).unwrap(), [1]);
        settle(&mut service, |status| {
            states(status) == ["running", "stopped"]
        })
        .await;
        assert!(!service.held());
        assert!(
            service
                .stop(Some(7), None)
                .unwrap_err()
                .contains("has no instance 7")
        );
        service.stop(None, None).unwrap();
        let said = settle(&mut service, |status| status.state == "stopped").await;
        assert!(
            said.iter()
                .any(|line| line.starts_with("service.stopped info: instance 0 stopped"))
        );
        assert!(service.held());

        // Started by hand it runs again, with nothing counted, and from
        // what the daemon worked out for it since, as a secret that has
        // changed would be.

        let mut renewed = setting(dir.path(), "copies", table);
        renewed.env = vec![("WHO".to_owned(), "renewed {index} in {run_dir}".to_owned())];
        service.refresh(renewed);
        assert_eq!(service.start(None).unwrap(), [0, 1]);
        settle(&mut service, |status| status.state == "running").await;
        let log = std::fs::read_to_string(dir.path().join("service.log")).unwrap();
        assert!(
            log.contains(&format!("renewed 1 in {}/1 copies", run.display())),
            "{log}"
        );
        assert_eq!((service.status().restarts, service.held()), (0, false));
        assert_eq!(service.start(None).unwrap(), Vec::<u32>::new());

        // An exit of its own is a restart, and is kept for the status.

        let pid = service.status().instances[0].pid;
        assert!(crate::host::signal_process(pid, libc::SIGKILL));
        settle(&mut service, |status| status.state == "backoff").await;
        let status = service.status();
        assert_eq!(status.instances[0].exits.len(), 1);
        assert!(
            status.instances[0].exits[0]
                .1
                .starts_with("exited with signal: 9 (SIGKILL) after ")
        );
        assert!(status.instances[0].next_start_unix_ms > 0);
        settle(&mut service, |status| status.state == "running").await;
        assert_eq!(service.status().restarts, 1);
        assert_eq!(service.exits().collect::<Vec<_>>(), [("failed", 1)]);
        service.stop_until(std::time::Instant::now()).await;
        assert!(!run.exists());
    }

    #[tokio::test]
    async fn an_instance_that_is_not_ready_in_time_is_stopped_and_counted_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let table = "command = ['/bin/sleep', '600']\n\
                     readiness = { unix = '{run_dir}/never.sock', timeout = '300ms', interval = '50ms' }\n\
                     restart = { give_up_after = 1 }\n";
        let mut service =
            Service::begin(setting(dir.path(), "late", table), false, Begin::Start).unwrap();

        let said = settle(&mut service, |status| status.state == "failed").await;
        assert!(
            said.contains(
                &"service.not_ready warning: instance 0 was not ready within 300ms and is stopped"
                    .to_owned()
            ),
            "{said:?}"
        );
        assert!(
            said.iter().any(|line| line.starts_with(
                "service.exited error: instance 0 was not ready within 300ms and was stopped"
            )),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|line| line.starts_with("service.gave_up error: "))
        );
        assert_eq!(service.exits().collect::<Vec<_>>(), [("failed", 1)]);

        // A socket that is there makes the next start ready.

        let socket = dir.path().join("run/0/never.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        service.start(None).unwrap();
        let said = settle(&mut service, |status| status.state == "running").await;
        assert!(
            said.iter()
                .any(|line| line.starts_with("service.ready info: "))
        );
        service.stop_until(std::time::Instant::now()).await;
    }

    #[tokio::test]
    async fn a_changed_definition_restarts_resizes_or_is_in_force_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let ready = "readiness = { after = '50ms' }\n";
        let first = format!("command = ['/bin/sleep', '600']\n{ready}");
        let mut service =
            Service::begin(setting(dir.path(), "tunnel", &first), false, Begin::Start).unwrap();
        settle(&mut service, |status| status.state == "running").await;
        let pid = service.status().instances[0].pid;

        // The policy is in force at once: the process is the same one.

        let policy = format!("{first}restart = {{ policy = 'never' }}\nstop_timeout = '5s'\n");
        let difference = service.update(setting(dir.path(), "tunnel", &policy));
        assert_eq!(difference.in_place, ["restart", "stop_timeout"]);
        settle(&mut service, |status| status.state == "running").await;
        assert_eq!(service.status().instances[0].pid, pid);
        assert_eq!(
            service.status().restart,
            "never, 1s to 60s, stable after 60s"
        );

        // More instances are started after it, and fewer are stopped
        // from the last.

        let more = format!("{policy}instances = 3\n");
        service.update(setting(dir.path(), "tunnel", &more));
        settle(&mut service, |status| states(status) == ["running"; 3]).await;
        assert_eq!(service.status().instances[0].pid, pid);
        service.update(setting(dir.path(), "tunnel", &policy));
        settle(&mut service, |status| states(status) == ["running"]).await;
        assert_eq!(service.status().instances[0].pid, pid);

        // A new command stops what ran and starts from the new one.

        let moved = format!("command = ['/bin/sleep', '601']\n{ready}");
        let difference = service.update(setting(dir.path(), "tunnel", &moved));
        assert_eq!(difference.restart, ["command"]);
        assert_eq!(service.status().state, "stopping");
        settle(&mut service, |status| status.state == "running").await;
        let replaced = service.status().instances[0].pid;
        assert_ne!(replaced, pid);
        assert!(!crate::host::signal_process(pid, 0) || !crate::host::alive(pid));

        // A service stopped by hand stays stopped through a change.

        service.stop(None, None).unwrap();
        settle(&mut service, |status| status.state == "stopped").await;
        service.update(setting(dir.path(), "tunnel", &first));
        tokio::time::sleep(Duration::from_millis(200)).await;
        settle(&mut service, |status| status.state == "stopped").await;
        assert!(service.held());

        // Removed, it is gone once nothing of it runs.

        service.start(None).unwrap();
        settle(&mut service, |status| status.state == "running").await;
        service.remove();
        assert!(!service.gone());
        settle(&mut service, |status| status.state == "stopped").await;
        assert!(service.gone());
        service.stop_until(std::time::Instant::now()).await;
    }

    #[tokio::test]
    async fn a_restart_one_at_a_time_waits_for_each_to_be_ready() {
        let dir = tempfile::tempdir().unwrap();
        let table = "command = ['/bin/sleep', '600']\ninstances = 2\n\
                     readiness = { after = '300ms' }\n";
        let mut service =
            Service::begin(setting(dir.path(), "pair", table), false, Begin::Start).unwrap();
        settle(&mut service, |status| status.state == "running").await;
        let before: Vec<u32> = service.status().instances.iter().map(|i| i.pid).collect();

        assert_eq!(service.restart(None, true).unwrap(), [0, 1]);
        assert!(service.rolling());

        // While the first is not yet ready again, the second still has
        // the process it had.

        settle(&mut service, |status| {
            status.instances[0].pid != before[0] && status.instances[0].pid != 0
        })
        .await;
        assert_eq!(service.status().instances[1].pid, before[1]);
        settle(&mut service, |status| {
            status.state == "running" && status.instances[1].pid != before[1]
        })
        .await;
        settle(&mut service, |status| status.state == "running").await;
        assert!(!service.rolling());

        // A restart by hand is neither a restart counted nor a quick exit.

        let status = service.status();
        assert_eq!(status.restarts, 0);
        assert!(
            status
                .instances
                .iter()
                .all(|instance| instance.quick_exits == 0)
        );
        assert!(service.exits().next().is_none());

        // All at once, every instance is replaced.

        let between: Vec<u32> = status.instances.iter().map(|i| i.pid).collect();
        service.restart(None, false).unwrap();
        settle(&mut service, |status| {
            status.state == "running"
                && status
                    .instances
                    .iter()
                    .zip(&between)
                    .all(|(instance, pid)| instance.pid != *pid)
        })
        .await;
        service.stop_until(std::time::Instant::now()).await;
    }

    #[tokio::test]
    async fn a_changeover_to_another_release_stops_first_or_starts_first() {
        let dir = tempfile::tempdir().unwrap();
        let of = |release: u32, seconds: u32| {
            let table = format!(
                "command = ['/bin/sleep', '{seconds}']\nreadiness = {{ after = '300ms' }}\n"
            );
            let mut setting = setting(dir.path(), "shop/worker", &table);
            setting.dirs = HostDirs {
                sockets: dir.path().join(format!("run/r{release}")),
                files: dir.path().join(format!("files/r{release}")),
            };
            setting.owner = Some(("shop".to_owned(), format!("r{release}")));
            setting
        };
        let pid = |service: &Service| service.status().instances[0].pid;

        // Whether a process is there, without reaping it, which would
        // keep its supervisor from seeing it end.

        let there = |pid: u32| crate::host::signal_process(pid, 0);
        let mut service = Service::begin(of(1, 600), false, Begin::Start).unwrap();
        settle(&mut service, |status| status.state == "running").await;
        let first = pid(&service);

        // Stopping first, there are never two: the new one is started
        // once the old has gone.

        service.replace(of(2, 601), false);
        assert_eq!(service.status().state, "stopping");
        assert_eq!(service.owner().unwrap().1, "r2");
        settle(&mut service, |status| status.instances[0].pid != 0).await;
        assert!(!there(first));
        settle(&mut service, |status| status.state == "running").await;
        let second = pid(&service);
        assert_ne!(second, first);

        // Starting first, there is no gap: the old one runs on until the
        // new is ready, and is stopped then.

        service.replace(of(3, 602), true);
        settle(&mut service, |status| status.instances[0].pid != 0).await;
        assert!(there(second));
        assert_eq!(service.status().state, "starting");
        settle(&mut service, |status| status.state == "running").await;
        let third = pid(&service);
        assert_ne!(third, second);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);

        while there(second) {
            assert!(
                std::time::Instant::now() < deadline,
                "the old one never stopped"
            );
            let _ = service.check().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Nothing was counted as a restart or an exit of its own.

        assert_eq!(service.status().restarts, 0);
        assert!(service.exits().next().is_none());

        // One stopped by hand stays stopped when the release changes.

        service.stop(None, None).unwrap();
        settle(&mut service, |status| status.state == "stopped").await;
        service.replace(of(4, 603), true);
        tokio::time::sleep(Duration::from_millis(300)).await;
        settle(&mut service, |status| status.state == "stopped").await;
        assert!(service.held());
        assert!(!there(third));
        service.stop_until(std::time::Instant::now()).await;
    }

    #[tokio::test]
    async fn the_next_daemon_takes_the_instances_over_and_keeps_what_was_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let table = "command = ['/bin/sleep', '600']\ninstances = 2\n\
                     readiness = { after = '50ms' }\n";
        let mut first =
            Service::begin(setting(dir.path(), "kept", table), false, Begin::Start).unwrap();
        settle(&mut first, |status| status.state == "running").await;
        first.stop(Some(1), None).unwrap();
        settle(&mut first, |status| {
            states(status) == ["running", "stopped"]
        })
        .await;
        let pid = first.status().instances[0].pid;
        let record = first.record();
        first.release();

        let mut second = Service::begin(
            setting(dir.path(), "kept", table),
            false,
            Begin::Adopt(&record),
        )
        .unwrap();
        assert_eq!(
            second.adopted(),
            [
                (0, supervisor::Adopted::Kept(pid)),
                (1, supervisor::Adopted::Left(Left::Stopped)),
            ]
        );
        let said = settle(&mut second, |status| {
            states(status) == ["running", "stopped"]
        })
        .await;
        assert_eq!(said, Vec::<String>::new());
        assert_eq!(second.status().instances[0].pid, pid);
        second.stop_until(std::time::Instant::now()).await;
    }
}
