//! The host process of one release in development mode: started with the
//! Python interpreter, watched until its workers listen, and stopped with
//! SIGTERM (design specification sections 6.1 and 13.1).

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use muru_common::log::{Gathered, Stream, Traceback};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::locations::HostDirs;
use crate::logs::Logs;

/// What a host says over its control connection (section 5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// How the host's own load of the application went (issue 753): how
    /// it ended (`fork`, or `serve` with `--single`), how long it took,
    /// and the import report's headline figures when it made one.
    /// `first` is set the first time the host says so, which is the news.
    Loaded {
        /// How the load ended.
        outcome: String,
        /// How long it took, in whole milliseconds.
        ms: u64,
        /// The import report's figures, when `import_report` is on.
        imports: Option<serde_json::Value>,
        /// Whether this is the first time the host has said so.
        first: bool,
    },
    /// A part of the load's import report (section 6.18): the JSON text
    /// split into `parts` pieces, this one the `part`th from 0.
    Imports {
        /// Which part this is, from 0.
        part: usize,
        /// How many parts the report is in.
        parts: usize,
        /// This part's text.
        data: String,
    },
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
        /// Evidence of a timeout or a killed worker for the records of
        /// the requests it names (issue 706), which only a development
        /// run keeps.
        evidence: Option<muru_edge::recorder::Evidence>,
        /// When it happened, in Unix milliseconds, for an event the host
        /// held while no supervisor was connected (section 11.5); none
        /// for one sent as it happened.
        time_unix_ms: Option<u64>,
        /// Which of the host's events this is (issue 982), for the
        /// supervisor to acknowledge once it is recorded and to tell one
        /// sent again from one it has; none from a host that numbers
        /// none.
        number: Option<EventNumber>,
    },
    /// Evidence for request records that is no event of its own (issue
    /// 706): that a request past its deadline has since ended.
    Capture(muru_edge::recorder::Evidence),
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
        /// The voluntary swaps waiting their turn, by group (section
        /// 7.8).
        swaps_waiting: std::collections::BTreeMap<String, u32>,
    },
    /// How far a restart of every worker has got (section 10.14), sent
    /// when it changes and to a supervisor that connects during one.
    Restart {
        /// The workers marked.
        total: u32,
        /// Those not yet replaced; 0 once it has finished.
        left: u32,
        /// When it began, in Unix milliseconds.
        started_unix_ms: u64,
        /// The one group it restarts; every group when empty.
        group: String,
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

/// The variable that is `1` in the host and workers of a development run
/// (issue 852).
pub const DEVELOPMENT_VARIABLE: &str = "MURU_DEVELOPMENT";

/// The variable each worker is given its index within its group in
/// (issue 852), which no host or command inherits.
pub const WORKER_ID_VARIABLE: &str = "MURU_WORKER_ID";

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
    /// Modules it has loaded for the first time since the fork (issue
    /// 753).
    pub modules: u64,
    /// Their time, the outermost loads summed, in microseconds.
    pub modules_us: u64,
    /// How long it took to load the application itself, with per-worker
    /// imports, in microseconds; 0 when the host loaded it.
    pub load_us: u64,
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

/// The evidence an event or a capture carries (issue 706): the event's
/// kind and message, and under `evidence` the worker, its pid, why it was
/// killed, the requests the capture names and its threads serving them.
fn evidence(value: &serde_json::Value) -> muru_edge::recorder::Evidence {
    let text =
        |item: &serde_json::Value, key: &str| item[key].as_str().unwrap_or_default().to_owned();
    let found = &value["evidence"];

    muru_edge::recorder::Evidence {
        kind: text(value, "kind"),
        message: text(value, "message"),
        at_unix_ms: muru_edge::recorder::now_unix_ms(),
        worker: text(found, "worker"),
        pid: found["pid"].as_u64().unwrap_or(0) as u32,
        reason: text(found, "reason"),
        requests: found["requests"]
            .as_array()
            .map(|requests| {
                requests
                    .iter()
                    .filter_map(|request| request.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        threads: found["threads"]
            .as_array()
            .map(|threads| {
                threads
                    .iter()
                    .map(|thread| muru_edge::recorder::EvidenceThread {
                        name: text(thread, "name"),
                        request_id: text(thread, "request_id"),
                        stack: text(thread, "stack"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
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
                evidence: value.get("evidence").map(|_| evidence(&value)),
                time_unix_ms: value["time_unix_ms"].as_u64(),
                number: value["seq"].as_u64().map(|seq| EventNumber {
                    stream: value["stream"].as_u64().unwrap_or(0),
                    seq,
                }),
            }),
            "capture" => Some(Self::Capture(evidence(&value))),
            "loaded" => Some(Self::Loaded {
                outcome: value["outcome"].as_str().unwrap_or_default().to_owned(),
                ms: value["ms"].as_f64().map_or(0, |ms| ms.round() as u64),
                imports: value
                    .get("imports")
                    .filter(|imports| imports.is_object())
                    .cloned(),
                first: value["first"].as_bool().unwrap_or(false),
            }),
            "imports" => Some(Self::Imports {
                part: value["part"].as_u64().unwrap_or(0) as usize,
                parts: value["parts"].as_u64().unwrap_or(1) as usize,
                data: value["data"].as_str().unwrap_or_default().to_owned(),
            }),
            "restart" => Some(Self::Restart {
                total: value["total"].as_u64().unwrap_or(0) as u32,
                left: value["left"].as_u64().unwrap_or(0) as u32,
                started_unix_ms: value["started_unix_ms"].as_u64().unwrap_or(0),
                group: value["group"].as_str().unwrap_or_default().to_owned(),
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
                                modules: worker["modules"].as_u64().unwrap_or(0),
                                modules_us: worker["modules_us"].as_u64().unwrap_or(0),
                                load_us: worker["load_us"].as_u64().unwrap_or(0),
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
                swaps_waiting: value["swaps_waiting"]
                    .as_object()
                    .map(|groups| {
                        groups
                            .iter()
                            .map(|(group, count)| {
                                (group.clone(), count.as_u64().unwrap_or(0) as u32)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        }
    }
}

/// The words of the event of a host's load (issue 753): how long it took
/// and how it ended, with the import report's figures when there are
/// some.
pub fn loaded_message(outcome: &str, ms: u64, imports: Option<&serde_json::Value>) -> String {
    let ended = match outcome {
        "serve" => "and serves it in the same process",
        _ => "before the fork",
    };

    let mut message = format!("the host loaded the application in {ms} ms {ended}");

    if let Some(imports) = imports {
        let figure = |key: &str| imports[key].as_f64().unwrap_or(0.0);

        message.push_str(&format!(
            "; importing took {:.0} ms over {:.0} modules, and deferring what can be deferred would save about {:.0} ms",
            figure("import_ms"),
            figure("modules"),
            figure("saving_ms"),
        ));

        let used = figure("deferred_used");

        if used > 0.0 {
            message.push_str(&format!(
                "; {used:.0} deferred imports were used before the fork"
            ));
        }
    }

    message
}

/// Which of a host's events one is (section 11.5, issue 982): the host
/// numbers the events it raises from 1 and names itself by a number
/// made when it started, so the events of the host that replaces it are
/// not taken for ones already recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventNumber {
    /// The host that raised the event.
    pub stream: u64,
    /// The event's number among that host's, from 1.
    pub seq: u64,
}

/// The last of a host's events a supervisor that keeps its events in
/// memory has recorded, by which it tells an event sent again.
#[derive(Debug, Default)]
pub struct Recorded {
    last: Option<EventNumber>,
}

impl Recorded {
    /// Take an event's number, and say whether the event is new: not
    /// when one of the same host with this number or a later one has
    /// been taken.
    pub fn take(&mut self, number: EventNumber) -> bool {
        let seen = self
            .last
            .is_some_and(|last| last.stream == number.stream && last.seq >= number.seq);

        if !seen {
            self.last = Some(number);
        }

        !seen
    }
}

/// The import report a host sends in parts (section 6.18), put back
/// together as they arrive.
#[derive(Debug, Default)]
pub struct ImportsParts {
    parts: Vec<String>,
}

impl ImportsParts {
    /// Take one part, and return the whole report's text once the last
    /// has arrived. A first part starts the report again, and a part out
    /// of order is dropped with what came before it.
    pub fn take(&mut self, part: usize, parts: usize, data: String) -> Option<String> {
        if part == 0 {
            self.parts.clear();
        }

        if part != self.parts.len() {
            self.parts.clear();
            return None;
        }

        self.parts.push(data);

        if self.parts.len() < parts.max(1) {
            return None;
        }

        Some(std::mem::take(&mut self.parts).concat())
    }
}

/// How long a host that is taken over is given to say what its workers
/// are, once its control socket has been connected to.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(5);

/// What a running host said when a supervisor connected to it: the
/// sockets the edge should route to, and every message up to and
/// including the one that said so.
pub struct Announced {
    /// The default group's sockets.
    pub sockets: Vec<PathBuf>,
    /// Each named group's sockets.
    pub groups: Vec<(String, Vec<PathBuf>)>,
    /// The messages the host sent, in order, the announcement last.
    pub sent: Vec<HostMessage>,
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

    /// Connect to the control socket of a host that is already running
    /// and wait for it to say which sockets its workers listen on, which
    /// it tells every supervisor that connects first of all (issue 981).
    /// Returns the connection, the default group's sockets, each named
    /// group's, and what the host has sent so far, the announcement
    /// included, for whoever acts on a host's messages.
    pub async fn connect_announced(
        path: &Path,
    ) -> anyhow::Result<(Self, mpsc::Receiver<HostMessage>, Announced)> {
        let (control, mut messages) = Self::connect(path).await?;
        let mut sent = Vec::new();

        let wait = async {
            while let Some(message) = messages.recv().await {
                let found = match &message {
                    HostMessage::Workers {
                        sockets, groups, ..
                    } => Some((sockets.clone(), groups.clone())),
                    _ => None,
                };

                sent.push(message);

                if found.is_some() {
                    return found;
                }
            }

            None
        };

        let Some((sockets, groups)) = tokio::time::timeout(ANNOUNCE_WAIT, wait)
            .await
            .with_context(|| {
                format!(
                    "the host did not say what its workers are on {}",
                    path.display()
                )
            })?
        else {
            anyhow::bail!(
                "the host closed {} before it said what its workers are",
                path.display()
            );
        };

        Ok((
            control,
            messages,
            Announced {
                sockets,
                groups,
                sent,
            },
        ))
    }

    /// Tell the host the edge routes by the sockets it announced at
    /// this version.
    pub fn acknowledge(&self, version: u64) {
        let _ = self
            .outgoing
            .send(format!("{{\"type\":\"applied\",\"version\":{version}}}\n"));
    }

    /// Tell the host that its events up to this number are recorded
    /// (section 11.5, issue 982), so it holds them no longer.
    pub fn recorded(&self, seq: u64) {
        let _ = self
            .outgoing
            .send(format!("{{\"type\":\"recorded\",\"seq\":{seq}}}\n"));
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

/// How long a supervisor waits before it tries a host's control socket
/// again, when the host is still there and a connection could not be made.
const CONTROL_RETRY: Duration = Duration::from_secs(1);

/// What `keep_control` tells the supervisor that keeps a control
/// connection to a host.
pub enum ControlEvent {
    /// A connection was made; `again` when it replaces one that ended.
    Attached {
        /// The connection, for acknowledgements and requests.
        control: HostControl,
        /// Whether an earlier connection to this host ended.
        again: bool,
    },
    /// The host said something.
    Message(Box<HostMessage>),
    /// The connection ended, or none could be made. Answered with
    /// whether the host is still the one to be connected to, in which
    /// case the connection is made again.
    Ended {
        /// Why no connection could be made, when that is what happened.
        failed: Option<anyhow::Error>,
        /// Whether an earlier connection to this host had ended before.
        again: bool,
        /// Where to say whether to connect again.
        answer: oneshot::Sender<bool>,
    },
}

/// Keep a control connection to a host for as long as its supervisor
/// wants one (section 5.9, issues 983 and 988): connect, hand the
/// connection over, pass on what the host says, and when the connection
/// ends ask whether the host is still the one to be connected to,
/// connecting again if so. A host closes the connection when a send to
/// its supervisor fails or does not complete, which a supervisor that
/// was stalled causes; the host sends its state and the events it holds
/// to whoever connects next. Everything goes to `inbox` as what `wrap`
/// makes of it, so the daemon takes it as commands and a run as the
/// events themselves, and the task ends when the inbox is closed or the
/// answer is no. `made` is a connection already up, with what the host
/// has said on it so far.
pub async fn keep_control<T>(
    inbox: mpsc::Sender<T>,
    path: PathBuf,
    made: Option<(Vec<HostMessage>, mpsc::Receiver<HostMessage>)>,
    wrap: impl Fn(ControlEvent) -> T,
) {
    let mut made = made;
    let mut again = false;

    loop {
        let (connected, failed) = match made.take() {
            Some(made) => (Some(made), None),
            None => match HostControl::connect(&path).await {
                Ok((control, messages)) => {
                    let attached = inbox
                        .send(wrap(ControlEvent::Attached { control, again }))
                        .await;

                    if attached.is_err() {
                        return;
                    }

                    (Some((Vec::new(), messages)), None)
                }
                Err(error) => (None, Some(error)),
            },
        };

        let retry = failed.is_some();

        if let Some((sent, mut messages)) = connected {
            for message in sent {
                if inbox
                    .send(wrap(ControlEvent::Message(Box::new(message))))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            while let Some(message) = messages.recv().await {
                if inbox
                    .send(wrap(ControlEvent::Message(Box::new(message))))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        // The connection ended, or none could be made. The supervisor
        // says whether the host is still the one it wants.

        let (answer, answered) = oneshot::channel();
        let asked = inbox
            .send(wrap(ControlEvent::Ended {
                failed,
                again,
                answer,
            }))
            .await;

        if asked.is_err() || !answered.await.unwrap_or(false) {
            return;
        }

        again = true;

        if retry {
            tokio::time::sleep(CONTROL_RETRY).await;
        }
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
    /// The modules the host imports after the entrypoint and the mounts
    /// (section 6.1).
    pub preload_modules: Vec<String>,
    /// The instrumentation modules, in order (section 6.15), given to the
    /// host in `MURU_INSTRUMENTATION`.
    pub instrumentation: Vec<muru_config::Instrumentation>,
    /// The directory to run the host in.
    pub chdir: Option<PathBuf>,
    /// Loopback TCP instead of Unix sockets for the link, from this port.
    pub tcp_base_port: Option<u16>,
    /// Serve in the host process itself, with no fork (section 13.1).
    pub single: bool,
    /// Wait, once preloaded, for the release's before_fork gates to pass
    /// before forking workers (issue 266): see `HostProcess::wait_held`.
    pub hold_before_fork: bool,
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
    /// How many workers a group may run over its count while workers are
    /// swapped (section 7.8).
    pub max_surge: u32,
    /// How many workers a group may run under its count while workers
    /// are swapped.
    pub max_unavailable: u32,
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
    /// Whether the host and its workers are a development run's, which
    /// `MURU_DEVELOPMENT=1` tells them (issue 852): `muructl run` without
    /// `--allow-public`, and a detour; never the daemon's or a serve's.
    pub development: bool,
    /// The user and group the host runs as, and its umask, when the
    /// daemon is root and the application names them (section 10.9).
    pub identity: Option<Identity>,
    /// Section 4.9: the default mount's path hint roots, each absolute,
    /// beneath which its workers open a hinted file.
    pub sendfile_roots: Vec<String>,
    /// A second header the default mount's workers honour as
    /// X-Muru-Sendfile.
    pub sendfile_header: Option<String>,
    /// The roots Muru provides that must be owned by a user, each with
    /// its uid (issue 849): the application's media part under a root
    /// daemon, so a link in its place cannot lead a worker into a
    /// directory of another user's.
    pub owned_roots: Vec<(String, u32)>,
}

/// The `--sendfile` argument: each mount's roots, the roots among them
/// that must be owned by a user, and its second header, as a JSON object
/// by mount name, the default mount's under "".
fn sendfile_argument(spec: &HostSpec) -> String {
    let entry = |roots: &[String], header: &Option<String>| {
        let owned: serde_json::Map<String, serde_json::Value> = spec
            .owned_roots
            .iter()
            .filter(|(root, _)| roots.contains(root))
            .map(|(root, uid)| (root.clone(), serde_json::Value::from(*uid)))
            .collect();

        serde_json::json!({ "roots": roots, "owned": owned, "header": header })
    };

    let mut mounts = serde_json::Map::new();
    mounts.insert(
        String::new(),
        entry(&spec.sendfile_roots, &spec.sendfile_header),
    );

    for mount in &spec.mounts {
        mounts.insert(
            mount.name.clone(),
            entry(&mount.sendfile_roots, &mount.sendfile_header),
        );
    }

    serde_json::Value::Object(mounts).to_string()
}

impl HostSpec {
    /// The release as app@release, as the host names itself: the
    /// application's name, else the top-level module of its entrypoint,
    /// and the release's id, else `r` and the number of the host among
    /// those its starter has started (`r1`, `r2` in development).
    pub fn label(&self, release: u32) -> String {
        let name = self.name.clone().unwrap_or_else(|| {
            let module = self.app.split(':').next().unwrap_or_default();
            let top = module.split('.').next().unwrap_or_default();

            if top.is_empty() {
                "default".to_owned()
            } else {
                top.to_owned()
            }
        });

        let release = self
            .release
            .clone()
            .unwrap_or_else(|| format!("r{release}"));

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

    /// The home directory the password database gives the user `uid`,
    /// whatever `HOME` says.
    pub fn account_home(uid: u32) -> Option<PathBuf> {
        let identity = Self {
            uid,
            gid: 0,
            umask: None,
        };

        identity
            .account()
            .into_iter()
            .find(|(variable, _)| variable == "HOME")
            .map(|(_, home)| PathBuf::from(home))
    }

    /// The home directory of every account the password database lists,
    /// for a command that reads what each user keeps under theirs.
    pub fn account_homes() -> Vec<PathBuf> {
        static LISTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

        // getpwent keeps its place in storage the whole process shares,
        // so one listing is made at a time.

        let _one_at_a_time = LISTING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut homes = Vec::new();

        // SAFETY: setpwent, getpwent and endpwent take no arguments; the
        // lock above keeps this the only walk of the database, and each
        // entry's strings are copied before the next call replaces them.

        unsafe {
            libc::setpwent();

            loop {
                let entry = libc::getpwent();

                if entry.is_null() {
                    break;
                }

                let home = (*entry).pw_dir;

                if !home.is_null() {
                    let home = std::ffi::CStr::from_ptr(home).to_string_lossy();

                    if !home.is_empty() {
                        homes.push(PathBuf::from(home.into_owned()));
                    }
                }
            }

            libc::endpwent();
        }

        homes.sort();
        homes.dedup();
        homes
    }

    /// Whether this process may switch to another user.
    pub fn can_switch() -> bool {
        // SAFETY: geteuid has no preconditions.

        unsafe { libc::geteuid() == 0 }
    }

    /// The effective user this process runs as.
    pub fn current_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.

        unsafe { libc::geteuid() }
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
    /// Section 4.9: the mount's path hint roots, each absolute.
    pub sendfile_roots: Vec<String>,
    /// A second header the mount's workers honour as X-Muru-Sendfile.
    pub sendfile_header: Option<String>,
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
    /// How many workers the group may run over its count while workers
    /// are swapped, resolved over the release's (section 7.8).
    pub max_surge: u32,
    /// How many workers the group may run under its count while workers
    /// are swapped, resolved over the release's.
    pub max_unavailable: u32,
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
    dirs: HostDirs,
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
type SharedRelays = Arc<std::sync::Mutex<Vec<RelayReading>>>;

/// A task reading what a relay writes to this process, and the way to
/// ask it to write everything the relay has written so far.
struct RelayReading {
    task: tokio::task::JoinHandle<()>,
    flushes: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
}

/// A host this process spawned, or one it adopted from a previous daemon.
enum Process {
    Child(Child),
    Adopted(u32),
}

impl HostProcess {
    /// Start the host for release number `release` in `dirs`, its sockets
    /// in one directory and its files in the other (section 13.8), with a
    /// log relay beside it (section 11.3). Without `detach_to` what
    /// the relay writes comes to this process's own streams and is kept in
    /// `logs` for the dashboard; `watcher` hears what the relay reports.
    pub fn start(
        spec: &HostSpec,
        dirs: &HostDirs,
        release: u32,
        logs: &Logs,
        watcher: RelayWatcher,
    ) -> anyhow::Result<Self> {
        let dirs = dirs.clone();
        dirs.create().with_context(|| {
            format!(
                "creating {} and {}",
                dirs.sockets.display(),
                dirs.files.display()
            )
        })?;
        let mut command = Command::new(&spec.python);
        command
            .arg("-m")
            .arg("muru.host")
            .arg("--app")
            .arg(&spec.app)
            .arg("--socket-dir")
            .arg(&dirs.sockets)
            .arg("--files-dir")
            .arg(&dirs.files)
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
                .arg(dirs.sockets.join(crate::relay::OUTPUT_SOCKET))
                .arg("--output-fd")
                .arg(crate::relay::HOST_OUTPUT_FD.to_string());
        }

        if let Some(hooks) = &spec.hooks {
            command.arg("--hooks").arg(hooks);
        }

        for module in &spec.preload_modules {
            command.arg("--preload-module").arg(module);
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

        if spec.hold_before_fork {
            command.arg("--hold-before-fork");
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

        // Section 4.9 (issue 848): each mount's path hint settings, which
        // its workers enforce, the default mount's under "".

        command.arg("--sendfile").arg(sendfile_argument(spec));

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
            command.arg("--group-swap").arg(format!(
                "{}={},{}",
                group.name, group.max_surge, group.max_unavailable
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
            .arg(dirs.sockets.join("control.sock"))
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

        command
            .arg("--max-surge")
            .arg(spec.max_surge.to_string())
            .arg("--max-unavailable")
            .arg(spec.max_unavailable.to_string());

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

        // Issues 849 and 850: the places are Muru's to name, so a value
        // inherited from the shell that started muructl never reaches the
        // application; those it has are in its environment.

        for name in muru_config::places::VARIABLES {
            command.env_remove(name);
        }

        command.envs(spec.env.iter().map(|(name, value)| (name, value)));

        // Issue 852: whether the run is a development one is Muru's to
        // say, over anything inherited or set, and the worker's index is
        // set in each worker alone.

        if spec.development {
            command.env(DEVELOPMENT_VARIABLE, "1");
        } else {
            command.env_remove(DEVELOPMENT_VARIABLE);
        }

        command.env_remove(WORKER_ID_VARIABLE);

        // Section 13.10: the host and its workers say which instance they
        // belong to, in their titles and their environment.

        command.envs(crate::instance::variables());

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

        let launch = RelayLaunch::new(spec, &dirs, release)?;
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
        // forks them: the default group first, then each named group. A
        // named group's sockets carry its place among the named groups,
        // from 1, and never its name, so that no socket's path grows
        // with a name (section 13.8).

        let socket = |group: Option<usize>, index: usize, position: usize| match base_port {
            None => match group {
                None => dirs.sockets.join(format!("worker-{index}.sock")),
                Some(group) => dirs.sockets.join(format!("worker-{group}-{index}.sock")),
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

        for (place, group) in spec.groups.iter().enumerate() {
            let mut these = Vec::new();

            for index in 0..group.processes {
                let path = socket(Some(place + 1), index, sockets.len());
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
            dirs,
            release,
            relays,
            relay,
        })
    }

    /// Take over a host a previous daemon started: it is checked to be
    /// alive, and is stopped with signals rather than waited for, since
    /// it is not this process's child. The sockets are those the host
    /// itself announced on its control connection (issue 981), since it
    /// replaces workers while no daemon is there to keep its record. Its
    /// relay is adopted with it when `relay_pid` names one still there;
    /// otherwise a new one is started and given the host's pipes.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        pid: u32,
        sockets: Vec<PathBuf>,
        group_sockets: Vec<(String, Vec<PathBuf>)>,
        optional: Vec<String>,
        dirs: HostDirs,
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

        Ok(Self {
            process: Process::Adopted(pid),
            stdin: None,
            sockets,
            group_sockets,
            optional,
            dirs,
            release,
            relays,
            relay,
        })
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

    /// The host's directories.
    pub fn dirs(&self) -> &HostDirs {
        &self.dirs
    }

    /// Where the host serves its control connection (section 5.9): the
    /// worker sockets as they change, and events about its workers.
    pub fn control_path(&self) -> PathBuf {
        self.dirs.sockets.join("control.sock")
    }

    /// Wait for a host started with `hold_before_fork` to have preloaded
    /// and to wait for its gates, which it says by writing the file
    /// `gates` among its files; or for it to exit first, or the time
    /// to be up.
    pub async fn wait_held(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let started = Instant::now();
        let held = self.dirs.files.join("gates");

        loop {
            if held.is_file() {
                return Ok(());
            }

            if let Some(status) = self.try_exited()? {
                anyhow::bail!("the host {status} before it had preloaded");
            }

            anyhow::ensure!(
                started.elapsed() < timeout,
                "the host did not preload within {} seconds",
                timeout.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Tell a host waiting for its gates whether they passed: on its
    /// standard input, `fork` to go on, `stop` to exit.
    pub fn release_gates(&mut self, passed: bool) {
        use std::io::Write as _;

        let word: &[u8] = if passed { b"fork\n" } else { b"stop\n" };

        if let Some(stdin) = &self.stdin
            && let Ok(fd) = stdin.as_fd().try_clone_to_owned()
        {
            let mut pipe = std::fs::File::from(fd);
            let _ = pipe.write_all(word);
        }
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

        self.dirs.remove();
    }

    /// Write when the workers must be gone where the host looks for it, by
    /// the clock both sides read. Before the signal, so the host finds it
    /// whenever the signal lands. A host that cannot read it stops by its
    /// own budget and is killed at the shutdown's deadline.
    fn tell_stop_by(&self, workers_by: Instant) {
        let at = SystemTime::now() + workers_by.saturating_duration_since(Instant::now());
        let path = self.dirs.files.join(STOP_BY_FILE);
        let Ok(since_epoch) = at.duration_since(SystemTime::UNIX_EPOCH) else {
            return;
        };

        // The host may run as another user than this process.

        let millis = since_epoch.as_millis().to_string();

        if let Err(error) = muru_common::files::write_whole_with_mode(&path, millis, 0o644) {
            tracing::warn!(%error, "could not tell the host when its workers must be gone");
        }
    }

    /// Have what the release's workers and host have written so far
    /// reach this process's streams (issue 838): the release's relay
    /// reads every pipe it has, or has been offered, until it would
    /// block and writes what it holds; then what it wrote to this process
    /// is read and written. Gives up after `OUTPUT_FLUSH`, saying whether
    /// it finished. A relay writing a release log has nothing to flush
    /// here.
    pub async fn flush_output(&self) -> bool {
        let readings: Vec<_> = self
            .relays
            .lock()
            .expect("relays poisoned")
            .iter()
            .filter(|reading| !reading.task.is_finished())
            .map(|reading| reading.flushes.clone())
            .collect();

        if readings.is_empty() {
            return true;
        }

        let control = self.dirs.sockets.join(crate::relay::CONTROL_SOCKET);

        let flushed = tokio::time::timeout(OUTPUT_FLUSH, async {
            // What the relay could not flush is still read here.

            let relayed = crate::relay::flush(&control).await.is_ok();

            for reading in readings {
                let (asked, answered) = tokio::sync::oneshot::channel();

                if reading.send(asked).is_ok() {
                    let _ = answered.await;
                }
            }

            relay_writer_flushed().await;
            relayed
        })
        .await;

        flushed.unwrap_or(false)
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
/// line, keeping each line for the dashboard. A flush asked for on
/// `flushes` has every whole line the stream holds now handed to the
/// writer, with any traceback being gathered, before it is answered.
async fn relay<R: AsyncRead + AsFd + Unpin>(
    stream: R,
    logs: Logs,
    to_stderr: bool,
    mut flushes: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
) {
    let mut lines = StreamLines::new(stream);

    let out = if to_stderr {
        Stream::Stderr
    } else {
        Stream::Stdout
    };

    let mut relaying = Relaying {
        out,
        logs,
        gathering: HashMap::new(),
    };

    loop {
        // A quiet moment, with a traceback being gathered, only lets
        // stale tracebacks go.

        let wait = if relaying.gathering.is_empty() {
            Duration::MAX
        } else {
            TRACEBACK_WAIT
        };

        tokio::select! {
            biased;
            Some(answer) = flushes.recv() => {
                lines.read_now();

                while let Some(line) = lines.whole_line() {
                    relaying.stale().await;
                    relaying.line(line).await;
                }

                relaying.all().await;
                let _ = answer.send(());
            }
            next = tokio::time::timeout(wait, lines.next_line()) => {
                relaying.stale().await;

                match next {
                    Ok(Some(line)) => relaying.line(line).await,
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        }
    }

    relaying.all().await;
}

/// A stream read into lines, with a buffer of its own rather than a
/// buffered reader's, so that a read a `select!` cancels loses nothing
/// and a flush can read what the stream holds now.
struct StreamLines<R> {
    stream: R,
    pending: Vec<u8>,
    chunk: Vec<u8>,
    ended: bool,
}

impl<R: AsyncRead + AsFd + Unpin> StreamLines<R> {
    fn new(stream: R) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            chunk: vec![0u8; 64 * 1024],
            ended: false,
        }
    }

    /// The next whole line already read, without its line ending.
    fn whole_line(&mut self) -> Option<String> {
        let at = self.pending.iter().position(|byte| *byte == b'\n')?;
        let line: Vec<u8> = self.pending.drain(..=at).collect();
        Some(text_of(&line[..at]))
    }

    /// The next line, the last one at the end whether whole or not; none
    /// once the stream has ended or failed.
    async fn next_line(&mut self) -> Option<String> {
        use tokio::io::AsyncReadExt as _;

        loop {
            if let Some(line) = self.whole_line() {
                return Some(line);
            }

            if self.ended {
                if self.pending.is_empty() {
                    return None;
                }

                let rest = std::mem::take(&mut self.pending);
                return Some(text_of(&rest));
            }

            match self.stream.read(&mut self.chunk).await {
                Ok(0) | Err(_) => self.ended = true,
                Ok(count) => self.pending.extend_from_slice(&self.chunk[..count]),
            }
        }
    }

    /// Read what the stream holds now, until it would block. The runtime
    /// may not have heard yet that it is readable, and a read through it
    /// would then wait for that, so the stream is read here through a
    /// copy of its descriptor, which shares its non-blocking mode; what
    /// the relay writes in the meantime is not waited for.
    fn read_now(&mut self) {
        use std::io::Read as _;

        if self.ended {
            return;
        }

        let Ok(copy) = self.stream.as_fd().try_clone_to_owned() else {
            return;
        };
        let mut file = std::fs::File::from(copy);

        for _ in 0..OUTPUT_FLUSH_READS {
            match file.read(&mut self.chunk) {
                Ok(0) => {
                    self.ended = true;
                    return;
                }
                Ok(count) => self.pending.extend_from_slice(&self.chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.ended = true;
                    return;
                }
            }
        }
    }
}

/// The most reads of a relay's stream a flush makes: enough for what the
/// largest pipe Linux gives by default holds.
const OUTPUT_FLUSH_READS: usize = 16;

/// A line as text, without a carriage return before its newline; bytes
/// that are not UTF-8 are replaced.
fn text_of(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

/// What `relay` keeps between lines.
struct Relaying {
    out: Stream,
    logs: Logs,
    /// Each source's traceback being gathered, when lines are JSON.
    gathering: HashMap<String, Gathering>,
}

impl Relaying {
    /// Write the tracebacks that have had nothing more for a moment.
    async fn stale(&mut self) {
        let stale: Vec<String> = self
            .gathering
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
            if let Some(mut gathered) = self.gathering.remove(&source) {
                write_traceback(self.out, &source, &mut gathered).await;
            }
        }
    }

    /// Write every traceback being gathered as it is.
    async fn all(&mut self) {
        for (source, mut gathered) in self.gathering.drain() {
            write_traceback(self.out, &source, &mut gathered).await;
        }
    }

    /// Take one line. Section 11.8: the time is the relay's, put in front
    /// when the terminal asks for it with --timestamps. When lines are
    /// JSON records, each source's traceback is gathered and written as
    /// one record, once its exception's line is followed by something
    /// else or nothing more has come for a moment.
    async fn line(&mut self, line: String) {
        let out = self.out;

        // The log relay put the time on the line when it read it, so a
        // delay on the way here does not move it.

        let (stamp, text) = muru_common::log::split_time(&line);
        let time = stamp
            .and_then(muru_common::log::parse_full)
            .unwrap_or_else(SystemTime::now);
        let line = text.to_owned();
        self.logs.push(time, &line);

        if !muru_common::log::json() {
            write_relayed(out, time, &line).await;
            return;
        }

        let (source, message) = match line.split_once(": ") {
            Some((source, message)) if source.starts_with("muru.") => (source, message),
            _ => ("", line.as_str()),
        };

        if let Some(gathered) = self.gathering.get_mut(source) {
            match gathered.traceback.push(message) {
                Gathered::Kept => {
                    gathered.updated = time;
                    return;
                }
                Gathered::Finished(text) => {
                    write_relayed(out, gathered.started, &tagged(source, &text)).await;
                    self.gathering.remove(source);
                }
            }
        }

        if Traceback::starts(message) {
            self.gathering.insert(
                source.to_owned(),
                Gathering {
                    started: time,
                    updated: time,
                    traceback: Traceback::new(message),
                },
            );
            return;
        }

        write_relayed(out, time, &line).await;
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
        let _ = tokio::time::timeout(RELAY_END, relay.task).await;
    }

    let _ = tokio::time::timeout(RELAY_END, relay_writer_flushed()).await;
}

/// Resolve once the writer has written every line handed to it before
/// now.
async fn relay_writer_flushed() {
    let Some(writer) = relay_writer() else {
        return;
    };

    let (answer, written) = tokio::sync::oneshot::channel();

    if writer.send(Relayed::Flushed(answer)).await.is_ok() {
        let _ = written.await;
    }
}

/// How long `HostProcess::flush_output` waits for a release's relay and
/// this process's reading of it, so that a relay held up by a stalled
/// destination under `block` does not hold up a switch.
const OUTPUT_FLUSH: Duration = Duration::from_secs(1);

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
    /// Issue 922: the instance a relay runs as its parent has ended, as
    /// the relay reaped it, before the relay itself exits.
    Ended {
        /// The instance's index.
        index: u32,
        /// The instance's pid.
        pid: u32,
    },
}

/// Hears what a release's relay reports.
pub type RelayWatcher = Arc<dyn Fn(RelayNews) + Send + Sync>;

/// How a release's relay is started, kept to start another the same way.
#[derive(Debug, Clone)]
pub struct RelayLaunch {
    /// The executable, this program.
    executable: PathBuf,
    /// The directories the host's and relay's sockets and files are in.
    dirs: HostDirs,
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
    fn new(spec: &HostSpec, dirs: &HostDirs, release: u32) -> anyhow::Result<Self> {
        let launch = Self::for_release(
            dirs.clone(),
            spec.detach_to.clone(),
            spec.relay,
            spec.identity,
            spec.label(release),
        )?;

        Ok(Self {
            single: spec.single,
            ..launch
        })
    }

    /// How to start the relay of a release whose sockets and files are
    /// in `dirs`, writing `log_file` or else to this process.
    pub fn for_release(
        dirs: HostDirs,
        log_file: Option<PathBuf>,
        settings: RelaySettings,
        identity: Option<Identity>,
        release: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            executable: crate::daemon::running_executable()
                .context("finding the executable to start the relay from")?,
            dirs,
            log_file,
            settings,
            identity,
            single: false,
            release,
        })
    }

    fn output_socket(&self) -> PathBuf {
        self.dirs.sockets.join(crate::relay::OUTPUT_SOCKET)
    }

    fn control_socket(&self) -> PathBuf {
        self.dirs.sockets.join(crate::relay::CONTROL_SOCKET)
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
            .envs(crate::instance::variables())
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
            let (flushes, asked) = mpsc::unbounded_channel();
            let task = tokio::spawn(relay(stderr, logs.clone(), true, asked));
            relays
                .lock()
                .expect("relays poisoned")
                .push(RelayReading { task, flushes });
        }

        if let Some(pid) = child.id() {
            self.record(pid);
        }

        Ok(child)
    }

    /// Say which process the relay is, for a daemon that adopts it. The
    /// record is written whole, as the relay writes its own, since a
    /// daemon may read it while a replacement relay is recorded.
    fn record(&self, pid: u32) {
        let path = self.dirs.files.join(crate::relay::RECORD);

        if let Err(error) = muru_common::files::write_whole(&path, format!("pid = {pid}\n")) {
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
            // A host's end is its own to report, as the host exiting.
            crate::relay::Report::Flushed | crate::relay::Report::Ended { .. } => {}
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

    #[test]
    fn a_restarts_progress_is_read() {
        let parsed = HostMessage::parse(
            r#"{"type":"restart","total":4,"left":3,"started_unix_ms":1700000000000,"group":"reports"}"#,
        );
        assert!(matches!(
            parsed,
            Some(HostMessage::Restart { total: 4, left: 3, started_unix_ms: 1_700_000_000_000, ref group })
                if group == "reports"
        ));
    }

    #[test]
    fn a_status_carries_the_swaps_waiting_in_each_group() {
        let parsed = HostMessage::parse(
            r#"{"type":"status","host":{},"workers":[],"memory_replacements":[],
                "swaps_waiting":{"default":2,"reports":0}}"#,
        );
        let Some(HostMessage::Status { swaps_waiting, .. }) = parsed else {
            panic!("not a status");
        };

        assert_eq!(swaps_waiting.get("default"), Some(&2));
        assert_eq!(swaps_waiting.get("reports"), Some(&0));

        // A host from before the queue says nothing of it.

        let old = HostMessage::parse(r#"{"type":"status","host":{},"workers":[]}"#);
        assert!(matches!(
            old,
            Some(HostMessage::Status { swaps_waiting, .. }) if swaps_waiting.is_empty()
        ));
    }

    #[test]
    fn a_hosts_load_is_an_event_with_the_import_reports_figures() {
        let parsed = HostMessage::parse(
            r#"{"type":"loaded","outcome":"fork","ms":519.6,"first":true,
                "imports":{"import_ms":412.3,"modules":213,"saving_ms":120.4,"deferred_used":2}}"#,
        );
        let Some(HostMessage::Loaded {
            outcome,
            ms,
            imports,
            first,
        }) = parsed
        else {
            panic!("not a load: {parsed:?}");
        };

        assert_eq!((outcome.as_str(), ms, first), ("fork", 520, true));
        assert_eq!(
            loaded_message(&outcome, ms, imports.as_ref()),
            "the host loaded the application in 520 ms before the fork; importing took 412 ms \
             over 213 modules, and deferring what can be deferred would save about 120 ms; \
             2 deferred imports were used before the fork"
        );
        assert_eq!(
            loaded_message("serve", 80, None),
            "the host loaded the application in 80 ms and serves it in the same process"
        );
    }

    #[test]
    fn an_import_report_in_parts_is_put_back_together() {
        let mut parts = ImportsParts::default();
        assert_eq!(parts.take(0, 3, "ab".to_owned()), None);
        assert_eq!(parts.take(1, 3, "cd".to_owned()), None);
        assert_eq!(parts.take(2, 3, "e".to_owned()).as_deref(), Some("abcde"));

        // A part out of order drops what came before it, and a report in
        // one part is whole at once.

        assert_eq!(parts.take(0, 2, "x".to_owned()), None);
        assert_eq!(parts.take(2, 2, "z".to_owned()), None);
        assert_eq!(parts.take(1, 2, "y".to_owned()), None);
        assert_eq!(parts.take(0, 1, "{}".to_owned()).as_deref(), Some("{}"));

        let parsed = HostMessage::parse(r#"{"type":"imports","part":1,"parts":2,"data":"x"}"#);
        assert!(matches!(
            parsed,
            Some(HostMessage::Imports { part: 1, parts: 2, ref data }) if data == "x"
        ));
    }

    #[test]
    fn an_events_evidence_and_a_capture_are_read_for_the_request_records() {
        let event = HostMessage::parse(
            r#"{"type":"event","kind":"worker.timeout_killed","level":"warning","message":"killed",
                "evidence":{"worker":"0","pid":7,"reason":"stuck","requests":["A","B"],
                "threads":[{"name":"t1","request_id":"A","stack":"here"}]}}"#,
        );
        let Some(HostMessage::Event {
            evidence: Some(evidence),
            ..
        }) = event
        else {
            panic!("{event:?}");
        };

        assert_eq!(
            (
                evidence.kind.as_str(),
                evidence.message.as_str(),
                evidence.pid
            ),
            ("worker.timeout_killed", "killed", 7)
        );
        assert_eq!(evidence.requests, ["A", "B"]);
        assert_eq!(evidence.threads[0].stack, "here");

        // An event without evidence has none, and a capture is evidence
        // alone.

        let plain = HostMessage::parse(r#"{"type":"event","kind":"worker.exited","message":"x"}"#);
        assert!(matches!(
            plain,
            Some(HostMessage::Event {
                evidence: None,
                time_unix_ms: None,
                ..
            })
        ));

        // Section 11.5: an event the host held carries when it happened.

        let held = HostMessage::parse(
            r#"{"type":"event","kind":"worker.recycled","message":"x","time_unix_ms":1700000000000}"#,
        );
        assert!(matches!(
            held,
            Some(HostMessage::Event {
                time_unix_ms: Some(1_700_000_000_000),
                ..
            })
        ));

        let ended = HostMessage::parse(
            r#"{"type":"capture","kind":"request.ended","message":"ended","evidence":{"requests":["A"]}}"#,
        );
        let Some(HostMessage::Capture(ended)) = ended else {
            panic!("{ended:?}");
        };
        assert_eq!(
            (ended.kind.as_str(), ended.requests.len()),
            ("request.ended", 1)
        );
    }

    // Issue 981: a host that is taken over says which sockets its
    // workers listen on, and what it said is kept for the daemon's inbox.

    #[tokio::test]
    async fn a_host_taken_over_is_asked_for_its_sockets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let host = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let lines = concat!(
                r#"{"type":"loaded","outcome":"preloaded","ms":3}"#,
                "\n",
                r#"{"type":"workers","version":4,"sockets":["/r/worker-0-g2.sock"],"#,
                r#""groups":[{"name":"jobs","sockets":["/r/worker-1-0.sock"]}]}"#,
                "\n",
                r#"{"type":"event","kind":"worker.recycled","message":"later"}"#,
                "\n",
            );
            stream.write_all(lines.as_bytes()).await.unwrap();
            stream
        });

        let (_control, mut messages, announced) =
            HostControl::connect_announced(&path).await.unwrap();
        assert_eq!(
            announced.sockets,
            vec![PathBuf::from("/r/worker-0-g2.sock")]
        );
        assert_eq!(
            announced.groups,
            vec![("jobs".to_owned(), vec![PathBuf::from("/r/worker-1-0.sock")])]
        );

        // Everything up to the announcement is handed over in order, and
        // what follows is still on the connection.

        assert!(matches!(
            announced.sent.as_slice(),
            [
                HostMessage::Loaded { .. },
                HostMessage::Workers { version: 4, .. }
            ]
        ));
        assert!(matches!(
            messages.recv().await,
            Some(HostMessage::Event { .. })
        ));
        drop(host.await.unwrap());
    }

    #[tokio::test]
    async fn a_host_that_closes_before_it_announces_is_not_taken_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let refused = HostControl::connect_announced(&path).await;
        assert!(
            refused
                .err()
                .is_some_and(|error| error.to_string().contains("before it said"))
        );
    }
}
