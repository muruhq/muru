//! The host process of one release in development mode: started with the
//! Python interpreter, watched until its workers listen, and stopped with
//! SIGTERM (design specification sections 6.1 and 13.1).

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use muru_common::log::{Gathered, Stream, Traceback};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::logs::Logs;

/// What a host says over its control connection (section 5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// The sockets the edge should route to now: the default group's,
    /// and each named group's. Acknowledged by version once the edge
    /// routes by them.
    Workers {
        /// The announcement's number, which the acknowledgement names.
        version: u64,
        /// The default group's sockets.
        sockets: Vec<PathBuf>,
        /// Each named group's sockets.
        groups: Vec<(String, Vec<PathBuf>)>,
        /// The interfaces the host settled, by mount, the default mount's
        /// under the empty name; none when the workers settle them.
        interfaces: Vec<(String, String)>,
        /// Every worker the host runs now.
        workers: Vec<HostWorker>,
    },
    /// An event about a worker, for the event log.
    Event {
        /// The event type, such as worker.recycled.
        kind: String,
        /// The level: info, warning or error.
        level: String,
        /// What happened.
        message: String,
        /// The worker's group, when the event is about one worker.
        group: String,
        /// The reason as one word, for the counters of section 11.1:
        /// why a worker was recycled or how it exited.
        reason: String,
    },
    /// What every worker reported last, for metrics and the dashboard.
    Status {
        /// One entry per worker.
        workers: Vec<WorkerStatus>,
        /// The host's own resident memory, when measured (sections 7.7
        /// and 13.9).
        host_rss: Option<u64>,
        /// The workers being replaced for memory, which the memory guard
        /// counts as its one at a time (section 13.9, issue 662).
        memory_replacements: Vec<MemoryReplacement>,
    },
    /// The workers' answers to a request (section 11.7).
    Response {
        /// The request's id.
        id: u64,
        /// One answer per worker asked.
        workers: Vec<WorkerReply>,
        /// Why no worker was asked, when none was.
        error: String,
    },
}

/// One worker a host runs, as it last announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostWorker {
    /// The worker's index within its group.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Its group.
    pub group: String,
    /// The socket it listens on.
    pub socket: String,
    /// What it is doing: starting, ready, debugging, replacing or retiring.
    pub state: String,
}

/// One worker's answer to a request (section 11.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReply {
    /// The worker's index within its group.
    pub index: u32,
    /// Its group.
    pub group: String,
    /// Its pid.
    pub pid: u32,
    /// Whether it answered.
    pub ok: bool,
    /// Why not, when it did not.
    pub error: String,
    /// Its answer as JSON text, when it did.
    pub result_json: String,
}

/// A worker its host is replacing for memory, by the worker's own limits
/// or the memory guard's ask, until its successor has taken its place
/// (section 13.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryReplacement {
    /// The worker's group.
    pub group: String,
    /// Its index within the group.
    pub index: u32,
    /// The pid of the worker being replaced.
    pub pid: u32,
}

/// The variable that gives the host its instrumentation list, as JSON
/// (section 6.15).
pub const INSTRUMENTATION_VARIABLE: &str = "MURU_INSTRUMENTATION";

/// What one worker last reported to its host (sections 7.6 and 7.7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerStatus {
    /// The worker's index within its group.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Its group.
    pub group: String,
    /// Requests served since it started.
    pub requests: u64,
    /// Requests in flight.
    pub in_flight: u64,
    /// The age of its oldest request, in milliseconds.
    pub oldest_ms: u64,
    /// The pools synchronous code runs on for its event loop (section
    /// 6.6), each by name with its threads running a call and the
    /// threads it has; empty for a worker without a loop.
    pub sync_pools: Vec<(String, u64, u64)>,
    /// Resident set size in bytes, when measured.
    pub rss: Option<u64>,
    /// Proportional set size in bytes, when measured.
    pub pss: Option<u64>,
    /// Unique set size in bytes, when measured.
    pub uss: Option<u64>,
    /// CPU time used since it started, in milliseconds, when measured
    /// (section 7.3).
    pub cpu_ms: Option<u64>,
    /// Whether its interpreter runs with a GIL.
    pub gil_enabled: bool,
    /// Its GIL waits, cumulative at or below each of the worker
    /// protocol's signal bounds, then the count of all.
    pub gil_wait: Vec<u64>,
    /// The sum of its GIL waits, in microseconds.
    pub gil_wait_us: u64,
    /// Its event loop's lateness, likewise.
    pub loop_lag: Vec<u64>,
    /// The sum of its event loop's lateness, in microseconds.
    pub loop_lag_us: u64,
    /// The waits for a thread of the requests it handed from ASGI to
    /// WSGI (section 6.14), likewise.
    pub handoff_wait: Vec<u64>,
    /// The sum of those waits, in microseconds.
    pub handoff_wait_us: u64,
    /// Its garbage collections by generation (section 11.1), those with
    /// a collection so far.
    pub gc: Vec<GcGeneration>,
}

/// One generation's garbage collections since a worker started, as the
/// runtime reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcGeneration {
    /// The generation: `0`, `1` or `2`, or `full` for a collector that
    /// takes the whole heap each time.
    pub generation: String,
    /// Collections run.
    pub collections: u64,
    /// Time spent collecting, in nanoseconds.
    pub nanos: u64,
    /// Objects the collections freed.
    pub collected: u64,
    /// Collections that ran while requests were in flight.
    pub busy: u64,
    /// The requests in flight during those, summed.
    pub waited: u64,
    /// Pauses with no request in flight, cumulative over the signal
    /// bounds then the count of all.
    pub idle: Vec<u64>,
    /// Their sum, in microseconds.
    pub idle_us: u64,
    /// Pauses with requests in flight, likewise.
    pub busy_pauses: Vec<u64>,
    /// Their sum, in microseconds.
    pub busy_us: u64,
}

/// A histogram's counts from a status line, or none.
fn counts(value: &serde_json::Value) -> Vec<u64> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(serde_json::Value::as_u64).collect())
        .unwrap_or_default()
}

/// The garbage collections of a status line, by generation.
fn generations(value: &serde_json::Value) -> Vec<GcGeneration> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| GcGeneration {
                    generation: item["generation"].as_str().unwrap_or("0").to_owned(),
                    collections: item["collections"].as_u64().unwrap_or(0),
                    nanos: item["ns"].as_u64().unwrap_or(0),
                    collected: item["collected"].as_u64().unwrap_or(0),
                    busy: item["busy"].as_u64().unwrap_or(0),
                    waited: item["waited"].as_u64().unwrap_or(0),
                    idle: counts(&item["idle_hist"]),
                    idle_us: item["idle_us"].as_u64().unwrap_or(0),
                    busy_pauses: counts(&item["busy_hist"]),
                    busy_us: item["busy_us"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

impl HostMessage {
    /// Parse one line of the control connection, or none for a line
    /// this version does not know.
    fn parse(line: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let strings = |key: &str| -> Vec<PathBuf> {
            value[key]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default()
        };

        match value["type"].as_str()? {
            "workers" => Some(Self::Workers {
                version: value["version"].as_u64().unwrap_or(0),
                sockets: strings("sockets"),
                groups: value["groups"]
                    .as_array()
                    .map(|groups| {
                        groups
                            .iter()
                            .filter_map(|group| {
                                let name = group["name"].as_str()?.to_owned();
                                let sockets = group["sockets"]
                                    .as_array()?
                                    .iter()
                                    .filter_map(|item| item.as_str().map(PathBuf::from))
                                    .collect();
                                Some((name, sockets))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                interfaces: value["interfaces"]
                    .as_object()
                    .map(|interfaces| {
                        interfaces
                            .iter()
                            .filter_map(|(mount, interface)| {
                                Some((mount.clone(), interface.as_str()?.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                workers: value["workers"]
                    .as_array()
                    .map(|workers| {
                        workers
                            .iter()
                            .map(|worker| HostWorker {
                                index: worker["index"].as_u64().unwrap_or(0) as u32,
                                pid: worker["pid"].as_u64().unwrap_or(0) as u32,
                                group: worker["group"].as_str().unwrap_or("default").to_owned(),
                                socket: worker["socket"].as_str().unwrap_or_default().to_owned(),
                                state: worker["state"].as_str().unwrap_or_default().to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "event" => Some(Self::Event {
                kind: value["kind"].as_str().unwrap_or("worker.event").to_owned(),
                level: value["level"].as_str().unwrap_or("info").to_owned(),
                message: value["message"].as_str().unwrap_or_default().to_owned(),
                group: value["group"].as_str().unwrap_or_default().to_owned(),
                reason: value["reason"].as_str().unwrap_or_default().to_owned(),
            }),
            "response" => Some(Self::Response {
                id: value["id"].as_u64().unwrap_or(0),
                error: value["error"].as_str().unwrap_or_default().to_owned(),
                workers: value["workers"]
                    .as_array()
                    .map(|workers| {
                        workers
                            .iter()
                            .map(|worker| WorkerReply {
                                index: worker["index"].as_u64().unwrap_or(0) as u32,
                                group: worker["group"].as_str().unwrap_or("default").to_owned(),
                                pid: worker["pid"].as_u64().unwrap_or(0) as u32,
                                ok: worker["ok"].as_bool().unwrap_or(false),
                                error: worker["error"].as_str().unwrap_or_default().to_owned(),
                                result_json: worker
                                    .get("result")
                                    .map(|result| result.to_string())
                                    .unwrap_or_default(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "status" => Some(Self::Status {
                host_rss: value["host"]["rss"].as_u64(),
                workers: value["workers"]
                    .as_array()
                    .map(|workers| {
                        workers
                            .iter()
                            .map(|worker| WorkerStatus {
                                index: worker["index"].as_u64().unwrap_or(0) as u32,
                                pid: worker["pid"].as_u64().unwrap_or(0) as u32,
                                group: worker["group"].as_str().unwrap_or("default").to_owned(),
                                requests: worker["requests"].as_u64().unwrap_or(0),
                                in_flight: worker["in_flight"].as_u64().unwrap_or(0),
                                oldest_ms: worker["oldest_ms"].as_u64().unwrap_or(0),
                                sync_pools: worker["sync_pools"]
                                    .as_object()
                                    .map(|pools| {
                                        pools
                                            .iter()
                                            .filter_map(|(name, pool)| {
                                                Some((
                                                    name.clone(),
                                                    pool["busy"].as_u64()?,
                                                    pool["of"].as_u64()?,
                                                ))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                rss: worker["rss"].as_u64(),
                                pss: worker["pss"].as_u64(),
                                uss: worker["uss"].as_u64(),
                                cpu_ms: worker["cpu_ms"].as_u64(),
                                gil_enabled: worker["gil_enabled"].as_bool().unwrap_or(true),
                                gil_wait: counts(&worker["gil_wait"]),
                                gil_wait_us: worker["gil_wait_us"].as_u64().unwrap_or(0),
                                loop_lag: counts(&worker["loop_lag"]),
                                loop_lag_us: worker["loop_lag_us"].as_u64().unwrap_or(0),
                                handoff_wait: counts(&worker["handoff_wait"]),
                                handoff_wait_us: worker["handoff_wait_us"].as_u64().unwrap_or(0),
                                gc: generations(&worker["gc"]),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                memory_replacements: value["memory_replacements"]
                    .as_array()
                    .map(|replacements| {
                        replacements
                            .iter()
                            .map(|replacement| MemoryReplacement {
                                group: replacement["group"]
                                    .as_str()
                                    .unwrap_or("default")
                                    .to_owned(),
                                index: replacement["index"].as_u64().unwrap_or(0) as u32,
                                pid: replacement["pid"].as_u64().unwrap_or(0) as u32,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        }
    }
}

/// The supervisor's side of a host's control connection: what the host
/// sends arrives on the receiver, and acknowledgements go back.
pub struct HostControl {
    outgoing: mpsc::UnboundedSender<String>,
}

impl HostControl {
    /// Connect to a host's control socket, trying for a few seconds
    /// while the host binds it, and deliver what it sends to the
    /// receiver until the connection ends.
    pub async fn connect(path: &Path) -> anyhow::Result<(Self, mpsc::Receiver<HostMessage>)> {
        let started = Instant::now();

        let stream = loop {
            match tokio::net::UnixStream::connect(path).await {
                Ok(stream) => break stream,
                Err(error) if started.elapsed() < Duration::from_secs(5) => {
                    tracing::debug!(%error, path = %path.display(), "the host control socket is not up yet");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("connecting to {}", path.display()));
                }
            }
        };

        let (read_half, mut write_half) = stream.into_split();
        let (incoming_tx, incoming_rx) = mpsc::channel(32);
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(message) = HostMessage::parse(&line)
                    && incoming_tx.send(message).await.is_err()
                {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(line) = outgoing_rx.recv().await {
                if write_half.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                outgoing: outgoing_tx,
            },
            incoming_rx,
        ))
    }

    /// Tell the host the edge routes by the sockets it announced at
    /// this version.
    pub fn acknowledge(&self, version: u64) {
        let _ = self
            .outgoing
            .send(format!("{{\"type\":\"applied\",\"version\":{version}}}\n"));
    }

    /// Ask the host's workers something (section 11.7), one of them when
    /// `worker` names it; the answer comes back as a Response message
    /// carrying the same id.
    pub fn request(
        &self,
        id: u64,
        op: &str,
        worker: Option<&muru_common::worker::WorkerName>,
        args: &serde_json::Value,
    ) {
        let message = serde_json::json!({
            "type": "request",
            "id": id,
            "op": op,
            "worker": worker.map(|worker| worker.index),
            "group": worker.map(|worker| worker.group.as_str()),
            "args": args,
        });
        let _ = self.outgoing.send(format!("{message}\n"));
    }
}

/// How much longer than its workers' stop budget a host is given to
/// stop them and exit, before it is killed in turn.
pub const STOP_MARGIN: Duration = Duration::from_secs(5);

/// How long to wait for a host to stop, given its workers' stop budget:
/// long enough for it to kill them and to be killed itself.
pub fn stop_wait(budget: Duration) -> Duration {
    budget + STOP_MARGIN * 2
}

/// The file in a host's run directory saying when a shutdown wants its
/// workers gone, in milliseconds since the Unix epoch (section 13.3). It
/// is written before the host is told to stop, so the host always finds
/// it; `muru.host` reads it under this name.
pub const STOP_BY_FILE: &str = "stop-by";

/// The variable naming, comma separated, the release's variables whose
/// values `debug inspect` shows (section 11.7); the runtime library reads
/// it as `muru._inspect.DECLARED`.
pub const SHOWN_ENV_VARIABLE: &str = "MURU_DECLARED_ENV";

/// What every host of a run is started with.
pub struct HostSpec {
    /// The interpreter with the muru runtime library installed.
    pub python: PathBuf,
    /// The application as module:callable.
    pub app: String,
    /// The application's name, which the runtime library reports through
    /// `muru.context()` (section 6.9); none in development, where the
    /// host names it after the entrypoint's module.
    pub name: Option<String>,
    /// The release's id, likewise; none in development, where each
    /// release of the run is `r1`, `r2` and so on, as the log names it.
    pub release: Option<String>,
    /// The interface the entrypoint speaks.
    pub interface: muru_config::Interface,
    /// Whether the entrypoint is a factory, and when it is called.
    pub factory: Option<muru_config::FactoryPhase>,
    /// Whether gRPC server reflection is served (section 6.13).
    pub reflection: bool,
    /// The interface the default mount's requests may be handed to
    /// (section 6.14).
    pub handoff: Option<muru_config::Handoff>,
    /// Worker processes to fork.
    pub processes: usize,
    /// Application threads per worker.
    pub threads: usize,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: usize,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: usize,
    /// The preload mode.
    pub preload: muru_config::Preload,
    /// The hooks module, if any.
    pub hooks: Option<String>,
    /// The instrumentation modules, in order (section 6.15), given to the
    /// host in `MURU_INSTRUMENTATION`.
    pub instrumentation: Vec<muru_config::Instrumentation>,
    /// The directory to run the host in.
    pub chdir: Option<PathBuf>,
    /// Loopback TCP instead of Unix sockets for the link, from this port.
    pub tcp_base_port: Option<u16>,
    /// Serve in the host process itself, with no fork (section 13.1).
    pub single: bool,
    /// Keep running when this process goes away, with the relay writing
    /// output to this file, so a daemon can re-adopt the host after it
    /// restarts (section 13.3). None has the relay write to this process.
    pub detach_to: Option<PathBuf>,
    /// What the release's log relay queues and does when full (section
    /// 11.3).
    pub relay: RelaySettings,
    /// Named worker groups beside the default one (section 7.1).
    pub groups: Vec<HostGroup>,
    /// Named mounts beside the default one at / (section 6.7).
    pub mounts: Vec<HostMount>,
    /// Python logging defaults (section 11.4).
    pub logging: muru_config::PythonLogging,
    /// Requests a worker serves before it is replaced (section 7.7).
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced (section 7.7).
    pub max_lifetime: Option<Duration>,
    /// How long a request may run before it is interrupted (section
    /// 7.6); off when absent.
    pub soft_timeout: Option<Duration>,
    /// How long a request may run before its worker is replaced (section
    /// 7.6); off when absent.
    pub hard_timeout: Option<Duration>,
    /// How long a replaced worker gets to finish its other requests.
    pub hard_drain_grace: Duration,
    /// How long a worker told to stop gets before it is killed, from
    /// when its WebSockets are closed.
    pub shutdown_timeout: Duration,
    /// How long a worker told to stop keeps its open WebSockets before it
    /// closes them with 1001.
    pub websocket_drain_grace: Duration,
    /// How long a worker has for its shutdown work once its requests have
    /// had their time (section 7.6).
    pub exit_grace: Duration,
    /// How long an ASGI worker's event loop may stall before the worker
    /// is replaced (section 7.6).
    pub loop_stall_timeout: Duration,
    /// How long a worker may run no Python, or send no report, before it
    /// is replaced (section 7.6).
    pub gil_stall_timeout: Duration,
    /// The memory policy for the workers (section 7.7).
    pub memory: crate::model::MemoryPolicy,
    /// Sections 7.7 and 13.9: the cgroup the workers run in, whose
    /// `memory.events` tells a worker the kernel killed for memory, and
    /// what is left under whose `memory.max` decides whether a worker
    /// replaced for memory stops before its replacement starts.
    pub cgroup: Option<PathBuf>,
    /// Environment variables for the host and its workers (section
    /// 10.8), secret values already resolved.
    pub env: Vec<(String, String)>,
    /// Section 11.7: the variables of `env` whose values `debug inspect`
    /// shows without `--env`, which leaves out a secret's value.
    pub shown_env: Vec<String>,
    /// Section 8.3: the runtime library directory to put first on
    /// PYTHONPATH, when the environment's own `muru` is not used.
    pub pythonpath: Option<PathBuf>,
    /// Section 8.2: the directory bytecode is written under
    /// (PYTHONPYCACHEPREFIX), when it is redirected.
    pub pycache_prefix: Option<PathBuf>,
    /// Section 8.2: the release's bytecode setting; none where bytecode
    /// is left to Python and the environment, as in a development run.
    pub bytecode: Option<muru_config::Bytecode>,
    /// Whether `env` is the whole environment rather than laid over this
    /// process's own (section 8.7), as it is under the daemon.
    pub clean_env: bool,
    /// The user and group the host runs as, and its umask, when the
    /// daemon is root and the application names them (section 10.9).
    pub identity: Option<Identity>,
}

impl HostSpec {
    /// The release as app@release, as the host names itself: the
    /// application's name, else the top-level module of its entrypoint,
    /// and the release's id, else the name of its run directory (`r1`,
    /// `r2` in development).
    pub fn label(&self, run_dir: &Path) -> String {
        let name = self.name.clone().unwrap_or_else(|| {
            let module = self.app.split(':').next().unwrap_or_default();
            let top = module.split('.').next().unwrap_or_default();

            if top.is_empty() {
                "default".to_owned()
            } else {
                top.to_owned()
            }
        });

        let release = self.release.clone().unwrap_or_else(|| {
            run_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });

        format!("{name}@{release}")
    }
}

/// Whom a host runs as (section 10.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The user id.
    pub uid: u32,
    /// The group id.
    pub gid: u32,
    /// The file creation mask, or none to keep the daemon's.
    pub umask: Option<u32>,
}

impl Identity {
    /// Look a user and group up by name: the group defaults to the
    /// user's primary group.
    pub fn lookup(user: &str, group: Option<&str>, umask: Option<u32>) -> anyhow::Result<Self> {
        let user_name = std::ffi::CString::new(user)?;

        // SAFETY: getpwnam takes a NUL-terminated name and returns a
        // pointer to static storage or null; the fields are read at once.

        let (uid, primary) = unsafe {
            let entry = libc::getpwnam(user_name.as_ptr());
            anyhow::ensure!(!entry.is_null(), "no user named {user}");
            ((*entry).pw_uid, (*entry).pw_gid)
        };

        let gid = match group {
            None => primary,
            Some(group) => {
                let group_name = std::ffi::CString::new(group)?;

                // SAFETY: as for getpwnam.

                unsafe {
                    let entry = libc::getgrnam(group_name.as_ptr());
                    anyhow::ensure!(!entry.is_null(), "no group named {group}");
                    (*entry).gr_gid
                }
            }
        };

        Ok(Self { uid, gid, umask })
    }

    /// Section 8.7: `HOME`, `USER` and `LOGNAME` for the user this is, from
    /// the password database. None for a user id with no entry, and an
    /// empty field is left out.
    pub fn account(&self) -> Vec<(String, String)> {
        let mut buffer = vec![0u8; 4096];

        // SAFETY: an all-zero passwd is a valid value of a C struct of
        // integers and pointers, and getpwuid_r overwrites it.

        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();

        // getpwuid_r, not getpwuid: the daemon has many threads, and the
        // other's static storage is shared between them.

        loop {
            // SAFETY: every pointer is to a live local, and the length is
            // that of the buffer; on return the strings in `entry` point
            // into the buffer.

            let status = unsafe {
                libc::getpwuid_r(
                    self.uid as libc::uid_t,
                    &mut entry,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            };

            if status == libc::ERANGE && buffer.len() < 1 << 20 {
                buffer.resize(buffer.len() * 2, 0);
                continue;
            }

            break;
        }

        if found.is_null() {
            return Vec::new();
        }

        let field = |pointer: *const libc::c_char| {
            // SAFETY: getpwuid_r found an entry, so its string fields are
            // NUL-terminated within the buffer, which is still alive.

            unsafe { std::ffi::CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned()
        };
        let (home, name) = (field(entry.pw_dir), field(entry.pw_name));

        [("HOME", home), ("USER", name.clone()), ("LOGNAME", name)]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(variable, value)| (variable.to_owned(), value))
            .collect()
    }

    /// Whether this process may switch to another user.
    pub fn can_switch() -> bool {
        // SAFETY: geteuid has no preconditions.

        unsafe { libc::geteuid() == 0 }
    }

    /// Give a path to the identity's user and group.
    pub fn own(&self, path: &Path) -> std::io::Result<()> {
        std::os::unix::fs::chown(path, Some(self.uid), Some(self.gid))
    }

    /// Whether this is the superuser.
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

/// A named mount, as the host loads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// The mount's name.
    pub name: String,
    /// The path prefix.
    pub path: String,
    /// The application as module:callable.
    pub entrypoint: String,
    /// The interface the entrypoint speaks.
    pub interface: muru_config::Interface,
    /// Threads of the mount's own (section 6.7).
    pub threads: Option<muru_config::MountThreads>,
    /// An ASGI mount's requests at once on each worker (section 6.7).
    pub concurrency: Option<usize>,
    /// An ASGI mount's open WebSockets at once on each worker.
    pub max_connections: Option<usize>,
    /// The interface the mount's requests may be handed to (section 6.14).
    pub handoff: Option<muru_config::Handoff>,
}

/// A named worker group's sizes, as the host forks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGroup {
    /// The group's name.
    pub name: String,
    /// Worker processes.
    pub processes: usize,
    /// Application threads per worker.
    pub threads: usize,
    /// Threads for synchronous code called from an ASGI application.
    pub sync_threads: usize,
    /// Requests an ASGI worker's event loop takes at once.
    pub concurrency: usize,
    /// The group's timeouts, resolved over the release's (section 7.1).
    pub timeouts: crate::model::WorkerTimeouts,
    /// The group's memory policy, resolved over the release's.
    pub memory: crate::model::MemoryPolicy,
    /// Requests a worker serves before it is replaced, resolved over the
    /// release's (section 7.7).
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced, resolved over the
    /// release's.
    pub max_lifetime: Option<Duration>,
    /// Whether the release is ready without the group's workers (ADR
    /// 0033).
    pub optional: bool,
    /// The named mounts its workers serve beside the default mount;
    /// every mount when empty (ADR 0033).
    pub mounts: Vec<String>,
}

/// A running host and the sockets its workers listen on.
pub struct HostProcess {
    process: Process,
    stdin: Option<ChildStdin>,
    /// Every worker link socket (paths, or addresses for the TCP link):
    /// the default group's first, then each named group's.
    pub sockets: Vec<PathBuf>,
    /// The sockets of each named group, in the order of the spec.
    pub group_sockets: Vec<(String, Vec<PathBuf>)>,
    /// The groups the release is ready without (ADR 0033).
    optional: Vec<String>,
    run_dir: PathBuf,
    /// The release number within this run, from 1.
    pub release: u32,
    /// The tasks relaying its log relay's output to this process's
    /// streams, which end when the relay closes it. Shared with the
    /// supervising task, which adds one for each relay it starts.
    relays: SharedRelays,
    /// Its log relay and the task supervising it (section 11.3).
    relay: RelaySupervisor,
}

/// The tasks reading what relays write to this process.
type SharedRelays = Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

/// A host this process spawned, or one it adopted from a previous daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

impl HostProcess {
    /// Start the host for release number `release` under the run directory,
    /// with a log relay beside it (section 11.3). Without `detach_to` what
    /// the relay writes comes to this process's own streams and is kept in
    /// `logs` for the dashboard; `watcher` hears what the relay reports.
    pub fn start(
        spec: &HostSpec,
        run_dir: &Path,
        release: u32,
        logs: &Logs,
        watcher: RelayWatcher,
    ) -> anyhow::Result<Self> {
        let run_dir = run_dir.join(format!("r{release}"));
        std::fs::create_dir_all(&run_dir)
            .with_context(|| format!("creating {}", run_dir.display()))?;
        let mut command = Command::new(&spec.python);
        command
            .arg("-m")
            .arg("muru.host")
            .arg("--app")
            .arg(&spec.app)
            .arg("--socket-dir")
            .arg(&run_dir)
            .args(
                spec.name
                    .iter()
                    .flat_map(|name| ["--name".to_owned(), name.clone()]),
            )
            .arg("--release")
            .arg(
                spec.release
                    .clone()
                    .unwrap_or_else(|| format!("r{release}")),
            )
            .arg("--processes")
            .arg(spec.processes.to_string())
            .arg("--threads")
            .arg(spec.threads.to_string())
            .arg("--sync-threads")
            .arg(spec.sync_threads.to_string())
            .arg("--concurrency")
            .arg(spec.concurrency.to_string())
            .arg("--preload")
            .arg(spec.preload.as_str())
            .arg("--interface")
            .arg(spec.interface.as_str())
            .stdin(Stdio::piped());

        // Section 11.3: the host's own output is a pipe made here, before
        // either process starts. The relay inherits its read end, so even
        // an interpreter that fails before the host's code runs is heard,
        // and the host inherits a copy to give any later relay.

        let (output_read, output_write) =
            std::io::pipe().context("making the host's output pipe")?;
        command
            .stdout(
                output_write
                    .try_clone()
                    .context("copying the host's output pipe")?,
            )
            .stderr(output_write);

        match &spec.detach_to {
            Some(_) => own_session(&mut command, spec.identity),
            None => {
                // Section 13.1: a Ctrl-C in the terminal reaches muructl
                // alone, which stops the edge before it tells the host,
                // so the host and its workers are in a group of their own.

                command.process_group(0);
            }
        }

        // With --single the host's own loop never runs, so nothing would
        // answer a relay or hold pipes for one: there are no workers, and
        // this process keeps the read end for any later relay instead.

        if !spec.single {
            crate::fdpass::inherit_at(
                command.as_std_mut(),
                output_read.as_raw_fd(),
                crate::relay::HOST_OUTPUT_FD,
            );
            command
                .arg("--output-socket")
                .arg(run_dir.join(crate::relay::OUTPUT_SOCKET))
                .arg("--output-fd")
                .arg(crate::relay::HOST_OUTPUT_FD.to_string());
        }

        if let Some(hooks) = &spec.hooks {
            command.arg("--hooks").arg(hooks);
        }

        if let Some(phase) = spec.factory {
            command
                .arg("--factory")
                .arg("--factory-phase")
                .arg(phase.as_str());
        }

        if spec.single {
            command.arg("--single");
        }

        if spec.reflection {
            command.arg("--reflection");
        }

        if let Some(handoff) = spec.handoff {
            command.arg("--handoff").arg(handoff.as_str());
        }

        if spec.detach_to.is_some() {
            command.arg("--detach");
        }

        for mount in &spec.mounts {
            let mut text = format!(
                "{}={}={}={}",
                mount.name,
                mount.path,
                mount.entrypoint,
                mount.interface.as_str()
            );

            // Section 6.14: a declared ASGI mount's threads are those its
            // handoffs to WSGI run on, passed with the handoff.

            let handoff_threads = mount
                .threads
                .filter(|_| mount.interface == muru_config::Interface::Asgi);

            // Section 6.7: the mount's own threads, then its own limits,
            // either of which may be absent; a mount left to be detected
            // may carry limits, which its worker checks against what it
            // detects.

            let threads = mount
                .threads
                .filter(|_| handoff_threads.is_none())
                .map(|threads| format!("{},{}", threads.reserved(), threads.max()))
                .unwrap_or_default();

            if mount.concurrency.is_some() || mount.max_connections.is_some() {
                text.push_str(&format!(
                    "={threads}={},{}",
                    mount.concurrency.unwrap_or(0),
                    mount.max_connections.unwrap_or(0)
                ));
            } else if !threads.is_empty() {
                text.push_str(&format!("={threads}"));
            }

            command.arg("--mount").arg(text);

            if let Some(handoff) = mount.handoff {
                let threads = handoff_threads
                    .map(|threads| format!(",{}", threads.max()))
                    .unwrap_or_default();
                command.arg("--mount-handoff").arg(format!(
                    "{}={}{threads}",
                    mount.name,
                    handoff.as_str()
                ));
            }
        }

        for group in &spec.groups {
            command.arg("--group").arg(format!(
                "{}={},{},{},{}",
                group.name, group.processes, group.threads, group.sync_threads, group.concurrency
            ));

            if group.optional {
                command.arg("--optional-group").arg(&group.name);
            }

            if !group.mounts.is_empty() {
                command.arg("--group-mounts").arg(format!(
                    "{}={}",
                    group.name,
                    group.mounts.join(",")
                ));
            }

            // Section 7.1: the group's own timeouts and memory policy,
            // every value spelled out, the release's where it set none.

            let seconds = |ms: Option<u64>| {
                ms.map_or_else(|| "off".to_owned(), |ms| (ms as f64 / 1000.0).to_string())
            };
            let timeouts = &group.timeouts;
            command.arg("--group-timeouts").arg(format!(
                "{}={},{},{},{},{}",
                group.name,
                seconds(timeouts.soft_timeout_ms),
                seconds(timeouts.hard_timeout_ms),
                seconds(Some(timeouts.hard_drain_grace_ms)),
                seconds(Some(timeouts.loop_stall_timeout_ms)),
                seconds(Some(timeouts.gil_stall_timeout_ms)),
            ));
            let memory = &group.memory;
            command.arg("--group-memory").arg(format!(
                "{}={},{},{},{},{},{}",
                group.name,
                memory.measure.as_str(),
                memory
                    .soft_limit
                    .map_or_else(|| "off".to_owned(), |bytes| bytes.to_string()),
                seconds(Some(memory.soft_for_ms)),
                memory.reclaim_first,
                memory
                    .hard_limit
                    .map_or_else(|| "off".to_owned(), |bytes| bytes.to_string()),
                seconds(Some(memory.check_interval_ms)),
            ));
            command.arg("--group-recycle").arg(format!(
                "{}={},{}",
                group.name,
                group
                    .max_requests
                    .map_or_else(|| "off".to_owned(), |count| count.to_string()),
                group.max_lifetime.map_or_else(
                    || "off".to_owned(),
                    |lifetime| lifetime.as_secs_f64().to_string()
                ),
            ));
        }

        if let Some(level) = spec.logging.level {
            command.arg("--log-level").arg(level.as_str());
        }

        if let Some(format) = &spec.logging.format {
            command.arg("--log-format").arg(format);
        }

        if spec.logging.capture_warnings == Some(true) {
            command.arg("--capture-warnings");
        }

        for (name, level) in &spec.logging.loggers {
            command
                .arg("--logger")
                .arg(format!("{name}={}", level.as_str()));
        }

        for filter in &spec.logging.warnings {
            command.arg("--warning").arg(filter);
        }

        let workers = spec.processes
            + spec
                .groups
                .iter()
                .map(|group| group.processes)
                .sum::<usize>();
        let base_port = spec
            .tcp_base_port
            .map(|base| base.wrapping_add((release - 1) as u16 * workers as u16));

        if let Some(base) = base_port {
            command.arg("--tcp-base-port").arg(base.to_string());
        }

        // Section 7.6 and 7.7: the host enforces these on its workers and
        // tells this process about replacements over its control socket.

        command
            .arg("--control")
            .arg(run_dir.join("control.sock"))
            .arg("--hard-drain-grace")
            .arg(spec.hard_drain_grace.as_secs_f64().to_string())
            .arg("--shutdown-timeout")
            .arg(spec.shutdown_timeout.as_secs_f64().to_string())
            .arg("--websocket-drain-grace")
            .arg(spec.websocket_drain_grace.as_secs_f64().to_string())
            .arg("--exit-grace")
            .arg(spec.exit_grace.as_secs_f64().to_string())
            .arg("--loop-stall-timeout")
            .arg(spec.loop_stall_timeout.as_secs_f64().to_string())
            .arg("--gil-stall-timeout")
            .arg(spec.gil_stall_timeout.as_secs_f64().to_string());

        if let Some(count) = spec.max_requests {
            command.arg("--max-requests").arg(count.to_string());
        }

        if let Some(lifetime) = spec.max_lifetime {
            command
                .arg("--max-lifetime")
                .arg(lifetime.as_secs_f64().to_string());
        }

        if let Some(timeout) = spec.soft_timeout {
            command
                .arg("--soft-timeout")
                .arg(timeout.as_secs_f64().to_string());
        }

        if let Some(timeout) = spec.hard_timeout {
            command
                .arg("--hard-timeout")
                .arg(timeout.as_secs_f64().to_string());
        }

        // Section 7.7: the memory policy.

        command
            .arg("--memory-measure")
            .arg(spec.memory.measure.as_str())
            .arg("--memory-soft-for")
            .arg((spec.memory.soft_for_ms as f64 / 1000.0).to_string())
            .arg("--memory-reclaim-first")
            .arg(spec.memory.reclaim_first.to_string())
            .arg("--memory-check-interval")
            .arg((spec.memory.check_interval_ms as f64 / 1000.0).to_string());

        if let Some(bytes) = spec.memory.soft_limit {
            command.arg("--memory-soft-limit").arg(bytes.to_string());
        }

        if let Some(bytes) = spec.memory.hard_limit {
            command.arg("--memory-hard-limit").arg(bytes.to_string());
        }

        if let Some(dir) = &spec.cgroup {
            command.arg("--cgroup").arg(dir);
        }

        if let Some(dir) = &spec.chdir {
            command.current_dir(dir);
        }

        // Section 8.2: where bytecode goes, when it is not left to Python.
        // The release's setting holds whatever the daemon inherited, but
        // its own environment may still turn bytecode off.

        if spec.clean_env {
            command.env_clear();
        }

        for name in crate::commands::not_inherited(spec.bytecode) {
            command.env_remove(name);
        }

        command.envs(spec.env.iter().map(|(name, value)| (name, value)));

        // Section 6.15: the instrumentation list, which the host takes out
        // of its environment before it imports anything.

        if spec.instrumentation.is_empty() {
            command.env_remove(INSTRUMENTATION_VARIABLE);
        } else {
            command.env(
                INSTRUMENTATION_VARIABLE,
                serde_json::to_string(&spec.instrumentation)
                    .context("the instrumentation list could not be written")?,
            );
        }

        // Section 11.7: the release's variables whose values debug
        // inspect shows, beside Muru's own and the interpreter's.

        command.env(SHOWN_ENV_VARIABLE, spec.shown_env.join(","));

        if let Some(prefix) = &spec.pycache_prefix {
            command.env("PYTHONPYCACHEPREFIX", prefix);
        }

        if spec.bytecode == Some(muru_config::Bytecode::Off) {
            command.env("PYTHONDONTWRITEBYTECODE", "1");
        }

        // Section 8.3: the runtime library first on PYTHONPATH, ahead of
        // whatever the environment or the settings put there.

        if let Some(runtime) = &spec.pythonpath {
            let existing = spec
                .env
                .iter()
                .find(|(name, _)| name == "PYTHONPATH")
                .map(|(_, value)| value.clone())
                .or_else(|| std::env::var("PYTHONPATH").ok())
                .filter(|value| !value.is_empty());

            let value = match existing {
                Some(existing) => format!("{}:{existing}", runtime.display()),
                None => runtime.display().to_string(),
            };

            command.env("PYTHONPATH", value);
        }

        // The relay first, so that the host's first line has a reader.

        let launch = RelayLaunch::new(spec, &run_dir)?;
        let relays: SharedRelays = Arc::default();
        let first = launch
            .spawn(Some(output_read.as_raw_fd()), logs, &relays)
            .context("starting the release's log relay")?;

        let mut child = command
            .spawn()
            .with_context(|| format!("starting the host with {}", spec.python.display()))?;

        // Both ends of the pipe now belong to the two processes, but for
        // the read end kept under --single.

        drop(command);
        let kept_output = spec.single.then(|| OwnedFd::from(output_read));

        // The host exits when this end of its stdin closes, whatever happens here.

        let stdin = child.stdin.take();
        let relay = RelaySupervisor::start(
            launch,
            RelayRunning::Child(first),
            kept_output,
            logs,
            &relays,
            watcher,
        );

        // Sockets are numbered across every group in the order the host
        // forks them: the default group first, then each named group.

        let socket = |group: Option<&str>, index: usize, position: usize| match base_port {
            None => match group {
                None => run_dir.join(format!("worker-{index}.sock")),
                Some(name) => run_dir.join(format!("worker-{name}-{index}.sock")),
            },
            Some(base) => {
                let port = base.wrapping_add(u16::try_from(position).unwrap_or(0));
                PathBuf::from(format!("127.0.0.1:{port}"))
            }
        };
        let mut sockets: Vec<PathBuf> = (0..spec.processes)
            .map(|index| socket(None, index, index))
            .collect();
        let mut group_sockets = Vec::new();

        for group in &spec.groups {
            let mut these = Vec::new();

            for index in 0..group.processes {
                let path = socket(Some(&group.name), index, sockets.len());
                sockets.push(path.clone());
                these.push(path);
            }

            group_sockets.push((group.name.clone(), these));
        }

        Ok(Self {
            process: Process::Child(child),
            stdin,
            sockets,
            group_sockets,
            optional: spec
                .groups
                .iter()
                .filter(|group| group.optional)
                .map(|group| group.name.clone())
                .collect(),
            run_dir,
            release,
            relays,
            relay,
        })
    }

    /// Take over a host a previous daemon started: it is checked to be
    /// alive and its workers to be listening, and is stopped with signals
    /// rather than waited for, since it is not this process's child. Its
    /// relay is adopted with it when `relay_pid` names one still there;
    /// otherwise a new one is started and given the host's pipes.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        pid: u32,
        sockets: Vec<PathBuf>,
        group_sockets: Vec<(String, Vec<PathBuf>)>,
        optional: Vec<String>,
        run_dir: PathBuf,
        release: u32,
        launch: RelayLaunch,
        relay_pid: Option<u32>,
        watcher: RelayWatcher,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(alive(pid), "process {pid} is gone");
        let relays: SharedRelays = Arc::default();

        let running = match relay_pid {
            Some(relay_pid) => RelayRunning::Adopted(relay_pid),
            None => RelayRunning::Gone,
        };

        let relay =
            RelaySupervisor::start(launch, running, None, &Logs::default(), &relays, watcher);
        let host = Self {
            process: Process::Adopted(pid),
            stdin: None,
            sockets,
            group_sockets,
            optional,
            run_dir,
            release,
            relays,
            relay,
        };
        anyhow::ensure!(
            host.required_sockets()
                .iter()
                .all(|socket| listening(socket)),
            "the workers of process {pid} are not listening"
        );

        Ok(host)
    }

    /// The sockets the release is not ready without: every group's but
    /// the optional groups' (ADR 0033).
    fn required_sockets(&self) -> Vec<PathBuf> {
        let optional: Vec<&PathBuf> = self
            .group_sockets
            .iter()
            .filter(|(name, _)| self.optional.contains(name))
            .flat_map(|(_, sockets)| sockets)
            .collect();

        self.sockets
            .iter()
            .filter(|socket| !optional.contains(socket))
            .cloned()
            .collect()
    }

    /// Keep of an optional group's planned sockets the ones a worker
    /// listens on: the others join as the host announces them.
    fn settle_optional(&mut self) {
        // The default group's sockets are counted from the named groups'
        // total, so they are taken before any group is pruned.

        let default = self.default_sockets();

        for (name, sockets) in &mut self.group_sockets {
            if self.optional.contains(name) {
                sockets.retain(|socket| listening(socket));
            }
        }

        let named = self
            .group_sockets
            .iter()
            .flat_map(|(_, sockets)| sockets.iter().cloned());
        self.sockets = default.into_iter().chain(named).collect();
    }

    /// The default group's sockets: those of no named group.
    pub fn default_sockets(&self) -> Vec<PathBuf> {
        let named: usize = self
            .group_sockets
            .iter()
            .map(|(_, sockets)| sockets.len())
            .sum();

        self.sockets[..self.sockets.len() - named].to_vec()
    }

    /// The host's pid.
    pub fn pid(&self) -> Option<u32> {
        match &self.process {
            Process::Child(child) => child.id(),
            Process::Adopted(pid) => Some(*pid),
        }
    }

    /// Send the host a signal. Returns whether it was delivered.
    pub fn signal(&self, signal: i32) -> bool {
        let Some(pid) = self.pid() else {
            return false;
        };

        // SAFETY: kill with a pid this process started or adopted.

        unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
    }

    /// The release run directory.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Where the host serves its control connection (section 5.9): the
    /// worker sockets as they change, and events about its workers.
    pub fn control_path(&self) -> PathBuf {
        self.run_dir.join("control.sock")
    }

    /// Wait until every worker the release needs listens, or the host
    /// exits or the time is up. An optional group's workers are not
    /// waited for: the ones listening by then are kept, and the rest
    /// join as the host announces them (ADR 0033).
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let started = Instant::now();
        let required = self.required_sockets();

        loop {
            if required.iter().all(|socket| listening(socket)) {
                self.settle_optional();
                return Ok(());
            }

            if let Some(status) = self.try_exited()? {
                anyhow::bail!("the host {status} before its workers were ready");
            }

            anyhow::ensure!(
                started.elapsed() < timeout,
                "the workers did not start within {} seconds",
                timeout.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Resolve when the host exits, once what it wrote last is written.
    pub async fn exited(&mut self) -> std::io::Result<ExitStatus> {
        match &mut self.process {
            Process::Child(child) => {
                let status = child.wait().await;
                relayed_to_the_end(&self.relays).await;
                status
            }
            Process::Adopted(pid) => {
                let pid = *pid;

                while alive(pid) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                Ok(ExitStatus::default())
            }
        }
    }

    /// Whether the host has exited, without waiting: a sentence saying how
    /// when it has.
    pub fn try_exited(&mut self) -> std::io::Result<Option<String>> {
        match &mut self.process {
            Process::Child(child) => Ok(child
                .try_wait()?
                .map(|status| format!("exited with {status}"))),
            Process::Adopted(pid) => Ok((!alive(*pid)).then(|| "is gone".to_owned())),
        }
    }

    /// Stop the host: SIGTERM and its stdin closed, then SIGKILL after a
    /// grace period. Its workers finish the requests they are serving.
    /// Stop the host and its workers. The host gives its workers `budget`,
    /// the WebSockets' grace, the shutdown timeout and the exit grace, before it kills
    /// them, and is itself killed if it has not exited `STOP_MARGIN`
    /// after that.
    pub async fn stop(self, budget: Duration) {
        self.stop_until(Instant::now() + budget + STOP_MARGIN, None)
            .await;
    }

    /// Stop the host as `stop` does, killing it at `kill_at` if it is
    /// still there: for a shutdown that has a budget of its own (section
    /// 13.3), which may be shorter than the workers' stop budget. With
    /// `workers_by`, the host is told when the shutdown wants its workers
    /// gone, which it tells each of them, so that a worker told to stop
    /// late in a shutdown still leaves itself the exit grace.
    pub async fn stop_until(mut self, kill_at: Instant, workers_by: Option<Instant>) {
        if let Some(workers_by) = workers_by {
            self.tell_stop_by(workers_by);
        }

        if let Some(pid) = self.pid() {
            // SAFETY: kill(2) with a valid pid and signal has no memory-safety
            // preconditions; the pid is a host this daemon started or adopted.

            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }

        drop(self.stdin.take());

        let grace = kill_at.saturating_duration_since(Instant::now());

        match &mut self.process {
            Process::Child(child) => {
                if tokio::time::timeout(grace, child.wait()).await.is_err() {
                    if let Some(pid) = child.id() {
                        kill_with_workers(pid);
                    }

                    let _ = child.wait().await;
                }
            }
            Process::Adopted(pid) => {
                let pid = *pid;
                let started = Instant::now();

                while alive(pid) && started.elapsed() < grace {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                if alive(pid) {
                    kill_with_workers(pid);
                }
            }
        }

        // The relay writes the host's last lines and exits; then what it
        // wrote to this process is read to the end.

        self.relay.finish().await;
        relayed_to_the_end(&self.relays).await;

        let _ = std::fs::remove_dir_all(&self.run_dir);
    }

    /// Write when the workers must be gone where the host looks for it, by
    /// the clock both sides read. Before the signal, so the host finds it
    /// whenever the signal lands. A host that cannot read it stops by its
    /// own budget and is killed at the shutdown's deadline.
    fn tell_stop_by(&self, workers_by: Instant) {
        let at = SystemTime::now() + workers_by.saturating_duration_since(Instant::now());
        let path = self.run_dir.join(STOP_BY_FILE);
        let Ok(since_epoch) = at.duration_since(SystemTime::UNIX_EPOCH) else {
            return;
        };

        if let Err(error) = std::fs::write(&path, since_epoch.as_millis().to_string()) {
            tracing::warn!(%error, "could not tell the host when its workers must be gone");
            return;
        }

        // The host may run as another user than this process.

        let permissions = std::os::unix::fs::PermissionsExt::from_mode(0o644);
        let _ = std::fs::set_permissions(&path, permissions);
    }

    /// The pid of the release's log relay, while one runs.
    pub fn relay_pid(&self) -> Option<u32> {
        let pid = self.relay.pid();
        (pid != 0).then_some(pid)
    }

    /// Have the release's log relay open the release log again by name,
    /// with SIGUSR1. When it was told, gives what changes once it reports
    /// that it has, for a caller that waits for the file to be reopened.
    ///
    /// With `workers`, the host is then told, with SIGUSR2, to have its
    /// workers run their on_reopen_logs hook, once the relay reports that
    /// it has reopened the file, so what the hooks write goes to the new
    /// one. A relay that does not report within `RELAY_REOPEN_WAIT`, or
    /// that was not running, does not hold the workers back.
    pub fn reopen_log(&self, workers: bool) -> Option<tokio::sync::watch::Receiver<u64>> {
        let answer = self.relay.reopens.subscribe();
        let mut reopened = self.relay.reopens.subscribe();
        let told = signal_process(self.relay.pid(), libc::SIGUSR1);

        if workers {
            let host = self.pid().unwrap_or(0);

            tokio::spawn(async move {
                if told {
                    let _ = tokio::time::timeout(RELAY_REOPEN_WAIT, reopened.changed()).await;
                }

                signal_process(host, libc::SIGUSR2);
            });
        }

        told.then_some(answer)
    }

    /// Lines of the release's output dropped so far, across its relays.
    pub fn output_dropped(&self) -> u64 {
        self.relay.dropped()
    }

    /// Leave the host running and forget it, for a daemon that detaches.
    pub fn release(self) {
        self.relay.release();
        drop(self.stdin);

        if let Process::Child(mut child) = self.process {
            // The child must not be killed when its handle drops.

            let _ = child.try_wait();
            std::mem::forget(child);
        }
    }
}

/// Kill a host that has outstayed its grace, and its workers with it when
/// it leads a process group of its own (a daemon's host does, through
/// `own_session`), so a host killed before it could stop them leaves no
/// worker running on (section 13.3).
fn kill_with_workers(pid: u32) {
    let pid = pid as libc::pid_t;

    // SAFETY: getpgid(2) and kill(2) with a valid pid and signal have no
    // memory-safety preconditions; the pid is a host this daemon started
    // or adopted, and the group is killed only when the host leads it.

    unsafe {
        if libc::getpgid(pid) == pid {
            libc::kill(-pid, libc::SIGKILL);
        }

        libc::kill(pid, libc::SIGKILL);
    }
}

/// Run a child in a session of its own, so the daemon's signals and
/// terminal do not reach it, and as `identity` when one is set (section
/// 10.9).
pub(crate) fn own_session(command: &mut Command, identity: Option<Identity>) {
    own_session_std(command.as_std_mut(), identity);
}

/// Send a signal to the session a process leads, which `own_session`
/// gave it. Returns whether it was delivered.
pub(crate) fn signal_group(pid: u32, signal: i32) -> bool {
    // SAFETY: kill with the negated pid of a process this one started in
    // a session of its own, whose process group has the same id. Pid 0
    // would name this process's own group and is never sent to.

    pid != 0 && unsafe { libc::kill(-(pid as libc::pid_t), signal) == 0 }
}

/// Send a signal to one process. Returns whether it was delivered.
/// Signal 0 sends nothing and so asks whether the process is there.
pub(crate) fn signal_process(pid: u32, signal: i32) -> bool {
    // SAFETY: kill(2) has no memory-safety preconditions. Pid 0 would
    // name this process's own group and is never sent to.

    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

/// Whether a process leads a process group of its own, as one that
/// `own_session` started does.
pub(crate) fn leads_group(pid: u32) -> bool {
    // SAFETY: getpgid(2) has no memory-safety preconditions.

    pid != 0 && unsafe { libc::getpgid(pid as libc::pid_t) == pid as libc::pid_t }
}

/// `own_session` for a standard library command, as a build runs.
pub(crate) fn own_session_std(command: &mut std::process::Command, identity: Option<Identity>) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setsid, setgroups, setgid, setuid and umask are
    // async-signal-safe and the closure runs in the child between fork
    // and exec, touching nothing else.

    unsafe {
        command.pre_exec(move || {
            libc::setsid();

            if let Some(identity) = identity {
                let groups = [identity.gid as libc::gid_t];

                if libc::setgroups(1, groups.as_ptr()) != 0
                    || libc::setgid(identity.gid as libc::gid_t) != 0
                    || libc::setuid(identity.uid as libc::uid_t) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                if let Some(mask) = identity.umask {
                    libc::umask(mask as libc::mode_t);
                }
            }

            Ok(())
        });
    }
}

/// Whether a process exists. A child of this process that has exited is
/// reaped here and reported gone: after an upgrade (section 13.4) the
/// hosts and the edge are children the previous image started, which
/// nothing else waits for.
pub fn alive(pid: u32) -> bool {
    // Pid 0 is no process: as a pid it names this process's own group,
    // which is always there, and waiting on it would reap any child.

    if pid == 0 {
        return false;
    }

    let mut status = 0;

    // SAFETY: waitpid with WNOHANG on one pid returns at once and reaps
    // the process only when it is a child of this one that has exited;
    // kill with signal 0 checks for the process and sends nothing.

    unsafe {
        if libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) == pid as libc::pid_t {
            return false;
        }

        libc::kill(pid as libc::pid_t, 0) == 0
    }
}

/// When the kernel says a process started, as a number that means
/// nothing but is the same every time it is read for one process and
/// differs for another process given the same pid later. None when it
/// cannot be read: no such process, or no way to ask on this system.
///
/// A pid alone does not name a process across time, since pids are used
/// again; a pid and this together do, which is how a record of a
/// process is checked against whatever has its pid now.
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }

    read_start_time(pid)
}

/// On Linux the start time is `starttime` in `/proc/PID/stat`: clock
/// ticks since the system booted.
#[cfg(target_os = "linux")]
fn read_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    start_time_in_stat(&stat)
}

/// On macOS the start time is in the process's BSD information: seconds
/// and microseconds, combined as microseconds.
#[cfg(target_os = "macos")]
fn read_start_time(pid: u32) -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;

    // SAFETY: proc_bsdinfo is plain data for which all zeroes is a
    // valid value.

    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };

    // SAFETY: proc_pidinfo writes at most `size` bytes to the buffer it
    // is given, which is a proc_bsdinfo of that size that outlives the
    // call.

    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };

    if written != size {
        return None;
    }

    Some(
        info.pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec),
    )
}

/// Elsewhere there is no start time to read.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_start_time(_pid: u32) -> Option<u64> {
    None
}

/// The `starttime` field of a `/proc/PID/stat` line, its 22nd. The
/// second field is the command's name in parentheses, which may itself
/// hold spaces and parentheses, so the fields are counted from the last
/// closing one: the state is the first after it and the start time the
/// 20th.
#[cfg(any(target_os = "linux", test))]
fn start_time_in_stat(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name.split_whitespace().nth(19)?.parse().ok()
}

/// Whether a worker accepts connections on its socket. The edge connects
/// to every worker as soon as the host is ready, so this connects too: a
/// Unix socket file exists from bind, a moment before listen.
fn listening(socket: &Path) -> bool {
    match socket.to_str() {
        Some(address) if address.starts_with("127.0.0.1:") => {
            std::net::TcpStream::connect(address).is_ok()
        }
        _ => std::os::unix::net::UnixStream::connect(socket).is_ok(),
    }
}

/// Copy a host stream to the matching stream of this process, line by
/// line, keeping each line for the dashboard.
async fn relay<R: AsyncRead + Unpin>(stream: R, logs: Logs, to_stderr: bool) {
    let mut lines = BufReader::new(stream).lines();

    let out = if to_stderr {
        Stream::Stderr
    } else {
        Stream::Stdout
    };

    // Section 11.8: the time is the relay's, put in front when the
    // terminal asks for it with --timestamps. When lines are
    // JSON records, each source's traceback is gathered and written as
    // one record, once its exception's line is followed by something
    // else or nothing more has come for a moment.

    let mut gathering: HashMap<String, Gathering> = HashMap::new();

    loop {
        // None is a moment with nothing read, which only lets stale
        // tracebacks go.

        let next = if gathering.is_empty() {
            Some(lines.next_line().await)
        } else {
            tokio::time::timeout(TRACEBACK_WAIT, lines.next_line())
                .await
                .ok()
        };

        let line = match next {
            None => None,
            Some(Ok(Some(line))) => Some(line),
            Some(_) => break,
        };

        let stale: Vec<String> = gathering
            .iter()
            .filter(|(_, gathered)| {
                gathered
                    .updated
                    .elapsed()
                    .is_ok_and(|age| age >= TRACEBACK_WAIT)
            })
            .map(|(source, _)| source.clone())
            .collect();

        for source in stale {
            if let Some(mut gathered) = gathering.remove(&source) {
                write_traceback(out, &source, &mut gathered).await;
            }
        }

        let Some(line) = line else {
            continue;
        };

        // The log relay put the time on the line when it read it, so a
        // delay on the way here does not move it.

        let (stamp, text) = muru_common::log::split_time(&line);
        let time = stamp
            .and_then(muru_common::log::parse_full)
            .unwrap_or_else(SystemTime::now);
        let line = text.to_owned();
        logs.push(time, &line);

        if !muru_common::log::json() {
            write_relayed(out, time, &line).await;
            continue;
        }

        let (source, message) = match line.split_once(": ") {
            Some((source, message)) if source.starts_with("muru.") => (source, message),
            _ => ("", line.as_str()),
        };

        if let Some(gathered) = gathering.get_mut(source) {
            match gathered.traceback.push(message) {
                Gathered::Kept => {
                    gathered.updated = time;
                    continue;
                }
                Gathered::Finished(text) => {
                    write_relayed(out, gathered.started, &tagged(source, &text)).await;
                    gathering.remove(source);
                }
            }
        }

        if Traceback::starts(message) {
            gathering.insert(
                source.to_owned(),
                Gathering {
                    started: time,
                    updated: time,
                    traceback: Traceback::new(message),
                },
            );
            continue;
        }

        write_relayed(out, time, &line).await;
    }

    for (source, gathered) in &mut gathering {
        write_traceback(out, source, gathered).await;
    }
}

/// How long a traceback waits for its next line before it is written as
/// it is.
const TRACEBACK_WAIT: Duration = Duration::from_millis(200);

/// One source's traceback being gathered.
struct Gathering {
    /// When its first line arrived, the time of the record.
    started: SystemTime,
    /// When its last line arrived.
    updated: SystemTime,
    traceback: Traceback,
}

async fn write_traceback(out: Stream, source: &str, gathered: &mut Gathering) {
    let text = gathered.traceback.take();
    write_relayed(out, gathered.started, &tagged(source, &text)).await;
}

/// What the relays hand the thread that writes their lines.
enum Relayed {
    /// A line, newline included, for one of this process's streams.
    Line(Stream, String),
    /// Answered once every line handed over before it is written.
    Flushed(tokio::sync::oneshot::Sender<()>),
}

/// How many lines may wait for the writer before a relay waits for room.
const RELAY_QUEUE: usize = 1024;

/// The most a batch of one stream's lines holds in one write.
const RELAY_BATCH: usize = 64 * 1024;

/// The thread that writes what the relays read, so that a terminal or a
/// container runtime that stops reading holds up that thread alone,
/// never one of the runtime's: a relay awaits room in the channel, and
/// its host's pipe fills behind it. None when the thread could not be
/// started, and the relays write themselves.
fn relay_writer() -> Option<&'static mpsc::Sender<Relayed>> {
    static WRITER: std::sync::OnceLock<Option<mpsc::Sender<Relayed>>> = std::sync::OnceLock::new();

    WRITER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel(RELAY_QUEUE);

            std::thread::Builder::new()
                .name("muru-relay-writer".to_owned())
                .spawn(move || write_relays(receiver))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Write what the relays hand over, gathering a stream's lines that are
/// already waiting into one write of up to `RELAY_BATCH` bytes.
fn write_relays(mut receiver: mpsc::Receiver<Relayed>) {
    let mut next = receiver.blocking_recv();

    while let Some(relayed) = next.take() {
        let (stream, mut batch) = match relayed {
            Relayed::Line(stream, line) => (stream, line),
            Relayed::Flushed(answer) => {
                let _ = answer.send(());
                next = receiver.blocking_recv();
                continue;
            }
        };

        // Gather what is already waiting for the same stream; anything
        // else waits its turn, so the order is kept.

        while batch.len() < RELAY_BATCH {
            match receiver.try_recv() {
                Ok(Relayed::Line(same, line)) if same == stream => batch.push_str(&line),
                Ok(other) => {
                    next = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }

        muru_common::log::write(stream, batch.as_bytes());

        if next.is_none() {
            next = receiver.blocking_recv();
        }
    }
}

/// Hand a relayed line to the writer, waiting while its channel is full.
async fn write_relayed(out: Stream, time: SystemTime, text: &str) {
    let line = muru_common::log::relayed_line(out, time, text);

    match relay_writer() {
        Some(writer) => {
            if let Err(mpsc::error::SendError(Relayed::Line(stream, line))) =
                writer.send(Relayed::Line(out, line)).await
            {
                muru_common::log::write(stream, line.as_bytes());
            }
        }
        None => muru_common::log::write(out, line.as_bytes()),
    }
}

/// How long a host's relays may take to reach the end of its output once
/// it has exited: a process it left behind may hold its streams open.
const RELAY_END: Duration = Duration::from_secs(2);

/// Wait for a host's relays to read the last of its output, and for the
/// writer to write it, so that a host's last lines come before whatever
/// this process writes next.
async fn relayed_to_the_end(relays: &SharedRelays) {
    let taken = std::mem::take(&mut *relays.lock().expect("relays poisoned"));

    for relay in taken {
        let _ = tokio::time::timeout(RELAY_END, relay).await;
    }

    let Some(writer) = relay_writer() else {
        return;
    };

    let (answer, written) = tokio::sync::oneshot::channel();

    if writer.send(Relayed::Flushed(answer)).await.is_ok() {
        let _ = tokio::time::timeout(RELAY_END, written).await;
    }
}

/// What a release's log relay queues and does when full (section 11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelaySettings {
    /// Each queue's bound in bytes (`[logs] output_queue`).
    pub queue: usize,
    /// What a full queue does (`[logs] on_overflow`).
    pub overflow: crate::relay::Overflow,
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self {
            queue: crate::relay::DEFAULT_QUEUE,
            overflow: crate::relay::Overflow::default(),
        }
    }
}

/// What a relay reports, for whoever runs the release.
#[derive(Debug, Clone)]
pub enum RelayNews {
    /// Lines of the release's output dropped so far.
    Dropped(u64),
    /// A stall began or ended, or a relay was started again.
    Event {
        /// The event's kind.
        kind: String,
        /// `warning` or `info`.
        level: String,
        /// What happened, in words.
        message: String,
    },
}

/// Hears what a release's relay reports.
pub type RelayWatcher = Arc<dyn Fn(RelayNews) + Send + Sync>;

/// How a release's relay is started, kept to start another the same way.
#[derive(Debug, Clone)]
pub struct RelayLaunch {
    /// The executable, this program.
    executable: PathBuf,
    /// The directory the host's and relay's sockets are in.
    run_dir: PathBuf,
    /// The release log, or none to write to this process.
    log_file: Option<PathBuf>,
    settings: RelaySettings,
    identity: Option<Identity>,
    /// The host serves in its own process, with no socket to connect to.
    single: bool,
    /// The release, as app@release, for the relay's process title.
    release: String,
}

impl RelayLaunch {
    /// How to start the relay of a host started with `spec`.
    fn new(spec: &HostSpec, run_dir: &Path) -> anyhow::Result<Self> {
        let launch = Self::for_release(
            run_dir.to_owned(),
            spec.detach_to.clone(),
            spec.relay,
            spec.identity,
            spec.label(run_dir),
        )?;

        Ok(Self {
            single: spec.single,
            ..launch
        })
    }

    /// How to start the relay of a release whose sockets are in
    /// `run_dir`, writing `log_file` or else to this process.
    pub fn for_release(
        run_dir: PathBuf,
        log_file: Option<PathBuf>,
        settings: RelaySettings,
        identity: Option<Identity>,
        release: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            executable: crate::daemon::running_executable()
                .context("finding the executable to start the relay from")?,
            run_dir,
            log_file,
            settings,
            identity,
            single: false,
            release,
        })
    }

    fn output_socket(&self) -> PathBuf {
        self.run_dir.join(crate::relay::OUTPUT_SOCKET)
    }

    fn control_socket(&self) -> PathBuf {
        self.run_dir.join(crate::relay::CONTROL_SOCKET)
    }

    /// Start a relay, handing it the read end of the host's output when
    /// `host_output` gives one. Without a release log what it writes
    /// comes to this process and is relayed on to its streams.
    fn spawn(
        &self,
        host_output: Option<RawFd>,
        logs: &Logs,
        relays: &SharedRelays,
    ) -> anyhow::Result<Child> {
        let mut command = Command::new(&self.executable);
        command
            .arg("relay")
            .arg("run")
            .arg("--control")
            .arg(self.control_socket())
            .arg("--queue")
            .arg(self.settings.queue.to_string())
            .arg("--on-overflow")
            .arg(self.settings.overflow.as_str())
            .arg("--release")
            .arg(&self.release)
            .stdin(Stdio::null())
            .stdout(Stdio::null());

        if !self.single {
            command.arg("--connect").arg(self.output_socket());
        }

        if let Some(fd) = host_output {
            crate::fdpass::inherit_at(command.as_std_mut(), fd, crate::relay::HOST_OUTPUT_FD);
            command
                .arg("--host-output-fd")
                .arg(crate::relay::HOST_OUTPUT_FD.to_string());
        }

        match &self.log_file {
            Some(log) => {
                // The relay's own failure to start goes where its lines go.

                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .with_context(|| format!("opening {}", log.display()))?;
                command.stderr(file).arg("--log-file").arg(log);
                own_session(&mut command, self.identity);
            }
            None => {
                command.stderr(Stdio::piped());
                command.process_group(0);
            }
        }

        let mut child = command.spawn().context("running muructl relay")?;

        if let Some(stderr) = child.stderr.take() {
            let reading = tokio::spawn(relay(stderr, logs.clone(), true));
            relays.lock().expect("relays poisoned").push(reading);
        }

        if let Some(pid) = child.id() {
            self.record(pid);
        }

        Ok(child)
    }

    /// Say which process the relay is, for a daemon that adopts it.
    fn record(&self, pid: u32) {
        let path = self.run_dir.join(crate::relay::RECORD);

        if let Err(error) = std::fs::write(&path, format!("pid = {pid}\n")) {
            tracing::warn!(%error, "could not record the release's log relay");
        }
    }
}

/// The relay a supervisor is looking after.
enum RelayRunning {
    /// One this process started.
    Child(Child),
    /// One a previous daemon started.
    Adopted(u32),
    /// None: one is to be started.
    Gone,
}

impl RelayRunning {
    fn pid(&self) -> u32 {
        match self {
            Self::Child(child) => child.id().unwrap_or(0),
            Self::Adopted(pid) => *pid,
            Self::Gone => 0,
        }
    }

    /// Wait for the relay to exit.
    async fn exited(&mut self) {
        match self {
            Self::Child(child) => {
                let _ = child.wait().await;
            }
            Self::Adopted(pid) => {
                while alive(*pid) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Self::Gone => {}
        }
    }
}

/// The task that looks after a release's relay, and what it shares.
struct RelaySupervisor {
    pid: Arc<AtomicU32>,
    dropped: Arc<AtomicU64>,
    /// Counts the reopens the relay has reported.
    reopens: Arc<tokio::sync::watch::Sender<u64>>,
    stopping: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// How long a stop waits for the relay to write what it holds and exit,
/// before telling it to.
const RELAY_DRAIN: Duration = Duration::from_secs(5);

/// How long a relay told to stop gets to do so before it is killed.
const RELAY_TERM: Duration = Duration::from_secs(3);

/// How long a reopen with the workers waits for the relay to report that
/// it has reopened the release log before the workers are told anyway.
const RELAY_REOPEN_WAIT: Duration = Duration::from_secs(5);

/// A relay that lasted this long is not failing: the wait before the
/// next is back to its shortest.
const RELAY_STEADY: Duration = Duration::from_secs(60);

/// The longest wait before starting another relay.
const RELAY_BACKOFF_MAX: Duration = Duration::from_secs(30);

impl RelaySupervisor {
    fn start(
        launch: RelayLaunch,
        running: RelayRunning,
        kept_output: Option<OwnedFd>,
        logs: &Logs,
        relays: &SharedRelays,
        watcher: RelayWatcher,
    ) -> Self {
        let pid = Arc::new(AtomicU32::new(running.pid()));
        let dropped = Arc::new(AtomicU64::new(0));
        let reopens = Arc::new(tokio::sync::watch::channel(0).0);
        let (stopping, stop) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(supervise_relay(
            launch,
            running,
            kept_output,
            logs.clone(),
            Arc::clone(relays),
            watcher,
            Arc::clone(&pid),
            Arc::clone(&dropped),
            Arc::clone(&reopens),
            stop,
        ));

        Self {
            pid,
            dropped,
            reopens,
            stopping,
            task,
        }
    }

    fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Let the relay finish now that the host is done: it writes what it
    /// holds and exits, and is told to if it takes too long.
    async fn finish(mut self) {
        let _ = self.stopping.send(true);

        if tokio::time::timeout(RELAY_DRAIN, &mut self.task)
            .await
            .is_ok()
        {
            return;
        }

        signal_process(self.pid(), libc::SIGTERM);

        if tokio::time::timeout(RELAY_TERM, &mut self.task)
            .await
            .is_ok()
        {
            return;
        }

        signal_process(self.pid(), libc::SIGKILL);
        let _ = tokio::time::timeout(RELAY_TERM, self.task).await;
    }

    /// Leave the relay running, for a daemon that detaches.
    fn release(self) {
        self.task.abort();
    }
}

/// Look after a release's relay: read what it reports while it runs,
/// start another when it exits and the host still listens for one, and
/// end when told the host is done and the relay has gone.
#[allow(clippy::too_many_arguments)]
async fn supervise_relay(
    launch: RelayLaunch,
    mut running: RelayRunning,
    kept_output: Option<OwnedFd>,
    logs: Logs,
    relays: SharedRelays,
    watcher: RelayWatcher,
    pid: Arc<AtomicU32>,
    dropped: Arc<AtomicU64>,
    reopens: Arc<tokio::sync::watch::Sender<u64>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut wait = Duration::from_secs(1);

    // What relays that have gone had dropped, and what the one running
    // has, so the total never goes back.

    let mut carried = 0u64;

    loop {
        let started = Instant::now();
        let current = Arc::new(AtomicU64::new(0));
        let reports = tokio::spawn(read_relay_reports(
            launch.control_socket(),
            carried,
            Arc::clone(&current),
            Arc::clone(&dropped),
            Arc::clone(&reopens),
            Arc::clone(&watcher),
        ));

        running.exited().await;

        // Whatever it said last is all it will say.

        tokio::time::sleep(Duration::from_millis(50)).await;
        reports.abort();
        carried += current.load(Ordering::Relaxed);
        dropped.store(carried, Ordering::Relaxed);
        pid.store(0, Ordering::Relaxed);

        // A relay ends when the host does; only one that left a host
        // still there is started again. A host that serves in its own
        // process does not listen: it is there while its output pipe has
        // a writer.

        let host_there = match &kept_output {
            Some(output) => crate::fdpass::has_writers(output.as_raw_fd()),
            None => listening(&launch.output_socket()),
        };

        if *stop.borrow() || !host_there {
            return;
        }

        if started.elapsed() >= RELAY_STEADY {
            wait = Duration::from_secs(1);
        }

        watcher(RelayNews::Event {
            kind: "output.relay_restarting".to_owned(),
            level: "warning".to_owned(),
            message: format!(
                "the release's log relay exited and will be started again in {} s",
                wait.as_secs()
            ),
        });

        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }

        wait = (wait * 2).min(RELAY_BACKOFF_MAX);

        let output = kept_output.as_ref().map(AsRawFd::as_raw_fd);

        match launch.spawn(output, &logs, &relays) {
            Ok(child) => {
                pid.store(child.id().unwrap_or(0), Ordering::Relaxed);
                running = RelayRunning::Child(child);
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not start the log relay again");
                running = RelayRunning::Gone;
            }
        }
    }
}

/// Read a relay's reports, trying for a moment while it starts, keeping
/// `current` as what it has dropped and `total` with `carried` added,
/// and counting its reopens in `reopens`.
async fn read_relay_reports(
    socket: PathBuf,
    carried: u64,
    current: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    reopens: Arc<tokio::sync::watch::Sender<u64>>,
    watcher: RelayWatcher,
) {
    for _ in 0..100 {
        let result = crate::relay::watch_reports(&socket, |report| match report {
            crate::relay::Report::Status(status) => {
                current.store(status.dropped, Ordering::Relaxed);
                let sum = carried + status.dropped;
                total.store(sum, Ordering::Relaxed);
                watcher(RelayNews::Dropped(sum));
            }
            crate::relay::Report::Event(event) => watcher(RelayNews::Event {
                kind: event.kind,
                level: event.level,
                message: event.message,
            }),
            crate::relay::Report::Reopened => reopens.send_modify(|count| *count += 1),
        })
        .await;

        if result.is_ok() {
            return;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A message with its source in front, as the host writes it.
fn tagged(source: &str, message: &str) -> String {
    if source.is_empty() {
        message.to_owned()
    } else {
        format!("{source}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_time_is_read_past_a_command_name_with_spaces_and_parentheses() {
        // A line as proc(5) gives it, for a process that named itself
        // awkwardly; field 22 is 8421.

        let stat = "4242 (a ) b) c) S 1 4242 4242 0 -1 4194560 120 0 0 0 3 1 0 0 20 0 1 0 8421 \
                    2318336 180 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(start_time_in_stat(stat), Some(8421));
        assert_eq!(start_time_in_stat("4242 (short) S 1 2 3"), None);
        assert_eq!(start_time_in_stat("no parenthesis at all"), None);
    }

    #[test]
    fn a_process_has_a_start_time_that_stays_the_same_and_is_its_own() {
        // Two children of the test, started far enough apart that no
        // clock gives them the same time.

        let spawn = || {
            std::process::Command::new("/bin/sleep")
                .arg("600")
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        std::thread::sleep(Duration::from_millis(50));
        let mut second = spawn();

        let started = start_time(first.id()).expect("a running process has a start time");
        assert_eq!(start_time(first.id()), Some(started));
        assert_ne!(start_time(second.id()), Some(started));
        assert!(start_time(second.id()).is_some());

        // A pid that names no process has none, and neither has pid 0.

        for child in [&mut first, &mut second] {
            child.kill().unwrap();
            child.wait().unwrap();
        }

        assert_eq!(start_time(first.id()), None);
        assert_eq!(start_time(0), None);
    }
}
