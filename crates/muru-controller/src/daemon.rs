//! The daemon: one long-running process that holds desired state, drives
//! releases toward it and serves the admin API (design specification
//! sections 9.1, 9.5, 10.1 and 13.3).
//!
//! This is the first slice: one node, one edge process (ADR 0002) on a
//! listener the daemon binds and hands down, applications deployed from
//! source directories with the blue/green strategy (start the new release,
//! move traffic when it is ready, drain the previous one), rollback to the
//! previous release, drains and events. Re-adoption of hosts across daemon
//! restarts, retention of drained releases and the rest of phase 2 follow.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use muru_proto::edge::v1 as pb;
use tokio::sync::{mpsc, oneshot};

use crate::edges::{EdgeProcess, EdgeSpec};
use crate::events::Events;
use crate::host::{HostControl, HostMessage, WorkerReply};
use crate::host::{HostProcess, HostSpec};
use crate::locations::Locations;
use crate::logs::Logs;
use crate::model::{
    AccessLogSpec, Release, ReleaseSpec, ReleaseState, Rollout, RolloutGates, RolloutState,
    RolloutStep, now_unix_ms, valid_name,
};
use crate::rollouts::{Counters, Verdict, judge};
use crate::scaling::{Decision, ScaleStatus};
use crate::scaling::{GroupScaling, Step, decision_message};
use crate::servers::{Servers, ServersRecord};
use crate::store::Store;

mod detouring;

/// How long a release may take from its host starting to every worker
/// listening (section 9.1 startup timeout, provisional).
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// How long requests in flight on a drained release may take to finish
/// before its workers are stopped anyway (section 9.5 drain_timeout).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a superseded release stays warm (section 9.1), provisional
/// (section 19.2).
pub const RETENTION: Duration = Duration::from_secs(30 * 60);

/// The workers each group of a retained release keeps, its warm floor
/// (section 9.1).
pub const RETAIN_WORKERS: u32 = 1;

/// How long a rolled back or aborted rollout's candidate is kept, for its
/// logs and workers to be looked at (section 9.3).
pub const INSPECTION: Duration = Duration::from_secs(10 * 60);

/// How long to wait for a release's processes to stop, given its stop
/// budget: never less than the drain timeout.
fn stop_wait(budget: Duration) -> Duration {
    DRAIN_TIMEOUT.max(crate::host::stop_wait(budget))
}

/// The open WebSockets the dashboard lists, the longest open first.
const DASHBOARD_SOCKETS: u32 = 200;

/// Section 9.1: how long a superseded release stays warm, and the
/// workers each of its groups keeps meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// How long; zero stops it once it has drained.
    pub lasts: Duration,
    /// The workers each group keeps; none keeps the warm floor, or no
    /// worker for a group whose min_workers is 0.
    pub workers: Option<u32>,
    /// Whether the release takes its application's one retained slot,
    /// as the rollback target, making an older retained release stop.
    /// A rejected rollout's candidate is kept beside it instead, for
    /// inspection only.
    pub slot: bool,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            lasts: RETENTION,
            workers: None,
            slot: true,
        }
    }
}

impl Retention {
    /// Section 9.3: how a rolled back or aborted rollout's candidate is
    /// kept: for a shorter window, outside the retained slot, so the
    /// release a rollback goes to stays warm.
    pub fn inspection() -> Self {
        Self {
            lasts: INSPECTION,
            slot: false,
            ..Self::default()
        }
    }
}

/// Section 9.1: the workers a group of a retained release keeps when the
/// deploy does not say: the warm floor, but none for a group that may
/// scale to zero, which is configured to hold none.
fn retained_by_default(spec: &crate::model::ScalingSpec) -> u32 {
    if spec.is_static() {
        RETAIN_WORKERS
    } else {
        RETAIN_WORKERS.min(spec.min_workers)
    }
}

/// What each group of `release` would keep when retained as `workers`
/// says, for a plan: "1 worker to a group", "its host alone", or each
/// group's count when they differ.
pub(crate) fn retained_workers_phrase(release: &ReleaseSpec, workers: Option<u32>) -> String {
    let counts: Vec<(&str, u32)> =
        std::iter::once((muru_edge::routing::DEFAULT_GROUP, &release.scaling))
            .chain(
                release
                    .groups
                    .iter()
                    .map(|group| (group.name.as_str(), &group.scaling)),
            )
            .map(|(name, scaling)| {
                (
                    name,
                    workers.unwrap_or_else(|| retained_by_default(scaling)),
                )
            })
            .collect();
    let first = counts.first().map_or(0, |(_, count)| *count);

    if counts.iter().all(|(_, count)| *count == first) {
        return match first {
            0 => "its host alone".to_owned(),
            1 => "1 worker to a group".to_owned(),
            count => format!("{count} workers to a group"),
        };
    }

    let parts: Vec<String> = counts
        .iter()
        .map(|(group, count)| format!("{count} in {group}"))
        .collect();
    format!("its host and {}", parts.join(", "))
}

/// Section 9.1: what a retained release keeps running, which decides
/// what a rollback to it costs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kept {
    /// Its host stopped while idle: a rollback waits for the import.
    Idle,
    /// A managed server's instances, which keep running.
    Instances,
    /// Its host, with the workers each group keeps, by group name.
    Workers(Vec<(String, u32)>),
}

impl Kept {
    /// The release's note while it is retained, until `until`.
    fn note(&self, until: &str) -> String {
        match self {
            Kept::Idle => format!("idle until {until}; a rollback to it waits for its import"),
            Kept::Workers(counts) if counts.iter().all(|(_, count)| *count == 0) => {
                format!("host kept until {until}; a rollback to it forks a worker")
            }
            Kept::Workers(counts) if counts.iter().any(|(_, count)| *count == 0) => format!(
                "warm until {until}; a rollback to it forks a worker for a group that kept none"
            ),
            _ => format!("warm until {until}; a rollback to it is instant"),
        }
    }

    /// The event's message: kept warm, kept idle or kept, for `lasts`,
    /// with what it keeps.
    fn event(&self, lasts: &str) -> String {
        let how = match self {
            Kept::Idle => "kept idle",
            Kept::Workers(counts) if counts.iter().any(|(_, count)| *count == 0) => "kept",
            _ => "kept warm",
        };

        format!("{how} for {lasts} {}", self.with())
    }

    /// What it is kept with, for the event.
    fn with(&self) -> String {
        match self {
            Kept::Idle => "with its host stopped".to_owned(),
            Kept::Instances => "with its instances".to_owned(),
            Kept::Workers(counts) => {
                let first = counts.first().map_or(0, |(_, count)| *count);

                if counts.iter().all(|(_, count)| *count == first) {
                    match first {
                        0 => "with its host alone".to_owned(),
                        1 => "with 1 worker per group".to_owned(),
                        count => format!("with {count} workers per group"),
                    }
                } else {
                    let parts: Vec<String> = counts
                        .iter()
                        .map(|(group, count)| format!("{count} in {group}"))
                        .collect();
                    format!("with its host and {}", parts.join(", "))
                }
            }
        }
    }
}

/// Section 9.4: how long sticky clients keep a release that is leaving
/// traffic, unless the policy says otherwise.
pub const STICKY_GRACE: Duration = Duration::from_secs(10 * 60);

/// The account a root daemon runs the edge and the applications as when
/// none is named (ADR 0026).
pub const SERVICE_USER: &str = "muru";

/// What the daemon runs with.
pub struct Config {
    /// Where everything lives.
    pub locations: Locations,
    /// The configuration file's settings, read as a reload reads them.
    pub settings: crate::settings::NodeSettings,
    /// The command line's settings, laid over the file on every reload.
    pub overrides: crate::settings::Overrides,
    /// Where the edge writes the access log, or none.
    pub access_log: Option<PathBuf>,
    /// The executable to spawn edges from, normally the daemon's own.
    pub executable: PathBuf,
    /// The arguments to re-execute the daemon with on an upgrade (section
    /// 13.4): its own, with the flag that names the version it ran before.
    pub upgrade_arguments: Vec<std::ffi::OsString>,
    /// The version this daemon ran before it re-executed itself for an
    /// upgrade, or none when it started afresh.
    pub upgraded_from: Option<String>,
    /// How often the log files are checked for a rename, when
    /// `reopen_on_rename` is on: `RENAME_CHECK` but in tests.
    pub rename_check: Duration,
    /// The locks on the state and run directories, taken before anything
    /// else and held for the life of the process (issue 804).
    pub ownership: crate::ownership::Ownership,
    /// The files to apply once the daemon is up (`--apply`, ADR 0081), or
    /// none.
    pub start_apply: Option<StartApply>,
}

/// The files a start applies (section 13.13): what `muructl apply -f`
/// would send for them, read before the daemon starts.
#[derive(Debug, Clone)]
pub struct StartApply {
    /// The paths, as the messages name them.
    pub paths: String,
    /// The files as one document, or why they could not be read as one.
    pub document: Result<String, String>,
    /// Also remove what the files do not contain.
    pub prune: bool,
}

/// An error on one line, for an event and a log line: a TOML parse
/// error's excerpt of the file, with its line numbers and its caret, is
/// left out, and what it says before and after is kept.
fn on_one_line(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|line| {
            let excerpt = line.starts_with('|')
                || line.starts_with('^')
                || line
                    .split_once(" |")
                    .is_some_and(|(number, _)| number.chars().all(|c| c.is_ascii_digit()));
            !line.is_empty() && !excerpt
        })
        .collect::<Vec<_>>()
        .join(": ")
}

impl StartApply {
    /// What the apply will do, planned against the store before anything
    /// is restored, as the pending event says it; and whether it can be
    /// applied at all.
    fn pending(
        &self,
        store: &Store,
        defined: &crate::config::Defined,
        provisioning: &crate::provision::Provisioning,
    ) -> (bool, String) {
        let document = match self
            .document
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|text| muru_config::Document::parse(text))
        {
            Ok(document) => document,
            Err(error) => return (false, format!("cannot be applied: {}", on_one_line(&error))),
        };

        let plan = crate::config::plan(store, &document, self.prune, false, defined, provisioning);

        if !plan.errors.is_empty() {
            return (
                false,
                format!("cannot be applied: {}", plan.errors.join("; ")),
            );
        }

        if plan.changes.is_empty() {
            return (true, "nothing to change".to_owned());
        }

        let changes: Vec<String> = plan
            .changes
            .iter()
            .map(|change| format!("{} {}", change.action.name(), change.subject()))
            .collect();
        (true, changes.join(", "))
    }

    /// Apply the files to the daemon at `socket`, as `muructl apply`
    /// does, and say what came of it: the summary of a success, or the
    /// report of a failure.
    async fn run(self, socket: PathBuf) -> Result<String, String> {
        let document = self.document.map_err(|error| on_one_line(&error))?;
        let mut client = crate::client::connect(&crate::client::Endpoint::unix(&socket))
            .await
            .map_err(|error| error.to_string())?;
        let report = client
            .apply_config(muru_proto::admin::v1::ApplyConfigRequest {
                document,
                prune: self.prune,
                ..muru_proto::admin::v1::ApplyConfigRequest::default()
            })
            .await
            .map_err(|status| status.message().to_owned())?
            .into_inner();

        let failed: Vec<String> = report
            .changes
            .iter()
            .filter(|change| change.outcome.starts_with("failed"))
            .map(|change| format!("{} {}", change.subject, change.outcome))
            .collect();
        let mut errors = report.errors.clone();
        errors.extend(failed);

        if !report.snapshot_error.is_empty() {
            errors.push(report.snapshot_error.clone());
        }

        if !errors.is_empty() {
            return Err(format!("nothing applied: {}", errors.join("; ")));
        }

        Ok(if report.changes.is_empty() {
            "nothing to change".to_owned()
        } else {
            format!("{} changes applied", report.changes.len())
        })
    }
}

/// How often the daemon checks whether a log file was renamed away
/// (section 11.3: reopen_on_rename).
pub const RENAME_CHECK: Duration = Duration::from_secs(5);

/// What reading the TLS hosts' certificate files again found (section
/// 4.2).
#[derive(Debug, Clone, Default)]
pub struct CertificateChanges {
    /// Hosts whose certificate or key changed, which the edge is given.
    pub renewed: Vec<String>,
    /// Hosts kept on the certificate they had, with why the files were
    /// refused.
    pub kept: Vec<(String, String)>,
    /// Why the edge did not take the renewed certificates, when it did
    /// not.
    pub refused: Option<String>,
}

/// An upgrade the daemon is about to carry out (section 13.4).
#[derive(Debug, Clone)]
pub struct Upgrade {
    /// The installed binary the daemon re-executes from.
    pub executable: PathBuf,
    /// The version that binary reports.
    pub version: String,
}

/// What to do with an active rollout (section 9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutAction {
    /// Hold the current step until resumed.
    Pause,
    /// Continue: from a pause, or into a step awaiting approval.
    Resume,
    /// Give the candidate all the traffic now.
    Promote,
    /// Take the candidate out of traffic now.
    Abort,
}

/// Section 9.3: each release's failed probes, by application and
/// release, and how the latest failed.
type ProbeFailures = Arc<std::sync::Mutex<HashMap<(String, String), (u64, String)>>>;

/// Where a rollout's current step started from: the releases' counters
/// at that moment, so the gates judge the step alone.
#[derive(Debug, Clone)]
struct RolloutRun {
    rollout: String,
    step: usize,
    candidate: Counters,
    baseline: Counters,
}

/// Section 7.3: the shortest time the workers' signals are measured
/// over.
const SIGNAL_WINDOW: Duration = Duration::from_secs(4);

/// How long `muructl logs reopen` waits for the edge and the relays to say
/// they have reopened their files before it answers anyway.
const REOPEN_WAIT: Duration = Duration::from_secs(5);

/// A request for the reconciler, answered when it has been carried out.
pub enum Command {
    /// Everything the dashboard shows (section 10.7).
    Dashboard {
        /// Answered with the data.
        done: oneshot::Sender<muru_dashboard::NodeData>,
    },
    /// The processes the daemon supervises (section 13.10).
    Processes {
        /// Answered with them, the daemon first.
        done: oneshot::Sender<Vec<crate::processes::Known>>,
    },
    /// The node's metrics as the Prometheus text exposition.
    Metrics {
        /// Answered with the text.
        done: oneshot::Sender<String>,
    },
    /// The WebSockets open on the edge.
    WebSockets {
        /// Only this application's; all when empty.
        app: String,
        /// How many to list, the longest open first.
        limit: u32,
        /// Answered with the edge's report.
        done: oneshot::Sender<anyhow::Result<pb::WebSockets>>,
    },
    /// The requests in flight on the edge (section 11.7).
    Requests {
        /// Only this application's; all when empty.
        app: String,
        /// How many to list, the oldest first.
        limit: u32,
        /// Answered with the edge's report.
        done: oneshot::Sender<anyhow::Result<pb::Requests>>,
    },
    /// Create a release and apply the blue/green strategy.
    Deploy {
        /// The application.
        app: String,
        /// The spec, resolved by the API from the request and the source,
        /// boxed to keep the command small.
        spec: Box<ReleaseSpec>,
        /// The release id, or none to generate one.
        id: Option<String>,
        /// How long the superseded release stays warm, zero to stop it,
        /// and the workers it keeps.
        retain: Retention,
        /// Start the release but leave traffic where it is (section 9.2).
        hold: bool,
        /// Where the release record goes as soon as it exists.
        created: oneshot::Sender<Result<Release, String>>,
        /// Where the outcome goes once the release serves or has failed.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Restore traffic to the application's previous release.
    Rollback {
        /// The application.
        app: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Section 9.3: start a rollout of a release against the one serving.
    StartRollout {
        /// The application.
        app: String,
        /// The candidate release.
        release: String,
        /// The steps, in order.
        steps: Vec<RolloutStep>,
        /// The gates.
        gates: RolloutGates,
        /// Whether each step after the first waits for approval.
        approve_each_step: bool,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Rollout, String>>,
    },
    /// Pause, resume, promote or abort the application's active rollout.
    ControlRollout {
        /// The application.
        app: String,
        /// What to do.
        action: RolloutAction,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Rollout, String>>,
    },
    /// Section 7.4: pin a worker group's count, or let the autoscaler
    /// decide again.
    SetScale {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The worker group.
        group: String,
        /// The count to pin.
        processes: Option<u32>,
        /// Let the autoscaler decide again.
        unpin: bool,
        /// Where the groups' states go.
        done: oneshot::Sender<Result<Vec<(String, ScaleStatus)>, String>>,
    },
    /// The autoscaler's state for each group of a release.
    GetScaleStatus {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the groups' states go.
        done: oneshot::Sender<Result<Vec<(String, ScaleStatus)>, String>>,
    },
    /// Section 11.2: change which requests the access log writes for a
    /// period, put the changes back, or list those in force.
    OverrideAccessLog {
        /// The change asked for, checked; none to list or reset.
        change: Option<muru_edge::access::Override>,
        /// Put back the changes, those for this application when it is
        /// not empty.
        reset: Option<String>,
        /// Answered with the changes in force after it and how many a
        /// reset put back.
        done: oneshot::Sender<Result<(Vec<muru_edge::access::Override>, usize), String>>,
    },
    /// Section 4.12: serve the maintenance page for an application, or
    /// serve it as usual again.
    SetMaintenance {
        /// The application.
        app: String,
        /// On or off.
        on: bool,
        /// Answered with when maintenance began, or none when it is off.
        done: oneshot::Sender<Result<Option<u64>, String>>,
    },
    /// Section 4.12: serve the maintenance page for every route of a
    /// virtual host, or serve the host as usual again.
    SetHostMaintenance {
        /// The virtual host.
        host: String,
        /// On or off.
        on: bool,
        /// Answered with when maintenance began, or none when it is off.
        done: oneshot::Sender<Result<Option<u64>, String>>,
    },
    /// Blue/green (section 9.2): move all of an application's traffic to a
    /// release now and drain the one that served.
    PromoteRelease {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// How long the release that served stays warm, and with how
        /// many workers.
        retain: Retention,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Take a release out of traffic and stop it.
    Drain {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// Section 9.1: the releases a route's traffic names and the
    /// candidates of the rollouts under way, which removal leaves.
    ReferencedReleases {
        /// Where the answer goes.
        done: oneshot::Sender<BTreeSet<(String, String)>>,
    },
    /// Section 9.1: drop a release's record unless it has become
    /// protected since it was chosen. The check and the removal are one
    /// step of the command loop, so a promotion or rollback cannot slip
    /// between them.
    ForgetRelease {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Section 9.1: why an application cannot be removed now, if so.
    AppRemovalRefused {
        /// The application.
        app: String,
        /// Where the answer goes.
        done: oneshot::Sender<Option<String>>,
    },
    /// Section 9.1: drain whatever an application runs and drop it and
    /// its release records, unless it cannot be removed now.
    RemoveApp {
        /// The application.
        app: String,
        /// Where the outcome goes.
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Section 10.10: start a recorded release held, for an apply's
    /// prepare phase, unless it runs.
    StartHeld {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Answered with whether the release was started, or why it did
        /// not start.
        done: oneshot::Sender<Result<bool, String>>,
    },
    /// Section 10.10: an apply's commit phase.
    CommitApply {
        /// The apply's plan; the changes of the commit phase are made.
        changes: Vec<crate::config::Change>,
        /// How long a release that served before stays warm, and with
        /// how many workers.
        retain: Retention,
        /// Answered with what the phase did, or why nothing was written.
        done: oneshot::Sender<Result<crate::config::Committed, String>>,
    },
    /// Section 4.2: read the TLS hosts' certificate files again, for an
    /// apply that changed nothing else, and send the edge what changed.
    RefreshCertificates {
        /// Answered with what was renewed and what was kept.
        done: oneshot::Sender<CertificateChanges>,
    },
    /// Re-read the configuration file for what can change while running
    /// (section 13.3).
    Reload {
        /// Answered with what was reloaded, or why it was refused.
        done: oneshot::Sender<Result<String, String>>,
    },
    /// Reopen every log file after an external rotation (section 11.3):
    /// the daemon's own, the edge's access log and each host's, and with
    /// `workers` the application's own files through on_reopen_logs.
    ReopenLogs {
        /// Also pass the request on to the workers.
        workers: bool,
        /// Where the count of what was told goes.
        done: oneshot::Sender<Reopened>,
    },
    /// Rotate these log files now (section 11.3), on the policy in
    /// force. Answered with each one's archive or error, by log name, or
    /// why none could be: rotation is external.
    RotateLogs {
        /// Each log's name and path.
        logs: Vec<(String, PathBuf)>,
        /// Where the answer goes.
        done: oneshot::Sender<Result<Vec<RotatedLog>, String>>,
    },
    /// Hold log files against rotation while a command cuts them
    /// (section 11.3). Answered with whether each file's writers cannot
    /// reopen it, or why the files could not be held.
    ClaimLogs {
        /// The files, in order.
        paths: Vec<PathBuf>,
        /// Where the answer goes.
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Let rotation consider these log files again.
    ReleaseLogs {
        /// The files.
        paths: Vec<PathBuf>,
    },
    /// Stop the daemon: detaching, with hosts and the edge left serving
    /// for the next daemon to re-adopt, or draining everything first.
    Shutdown {
        /// Drain and stop every release and the edge before exiting.
        drain: bool,
        /// Where the acknowledgement goes, before the daemon exits.
        done: oneshot::Sender<()>,
    },
    /// Re-execute the daemon from the installed binary and replace the
    /// edge (section 13.4).
    Upgrade {
        /// Answered with what is about to happen, before the daemon
        /// re-executes, or why it will not.
        done: oneshot::Sender<Result<Upgrade, String>>,
    },
    /// A host's control connection is up (section 5.9).
    HostAttached {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The connection, for acknowledgements.
        control: HostControl,
    },
    /// A host said something over its control connection.
    HostMessage {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// What it said.
        message: HostMessage,
    },
    /// Ask a release's workers something (section 11.7).
    Debug {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The operation: status, stacks, tasks, inspect or logging.
        op: String,
        /// One worker, or every worker.
        worker: Option<muru_common::worker::WorkerName>,
        /// The operation's arguments.
        args: serde_json::Value,
        /// Answered with each worker's reply, or why none could be asked.
        done: oneshot::Sender<Result<Vec<WorkerReply>, String>>,
    },
    /// Issue 922: the relay of a managed server's instance reports that
    /// the instance has ended.
    ServerEnded {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The instance's index.
        index: u32,
        /// The pid that ended.
        pid: u32,
    },
    /// Issue 917: the probe of a managed server's instance started again
    /// has ended, on a task beside the loop.
    ServerProbed {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// The instance's index.
        index: u32,
        /// The pid the probe was of.
        pid: u32,
        /// Whether it answered within the readiness timeout.
        answered: bool,
    },
    /// Issue 699: a release's stop, on a task beside the loop, has ended.
    Stopped {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// Why it was stopped, for its note.
        why: String,
        /// Whether its processes outlasted the wait for them.
        timed_out: bool,
    },
    /// Issue 693: a promotion's before_promote gates have run, on a task
    /// beside the loop.
    PromoteGated {
        /// The application.
        app: String,
        /// The release being promoted.
        id: String,
        /// How long the release that served stays warm.
        retain: Retention,
        /// What follows the promotion.
        then: Box<AfterPromote>,
        /// Whether the gates passed.
        outcome: Result<(), String>,
    },
    /// Issue 693: a stage of a release's start, which a task beside the
    /// loop reports.
    StartStage {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// What the stage gave.
        stage: Box<StartStage>,
    },
    /// Resolve what a command runs with from its release (section 9.12).
    PrepareCommand {
        /// The application.
        app: String,
        /// The release.
        id: String,
        /// What to run.
        request: crate::commands::CommandRequest,
        /// Answered with the command ready to start, or why it cannot be.
        done: oneshot::Sender<Result<crate::commands::Prepared, String>>,
    },
    /// Activate, end or promote an overlay (section 9.10).
    Overlay {
        /// The overlay.
        name: String,
        /// What to do.
        action: OverlayAction,
        /// Whether the signed webhook asks, rather than a person.
        external: bool,
        /// Answered with what happened.
        done: oneshot::Sender<Result<OverlayOutcome, String>>,
    },
    /// Section 7.5: a temporary schedule raising a worker group's floor,
    /// and its ceiling with it, or capping it (issue 712), until a time.
    ScaleTemporary {
        /// The application.
        app: String,
        /// The worker group.
        group: String,
        /// The floor.
        min_workers: Option<u32>,
        /// The ceiling, when raised too.
        max_workers: Option<u32>,
        /// The cap, in place of a floor.
        cap_workers: Option<u32>,
        /// When it ends, Unix milliseconds.
        until: u64,
        /// Answered with a sentence about what happened.
        done: oneshot::Sender<Result<String, String>>,
    },
    /// Section 9.13: open a detour for a development run.
    OpenDetour {
        /// What the run asked for.
        opening: crate::detours::Opening,
        /// The stream to the run, for later messages.
        updates: crate::detours::Updates,
        /// Answered with the first message, or why it was refused, and
        /// the opening's serial.
        done: oneshot::Sender<Result<(muru_proto::admin::v1::DetourOpened, u64), String>>,
    },
    /// Section 9.13: change what a detour takes.
    SetDetourTraffic {
        /// The application.
        app: String,
        /// The detour's release.
        release: String,
        /// What it is to take.
        traffic: crate::detours::Traffic,
        /// When it goes back to preview, Unix milliseconds; 0 for never.
        until_unix_ms: u64,
        /// The paths it is to take requests under, every path when
        /// empty; left as they are when none.
        paths: Option<Vec<String>>,
        /// Answered with the detour as it is now, or why not.
        done: oneshot::Sender<Result<muru_proto::admin::v1::Detour, String>>,
    },
    /// Section 4.21: mint entry links to a detour's preview.
    PreviewDetour {
        /// Which detour, and which of its application's addresses.
        request: muru_proto::admin::v1::PreviewDetourRequest,
        /// Answered with the links, or why not.
        done: oneshot::Sender<Result<muru_proto::admin::v1::DetourPreview, String>>,
    },
    /// Section 9.13: a detour's stream ended, from its run's side or the
    /// daemon's.
    CloseDetour {
        /// The opening's serial.
        serial: u64,
        /// Why, for the event.
        reason: String,
    },
}

/// What came of what was asked of an overlay (section 9.10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayOutcome {
    /// A sentence about what happened.
    pub message: String,
    /// After a promotion, what the applied files must say for the next
    /// apply to keep it, as TOML blocks (issue 759).
    pub files: Vec<String>,
}

/// What following a condition did to its overlay (issue 689).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Followed {
    /// Nothing changed.
    Stayed,
    /// The condition activated it.
    Activated,
    /// The condition ended it.
    Ended,
}

/// Issue 790: what a promotion changed, to put back when the edge
/// refuses the result.
#[derive(Debug, Default)]
struct PromotionUndo {
    /// The overlay's state before.
    state: Option<crate::model::OverlayState>,
    /// Each application whose serving release it switched.
    serving: Vec<PromotedSwitch>,
    /// Each route it wrote, by host and key, as it was, or none for a
    /// route it added.
    routes: Vec<(String, String, Option<muru_config::Route>)>,
    /// Each application's maintenance before, since when, in the store
    /// and in memory.
    maintenance: Vec<(String, Option<u64>, Option<u64>)>,
}

/// Issue 790: an application whose serving release a promotion switched,
/// with what it replaced.
#[derive(Debug)]
struct PromotedSwitch {
    app: String,
    /// The release that serves now.
    release: String,
    /// The serving release in memory before.
    serving: Option<String>,
    /// Whether the release was held before.
    held: bool,
    /// The application's current and previous releases in the store
    /// before.
    current: Option<String>,
    previous: Option<String>,
}

/// What a person asks of an overlay (section 9.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    /// Apply it now, until a time or until ended.
    Activate {
        /// When it ends, Unix milliseconds; never without one.
        until: Option<u64>,
    },
    /// Stop applying it; a schedule's window stays ended until it is over.
    End,
    /// Fold its changes into the base configuration and end it for good.
    Promote,
}

/// What a previous daemon left behind about its edge, for re-adoption
/// (section 10.2), in the runtime directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct EdgeRecord {
    pid: u32,
    control: PathBuf,
    listener_socket: PathBuf,
    #[serde(default)]
    generation: u32,
    #[serde(default)]
    listeners: Vec<ListenerRecord>,
}

/// One of the edge's listeners, for re-adoption.
#[derive(serde::Serialize, serde::Deserialize)]
struct ListenerRecord {
    address: SocketAddr,
    tls: bool,
}

/// What a previous daemon left behind about a release's host, in the
/// release's run directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct HostRecord {
    pid: u32,
    sockets: Vec<PathBuf>,
    #[serde(default)]
    groups: Vec<GroupRecord>,
    /// The runtime library directory the host runs with, which `muructl
    /// cache` leaves while the host is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime: Option<PathBuf>,
}

/// A release's log relay, for re-adoption (section 11.3), written beside
/// its sockets.
#[derive(serde::Deserialize)]
struct RelayRecord {
    pid: u32,
}

/// A named worker group's sockets and paths, for re-adoption and the
/// snapshot.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GroupRecord {
    name: String,
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    client_classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    methods: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
    sockets: Vec<PathBuf>,
}

/// Releases, as (app, release).
type ReleaseSet = BTreeSet<(String, String)>;

/// What the API needs to talk to a running daemon.
#[derive(Clone)]
pub struct Handle {
    /// The store, for reads.
    pub store: Arc<Store>,
    /// The event stream.
    pub events: Events,
    /// Where commands go.
    pub commands: mpsc::Sender<Command>,
    /// The daemon's locations and listener, for status.
    pub info: Arc<Info>,
    /// The commands running for the API's callers, which the daemon stops
    /// and waits for before it exits.
    pub underway: crate::commands::Underway,
    /// The runs the daemon keeps, and the detached ones it watches (issue
    /// 688).
    pub runs: crate::runs::Runs,
}

/// Section 13.10: the daemon's processes as ListProcesses answers, which
/// the dashboard's Processes section shows as well; none when the daemon
/// is stopping.
pub async fn processes(
    commands: &mpsc::Sender<Command>,
    info: &Info,
    request: muru_proto::admin::v1::ListProcessesRequest,
) -> Option<muru_proto::admin::v1::Processes> {
    let (done, answer) = oneshot::channel();
    commands.send(Command::Processes { done }).await.ok()?;
    let known = answer.await.ok()?;

    let header = crate::processes::Header {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        mode: "daemon".to_owned(),
        started_unix_ms: info.started_unix_ms,
        limits: info.cgroup_limits.lock().expect("limits poisoned").clone(),
        state_dir: crate::registry::home_shortened(&info.locations.state.path),
    };

    Some(
        crate::processes::snapshot(
            known,
            request,
            header,
            Arc::clone(&info.cpu),
            &info.process_figures,
        )
        .await,
    )
}

/// Facts about the running daemon, for status.
pub struct Info {
    /// Where everything lives.
    pub locations: Locations,
    /// The addresses the edge serves, each with whether it carries TLS,
    /// as a reload may change them.
    pub listeners: std::sync::RwLock<Vec<(SocketAddr, bool)>>,
    /// The node's limits (section 4.6), as a reload may change them.
    pub limits: std::sync::RwLock<muru_edge::Limits>,
    /// When the daemon started, Unix milliseconds.
    pub started_unix_ms: u64,
    /// The edge process's pid, for status.
    pub edge_pid: std::sync::atomic::AtomicU32,
    /// Where the Prometheus exposition is served, or nowhere.
    pub metrics: Option<SocketAddr>,
    /// Where the dashboard is served, or nowhere.
    pub dashboard: Option<SocketAddr>,
    /// The dashboard's login codes and sessions, when it is served
    /// (section 10.7).
    pub dashboard_sessions: Option<Arc<muru_dashboard::Sessions>>,
    /// The rate limits (section 4.15) and client classes (section 4.14)
    /// the daemon's file defines, which applied files may name.
    pub defined: std::sync::RwLock<crate::config::Defined>,
    /// The Python logging defaults every host starts with under its
    /// release's own (`[logging.python]`, section 11.4), as a reload may
    /// change them.
    pub python_logging: std::sync::RwLock<muru_config::PythonLogging>,
    /// WebSockets open by application and release, from the edge's
    /// metrics as last sampled.
    pub websockets: std::sync::Mutex<HashMap<(String, String), u64>>,
    /// The live figures of `muructl status --processes`, sampled every
    /// second (section 13.10).
    pub process_figures: crate::processes::Live,
    /// Each process's CPU time when last read, for its CPU percentage.
    pub cpu: Arc<crate::processes::CpuMeter>,
    /// What each release's entrypoints run as, as its host last
    /// announced them.
    pub interfaces: Interfaces,
    /// The instances of each managed server release (section 9.9), by
    /// application and release, as the daemon last checked them.
    pub servers: std::sync::Mutex<HashMap<(String, String), Vec<crate::servers::InstanceRecord>>>,
    /// Section 9.10 (issue 123): the conditions and their states, as of the
    /// last evaluation.
    pub conditions: std::sync::Mutex<Vec<crate::conditions::Status>>,
    /// What runs for each release now, by application and release, as the
    /// daemon last published it.
    pub live: std::sync::Mutex<HashMap<(String, String), LiveRelease>>,
    /// The applications serving with no host declared to choose between
    /// them, comma separated, or empty (section 4.3).
    pub no_host: std::sync::Mutex<String>,
    /// Section 8.7: `[daemon] pass_env`, as a reload may change it.
    pub pass_env: std::sync::RwLock<Vec<String>>,
    /// The service user (ADR 0026), when this daemon is root, for what a
    /// deploy's plan says a release would run as.
    pub service: Option<crate::host::Identity>,
    /// Section 8.4: uv as found and how it is run, from the `[tools]`
    /// table read at start.
    pub provisioning: crate::provision::Provisioning,
    /// Section 13.9: set once the daemon has finished starting, for the
    /// startup and readiness probes.
    pub started: std::sync::atomic::AtomicBool,
    /// Set when the daemon begins to stop, which fails readiness.
    pub stopping: std::sync::atomic::AtomicBool,
    /// Sections 7.7 and 13.9: the daemon's cgroup limits as last read,
    /// and the memory guard, for status.
    pub cgroup_limits: std::sync::Mutex<Option<muru_proto::admin::v1::CgroupLimits>>,
    /// The instance's ID, kept in the state directory so it is the same
    /// across restarts (section 13.8, issue 805).
    pub instance_id: crate::registry::InstanceId,
    /// Section 9.13: the open detours, as the daemon last published them.
    pub detours: std::sync::Mutex<Vec<muru_proto::admin::v1::Detour>>,
    /// The flags the daemon was started with that stand over its file.
    pub start_flags: Vec<String>,
}

/// What runs for one release now, for the API.
#[derive(Debug, Clone, Default)]
pub struct LiveRelease {
    /// Whether a host runs for it: false for a dormant release.
    pub host: bool,
    /// Whether its host has said which workers it runs.
    pub announced: bool,
    /// Its workers, as its host last announced them.
    pub workers: Vec<crate::host::HostWorker>,
    /// For a retained release, when it is stopped, in Unix milliseconds;
    /// otherwise 0.
    pub retained_until_unix_ms: u64,
}

/// What each release's entrypoints run as (section 6.7): the interface
/// its host settled for each, a declared one as declared and one left to
/// be detected as the callable showed, as the host last announced them.
/// Everything that needs an interface once a release has started takes
/// it from `runs_as`, never from the configuration, where it may still
/// say `auto`.
#[derive(Debug, Default)]
pub struct Interfaces {
    /// By application and release.
    announced: std::sync::Mutex<HashMap<(String, String), Announced>>,
}

/// What one release's host announced: each mount with its interface, the
/// default mount's under the empty name.
type Announced = Vec<(String, String)>;

impl Interfaces {
    /// Record what a release's host announced, each mount with its
    /// interface. An announcement that names none leaves the last as it
    /// was: a host says nothing of what each worker settles for itself.
    pub fn announce(&self, app: &str, release: &str, interfaces: Vec<(String, String)>) {
        if interfaces.is_empty() {
            return;
        }

        self.announced
            .lock()
            .expect("interfaces poisoned")
            .insert((app.to_owned(), release.to_owned()), interfaces);
    }

    /// What a release's host announced, each mount with its interface;
    /// nothing before its first announcement.
    pub fn announced(&self, app: &str, release: &str) -> Vec<(String, String)> {
        self.announced
            .lock()
            .expect("interfaces poisoned")
            .get(&(app.to_owned(), release.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    /// The interface a release's default mount, under the empty name,
    /// or a named mount runs as: what its host announced, and
    /// `configured` only until it has announced one for the mount.
    pub fn runs_as(
        &self,
        app: &str,
        release: &str,
        mount: &str,
        configured: muru_config::Interface,
    ) -> muru_config::Interface {
        let announced = self.announced(app, release);
        let found = announced
            .iter()
            .find(|(name, _)| name == mount)
            .map(|(_, interface)| interface.as_str());

        match found {
            Some("wsgi") => muru_config::Interface::Wsgi,
            Some("asgi") => muru_config::Interface::Asgi,
            Some("grpc") => muru_config::Interface::Grpc,
            _ => configured,
        }
    }
}

/// One log rotated at a command's asking: its archive, or why not.
#[derive(Debug, Clone)]
pub struct RotatedLog {
    /// The log's name.
    pub name: String,
    /// Its archive, relative to the log directory.
    pub archive: Result<String, String>,
}

/// What a ReopenLogs command reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reopened {
    /// Whether the edge was told.
    pub edge: bool,
    /// How many hosts were told.
    pub hosts: u32,
}

/// Section 9.12 (issue 268): when a scheduled command is next due, and the
/// cron expression and time zone that time was worked out from.
struct Scheduled {
    shape: (String, Option<String>),
    /// Unix milliseconds.
    next: u64,
}

/// What runs a release: a host with Muru's workers, the instances of a
/// managed HTTP server (section 9.9), or nothing for now: a release whose
/// host stopped once every group had been at zero for its
/// host_idle_timeout, started again by the next request (section 7.4).
enum Backend {
    Host(HostProcess),
    Servers(Servers),
    Dormant,
}

impl Backend {
    /// The host, for a release with workers.
    fn host(&self) -> Option<&HostProcess> {
        match self {
            Self::Host(host) => Some(host),
            Self::Servers(_) | Self::Dormant => None,
        }
    }

    /// The managed server's instances, for a release that runs one.
    fn servers(&self) -> Option<&Servers> {
        match self {
            Self::Servers(servers) => Some(servers),
            Self::Host(_) | Self::Dormant => None,
        }
    }

    /// Whether the host has exited on its own: a sentence saying how.
    /// Instances look after themselves (check_hosts starts them again).
    fn try_exited(&mut self) -> std::io::Result<Option<String>> {
        match self {
            Self::Host(host) => host.try_exited(),
            Self::Servers(_) | Self::Dormant => Ok(None),
        }
    }

    /// Have the release's log relays open the release log again by name,
    /// and with `workers` the host's workers run their on_reopen_logs
    /// hook once its relay has: the host's relay, or the relay of each of
    /// a managed server's instances. Gives, for each relay told, what
    /// changes once it has reopened the file.
    fn reopen_log(&self, workers: bool) -> Vec<tokio::sync::watch::Receiver<u64>> {
        match self {
            Self::Host(host) => host.reopen_log(workers).into_iter().collect(),
            Self::Servers(servers) => servers.reopen_log(),
            Self::Dormant => Vec::new(),
        }
    }

    /// Lines of the release's output its relays have dropped so far.
    fn output_dropped(&self) -> Option<u64> {
        match self {
            Self::Host(host) => Some(host.output_dropped()),
            Self::Servers(servers) => Some(servers.output_dropped()),
            Self::Dormant => None,
        }
    }

    /// The record a previous daemon reads to take the release over.
    fn record_path(&self, run_dir: &Path) -> PathBuf {
        match self {
            Self::Host(_) | Self::Dormant => run_dir.join("host.toml"),
            Self::Servers(_) => run_dir.join("servers.toml"),
        }
    }

    /// Stop the processes; `grace` is the release's stop budget: what a
    /// managed server's instances, or a host's workers, get before
    /// SIGKILL.
    async fn stop(self, grace: Duration) {
        match self {
            Self::Host(host) => host.stop(grace).await,
            Self::Servers(servers) => servers.stop(grace).await,
            Self::Dormant => {}
        }
    }

    /// Stop the processes as `stop` does, killing what is still there at
    /// `kill_at` (section 13.3).
    async fn stop_until(self, kill_at: Instant, workers_by: Instant) {
        match self {
            Self::Host(host) => host.stop_until(kill_at, Some(workers_by)).await,
            Self::Servers(servers) => servers.stop_until(kill_at).await,
            Self::Dormant => {}
        }
    }

    /// Leave the processes running for the next daemon.
    fn release(self) {
        match self {
            Self::Host(host) => host.release(),
            Self::Servers(servers) => servers.release(),
            Self::Dormant => {}
        }
    }

    /// Section 9.9: a managed server's instances as the upstream that
    /// stands in for the release's workers in the snapshot.
    fn server_message(&self, app: &str, id: &str) -> Option<pb::Upstream> {
        self.servers().map(|servers| pb::Upstream {
            name: format!("{app}@{id}"),
            addresses: servers.addresses(),
            starting: servers.starting(),
            protocol: "http1".to_owned(),
            trust_muru_headers: true,
            eject_after: 3,
            eject_for_ms: 5_000,
            ..Default::default()
        })
    }
}

/// A running release: its host and the sockets its workers listen on,
/// or the instances of its managed server, and, once superseded, when
/// its retention ends.
struct Running {
    backend: Backend,
    /// The default group's sockets.
    sockets: Vec<PathBuf>,
    /// The named groups' sockets and paths.
    groups: Vec<GroupRecord>,
    retained_until: Option<Instant>,
    /// `retained_until` as Unix milliseconds, for the API; 0 when not
    /// retained.
    retained_until_unix_ms: u64,
    /// The workers the host announced last, and whether it has yet.
    workers: Vec<crate::host::HostWorker>,
    announced: bool,
    /// Retained for inspection after a rejected rollout, outside the
    /// application's retained slot: it makes way before the rollback
    /// target does.
    inspected: bool,
    /// Section 9.4: while set, the release is leaving traffic but stays
    /// reachable by sticky cookie until the instant, when it drains with
    /// the retention given.
    sticky_until: Option<(Instant, Retention)>,
    /// Deployed with hold (section 9.2): running and reachable by preview,
    /// taking no traffic until a rollout or a promotion, and left alone
    /// by the traffic reconciliation.
    held: bool,
    /// The host's control connection, once it is up.
    control: Option<HostControl>,
    /// What the workers last reported, for metrics.
    status: Vec<crate::host::WorkerStatus>,
    /// How long the host took to load the application, in milliseconds,
    /// once it has said (issue 753).
    load_ms: Option<u64>,
    /// The workers the host is replacing for memory, for the memory
    /// guard (section 13.9).
    memory_replacements: Vec<crate::host::MemoryReplacement>,
    /// Section 11.1: workers the host replaced, by group and reason, and
    /// workers that exited, likewise, since the release started.
    recycles: BTreeMap<(String, String), u64>,
    exits: BTreeMap<(String, String), u64>,
    /// Section 7.3: the report the workers' signals are measured from,
    /// and when it arrived.
    signals_from: Option<(Instant, Vec<crate::host::WorkerStatus>)>,
    /// Section 7.4: each worker group's autoscaler, the default first.
    scalers: Vec<GroupScaling>,
    /// The host's own proportional set size, where it can be read, for
    /// the node's memory budget.
    host_pss: Option<u64>,
    /// Section 7.4: since when every group has been at zero, for the
    /// host_idle_timeout.
    idle_since: Option<Instant>,
    /// When each worker socket entered routing, for the age of the
    /// youngest worker.
    ready_at: HashMap<PathBuf, Instant>,
}

impl Running {
    /// Section 7.4: what one worker of a group costs: its hard limit
    /// when one is set (issue 384), else its workers' mean proportional
    /// set size and CPU, the soft memory limit standing in before any has
    /// reported, then `fallback`.
    fn worker_cost(&self, group: &GroupScaling, fallback: Option<u64>) -> crate::budget::Cost {
        let sizes: Vec<u64> = self
            .status
            .iter()
            .filter(|worker| worker.group == group.name)
            .filter_map(|worker| worker.pss)
            .collect();
        let measured = (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>() / sizes.len() as u64);

        crate::budget::Cost {
            memory: group
                .hard_limit
                .or(measured)
                .or(group.memory_limit.map(|(_, limit)| limit))
                .or(fallback),
            cpu: group.scaler.worker_signals().cores,
        }
    }

    /// Every socket the edge routes to: the default group's, then the
    /// named groups'.
    fn routed_sockets(&self) -> Vec<PathBuf> {
        let mut all = self.sockets.clone();

        for group in &self.groups {
            all.extend(group.sockets.iter().cloned());
        }

        all
    }

    /// A group's workers, as the sockets the host announced; none for a
    /// group it has not announced yet.
    fn group_sockets(&self, group: &str) -> Option<&[PathBuf]> {
        if group == muru_edge::routing::DEFAULT_GROUP {
            return Some(&self.sockets);
        }

        self.groups
            .iter()
            .find(|record| record.name == group)
            .map(|record| record.sockets.as_slice())
    }

    /// Section 7.4: the groups with no workers, whose requests wait at
    /// the edge to be woken rather than being refused: a group scaled to
    /// zero, or one whose workers are on their way.
    fn waking_groups(&self) -> Vec<String> {
        if self.backend.servers().is_some() {
            return Vec::new();
        }

        std::iter::once((muru_edge::routing::DEFAULT_GROUP, &self.sockets))
            .chain(
                self.groups
                    .iter()
                    .map(|group| (group.name.as_str(), &group.sockets)),
            )
            .filter(|(_, sockets)| sockets.is_empty())
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// Section 7.4: what the release holds against the node's budget:
    /// each group at the larger of the workers it has and the count its
    /// autoscaler is bringing it to, at what a worker costs, and the
    /// host's own memory; a managed server's instances count as workers.
    fn usage(&self, fallback: Option<u64>) -> crate::budget::Usage {
        let mut usage = crate::budget::Usage::default();

        if matches!(self.backend, Backend::Servers(_)) {
            usage.workers = self.workers() as u32;
            return usage;
        }

        for group in &self.scalers {
            let mut count = self.group_sockets(&group.name).map_or(0, <[PathBuf]>::len) as u32;

            if self.retained_until.is_none() {
                count = count.max(group.scaler.desired());
            }

            usage.add(count, self.worker_cost(group, fallback));
        }

        usage.memory += self.host_pss.unwrap_or(0);
        usage
    }

    /// The processes serving: workers in every group, or a managed
    /// server's instances.
    fn workers(&self) -> usize {
        match &self.backend {
            Backend::Servers(servers) => servers.running(),
            Backend::Host(_) | Backend::Dormant => {
                self.sockets.len()
                    + self
                        .groups
                        .iter()
                        .map(|group| group.sockets.len())
                        .sum::<usize>()
            }
        }
    }
}

/// Issue 693: a stage of a release's start a task reports to the loop.
pub enum StartStage {
    /// The environment is built and the runtime library provided: the
    /// interpreter and the library, or why not and the event to raise.
    Built(Result<(PathBuf, muru_provision::Provided), (String, &'static str)>),
    /// The host has preloaded, passed its gates and has its workers
    /// listening; or why not, the host stopped and the failure recorded.
    Host(Result<HostProcess, String>),
    /// A managed server's instances answer their probes; or why not, the
    /// instances stopped and the failure recorded.
    Servers(Result<Servers, String>),
}

/// Issue 693: what a release's start has resolved for its later stages.
struct StartPrep {
    release: Release,
    env: Vec<(String, String)>,
    identity: Option<crate::host::Identity>,
    run_dir: PathBuf,
    started: Instant,
    /// Once built: the runtime library the host was started with.
    library: Option<muru_provision::Provided>,
}

/// Issue 693: a release's start under way on tasks beside the loop.
struct Starting {
    /// What the next stage needs; taken by it.
    prep: Option<StartPrep>,
    /// Set to cancel the start: the host, if started, is stopped.
    cancel: tokio::sync::watch::Sender<bool>,
    /// Why the start was cancelled, once it has been.
    cancelled: Option<String>,
    /// The memory the release is expected to hold, counted against the
    /// node's budget while it starts.
    reserved: u64,
    /// What follows the start, carried out when it has ended.
    waiters: Vec<AfterStart>,
}

/// Issue 699: a release being stopped beside the loop.
struct Stop {
    /// When the stop's task gives up waiting for the processes.
    until: Instant,
    /// The release's run directory and the record in it, removed once it
    /// has stopped.
    run_dir: PathBuf,
    record: PathBuf,
    /// What follows the stop, carried out when it has ended.
    waiters: Vec<AfterStop>,
}

/// Issue 699: what follows a release's stop, carried out in the loop once
/// it has ended.
enum AfterStop {
    /// Answer a drain with the release as it is then.
    Drained(oneshot::Sender<Result<Release, String>>),
    /// Start the release again.
    Start(AfterStart),
    /// Go on with an application's removal.
    RemoveApp(String, oneshot::Sender<Result<(), String>>),
}

/// Issue 693: what follows a release's start, carried out in the loop
/// with how the start went: what each operation that starts a release
/// did after its start, before the start ran beside the loop.
enum AfterStart {
    /// The daemon's restore: the release serves again.
    Restore { app: String, id: String },
    /// A deploy: held, or serving with the release before it drained.
    Deploy {
        app: String,
        release: Box<Release>,
        hold: bool,
        retain: Retention,
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// A rollout's candidate, whose rollout then begins.
    StartRollout {
        app: String,
        candidate: String,
        steps: Vec<RolloutStep>,
        gates: RolloutGates,
        approve_each_step: bool,
        done: oneshot::Sender<Result<Rollout, String>>,
    },
    /// A promotion: its before_promote gates, then the switch.
    Promote {
        app: String,
        id: String,
        retain: Retention,
        then: AfterPromote,
    },
    /// An apply's prepare phase: the release runs held.
    Held {
        app: String,
        id: String,
        done: oneshot::Sender<Result<bool, String>>,
    },
    /// A release a traffic policy or a warm-up names.
    Referenced {
        app: String,
        id: String,
        warming: bool,
    },
    /// A host stopped while idle, woken by a request.
    Wake { app: String, id: String },
    /// A rollback to the release.
    Rollback {
        app: String,
        previous: String,
        current: Option<String>,
        done: oneshot::Sender<Result<Release, String>>,
    },
    /// A serving release whose host exited, started again.
    Restart { app: String, id: String },
}

/// Issue 693: what follows a promotion once it has switched, or failed.
pub enum AfterPromote {
    /// Answer whoever asked.
    Reply(oneshot::Sender<Result<Release, String>>),
    /// A rollout's promotion: record its end, and answer whoever asked
    /// for it, if anyone did.
    Rollout {
        /// The rollout.
        rollout: Box<Rollout>,
        /// How its end is described.
        message: String,
        /// Who asked, if anyone.
        done: Option<oneshot::Sender<Result<Rollout, String>>>,
    },
}

/// Issue 693: what the tasks a release's start runs on beside the loop
/// share with the daemon: the store, the event stream, what the daemon
/// publishes and its locations.
#[derive(Clone)]
struct StartContext {
    store: Arc<Store>,
    events: Events,
    info: Arc<Info>,
    locations: Locations,
}

impl StartContext {
    fn set_state(&self, release: &Release, state: ReleaseState, message: &str) {
        if let Err(error) = self
            .store
            .set_release_state(&release.app, &release.id, state, message)
        {
            tracing::error!(%error, "recording a release state failed");
        }
    }

    /// The release's log, its directory made.
    fn host_log(&self, app: &str, id: &str) -> PathBuf {
        let path = self.locations.release_log(app, id);

        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        path
    }

    /// Section 8.2: build the release's environment when its spec says
    /// so and it is not there yet (a release started again after a daemon
    /// restart, or a rollback target, keeps the one it has), as the
    /// release's user, with uv's lines in the release log tagged [build].
    /// Returns the interpreter the host runs: the environment's, or the
    /// spec's when nothing is built.
    async fn build_environment(
        &self,
        app: &str,
        id: &str,
        release: &Release,
        identity: Option<crate::host::Identity>,
    ) -> Result<PathBuf, String> {
        let spec = &release.spec;

        if !spec.build.builds {
            return Ok(spec.python.clone());
        }

        let environment =
            crate::provision::Provisioning::environment_dir(&self.locations.state.path, app, id);
        let python = environment.join("bin").join("python");

        if python.is_file() {
            return Ok(python);
        }

        let Some(uv) = self.info.provisioning.uv.clone() else {
            return Err(
                "the release needs an environment built and uv was not found; install uv, or                  deploy a source with its environment built"
                    .to_owned(),
            );
        };
        let subject = format!("{app}@{id}");
        self.set_state(release, ReleaseState::Building, "building the environment");
        self.events.emit(
            "release.building",
            &subject,
            "info",
            &format!("from {}", spec.source.join("uv.lock").display()),
        );

        // The directories the build writes, owned by the release's user:
        // the application's environments and a uv cache of its own.

        let user =
            identity.map_or_else(|| "daemon".to_owned(), |identity| identity.uid.to_string());
        let cache = self.locations.state.path.join("uv-cache").join(&user);
        let parent = environment
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.locations.state.path.clone());

        for dir in [&parent, &cache] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("creating {}: {error}", dir.display()))?;

            if let Some(identity) = identity {
                std::os::unix::fs::chown(dir, Some(identity.uid), Some(identity.gid))
                    .map_err(|error| format!("owning {}: {error}", dir.display()))?;
            }
        }

        let isolation = self.info.provisioning.build_isolation(cache);
        let build = muru_provision::Build {
            project: spec.source.clone(),
            python: Some(spec.python.display().to_string()),
            deployment: true,
            deps_only: false,
            target: Some(environment.clone()),
            extras: spec.build.extras.clone(),
            groups: spec.build.groups.clone(),
            source_builds: spec.build.source_builds,
            compile_bytecode: spec.build.compile_bytecode,
        };
        let log_path = self.host_log(app, id);
        let started = std::time::Instant::now();
        let digest = muru_provision::build::lock_digest(&spec.source).unwrap_or_default();
        let outcome = tokio::task::spawn_blocking(move || {
            use std::io::Write;

            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|error| format!("opening {}: {error}", log_path.display()))?;
            let mut command = build.command(&uv, &isolation);

            // The build runs as the release's user, so it has that user's
            // home too and not the daemon's (section 8.7).

            if let Some(identity) = identity {
                command.envs(identity.account());
            }

            crate::host::own_session_std(&mut command, identity);

            // Every line of the release's log has the full time in front,
            // as the relay puts on the host's and the workers' lines.

            let mut write = |line: &str| {
                let time = muru_common::log::full(std::time::SystemTime::now());
                let _ = writeln!(log, "{time} [build] {line}");
            };

            write(&build.describe());
            build
                .run_command(command, &mut write)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;

        outcome?;
        self.events.emit(
            "release.built",
            &subject,
            "info",
            &format!(
                "{} in {:.1}s from uv.lock {digest}",
                environment.display(),
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(python)
    }

    /// Section 8.3: the runtime library for an interpreter, from the
    /// environment, the state directory's runtime directories or a fetch,
    /// under the `[tools]` table's offline and wheel directory settings.
    async fn runtime_library(&self, python: &Path) -> Result<muru_provision::Provided, String> {
        let probe = muru_provision::probe::probe(python).map_err(|error| error.to_string())?;
        let tools = &self.info.provisioning.tools;
        let version = muru_provision::runtime::pep440(env!("CARGO_PKG_VERSION"));
        let root = self.locations.state.path.join("runtime");

        // The `[tools]` table's wheel directory, else the one the
        // environment names, as `muructl run` takes it.

        let (wheel_dir, named_by) = match tools.wheel_dir.as_deref() {
            Some(directory) => (Some(PathBuf::from(directory)), "tools.wheel_dir"),
            None => match muru_provision::runtime::wheel_dir_from_environment() {
                Some(directory) => (Some(directory), muru_provision::runtime::WHEEL_DIR_VARIABLE),
                None => (None, "tools.wheel_dir"),
            },
        };

        let options = muru_provision::runtime::Options {
            probe: &probe,
            version: &version,
            root: &root,
            wheel_dir: wheel_dir.as_deref(),
            offline: tools.offline.unwrap_or(false),
        };

        muru_provision::runtime::provide(&options, named_by)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Issue 693: how long a stopping daemon waits for the starts it has
/// cancelled to stop what they started.
const START_CANCEL_WAIT: Duration = Duration::from_secs(60);

/// Issue 693: wait, on a task beside the loop, for a host just started:
/// its preload, then the release's before_fork gates, each run to its
/// end, then its workers listening. A failure stops the host and records
/// the release failed, with the reason the log gives; a cancel stops the
/// host and leaves the release to whoever cancelled it.
#[allow(clippy::too_many_arguments)]
async fn wait_for_host(
    context: &StartContext,
    runs: &crate::runs::Runs,
    mut host: HostProcess,
    release: &Release,
    gates: Vec<crate::commands::Prepared>,
    log: &Path,
    log_start: u64,
    cancelled: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<HostProcess, String> {
    let subject = format!("{}@{}", release.app, release.id);
    let cancel = |cancelled: &mut tokio::sync::watch::Receiver<bool>| {
        let mut cancelled = cancelled.clone();
        async move {
            let _ = cancelled.wait_for(|cancelled| *cancelled).await;
        }
    };

    let waited: Result<(), String> = async {
        if !gates.is_empty() {
            tokio::select! {
                held = host.wait_held(START_TIMEOUT) => held.map_err(|error| format!("{error:#}"))?,
                () = cancel(cancelled) => return Err("its start was cancelled".to_owned()),
            }

            context.set_state(
                release,
                ReleaseState::Warming,
                "preloaded, running its before_fork gates",
            );

            let passed = tokio::select! {
                passed = run_gate_commands(context, runs, release, gates, crate::model::Gate::BeforeFork) => passed,
                () = cancel(cancelled) => Err("its start was cancelled".to_owned()),
            };

            host.release_gates(passed.is_ok());
            passed?;
        }

        context.set_state(release, ReleaseState::Warming, "waiting for the workers");

        tokio::select! {
            ready = host.wait_ready(START_TIMEOUT) => ready.map_err(|error| format!("{error:#}")),
            () = cancel(cancelled) => Err("its start was cancelled".to_owned()),
        }
    }
    .await;

    match waited {
        Ok(()) => Ok(host),
        Err(mut message) => {
            host.stop(release.spec.timeouts.stop_budget()).await;

            if *cancelled.borrow() {
                return Err(message);
            }

            // Section 9.1: the failure names its cause, wherever it is read.

            if let Some(reason) = crate::logs::start_failure(log, log_start).await {
                message = format!("{message}: {reason}");
            }

            context.set_state(release, ReleaseState::Failed, &message);
            context
                .events
                .emit("release.failed", &subject, "error", &message);
            Err(message)
        }
    }
}

/// Section 9.12 (issue 266): run a release's gates at `stage`, prepared
/// in the loop, one after another in the order of their names, each as a
/// run with the trigger `gate:STAGE`, and record each that passes. The
/// first that fails ends it, with why.
async fn run_gate_commands(
    context: &StartContext,
    runs: &crate::runs::Runs,
    release: &Release,
    gates: Vec<crate::commands::Prepared>,
    stage: crate::model::Gate,
) -> Result<(), String> {
    let subject = format!("{}@{}", release.app, release.id);

    for gate in gates {
        let name = gate.run_name().to_owned();
        let failed = |why: &str| format!("the {} gate {name} {why}", stage.as_str());
        let recorded = runs.recorded(stage.trigger(), crate::runs::DAEMON);
        let mut running = crate::commands::run(gate, Arc::new(recorded))
            .map_err(|error| failed(&format!("could not be started: {error}")))?;
        let mut ended = None;

        while let Some(message) = running.output.recv().await {
            if message.finished {
                ended = Some(message);
                break;
            }
        }

        let Some(ended) = ended else {
            return Err(failed("ended without saying how"));
        };

        if !ended.ok {
            return Err(failed(&ended.message));
        }

        if let Err(error) =
            context
                .store
                .pass_gate(&release.app, &release.id, &name, stage.as_str(), ended.run)
        {
            tracing::warn!(%error, "recording the gate {name} failed");
        }

        context.events.emit(
            "release.gate_passed",
            &subject,
            "info",
            &format!(
                "the {} gate {name} passed: run {} {}",
                stage.as_str(),
                ended.run,
                ended.message
            ),
        );
    }

    Ok(())
}

/// The daemon's state while it runs.
struct Daemon {
    store: Arc<Store>,
    events: Events,
    /// Sections 7.7 and 13.9: the daemon's own cgroup, read again at
    /// every memory check, and the memory guard watching its
    /// `memory.max`, across every release's workers (issue 622).
    cgroup: Option<crate::cgroup::Limits>,
    guard: crate::guard::Guard,
    guard_checked: Option<(Instant, Duration)>,
    locations: Locations,
    edge: EdgeProcess,
    info: Arc<Info>,
    logs: Logs,
    /// The [proxy] table of the configuration file, for every snapshot.
    proxy: Option<pb::Proxy>,
    /// The [compression] table, for every snapshot.
    compression: pb::Compression,
    /// The node-wide [error_pages] and [error_override] tables (section
    /// 4.12), for every snapshot.
    error_pages: Vec<pb::ErrorPage>,
    error_override: Option<pb::ErrorOverride>,
    /// The daemon file's client classes and rate limits (sections 4.14
    /// and 4.15).
    client_rules: crate::clients::FileRules,
    /// Section 9.10 (issue 123): the conditions the daemon's file
    /// declares, and the evaluator of those and the applied ones.
    file_conditions: BTreeMap<String, muru_config::Condition>,
    conditions: crate::conditions::Engine,
    /// Those with the applied files', for every snapshot, with the list
    /// files they were read from.
    clients: crate::clients::ClientPolicies,
    /// Which rate limits are refusing requests, for the events.
    enforcement: crate::clients::Enforcement,
    /// The applications in maintenance mode (section 4.12), with when
    /// each was put there.
    maintenance: HashMap<String, u64>,
    /// The virtual hosts in maintenance mode (section 4.12), with when
    /// each was turned on.
    host_maintenance: HashMap<String, u64>,
    /// Section 9.10: the overlays in the warm-up lead before a window,
    /// with the release each keeps running meanwhile.
    warm_ups: BTreeMap<String, Option<(String, String)>>,
    /// Section 9.10: when the clock's synchronisation was last checked,
    /// and whether the daemon has warned that it is not synchronised.
    clock: (Option<Instant>, bool),
    /// The secret stores releases may refer to (section 10.8).
    secrets: Vec<crate::secrets::Store>,
    /// Built-in log rotation (section 11.3), when it is on.
    rotation: Option<crate::rotation::Rotator>,
    /// Section 11.3: the log files a command is cutting, which rotation
    /// leaves alone.
    claimed_logs: std::collections::HashSet<PathBuf>,
    /// Section 11.3: whether a log file renamed away has its writer
    /// reopen it, the file last seen at each path, how often they are
    /// checked and when next.
    reopen_on_rename: bool,
    relay_settings: crate::host::RelaySettings,
    renames: crate::renames::Watch,
    rename_check: Duration,
    rename_due: Instant,
    /// Section 13.3: what SIGTERM means, and how a shutdown runs.
    shutdown: crate::shutdown::Policy,
    /// The service user (ADR 0026), when this daemon is root: whom the
    /// edge and the applications that name no user run as.
    service: Option<crate::host::Identity>,
    /// Whether `muructl debug exec` may run code in workers.
    debug_exec: bool,
    /// Section 9.13: whether development runs may open detours, the
    /// detours open now by release, and the serial the next opening
    /// takes. They are kept in memory alone, so a daemon that starts
    /// again has none.
    detours_enabled: bool,
    detours: BTreeMap<(String, String), crate::detours::Detour>,
    detour_serial: u64,
    /// When the detours' runs were last checked in the registry.
    detours_checked: Instant,
    /// The runs the daemon keeps (issue 688), and how many of each
    /// command's it keeps.
    runs: crate::runs::Runs,
    keep_command_runs: usize,
    /// The listeners' h2c policies, for every snapshot (section 6.13).
    h2c: Vec<pb::H2cPolicy>,
    /// The listeners' own limits, for every snapshot (section 4.18).
    listener_limits: Vec<pb::ListenerLimits>,
    /// The listeners' own proxy settings, for every snapshot (section
    /// 4.7).
    listener_proxies: Vec<pb::ListenerProxy>,
    /// The `[limits]` table (section 4.6), for every snapshot.
    limits: muru_edge::Limits,
    /// Which records the access log writes, from the `[logs]` table
    /// (section 11.2), for every snapshot.
    access_log: muru_edge::access::Filter,
    /// Changes to it for a period, the newest last (section 11.2): kept
    /// only in memory, so a daemon that starts again has none.
    access_overrides: Vec<muru_edge::access::Override>,
    /// The command line's settings, laid over the file on every reload.
    overrides: crate::settings::Overrides,
    /// The settings read only at start, as this daemon started with
    /// them, which a reload compares the file's with.
    read_at_start: Vec<(&'static str, String)>,
    /// The event sinks delivering (section 11.5), reconciled on reload.
    sinks: crate::sinks::Sinks,
    /// Section 4.2: each TLS host's last good certificate chain and key
    /// as PEM text, by host, for the snapshots; read again on reload and
    /// apply, and kept when the files are wrong.
    certificates: HashMap<String, (String, String)>,
    /// Section 7.4: the node's budget, and the schedule windows already
    /// checked against it, by overlay and start.
    /// The `[daemon.budget]` table as the file sets it; `budget()` is
    /// the budget in force, with the cgroup's limits for what it leaves
    /// out (issue 384).
    budget_table: crate::budget::Budget,
    budget_checked: BTreeSet<(String, u64)>,
    /// The points the dashboard's charts draw, one a second.
    history: Arc<muru_dashboard::History>,
    /// Turns the edge's counters into those points.
    sampler: muru_dashboard::Sampler,
    running: HashMap<(String, String), Running>,
    /// Section 9.3: the counters each running rollout's current step
    /// started from, by application.
    rollout_runs: HashMap<String, RolloutRun>,
    /// Section 9.3: each release's worker crashes and timeout kills, from
    /// the host's events, for the gates.
    worker_faults: HashMap<(String, String), u64>,
    /// Section 9.3: each release's failed probes and how the latest
    /// failed, written by the probe tasks.
    probe_failures: ProbeFailures,
    /// When each application's rollout probes were last sent.
    probed: HashMap<String, Instant>,
    /// The client probes are sent with.
    probe_client: crate::rollouts::ProbeClient,
    /// The release each application's traffic goes to, as the edge has
    /// been told; the store catches up once a switch has happened.
    serving: HashMap<String, String>,
    /// The applications serving with no host to choose between them, as
    /// last announced by a `routing.no_host` event.
    no_host: BTreeSet<String>,
    /// Whether this process writes to the daemon log file, as a daemon
    /// started in the background does, so a reopen applies to it too.
    owns_log: bool,
    /// Where the hosts' control messages are sent, into the same inbox
    /// as the commands.
    commands: mpsc::Sender<Command>,
    /// Requests to hosts awaiting their answers, by request id.
    asked: HashMap<u64, oneshot::Sender<Result<Vec<WorkerReply>, String>>>,
    /// Section 6.18: the import reports hosts are sending in parts, by
    /// release.
    imports_parts: HashMap<(String, String), crate::host::ImportsParts>,
    /// The last request id given out.
    last_request: u64,
    /// Section 9.12 (issue 268): each scheduled command's next due time,
    /// by application and command, with the schedule it was worked out
    /// from.
    schedules: HashMap<(String, String), Scheduled>,
    /// Issue 693: the releases being started beside the loop.
    starting: HashMap<(String, String), Starting>,
    /// Issue 693: the starts waiting for another of their application's
    /// to end, in the order they were asked for.
    start_queue: HashMap<String, std::collections::VecDeque<(String, AfterStart)>>,
    /// Issue 693: the applications whose rollout is being promoted.
    promoting: BTreeSet<String>,
    /// Issue 699: the releases being stopped beside the loop.
    stops: HashMap<(String, String), Stop>,
    /// Section 9.10 (issue 689): the overlays whose activation by their
    /// firing condition was skipped, so the event is raised once while the
    /// condition keeps firing.
    condition_skips: BTreeSet<String>,
    /// Whether this daemon started from nothing, with no edge left by a
    /// previous one to take over: its first start, after a reboot or
    /// after a shutdown. Commands with `at_boot` run then.
    booted: bool,
}

/// The listeners as a start says them: each as a URL, joined with `and`.
fn serving_on_text(listeners: &[(SocketAddr, bool)]) -> String {
    listeners
        .iter()
        .map(|(address, tls)| format!("{}://{address}", if *tls { "https" } else { "http" }))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Run the daemon until it is told to stop: bind the edge, serve the admin
/// API, start the releases that were serving, then reconcile commands as
/// they arrive.
pub async fn run(config: Config) -> anyhow::Result<()> {
    config
        .locations
        .check_run_dir()
        .map_err(|error| anyhow::anyhow!(error))?;

    // Section 9.13: a system daemon's edge serves real traffic, which a
    // detour would send to a process owned by whoever opened it.

    if let Some(refusal) =
        crate::detours::setting_refusal(config.settings.detours, config.locations.mode)
    {
        anyhow::bail!(refusal);
    }

    // The file was checked whole when it was read (section 10.3); a
    // reload reads it the same way and applies the difference.

    let mut settings = config.settings;
    let proxy = settings.proxy.clone();
    let compression = settings.compression.clone();
    let error_pages = settings.error_pages.clone();
    let error_override = settings.error_override.clone();
    let client_rules = settings.client_rules.clone();
    let metrics = settings.metrics;
    let dashboard = settings.dashboard;
    let dashboard_sessions = dashboard.is_some().then(muru_dashboard::Sessions::new);
    let sinks = std::mem::take(&mut settings.sinks);
    let locations = config.locations;

    for location in [&locations.state, &locations.run, &locations.logs] {
        std::fs::create_dir_all(&location.path)
            .with_context(|| format!("creating {}", location.path.display()))?;
    }

    // Section 13.8: the daemon registers in the instance registry, under
    // the ID kept in its state directory, taking over the entry it left
    // when it detached or was killed. It is written again once the edge
    // is up, and whenever what it says changes.

    let instance_id =
        crate::registry::InstanceId::kept_in(&locations.state.path.join(INSTANCE_ID_FILE))
            .with_context(|| {
                format!(
                    "keeping the instance ID in {}",
                    locations.state.path.display()
                )
            })?;
    let registered = match crate::registry::Entry::publish(
        &crate::registry::user_registry(),
        daemon_record(
            instance_id,
            &locations,
            &settings.listeners,
            settings.dashboard,
            None,
        ),
    ) {
        Ok(entry) => Some(entry),
        Err(error) => {
            tracing::warn!(%error, "registering in the instance registry failed");
            None
        }
    };

    // ADR 0026: a daemon that is root starts everything else as the
    // service user, and does not start without one.

    let service = service_identity(settings.service_user.as_deref())?;

    if let (Some(service), Some(access_log)) = (&service, &config.access_log) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(access_log);
        service
            .own(access_log)
            .with_context(|| format!("giving {} to the service user", access_log.display()))?;
    }

    let store = Arc::new(Store::open(&locations.state_store())?);
    let clients = client_rules
        .policies(&store)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let events = Events::new(Arc::clone(&store));

    // Section 11.5: each sink delivers from its own cursor, so nothing
    // emitted from here on is missed by one.

    let sinks = crate::sinks::Sinks::start(sinks, Arc::clone(&store), events.clone());

    // An edge a previous daemon left running is taken over, with its
    // listener handed back. Otherwise the daemon binds the listener and
    // keeps its own copy, so the edge process can be replaced while
    // connections wait in the backlog; the edge is up before any release,
    // answering 503 until one serves.

    let edge_log = locations.edge_log();

    // Section 4.11: the upload spool is the edge's own, in the state
    // directory rather than the run directory, which may be memory.

    let spool = locations.state.path.join("spool");
    prepare_spool(&spool, service)
        .with_context(|| format!("preparing the upload spool {}", spool.display()))?;

    let adopted = match read_record::<EdgeRecord>(&locations.run.path.join("edge.toml")) {
        Some(record) => EdgeProcess::adopt(
            record.pid,
            config.executable.clone(),
            record.control,
            record.listener_socket,
            record.generation,
            record
                .listeners
                .iter()
                .map(|listener| listener.tls)
                .collect(),
            edge_log.clone(),
            config.access_log.clone(),
            spool.clone(),
            service,
            locations.scope(),
        )
        .await
        .map_err(|error| tracing::info!(%error, "no edge to re-adopt"))
        .ok(),
        None => None,
    };

    let booted = adopted.is_none();
    let wanted_listeners = settings.listeners.clone();
    let read_at_start = settings.at_start();

    let edge = match adopted {
        Some(edge) => {
            tracing::info!(pid = edge.pid().unwrap_or(0), "re-adopted the edge");
            edge
        }
        None => {
            anyhow::ensure!(!settings.listeners.is_empty(), "no listener to serve on");
            let mut listeners = Vec::new();

            for (address, tls) in &settings.listeners {
                let socket = std::net::TcpListener::bind(address)
                    .map_err(|error| bind_refused(error, *address, "listener"))
                    .with_context(|| format!("cannot listen on {address}"))?;
                listeners.push(crate::edges::Listener { socket, tls: *tls });
            }

            let (control, listener_socket) =
                crate::edges::socket_paths(&crate::edges::directory(&locations.run.path), 1);
            EdgeProcess::spawn(EdgeSpec {
                executable: config.executable.clone(),
                listeners,
                control,
                listener_socket,
                generation: 1,
                log: edge_log,
                access_log: config.access_log,
                spool,
                identity: service,
                scope: locations.scope(),
            })
            .await?
        }
    };

    let listeners = edge.listeners();

    // Section 8.4: uv as found and how it is run, said once at start.

    let provisioning =
        crate::provision::Provisioning::for_daemon(&settings.tools, &locations.state.path);
    tracing::info!("{}", provisioning.describe_uv());
    write_edge_record(&locations.run.path, &edge);

    if let Some(entry) = &registered {
        entry.update(|record| {
            *record = daemon_record(
                instance_id,
                &locations,
                &edge.listeners(),
                dashboard,
                edge.pid(),
            );
        });
    }

    // The admin API on its socket.

    let (commands, mut inbox) = mpsc::channel::<Command>(64);
    let handle_commands = commands.clone();
    let info = Arc::new(Info {
        locations: locations.clone(),
        listeners: std::sync::RwLock::new(listeners.clone()),
        limits: std::sync::RwLock::new(settings.limits.clone()),
        started_unix_ms: crate::model::now_unix_ms(),
        edge_pid: std::sync::atomic::AtomicU32::new(edge.pid().unwrap_or(0)),
        metrics,
        dashboard,
        dashboard_sessions: dashboard_sessions.clone(),
        defined: std::sync::RwLock::new(crate::config::Defined {
            budget: crate::budget::View {
                budget: settings.budget,
                ..crate::budget::View::default()
            },
            conditions: settings.conditions.clone(),
            ..client_rules.defined()
        }),
        python_logging: std::sync::RwLock::new(settings.python_logging.clone()),
        interfaces: Interfaces::default(),
        websockets: std::sync::Mutex::new(HashMap::new()),
        process_figures: crate::processes::Live::default(),
        cpu: Arc::default(),
        live: std::sync::Mutex::new(HashMap::new()),
        no_host: std::sync::Mutex::new(String::new()),
        pass_env: std::sync::RwLock::new(settings.pass_env.clone()),
        service,
        servers: std::sync::Mutex::new(HashMap::new()),
        conditions: std::sync::Mutex::new(Vec::new()),
        provisioning,
        started: std::sync::atomic::AtomicBool::new(false),
        stopping: std::sync::atomic::AtomicBool::new(false),
        cgroup_limits: std::sync::Mutex::new(None),
        instance_id,
        detours: std::sync::Mutex::new(Vec::new()),
        start_flags: config.overrides.flags(),
    });
    let underway = crate::commands::Underway::default();
    let runs = crate::runs::Runs::new(
        Arc::clone(&store),
        events.clone(),
        locations.logs.path.clone(),
        settings.keep_command_runs,
    );
    let handle = Handle {
        store: Arc::clone(&store),
        events: events.clone(),
        commands,
        info: Arc::clone(&info),
        underway: underway.clone(),
        runs: runs.clone(),
    };

    // The metrics listener (section 11.1), on an internal address.

    let scraper = match metrics {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .map_err(|error| bind_refused(error, address, "metrics address"))
                .with_context(|| format!("cannot listen for metrics on {address}"))?;
            Some(tokio::spawn(crate::metrics::serve(
                listener,
                handle.commands.clone(),
                crate::probes::Probes {
                    store: Arc::clone(&store),
                    info: Arc::clone(&info),
                },
            )))
        }
        None => None,
    };

    // The dashboard and gateway (section 10.7), on an internal address,
    // with the history the reconciler records every second for its
    // charts.

    let history = Arc::new(muru_dashboard::History::new());

    let pages = match dashboard {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .map_err(|error| bind_refused(error, address, "dashboard address"))
                .with_context(|| format!("cannot listen for the dashboard on {address}"))?;
            Some(tokio::spawn(crate::dashboard::serve(
                listener,
                handle.commands.clone(),
                Arc::clone(&history),
                events.clone(),
                dashboard_sessions.unwrap_or_else(muru_dashboard::Sessions::new),
                Arc::clone(&handle.info),
            )))
        }
        None => None,
    };

    // The overlays' external trigger (section 9.10), on its own address.

    let triggers = match settings.overlay_webhook.take() {
        Some(webhook) => {
            let listener = tokio::net::TcpListener::bind(webhook.address)
                .await
                .with_context(|| {
                    format!(
                        "cannot listen for the overlay webhook on {}",
                        webhook.address
                    )
                })?;
            Some(tokio::spawn(crate::triggers::serve(
                listener,
                webhook.secret,
                Arc::clone(&handle.store),
                handle.commands.clone(),
            )))
        }
        None => None,
    };

    let socket = locations.admin_socket();
    let (api_stop, api_stopped) = oneshot::channel();
    let owns_log = writes_to(&locations.daemon_log());

    // A daemon started in the background names itself in the pid file
    // before the admin socket answers, and removes it when it exits; an
    // upgraded one is the same process and writes the same pid.

    if owns_log {
        let path = locations.pid_file();

        if let Err(error) = muru_common::files::write_whole(&path, std::process::id().to_string()) {
            tracing::warn!(%error, path = %path.display(), "writing the pid file failed");
        }
    }

    let api = match crate::api::serve(socket.clone(), handle, api_stopped).await {
        Ok(api) => api,
        Err(error) => {
            remove_pid_file(&locations);
            return Err(error);
        }
    };

    let mut api = tokio::spawn(api);
    let serving_on = serving_on_text(&listeners);

    match &config.upgraded_from {
        Some(before) => events.emit(
            "daemon.upgraded",
            "daemon",
            "info",
            &format!(
                "re-executed as version {} (was {before}) from {}, pid {}; serving on {serving_on}",
                env!("CARGO_PKG_VERSION"),
                config.executable.display(),
                std::process::id()
            ),
        ),
        None => events.emit(
            "daemon.started",
            "daemon",
            "info",
            &format!("serving on {serving_on}, admin socket {}", socket.display()),
        ),
    }

    let rotation = settings
        .rotation
        .clone()
        .map(|policy| restored_rotator(policy, &store));

    // Section 11.3: the files last seen at the logs' paths, so that a
    // rename made while the daemon was stopped is noticed at its first
    // check.

    let renames =
        crate::renames::Watch::from_seen(store.log_files_seen().unwrap_or_else(|error| {
            tracing::warn!(%error, "reading the log files last seen failed");
            Vec::new()
        }));

    let mut daemon = Daemon {
        store,
        events,
        locations,
        edge,
        info,
        logs: Logs::default(),
        proxy,
        compression,
        error_pages,
        error_override,
        client_rules,
        file_conditions: settings.conditions.clone(),
        conditions: crate::conditions::Engine::default(),
        clients,
        enforcement: crate::clients::Enforcement::default(),
        maintenance: HashMap::new(),
        host_maintenance: HashMap::new(),
        warm_ups: BTreeMap::new(),
        clock: (None, false),
        secrets: settings.secrets.clone(),
        rotation,
        claimed_logs: std::collections::HashSet::new(),
        reopen_on_rename: settings.reopen_on_rename,
        relay_settings: settings.relay,
        renames,
        rename_check: config.rename_check,
        rename_due: Instant::now(),
        shutdown: settings.shutdown.clone(),
        service,
        debug_exec: settings.debug_exec,
        detours_enabled: settings.detours,
        detours: BTreeMap::new(),
        detour_serial: 0,
        detours_checked: Instant::now(),
        runs: runs.clone(),
        keep_command_runs: settings.keep_command_runs,
        h2c: settings.h2c.clone(),
        listener_limits: settings.listener_limits.clone(),
        listener_proxies: settings.listener_proxies.clone(),
        limits: settings.limits.clone(),
        access_log: settings.access_log.clone(),
        access_overrides: Vec::new(),
        overrides: config.overrides,
        read_at_start,
        sinks,
        certificates: HashMap::new(),
        budget_table: settings.budget,
        budget_checked: BTreeSet::new(),
        history,
        sampler: muru_dashboard::Sampler::default(),
        running: HashMap::new(),
        rollout_runs: HashMap::new(),
        worker_faults: HashMap::new(),
        probe_failures: Arc::default(),
        probed: HashMap::new(),
        probe_client: crate::rollouts::probe_client(),
        serving: HashMap::new(),
        no_host: BTreeSet::new(),
        owns_log,
        commands: handle_commands,
        asked: HashMap::new(),
        imports_parts: HashMap::new(),
        last_request: 0,
        schedules: HashMap::new(),
        condition_skips: BTreeSet::new(),
        starting: HashMap::new(),
        start_queue: HashMap::new(),
        promoting: BTreeSet::new(),
        stops: HashMap::new(),
        booted,
        cgroup: crate::cgroup::Limits::read(),
        guard: crate::guard::Guard::default(),
        guard_checked: None,
    };

    if let Some(limits) = &daemon.cgroup {
        tracing::info!("cgroup limits: {}", limits.describe());
        let budget = daemon.budget();

        if budget.is_set() {
            tracing::info!(
                "node budget: {}{}",
                budget.describe(&crate::budget::Usage::default()),
                daemon.budget_sources()
            );
        }
    }

    // ADR 0081: the files a start applies are planned against the store
    // before anything is restored, and what is pending is said first, so
    // the log shows why a release starts only to be replaced, or that the
    // files cannot be applied and the daemon will exit once what the
    // store holds is back.

    let start_apply = config
        .start_apply
        .filter(|_| config.upgraded_from.is_none());

    if let Some(apply) = &start_apply {
        let defined = daemon
            .info
            .defined
            .read()
            .expect("defined names poisoned")
            .clone();
        let (possible, what) = apply.pending(&daemon.store, &defined, &daemon.info.provisioning);
        daemon.events.emit(
            "config.apply_pending",
            "config",
            if possible { "notice" } else { "error" },
            &format!(
                "start apply of {}: {what}; {}",
                apply.paths,
                if possible {
                    "applied once what the store holds is restored"
                } else {
                    "what the store holds is restored before the daemon exits"
                }
            ),
        );
    }

    daemon.restore().await;
    daemon.reconcile_traffic().await;
    daemon.run_at_boot().await;

    // The edge has the node's settings, a listener's h2c among them, from
    // the start, whether or not any release runs yet.

    if let Err(error) = daemon.push_snapshot().await {
        tracing::warn!(%error, "pushing the first snapshot failed");
    }

    // Issue 930: a start and a reload read the file the same way. An
    // edge re-adopted after a detach has the listeners it was started
    // with, so a file that now names others takes a new edge, as a reload
    // gives it, where the old listeners served until the next reload.

    let relisten = if daemon.booted {
        None
    } else {
        daemon
            .bind_listeners(&wanted_listeners)
            .map_err(anyhow::Error::msg)?
    };

    // Section 13.4: an upgraded daemon replaces the edge with one from
    // its own image once the hosts are re-adopted and the snapshot is
    // whole again.

    match relisten {
        Some((listeners, what)) => {
            daemon
                .replace_edge(Some(listeners), &what)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        None if config.upgraded_from.is_some() => {
            let _ = daemon.replace_edge(None, "the same listeners").await;
        }
        None => {}
    }

    let serving_on = serving_on_text(&daemon.edge.listeners());

    // Section 7.4: the edge says when a request waits for a group with
    // no workers.

    let (wake_sender, mut wakes) = mpsc::channel(64);
    daemon.edge.watch_wakes(wake_sender);

    // ADR 0081: readiness waits for the files a start applies. They are
    // applied through the daemon's own admin API once the loop below is
    // carrying out commands, as muructl apply would apply them.

    let (applied_tx, mut applied) = oneshot::channel::<Result<String, String>>();
    let mut applying = start_apply.is_some();
    let apply_paths = start_apply
        .as_ref()
        .map(|apply| apply.paths.clone())
        .unwrap_or_default();

    match start_apply {
        Some(apply) => {
            let socket = daemon.locations.admin_socket();
            tokio::spawn(async move {
                let _ = applied_tx.send(apply.run(socket).await);
            });
        }
        None => {
            crate::notify::ready(&format!("serving on {serving_on}"));
            daemon
                .info
                .started
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Commands are carried out one at a time; a host dying in between is
    // noticed by the next pass. SIGTERM and SIGINT detach, as a service
    // manager's stop or a terminal's interrupt should, unless the
    // [shutdown] table or being a container's main process makes them
    // shut everything down (section 13.3), and SIGHUP reloads the
    // configuration file.

    let mut drain_on_exit = true;
    let mut upgrade_to: Option<Upgrade> = None;
    let mut reopen = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // The once-a-second work keeps its own clock, so a stream of commands
    // arriving faster than that never starves it.

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;

    // Section 11.3: a log file renamed while the daemon was stopped is
    // noticed now, before anything else is written to it.

    daemon.watch_renames();

    let outcome = loop {
        tokio::select! {
            _ = reopen.recv() => {
                daemon.reopen_logs(false);
            }
            _ = hangup.recv() => {
                if let Err(error) = daemon.reload().await {
                    tracing::warn!(%error, "reloading the configuration failed");
                }
            }
            _ = terminate.recv() => {
                drain_on_exit = daemon.signalled("SIGTERM");
                break Ok(());
            }
            _ = interrupt.recv() => {
                drain_on_exit = daemon.signalled("SIGINT");
                break Ok(());
            }
            command = inbox.recv() => match command {
                Some(Command::Shutdown { drain, done }) => {
                    drain_on_exit = drain;
                    let _ = done.send(());
                    break Ok(());
                }
                Some(Command::Upgrade { done }) => match prepare_upgrade().await {
                    Ok(upgrade) => {
                        let _ = done.send(Ok(upgrade.clone()));
                        upgrade_to = Some(upgrade);
                        drain_on_exit = false;
                        break Ok(());
                    }
                    Err(error) => {
                        let _ = done.send(Err(error));
                    }
                },
                Some(command) => daemon.carry_out(command).await,
                None => break Ok(()),
            },
            Some(wake) = wakes.recv() => {
                daemon.wake(wake).await;
            }
            result = &mut applied, if applying => {
                applying = false;

                match result.unwrap_or_else(|_| Err("the apply did not finish".to_owned())) {
                    Ok(what) => {
                        tracing::info!("start apply of {apply_paths}: {what}");
                        crate::notify::ready(&format!("serving on {serving_on}"));
                        daemon
                            .info
                            .started
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) => {
                        // A failed start apply fails the start: the daemon
                        // exits, leaving what serves detached for the next
                        // daemon, as muructl daemon stop leaves it.

                        let message = format!(
                            "start apply of {apply_paths} failed: {error}; the daemon exits, and \
                             what was serving is left serving"
                        );
                        daemon.events.emit("daemon.start_failed", "daemon", "error", &message);
                        drain_on_exit = false;
                        break Err(anyhow::anyhow!(message));
                    }
                }
            }
            served = &mut api => {
                break served.map_err(anyhow::Error::from).and_then(|result| result);
            }
            _ = tick.tick() => {
                if let Some(entry) = &registered {
                    refresh_record(entry, &daemon.info);
                }

                daemon.check_edge().await;
                daemon.check_hosts().await;
                daemon.run_schedules().await;
                daemon.expire_sticky().await;
                daemon.expire_retained().await;
                daemon.rotate_logs();
                daemon.watch_renames();
                daemon.expire_access_overrides().await;

                let samples = match daemon.edge.metrics().await {
                    Ok(samples) => Some(samples),
                    Err(error) => {
                        tracing::debug!(%error, "the edge's metrics could not be read");
                        None
                    }
                };

                daemon.sample_history(samples.as_deref()).await;
                daemon.evaluate_conditions(samples.as_deref());
                daemon.advance_rollouts(samples.as_deref()).await;
                daemon.autoscale(samples.as_deref()).await;
                daemon.guard_memory();
                daemon.refresh_client_lists().await;
                daemon.reconcile_overlays().await;
                daemon.expire_detour_traffic().await;
                daemon.check_detours().await;
                daemon.check_clock();
                daemon.watch_rate_limits(samples.as_deref());
            }
        }

        daemon.publish_live();
    };

    daemon
        .info
        .stopping
        .store(true, std::sync::atomic::Ordering::Relaxed);

    // Section 9.13: the detours end with the daemon, and their runs open
    // them again on the next one.

    daemon.close_detours("the daemon is stopping").await;

    // Section 13.3: by default the daemon detaches, leaving hosts and the
    // edge serving for the next daemon to re-adopt; with drain it takes
    // everything down with it. For an upgrade (section 13.4) it detaches
    // the same way and then re-executes itself, so the next daemon is
    // this process with the installed binary's image.

    if upgrade_to.is_some() {
        crate::notify::notify("RELOADING=1\nSTATUS=upgrading");
    } else {
        crate::notify::stopping();
    }

    // Issue 693: a host still starting has nothing the next daemon could
    // adopt, so every start under way is cancelled, whether the daemon
    // drains, detaches or upgrades, and its report waited for, which
    // stops what it started. Issue 699: a release being stopped has left
    // what the next daemon adopts too, so its stop is waited for, each
    // as long as its own wait for its processes. Other commands arriving
    // meanwhile are dropped, so their callers hear the daemon has gone;
    // what waited on a start or a stop is told it failed or carried out
    // as far as a daemon that stops can.

    daemon.cancel_starts("the daemon is stopping").await;
    let deadline = daemon
        .stops
        .values()
        .map(|stop| stop.until + Duration::from_secs(1))
        .fold(Instant::now() + START_CANCEL_WAIT, Instant::max);

    while !daemon.starting.is_empty() || !daemon.stops.is_empty() {
        match tokio::time::timeout_at(deadline.into(), inbox.recv()).await {
            Ok(Some(Command::StartStage { app, id, stage })) => {
                daemon.start_stage(&app, &id, *stage).await;
            }
            Ok(Some(Command::Stopped {
                app,
                id,
                why,
                timed_out,
            })) => {
                daemon.stopped(&app, &id, &why, timed_out).await;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                tracing::warn!(
                    starts = daemon.starting.len(),
                    stops = daemon.stops.len(),
                    "starts and stops under way did not end before the daemon stopped"
                );
                break;
            }
        }
    }

    // Section 9.12: a command cannot outlive the daemon it runs under, so
    // the commands under way are stopped as at their timeout, inside the
    // shutdown's budget when there is one, while the rest stops. They are
    // waited for before the API goes, so each caller hears how its
    // command ended.

    let (why, commands_deadline) = if drain_on_exit {
        (
            "the daemon shut down",
            Instant::now() + daemon.shutdown.budget,
        )
    } else {
        (
            "the daemon stopped",
            Instant::now() + crate::commands::STOP_GRACE,
        )
    };

    let stopping_commands = tokio::spawn(async move {
        underway.stop_all(why, commands_deadline.into()).await;
    });

    if drain_on_exit {
        // The shutdown, as the stop signal and muructl daemon stop
        // --shutdown both run it, inside the policy's budget: the delay
        // with the edge still accepting, the drain of the requests in
        // flight, every host stopped at once, and the edge last, so the
        // workers' WebSocket close frames get through it.

        let policy = daemon.shutdown.clone();
        let started = Instant::now();
        let deadline = started + policy.budget;
        let runs = daemon.runs.clone();
        let stopping_runs =
            tokio::spawn(
                async move { runs.stop_all("the daemon shut down", deadline.into()).await },
            );
        daemon.drain_edge(&policy, deadline).await;
        daemon.stop_all_until(deadline).await;
        let _ = stopping_runs.await;
        daemon
            .edge
            .stop_within(deadline.saturating_duration_since(Instant::now()))
            .await;
        forget_edge(&daemon.locations);
        daemon.events.emit(
            "daemon.stopped",
            "daemon",
            "info",
            &format!(
                "stopped in {:.1} s of the {} budget",
                started.elapsed().as_secs_f64(),
                muru_config::format_duration(policy.budget)
            ),
        );
    } else if upgrade_to.is_none() {
        daemon.detach();
        daemon.runs.release();
        keep_record(registered.as_ref());
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "info",
            "detached; hosts, the edge and detached command runs keep running",
        );
    } else {
        daemon.detach();
        daemon.runs.release();
        keep_record(registered.as_ref());
    }

    // The API answers the Shutdown call that brought the daemon here
    // before the socket goes. A client holding a connection open would
    // keep the server from finishing, so after a grace period it is
    // aborted, which closes the listener and every connection before the
    // socket is unlinked; a stop command polling the socket then gets a
    // refusal straight away instead of waiting on a silent connection.

    let _ = stopping_commands.await;
    let _ = api_stop.send(());

    if tokio::time::timeout(Duration::from_secs(5), &mut api)
        .await
        .is_err()
    {
        api.abort();
    }

    if let Some(scraper) = scraper {
        scraper.abort();
    }

    daemon.sinks.stop();

    if let Some(pages) = pages {
        pages.abort();
    }

    if let Some(triggers) = triggers {
        triggers.abort();
    }

    // The pid file goes before the socket, which muructl daemon stop waits
    // on; an upgrade keeps it for the same pid, unless the exec fails.

    if upgrade_to.is_none() {
        remove_pid_file(&daemon.locations);
    }

    let _ = std::fs::remove_file(&socket);

    if let Some(upgrade) = upgrade_to {
        // exec returns only when it failed; the hosts and the edge are
        // still serving, so this daemon leaves as a detached one does.

        // The locks on the directories are kept through the exec, so
        // there is no moment when another daemon could take them.

        let mut arguments = config.upgrade_arguments.clone();
        let held = config.ownership.keep_across_exec();

        if !held.is_empty() {
            arguments.push(crate::ownership::HELD_LOCKS_FLAG.into());
            arguments.push(
                held.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
                    .into(),
            );
        }

        let error = exec_daemon(&upgrade.executable, &arguments);
        config.ownership.close_on_exec();
        remove_pid_file(&daemon.locations);
        daemon.events.emit(
            "daemon.detached",
            "daemon",
            "error",
            &format!(
                "re-executing {} failed: {error}; hosts and the edge keep serving",
                upgrade.executable.display()
            ),
        );
        return Err(anyhow::anyhow!(
            "re-executing {} failed: {error}",
            upgrade.executable.display()
        ));
    }

    outcome
}

/// A bind the daemon was refused, as an error: with the instance of
/// Muru's that holds the address named when one does (issue 808), or
/// else the program lsof or ss name (issue 837), as the error's own
/// message under the context the caller adds.
fn bind_refused(error: std::io::Error, address: SocketAddr, what: &str) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::AddrInUse
        && let Some(message) =
            crate::registry::in_use_by(address, &format!("choose another {what}"))
    {
        return anyhow::anyhow!(message);
    }

    anyhow::Error::from(error)
}

/// The file in the state directory that keeps a daemon's instance ID.
pub const INSTANCE_ID_FILE: &str = "instance-id";

/// A daemon's registry record (section 13.8): where it is, what it
/// listens on, and its edge.
fn daemon_record(
    id: crate::registry::InstanceId,
    locations: &Locations,
    listeners: &[(SocketAddr, bool)],
    dashboard: Option<SocketAddr>,
    edge_pid: Option<u32>,
) -> crate::registry::Record {
    let mut record = crate::registry::Record::new(
        id,
        crate::registry::Kind::Daemon,
        crate::client::Endpoint::unix(locations.admin_socket()),
        locations.run.path.clone(),
    );

    record.name = locations.instance_name();
    record.scope = locations.mode_word().to_owned();
    record.state_dir = Some(locations.state.path.clone());
    record.config = Some(locations.config.path.clone());
    record.log_dir = Some(locations.logs.path.clone());
    record.dashboard = dashboard.map(|address| format!("http://{address}/"));
    record.edge_pid = edge_pid.filter(|pid| *pid != 0);

    // Before the edge is up the listeners are as configured; once it is,
    // as it serves them.

    record.listeners = listeners
        .iter()
        .map(|(address, tls)| crate::registry::Listener {
            name: if *tls { "https" } else { "http" }.to_owned(),
            configured: address.to_string(),
            bound: edge_pid.map(|_| address.to_string()),
            tls: *tls,
        })
        .collect();

    record
}

/// Write a daemon's record again when its listeners or edge changed.
fn refresh_record(entry: &crate::registry::Entry, info: &Info) {
    let listeners = info.listeners.read().expect("listeners poisoned").clone();
    let edge_pid = info.edge_pid.load(std::sync::atomic::Ordering::Relaxed);
    let record = daemon_record(
        info.instance_id,
        &info.locations,
        &listeners,
        info.dashboard,
        Some(edge_pid),
    );

    entry.update(|current| {
        current.listeners = record.listeners;
        current.edge_pid = record.edge_pid;
    });
}

/// Leave the daemon's registry entry for the next daemon to take over,
/// when it detaches or upgrades: the edge and hosts it leaves serving
/// belong to it.
fn keep_record(entry: Option<&crate::registry::Entry>) {
    if let Some(entry) = entry {
        entry.keep();
    }
}

/// Numbers each command run's directory of secret files.
static COMMAND_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Give a command run's secret files, and their directory, to the user it
/// runs as.
fn own_secret_files(identity: crate::host::Identity, dir: &Path) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };

    let paths =
        std::iter::once(dir.to_path_buf()).chain(entries.flatten().map(|entry| entry.path()));

    for path in paths {
        identity.own(&path).map_err(|error| {
            format!(
                "giving {} to the application's user failed: {error}",
                path.display()
            )
        })?;
    }

    Ok(())
}

/// Remove the pid file if it names this process: a daemon run in the
/// foreground never wrote it, and leaves another's alone.
fn remove_pid_file(locations: &Locations) {
    let path = locations.pid_file();
    let ours = std::fs::read_to_string(&path)
        .is_ok_and(|text| text.trim() == std::process::id().to_string());

    if ours {
        let _ = std::fs::remove_file(&path);
    }
}

/// After a full shutdown, remove what only a running edge needs: the
/// record the next daemon would adopt it by, and the rate limits' shared
/// buckets, with the edges' directory when that leaves it empty.
fn forget_edge(locations: &Locations) {
    let run_dir = &locations.run.path;
    let edges = crate::edges::directory(run_dir);
    let _ = std::fs::remove_file(run_dir.join("edge.toml"));
    let _ = std::fs::remove_file(edges.join(crate::edges::BUCKETS_FILE));
    let _ = std::fs::remove_dir(edges);
}

/// Remove a release's record, its host's secret files and the
/// directories that leaves empty: the release's own and then its
/// application's.
fn forget_release_dir(run_dir: &Path, record: &Path) {
    let _ = std::fs::remove_file(record);
    let _ = std::fs::remove_dir_all(run_dir.join("secrets"));

    if std::fs::remove_dir(run_dir).is_ok()
        && let Some(app_dir) = run_dir.parent()
    {
        let _ = std::fs::remove_dir(app_dir);
    }
}

/// Section 10.8: the environment a release's host starts with, its
/// secrets resolved from the stores; a secret delivered as a file is
/// written under `secrets_dir` and the variable NAME_FILE names it.
async fn resolve_env(
    stores: &[crate::secrets::Store],
    env: &[crate::model::EnvSpec],
    secrets_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let mut resolved = Vec::with_capacity(env.len());

    for entry in env {
        let (Some(store_name), Some(key)) = (&entry.secret_store, &entry.secret_key) else {
            resolved.push((entry.name.clone(), entry.value.clone().unwrap_or_default()));
            continue;
        };
        let store = stores
            .iter()
            .find(|store| &store.name == store_name)
            .ok_or_else(|| format!("env {}: no secret store named {store_name}", entry.name))?;
        let value = store.resolve(key).await?;

        if entry.as_file {
            let path = secrets_dir.join(&entry.name);
            crate::secrets::write_private(&path, &value).map_err(|error| {
                format!(
                    "env {}: writing the secret file failed: {error}",
                    entry.name
                )
            })?;
            resolved.push((format!("{}_FILE", entry.name), path.display().to_string()));
        } else {
            resolved.push((entry.name.clone(), value));
        }
    }

    Ok(resolved)
}

/// Write the edge's record for the next daemon.
/// The service user (ADR 0026) for a daemon that is root: the one named,
/// else `muru`, which must exist and must not be root. A daemon that is
/// not root switches to no one, and says so when a user was named.
fn service_identity(named: Option<&str>) -> anyhow::Result<Option<crate::host::Identity>> {
    if !crate::host::Identity::can_switch() {
        if let Some(name) = named {
            tracing::info!("service user {name} not applied: the daemon is not root");
        }

        return Ok(None);
    }

    let name = named.unwrap_or(SERVICE_USER);
    let identity = crate::host::Identity::lookup(name, None, None).map_err(|error| {
        anyhow::anyhow!(
            "{error:#}: a daemon that runs as root starts the edge and the applications as a \
             service user, never as root. Create the account (useradd --system \
             --no-create-home {name}, or a sysusers.d entry), name another with --user or the \
             [daemon] table's user, or run the daemon as that user instead"
        )
    })?;
    anyhow::ensure!(
        !identity.is_root(),
        "the service user {name} is root: name an unprivileged account with --user or the \
         [daemon] table's user"
    );
    tracing::info!(
        user = name,
        "the edge and applications run as the service user"
    );
    Ok(Some(identity))
}

/// A level's limits on a request (section 4.6) as the snapshot carries
/// them, or none when the level says nothing.
fn request_limits_message(limits: &muru_config::RequestLimitSettings) -> Option<pb::RequestLimits> {
    muru_edge::RequestLimits {
        max_body_size: limits.max_body_size,
        queue_timeout: limits.queue_timeout,
        request_deadline: limits.request_deadline,
        body_idle_timeout: limits.body_idle_timeout,
        response_idle_timeout: limits.response_idle_timeout,
    }
    .to_message()
}

/// Section 4.11: a route's upload settings for the edge, with their
/// defaults in place. Apply has checked the keys, so a route that fails
/// them here takes no uploads.
fn upload_message(route: &muru_config::Route) -> Option<pb::Upload> {
    let settings = muru_config::check_upload(route).ok().flatten()?;

    Some(pb::Upload {
        mode: settings.mode,
        max_file_size: settings.max_file_size.unwrap_or(0),
        max_files: settings.max_files,
        hash: settings.hash,
        precheck: settings.precheck,
        spool_quota: settings.spool_quota,
    })
}

/// The upload spool, open to the edge's user alone: the service user
/// under a root daemon. The edge makes each application's directory in
/// it.
fn prepare_spool(directory: &Path, service: Option<crate::host::Identity>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(directory)?;

    if let Some(service) = service {
        std::os::unix::fs::chown(directory, Some(service.uid), Some(service.gid))?;
    }

    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

/// Give a release's socket directory to the application's user with the
/// service user's group, set-group-id so that the sockets bound in it
/// carry that group, and open to the owner and the group alone.
fn share_sockets(
    directory: &Path,
    owner: crate::host::Identity,
    service: crate::host::Identity,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::os::unix::fs::chown(directory, Some(owner.uid), Some(service.gid))?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o2750))
}

fn write_edge_record(run_dir: &Path, edge: &EdgeProcess) {
    write_record(
        &run_dir.join("edge.toml"),
        &EdgeRecord {
            pid: edge.pid().unwrap_or(0),
            control: edge.control().to_path_buf(),
            listener_socket: edge.listener_socket().to_path_buf(),
            generation: edge.generation(),
            listeners: edge
                .listeners()
                .iter()
                .map(|(address, tls)| ListenerRecord {
                    address: *address,
                    tls: *tls,
                })
                .collect(),
        },
    );
}

/// The executable to start this program again from: what is running,
/// or the installed binary when an upgrade has replaced that file.
pub(crate) fn running_executable() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe().context("finding the running executable")?;

    if current.is_file() {
        return Ok(current);
    }

    installed_executable()
}

/// Section 13.4: the binary an upgrade re-executes from. The daemon was
/// started as `muructl` by name or by path, so the installed binary is
/// the one that name finds on the PATH or that path names now; what the
/// kernel reports as the running executable may be a replaced file.
pub fn installed_executable() -> anyhow::Result<PathBuf> {
    let name = muru_common::title::args_os()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("the daemon has no command name"))?;
    let name = PathBuf::from(name);

    let found = if name.components().count() > 1 {
        name.is_file().then_some(name)
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
    };

    match found {
        Some(path) => Ok(path),
        None => std::env::current_exe().context("finding the installed executable"),
    }
}

/// Check the installed binary runs, and learn its version, before the
/// daemon commits to re-executing from it.
async fn prepare_upgrade() -> Result<Upgrade, String> {
    let executable = installed_executable().map_err(|error| format!("{error:#}"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(&executable)
            .arg("--version")
            .output(),
    )
    .await
    .map_err(|_| format!("{} did not report its version", executable.display()))?
    .map_err(|error| format!("{} cannot be run: {error}", executable.display()))?;

    if !output.status.success() {
        return Err(format!(
            "{} failed to report its version: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned();

    if version.is_empty() {
        return Err(format!("{} reported no version", executable.display()));
    }

    Ok(Upgrade {
        executable,
        version,
    })
}

/// Replace this process's image with the installed binary, keeping the
/// pid, the children and the environment. Returns only on failure.
fn exec_daemon(executable: &Path, arguments: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;

    std::process::Command::new(executable)
        .args(arguments)
        .exec()
}

/// The subject of an event about a change to the access log: its
/// application, or the logs when it covers every one.
fn access_subject(change: &muru_edge::access::Override) -> &str {
    if change.app.is_empty() {
        "logs"
    } else {
        &change.app
    }
}

/// The [proxy] table as the snapshot carries it, with the defaults of
/// section 4.7 filled in: the four common header types, recursive
/// resolution, and the configured headers kept.
pub fn proxy_message(proxy: &muru_config::Proxy) -> pb::Proxy {
    let headers = if proxy.headers.is_empty() {
        muru_config::DEFAULT_PROXY_HEADERS
            .iter()
            .map(|header| (*header).to_owned())
            .collect()
    } else {
        proxy.headers.clone()
    };

    pb::Proxy {
        trusted: proxy.trusted.clone(),
        headers,
        recursive: proxy.recursive.unwrap_or(true),
        consume: proxy.forwarded_headers == Some(muru_config::ForwardedHeaders::Consume),
    }
}

/// The node-wide error pages and override as the snapshot carries them
/// (section 4.12), checked; a page file must be an absolute path.
pub(crate) fn error_tables_message(
    pages: &std::collections::BTreeMap<String, muru_config::ErrorPage>,
    override_: Option<&muru_config::ErrorOverride>,
) -> Result<(Vec<pb::ErrorPage>, Option<pb::ErrorOverride>), String> {
    muru_config::check_error_pages(pages, false).map_err(|error| format!("[{error}]"))?;

    if let Some(table) = override_ {
        muru_config::check_error_override(table).map_err(|error| format!("[{error}]"))?;
    }

    Ok((
        error_pages_message(pages, None),
        override_.map(error_override_message),
    ))
}

/// Error pages as the snapshot carries them, a relative file resolved
/// against `base`.
pub fn error_pages_message(
    pages: &std::collections::BTreeMap<String, muru_config::ErrorPage>,
    base: Option<&Path>,
) -> Vec<pb::ErrorPage> {
    pages
        .iter()
        .map(|(key, page)| pb::ErrorPage {
            key: key.clone(),
            file: page
                .file
                .as_ref()
                .map(|file| match base {
                    Some(base) => base.join(file).display().to_string(),
                    None => file.clone(),
                })
                .unwrap_or_default(),
            template: page.template.clone().unwrap_or_default(),
            path: page.path.clone().unwrap_or_default(),
            content_type: page.content_type.clone().unwrap_or_default(),
        })
        .collect()
}

/// An error override as the snapshot carries it.
pub fn error_override_message(table: &muru_config::ErrorOverride) -> pb::ErrorOverride {
    pb::ErrorOverride {
        statuses: table
            .statuses
            .iter()
            .map(|status| u32::from(*status))
            .collect(),
        when: table.when.clone().unwrap_or_default(),
    }
}

/// A route's authentication sub-request as the snapshot carries it
/// (section 4.16). The table was checked when it was applied.
pub fn auth_message(table: &muru_config::Auth) -> pb::Auth {
    let millis = |text: Option<&str>| {
        text.and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    };

    pb::Auth {
        app: table.app.clone(),
        upstream: table.upstream.clone(),
        path: table.path.clone(),
        timeout_ms: millis(table.timeout.as_deref()),
        copy_headers: table.copy_headers.clone(),
        cache_ttl_ms: millis(table.cache.as_ref().map(|cache| cache.ttl.as_str())),
        cache_key: table
            .cache
            .as_ref()
            .map(|cache| cache.key.clone())
            .unwrap_or_default(),
    }
}

/// Maintenance mode as the snapshot carries it: whether it is on, and the
/// release's exemptions.
fn maintenance_message(on: bool, table: Option<&muru_config::Maintenance>) -> pb::Maintenance {
    let table = table.cloned().unwrap_or_default();
    pb::Maintenance {
        on,
        exempt_paths: table.exempt_paths,
        exempt_addresses: table.exempt_addresses,
        bypass_cookie: table.bypass_cookie.unwrap_or_default(),
        retry_after_s: table
            .retry_after
            .as_deref()
            .and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| duration.as_secs()),
    }
}

/// The [compression] table as the snapshot carries it, checked: the
/// defaults of section 4.17 where the table is silent.
pub(crate) fn compression_message(
    table: Option<&muru_config::Compression>,
) -> Result<pb::Compression, String> {
    let Some(table) = table else {
        return Ok(muru_edge::compression::Compression::default().to_message());
    };

    // The snapshot's zero and empty lists stand for the defaults, so a
    // table that writes one is refused rather than given the default.

    let min_size = match &table.min_size {
        Some(text) => {
            match muru_config::parse_size(text).map_err(|error| format!("min_size: {error}"))? {
                0 => return Err("min_size must be at least 1 byte".to_owned()),
                size => size,
            }
        }
        None => 0,
    };

    if table.encodings.as_ref().is_some_and(Vec::is_empty) {
        return Err(
            "encodings names no encoding; enabled = false turns compression off".to_owned(),
        );
    }

    if table.types.as_ref().is_some_and(Vec::is_empty) {
        return Err(
            "types names no content type; enabled = false turns compression off".to_owned(),
        );
    }

    let message = pb::Compression {
        enabled: table.enabled.unwrap_or(true),
        encodings: table.encodings.clone().unwrap_or_default(),
        min_size,
        types: table.types.clone().unwrap_or_default(),
        level: table.level.clone().unwrap_or_default(),
    };

    // Checked the way the edge will read it, so a bad word is refused
    // here rather than in the edge's log.

    muru_edge::compression::Compression::from_snapshot(&message).map(|policy| policy.to_message())
}

/// A virtual host's or a route's own `compression` table as the snapshot
/// carries it, checked as the `[compression]` table is: the keys given,
/// and nothing for the rest, which the level beneath supplies.
pub(crate) fn compression_choice(
    table: &muru_config::Compression,
) -> Result<pb::CompressionChoice, String> {
    let min_size = match &table.min_size {
        Some(text) => {
            match muru_config::parse_size(text).map_err(|error| format!("min_size: {error}"))? {
                0 => return Err("min_size must be at least 1 byte".to_owned()),
                size => size,
            }
        }
        None => 0,
    };

    if table.encodings.as_ref().is_some_and(Vec::is_empty) {
        return Err(
            "encodings names no encoding; enabled = false turns compression off".to_owned(),
        );
    }

    if table.types.as_ref().is_some_and(Vec::is_empty) {
        return Err(
            "types names no content type; enabled = false turns compression off".to_owned(),
        );
    }

    let choice = pb::CompressionChoice {
        enabled: table.enabled,
        encodings: table.encodings.clone().unwrap_or_default(),
        min_size,
        types: table.types.clone().unwrap_or_default(),
        level: table.level.clone().unwrap_or_default(),
    };

    muru_edge::compression::Compression::default().with(&choice)?;
    Ok(choice)
}

/// A `request_headers` table (section 4.7) as the snapshot carries it.
fn request_headers_message(table: &muru_config::RequestHeaders) -> pb::HeaderRules {
    pb::HeaderRules {
        remove: table.remove.clone(),
        set: header_fields(&table.set),
        add: Vec::new(),
    }
}

/// A `response_headers` table (section 4.7) as the snapshot carries it,
/// a header added more than once with a field for each value.
fn response_headers_message(table: &muru_config::ResponseHeaders) -> pb::HeaderRules {
    pb::HeaderRules {
        remove: table.remove.clone(),
        set: header_fields(&table.set),
        add: table
            .add
            .iter()
            .flat_map(|(name, values)| {
                values.values().iter().map(|value| pb::HeaderField {
                    name: name.clone(),
                    value: value.clone(),
                })
            })
            .collect(),
    }
}

fn header_fields(headers: &BTreeMap<String, String>) -> Vec<pb::HeaderField> {
    headers
        .iter()
        .map(|(name, value)| pb::HeaderField {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

/// A host's certificate chain and key as PEM text, for the snapshot.
fn read_pem_pair(certificate: &str, key: &str) -> anyhow::Result<(String, String)> {
    let chain =
        std::fs::read_to_string(certificate).with_context(|| format!("reading {certificate}"))?;
    let key = std::fs::read_to_string(key).with_context(|| format!("reading {key}"))?;

    Ok((chain, key))
}

/// Read a record a previous daemon wrote, if there is one it can decode.
fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Write a record for the next daemon, whole, so a daemon that reads it
/// as it is rewritten never takes it for no record.
fn write_record<T: serde::Serialize>(path: &Path, record: &T) {
    let written = toml::to_string(record)
        .map_err(std::io::Error::other)
        .and_then(|text| muru_common::files::write_whole(path, text));

    if let Err(error) = written {
        tracing::error!(%error, path = %path.display(), "writing a record failed");
    }
}

/// What a managed server's probes of its instances started again report
/// to (issue 917): a command to the loop, which takes the outcome.
fn probed(commands: &mpsc::Sender<Command>, app: &str, id: &str) -> crate::servers::Probed {
    let commands = commands.clone();
    let (app, id) = (app.to_owned(), id.to_owned());

    Arc::new(move |index, pid, answered| {
        let commands = commands.clone();
        let command = Command::ServerProbed {
            app: app.clone(),
            id: id.clone(),
            index,
            pid,
            answered,
        };

        tokio::spawn(async move {
            let _ = commands.send(command).await;
        });
    })
}

/// What the event says when a managed server's instances are taken
/// over: the pids of those that were still there, how each that ended
/// while no daemon ran did and when it is started again (issue 916), and
/// which were gone and are started again by the next check.
fn adoption_message(adopted: &[(u32, crate::supervisor::Adopted)]) -> String {
    use crate::supervisor::Adopted;

    let list = |words: Vec<String>| match words.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [most @ .., last] => format!("{} and {last}", most.join(", ")),
    };
    let mut pids = Vec::new();
    let mut ended = Vec::new();
    let mut gone = Vec::new();

    for (index, found) in adopted {
        match found {
            Adopted::Kept(pid) => pids.push(pid.to_string()),
            Adopted::Ended { exit, wait } => ended.push(format!(
                "instance {index} {exit} while no daemon ran and is started again in {}",
                muru_config::format_duration(*wait)
            )),
            Adopted::Gone => gone.push(index.to_string()),
        }
    }

    let mut message = if pids.is_empty() {
        "none of the server instances was still there".to_owned()
    } else {
        format!("re-adopted the server instances (pid {})", pids.join(", "))
    };

    for clause in ended {
        message.push_str("; ");
        message.push_str(&clause);
    }

    match gone.len() {
        0 => {}
        1 => message.push_str(&format!(
            "; instance {} was gone and is started again",
            list(gone)
        )),
        _ => message.push_str(&format!(
            "; instances {} were gone and are started again",
            list(gone)
        )),
    }

    message
}

/// Whether a process's command line names the path or is the title, which
/// is how a host's or relay's record is checked against the process that
/// has its pid now.
fn command_mentions(pid: u32, path: &Path, title: &str) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output();

    match output {
        Ok(output) => names(&String::from_utf8_lossy(&output.stdout), path, title),
        Err(_) => false,
    }
}

/// Whether a command line, as `ps` shows it, is the process a record
/// names: it mentions `path`, as the process was started, or it is
/// `title`, which the process set in place of its arguments (section
/// 2.1), with whatever spaces `ps` shows for the rest of the area, and
/// the instance's tag in parentheses after it (section 13.10).
fn names(command: &str, path: &Path, title: &str) -> bool {
    let command = command.trim();

    command.contains(&path.display().to_string())
        || command == title
        || command
            .strip_prefix(title)
            .and_then(|rest| rest.strip_prefix(" ("))
            .is_some_and(|tag| tag.ends_with(')') && !tag.contains(' '))
}

impl Daemon {
    /// Everything the dashboard shows (section 10.7): the node from the
    /// daemon's own facts, the releases from the store and the hosts,
    /// the requests and pools from the edge, and each serving release's
    /// log tail from its file.
    async fn dashboard_data(&mut self) -> muru_dashboard::NodeData {
        use muru_dashboard::{
            AppRow, EventRow, HostRow, LogPanel, NodeData, NodeRequestRow, ReleaseRow, RolloutRow,
            RouteRow, ScalingRow, Summary,
        };
        use muru_edge::access::{format_bytes, format_duration};

        let recent = match self.edge.recent(200).await {
            Ok(recent) => recent,
            Err(error) => {
                tracing::debug!(%error, "the edge's recent requests could not be read");
                pb::RecentRequests::default()
            }
        };

        let websockets = match self.edge.websockets("", DASHBOARD_SOCKETS).await {
            Ok(report) => report,
            Err(error) => {
                tracing::debug!(%error, "the edge's WebSockets could not be read");
                pb::WebSockets::default()
            }
        };

        // A WebSocket's closing record carries how long it was open, which
        // is not a request's time.

        let mut totals: Vec<u64> = recent
            .records
            .iter()
            .filter(|record| record.websocket_close.is_empty())
            .map(|record| record.total_us)
            .collect();
        totals.sort_unstable();
        let percentile = |fraction: f64| -> String {
            if totals.is_empty() {
                return "-".to_owned();
            }

            let index = ((totals.len() - 1) as f64 * fraction).round() as usize;
            format_duration(Duration::from_micros(totals[index]))
        };
        let summary = Summary {
            rate: format!(
                "{:.1}",
                recent.minute_requests as f64 / recent.minute_seconds.max(1) as f64
            ),
            p50: percentile(0.5),
            p95: percentile(0.95),
            errors: recent.minute_errors,
            in_flight: recent
                .pools
                .iter()
                .map(|pool| pool.busy.saturating_sub(pool.websockets) as usize)
                .sum(),
            websockets: websockets.open as usize,
        };

        let mut apps = Vec::new();
        let mut maintenance: HashMap<String, (u64, String)> = self
            .store
            .maintenance()
            .unwrap_or_default()
            .into_iter()
            .map(|(app, since)| (app, (since, String::new())))
            .collect();

        // Issue 757: an active overlay's maintenance setting stands over
        // the application's own, and dates from the overlay's activation.

        let overlays = self.store.overlays().unwrap_or_default();
        let states = self.store.overlay_states().unwrap_or_default();
        let settings =
            crate::overlays::maintenance_settings(&crate::overlays::ordered(&overlays, &states));

        for (app, (on, by)) in settings {
            if !on {
                maintenance.remove(&app);
                continue;
            }

            let since = states
                .iter()
                .find(|state| state.name == by)
                .map_or(0, |state| state.since_unix_ms);
            maintenance.insert(app, (since, by));
        }

        // Issue 815: where each application can be reached, as muructl
        // open --list gives it.

        let routes = self.store.routes().unwrap_or_default();
        let hosts = self.store.hosts().unwrap_or_default();
        let listeners = self
            .info
            .listeners
            .read()
            .expect("listeners poisoned")
            .clone();

        for app in self.store.apps().unwrap_or_default() {
            let mut releases = Vec::new();

            for release in self.store.releases(Some(&app.name)).unwrap_or_default() {
                let workers = self
                    .running
                    .get(&(app.name.clone(), release.id.clone()))
                    .map_or(0, Running::workers);
                let pools: Vec<&pb::PoolState> = recent
                    .pools
                    .iter()
                    .filter(|pool| pool.app == app.name && pool.release == release.id)
                    .collect();
                releases.push(ReleaseRow {
                    id: release.id.clone(),
                    state: release.state.as_str().to_owned(),
                    serving: app.current.as_deref() == Some(release.id.as_str()),
                    message: release.message.clone(),
                    workers,
                    slots_busy: pools.iter().map(|pool| pool.busy as usize).sum(),
                    websockets: pools.iter().map(|pool| pool.websockets as usize).sum(),
                    slots_total: pools.iter().map(|pool| pool.slots as usize).sum(),
                    threads: muru_dashboard::SlotSide::of(
                        pools.iter().map(|pool| pool.threads_busy as usize).sum(),
                        pools.iter().map(|pool| pool.threads as usize).sum(),
                    ),
                    event_loop: muru_dashboard::SlotSide::of(
                        pools.iter().map(|pool| pool.loop_busy as usize).sum(),
                        pools.iter().map(|pool| pool.loop_slots as usize).sum(),
                    ),
                    waiting: pools.iter().map(|pool| pool.waiting as usize).sum(),
                    created: full_time(release.created_unix_ms),
                    detour: false,
                    commit: muru_config::commit_label(&release.spec.metadata).unwrap_or_default(),
                    git_ref: release
                        .spec
                        .metadata
                        .get("ref")
                        .map(|name| muru_config::short_ref(name).to_owned())
                        .unwrap_or_default(),
                    url: release
                        .spec
                        .metadata
                        .get("url")
                        .cloned()
                        .unwrap_or_default(),
                    description: release
                        .spec
                        .metadata
                        .get("description")
                        .cloned()
                        .unwrap_or_default(),
                });
            }

            releases.reverse();

            // Section 9.13: the application's detours, newest of all.

            for detour in self
                .detours
                .values()
                .filter(|detour| detour.app() == app.name)
            {
                releases.insert(
                    0,
                    ReleaseRow {
                        id: detour.release.clone(),
                        state: ReleaseState::Ready.as_str().to_owned(),
                        message: format!(
                            "{}, traffic {}",
                            detour.describe(),
                            crate::detours::listed_traffic(
                                &detour.opening.traffic.word(),
                                detour.until_unix_ms,
                                &detour.opening.paths
                            )
                        ),
                        created: full_time(
                            u64::try_from(detour.opened_unix_ms).unwrap_or_default(),
                        ),
                        detour: true,
                        ..ReleaseRow::default()
                    },
                );
            }

            apps.push(AppRow {
                name: app.name.clone(),
                current: app.current.clone().unwrap_or_default(),
                previous: app.previous.clone().unwrap_or_default(),
                maintenance_since: maintenance
                    .get(&app.name)
                    .map(|(since, _)| full_time(*since))
                    .unwrap_or_default(),
                maintenance_overlay: maintenance
                    .get(&app.name)
                    .map(|(_, by)| by.clone())
                    .unwrap_or_default(),
                releases,
                urls: crate::urls::for_app(&app.name, &routes, &hosts, &listeners, None)
                    .into_iter()
                    .map(|candidate| candidate.url)
                    .collect(),
            });
        }

        // Sections 7.3 and 7.4: each running release's groups as their
        // autoscalers see them.

        let mut scaling = Vec::new();
        let mut running: Vec<&(String, String)> = self.running.keys().collect();
        running.sort();

        for (app, id) in running {
            let measured = |value: Option<f64>, unit: &str| {
                value.map_or("-".to_owned(), |value| format!("{value:.0}{unit}"))
            };
            let p95 = |value: Option<f64>| {
                value.map_or("-".to_owned(), |ms| {
                    crate::scaling::describe_p95(ms, crate::scaling::SIGNAL_LARGEST_MS)
                })
            };

            let optional: Vec<String> = self
                .running
                .get(&(app.clone(), id.clone()))
                .map(|running| {
                    running
                        .groups
                        .iter()
                        .filter(|group| group.optional)
                        .map(|group| group.name.clone())
                        .collect()
                })
                .unwrap_or_default();

            for (group, status) in self.scale_status(app, id).unwrap_or_default() {
                scaling.push(ScalingRow {
                    release: format!("{app}@{id}"),
                    app: app.clone(),
                    id: id.clone(),
                    pinned: status.pinned.is_some(),
                    optional: optional.contains(&group),
                    group,
                    workers: if status.desired == status.ready {
                        status.ready.to_string()
                    } else {
                        format!("{} -> {}", status.ready, status.desired)
                    },
                    policy: status.spec.describe(),
                    cpu: status
                        .workers
                        .worker_cpu
                        .map_or("-".to_owned(), crate::scaling::describe_cpu),
                    gc: measured(status.workers.gc_share.map(|share| share * 100.0), "%"),
                    gil_wait: p95(status.workers.gil_wait_p95_ms),
                    loop_lag: p95(status.workers.loop_lag_p95_ms),
                    handoff_wait: status
                        .workers
                        .handoff_queue_wait_p95_ms
                        .map(|wait| p95(Some(wait))),
                    diagnosis: status.diagnosis.summary.to_owned(),
                    advice: status.diagnosis.advice.to_owned(),
                    last_decision: status
                        .last
                        .map(|(age, what)| {
                            format!(
                                "{what} ({} ago)",
                                muru_config::format_duration(Duration::from_secs(age.as_secs()))
                            )
                        })
                        .unwrap_or_default(),
                });
            }
        }

        // Section 9.3: the rollouts under way, then the latest that ended.

        let mut rollouts = self.store.rollouts(None).unwrap_or_default();
        rollouts.sort_by_key(|rollout| {
            (
                !rollout.state.is_active(),
                std::cmp::Reverse(rollout.updated_unix_ms),
            )
        });
        let rollouts = rollouts
            .into_iter()
            .take(10)
            .map(|rollout| RolloutRow {
                percent: rollout.percent(),
                app: rollout.app,
                id: rollout.id,
                candidate: rollout.candidate,
                baseline: rollout.baseline,
                state: rollout.state.as_str().to_owned(),
                message: rollout.message,
                updated: full_time(rollout.updated_unix_ms),
            })
            .collect();

        let hosts = self
            .store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, host)| HostRow {
                maintenance_since: self
                    .host_maintenance
                    .get(&name)
                    .map(|since| full_time(*since))
                    .unwrap_or_default(),
                name,
                names: host.names,
                listeners: host.listeners,
                tls: host.tls.is_some(),
                default: host.default,
            })
            .collect();
        let routes = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .map(|route| RouteRow {
                host: route.host,
                path: route.path,
                app: route.app,
            })
            .collect();
        let requests = recent
            .records
            .iter()
            .rev()
            .map(|record| NodeRequestRow {
                time: full_time(record.started_unix_ms),
                status: record.status as u16,
                status_class: (record.status / 100) as u16,
                reason: if record.websocket_close.is_empty() {
                    record.reason.clone()
                } else {
                    format!("websocket {}", record.websocket_close)
                },
                method: record.method.clone(),
                target: record.target.clone(),
                release: if record.app.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{}@{}", record.app, record.release)
                },
                host: record.host.clone(),
                bytes: format_bytes(record.bytes),
                total: format_duration(Duration::from_micros(record.total_us)),
                queue: format_duration(Duration::from_micros(record.queue_us)),
                application: format_duration(Duration::from_micros(record.application_us)),
                worker: if record.worker < 0 {
                    "-".to_owned()
                } else {
                    record.worker.to_string()
                },
                request_id: record.request_id.clone(),
            })
            .collect();
        let latest = self.store.last_event_sequence().unwrap_or(0);
        let mut events: Vec<EventRow> = self
            .store
            .events_after(latest.saturating_sub(50), 50)
            .unwrap_or_default()
            .into_iter()
            .map(|event| EventRow {
                sequence: event.sequence,
                time: full_time(event.time_unix_ms),
                level: event.level.to_owned(),
                kind: event.kind,
                subject: event.subject,
                message: event.message,
            })
            .collect();
        events.reverse();
        let mut logs = Vec::new();

        for app in &apps {
            if app.current.is_empty() {
                continue;
            }

            let path = self.locations.release_log(&app.name, &app.current);
            logs.push(LogPanel {
                app: app.name.clone(),
                release: app.current.clone(),
                lines: crate::logs::last_lines(&path, 40)
                    .await
                    .iter()
                    .map(|line| muru_dashboard::LogLine::parse(line))
                    .collect(),
            });
        }

        NodeData {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            uptime: muru_dashboard::uptime(Duration::from_millis(
                crate::model::now_unix_ms().saturating_sub(self.info.started_unix_ms),
            )),
            listeners: self
                .info
                .listeners
                .read()
                .expect("listeners poisoned")
                .iter()
                .map(|(address, tls)| {
                    format!("{}://{address}", if *tls { "https" } else { "http" })
                })
                .collect(),
            edge_pid: self
                .info
                .edge_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            snapshot_version: self.edge.snapshot_version(),
            metrics: self
                .info
                .metrics
                .map(|address| format!("http://{address}/metrics")),
            summary,
            apps,
            hosts,
            routes,
            scaling,
            rollouts,
            requests,
            websockets: crate::dashboard::websockets_view(websockets),
            events,
            logs,
        }
    }

    /// Section 13.3: read the configuration file again and apply what
    /// changed, each table in its own way: the edge's tables go to it in
    /// a snapshot, changed listeners replace the edge, sinks are started
    /// and stopped, and the rest are swapped in place. The file is
    /// checked whole and the new listeners bound before anything is
    /// applied, so a mistake applies nothing.
    async fn reload(&mut self) -> Result<String, String> {
        let path = self.locations.config.path.clone();
        let settings = crate::settings::NodeSettings::read(
            &path,
            &self.locations.state.path.join("secrets"),
            &self.overrides,
        )?;
        let clients = settings.client_rules.policies(&self.store)?;
        let listeners = self.bind_listeners(&settings.listeners)?;
        let mut report = crate::reload::Report::new(&path);

        // Issue 894: what is read only at start and now differs from what
        // this daemon started with, which a restart applies.

        for ((setting, started), (_, now)) in self.read_at_start.iter().zip(settings.at_start()) {
            if *started != now {
                report.stale(setting);
            }
        }

        // Section 4.4: the tables the edge takes from each snapshot, and
        // the limits with them (section 4.6).

        let mut snapshot = false;

        for (table, changed) in [
            ("[proxy]", settings.proxy != self.proxy),
            ("[compression]", settings.compression != self.compression),
            ("[error_pages]", settings.error_pages != self.error_pages),
            (
                "[error_override]",
                settings.error_override != self.error_override,
            ),
            (
                "[[client_class]]",
                clients.classes_message != self.clients.classes_message,
            ),
            (
                "[rate_limit]",
                clients.limits_message != self.clients.limits_message,
            ),
            ("[limits]", settings.limits != self.limits),
            ("[[listener]] h2c", settings.h2c != self.h2c),
            (
                "[[listener]] limits",
                settings.listener_limits != self.listener_limits,
            ),
            (
                "[[listener]] proxy",
                settings.listener_proxies != self.listener_proxies,
            ),
            ("[logs] access_log", settings.access_log != self.access_log),
        ] {
            if changed {
                report.changed(table, "the edge has it");
                snapshot = true;
            } else {
                report.unchanged(table);
            }
        }

        self.proxy = settings.proxy;
        self.compression = settings.compression;
        self.error_pages = settings.error_pages;
        self.error_override = settings.error_override;
        self.limits = settings.limits;
        *self.info.limits.write().expect("limits poisoned") = self.limits.clone();
        self.access_log = settings.access_log;
        self.h2c = settings.h2c;
        self.listener_limits = settings.listener_limits;
        self.listener_proxies = settings.listener_proxies;
        self.client_rules = settings.client_rules;
        self.clients = clients;

        if self.file_conditions != settings.conditions {
            self.file_conditions = settings.conditions;
            self.load_conditions();
            report.changed("[condition]", "at once");
        } else {
            report.unchanged("[condition]");
        }

        // Section 7.4: the budget.

        if settings.budget != self.budget_table {
            self.budget_table = settings.budget;
            report.changed(
                "[daemon.budget]",
                &self.budget().describe(&self.budget_usage()),
            );
        } else {
            report.unchanged("[daemon.budget]");
        }

        let mut defined = self.client_rules.defined();
        defined.budget = self.budget_view(self.budget_usage());
        defined.conditions = self.file_conditions.clone();
        *self.info.defined.write().expect("defined names poisoned") = defined;
        let pass_env_was = std::mem::replace(
            &mut *self.info.pass_env.write().expect("pass_env poisoned"),
            settings.pass_env.clone(),
        );

        if pass_env_was != settings.pass_env {
            report.changed("[daemon] pass_env", "for releases started from now on");
        } else {
            report.unchanged("[daemon] pass_env");
        }

        if self.keep_command_runs != settings.keep_command_runs {
            self.keep_command_runs = settings.keep_command_runs;
            self.runs.set_keep(settings.keep_command_runs);
            report.changed("[daemon] keep_command_runs", "as each run ends");
        } else {
            report.unchanged("[daemon] keep_command_runs");
        }

        // Section 11.5: sinks start, stop and restart on their cursors.

        let sink_changes = self.sinks.reconcile(settings.sinks);

        if sink_changes.is_empty() {
            report.unchanged("[[events.sink]]");
        } else {
            report.changed("[[events.sink]]", &sink_changes.describe());
        }

        // Section 10.8: the stores matter when a release starts, so they
        // are swapped whole; section 11.3: the rotator takes the new
        // policy and keeps what it has seen of each file.

        if settings.secrets != self.secrets {
            self.secrets = settings.secrets;
            let names: Vec<&str> = self
                .secrets
                .iter()
                .map(|store| store.name.as_str())
                .collect();
            report.changed(
                "[secrets.store.NAME]",
                &if names.is_empty() {
                    "no stores".to_owned()
                } else {
                    format!("the stores are {}", names.join(", "))
                },
            );
        } else {
            report.unchanged("[secrets.store.NAME]");
        }

        let rotation = self
            .rotation
            .as_ref()
            .map(|rotator| rotator.policy().clone());

        if settings.rotation != rotation {
            match (settings.rotation, self.rotation.as_mut()) {
                (Some(policy), Some(rotator)) => rotator.set_policy(policy),
                (Some(policy), None) => {
                    self.rotation = Some(restored_rotator(policy, &self.store));
                }
                (None, _) => self.rotation = None,
            }

            report.changed(
                "[logs] rotation",
                if self.rotation.is_some() {
                    "the daemon rotates on the new policy"
                } else {
                    "rotation is external"
                },
            );
        } else {
            report.unchanged("[logs] rotation");
        }

        // Section 11.4: the Python logging defaults are installed before
        // an application is imported, so a running host keeps those it
        // started with.

        let python_logging = self
            .info
            .python_logging
            .read()
            .expect("python logging poisoned")
            .clone();

        if settings.python_logging != python_logging {
            *self
                .info
                .python_logging
                .write()
                .expect("python logging poisoned") = settings.python_logging.clone();
            report.changed(
                "[logging.python]",
                "each application takes the new defaults at its next restart or deploy",
            );
        } else {
            report.unchanged("[logging.python]");
        }

        if settings.reopen_on_rename != self.reopen_on_rename {
            self.reopen_on_rename = settings.reopen_on_rename;
            self.rename_due = Instant::now();
            self.renames.forget();

            report.changed(
                "[logs] reopen_on_rename",
                if self.reopen_on_rename {
                    "a log file renamed away is reopened"
                } else {
                    "a log file renamed away waits for muructl logs reopen"
                },
            );
        } else {
            report.unchanged("[logs] reopen_on_rename");
        }

        if settings.relay != self.relay_settings {
            self.relay_settings = settings.relay;
            report.changed(
                "[logs] on_overflow and output_queue",
                "each application takes the new values at its next restart or deploy",
            );
        } else {
            report.unchanged("[logs] on_overflow and output_queue");
        }

        // Section 13.3: what the next stop signal means.

        if settings.shutdown != self.shutdown {
            self.shutdown = settings.shutdown;
            report.changed("[shutdown]", &self.shutdown.describe());
        } else {
            report.unchanged("[shutdown]");
        }

        // Section 4.2: the hosts' certificate files, read again.

        let certificates = self.refresh_certificates(true);

        if certificates.renewed.is_empty() {
            report.unchanged("the hosts' certificates");
        } else {
            report.changed(
                "the hosts' certificates",
                &format!(
                    "{} renewed; the edge has them",
                    certificates.renewed.join(", ")
                ),
            );
            snapshot = true;
        }

        for (host, why) in certificates.kept {
            report.kept(&format!("host {host} keeps its certificate"), &why);
        }

        if snapshot && let Err(error) = self.push_snapshot().await {
            report.failed("the edge's tables", &error);
        }

        // Section 13.4: changed listeners take a new edge, which gets the
        // snapshot just pushed before the old one is told to stop.

        match listeners {
            Some((listeners, what)) => match self.replace_edge(Some(listeners), &what).await {
                Ok(_) => report.changed("[[listener]]", &format!("{what}; the edge was replaced")),
                Err(error) => report.failed("[[listener]]", &error),
            },
            None => report.unchanged("[[listener]]"),
        }

        self.events.emit(
            "config.reloaded",
            "daemon",
            if report.is_ok() { "info" } else { "warning" },
            &report.message(),
        );
        report.outcome()
    }

    /// Section 4.2: read the certificate and key files of the applied
    /// TLS hosts again, every host or only those not read yet, and keep
    /// each pair that is whole and goes together, for the snapshots. A
    /// host whose files cannot be read, or do not go together, keeps its
    /// last good pair, named in a `certificate.unreadable` event; a host
    /// that never had one serves without TLS until the files are right.
    fn refresh_certificates(&mut self, every: bool) -> CertificateChanges {
        let mut changes = CertificateChanges::default();
        let hosts = self.store.hosts().unwrap_or_default();
        self.certificates.retain(|name, _| {
            hosts
                .iter()
                .any(|(host, spec)| host == name && spec.tls.is_some())
        });

        for (name, host) in hosts {
            let Some(tls) = &host.tls else {
                continue;
            };

            if !every && self.certificates.contains_key(&name) {
                continue;
            }

            let read = read_pem_pair(&tls.certificate, &tls.key).and_then(|(chain, key)| {
                let (parsed_chain, parsed_key) = muru_edge::tls::parse_pem(&chain, &key)?;
                muru_edge::tls::certified_key(parsed_chain, parsed_key)?;
                Ok((chain, key))
            });

            match read {
                Ok(pair) => {
                    if self.certificates.get(&name) != Some(&pair)
                        && self.certificates.insert(name.clone(), pair).is_some()
                    {
                        changes.renewed.push(name);
                    }
                }
                Err(error) => {
                    let kept = self.certificates.contains_key(&name);
                    self.events.emit(
                        "certificate.unreadable",
                        &format!("host {name}"),
                        "error",
                        &format!(
                            "{error:#}; {}",
                            if kept {
                                "the host keeps the certificate it had"
                            } else {
                                "the host serves without TLS until the files are right"
                            }
                        ),
                    );
                    changes.kept.push((name, format!("{error:#}")));
                }
            }
        }

        changes
    }

    /// Section 13.3: the sockets for a changed set of listeners, bound
    /// before anything else is applied so that a port in use applies
    /// nothing. A kept listener's socket is the edge's own, so its
    /// backlog survives the replacement. None when the set is unchanged.
    fn bind_listeners(
        &self,
        wanted: &[(SocketAddr, bool)],
    ) -> Result<Option<(Vec<crate::edges::Listener>, String)>, String> {
        let current = self.edge.listeners();

        if wanted.len() == current.len() && wanted.iter().all(|listener| current.contains(listener))
        {
            return Ok(None);
        }

        let mut listeners = Vec::new();

        for &(address, tls) in wanted {
            let socket = match self.edge.socket_on(address) {
                Some(socket) => socket,
                None => std::net::TcpListener::bind(address).map_err(|error| {
                    format!(
                        "cannot listen on {address}: {}",
                        bind_refused(error, address, "listener")
                    )
                })?,
            };

            listeners.push(crate::edges::Listener { socket, tls });
        }

        let describe = |(address, tls): &(SocketAddr, bool)| {
            format!("{}://{address}", if *tls { "https" } else { "http" })
        };
        let added: Vec<String> = wanted
            .iter()
            .filter(|listener| !current.contains(listener))
            .map(describe)
            .collect();
        let removed: Vec<String> = current
            .iter()
            .filter(|listener| !wanted.contains(listener))
            .map(describe)
            .collect();
        let mut what = Vec::new();

        if !added.is_empty() {
            what.push(format!("{} added", added.join(", ")));
        }

        if !removed.is_empty() {
            what.push(format!("{} removed", removed.join(", ")));
        }

        Ok(Some((listeners, what.join(", "))))
    }

    /// How long an edge is given to finish its connections when it is
    /// replaced: the request deadline, so a response in flight can
    /// complete, and a little over. A virtual host or a route may set a
    /// deadline of its own (section 4.6), so it is the longest of them.
    fn drain_grace(&self) -> Duration {
        let hosts = self
            .store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, host)| host.timeouts);
        let routes = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route.timeouts);
        let longest = hosts
            .chain(routes)
            .filter_map(|timeouts| timeouts.request_deadline)
            .filter_map(|text| muru_config::parse_duration(&text).ok())
            .fold(self.limits.request_deadline, Duration::max);

        longest + Duration::from_secs(5)
    }

    /// Section 9.10: the routes and the applications in maintenance mode
    /// with the active overlays laid over them, lowest priority first.
    fn effective(&self) -> (Vec<muru_config::Route>, BTreeSet<String>) {
        let overlays = self.store.overlays().unwrap_or_default();
        let states = self.store.overlay_states().unwrap_or_default();
        let active = crate::overlays::ordered(&overlays, &states);
        let mut routes = self.store.routes().unwrap_or_default();
        crate::overlays::apply_routes(&mut routes, &active);

        // Section 9.13: the detours' overlays, over every declared one.

        let takings = self.detour_takings();

        for route in &mut routes {
            crate::detours::lay_over(route, &takings);
        }

        let base: BTreeSet<String> = self.maintenance.keys().cloned().collect();
        let maintained = crate::overlays::apply_maintenance(&base, &active);
        (routes, maintained)
    }

    /// Section 9.10: whether an overlay's preconditions hold, or why not.
    async fn overlay_preconditions(&self, overlay: &muru_config::Overlay) -> Result<(), String> {
        let Some(ready) = overlay
            .preconditions
            .as_ref()
            .and_then(|preconditions| preconditions.release_ready.as_deref())
        else {
            return Ok(());
        };

        let route_apps: std::collections::BTreeMap<String, String> = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route.name.map(|name| (name, route.app)))
            .collect();
        let (app, id) = crate::config::precondition_release(ready, overlay, &route_apps)
            .ok_or_else(|| format!("release_ready {ready:?} names no release"))?;

        // A retained release is warm and takes traffic at once, so it
        // counts as ready.

        match self.store.release(&app, &id) {
            Ok(release)
                if matches!(release.state, ReleaseState::Ready | ReleaseState::Retained) => {}
            Ok(release) => return Err(format!("{app}@{id} is {}, not ready", release.state)),
            Err(_) => return Err(format!("{app}@{id} does not exist")),
        }

        // Issue 689: the release answers its probe, asked through the
        // edge's own listener as a preview of it, at the host of the
        // first route the overlay changes.

        let Some(path) = overlay
            .preconditions
            .as_ref()
            .and_then(|preconditions| preconditions.probe.as_deref())
        else {
            return Ok(());
        };

        let Some(port) = self
            .edge
            .listeners()
            .into_iter()
            .find(|(_, tls)| !tls)
            .map(|(address, _)| address.port())
        else {
            return Err("the edge has no plain HTTP listener to probe through".to_owned());
        };

        let host = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .find(|route| {
                route
                    .name
                    .as_ref()
                    .is_some_and(|name| overlay.routes.contains_key(name))
            })
            .map(|route| route.host)
            .filter(|host| !host.is_empty() && !host.contains('*'));
        let url = format!("http://127.0.0.1:{port}{path}");

        crate::rollouts::probe_host(&self.probe_client, &url, host.as_deref(), &id)
            .await
            .map_err(|error| {
                crate::overlays::describe_probe_failure(
                    &format!("{app}@{id}"),
                    &url,
                    path,
                    host.as_deref(),
                    &error,
                )
            })
    }

    /// Section 9.10: begin and end overlays by their schedules and by the
    /// time they were activated for, start what their warm-ups name, and
    /// give the edge the result.
    async fn reconcile_overlays(&mut self) {
        let overlays = self.store.overlays().unwrap_or_default();
        let states: HashMap<String, crate::model::OverlayState> = self
            .store
            .overlay_states()
            .unwrap_or_default()
            .into_iter()
            .map(|state| (state.name.clone(), state))
            .collect();
        let now = now_unix_ms();
        let mut changed = false;

        // Issue 790: the overlays this pass ends, with the event kind and
        // the reason, for a warning should the edge refuse the result.

        let mut ended: Vec<(&'static str, String, String)> = Vec::new();

        for (name, overlay) in &overlays {
            let subject = format!("overlay {name}");

            match crate::overlays::step(overlay, states.get(name), now) {
                crate::overlays::Step::Stay => {}
                crate::overlays::Step::Release => {
                    let _ = self.store.set_overlay_state(name, None);
                }
                crate::overlays::Step::Extend { until } => {
                    if let Some(state) = states.get(name) {
                        let state = crate::model::OverlayState {
                            until_unix_ms: until,
                            ..state.clone()
                        };
                        let _ = self.store.set_overlay_state(name, Some(&state));
                    }
                }
                crate::overlays::Step::End { reason } => {
                    let _ = self.store.set_overlay_state(name, None);

                    if crate::overlays::is_temporary(name) {
                        let _ = self.store.delete_overlay(name);
                    }

                    let (kind, refused) = if crate::overlays::is_schedule(name) {
                        ("schedule.expired", "schedule.end_refused")
                    } else {
                        ("overlay.ended", "overlay.end_refused")
                    };
                    self.events
                        .emit(kind, &subject, "notice", &format!("ended: {reason}"));
                    ended.push((refused, subject, reason.to_string()));
                    changed = true;
                }
                crate::overlays::Step::Begin { until } => {
                    let preconditions = self.overlay_preconditions(overlay).await;

                    // An advisory overlay only says what it would do, once
                    // a window; a skipped one is not retried every second.

                    let (source, kind, level, detail) =
                        match (overlay.advisory == Some(true), &preconditions) {
                            (true, Ok(())) => (
                                "advised",
                                "overlay.would_activate",
                                "notice",
                                "would activate: its schedule's window began; it is advisory"
                                    .to_owned(),
                            ),
                            (true, Err(why)) => (
                                "advised",
                                "overlay.would_activate",
                                "notice",
                                format!("would be skipped: {why}; it is advisory"),
                            ),
                            (false, Ok(())) if crate::overlays::is_schedule(name) => (
                                "schedule",
                                "schedule.activated",
                                "notice",
                                "activated: its window began".to_owned(),
                            ),
                            (false, Ok(())) => (
                                "schedule",
                                "overlay.activated",
                                "notice",
                                "activated: its schedule's window began".to_owned(),
                            ),
                            (false, Err(why)) => (
                                "skipped",
                                "overlay.skipped",
                                "warning",
                                format!("not activated: {why}"),
                            ),
                        };

                    let active = source == "schedule";
                    let latched = active && overlay.latch == Some(true);
                    let state = crate::model::OverlayState {
                        name: name.clone(),
                        active,
                        source: source.to_owned(),
                        since_unix_ms: now,
                        until_unix_ms: if latched { None } else { until },
                    };
                    let _ = self.store.set_overlay_state(name, Some(&state));

                    // Issue 779: an activation the edge refuses is skipped
                    // for the window, saying why.

                    if !active {
                        self.events.emit(kind, &subject, level, &detail);
                    } else if let Err(error) =
                        self.push_overlay_change(name, states.get(name)).await
                    {
                        let skipped = crate::model::OverlayState {
                            active: false,
                            source: "skipped".to_owned(),
                            until_unix_ms: until,
                            ..state
                        };
                        let _ = self.store.set_overlay_state(name, Some(&skipped));
                        self.events.emit(
                            "overlay.skipped",
                            &subject,
                            "warning",
                            &format!("not activated: {error}"),
                        );
                    } else {
                        self.events.emit(kind, &subject, level, &detail);
                    }
                }
            }
        }

        // Issue 689: overlays that follow a named condition.

        let conditions: HashMap<String, crate::conditions::Status> = self
            .conditions
            .statuses()
            .into_iter()
            .map(|status| (status.name.clone(), status))
            .collect();

        for (name, overlay) in &overlays {
            if let Some(condition) = crate::overlays::condition(overlay) {
                let followed = self
                    .follow_condition(
                        name,
                        overlay,
                        condition,
                        conditions.get(condition),
                        states.get(name),
                        now,
                    )
                    .await;

                if followed == Followed::Ended {
                    ended.push((
                        "overlay.end_refused",
                        format!("overlay {name}"),
                        format!("its condition {condition} resolved"),
                    ));
                }

                changed |= followed != Followed::Stayed;
            }
        }

        let warm_ups = self.warm_ups(&overlays, now);

        if warm_ups != self.warm_ups {
            for (name, release) in &warm_ups {
                if self.warm_ups.contains_key(name) {
                    continue;
                }

                let wait = overlays
                    .iter()
                    .find(|(declared, _)| declared == name)
                    .and_then(|(_, overlay)| {
                        crate::overlays::window(overlay, now).map(|(start, _)| {
                            crate::overlays::warming_wait(overlay, start.saturating_sub(now))
                        })
                    })
                    .unwrap_or_else(|| crate::overlays::describe_wait(0));
                let mut detail = format!("warming up: its window begins in {wait}");

                if let Some((app, id)) = release {
                    detail.push_str(&format!("; starting {app}@{id}"));
                }

                self.events.emit(
                    "overlay.warming",
                    &format!("overlay {name}"),
                    "info",
                    &detail,
                );
            }

            let releases_changed = warm_ups.values().collect::<BTreeSet<_>>()
                != self.warm_ups.values().collect::<BTreeSet<_>>();
            self.warm_ups = warm_ups;

            if releases_changed && !changed {
                self.reconcile_traffic().await;
            }
        }

        if changed {
            self.reconcile_traffic().await;

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for an overlay failed");

                // Issue 790: an end restores the base configuration, so
                // there is nothing to put back when the edge refuses it.

                for (kind, subject, reason) in &ended {
                    self.events.emit(
                        kind,
                        subject,
                        "warning",
                        &format!(
                            "ended: {reason}, but {error}; the edge keeps serving with the \
                             overlay in force, and nothing can be put back, since what it \
                             refused is the base configuration: correct that and apply it again"
                        ),
                    );
                }
            }
        }
    }

    /// Section 9.10 (issue 689): activate or end an overlay by its named
    /// condition's state: active while it is firing, ended once it has
    /// resolved and the overlay has been active for `min_active`, held as
    /// it is while it is unknown. An activation the cooldown after the
    /// last end, the cap on activations an hour or a precondition holds
    /// back is skipped, with one event while the condition keeps firing.
    /// Gives what changed.
    async fn follow_condition(
        &mut self,
        name: &str,
        overlay: &muru_config::Overlay,
        condition: &str,
        status: Option<&crate::conditions::Status>,
        state: Option<&crate::model::OverlayState>,
        now: u64,
    ) -> Followed {
        use crate::conditions::State;

        let subject = format!("overlay {name}");
        let firing = status.map(|status| status.state);
        let figures = status
            .map(crate::overlays::describe_terms)
            .unwrap_or_default();

        if firing != Some(State::Firing) {
            self.condition_skips.remove(name);
        }

        let recent = if state.is_none() && firing == Some(State::Firing) {
            self.store
                .overlay_activations_since(name, now.saturating_sub(3_600_000))
                .unwrap_or(0)
        } else {
            0
        };

        let step = match crate::overlays::condition_step(overlay, firing, state, recent, now) {
            crate::overlays::ConditionStep::Activate => {
                match self.overlay_preconditions(overlay).await {
                    Ok(()) => crate::overlays::ConditionStep::Activate,
                    Err(why) => crate::overlays::ConditionStep::Skip(why),
                }
            }
            step => step,
        };

        match step {
            crate::overlays::ConditionStep::Stay => Followed::Stayed,
            crate::overlays::ConditionStep::Skip(why) => {
                if self.condition_skips.insert(name.to_owned()) {
                    self.events.emit(
                        "overlay.skipped",
                        &subject,
                        "warning",
                        &format!("not activated though its condition {condition} is firing: {why}"),
                    );
                }

                Followed::Stayed
            }
            crate::overlays::ConditionStep::End { held_until } => {
                let held = held_until.map(|until| crate::model::OverlayState {
                    name: name.to_owned(),
                    active: false,
                    source: "cooldown".to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: Some(until),
                });
                let _ = self.store.set_overlay_state(name, held.as_ref());
                self.events.emit(
                    "overlay.ended",
                    &subject,
                    "notice",
                    &format!("ended: its condition {condition} resolved{figures}"),
                );
                Followed::Ended
            }
            crate::overlays::ConditionStep::Activate => {
                let state = crate::model::OverlayState {
                    name: name.to_owned(),
                    active: true,
                    source: "condition".to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: None,
                };
                let _ = self.store.set_overlay_state(name, Some(&state));

                // Issue 779: an activation the edge refuses is held back
                // for its cooldown, a minute at least, before the firing
                // condition tries it again.

                if let Err(error) = self.push_overlay_change(name, None).await {
                    let wait = crate::overlays::cooldown(overlay).max(Duration::from_secs(60));
                    let held = crate::model::OverlayState {
                        active: false,
                        source: "skipped".to_owned(),
                        until_unix_ms: Some(now + wait.as_millis() as u64),
                        ..state
                    };
                    let _ = self.store.set_overlay_state(name, Some(&held));
                    self.condition_skips.insert(name.to_owned());
                    self.events.emit(
                        "overlay.skipped",
                        &subject,
                        "warning",
                        &format!(
                            "not activated though its condition {condition} is firing: {error}; \
                             it is tried again in {}",
                            crate::overlays::describe_wait(wait.as_millis() as u64)
                        ),
                    );
                    return Followed::Stayed;
                }

                let _ = self.store.record_overlay_activation(name, now);
                self.events.emit(
                    "overlay.activated",
                    &subject,
                    "notice",
                    &format!("activated: its condition {condition} is firing{figures}"),
                );
                Followed::Activated
            }
        }
    }

    /// Section 9.10: warn, every ten minutes while schedules are declared,
    /// when the system clock is not synchronised, as their windows follow
    /// it, and say so once it is again.
    fn check_clock(&mut self) {
        let (checked, warned) = self.clock;

        if checked.is_some_and(|at| at.elapsed() < CLOCK_CHECK) {
            return;
        }

        let scheduled = self
            .store
            .overlays()
            .unwrap_or_default()
            .iter()
            .any(|(_, overlay)| {
                overlay
                    .trigger
                    .as_ref()
                    .is_some_and(|trigger| trigger.schedule.is_some())
            });
        let event = clock_event(clock_synchronised(), warned, scheduled);
        self.clock = (Some(Instant::now()), warned);

        if let Some((kind, level, message)) = event {
            self.clock.1 = kind == "clock.unsynchronised";
            self.events.emit(kind, "daemon", level, message);
        }
    }

    /// Section 9.10: the overlays in the lead before a window of their
    /// schedule, with the release each warm-up starts.
    fn warm_ups(
        &self,
        overlays: &[(String, muru_config::Overlay)],
        now: u64,
    ) -> BTreeMap<String, Option<(String, String)>> {
        let route_apps: BTreeMap<String, String> = self
            .store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route.name.map(|name| (name, route.app)))
            .collect();

        overlays
            .iter()
            .filter(|(_, overlay)| crate::overlays::warming(overlay, now))
            .map(|(name, overlay)| {
                let release = overlay
                    .warm_up
                    .as_ref()
                    .and_then(|warm_up| warm_up.start_release.as_deref())
                    .and_then(|release| {
                        crate::config::precondition_release(release, overlay, &route_apps)
                    });
                (name.clone(), release)
            })
            .collect()
    }

    /// Section 7.5: record and activate the temporary overlay `muructl
    /// scale --min` stands for, or `--cap` (issue 712), replacing the one
    /// already there for the group. A floor and a cap are overlays apart,
    /// so each ends on its own.
    async fn scale_temporary(
        &mut self,
        app: &str,
        group: &str,
        change: muru_config::AppOverlay,
        until: u64,
    ) -> Result<String, String> {
        self.store.app(app).map_err(|error| error.to_string())?;
        let now = now_unix_ms();

        if until <= now {
            return Err("--until is not in the future".to_owned());
        }

        if change.min_workers.is_some() == change.cap_workers.is_some() {
            return Err("give a floor or a cap".to_owned());
        }

        let change = muru_config::AppOverlay {
            group: (group != muru_edge::routing::DEFAULT_GROUP).then(|| group.to_owned()),
            ..change
        };
        let overlay = muru_config::Overlay {
            apps: BTreeMap::from([(app.to_owned(), change.clone())]),
            ..muru_config::Overlay::default()
        };
        muru_config::check_overlay("scale", &overlay)?;

        let name = if change.cap_workers.is_some() {
            crate::overlays::temporary_cap_name(app, group)
        } else {
            crate::overlays::temporary_name(app, group)
        };

        self.store
            .put_overlay(&name, &overlay)
            .map_err(|error| error.to_string())?;
        let state = crate::model::OverlayState {
            name: name.clone(),
            active: true,
            source: "manual".to_owned(),
            since_unix_ms: now,
            until_unix_ms: Some(until),
        };
        self.store
            .set_overlay_state(&name, Some(&state))
            .map_err(|error| error.to_string())?;
        let detail = format!(
            "activated by muructl scale: {group} {} for {}",
            crate::overlays::describe_capacity(&change),
            crate::overlays::describe_wait(until - now)
        );
        self.events.emit(
            "overlay.activated",
            &format!("overlay {name}"),
            "notice",
            &detail,
        );
        Ok(detail)
    }

    /// Section 9.10: what a person, or with `external` the signed webhook,
    /// asks of an overlay.
    async fn control_overlay(
        &mut self,
        name: &str,
        action: OverlayAction,
        external: bool,
    ) -> Result<OverlayOutcome, String> {
        let overlay = self
            .store
            .overlays()
            .unwrap_or_default()
            .into_iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, overlay)| overlay)
            .ok_or_else(|| format!("no overlay is named {name}"))?;
        let state = self
            .store
            .overlay_states()
            .unwrap_or_default()
            .into_iter()
            .find(|state| state.name == name);
        let active = state.as_ref().is_some_and(|state| state.active);
        let subject = format!("overlay {name}");
        let now = now_unix_ms();

        let (source, by) = if external {
            ("external", "by the webhook")
        } else {
            ("manual", "by hand")
        };

        let (kind, verb, message) = match action {
            OverlayAction::Activate { until } => {
                if active {
                    return Err(format!("overlay {name} is already active"));
                }

                // Issue 776: a promoted overlay's changes are the base now,
                // and a pin laid over it again would send the traffic back
                // to the release it promoted. An apply that changes it
                // makes it a new overlay.

                if state
                    .as_ref()
                    .is_some_and(|state| state.source == "promoted")
                {
                    return Err(format!(
                        "overlay {name} was promoted, so its changes are already the base \
                         configuration; remove it from the applied files, or change it and \
                         apply them to use it as a new overlay"
                    ));
                }

                self.overlay_preconditions(&overlay)
                    .await
                    .map_err(|why| format!("overlay {name} was not activated: {why}"))?;
                let state = crate::model::OverlayState {
                    name: name.to_owned(),
                    active: true,
                    source: source.to_owned(),
                    since_unix_ms: now,
                    until_unix_ms: until,
                };
                self.store
                    .set_overlay_state(name, Some(&state))
                    .map_err(|error| error.to_string())?;
                ("overlay.activated", "activated", format!("activated {by}"))
            }
            OverlayAction::End => {
                if !active {
                    return Err(format!("overlay {name} is not active"));
                }

                // Ended inside its schedule's window, it stays ended until
                // the window is over; otherwise it is simply inactive.

                let window = crate::overlays::window(&overlay, now)
                    .filter(|(start, end)| now >= *start && end.is_none_or(|end| now < end));

                // Issue 689: one its condition activates is held through
                // its cooldown, so a condition still firing does not
                // activate it again at once.

                let cooldown = crate::overlays::condition(&overlay)
                    .map(|_| now + crate::overlays::cooldown(&overlay).as_millis() as u64);
                let held = window
                    .map(|(_, end)| end)
                    .or(cooldown.map(Some))
                    .map(|until| crate::model::OverlayState {
                        name: name.to_owned(),
                        active: false,
                        source: "ended".to_owned(),
                        since_unix_ms: now,
                        until_unix_ms: until,
                    });
                self.store
                    .set_overlay_state(name, held.as_ref())
                    .map_err(|error| error.to_string())?;
                ("overlay.ended", "ended", format!("ended {by}"))
            }
            OverlayAction::Promote => {
                if !active {
                    return Err(format!("overlay {name} is not active; activate it first"));
                }

                return self.promote_overlay(name, &overlay).await;
            }
        };

        // Issue 779: the change stands, and is reported, once the edge
        // has taken it.

        self.push_overlay_change(name, state.as_ref())
            .await
            .map_err(|error| format!("overlay {name} was not {verb}: {error}"))?;

        if kind == "overlay.ended" && crate::overlays::is_temporary(name) {
            self.store
                .delete_overlay(name)
                .map_err(|error| error.to_string())?;
        }

        self.events.emit(kind, &subject, "notice", &message);
        Ok(OverlayOutcome {
            message,
            files: Vec::new(),
        })
    }

    /// Issue 779: give the edge the snapshot once an overlay's state has
    /// changed from `previous`, and put `previous` back when the edge
    /// refuses it, so that the overlay is not recorded as in force, or
    /// as ended, when the edge serves otherwise. Gives the edge's error.
    async fn push_overlay_change(
        &mut self,
        name: &str,
        previous: Option<&crate::model::OverlayState>,
    ) -> Result<(), String> {
        self.reconcile_traffic().await;

        let Err(error) = self.push_snapshot().await else {
            return Ok(());
        };

        let _ = self.store.set_overlay_state(name, previous);
        self.reconcile_traffic().await;

        if let Err(again) = self.push_snapshot().await {
            tracing::error!(error = %again, "pushing the snapshot without overlay {name}'s change failed");
        }

        Err(error)
    }

    /// Section 9.10: fold an active overlay into the base and end it for
    /// good. Issue 759: a pin makes the pinned release its application's
    /// serving release, the one that served before drained and retained
    /// as after a deploy, so later deploys replace it; the routes take
    /// the rest of what the overlay sets, and the applications its
    /// maintenance. A split is refused, being a route's traffic policy.
    /// Issue 790: a promotion the edge refuses is put back whole and
    /// refused with the edge's reason. The outcome carries what the
    /// applied files must say for the next apply to keep the promotion.
    async fn promote_overlay(
        &mut self,
        name: &str,
        overlay: &muru_config::Overlay,
    ) -> Result<OverlayOutcome, String> {
        let refused = |why: String| format!("overlay {name} was not promoted: {why}");
        let routes = self.store.routes().unwrap_or_default();
        let promotion = crate::overlays::promotion(name, overlay, &routes).map_err(refused)?;

        for (app, release) in &promotion.serving {
            if let Ok(Some(rollout)) = self.store.active_rollout(app) {
                return Err(refused(format!(
                    "a rollout of {app}@{} is under way; promote or abort it first",
                    rollout.candidate
                )));
            }

            if !self.running.contains_key(&(app.clone(), release.clone())) {
                return Err(refused(format!("{app}@{release} is not running")));
            }
        }

        // Issue 790: the edge has no way to check a snapshot without
        // applying it, so every change is made, the result pushed once,
        // and the changes put back when the edge refuses it. Nothing
        // drains, and no event is raised, until it has taken it.

        let mut undo = PromotionUndo::default();
        let changed = self.promote_changes(name, overlay, &promotion, &mut undo);

        let pushed = match changed {
            Ok(()) => self.push_snapshot().await,
            Err(error) => Err(error),
        };

        if let Err(error) = pushed {
            self.undo_promotion(name, undo);

            if let Err(again) = self.push_snapshot().await {
                tracing::error!(error = %again, "pushing the snapshot without overlay {name}'s promotion failed");
            }

            return Err(refused(error));
        }

        // The edge serves the pinned releases now, so the releases that
        // served before drain, unless something else still names them.

        let mut message = "promoted: its changes are now the base configuration".to_owned();
        let referenced = self.referenced_releases();

        for switch in &undo.serving {
            let subject = format!("{}@{}", switch.app, switch.release);
            self.mark_ready(&switch.app, &switch.release, "");
            self.events.emit(
                "traffic.switched",
                &subject,
                "notice",
                "traffic moved to the release",
            );

            match &switch.current {
                Some(current) => message.push_str(&format!(
                    "; {}@{} serves in place of {}@{current}, which is retained",
                    switch.app, switch.release, switch.app
                )),
                None => message.push_str(&format!("; {subject} serves")),
            }

            if let Some(previous) = &switch.current
                && !referenced.contains(&(switch.app.clone(), previous.clone()))
            {
                self.drain(&switch.app, previous, Retention::default())
                    .await;
            }
        }

        self.events.emit(
            "overlay.promoted",
            &format!("overlay {name}"),
            "notice",
            &message,
        );
        self.reconcile_traffic().await;
        Ok(OverlayOutcome {
            message,
            files: crate::overlays::promotion_files(&promotion),
        })
    }

    /// Issue 790: make a promotion's changes, recording in `undo` what
    /// each replaced: the pinned releases serve, in memory and in the
    /// store, the routes take what the overlay sets, the applications its
    /// maintenance, and the overlay is recorded promoted. Nothing is
    /// pushed to the edge.
    fn promote_changes(
        &mut self,
        name: &str,
        overlay: &muru_config::Overlay,
        promotion: &crate::overlays::Promotion,
        undo: &mut PromotionUndo,
    ) -> Result<(), String> {
        let now = now_unix_ms();

        undo.state = self
            .store
            .overlay_states()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|state| state.name == name);

        for (app, release) in &promotion.serving {
            let record = self.store.app(app).map_err(|error| error.to_string())?;

            if record.current.as_deref() == Some(release.as_str()) {
                continue;
            }

            let key = (app.clone(), release.clone());
            let held = self.running.get(&key).is_some_and(|running| running.held);

            undo.serving.push(PromotedSwitch {
                app: app.clone(),
                release: release.clone(),
                serving: self.serving.get(app).cloned(),
                held,
                current: record.current.clone(),
                previous: record.previous.clone(),
            });
            self.serving.insert(app.clone(), release.clone());

            if let Some(running) = self.running.get_mut(&key) {
                running.held = false;
            }

            self.store
                .set_app_releases(app, Some(release), record.current.as_deref())
                .map_err(|error| error.to_string())?;
        }

        let routes = self.store.routes().map_err(|error| error.to_string())?;

        for route in &promotion.routes {
            let before = routes
                .iter()
                .find(|old| old.host == route.host && old.key() == route.key())
                .cloned();
            undo.routes.push((route.host.clone(), route.key(), before));
            self.store
                .put_route(route)
                .map_err(|error| error.to_string())?;
        }

        let stored: HashMap<String, u64> = self
            .store
            .maintenance()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect();

        for (app, change) in &overlay.apps {
            let Some(on) = change.maintenance else {
                continue;
            };

            let since = on.then_some(now);
            undo.maintenance.push((
                app.clone(),
                stored.get(app).copied(),
                self.maintenance.get(app).copied(),
            ));
            self.store
                .set_maintenance(app, since)
                .map_err(|error| error.to_string())?;

            match since {
                Some(since) => {
                    self.maintenance.entry(app.clone()).or_insert(since);
                }
                None => {
                    self.maintenance.remove(app);
                }
            }
        }

        let held = crate::model::OverlayState {
            name: name.to_owned(),
            active: false,
            source: "promoted".to_owned(),
            since_unix_ms: now,
            until_unix_ms: None,
        };
        self.store
            .set_overlay_state(name, Some(&held))
            .map_err(|error| error.to_string())
    }

    /// Issue 790: put back what a promotion the edge refused changed, as
    /// `undo` recorded it, latest first.
    fn undo_promotion(&mut self, name: &str, undo: PromotionUndo) {
        let _ = self.store.set_overlay_state(name, undo.state.as_ref());

        for (app, stored, kept) in undo.maintenance.into_iter().rev() {
            let _ = self.store.set_maintenance(&app, None);
            let _ = self.store.set_maintenance(&app, stored);

            match kept {
                Some(since) => {
                    self.maintenance.insert(app, since);
                }
                None => {
                    self.maintenance.remove(&app);
                }
            }
        }

        for (host, key, before) in undo.routes.into_iter().rev() {
            let _ = match before {
                Some(route) => self.store.put_route(&route),
                None => self.store.delete_route(&host, &key),
            };
        }

        for switch in undo.serving.into_iter().rev() {
            let _ = self.store.set_app_releases(
                &switch.app,
                switch.current.as_deref(),
                switch.previous.as_deref(),
            );

            match switch.serving {
                Some(serving) => {
                    self.serving.insert(switch.app.clone(), serving);
                }
                None => {
                    self.serving.remove(&switch.app);
                }
            }

            if let Some(running) = self.running.get_mut(&(switch.app, switch.release)) {
                running.held = switch.held;
            }
        }
    }

    /// Section 4.14: read the client classes' list files again when one
    /// has changed, and give the edge the result.
    async fn refresh_client_lists(&mut self) {
        match self.clients.refresh() {
            Ok(changed) if changed.is_empty() => {}
            Ok(changed) => {
                let names: Vec<String> = changed
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                self.events.emit(
                    "client_class.list_reloaded",
                    "daemon",
                    "info",
                    &format!("read {} again", names.join(", ")),
                );

                if let Err(error) = self.push_snapshot().await {
                    tracing::error!(%error, "pushing the snapshot with the reloaded lists failed");
                }
            }
            Err(error) => {
                // The classes the edge has stay until the list reads.

                tracing::warn!(%error, "a client class list could not be read; the edge keeps the last one read");
            }
        }
    }

    /// Section 4.15: a rate limit that begins refusing or holding
    /// requests, after a quiet minute, is an event.
    fn watch_rate_limits(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };

        for (limit, count) in self.enforcement.observe(samples, Instant::now()) {
            self.events.emit(
                "ratelimit.enforcing",
                &format!("rate_limit {limit}"),
                "warning",
                &format!("began refusing or holding requests: {count:.0} so far"),
            );
        }
    }

    /// The node's metrics: the edge's samples with the daemon's own
    /// gauges added, as the Prometheus text exposition.
    async fn metrics(&self) -> String {
        let mut samples = match self.edge.metrics().await {
            Ok(samples) => samples,
            Err(error) => {
                tracing::warn!(%error, "the edge's metrics could not be read");
                Vec::new()
            }
        };

        for ((app, id), running) in &self.running {
            let mut groups = match &running.backend {
                Backend::Servers(servers) => vec![(
                    muru_edge::routing::SERVER_GROUP.to_owned(),
                    servers.running(),
                )],
                Backend::Host(_) | Backend::Dormant => vec![(
                    muru_edge::routing::DEFAULT_GROUP.to_owned(),
                    running.sockets.len(),
                )],
            };

            for group in &running.groups {
                groups.push((group.name.clone(), group.sockets.len()));
            }

            for (group, workers) in groups {
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_workers_ready",
                    &[
                        ("app", app.as_str()),
                        ("release", id.as_str()),
                        ("group", group.as_str()),
                    ],
                    workers as f64,
                ));
            }

            // Section 7.4: what the autoscaler wants and has done.

            for scaling in &running.scalers {
                let labels = [
                    ("app", app.as_str()),
                    ("release", id.as_str()),
                    ("group", scaling.name.as_str()),
                ];
                let (outs, ins) = scaling.scaler.status(Instant::now()).decisions;
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_workers_desired",
                    &labels,
                    scaling.scaler.desired() as f64,
                ));
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_churn_reaps_total",
                    &labels,
                    scaling.scaler.reaps() as f64,
                ));

                for (direction, count) in [("out", outs), ("in", ins)] {
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_scaling_decisions_total",
                        &[
                            ("app", app.as_str()),
                            ("release", id.as_str()),
                            ("group", scaling.name.as_str()),
                            ("direction", direction),
                        ],
                        count as f64,
                    ));
                }
            }

            // Issue 753: how long the host took to load the application.

            if let Some(ms) = running.load_ms {
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_release_load_seconds",
                    &[("app", app.as_str()), ("release", id.as_str())],
                    ms as f64 / 1000.0,
                ));
            }

            // Sections 7.6 and 7.7: what each worker last reported.

            for worker in &running.status {
                let index = worker.index.to_string();
                let labels = [
                    ("app", app.as_str()),
                    ("release", id.as_str()),
                    ("group", worker.group.as_str()),
                    ("worker", index.as_str()),
                ];

                if let Some(cpu_ms) = worker.cpu_ms {
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_worker_cpu_seconds_total",
                        &labels,
                        cpu_ms as f64 / 1000.0,
                    ));
                }

                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_total",
                    &labels,
                    worker.requests as f64,
                ));
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_worker_requests_in_flight",
                    &labels,
                    worker.in_flight as f64,
                ));
                samples.extend(muru_edge::metrics::module_samples(
                    &labels,
                    worker.modules,
                    worker.modules_us,
                    worker.load_us,
                ));

                // Section 6.6: the pools an ASGI worker runs synchronous
                // code on, each of which fills while every new call
                // waits.

                for (pool, busy, threads) in &worker.sync_pools {
                    let mut labels = labels.to_vec();
                    labels.push(("pool", pool.as_str()));
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_worker_sync_threads_busy",
                        &labels,
                        *busy as f64,
                    ));
                    samples.push(muru_edge::metrics::Sample::new(
                        "muru_worker_sync_threads",
                        &labels,
                        *threads as f64,
                    ));
                }

                // Section 7.3: the worker's own signals, as histograms
                // once it has observed anything.

                samples.extend(muru_edge::metrics::signal_histogram(
                    "muru_gil_wait_seconds",
                    &labels,
                    &worker.gil_wait,
                    worker.gil_wait_us,
                ));
                samples.extend(muru_edge::metrics::signal_histogram(
                    "muru_loop_lag_seconds",
                    &labels,
                    &worker.loop_lag,
                    worker.loop_lag_us,
                ));
                samples.extend(muru_edge::metrics::signal_histogram(
                    "muru_handoff_queue_wait_seconds",
                    &labels,
                    &worker.handoff_wait,
                    worker.handoff_wait_us,
                ));

                // Section 11.1: its garbage collections, by generation.

                for generation in &worker.gc {
                    let labels = [
                        ("app", app.as_str()),
                        ("release", id.as_str()),
                        ("group", worker.group.as_str()),
                        ("worker", index.as_str()),
                        ("generation", generation.generation.as_str()),
                    ];

                    for (name, value) in [
                        ("muru_gc_collections_total", generation.collections as f64),
                        ("muru_gc_seconds_total", generation.nanos as f64 / 1e9),
                        (
                            "muru_gc_collected_objects_total",
                            generation.collected as f64,
                        ),
                    ] {
                        samples.push(muru_edge::metrics::Sample::new(name, &labels, value));
                    }

                    for (requests, counts, sum_us) in [
                        ("none", &generation.idle, generation.idle_us),
                        ("in_flight", &generation.busy_pauses, generation.busy_us),
                    ] {
                        let mut with_requests = labels.to_vec();
                        with_requests.push(("requests", requests));
                        samples.extend(muru_edge::metrics::signal_histogram(
                            "muru_gc_pause_seconds",
                            &with_requests,
                            counts,
                            sum_us,
                        ));
                    }
                }

                for (measure, bytes) in [
                    ("rss", worker.rss),
                    ("pss", worker.pss),
                    ("uss", worker.uss),
                ] {
                    if let Some(bytes) = bytes {
                        samples.push(muru_edge::metrics::Sample::new(
                            "muru_worker_memory_bytes",
                            &[
                                ("app", app.as_str()),
                                ("release", id.as_str()),
                                ("group", worker.group.as_str()),
                                ("worker", index.as_str()),
                                ("measure", measure),
                            ],
                            bytes as f64,
                        ));
                    }
                }
            }

            // Section 11.3: output the release's relay could not keep.

            if let Some(dropped) = running.backend.output_dropped() {
                samples.push(muru_edge::metrics::Sample::new(
                    "muru_app_output_dropped_total",
                    &[("app", app.as_str()), ("release", id.as_str())],
                    dropped as f64,
                ));
            }

            // Sections 7.6 and 7.7: workers replaced and workers gone.

            for (name, counts) in [
                ("muru_worker_recycles_total", &running.recycles),
                ("muru_worker_exits_total", &running.exits),
            ] {
                for ((group, reason), count) in counts {
                    samples.push(muru_edge::metrics::Sample::new(
                        name,
                        &[
                            ("app", app.as_str()),
                            ("release", id.as_str()),
                            ("group", group.as_str()),
                            ("reason", reason.as_str()),
                        ],
                        *count as f64,
                    ));
                }
            }
        }

        muru_edge::metrics::render(&muru_edge::metrics::aggregate(samples))
    }

    /// Re-adopt the hosts a previous daemon left running (section 10.2),
    /// and start again the release that served if its host is gone.
    async fn restore(&mut self) {
        // Issue 688: the runs a previous daemon left under way.

        self.runs.adopt();
        self.load_conditions();

        self.maintenance = self
            .store
            .maintenance()
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.host_maintenance = self
            .store
            .host_maintenance()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let apps = match self.store.apps() {
            Ok(apps) => apps,
            Err(error) => {
                tracing::error!(%error, "reading the applications failed");
                return;
            }
        };

        for app in apps {
            // A release in a running state is taken over when its host is
            // still there and listening; otherwise it is gone with the
            // previous daemon.

            if let Ok(releases) = self.store.releases(Some(&app.name)) {
                for release in releases.iter().filter(|release| release.state.is_running()) {
                    if self.adopt_host(&release.app, &release.id).await {
                        continue;
                    }

                    let _ = self.store.set_release_state(
                        &release.app,
                        &release.id,
                        ReleaseState::Stopped,
                        "the daemon restarted",
                    );
                }
            }

            let Some(current) = app.current.clone() else {
                continue;
            };
            if self
                .running
                .contains_key(&(app.name.clone(), current.clone()))
            {
                self.switch_to(&app.name, &current).await;
                continue;
            }

            self.start_then(
                &app.name,
                &current,
                AfterStart::Restore {
                    app: app.name.clone(),
                    id: current.clone(),
                },
            )
            .await;
        }
    }

    /// Take over a release's host, or a managed server's instances, from
    /// its record, if it is still there.
    async fn adopt_host(&mut self, app: &str, id: &str) -> bool {
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);

        if let Some(record) = read_record::<ServersRecord>(&run_dir.join("servers.toml")) {
            return self.adopt_servers(app, id, &run_dir, &record).await;
        }

        let Some(record) = read_record::<HostRecord>(&run_dir.join("host.toml")) else {
            tracing::info!("release {app}@{id} could not be re-adopted: no readable host record");
            return false;
        };

        if !command_mentions(record.pid, &run_dir, &format!("muru host {app}@{id}")) {
            tracing::info!(
                pid = record.pid,
                "release {app}@{id} could not be re-adopted: no process of its host has the pid"
            );
            return false;
        }

        let group_sockets = record
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.sockets.clone()))
            .collect();
        let optional = record
            .groups
            .iter()
            .filter(|group| group.optional)
            .map(|group| group.name.clone())
            .collect();

        // Section 11.3: the relay is adopted with its host, or replaced
        // if it is gone.

        let subject = format!("{app}@{id}");

        let identity = match self.resolve_identity(app, &subject) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                return false;
            }
        };

        let host_dir = run_dir.join("r1");
        let launch = match crate::host::RelayLaunch::for_release(
            host_dir.clone(),
            Some(self.host_log(app, id)),
            self.relay_settings,
            identity,
            subject.clone(),
        ) {
            Ok(launch) => launch,
            Err(error) => {
                tracing::info!(error = %format!("{error:#}"), "release {subject} could not be re-adopted");
                return false;
            }
        };
        let relay_pid = read_record::<RelayRecord>(&host_dir.join(crate::relay::RECORD))
            .map(|record| record.pid)
            .filter(|pid| command_mentions(*pid, &host_dir, &format!("muru relay {subject}")));
        let watcher = self.relay_watcher(&subject);
        let adopted = HostProcess::adopt(
            record.pid,
            record.sockets.clone(),
            group_sockets,
            optional,
            host_dir,
            1,
            launch,
            relay_pid,
            watcher,
        );

        match adopted {
            Ok(host) => {
                let sockets = host.default_sockets();
                let retained_until = matches!(
                    self.store.release(app, id).map(|release| release.state),
                    Ok(ReleaseState::Retained)
                )
                .then(|| Instant::now() + RETENTION);
                let now = Instant::now();
                let pins = self.store.scale_pins(app, id).unwrap_or_default();
                let scalers = self
                    .store
                    .release(app, id)
                    .map(|release| {
                        let interface =
                            self.info
                                .interfaces
                                .runs_as(app, id, "", release.spec.interface);
                        scalers_for(
                            &release.spec,
                            interface,
                            &sockets,
                            &record.groups,
                            &pins,
                            now,
                        )
                    })
                    .unwrap_or_default();
                let ready_at = record
                    .sockets
                    .iter()
                    .map(|socket| (socket.clone(), now))
                    .collect();
                self.running.insert(
                    (app.to_owned(), id.to_owned()),
                    Running {
                        backend: Backend::Host(host),
                        sockets,
                        groups: record.groups.clone(),
                        retained_until,
                        retained_until_unix_ms: unix_ms_of(retained_until),
                        workers: Vec::new(),
                        announced: false,
                        inspected: false,
                        sticky_until: None,
                        held: false,
                        control: None,
                        status: Vec::new(),
                        load_ms: None,
                        memory_replacements: Vec::new(),
                        recycles: BTreeMap::new(),
                        exits: BTreeMap::new(),
                        signals_from: None,
                        scalers,
                        host_pss: None,
                        idle_since: None,
                        ready_at,
                    },
                );
                self.attach_control(app, id);
                self.events.emit(
                    "release.adopted",
                    &format!("{app}@{id}"),
                    "info",
                    &format!("re-adopted the host (pid {})", record.pid),
                );
                true
            }
            Err(error) => {
                tracing::info!(%error, "release {app}@{id} could not be re-adopted");
                false
            }
        }
    }

    /// Section 9.9: take over a managed server's instances from their
    /// record, with what a restart of one would need resolved again.
    async fn adopt_servers(
        &mut self,
        app: &str,
        id: &str,
        run_dir: &Path,
        record: &ServersRecord,
    ) -> bool {
        let Ok(release) = self.store.release(app, id) else {
            return false;
        };
        let Some(http) = release.spec.http.clone() else {
            return false;
        };

        let subject = format!("{app}@{id}");

        let identity = match self.resolve_identity(app, &subject) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                return false;
            }
        };

        let env =
            match resolve_env(&self.secrets, &release.spec.env, &run_dir.join("secrets")).await {
                Ok(env) => env,
                Err(error) => {
                    tracing::info!(%error, "release {subject} could not be re-adopted");
                    return false;
                }
            };
        let env = self.server_environment(env, app, id, &release.spec, identity);

        let relays = match self.server_relays(&subject) {
            Ok(relays) => relays,
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                return false;
            }
        };

        let adopted = Servers::adopt(
            &http,
            &run_dir.join("r1"),
            &release.spec.source,
            env,
            identity,
            &self.host_log(app, id),
            Some(relays),
            record,
        )
        .await;

        match adopted {
            Ok(servers) => {
                let retained_until = matches!(release.state, ReleaseState::Retained)
                    .then(|| Instant::now() + RETENTION);
                let message = adoption_message(&servers.adopted());
                self.running.insert(
                    (app.to_owned(), id.to_owned()),
                    Running {
                        backend: Backend::Servers(servers),
                        sockets: Vec::new(),
                        groups: Vec::new(),
                        retained_until,
                        retained_until_unix_ms: unix_ms_of(retained_until),
                        workers: Vec::new(),
                        announced: false,
                        inspected: false,
                        sticky_until: None,
                        held: false,
                        control: None,
                        status: Vec::new(),
                        load_ms: None,
                        memory_replacements: Vec::new(),
                        recycles: BTreeMap::new(),
                        exits: BTreeMap::new(),
                        signals_from: None,
                        scalers: Vec::new(),
                        host_pss: None,
                        idle_since: None,
                        ready_at: HashMap::new(),
                    },
                );
                self.publish_servers();
                self.events
                    .emit("release.adopted", &subject, "info", &message);
                true
            }
            Err(error) => {
                tracing::info!(%error, "release {subject} could not be re-adopted");
                false
            }
        }
    }

    /// Section 9.10 (issue 123): give the evaluator the conditions declared
    /// now, the daemon's file's and the applied files', a name the file
    /// declares staying the file's, as for rate limits.
    fn load_conditions(&mut self) {
        let mut declared: Vec<(String, muru_config::Condition, &'static str)> = self
            .file_conditions
            .iter()
            .map(|(name, condition)| (name.clone(), condition.clone(), "file"))
            .collect();

        for (name, condition) in self.store.conditions().unwrap_or_default() {
            if self.file_conditions.contains_key(&name) {
                tracing::warn!(condition = %name, "the daemon's file declares an applied condition too; its table stands");
            } else {
                declared.push((name, condition, "applied"));
            }
        }

        for error in self.conditions.set(declared) {
            tracing::warn!("{error}");
        }

        self.publish_conditions();
    }

    /// Section 9.10 (issue 123): evaluate every condition over this
    /// second's figures, raising its events, and publish their states.
    fn evaluate_conditions(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        if self.conditions.is_empty() {
            return;
        }

        let mut scaling = Vec::new();

        for ((app, _), running) in &self.running {
            for group in &running.scalers {
                scaling.push((
                    app.clone(),
                    group.name.clone(),
                    group.at_max_wanting_more(),
                    group.denied(),
                ));
            }
        }

        let budget = self.budget();
        let figures = crate::conditions::Figures {
            scaling,
            node_memory_available: budget
                .memory
                .map(|limit| limit.saturating_sub(self.budget_usage().memory) as f64),
        };

        let events = self.conditions.observe(
            crate::model::now_unix_ms(),
            samples.unwrap_or_default(),
            &figures,
        );

        for (kind, subject, level, message) in events {
            self.events.emit(kind, &subject, level, &message);
        }

        self.publish_conditions();
    }

    /// The conditions' states, for the admin API.
    fn publish_conditions(&self) {
        *self.info.conditions.lock().expect("conditions poisoned") = self.conditions.statuses();
    }

    /// Section 9.12 (issue 268): run each command whose cron schedule is
    /// due, with its application's current release. A schedule first seen,
    /// at the daemon's start or with a new release, is due from now, so a
    /// time that passed while no daemon ran, or while the application had
    /// no release that runs commands, is not caught up.
    async fn run_schedules(&mut self) {
        let now = crate::model::now_unix_ms();
        let mut seen = BTreeSet::new();
        let mut due = Vec::new();

        for (app, release, command) in self.scheduled_commands() {
            let Some(schedule) = &command.schedule else {
                continue;
            };
            let Some(cron) = &schedule.cron else {
                continue;
            };

            let key = (app.clone(), command.name.clone());
            let shape = (cron.clone(), schedule.timezone.clone());
            seen.insert(key.clone());

            match self.schedules.get(&key) {
                Some(scheduled) if scheduled.shape == shape && now >= scheduled.next => {
                    due.push((app, release, command.clone(), scheduled.next));
                }
                Some(scheduled) if scheduled.shape == shape => continue,
                _ => {}
            }

            if let Some(next) = schedule.next_after(now) {
                self.schedules.insert(key, Scheduled { shape, next });
            }
        }

        self.schedules.retain(|key, _| seen.contains(key));

        for (app, release, command, at) in due {
            self.run_scheduled(&app, &release, &command, Some(at)).await;
        }
    }

    /// Section 9.12 (issue 268): run the commands with `at_boot`, once,
    /// when this daemon started from nothing.
    async fn run_at_boot(&mut self) {
        if !self.booted {
            return;
        }

        for (app, release, command) in self.scheduled_commands() {
            if command
                .schedule
                .as_ref()
                .is_some_and(|schedule| schedule.at_boot)
            {
                self.run_scheduled(&app, &release, &command, None).await;
            }
        }
    }

    /// Every command with a schedule, with its application and the
    /// application's current release: only a release that runs commands,
    /// which a managed server's does not.
    fn scheduled_commands(&self) -> Vec<(String, String, crate::model::CommandSpec)> {
        let mut scheduled = Vec::new();

        for app in self.store.apps().unwrap_or_default() {
            let Some(current) = app.current else {
                continue;
            };
            let Ok(release) = self.store.release(&app.name, &current) else {
                continue;
            };

            if release.spec.http.is_some() {
                continue;
            }

            for command in release.spec.commands {
                if command.schedule.is_some() {
                    scheduled.push((app.name.clone(), current.clone(), command));
                }
            }
        }

        scheduled
    }

    /// Start a scheduled run of a command, detached, with the trigger
    /// `schedule`: due at `due` by its cron schedule, or at the daemon's
    /// start. A run of the command already under way, scheduled or by
    /// hand, skips this one.
    async fn run_scheduled(
        &mut self,
        app: &str,
        release: &str,
        command: &crate::model::CommandSpec,
        due: Option<u64>,
    ) {
        let subject = format!("{app}@{release}");

        let when = match due {
            Some(at) => format!("the run due at {}", full_time(at)),
            None => "the run at the daemon's start".to_owned(),
        };

        if let Some(run) = self
            .store
            .runs(app, Some(&command.name))
            .unwrap_or_default()
            .into_iter()
            .find(crate::model::RunRecord::under_way)
        {
            self.events.emit(
                "command.skipped",
                &subject,
                "info",
                &format!(
                    "{}: {when} was skipped, since run {} is under way",
                    command.name, run.id
                ),
            );
            return;
        }

        let Some(schedule) = &command.schedule else {
            return;
        };

        let request = crate::commands::CommandRequest::Declared {
            name: command.name.clone(),
            inputs: schedule.inputs.iter().cloned().collect(),
            secret_inputs: schedule.secret_inputs.iter().cloned().collect(),
        };

        let started = match (
            self.prepare_command(app, release, request).await,
            running_executable(),
        ) {
            (Ok(prepared), Ok(executable)) => Ok((prepared, executable)),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error.to_string()),
        };

        let (prepared, executable) = match started {
            Ok(started) => started,
            Err(error) => {
                self.events.emit(
                    "command.failed",
                    &subject,
                    "warning",
                    &format!("{}: {when} could not be started: {error}", command.name),
                );
                return;
            }
        };

        // The run starts on a task of its own, so the daemon's loop does
        // not wait for its relay; a run it cannot start records its end.

        let runs = self.runs.clone();
        let events = self.events.clone();
        let name = command.name.clone();

        tokio::spawn(async move {
            if let Err(error) = runs
                .start_detached(
                    prepared,
                    crate::runs::SCHEDULE,
                    crate::runs::DAEMON,
                    &executable,
                )
                .await
                && error.contains("runs one at a time")
            {
                events.emit(
                    "command.skipped",
                    &subject,
                    "info",
                    &format!("{name}: {when} was skipped: {error}"),
                );
            }
        });
    }

    /// Leave every host and the edge running, for the next daemon.
    fn detach(&mut self) {
        let running = std::mem::take(&mut self.running);

        for (_, running) in running {
            running.backend.release();
        }
    }

    async fn carry_out(&mut self, command: Command) {
        match command {
            Command::Deploy {
                app,
                spec,
                id,
                retain,
                hold,
                created,
                done,
            } => {
                self.deploy(&app, *spec, id, retain, hold, created, done)
                    .await;
            }
            Command::Rollback { app, done } => {
                self.rollback(&app, done).await;
            }
            Command::StartRollout {
                app,
                release,
                steps,
                gates,
                approve_each_step,
                done,
            } => {
                let checked = self.check_rollout(&app, &release, &steps, &gates).await;

                match checked {
                    Ok(()) => {
                        self.start_then(
                            &app,
                            &release,
                            AfterStart::StartRollout {
                                app: app.clone(),
                                candidate: release.clone(),
                                steps,
                                gates,
                                approve_each_step,
                                done,
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        let _ = done.send(Err(error));
                    }
                }
            }
            Command::ControlRollout {
                app,
                action: RolloutAction::Promote,
                done,
            } => {
                self.promote_rollout_by_hand(&app, done).await;
            }
            Command::ControlRollout { app, action, done } => {
                let _ = done.send(self.control_rollout(&app, action).await);
            }
            Command::PromoteRelease {
                app,
                id,
                retain,
                done,
            } => {
                // Section 9.3: a promotion by hand waits for the rollout
                // under way to end; the rollout's own promotion does not
                // come this way.

                let rollout = self.store.active_rollout(&app).ok().flatten();

                match crate::rollouts::promotion_refused(rollout.as_ref(), &app, &id) {
                    Some(why) => {
                        let _ = done.send(Err(why));
                    }
                    None => {
                        self.promote_release(&app, &id, retain, AfterPromote::Reply(done))
                            .await;
                    }
                }
            }
            Command::SetScale {
                app,
                id,
                group,
                processes,
                unpin,
                done,
            } => {
                let _ = done.send(self.set_scale(&app, &id, &group, processes, unpin));
            }
            Command::GetScaleStatus { app, id, done } => {
                let _ = done.send(self.scale_status(&app, &id));
            }
            Command::SetMaintenance { app, on, done } => {
                let _ = done.send(self.set_maintenance(&app, on).await);
            }
            Command::SetHostMaintenance { host, on, done } => {
                let _ = done.send(self.set_host_maintenance(&host, on).await);
            }
            Command::OverrideAccessLog {
                change,
                reset,
                done,
            } => {
                let _ = done.send(self.override_access_log(change, reset).await);
            }
            Command::Drain { app, id, done } => {
                // Issue 699: a drain is answered once the release's stop has
                // ended.

                let drained = self.drain_release(&app, &id).await;

                match self.stops.get_mut(&(app, id)) {
                    Some(stop) if drained.is_ok() => stop.waiters.push(AfterStop::Drained(done)),
                    _ => {
                        let _ = done.send(drained);
                    }
                }
            }
            Command::ReferencedReleases { done } => {
                let _ = done.send(self.referenced_releases());
            }
            Command::ForgetRelease { app, id, done } => {
                let _ = done.send(self.forget_release(&app, &id));
            }
            Command::AppRemovalRefused { app, done } => {
                let _ = done.send(self.app_removal_refused(&app));
            }
            Command::RemoveApp { app, done } => {
                self.remove_app_then(app, done).await;
            }
            Command::Stopped {
                app,
                id,
                why,
                timed_out,
            } => {
                self.stopped(&app, &id, &why, timed_out).await;
            }
            Command::ServerProbed {
                app,
                id,
                index,
                pid,
                answered,
            } => {
                self.server_probed(&app, &id, index, pid, answered).await;
            }
            Command::ServerEnded {
                app,
                id,
                index,
                pid,
            } => {
                self.server_ended(&app, &id, index, pid).await;
            }
            Command::StartHeld { app, id, done } => {
                self.start_held(&app, &id, done).await;
            }
            Command::CommitApply {
                changes,
                retain,
                done,
            } => {
                let _ = done.send(self.commit_apply(&changes, retain).await);
            }
            Command::RefreshCertificates { done } => {
                let mut changes = self.refresh_certificates(true);

                if !changes.renewed.is_empty()
                    && let Err(error) = self.push_snapshot().await
                {
                    tracing::warn!(%error, "pushing the renewed certificates failed");
                    changes.refused = Some(error);
                }

                let _ = done.send(changes);
            }
            Command::Reload { done } => {
                let _ = done.send(self.reload().await);
            }
            Command::ReopenLogs { workers, done } => {
                self.reopen_logs_answering(workers, done);
            }
            Command::RotateLogs { logs, done } => {
                let _ = done.send(self.rotate_named_logs(logs));
            }
            Command::ClaimLogs { paths, done } => {
                let _ = done.send(self.claim_logs(paths));
            }
            Command::ReleaseLogs { paths } => {
                for path in paths {
                    self.claimed_logs.remove(&path);
                }
            }
            Command::Metrics { done } => {
                let _ = done.send(self.metrics().await);
            }
            Command::WebSockets { app, limit, done } => {
                let _ = done.send(self.edge.websockets(&app, limit).await);
            }
            Command::Requests { app, limit, done } => {
                let _ = done.send(self.edge.requests(&app, limit).await);
            }
            Command::Dashboard { done } => {
                let _ = done.send(self.dashboard_data().await);
            }
            Command::Processes { done } => {
                let _ = done.send(self.processes());
            }
            Command::HostAttached { app, id, control } => {
                if let Some(running) = self.running.get_mut(&(app, id)) {
                    running.control = Some(control);
                }
            }
            Command::HostMessage { app, id, message } => {
                self.host_message(&app, &id, message).await;
            }
            Command::StartStage { app, id, stage } => {
                self.start_stage(&app, &id, *stage).await;
            }
            Command::PromoteGated {
                app,
                id,
                retain,
                then,
                outcome,
            } => {
                let promoted = match outcome {
                    Ok(()) => self.promote_switch(&app, &id, retain).await,
                    Err(error) => Err(error),
                };

                self.promoted(*then, promoted).await;
            }
            Command::PrepareCommand {
                app,
                id,
                request,
                done,
            } => {
                let _ = done.send(self.prepare_command(&app, &id, request).await);
            }
            Command::Debug {
                app,
                id,
                op,
                worker,
                args,
                done,
            } => {
                // Section 11.7: exec runs code in a worker, and so does a
                // debugger session, so both are off unless the daemon's
                // file allows them, and the operations that reach into a
                // worker are recorded.

                if matches!(op.as_str(), "exec" | "attach" | "attach_io" | "detach")
                    && !self.debug_exec
                {
                    let what = if op == "exec" {
                        "debug exec"
                    } else {
                        "debugger attach"
                    };
                    let _ = done.send(Err(format!(
                        "{what} is off: set debug_exec = true in the [daemon] table of the \
                         daemon's configuration file"
                    )));
                    return;
                }

                let whom = worker.as_ref().map_or_else(
                    || "every worker".to_owned(),
                    |worker| format!("worker {worker}"),
                );

                if op == "attach" {
                    self.events.emit(
                        "debug.attached",
                        &format!("{app}@{id}"),
                        "warning",
                        &format!("a debugger session opened in {whom}"),
                    );
                }

                if matches!(op.as_str(), "profile" | "trace" | "memory" | "exec") {
                    let subject = format!("{app}@{id}");

                    let detail = if op == "exec" {
                        let code: String = args
                            .get("code")
                            .and_then(|code| code.as_str())
                            .unwrap_or_default()
                            .chars()
                            .take(200)
                            .collect();
                        format!("ran code in {whom}: {code}")
                    } else {
                        let seconds = args
                            .get("duration_s")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0);
                        format!("{op} of {whom} for {seconds} s")
                    };

                    let level = if op == "exec" { "warning" } else { "info" };
                    self.events
                        .emit(&format!("debug.{op}"), &subject, level, &detail);
                }

                let control = self
                    .running
                    .get(&(app.clone(), id.clone()))
                    .and_then(|running| running.control.as_ref());

                match control {
                    Some(control) => {
                        self.last_request += 1;
                        control.request(self.last_request, &op, worker.as_ref(), &args);
                        self.asked.insert(self.last_request, done);
                    }
                    None if self.starting.contains_key(&(app.clone(), id.clone())) => {
                        let _ = done.send(Err(starting_refusal(&app, &id)));
                    }
                    None => {
                        let _ = done.send(Err(format!(
                            "release {app}@{id} is not running with a host control connection"
                        )));
                    }
                }
            }
            Command::Overlay {
                name,
                action,
                external,
                done,
            } => {
                let _ = done.send(self.control_overlay(&name, action, external).await);
            }
            Command::ScaleTemporary {
                app,
                group,
                min_workers,
                max_workers,
                cap_workers,
                until,
                done,
            } => {
                let change = muru_config::AppOverlay {
                    min_workers,
                    max_workers,
                    cap_workers,
                    ..muru_config::AppOverlay::default()
                };
                let outcome = self.scale_temporary(&app, &group, change, until).await;
                let _ = done.send(outcome);
            }
            Command::OpenDetour {
                opening,
                updates,
                done,
            } => {
                let _ = done.send(self.open_detour(opening, updates).await);
            }
            Command::SetDetourTraffic {
                app,
                release,
                traffic,
                until_unix_ms,
                paths,
                done,
            } => {
                let _ = done.send(
                    self.set_detour_traffic(&app, &release, traffic, until_unix_ms, paths, None)
                        .await,
                );
            }
            Command::PreviewDetour { request, done } => {
                let _ = done.send(self.preview_detour(request).await);
            }
            Command::CloseDetour { serial, reason } => {
                self.close_detour(serial, &reason).await;
            }
            Command::Shutdown { .. } | Command::Upgrade { .. } => {}
        }
    }

    /// Blue/green (section 9.2): record the release, start it, move traffic
    /// once it is ready, drain the release that served before.
    #[allow(clippy::too_many_arguments)]
    async fn deploy(
        &mut self,
        app: &str,
        spec: ReleaseSpec,
        id: Option<String>,
        retain: Retention,
        hold: bool,
        created: oneshot::Sender<Result<Release, String>>,
        done: oneshot::Sender<Result<Release, String>>,
    ) {
        // Section 4.3: refused before anything is started.

        if !hold && let Some(refusal) = crate::config::second_app_refusal(&self.store, app) {
            let _ = created.send(Err(refusal.clone()));
            let _ = done.send(Err(refusal));
            return;
        }

        if let Err(error) = self.make_room(app, &spec).await {
            let _ = created.send(Err(error.clone()));
            let _ = done.send(Err(error));
            return;
        }

        let record = self.create(app, spec, id);

        let release = match record {
            Ok(release) => release,
            Err(error) => {
                let _ = created.send(Err(error.clone()));
                let _ = done.send(Err(error));
                return;
            }
        };

        let _ = created.send(Ok(release.clone()));
        let subject = format!("{app}@{}", release.id);
        self.events.emit(
            "release.created",
            &subject,
            "info",
            &format!("from {}", crate::model::created_from(&release.spec)),
        );

        let id = release.id.clone();

        self.start_then(
            app,
            &id,
            AfterStart::Deploy {
                app: app.to_owned(),
                release: Box::new(release),
                hold,
                retain,
                done,
            },
        )
        .await;
    }

    /// What follows a deploy's start: section 9.2, held, the release runs
    /// and can be previewed, and traffic stays where it is until a rollout
    /// or a promotion; otherwise traffic moves to it and the release that
    /// served drains.
    async fn deployed(
        &mut self,
        app: &str,
        release: Release,
        hold: bool,
        retain: Retention,
    ) -> Result<Release, String> {
        if hold {
            if let Some(running) = self.running.get_mut(&(app.to_owned(), release.id.clone())) {
                running.held = true;
            }

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a held release failed");
            }

            self.mark_ready(app, &release.id, "held: not serving");

            return self
                .store
                .release(app, &release.id)
                .map_err(|error| error.to_string());
        }

        // Traffic moves, then the previous release drains.

        let previous = self
            .store
            .app(app)
            .map_err(|error| error.to_string())?
            .current;
        self.switch_to(app, &release.id).await;
        self.store
            .set_app_releases(app, Some(&release.id), previous.as_deref())
            .map_err(|error| error.to_string())?;

        if let Some(previous) = previous.filter(|previous| previous != &release.id)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), previous.clone()))
        {
            self.drain(app, &previous, retain).await;
        }

        self.store
            .release(app, &release.id)
            .map_err(|error| error.to_string())
    }

    /// Record a release from its spec, with a generated id unless one is given.
    fn create(&self, app: &str, spec: ReleaseSpec, id: Option<String>) -> Result<Release, String> {
        if !valid_name(app) {
            return Err(format!(
                "{app:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters"
            ));
        }

        self.store
            .ensure_app(app)
            .map_err(|error| error.to_string())?;

        let id = match id {
            Some(id) if let Some(refusal) = crate::detours::reserved_release(&id) => {
                return Err(refusal);
            }
            Some(id) if valid_name(&id) => id,
            Some(id) => return Err(format!("{id:?} is not a valid release id")),
            None => self
                .store
                .next_release_id(app)
                .map_err(|error| error.to_string())?,
        };

        if self.store.release(app, &id).is_ok() {
            return Err(format!("release {app}@{id} already exists"));
        }

        // Issues 849 and 850: what the release serves is checked against
        // the application's data directory, and its static directory is
        // made empty.

        crate::places::prepare_release(&self.locations.state.path, &self.store, app, &id, &spec)?;

        self.store
            .create_release(app, &id, &spec)
            .map_err(|error| error.to_string())
    }

    /// What a release's start shares with the tasks it runs (issue 693).
    fn context(&self) -> StartContext {
        StartContext {
            store: Arc::clone(&self.store),
            events: self.events.clone(),
            info: Arc::clone(&self.info),
            locations: self.locations.clone(),
        }
    }

    /// Section 8.3: the runtime library for an interpreter, as a
    /// release's start provides it.
    async fn runtime_library(&self, python: &Path) -> Result<muru_provision::Provided, String> {
        self.context().runtime_library(python).await
    }

    /// Issues 849 and 850: the places a release of the application is
    /// given: its data directory as the application's `[app.NAME]` table
    /// says now, the release's own static directory and, under a root
    /// daemon, the user its media part must belong to.
    fn places(
        &self,
        app: &str,
        id: &str,
        identity: Option<crate::host::Identity>,
    ) -> crate::places::Places {
        let state = &self.locations.state.path;

        crate::places::Places {
            data: self
                .store
                .app(app)
                .ok()
                .map(|record| crate::places::app_data(state, &record)),
            static_dir: Some(crate::places::static_dir(state, app, id)),
            owner: self.service.and(identity).map(|identity| identity.uid),
        }
    }

    /// Make what is missing of a release's places, given to the user it
    /// runs as, share its static directory with the edge, and check what
    /// the release serves against its data directory, which may have
    /// moved since the release was created.
    fn make_places(
        &self,
        spec: &ReleaseSpec,
        places: &crate::places::Places,
        identity: Option<crate::host::Identity>,
    ) -> Result<(), String> {
        if let Some(dirs) = &places.data {
            crate::places::make_data(
                dirs,
                identity.map(|identity| (identity.uid, identity.gid)),
                crate::places::DATA_MODE,
            )?;
        }

        if let Some(dir) = &places.static_dir {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("making {} failed: {error}", dir.display()))?;

            if let (Some(identity), Some(service)) = (identity, self.service) {
                crate::places::share_static(dir, identity.uid, service.gid).map_err(|error| {
                    format!(
                        "sharing the static directory {} with the edge failed: {error}",
                        dir.display()
                    )
                })?;
            }
        }

        crate::places::check_release(spec, places)
    }

    /// The user an application's media part must belong to under a root
    /// daemon, for the static entries the edge serves from it; none in
    /// user mode, where the edge and the application are one user.
    fn media_owner(&self, app: &str) -> Option<u32> {
        let service = self.service?;
        let user = self.store.app(app).ok().and_then(|record| record.user);

        match user {
            Some(user) => crate::host::Identity::lookup(&user, None, None)
                .ok()
                .map(|identity| identity.uid),
            None => Some(service.uid),
        }
    }

    /// Section 8.2: redirected bytecode goes to a directory of the
    /// release's own, writable by its user and removed with it;
    /// precompiled bytecode in the source is used where it is.
    fn pycache_prefix(
        &self,
        app: &str,
        id: &str,
        spec: &ReleaseSpec,
        identity: Option<crate::host::Identity>,
    ) -> Option<PathBuf> {
        if spec.bytecode.effective() != muru_config::Bytecode::Redirect {
            return None;
        }

        let prefix =
            crate::provision::Provisioning::pycache_dir(&self.locations.state.path, app, id);

        match std::fs::create_dir_all(&prefix) {
            Ok(()) => {
                if let Some(identity) = identity {
                    let _ =
                        std::os::unix::fs::chown(&prefix, Some(identity.uid), Some(identity.gid));
                }

                Some(prefix)
            }
            Err(error) => {
                tracing::warn!(%error, "creating {} failed; bytecode is left to Python", prefix.display());
                None
            }
        }
    }

    /// Section 9.12: what a command of a release runs with: the release's
    /// interpreter, environment with its secrets resolved, user and
    /// source directory, as its host has them; for a declared command,
    /// its inputs checked and put in place.
    async fn prepare_command(
        &mut self,
        app: &str,
        id: &str,
        request: crate::commands::CommandRequest,
    ) -> Result<crate::commands::Prepared, String> {
        use crate::commands::CommandRequest;

        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        let spec = &release.spec;
        let subject = format!("{app}@{id}");

        if spec.http.is_some() {
            return Err(format!(
                "{subject} runs an HTTP server rather than workers, so it has no interpreter to run a command with"
            ));
        }

        // The request first, so a bad name or input refuses the run
        // before any secret of the release is read.

        let declared = match &request {
            CommandRequest::OneOff(argv) if argv.is_empty() => {
                return Err("no command was given".to_owned());
            }
            CommandRequest::OneOff(_) => None,
            CommandRequest::Declared {
                name,
                inputs,
                secret_inputs,
            } => {
                let command = crate::commands::find(&spec.commands, name, &subject)?;
                let given = self.given_inputs(inputs, secret_inputs).await?;
                let values = crate::commands::input_values(command, &given)?;
                Some((command.clone(), given, values))
            }
        };

        // A command's secret files are its own, so that its end removes
        // them without touching those of a host still running.

        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let identity = self.resolve_identity(app, &subject)?;

        // Its name is this daemon's start and a count, so that a detached
        // run's, which outlives the daemon, is never another run's.

        let run = COMMAND_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let secrets = run_dir
            .join("commands")
            .join(format!("{}-{run}", self.info.started_unix_ms));

        let env = match resolve_env(&self.secrets, &spec.env, &secrets).await {
            Ok(env) => env,
            Err(message) => {
                let _ = std::fs::remove_dir_all(&secrets);
                return Err(message);
            }
        };

        if let Some(identity) = identity {
            self.own_release_dirs(identity, &run_dir, app, id)?;
            own_secret_files(identity, &secrets)?;
        }

        // A release that builds its environment has it once it has
        // started; a command does not build one.

        let python = if spec.build.builds {
            let python = crate::provision::Provisioning::environment_dir(
                &self.locations.state.path,
                app,
                id,
            )
            .join("bin")
            .join("python");

            if !python.is_file() {
                return Err(format!(
                    "{subject} has no environment built yet; start the release before running a command with it"
                ));
            }

            python
        } else {
            spec.python.clone()
        };

        let library = self.runtime_library(&python).await?;
        let pycache = self.pycache_prefix(app, id, spec, identity);
        let places = self.places(app, id, identity);
        self.make_places(spec, &places, identity)
            .map_err(|message| format!("{subject}: {message}"))?;
        let bytecode = Some(spec.bytecode.effective());
        let base = self.daemon_base(bytecode, identity);
        let env = crate::commands::environment(
            env,
            &crate::commands::Interpreter {
                python: &python,
                runtime: library.pythonpath.as_deref(),
                pycache: pycache.as_deref(),
                bytecode,
                base: Some(&base),
                places: &places.variables(),
            },
            app,
            id,
            &spec.source,
        );

        let prepared = crate::commands::Prepared {
            app: app.to_owned(),
            release: id.to_owned(),
            label: "exec".to_owned(),
            argv: Vec::new(),
            env,
            unset: Vec::new(),
            clean: true,
            cwd: spec.source.clone(),
            identity,
            log: Some(self.host_log(app, id)),
            timeout: None,
            declared: None,
            exclusive: false,
            inputs: String::new(),
            secrets: Some(secrets),
        };

        match (declared, request) {
            (Some((command, given, values)), _) => {
                crate::commands::declared(prepared, &command, &given, &values, &python)
            }
            (None, CommandRequest::OneOff(argv)) => Ok(crate::commands::Prepared {
                argv: crate::commands::with_interpreter(argv, &python),
                ..prepared
            }),
            (None, CommandRequest::Declared { .. }) => {
                unreachable!("a declared command was found above")
            }
        }
    }

    /// Section 9.12: the inputs a run gives, with those named as secret
    /// store entries (STORE/KEY) resolved here, so their values never
    /// pass through the caller.
    async fn given_inputs(
        &self,
        inputs: &BTreeMap<String, String>,
        secret_inputs: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, String> {
        let mut given = inputs.clone();

        for (input, reference) in secret_inputs {
            if given.contains_key(input) {
                return Err(format!("the input {input} is given twice"));
            }

            let Some((store_name, key)) = reference.split_once('/') else {
                return Err(format!(
                    "the input {input} names {reference:?}, which is not STORE/KEY"
                ));
            };
            let store = self
                .secrets
                .iter()
                .find(|store| store.name == store_name)
                .ok_or_else(|| format!("the input {input}: no secret store named {store_name}"))?;
            given.insert(input.clone(), store.resolve(key).await?);
        }

        Ok(given)
    }

    /// Start a release's host and wait until every worker listens
    /// (pending or stopped to starting and warming; or failed). The
    /// caller marks the release ready once the edge has it, with
    /// `mark_ready`, so a client told it is ready can reach it.
    async fn begin_start(&mut self, app: &str, id: &str) -> Result<bool, String> {
        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        let subject = format!("{app}@{id}");

        if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
            // Retained and warm: no restart, and ready again once the
            // caller has given it to the edge.

            running.retained_until = None;
            running.retained_until_unix_ms = 0;
            running.inspected = false;
            return Ok(true);
        }

        self.set_state(
            &release,
            ReleaseState::Starting,
            if release.spec.http.is_some() {
                "starting the server"
            } else {
                "starting the host"
            },
        );
        let spec = &release.spec;
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);

        let identity = match self.resolve_identity(app, &subject) {
            Ok(identity) => identity,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        // Section 10.8: secrets are resolved now, for this start, and
        // delivered as variables or as files only the host can read.

        let env = match resolve_env(&self.secrets, &spec.env, &run_dir.join("secrets")).await {
            Ok(env) => env,
            Err(message) => {
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        let started = Instant::now();

        // The release's directories, its secret files and its log belong
        // to the user the host runs as, so it can bind its sockets,
        // read its secrets and reopen its log.

        if let Some(identity) = identity
            && let Err(message) = self.own_release_dirs(identity, &run_dir, app, id)
        {
            self.set_state(&release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        // Issues 849 and 850: the data directory and the static directory
        // are there, the release's user's, and what it serves is still
        // only media.

        let places = self.places(app, id, identity);

        if let Err(message) = self.make_places(spec, &places, identity) {
            self.set_state(&release, ReleaseState::Failed, &message);
            self.events
                .emit("release.failed", &subject, "error", &message);
            return Err(message);
        }

        // Section 9.9: a managed HTTP release runs its server's
        // instances instead of a host.

        if let Some(http) = spec.http.clone() {
            let env = self.server_environment(env, app, id, spec, identity);

            return self
                .start_servers(app, id, &release, &http, env, identity, &run_dir, started)
                .map(|()| false);
        }

        // Issue 693: the environment is built and the runtime library
        // provided on a task of its own, beside the loop, which carries on
        // in `continue_start` once it reports back.

        let context = self.context();
        let commands = self.commands.clone();
        let (key_app, key_id) = (app.to_owned(), id.to_owned());
        let building = release.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            let built = tokio::select! {
                built = async {
                    let python = context
                        .build_environment(&key_app, &key_id, &building, identity)
                        .await
                        .map_err(|message| (message, "release.build_failed"))?;
                    let library = context
                        .runtime_library(&python)
                        .await
                        .map_err(|message| (message, "release.failed"))?;
                    Ok((python, library))
                } => built,
                _ = cancelled.wait_for(|cancelled| *cancelled) => {
                    Err(("its start was cancelled".to_owned(), "release.failed"))
                }
            };

            let _ = commands
                .send(Command::StartStage {
                    app: key_app,
                    id: key_id,
                    stage: Box::new(StartStage::Built(built)),
                })
                .await;
        });

        self.starting.insert(
            (app.to_owned(), id.to_owned()),
            Starting {
                prep: Some(StartPrep {
                    release,
                    env,
                    identity,
                    run_dir,
                    started,
                    library: None,
                }),
                cancel,
                cancelled: None,
                reserved: 0,
                waiters: Vec::new(),
            },
        );
        Ok(false)
    }

    /// Issue 693: the second stage of a release's start, once its
    /// environment is built and its runtime library provided: the host's
    /// settings, its before_fork gates prepared, and the host started,
    /// whose preload, gates and workers a task waits for.
    async fn continue_start(
        &mut self,
        app: &str,
        id: &str,
        python: PathBuf,
        library: muru_provision::Provided,
    ) -> Result<(), String> {
        let Some(prep) = self
            .starting
            .get_mut(&(app.to_owned(), id.to_owned()))
            .and_then(|starting| starting.prep.take())
        else {
            return Err("its start was cancelled".to_owned());
        };

        let StartPrep {
            release,
            env,
            identity,
            run_dir,
            started,
            ..
        } = prep;
        let subject = format!("{app}@{id}");
        let spec = &release.spec;

        for note in &library.notes {
            let level = if library.how == muru_provision::How::Fallback {
                "warning"
            } else {
                "info"
            };
            self.events
                .emit("release.state_changed", &subject, level, note);
        }

        let pycache_prefix = self.pycache_prefix(app, id, spec, identity);
        let places = self.places(app, id, identity);
        let media = places.media();

        // Section 8.7: the host starts from a clean environment, built
        // once for its host and workers.

        let bytecode = Some(spec.bytecode.effective());
        let base = self.daemon_base(bytecode, identity);
        let env = crate::commands::environment(
            env,
            &crate::commands::Interpreter {
                python: &python,
                runtime: library.pythonpath.as_deref(),
                pycache: pycache_prefix.as_deref(),
                bytecode,
                base: Some(&base),
                places: &places.variables(),
            },
            app,
            id,
            &spec.source,
        );

        // Section 9.12 (issue 266): the before_fork gates still to pass
        // hold the host once it has preloaded.

        let gates = self.gates_due(app, id, &spec.commands, crate::model::Gate::BeforeFork);

        let host_spec = HostSpec {
            python,
            pythonpath: None,
            pycache_prefix: None,
            bytecode: None,
            clean_env: true,
            development: false,
            app: spec.entrypoint.clone(),
            name: Some(app.to_owned()),
            release: Some(id.to_owned()),
            interface: spec.interface,
            factory: spec.factory,
            reflection: spec.reflection,
            handoff: spec.handoff,
            processes: spec.processes as usize,
            threads: spec.threads as usize,
            sync_threads: spec.sync_threads as usize,
            concurrency: spec.concurrency as usize,
            preload: spec.preload,
            hooks: spec.hooks.clone(),
            preload_modules: spec.preload_modules.clone(),
            instrumentation: spec.host_instrumentation(),
            chdir: Some(spec.source.clone()),
            tcp_base_port: None,
            single: false,
            hold_before_fork: !gates.is_empty(),
            detach_to: Some(self.host_log(app, id)),
            relay: self.relay_settings,
            logging: self
                .info
                .python_logging
                .read()
                .expect("python logging poisoned")
                .layered(&spec.logging),
            max_requests: spec.max_requests,
            max_lifetime: spec.max_lifetime_ms.map(Duration::from_millis),
            soft_timeout: spec.timeouts.soft_timeout_ms.map(Duration::from_millis),
            hard_timeout: spec.timeouts.hard_timeout_ms.map(Duration::from_millis),
            hard_drain_grace: Duration::from_millis(spec.timeouts.hard_drain_grace_ms),
            shutdown_timeout: Duration::from_millis(spec.timeouts.shutdown_timeout_ms),
            websocket_drain_grace: Duration::from_millis(spec.timeouts.websocket_drain_grace_ms),
            exit_grace: Duration::from_millis(spec.timeouts.exit_grace_ms),
            loop_stall_timeout: Duration::from_millis(spec.timeouts.loop_stall_timeout_ms),
            gil_stall_timeout: Duration::from_millis(spec.timeouts.gil_stall_timeout_ms),
            memory: spec.memory.clone(),
            cgroup: self
                .cgroup
                .as_ref()
                .filter(|limits| limits.memory_max.is_some())
                .map(|limits| limits.dir.clone()),
            env,
            shown_env: spec
                .env
                .iter()
                .filter_map(crate::model::EnvSpec::shown_name)
                .chain(base.into_iter().map(|(name, _)| name))
                .collect(),
            identity,
            groups: spec
                .groups
                .iter()
                .map(|group| crate::host::HostGroup {
                    name: group.name.clone(),
                    processes: group.processes as usize,
                    threads: group.threads as usize,
                    sync_threads: group.sync_threads as usize,
                    concurrency: group.concurrency as usize,
                    timeouts: group.timeouts.resolve(&spec.timeouts),
                    memory: group.memory.clone().unwrap_or_else(|| spec.memory.clone()),
                    max_requests: group.max_requests.or(spec.max_requests),
                    max_lifetime: group
                        .max_lifetime_ms
                        .or(spec.max_lifetime_ms)
                        .map(Duration::from_millis),
                    optional: group.optional,
                    mounts: group.mounts.clone(),
                })
                .collect(),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| crate::host::HostMount {
                    name: mount.name.clone(),
                    path: mount.path.clone(),
                    entrypoint: mount.entrypoint.clone(),
                    interface: mount.interface,
                    threads: mount.threads,
                    concurrency: mount.concurrency.map(|value| value as usize),
                    max_connections: mount.max_connections.map(|value| value as usize),
                    handoff: mount.handoff,
                    sendfile_roots: muru_config::worker_root_paths(
                        &mount.sendfile_roots,
                        &spec.source,
                        media,
                    ),
                    sendfile_header: mount.sendfile_header.clone(),
                })
                .collect(),
            sendfile_roots: muru_config::worker_root_paths(
                &spec.sendfile_roots,
                &spec.source,
                media,
            ),
            sendfile_header: spec.sendfile_header.clone(),
            owned_roots: media
                .zip(places.owner)
                .map(|(media, owner)| (media.display().to_string(), owner))
                .into_iter()
                .collect(),
        };

        // What the host writes from here on is what says why, if it fails.

        let log = self.host_log(app, id);
        let log_start = std::fs::metadata(&log).map_or(0, |metadata| metadata.len());

        let watcher = self.relay_watcher(&subject);

        let mut host = match HostProcess::start(&host_spec, &run_dir, 1, &self.logs, watcher) {
            Ok(host) => host,
            Err(error) => {
                let message = format!("the host could not be started: {error:#}");
                self.set_state(&release, ReleaseState::Failed, &message);
                self.events
                    .emit("release.failed", &subject, "error", &message);
                return Err(message);
            }
        };

        // The gates' runs are prepared here, with the environment the
        // host has; a gate that cannot be prepared fails the start.

        let mut prepared = Vec::new();

        for gate in &gates {
            let request = crate::commands::CommandRequest::Declared {
                name: gate.name.clone(),
                inputs: BTreeMap::new(),
                secret_inputs: BTreeMap::new(),
            };

            match self.prepare_command(app, id, request).await {
                Ok(mut command) => {
                    command.exclusive = true;
                    prepared.push(command);
                }
                Err(error) => {
                    let message = format!(
                        "the before_fork gate {} could not be started: {error}",
                        gate.name
                    );
                    host.release_gates(false);
                    host.stop(release.spec.timeouts.stop_budget()).await;
                    self.set_state(&release, ReleaseState::Failed, &message);
                    self.events
                        .emit("release.failed", &subject, "error", &message);
                    return Err(message);
                }
            }
        }

        let context = self.context();
        let runs = self.runs.clone();
        let commands = self.commands.clone();
        let key = (app.to_owned(), id.to_owned());

        let mut cancelled = match self.starting.get_mut(&key) {
            Some(starting) => {
                starting.prep = Some(StartPrep {
                    release: release.clone(),
                    env: Vec::new(),
                    identity,
                    run_dir,
                    started,
                    library: Some(library),
                });
                starting.cancel.subscribe()
            }
            None => tokio::sync::watch::channel(true).1,
        };

        tokio::spawn(async move {
            let outcome = wait_for_host(
                &context,
                &runs,
                host,
                &release,
                prepared,
                &log,
                log_start,
                &mut cancelled,
            )
            .await;
            let _ = commands
                .send(Command::StartStage {
                    app: key.0,
                    id: key.1,
                    stage: Box::new(StartStage::Host(outcome)),
                })
                .await;
        });

        Ok(())
    }

    /// Issue 693: the last stage of a release's start, once its workers
    /// listen: the host recorded for the next daemon, taken into what
    /// runs, and announced.
    fn finish_start(&mut self, app: &str, id: &str, host: HostProcess) -> Result<(), String> {
        let Some(prep) = self
            .starting
            .get_mut(&(app.to_owned(), id.to_owned()))
            .and_then(|starting| starting.prep.take())
        else {
            return Err("its start was cancelled".to_owned());
        };

        let StartPrep {
            release,
            run_dir,
            started,
            library,
            ..
        } = prep;
        let library = library.unwrap_or_else(|| muru_provision::Provided {
            pythonpath: None,
            version: String::new(),
            how: muru_provision::How::Environment,
            notes: Vec::new(),
        });
        let subject = format!("{app}@{id}");
        let spec = &release.spec;

        let sockets = host.default_sockets();
        let host_sockets = host.sockets.len();

        // ADR 0033: an optional group whose workers are not all listening
        // yet is named in the event; they join as the host announces them.

        let awaited: Vec<String> = spec
            .groups
            .iter()
            .filter(|group| group.optional)
            .filter(|group| {
                host.group_sockets
                    .iter()
                    .find(|(name, _)| name == &group.name)
                    .is_some_and(|(_, sockets)| sockets.len() < group.processes as usize)
            })
            .map(|group| format!("group {}", group.name))
            .collect();
        let groups: Vec<GroupRecord> = host
            .group_sockets
            .iter()
            .map(|(name, sockets)| {
                let group = spec.groups.iter().find(|group| &group.name == name);

                GroupRecord {
                    name: name.clone(),
                    paths: group.map(|group| group.paths.clone()).unwrap_or_default(),
                    client_classes: group
                        .map(|group| group.client_class.clone())
                        .unwrap_or_default(),
                    hosts: group.map(|group| group.host.clone()).unwrap_or_default(),
                    methods: group.map(|group| group.method.clone()).unwrap_or_default(),
                    optional: group.is_some_and(|group| group.optional),
                    sockets: sockets.clone(),
                }
            })
            .collect();
        write_record(
            &run_dir.join("host.toml"),
            &HostRecord {
                pid: host.pid().unwrap_or(0),
                sockets: host.sockets.clone(),
                groups: groups.clone(),
                runtime: library.pythonpath.clone(),
            },
        );
        let now = Instant::now();
        let pins = self.store.scale_pins(app, id).unwrap_or_default();
        let interface = self.info.interfaces.runs_as(app, id, "", spec.interface);
        let scalers = scalers_for(spec, interface, &sockets, &groups, &pins, now);
        let ready_at = host
            .sockets
            .iter()
            .map(|socket| (socket.clone(), now))
            .collect();
        self.running.insert(
            (app.to_owned(), id.to_owned()),
            Running {
                backend: Backend::Host(host),
                sockets,
                groups,
                retained_until: None,
                retained_until_unix_ms: 0,
                workers: Vec::new(),
                announced: false,
                inspected: false,
                sticky_until: None,
                held: false,
                control: None,
                status: Vec::new(),
                load_ms: None,
                memory_replacements: Vec::new(),
                recycles: BTreeMap::new(),
                exits: BTreeMap::new(),
                signals_from: None,
                scalers,
                host_pss: None,
                idle_since: None,
                ready_at,
            },
        );
        self.attach_control(app, id);
        self.set_state(
            &release,
            ReleaseState::Warming,
            "workers ready, waiting for the edge",
        );
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{} worker{} ready in {:.1} s{}",
                host_sockets,
                if host_sockets == 1 { "" } else { "s" },
                started.elapsed().as_secs_f64(),
                if awaited.is_empty() {
                    String::new()
                } else {
                    format!(", without the optional {}", awaited.join(", "))
                }
            ),
        );
        Ok(())
    }

    /// Section 10.9 and ADR 0026: whom a release runs as. Under a root
    /// daemon that is the application's user, or the service user when
    /// it names none, and never root unless the application says so,
    /// which is warned about; a daemon that is not root reports a
    /// configured user as not applied.
    fn resolve_identity(
        &self,
        app: &str,
        subject: &str,
    ) -> Result<Option<crate::host::Identity>, String> {
        let named = self.store.app(app).ok().and_then(|record| {
            record
                .user
                .clone()
                .map(|user| (user, record.group.clone(), record.umask.clone()))
        });

        match named {
            Some((user, group, umask)) if crate::host::Identity::can_switch() => {
                let mask = umask
                    .as_deref()
                    .and_then(|text| u32::from_str_radix(text, 8).ok());
                let identity = crate::host::Identity::lookup(&user, group.as_deref(), mask)
                    .map_err(|error| format!("the application's user cannot be used: {error:#}"))?;

                if identity.is_root() {
                    self.events.emit(
                        "app.user_is_root",
                        subject,
                        "warning",
                        &format!(
                            "user {user}: the release runs as root, which is not \
                             recommended; a compromised application would have the \
                             node"
                        ),
                    );
                }

                Ok(Some(identity))
            }
            Some((user, _, _)) => {
                self.events.emit(
                    "app.user_not_applied",
                    subject,
                    "warning",
                    &format!("user {user} not applied: the daemon is not root"),
                );
                Ok(None)
            }
            None => Ok(self.service),
        }
    }

    /// Give a release's directories, secret files and log to the user
    /// its processes run as, and open its socket directory to the edge's
    /// group alone (section 10.9), so the edge reaches the sockets
    /// without being root.
    fn own_release_dirs(
        &self,
        identity: crate::host::Identity,
        run_dir: &Path,
        app: &str,
        id: &str,
    ) -> Result<(), String> {
        let owned = [
            run_dir.to_path_buf(),
            run_dir.join("r1"),
            run_dir.join("secrets"),
        ];

        for path in &owned {
            let _ = std::fs::create_dir_all(path);
        }

        let mut paths: Vec<PathBuf> = owned.to_vec();

        if let Ok(entries) = std::fs::read_dir(run_dir.join("secrets")) {
            paths.extend(entries.flatten().map(|entry| entry.path()));
        }

        let log = self.host_log(app, id);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log);
        paths.push(log);

        for path in paths {
            identity.own(&path).map_err(|error| {
                format!(
                    "giving {} to the application's user failed: {error}",
                    path.display()
                )
            })?;
        }

        if let Some(service) = self.service {
            share_sockets(&run_dir.join("r1"), identity, service).map_err(|error| {
                format!("sharing the socket directory with the edge failed: {error}")
            })?;
        }

        Ok(())
    }

    /// Section 9.9: start a managed HTTP release's instances and wait
    /// for their readiness probes, then route to them as an upstream.
    #[allow(clippy::too_many_arguments)]
    fn start_servers(
        &mut self,
        app: &str,
        id: &str,
        release: &Release,
        http: &crate::model::HttpServerSpec,
        env: Vec<(String, String)>,
        identity: Option<crate::host::Identity>,
        run_dir: &Path,
        started: Instant,
    ) -> Result<(), String> {
        let subject = format!("{app}@{id}");
        let log = self.host_log(app, id);
        let grace = Duration::from_millis(release.spec.timeouts.shutdown_timeout_ms);
        let relays = self.server_relays(&subject)?;

        // Issue 693: the instances are started and their probes waited
        // for on a task beside the loop, which `finish_servers` takes on.

        let context = self.context();
        let commands = self.commands.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (task_release, task_http, task_dir) =
            (release.clone(), http.clone(), run_dir.to_path_buf());
        let key = (app.to_owned(), id.to_owned());

        tokio::spawn(async move {
            let release = task_release;
            let subject = format!("{}@{}", release.app, release.id);

            let outcome = async {
                let spawned = Servers::start(
                    &task_http,
                    &task_dir.join("r1"),
                    &release.spec.source,
                    env,
                    identity,
                    &log,
                    Some(relays),
                    grace,
                )
                .await;

                let mut servers = match spawned {
                    Ok(servers) => servers,
                    Err(error) => {
                        return Err(format!("the server could not be started: {error:#}"));
                    }
                };

                context.set_state(&release, ReleaseState::Warming, "waiting for the instances");
                let timeout = Duration::from_millis(task_http.readiness_timeout_ms);

                let ready = tokio::select! {
                    ready = servers.wait_ready(timeout) => ready.map_err(|error| format!("{error:#}")),
                    _ = cancelled.wait_for(|cancelled| *cancelled) => {
                        Err("its start was cancelled".to_owned())
                    }
                };

                match ready {
                    Ok(()) => Ok(servers),
                    Err(message) => {
                        servers.stop(grace).await;
                        Err(message)
                    }
                }
            }
            .await;

            if let Err(message) = &outcome
                && !*cancelled.borrow()
            {
                context.set_state(&release, ReleaseState::Failed, message);
                context
                    .events
                    .emit("release.failed", &subject, "error", message);
            }

            let _ = commands
                .send(Command::StartStage {
                    app: key.0,
                    id: key.1,
                    stage: Box::new(StartStage::Servers(outcome)),
                })
                .await;
        });

        self.starting.insert(
            (app.to_owned(), id.to_owned()),
            Starting {
                prep: Some(StartPrep {
                    release: release.clone(),
                    env: Vec::new(),
                    identity,
                    run_dir: run_dir.to_path_buf(),
                    started,
                    library: None,
                }),
                cancel,
                cancelled: None,
                reserved: 0,
                waiters: Vec::new(),
            },
        );
        Ok(())
    }

    /// Issue 693: a managed server's instances answer their probes: record
    /// them for the next daemon, take them into what runs, and announce
    /// them.
    fn finish_servers(&mut self, app: &str, id: &str, servers: Servers) -> Result<(), String> {
        let Some(prep) = self
            .starting
            .get_mut(&(app.to_owned(), id.to_owned()))
            .and_then(|starting| starting.prep.take())
        else {
            return Err("its start was cancelled".to_owned());
        };

        let StartPrep {
            release,
            run_dir,
            started,
            ..
        } = prep;
        let subject = format!("{app}@{id}");
        let describe = release
            .spec
            .http
            .as_ref()
            .map(crate::model::HttpServerSpec::describe)
            .unwrap_or_default();

        write_record(&run_dir.join("servers.toml"), &servers.record());
        self.running.insert(
            (app.to_owned(), id.to_owned()),
            Running {
                backend: Backend::Servers(servers),
                sockets: Vec::new(),
                groups: Vec::new(),
                retained_until: None,
                retained_until_unix_ms: 0,
                workers: Vec::new(),
                announced: false,
                inspected: false,
                sticky_until: None,
                held: false,
                control: None,
                status: Vec::new(),
                load_ms: None,
                memory_replacements: Vec::new(),
                recycles: BTreeMap::new(),
                exits: BTreeMap::new(),
                signals_from: None,
                scalers: Vec::new(),
                host_pss: None,
                idle_since: None,
                ready_at: HashMap::new(),
            },
        );
        self.publish_servers();
        self.set_state(
            &release,
            ReleaseState::Warming,
            "instances ready, waiting for the edge",
        );
        self.events.emit(
            "release.ready",
            &subject,
            "info",
            &format!(
                "{describe} ready in {:.1} s",
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(())
    }

    /// Issue 693: start a release beside the loop, then carry out `then`
    /// with how the start went. A release already running needs no start
    /// and `then` runs at once; one already starting gives `then` the
    /// same outcome; and a release of an application another of whose
    /// releases is starting waits its turn, since two would compete for
    /// the same memory and migrations. Releases of different applications
    /// start side by side.
    async fn start_then(&mut self, app: &str, id: &str, then: AfterStart) {
        let key = (app.to_owned(), id.to_owned());

        // Issue 693: a daemon that is stopping starts nothing.

        if self
            .info
            .stopping
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.after_start(then, Err("the daemon is stopping".to_owned()))
                .await;
            return;
        }

        // Issue 699: a release being stopped is started again once its
        // stop has ended.

        if let Some(stop) = self.stops.get_mut(&key) {
            stop.waiters.push(AfterStop::Start(then));
            return;
        }

        if let Some(starting) = self.starting.get_mut(&key) {
            starting.waiters.push(then);
            return;
        }

        if self.starting.keys().any(|(starting, _)| starting == app) {
            self.start_queue
                .entry(app.to_owned())
                .or_default()
                .push_back((id.to_owned(), then));
            return;
        }

        // The memory the release will hold counts against the budget from
        // now, so two starts cannot each fit and together overshoot it.

        let reserved = self
            .store
            .release(app, id)
            .ok()
            .and_then(|release| {
                self.budget_view(self.budget_usage())
                    .release_memory(app, &release.spec)
            })
            .unwrap_or(0);

        match self.begin_start(app, id).await {
            Ok(true) => self.after_start(then, Ok(())).await,
            Ok(false) => {
                if let Some(starting) = self.starting.get_mut(&key) {
                    starting.reserved = reserved;
                    starting.waiters.push(then);
                }
            }
            Err(error) => {
                self.after_start(then, Err(error)).await;
                self.start_next(app).await;
            }
        }
    }

    /// Issue 693: carry out what followed a release's start in the
    /// operation that started it, with how the start went.
    async fn after_start(&mut self, then: AfterStart, started: Result<(), String>) {
        match then {
            AfterStart::Restore { app, id } => match started {
                Ok(()) => self.switch_to(&app, &id).await,
                Err(error) => self.events.emit(
                    "release.failed",
                    &format!("{app}@{id}"),
                    "error",
                    &format!("could not restart after the daemon restarted: {error}"),
                ),
            },
            AfterStart::Deploy {
                app,
                release,
                hold,
                retain,
                done,
            } => {
                let outcome = match started {
                    Ok(()) => self.deployed(&app, *release, hold, retain).await,
                    Err(error) => Err(error),
                };

                let _ = done.send(outcome);
            }
            AfterStart::StartRollout {
                app,
                candidate,
                steps,
                gates,
                approve_each_step,
                done,
            } => {
                let outcome = match started {
                    Ok(()) => {
                        self.rollout_started(&app, &candidate, steps, gates, approve_each_step)
                            .await
                    }
                    Err(error) => Err(error),
                };

                let _ = done.send(outcome);
            }
            AfterStart::Promote {
                app,
                id,
                retain,
                then,
            } => match started {
                Ok(()) => Box::pin(self.promote_started(&app, &id, retain, then)).await,
                Err(error) => Box::pin(self.promoted(then, Err(error))).await,
            },
            AfterStart::Held { app, id, done } => {
                let outcome = match started {
                    Ok(()) => self.held_started(&app, &id).await,
                    Err(error) => Err(error),
                };

                let _ = done.send(outcome);
            }
            AfterStart::Referenced { app, id, warming } => {
                self.referenced_started(&app, &id, warming, started).await;
            }
            AfterStart::Wake { app, id } => {
                let subject = format!("{app}@{id}");

                match started {
                    Ok(()) => {
                        if let Err(error) = self.push_snapshot().await {
                            tracing::error!(%error, "pushing the snapshot for a woken release failed");
                        }

                        self.mark_ready(&app, &id, "woken by a request");
                        self.make_worker_room(&subject).await;
                    }
                    Err(error) => {
                        self.events
                            .emit("release.failed", &subject, "error", &error);
                    }
                }
            }
            AfterStart::Rollback {
                app,
                previous,
                current,
                done,
            } => {
                let outcome = match started {
                    Ok(()) => Box::pin(self.rolled_back(&app, &previous, current)).await,
                    Err(error) => Err(error),
                };

                let _ = done.send(outcome);
            }
            AfterStart::Restart { app, id } => {
                if started.is_ok() {
                    self.switch_to(&app, &id).await;
                }
            }
        }
    }

    /// Issue 693: a stage of a release's start has reported: go on to the
    /// next, or end the start.
    async fn start_stage(&mut self, app: &str, id: &str, stage: StartStage) {
        let key = (app.to_owned(), id.to_owned());

        if let Some(why) = self
            .starting
            .get(&key)
            .and_then(|starting| starting.cancelled.clone())
        {
            self.start_cancelled(app, id, stage, &why).await;
            return;
        }

        let outcome = match stage {
            StartStage::Built(Ok((python, library))) => {
                match self.continue_start(app, id, python, library).await {
                    Ok(()) => return,
                    Err(error) => Err(error),
                }
            }
            StartStage::Built(Err((message, kind))) => {
                if let Ok(release) = self.store.release(app, id) {
                    self.set_state(&release, ReleaseState::Failed, &message);
                }

                self.events
                    .emit(kind, &format!("{app}@{id}"), "error", &message);
                Err(message)
            }
            StartStage::Host(Ok(host)) => self.finish_start(app, id, host),
            StartStage::Servers(Ok(servers)) => self.finish_servers(app, id, servers),
            StartStage::Host(Err(message)) | StartStage::Servers(Err(message)) => Err(message),
        };

        self.end_start(app, id, outcome).await;
    }

    /// Issue 693: a cancelled start has reported: stop what it started, if
    /// anything, record the release stopped unless whoever cancelled it
    /// has already, and end the start.
    async fn start_cancelled(&mut self, app: &str, id: &str, stage: StartStage, why: &str) {
        let subject = format!("{app}@{id}");

        match stage {
            StartStage::Host(Ok(host)) => {
                let grace = self
                    .store
                    .release(app, id)
                    .map_or(Duration::from_secs(10), |release| {
                        release.spec.timeouts.stop_budget()
                    });
                host.stop(grace).await;
            }
            StartStage::Servers(Ok(servers)) => {
                let grace = self
                    .store
                    .release(app, id)
                    .map_or(Duration::from_secs(10), |release| {
                        Duration::from_millis(release.spec.timeouts.shutdown_timeout_ms)
                    });
                servers.stop(grace).await;
            }
            _ => {}
        }

        let message = format!("its start was cancelled: {why}");

        if let Ok(release) = self.store.release(app, id)
            && (release.state.is_running()
                || matches!(
                    release.state,
                    ReleaseState::Pending | ReleaseState::Building
                ))
        {
            self.set_state(&release, ReleaseState::Stopped, &message);
            self.events
                .emit("release.stopped", &subject, "info", &message);
        }

        self.end_start(app, id, Err(message)).await;
    }

    /// Issue 693: a release's start has ended: carry out what was waiting
    /// on it, then the next start of its application.
    async fn end_start(&mut self, app: &str, id: &str, outcome: Result<(), String>) {
        let Some(starting) = self.starting.remove(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        for then in starting.waiters {
            self.after_start(then, outcome.clone()).await;
        }

        self.start_next(app).await;
    }

    /// Issue 693: the next start of an application waiting for its turn.
    async fn start_next(&mut self, app: &str) {
        if self.starting.keys().any(|(starting, _)| starting == app) {
            return;
        }

        let next = self
            .start_queue
            .get_mut(app)
            .and_then(std::collections::VecDeque::pop_front);

        if self
            .start_queue
            .get(app)
            .is_some_and(std::collections::VecDeque::is_empty)
        {
            self.start_queue.remove(app);
        }

        if let Some((id, then)) = next {
            Box::pin(self.start_then(app, &id, then)).await;
        }
    }

    /// Issue 693: cancel a release's start under way, which stops its host
    /// if one has started; what waits on it is told the start failed. A
    /// start waiting for its turn is dropped, failed the same way. Gives
    /// whether there was one.
    async fn cancel_start(&mut self, app: &str, id: &str, why: &str) -> bool {
        let mut found = false;

        if let Some(starting) = self.starting.get_mut(&(app.to_owned(), id.to_owned())) {
            let _ = starting.cancel.send(true);
            starting.cancelled = Some(why.to_owned());
            found = true;
        }

        let queued: Vec<AfterStart> = match self.start_queue.get_mut(app) {
            Some(queue) => {
                let (dropped, kept): (Vec<_>, Vec<_>) =
                    queue.drain(..).partition(|(queued, _)| queued == id);
                queue.extend(kept);
                dropped.into_iter().map(|(_, then)| then).collect()
            }
            None => Vec::new(),
        };

        for then in queued {
            found = true;
            self.after_start(then, Err(format!("its start was cancelled: {why}")))
                .await;
        }

        found
    }

    /// Issue 693: cancel every start under way or waiting for its turn.
    async fn cancel_starts(&mut self, why: &str) {
        let mut keys: Vec<(String, String)> = self.starting.keys().cloned().collect();

        for (app, queue) in &self.start_queue {
            keys.extend(queue.iter().map(|(id, _)| (app.clone(), id.clone())));
        }

        for (app, id) in keys {
            self.cancel_start(&app, &id, why).await;
        }
    }

    /// Point the edge at a release's workers: the application's serving
    /// release in the snapshot, and the default workers when no host is
    /// declared.
    async fn switch_to(&mut self, app: &str, id: &str) {
        let subject = format!("{app}@{id}");

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return;
        }

        self.serving.insert(app.to_owned(), id.to_owned());

        if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
            running.held = false;
        }

        let pushed = self.push_snapshot().await;
        self.mark_ready(app, id, "");

        match pushed {
            Ok(()) => {
                self.events.emit(
                    "traffic.switched",
                    &subject,
                    "notice",
                    "traffic moved to the release",
                );
            }
            Err(error) => {
                self.events.emit(
                    "release.failed",
                    &subject,
                    "error",
                    &format!("the edge could not take the release: {error}"),
                );
            }
        }
    }

    /// Section 4.3: say once when several applications serve with no
    /// host to choose between them, and when that ends, and keep what
    /// `muructl status` shows.
    fn note_no_host(&mut self, serving: BTreeSet<String>) {
        if serving == self.no_host {
            return;
        }

        let names = serving.iter().cloned().collect::<Vec<_>>().join(", ");
        *self.info.no_host.lock().expect("no_host poisoned") = names.clone();

        if !serving.is_empty() {
            self.events.emit(
                "routing.no_host",
                "edge",
                "warning",
                &format!(
                    "{names} serve and no hosts are declared, so every request is answered 421; \
                     declare hosts and routes for them"
                ),
            );
        }

        self.no_host = serving;
    }

    /// Section 4.4: desired state compiled for the edge. Each application
    /// contributes the workers of its serving release when that release
    /// runs; the hosts carry their certificates read from disk, so a
    /// replaced file takes effect with the next snapshot; the default
    /// workers are those of the application traffic was last switched to.
    fn build_snapshot(&mut self) -> pb::Snapshot {
        let mut apps = Vec::new();

        // Section 9.10: the routes and the applications in maintenance
        // mode with the active overlays laid over them.

        let (routes, maintained) = self.effective();
        let rollouts: Vec<Rollout> = self
            .store
            .rollouts(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|rollout| rollout.state.is_active())
            .collect();
        let mut referenced = referenced_in(&routes);
        referenced.extend(
            rollouts
                .iter()
                .map(|rollout| (rollout.app.clone(), rollout.candidate.clone())),
        );

        // Section 9.4: an application's serving release takes requests,
        // and so do the releases its routes' policies name and a release
        // sticky clients may still reach, while they run and are not
        // merely kept warm.

        let mut routable: Vec<(&String, &String)> = self
            .running
            .iter()
            .filter(|((app, id), running)| {
                self.serving.get(app) == Some(id)
                    || running.sticky_until.is_some()
                    || running.held
                    || (running.retained_until.is_none()
                        && referenced.contains(&((*app).clone(), (*id).clone())))
            })
            .map(|((app, id), _)| (app, id))
            .collect();
        routable.sort();
        let mut included: BTreeSet<(String, String)> = routable
            .iter()
            .map(|(app, id)| ((*app).clone(), (*id).clone()))
            .collect();

        // Section 9.13: a detour takes what its overlay gives it.

        included.extend(self.detours.keys().cloned());

        for (name, id) in routable {
            if let Some(running) = self.running.get(&(name.clone(), id.clone())) {
                // Section 11.2: the application's own access log choice,
                // which the edge lays over its host's and the node's.

                let access_log = self
                    .store
                    .app(name)
                    .ok()
                    .filter(|app| !app.access_log.is_empty())
                    .map(|app| app.access_log.choice().to_message());

                // Section 4.6: the limits the release, its groups and its
                // mounts set on the requests they serve.

                let spec = self
                    .store
                    .release(name, id)
                    .ok()
                    .map(|release| release.spec);
                let group_limits = |group: &str| {
                    spec.as_ref()
                        .and_then(|spec| spec.groups.iter().find(|own| own.name == group))
                        .and_then(|own| own.limits.message())
                };

                apps.push(pb::AppWorkers {
                    preview_secret: String::new(),
                    detour_paths: Vec::new(),
                    limits: spec.as_ref().and_then(|spec| spec.limits.message()),
                    app: name.clone(),
                    release: id.clone(),
                    serving: self.serving.get(name) == Some(id),
                    sockets: running
                        .sockets
                        .iter()
                        .map(|socket| socket.display().to_string())
                        .collect(),
                    waking_groups: running.waking_groups(),
                    access_log,
                    grpc_web_origins: self
                        .store
                        .release(name, id)
                        .map(|release| release.spec.grpc_web_origins)
                        .unwrap_or_default(),
                    static_dirs: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            let places = crate::places::Places {
                                owner: self.media_owner(name),
                                ..self.places(name, id, None)
                            };

                            release
                                .spec
                                .static_dirs
                                .iter()
                                .filter_map(|entry| entry.message(&release.spec.source, &places))
                                .collect()
                        })
                        .unwrap_or_default(),
                    groups: running
                        .groups
                        .iter()
                        .map(|group| pb::Group {
                            limits: group_limits(&group.name),
                            name: group.name.clone(),
                            paths: group.paths.clone(),
                            client_classes: group.client_classes.clone(),
                            hosts: group.hosts.clone(),
                            methods: group.methods.clone(),
                            sockets: group
                                .sockets
                                .iter()
                                .map(|socket| socket.display().to_string())
                                .collect(),
                        })
                        .collect(),
                    mounts: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            release
                                .spec
                                .mounts
                                .iter()
                                .map(|mount| pb::Mount {
                                    limits: mount.limits.message(),
                                    name: mount.name.clone(),
                                    path: mount.path.clone(),
                                    internal: mount.internal,
                                    accept_redirects_from: mount.accept_redirects_from.clone(),
                                    accel_redirect: mount.accel_redirect,
                                    cgi_local_redirect: mount.cgi_local_redirect,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    accel_redirect: self
                        .store
                        .release(name, id)
                        .is_ok_and(|release| release.spec.accel_redirect),
                    cgi_local_redirect: self
                        .store
                        .release(name, id)
                        .is_ok_and(|release| release.spec.cgi_local_redirect),
                    error_pages: self
                        .store
                        .release(name, id)
                        .map(|release| {
                            error_pages_message(
                                &release.spec.error_pages,
                                Some(&release.spec.source),
                            )
                        })
                        .unwrap_or_default(),
                    error_override: self
                        .store
                        .release(name, id)
                        .ok()
                        .and_then(|release| release.spec.error_override)
                        .map(|table| error_override_message(&table)),
                    maintenance: Some(maintenance_message(
                        maintained.contains(name),
                        self.store
                            .release(name, id)
                            .ok()
                            .and_then(|release| release.spec.maintenance)
                            .as_ref(),
                    )),
                    server: running.backend.server_message(name, id).map(|mut server| {
                        // Section 4.9: the release's file hint keys apply
                        // to the server's responses, which are trusted.

                        if let Ok(release) = self.store.release(name, id) {
                            server.sendfile_roots = muru_config::sendfile_root_paths(
                                &release.spec.sendfile_roots,
                                &release.spec.source,
                                self.places(name, id, None).media(),
                            );
                            server.sendfile_header =
                                release.spec.sendfile_header.clone().unwrap_or_default();
                        }

                        server
                    }),
                });
            }
        }

        // Section 9.13: each detour is a release of its application.

        apps.extend(self.detour_apps(&maintained));

        // Section 4.3: with no host declared, the one application that
        // serves takes every request. Nothing is chosen between several,
        // and nothing is kept, so a restart cannot move the traffic.

        let no_hosts = self.store.hosts().unwrap_or_default().is_empty();
        let serving: BTreeSet<String> = apps
            .iter()
            .filter(|app| app.serving)
            .map(|app| app.app.clone())
            .collect();
        let no_host = no_hosts && serving.len() > 1;
        let default = (no_hosts && serving.len() == 1)
            .then(|| apps.iter().find(|app| app.serving))
            .flatten();
        self.note_no_host(if no_host { serving } else { BTreeSet::new() });
        let worker_sockets = default.map(|app| app.sockets.clone()).unwrap_or_default();
        let default_app = default.map(|app| app.app.clone()).unwrap_or_default();
        let default_release = default.map(|app| app.release.clone()).unwrap_or_default();

        // Section 4.2: a host applied since the last snapshot has its
        // certificate files read now; the others carry their last good
        // pair.

        self.refresh_certificates(false);
        let mut hosts = Vec::new();

        for (name, host) in self.store.hosts().unwrap_or_default() {
            let (certificate_pem, key_pem) =
                self.certificates.get(&name).cloned().unwrap_or_default();

            // Sections 4.6 and 11.2: the host's limits on a request and
            // its access log choice, which were checked when applied.

            let limits = muru_config::check_request_limits(
                host.max_body_size.as_deref(),
                host.timeouts.as_ref(),
            )
            .ok()
            .and_then(|limits| request_limits_message(&limits));
            let mut choice = AccessLogSpec::from_host(&host).choice();
            choice.format = host
                .access_log_format
                .as_deref()
                .and_then(muru_edge::access::Format::parse);
            choice.file = host
                .access_log_file
                .as_ref()
                .map(|file| Arc::from(self.locations.logs.path.join(file).as_path()));
            choice.query = host
                .access_log_query
                .as_deref()
                .and_then(muru_edge::access::Query::parse);
            let access_log = (!choice.is_empty()).then(|| choice.to_message());

            // Section 4.12: the host's maintenance mode, with its
            // exemptions. A host removed since it was turned on is no
            // longer in it.

            let maintenance = self
                .host_maintenance
                .contains_key(&name)
                .then(|| maintenance_message(true, host.maintenance.as_ref()));

            hosts.push(pb::Host {
                maintenance,
                name,
                limits,
                access_log,
                request_headers: host.request_headers.as_ref().map(request_headers_message),
                response_headers: host.response_headers.as_ref().map(response_headers_message),
                compression: host
                    .compression
                    .as_ref()
                    .and_then(|table| compression_choice(table).ok()),
                canonical_redirect: host.canonical_redirect,
                names: host.names,
                listeners: host.listeners,
                certificate_pem,
                key_pem,
                https_redirect: host.https_redirect,
                canonical: host.canonical.unwrap_or_default(),
                use_canonical_name: host.use_canonical_name,
                default: host.default,
                error_pages: error_pages_message(&host.error_pages, None),
                error_override: host.error_override.as_ref().map(error_override_message),
                rate_limits: host.rate_limits,
            });
        }

        let mut upstreams = Vec::new();

        for (name, upstream) in self.store.upstreams().unwrap_or_default() {
            match upstream_message(&name, &upstream) {
                Ok(message) => upstreams.push(message),
                Err(error) => self.events.emit(
                    "upstream.unusable",
                    &format!("upstream {name}"),
                    "error",
                    &format!("{error:#}; routes to it answer 502 until it is applied again"),
                ),
            }
        }

        let routes = routes
            .into_iter()
            .map(|route| pb::Route {
                upload: upload_message(&route),
                request_headers: route.request_headers.as_ref().map(request_headers_message),
                response_headers: route
                    .response_headers
                    .as_ref()
                    .map(response_headers_message),
                compression: route
                    .compression
                    .as_ref()
                    .and_then(|table| compression_choice(table).ok()),
                limits: muru_config::check_request_limits(
                    route.max_body_size.as_deref(),
                    route.timeouts.as_ref(),
                )
                .ok()
                .and_then(|limits| request_limits_message(&limits)),
                traffic: self.route_traffic(&route, &included, &rollouts),
                client_classes: route.classes().to_vec(),
                strip_prefix: muru_config::stripped_part(route.strip_prefix.as_deref()).to_owned(),
                host: route.host,
                path: route.path,
                app: route.app,
                upstream: route.upstream,
                fallback_upstream: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.upstream.clone())
                    .unwrap_or_default(),
                static_dir: route.static_dir.unwrap_or_default(),
                index: route.index,
                fallback_app: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.app.clone())
                    .unwrap_or_default(),
                fallback_file: route
                    .fallback
                    .as_ref()
                    .and_then(|fallback| fallback.file.clone())
                    .unwrap_or_default(),
                cache_max_age_s: route
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.max_age.as_deref())
                    .and_then(|text| muru_config::parse_duration(text).ok())
                    .map_or(0, |duration| duration.as_secs()),
                cache_immutable: route
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.immutable)
                    .unwrap_or(false),
                cache_except: route
                    .cache
                    .as_ref()
                    .map(|cache| cache.except.clone())
                    .unwrap_or_default(),
                no_redirect_slash: route.redirect_slash == Some(false),
                case_insensitive: route.case.as_deref() == Some("insensitive"),
                internal: route.internal == Some(true),
                accept_redirects_from: route.accept_redirects_from,
                error_pages: error_pages_message(&route.error_pages, None),
                error_override: route.error_override.as_ref().map(error_override_message),
                auth: route.auth.as_ref().map(auth_message),
                rate_limits: route.rate_limits,
                methods: route.methods.as_ref().map(|methods| pb::Methods {
                    deny: methods.deny.clone(),
                    not_allowed: methods.answer.as_deref() == Some("method_not_allowed"),
                }),
                mode: route.mode.unwrap_or_default(),
            })
            .collect();

        // Section 9.3: with no host declared, the default application's
        // rollout chooses among its releases as a route's would.

        let default_traffic = if hosts.is_empty() && !default_app.is_empty() {
            let mut route = muru_config::Route {
                app: default_app.clone(),
                ..muru_config::Route::default()
            };
            crate::detours::lay_over(&mut route, &self.detour_takings());
            self.route_traffic(&route, &included, &rollouts)
        } else {
            None
        };

        pb::Snapshot {
            version: 0,
            worker_sockets,
            hosts,
            routes,
            apps,
            proxy: self.proxy.clone(),
            compression: Some(self.compression.clone()),
            error_pages: self.error_pages.clone(),
            error_override: self.error_override.clone(),
            upstreams,
            default_app,
            default_release,
            client_classes: self.clients.classes_message.clone(),
            rate_limits: self.clients.limits_message.clone(),
            default_traffic,
            no_host,
            h2c: self.h2c.clone(),
            listener_limits: self.listener_limits.clone(),
            listener_proxies: self.listener_proxies.clone(),
            limits: Some(self.limits.to_message()),
            access_log: Some(self.access_log.to_message()),
            access_log_overrides: self
                .access_overrides
                .iter()
                .map(muru_edge::access::Override::to_message)
                .collect(),
            detours: self.detours_enabled && self.locations.mode != crate::locations::Mode::System,
        }
    }

    /// Compile and push a snapshot. A worker the edge could not connect to
    /// (section 5.1) is reported as an event and replaced at once, as
    /// `muructl restart --force` would: the snapshot has been applied
    /// without it, and a worker that accepts a connection and never
    /// answers is not going to.
    async fn push_snapshot(&mut self) -> Result<(), String> {
        let snapshot = self.build_snapshot();
        let applied = self
            .edge
            .apply(snapshot)
            .await
            .map_err(|error| format!("{error:#}"))?;
        self.replace_unreachable(applied);
        Ok(())
    }

    /// Section 5.1: report each worker the edge's answer to a snapshot
    /// names as one it could not connect to, and replace it at once.
    fn replace_unreachable(&mut self, applied: pb::Applied) {
        for worker in applied.unreachable {
            let subject = format!("{}@{}", worker.app, worker.release);
            let running = self
                .running
                .get(&(worker.app.clone(), worker.release.clone()));

            // The host announced which worker listens on each socket.

            let name = running.and_then(|running| {
                running
                    .workers
                    .iter()
                    .find(|announced| announced.socket == worker.socket)
                    .map(|announced| {
                        muru_common::worker::WorkerName::new(&announced.group, announced.index)
                    })
            });
            let whom = name.as_ref().map_or_else(
                || format!("the worker at {}", worker.socket),
                |name| format!("worker {name}"),
            );
            let control = running.and_then(|running| running.control.as_ref());

            let action = match (&name, control) {
                (Some(name), Some(control)) => {
                    self.last_request += 1;
                    control.request(
                        self.last_request,
                        "restart",
                        Some(name),
                        &serde_json::json!({ "force": true }),
                    );
                    "; it is being replaced"
                }
                _ => "",
            };

            self.events.emit(
                "worker.unreachable",
                &subject,
                "warning",
                &format!(
                    "{whom} did not answer the edge ({}); the snapshot was applied without it{action}",
                    worker.reason
                ),
            );
        }
    }

    /// Section 9.5: with traffic already elsewhere, wait for the release's
    /// requests in flight to finish, then keep it warm for `retain` (section
    /// 9.1) or stop its host.
    async fn drain(&mut self, app: &str, id: &str, retain: Retention) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            if release.state.is_running() {
                self.set_state(&release, ReleaseState::Stopped, "");
            }

            return;
        }

        // Section 9.4: with a sticky policy on the application's routes,
        // clients whose cookie names the release keep reaching it for the
        // grace period; it drains when that passes (expire_sticky).

        if let Some(grace) = self.sticky_grace(app)
            && let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned()))
            && running.sticky_until.is_none()
        {
            running.sticky_until = Some((Instant::now() + grace, retain));
            let message = format!(
                "sticky clients keep it for {}",
                muru_config::format_duration(grace)
            );
            self.set_state(&release, ReleaseState::Draining, &message);
            self.events
                .emit("release.draining", &subject, "info", &message);

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a sticky drain failed");
            }

            return;
        }

        self.drain_now(app, id, retain).await;
    }

    /// Drain a release that is out of the snapshot: wait for its requests
    /// in flight, then keep it warm for `retain` or stop it.
    async fn drain_now(&mut self, app: &str, id: &str, retain: Retention) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        // The previous pool is gone from the edge, so what is in flight on
        // these workers is what the edge had already dispatched; a short
        // wait lets it finish.

        self.set_state(
            &release,
            ReleaseState::Draining,
            "requests in flight are finishing",
        );
        self.events
            .emit("release.draining", &subject, "info", "draining");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // One retained release per application: an older one makes way,
        // unless this one is kept for inspection only.

        if !retain.lasts.is_zero() {
            let older: Vec<String> = self
                .running
                .iter()
                .filter(|((other_app, other_id), running)| {
                    retain.slot
                        && other_app == app
                        && other_id != id
                        && running.retained_until.is_some()
                })
                .map(|((_, other_id), _)| other_id.clone())
                .collect();

            for other in older {
                self.stop_release(app, &other, "superseded again");
            }

            // Section 9.1: once drained, each group keeps its warm floor;
            // workers fork from the preloaded host when it serves again.

            let mut floors = Vec::new();
            let mut kept = Kept::Workers(Vec::new());
            let until = crate::model::now_unix_ms() + retain.lasts.as_millis() as u64;

            if let Some(running) = self.running.get_mut(&(app.to_owned(), id.to_owned())) {
                running.retained_until = Some(Instant::now() + retain.lasts);
                running.retained_until_unix_ms = until;
                running.inspected = !retain.slot;
                let host = running.backend.host().is_some();
                let mut counts = Vec::new();

                for group in &mut running.scalers {
                    let held = running
                        .groups
                        .iter()
                        .find(|record| record.name == group.name)
                        .map_or(running.sockets.len(), |record| record.sockets.len())
                        as u32;
                    let keep = retain
                        .workers
                        .unwrap_or_else(|| retained_by_default(group.scaler.spec()));

                    if held > keep && host {
                        group.requested = Some(keep);
                        floors.push((group.name.clone(), keep));
                    }

                    counts.push((group.name.clone(), if host { held.min(keep) } else { held }));
                }

                kept = match running.backend {
                    Backend::Dormant => Kept::Idle,
                    Backend::Servers(_) => Kept::Instances,
                    Backend::Host(_) => Kept::Workers(counts),
                };
            }

            for (group, keep) in floors {
                self.ask_scale(app, id, &group, keep);
            }

            // Section 9.1: the note says what a rollback to the release
            // would cost, from what is kept. A retained release is not
            // scaled or idled, so the note holds until it is stopped.

            let lasts = muru_config::format_duration(retain.lasts);

            let (note, event) = if retain.slot {
                (kept.note(&clock(until)), kept.event(&lasts))
            } else {
                (
                    format!(
                        "kept for inspection until {}; not a rollback target",
                        clock(until)
                    ),
                    format!(
                        "kept for inspection for {lasts} {}; not a rollback target",
                        kept.with()
                    ),
                )
            };

            self.set_state(&release, ReleaseState::Retained, &note);
            self.events
                .emit("release.retained", &subject, "info", &event);
            return;
        }

        self.stop_release(app, id, "");
    }

    /// Stop a release's host and record it stopped. Issue 699: the release
    /// leaves what runs at once, and its processes are stopped on a task
    /// beside the loop, which `stopped` takes on when they have.
    fn stop_release(&mut self, app: &str, id: &str, why: &str) {
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };
        let Some(running) = self.running.remove(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        self.set_state(
            &release,
            ReleaseState::Stopping,
            if running.backend.servers().is_some() {
                "stopping the instances"
            } else {
                "stopping the host"
            },
        );
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let record = running.backend.record_path(&run_dir);
        let grace = release.spec.timeouts.stop_budget();
        let wait = stop_wait(grace);
        let backend = running.backend;
        let commands = self.commands.clone();
        let (task_app, task_id, task_why) = (app.to_owned(), id.to_owned(), why.to_owned());

        tokio::spawn(async move {
            let timed_out = tokio::time::timeout(wait, backend.stop(grace))
                .await
                .is_err();

            if timed_out {
                tracing::warn!(%subject, "the host did not stop within the drain timeout");
            }

            let _ = commands
                .send(Command::Stopped {
                    app: task_app,
                    id: task_id,
                    why: task_why,
                    timed_out,
                })
                .await;
        });

        self.stops.insert(
            (app.to_owned(), id.to_owned()),
            Stop {
                until: Instant::now() + wait,
                run_dir,
                record,
                waiters: Vec::new(),
            },
        );
    }

    /// Issue 699: a release's processes have stopped, or outlasted the
    /// wait for them: forget its run directory, record it stopped, and
    /// carry out what was waiting on the stop.
    async fn stopped(&mut self, app: &str, id: &str, why: &str, timed_out: bool) {
        let subject = format!("{app}@{id}");
        let Some(stop) = self.stops.remove(&(app.to_owned(), id.to_owned())) else {
            return;
        };

        forget_release_dir(&stop.run_dir, &stop.record);
        let _ = self.store.remove_scale_pins(app, id, None);

        if let Ok(release) = self.store.release(app, id) {
            self.set_state(&release, ReleaseState::Stopped, why);

            let detail = if timed_out {
                "stopped; its processes outlasted the wait for them"
            } else {
                "stopped"
            };
            self.events
                .emit("release.stopped", &subject, "info", detail);
        }

        for then in stop.waiters {
            match then {
                AfterStop::Drained(done) => {
                    let _ = done.send(
                        self.store
                            .release(app, id)
                            .map_err(|error| error.to_string()),
                    );
                }
                AfterStop::Start(then) => Box::pin(self.start_then(app, id, then)).await,
                AfterStop::RemoveApp(app, done) => Box::pin(self.remove_app_then(app, done)).await,
            }
        }
    }

    /// Section 9.3: begin a rollout of `candidate` against the release that
    /// serves. The candidate is started if it is not running, takes the
    /// first step's share at once, and the ticks move it along.
    async fn check_rollout(
        &mut self,
        app: &str,
        candidate: &str,
        steps: &[RolloutStep],
        gates: &RolloutGates,
    ) -> Result<(), String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let Some(baseline) = record.current.clone() else {
            return Err(format!("{app} has no serving release to roll out against"));
        };

        if baseline == candidate {
            return Err(format!("{app}@{candidate} already serves"));
        }

        self.store
            .release(app, candidate)
            .map_err(|error| error.to_string())?;

        if let Some(active) = self
            .store
            .active_rollout(app)
            .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "{app} has rollout {} under way ({}); promote or abort it first",
                active.id,
                active.state.as_str()
            ));
        }

        if steps.is_empty() {
            return Err("a rollout needs at least one step".to_owned());
        }

        for url in &gates.probes {
            crate::rollouts::check_probe(url)?;
        }

        for (index, step) in steps.iter().enumerate() {
            if step.percent == 0 || step.percent > 100 {
                return Err(format!(
                    "step {}: the share must be 1 to 100 percent",
                    index + 1
                ));
            }

            if index > 0 && step.percent <= steps[index - 1].percent {
                return Err(format!("step {}: the shares must increase", index + 1));
            }
        }

        self.make_room_for(app, candidate).await
    }

    /// What follows the start of a rollout's candidate (issue 693): the
    /// rollout begins at its first step.
    async fn rollout_started(
        &mut self,
        app: &str,
        candidate: &str,
        steps: Vec<RolloutStep>,
        gates: RolloutGates,
        approve_each_step: bool,
    ) -> Result<Rollout, String> {
        let baseline = self
            .store
            .app(app)
            .map_err(|error| error.to_string())?
            .current
            .ok_or_else(|| format!("{app} has no serving release to roll out against"))?;

        if let Some(running) = self
            .running
            .get_mut(&(app.to_owned(), candidate.to_owned()))
        {
            running.held = false;
        }

        let now = now_unix_ms();
        let rollout = Rollout {
            app: app.to_owned(),
            id: self
                .store
                .next_rollout_id(app)
                .map_err(|error| error.to_string())?,
            candidate: candidate.to_owned(),
            baseline: baseline.clone(),
            message: step_message(&steps, 0),
            steps,
            gates,
            approve_each_step,
            state: RolloutState::Running,
            step: 0,
            step_started_unix_ms: now,
            created_unix_ms: now,
            updated_unix_ms: now,
        };
        self.store
            .put_rollout(&rollout)
            .map_err(|error| error.to_string())?;
        self.events.emit(
            "rollout.started",
            &format!("{app}@{candidate}"),
            "notice",
            &format!(
                "rollout {} against {baseline}: {}",
                rollout.id,
                describe_steps(&rollout.steps)
            ),
        );
        let pushed = self.push_snapshot().await;
        self.mark_ready(app, candidate, "");
        pushed?;
        self.begin_rollout_run(&rollout).await;
        Ok(rollout)
    }

    /// Section 9.3: a release's counters for the gates, the edge's with
    /// its workers' crashes and timeout kills and its failed probes.
    fn counters(
        &self,
        samples: &[muru_edge::metrics::Sample],
        app: &str,
        release: &str,
    ) -> Counters {
        let key = (app.to_owned(), release.to_owned());
        let mut counters = Counters::from_samples(samples, app, release);
        counters.faults = self.worker_faults.get(&key).copied().unwrap_or(0) as f64;

        if let Some((failures, error)) = self
            .probe_failures
            .lock()
            .expect("probe failures poisoned")
            .get(&key)
        {
            counters.probe_failures = *failures as f64;
            counters.probe_error = Some(error.clone());
        }

        counters
    }

    /// Section 9.3: send a running rollout's probes to the candidate and
    /// the baseline, each through the edge by preview, when the interval
    /// has passed; a task sends them and records the round's failures
    /// together, so a fault both releases share is never judged half
    /// seen.
    fn send_probes(&mut self, rollout: &Rollout) {
        if rollout.gates.probes.is_empty()
            || self
                .probed
                .get(&rollout.app)
                .is_some_and(|sent| sent.elapsed() < crate::rollouts::PROBE_INTERVAL)
        {
            return;
        }

        self.probed.insert(rollout.app.clone(), Instant::now());
        let client = self.probe_client.clone();
        let failures = self.probe_failures.clone();
        let urls = rollout.gates.probes.clone();
        let app = rollout.app.clone();
        let releases = [rollout.candidate.clone(), rollout.baseline.clone()];

        tokio::spawn(async move {
            let mut failed = Vec::new();

            for release in releases {
                for url in &urls {
                    if let Err(error) = crate::rollouts::probe(&client, url, &release).await {
                        tracing::debug!(%app, %release, %error, "a rollout probe failed");
                        failed.push((release.clone(), error));
                    }
                }
            }

            let mut failures = failures.lock().expect("probe failures poisoned");

            for (release, error) in failed {
                let entry = failures
                    .entry((app.clone(), release))
                    .or_insert((0, String::new()));
                entry.0 += 1;
                entry.1 = error;
            }
        });
    }

    /// Section 9.3: mark where a rollout's current step starts from, once
    /// its share is in force, so the gates judge the requests served under
    /// this step alone. Without the edge's counters the first tick marks
    /// it instead.
    async fn begin_rollout_run(&mut self, rollout: &Rollout) {
        match self.edge.metrics().await {
            Ok(samples) => {
                self.rollout_runs.insert(
                    rollout.app.clone(),
                    RolloutRun {
                        rollout: rollout.id.clone(),
                        step: rollout.step,
                        candidate: self.counters(&samples, &rollout.app, &rollout.candidate),
                        baseline: self.counters(&samples, &rollout.app, &rollout.baseline),
                    },
                );
            }
            Err(error) => {
                tracing::debug!(%error, "the edge's metrics could not be read for the rollout");
                self.rollout_runs.remove(&rollout.app);
            }
        }
    }

    /// Pause, resume, promote or abort the application's active rollout.
    async fn control_rollout(
        &mut self,
        app: &str,
        action: RolloutAction,
    ) -> Result<Rollout, String> {
        let Some(mut rollout) = self
            .store
            .active_rollout(app)
            .map_err(|error| error.to_string())?
        else {
            return Err(format!("{app} has no rollout under way"));
        };

        let subject = format!("{app}@{}", rollout.candidate);
        let percent = rollout.percent();

        match action {
            RolloutAction::Pause => {
                if rollout.state != RolloutState::Running {
                    return Err(format!(
                        "rollout {} is {}",
                        rollout.id,
                        rollout.state.as_str()
                    ));
                }

                rollout.state = RolloutState::Paused;
                rollout.message = format!("paused at {percent}%");
                self.events
                    .emit("rollout.paused", &subject, "notice", &rollout.message);
            }
            RolloutAction::Resume => {
                let approved = match rollout.state {
                    RolloutState::Paused => false,
                    RolloutState::AwaitingApproval => true,
                    RolloutState::Running => {
                        return Err(format!("rollout {} is running", rollout.id));
                    }
                    RolloutState::Promoted | RolloutState::RolledBack => unreachable!(),
                };

                rollout.state = RolloutState::Running;
                rollout.step_started_unix_ms = now_unix_ms();
                rollout.message = step_message(&rollout.steps, rollout.step);

                if approved {
                    self.events.emit(
                        "rollout.step",
                        &subject,
                        "notice",
                        &format!("approved: {}", rollout.message),
                    );
                    self.store
                        .put_rollout(&rollout)
                        .map_err(|error| error.to_string())?;
                    self.push_snapshot().await?;
                } else {
                    self.events
                        .emit("rollout.resumed", &subject, "notice", &rollout.message);
                }

                self.begin_rollout_run(&rollout).await;
            }
            RolloutAction::Promote => {
                return Err("a rollout is promoted by promote_rollout_by_hand".to_owned());
            }
            RolloutAction::Abort => {
                self.finish_rollout(
                    &mut rollout,
                    RolloutState::RolledBack,
                    &format!("aborted by hand at {percent}%"),
                )
                .await?;
            }
        }

        rollout.updated_unix_ms = now_unix_ms();
        self.store
            .put_rollout(&rollout)
            .map_err(|error| error.to_string())?;
        Ok(rollout)
    }

    /// End a rollout. Promoted, the candidate takes all the traffic and
    /// the baseline drains before the record says so; rolled back, the
    /// candidate leaves traffic and is kept warm, so it can be looked at.
    async fn finish_rollout(
        &mut self,
        rollout: &mut Rollout,
        state: RolloutState,
        message: &str,
    ) -> Result<(), String> {
        let subject = format!("{}@{}", rollout.app, rollout.candidate);
        self.rollout_runs.remove(&rollout.app);
        self.probed.remove(&rollout.app);

        let record = |rollout: &mut Rollout, store: &Store| -> Result<(), String> {
            rollout.state = state;
            rollout.message = message.to_owned();
            rollout.updated_unix_ms = now_unix_ms();
            store
                .put_rollout(rollout)
                .map_err(|error| error.to_string())
        };

        match state {
            RolloutState::Promoted => {
                self.promote_rollout(rollout.clone(), message, None).await;
            }
            _ => {
                record(rollout, &self.store)?;
                self.push_snapshot().await?;
                self.events.emit(
                    "rollout.rolled_back",
                    &subject,
                    "warning",
                    &format!(
                        "rollout {}: {message}; the candidate is kept for inspection",
                        rollout.id
                    ),
                );
                let candidate = (rollout.app.clone(), rollout.candidate.clone());

                if !self.referenced_releases().contains(&candidate)
                    && self.serving.get(&rollout.app) != Some(&rollout.candidate)
                {
                    self.drain(&rollout.app, &rollout.candidate, Retention::inspection())
                        .await;
                }
            }
        }

        Ok(())
    }

    /// Blue/green (section 9.2): all of the application's traffic to the
    /// release now, which is started if it is not running, and the release
    /// that served drains unless a policy names it, kept warm as `retain`
    /// says.
    async fn promote_release(
        &mut self,
        app: &str,
        id: &str,
        retain: Retention,
        then: AfterPromote,
    ) {
        let checked = (|| {
            let record = self.store.app(app).map_err(|error| error.to_string())?;

            if record.current.as_deref() == Some(id) {
                return Err(format!("{app}@{id} already serves"));
            }

            match crate::config::second_app_refusal(&self.store, app) {
                Some(refusal) => Err(refusal),
                None => Ok(()),
            }
        })();

        let checked = match checked {
            Ok(()) => self.make_room_for(app, id).await,
            Err(error) => Err(error),
        };

        if let Err(error) = checked {
            self.promoted(then, Err(error)).await;
            return;
        }

        self.start_then(
            app,
            id,
            AfterStart::Promote {
                app: app.to_owned(),
                id: id.to_owned(),
                retain,
                then,
            },
        )
        .await;
    }

    /// What follows the start of a release being promoted (issue 693):
    /// section 9.12 (issue 266), its before_promote gates run, prepared
    /// here and run on a task beside the loop, with the release about to
    /// take all the traffic; one that fails stops the promotion. With none
    /// to run, traffic moves at once.
    async fn promote_started(
        &mut self,
        app: &str,
        id: &str,
        retain: Retention,
        then: AfterPromote,
    ) {
        let commands = self
            .store
            .release(app, id)
            .map(|release| release.spec.commands)
            .unwrap_or_default();
        let gates = self.gates_due(app, id, &commands, crate::model::Gate::BeforePromote);

        if gates.is_empty() {
            let promoted = self.promote_switch(app, id, retain).await;
            self.promoted(then, promoted).await;
            return;
        }

        let mut prepared = Vec::new();

        for gate in &gates {
            let request = crate::commands::CommandRequest::Declared {
                name: gate.name.clone(),
                inputs: BTreeMap::new(),
                secret_inputs: BTreeMap::new(),
            };

            match self.prepare_command(app, id, request).await {
                Ok(mut command) => {
                    command.exclusive = true;
                    prepared.push(command);
                }
                Err(error) => {
                    let message = format!(
                        "the before_promote gate {} could not be started: {error}",
                        gate.name
                    );
                    self.promoted(then, Err(message)).await;
                    return;
                }
            }
        }

        let Ok(release) = self.store.release(app, id) else {
            self.promoted(then, Err(format!("{app}@{id} does not exist")))
                .await;
            return;
        };

        let context = self.context();
        let runs = self.runs.clone();
        let commands = self.commands.clone();

        tokio::spawn(async move {
            let outcome = run_gate_commands(
                &context,
                &runs,
                &release,
                prepared,
                crate::model::Gate::BeforePromote,
            )
            .await;
            let _ = commands
                .send(Command::PromoteGated {
                    app: release.app.clone(),
                    id: release.id.clone(),
                    retain,
                    then: Box::new(then),
                    outcome,
                })
                .await;
        });
    }

    /// Blue/green (section 9.2), once the release runs and its gates have
    /// passed: all of the application's traffic to it, and the release
    /// that served drains unless a policy names it, kept warm as `retain`
    /// says.
    async fn promote_switch(
        &mut self,
        app: &str,
        id: &str,
        retain: Retention,
    ) -> Result<Release, String> {
        let current = self
            .store
            .app(app)
            .map_err(|error| error.to_string())?
            .current;

        self.switch_to(app, id).await;
        self.store
            .set_app_releases(app, Some(id), current.as_deref())
            .map_err(|error| error.to_string())?;

        if let Some(previous) = current.filter(|previous| previous != id)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), previous.clone()))
        {
            self.drain(app, &previous, retain).await;
        }

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// What follows a promotion, once it has switched or failed: the
    /// answer to whoever asked, or a rollout's end. A rollout whose
    /// promotion a gate stops is paused where it was, to be promoted or
    /// aborted once looked at.
    async fn promoted(&mut self, then: AfterPromote, promoted: Result<Release, String>) {
        let (mut rollout, message, done) = match then {
            AfterPromote::Reply(done) => {
                let _ = done.send(promoted);
                return;
            }
            AfterPromote::Rollout {
                rollout,
                message,
                done,
            } => (*rollout, message, done),
        };

        self.promoting.remove(&rollout.app);
        let subject = format!("{}@{}", rollout.app, rollout.candidate);

        let outcome = match promoted {
            Ok(_) => {
                rollout.state = RolloutState::Promoted;
                rollout.message = message.clone();
                rollout.updated_unix_ms = now_unix_ms();

                match self.store.put_rollout(&rollout) {
                    Ok(()) => {
                        if let Err(error) = self.push_snapshot().await {
                            tracing::error!(%error, "pushing the snapshot after a promotion failed");
                        }

                        self.events.emit(
                            "rollout.completed",
                            &subject,
                            "notice",
                            &format!("rollout {}: {message}", rollout.id),
                        );
                        Ok(rollout)
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
            Err(error) => {
                rollout.state = RolloutState::Paused;
                rollout.message = format!("promotion stopped: {error}");
                rollout.updated_unix_ms = now_unix_ms();
                let _ = self.store.put_rollout(&rollout);
                self.events
                    .emit("rollout.paused", &subject, "warning", &rollout.message);
                Err(error)
            }
        };

        if let Some(done) = done {
            let _ = done.send(outcome);
        } else if let Err(error) = outcome {
            tracing::error!(%error, "promoting a rollout failed");
        }
    }

    /// End a rollout by promoting its candidate (issue 693): the promotion
    /// runs beside the loop, and the rollout is recorded promoted, or
    /// paused, once it has. A rollout already being promoted is left to
    /// that promotion.
    async fn promote_rollout(
        &mut self,
        rollout: Rollout,
        message: &str,
        done: Option<oneshot::Sender<Result<Rollout, String>>>,
    ) {
        if !self.promoting.insert(rollout.app.clone()) {
            if let Some(done) = done {
                let _ = done.send(Err(format!(
                    "rollout {} is already being promoted",
                    rollout.id
                )));
            }

            return;
        }

        self.rollout_runs.remove(&rollout.app);
        self.probed.remove(&rollout.app);
        let (app, candidate) = (rollout.app.clone(), rollout.candidate.clone());

        self.promote_release(
            &app,
            &candidate,
            Retention::default(),
            AfterPromote::Rollout {
                rollout: Box::new(rollout),
                message: message.to_owned(),
                done,
            },
        )
        .await;
    }

    /// `muructl rollout promote`: promote the application's active
    /// rollout now, answering once the promotion has ended.
    async fn promote_rollout_by_hand(
        &mut self,
        app: &str,
        done: oneshot::Sender<Result<Rollout, String>>,
    ) {
        let rollout = match self.store.active_rollout(app) {
            Ok(Some(rollout)) => rollout,
            Ok(None) => {
                let _ = done.send(Err(format!("{app} has no rollout under way")));
                return;
            }
            Err(error) => {
                let _ = done.send(Err(error.to_string()));
                return;
            }
        };

        let message = format!("promoted by hand at {}%", rollout.percent());
        self.promote_rollout(rollout, &message, Some(done)).await;
    }

    /// Section 9.12 (issue 266): the commands of a release that are gates
    /// at `stage` and still to run: those it has not passed, or, before a
    /// fork, those that run at every start. A managed server's release
    /// runs no commands, so has no gates.
    fn gates_due(
        &self,
        app: &str,
        id: &str,
        commands: &[crate::model::CommandSpec],
        stage: crate::model::Gate,
    ) -> Vec<crate::model::CommandSpec> {
        let passed = self.store.gates_passed(app, id).unwrap_or_default();

        commands
            .iter()
            .filter(|command| command.gates(stage))
            .filter(|command| {
                (stage == crate::model::Gate::BeforeFork && command.every_start)
                    || !passed
                        .iter()
                        .any(|(name, at, _, _)| *name == command.name && at == stage.as_str())
            })
            .cloned()
            .collect()
    }

    /// Section 10.10: start a recorded release held, traffic left where
    /// it is, for an apply's prepare phase. Answers whether it was
    /// started: false when it ran already.
    async fn start_held(
        &mut self,
        app: &str,
        id: &str,
        done: oneshot::Sender<Result<bool, String>>,
    ) {
        let key = (app.to_owned(), id.to_owned());

        if self.running.get(&key).is_some_and(|running| {
            running.retained_until.is_none() && running.sticky_until.is_none()
        }) {
            let _ = done.send(Ok(false));
            return;
        }

        if let Err(error) = self.make_room_for(app, id).await {
            let _ = done.send(Err(error));
            return;
        }

        self.start_then(
            app,
            id,
            AfterStart::Held {
                app: app.to_owned(),
                id: id.to_owned(),
                done,
            },
        )
        .await;
    }

    /// What follows an apply's start of a release held (issue 693).
    async fn held_started(&mut self, app: &str, id: &str) -> Result<bool, String> {
        let key = (app.to_owned(), id.to_owned());

        if let Some(running) = self.running.get_mut(&key) {
            running.held = true;
            running.sticky_until = None;
        }

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the snapshot for a held release failed");
        }

        self.mark_ready(app, id, "held: not serving");
        Ok(true)
    }

    /// Section 10.10: an apply's commit phase. Its writes are made in one
    /// transaction; the releases they make serve take the traffic and the
    /// releases the routes name are started; then one snapshot gives the
    /// edge all of it, and the releases that served before drain.
    async fn commit_apply(
        &mut self,
        changes: &[crate::config::Change],
        retain: Retention,
    ) -> Result<crate::config::Committed, String> {
        // Section 9.3: a promotion by hand waits for the rollout under
        // way to end. The prepare phase left each release to serve
        // running.

        for (app, id) in changes.iter().filter_map(|change| change.action.serves()) {
            let rollout = self.store.active_rollout(app).ok().flatten();

            if let Some(why) = crate::rollouts::promotion_refused(rollout.as_ref(), app, id) {
                return Err(why);
            }

            if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
                return Err(format!("{app}@{id} stopped before it could serve"));
            }
        }

        let mut committed = crate::config::commit(&self.store, changes)?;

        for (app, id, _) in &committed.promoted {
            self.serving.insert(app.clone(), id.clone());

            if let Some(running) = self.running.get_mut(&(app.clone(), id.clone())) {
                running.held = false;
            }
        }

        for (kind, subject, message) in &committed.events {
            self.events.emit(kind, subject, "notice", message);
        }

        // The applied client classes and limits may have changed, and so
        // may the conditions.

        match self.client_rules.policies(&self.store) {
            Ok(clients) => self.clients = clients,
            Err(error) => {
                tracing::warn!(%error, "the client classes stay as they were");
            }
        }

        self.load_conditions();

        self.start_referenced().await;
        self.refresh_certificates(true);
        committed.snapshot_error = self.push_snapshot().await.err();

        for (app, id, previous) in committed.promoted.clone() {
            let subject = format!("{app}@{id}");
            self.mark_ready(&app, &id, "");

            match &committed.snapshot_error {
                None => self.events.emit(
                    "traffic.switched",
                    &subject,
                    "notice",
                    "traffic moved to the release",
                ),
                Some(error) => self.events.emit(
                    "release.failed",
                    &subject,
                    "error",
                    &format!("the edge could not take the release: {error}"),
                ),
            }

            if let Some(previous) = previous.filter(|previous| previous != &id)
                && !self
                    .referenced_releases()
                    .contains(&(app.clone(), previous.clone()))
            {
                self.drain(&app, &previous, retain).await;
            }
        }

        self.drain_unreferenced().await;
        Ok(committed)
    }

    /// Section 9.3: move every running rollout along. The current step is
    /// judged by its gates against the baseline over the same window; a
    /// failing gate rolls back at once; once the hold has passed with the
    /// gates satisfied, the next step begins, or waits for approval, or
    /// the last step promotes the candidate. Without enough requests to
    /// judge, the step holds until there are.
    async fn advance_rollouts(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };
        let running: Vec<Rollout> = self
            .store
            .rollouts(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|rollout| rollout.state == RolloutState::Running)
            .collect();

        // Section 9.10: an overlay that pins or splits a route's traffic
        // holds its application's rollout until it ends, as a deploy
        // freeze does (section 9.6).

        let pinned = {
            let overlays = self.store.overlays().unwrap_or_default();
            let states = self.store.overlay_states().unwrap_or_default();
            let active = crate::overlays::ordered(&overlays, &states);
            let mut pinned =
                crate::overlays::pinned_apps(&self.store.routes().unwrap_or_default(), &active);
            pinned.extend(crate::overlays::freezes(&active).into_keys());
            pinned
        };

        for mut rollout in running {
            // Issue 693: a rollout being promoted is left to its promotion.

            if self.promoting.contains(&rollout.app) {
                continue;
            }

            let app = rollout.app.clone();

            if pinned.contains(&app) {
                continue;
            }

            let subject = format!("{app}@{}", rollout.candidate);
            let now = now_unix_ms();

            let run = match self.rollout_runs.get(&app) {
                Some(run) if run.rollout == rollout.id && run.step == rollout.step => run.clone(),
                _ => {
                    let run = RolloutRun {
                        rollout: rollout.id.clone(),
                        step: rollout.step,
                        candidate: self.counters(samples, &app, &rollout.candidate),
                        baseline: self.counters(samples, &app, &rollout.baseline),
                    };
                    rollout.step_started_unix_ms = now;
                    let _ = self.store.put_rollout(&rollout);
                    self.rollout_runs.insert(app.clone(), run.clone());
                    run
                }
            };

            self.send_probes(&rollout);
            let candidate = self
                .counters(samples, &app, &rollout.candidate)
                .since(&run.candidate);
            let baseline = self
                .counters(samples, &app, &rollout.baseline)
                .since(&run.baseline);
            let verdict = judge(&candidate, &baseline, &rollout.gates);
            let percent = rollout.percent();

            if let Verdict::Fail(reason) = verdict {
                self.events.emit(
                    "rollout.gate_failed",
                    &subject,
                    "warning",
                    &format!("step {} at {percent}%: {reason}", rollout.step + 1),
                );

                if let Err(error) = self
                    .finish_rollout(
                        &mut rollout,
                        RolloutState::RolledBack,
                        &format!("gate failed at {percent}%: {reason}"),
                    )
                    .await
                {
                    tracing::error!(%error, "rolling back a rollout failed");
                }

                continue;
            }

            let hold = rollout
                .steps
                .get(rollout.step)
                .map_or(0, |step| step.hold_ms);

            if now < rollout.step_started_unix_ms + hold {
                continue;
            }

            if verdict == Verdict::Insufficient {
                let message = format!(
                    "step {} at {percent}%: holding for {} requests to judge, {} so far",
                    rollout.step + 1,
                    rollout.gates.min_requests,
                    candidate.total as u64
                );

                if rollout.message != message {
                    rollout.message = message;
                    rollout.updated_unix_ms = now;
                    let _ = self.store.put_rollout(&rollout);
                }

                continue;
            }

            if rollout.step + 1 >= rollout.steps.len() {
                self.promote_rollout(rollout, "every step passed its gates", None)
                    .await;
                continue;
            }

            rollout.step += 1;
            rollout.step_started_unix_ms = now;
            rollout.updated_unix_ms = now;
            self.rollout_runs.remove(&app);

            if rollout.approve_each_step {
                rollout.state = RolloutState::AwaitingApproval;
                rollout.message = format!(
                    "{} awaits approval: muructl rollout resume {app}",
                    step_message(&rollout.steps, rollout.step)
                );
                let _ = self.store.put_rollout(&rollout);
                self.events
                    .emit("rollout.step", &subject, "notice", &rollout.message);
                continue;
            }

            rollout.message = step_message(&rollout.steps, rollout.step);
            let _ = self.store.put_rollout(&rollout);
            self.events
                .emit("rollout.step", &subject, "notice", &rollout.message);

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot for a rollout step failed");
            }

            self.begin_rollout_run(&rollout).await;
        }
    }

    /// Section 9.4: the releases whose sticky grace has passed leave the
    /// snapshot and drain.
    async fn expire_sticky(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String, Retention)> = self
            .running
            .iter()
            .filter_map(|((app, id), running)| {
                let (until, retain) = running.sticky_until?;
                (until <= now).then(|| (app.clone(), id.clone(), retain))
            })
            .collect();

        for (app, id, retain) in expired {
            if let Some(running) = self.running.get_mut(&(app.clone(), id.clone())) {
                running.sticky_until = None;
            }

            if let Err(error) = self.push_snapshot().await {
                tracing::error!(%error, "pushing the snapshot after a sticky grace failed");
            }

            self.drain_now(&app, &id, retain).await;
        }
    }

    /// Section 9.4: the releases the route policies name must run, and a
    /// release that only a policy kept in traffic drains once no policy
    /// names it. Called after the configuration changes and at start.
    async fn reconcile_traffic(&mut self) {
        self.start_referenced().await;

        self.drain_unreferenced().await;
    }

    /// The releases the route policies and the overlays' warm-ups name, as
    /// (app, release), and of those the ones only a warm-up names.
    fn traffic_referenced(&self) -> (ReleaseSet, ReleaseSet) {
        let mut referenced = self.referenced_releases();
        let warming: BTreeSet<(String, String)> = self
            .warm_ups
            .values()
            .flatten()
            .filter(|release| !referenced.contains(*release))
            .cloned()
            .collect();
        referenced.extend(warming.iter().cloned());
        (referenced, warming)
    }

    /// Start the releases a policy or a warm-up names that do not run;
    /// each is given to the edge, and marked ready, once it has started.
    async fn start_referenced(&mut self) {
        let (referenced, warming) = self.traffic_referenced();

        for (app, id) in &referenced {
            let key = (app.clone(), id.clone());

            let needs_start = match self.running.get(&key) {
                None => true,
                Some(running) => running.retained_until.is_some() || running.sticky_until.is_some(),
            };

            if !needs_start || self.store.release(app, id).is_err() {
                continue;
            }

            if let Some(running) = self.running.get_mut(&key) {
                running.sticky_until = None;
            }

            let then = AfterStart::Referenced {
                app: app.clone(),
                id: id.clone(),
                warming: warming.contains(&key),
            };

            match self.make_room_for(app, id).await {
                Ok(()) => self.start_then(app, id, then).await,
                Err(error) => self.after_start(then, Err(error)).await,
            }
        }
    }

    /// What follows the start of a release a policy or a warm-up names
    /// (issue 693): the edge is given it, or the failure said.
    async fn referenced_started(
        &mut self,
        app: &str,
        id: &str,
        warming: bool,
        started: Result<(), String>,
    ) {
        match started {
            Ok(()) => {
                if let Err(error) = self.push_snapshot().await {
                    tracing::error!(%error, "pushing the snapshot for a policy's releases failed");
                }

                self.mark_ready(app, id, "");
            }
            Err(error) => {
                let why = if warming {
                    "an overlay's warm-up names the release"
                } else {
                    "a traffic policy names the release"
                };
                self.events.emit(
                    "traffic_policy.release_unavailable",
                    &format!("{app}@{id}"),
                    "warning",
                    &format!("{why} but it cannot start: {error}"),
                );
            }
        }
    }

    /// Drain the releases that only a policy kept in traffic, once no
    /// policy names them.
    async fn drain_unreferenced(&mut self) {
        let (referenced, _) = self.traffic_referenced();
        let unreferenced: Vec<(String, String)> = self
            .running
            .iter()
            .filter(|((app, id), running)| {
                running.retained_until.is_none()
                    && running.sticky_until.is_none()
                    && !running.held
                    && self.serving.get(app) != Some(id)
                    && !referenced.contains(&((*app).clone(), (*id).clone()))
            })
            .map(|(key, _)| key.clone())
            .collect();

        for (app, id) in unreferenced {
            self.drain(&app, &id, Retention::default()).await;
        }
    }

    /// Section 7.4: feed every running release's scalers what the edge
    /// reports and carry out their decisions, each one an event with its
    /// reasons, scale-outs once the node's budget admits them. A group
    /// whose count is not what its scaler wants, after a restart or a
    /// restored pin, is asked for again.
    async fn autoscale(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let budget = self.budget();

        let Some(samples) = samples else {
            return;
        };
        let now = Instant::now();
        let mut requests: Vec<(String, String, String, u32)> = Vec::new();
        let mut emitted: Vec<(&'static str, String, &'static str, String)> = Vec::new();
        let mut capacities = self.capacities();

        // Scale-outs are admitted against what the releases hold before
        // any of them, the highest application priority first.

        let fallback = self.node_mean_pss();
        let used = self.budget_usage();
        let priorities: HashMap<String, i32> = self
            .store
            .apps()
            .unwrap_or_default()
            .into_iter()
            .map(|app| (app.name, app.priority))
            .collect();
        let mut asks = Vec::new();
        let mut reasons: HashMap<(String, String, String), (u32, String)> = HashMap::new();

        for ((app, id), running) in &mut self.running {
            // A retained release is at its warm floor (section 9.1), not
            // the autoscaler's to size.

            if running.retained_until.is_some() || running.control.is_none() {
                continue;
            }

            let subject = format!("{app}@{id}");

            for index in 0..running.scalers.len() {
                let cost = running.worker_cost(&running.scalers[index], fallback);
                let name = running.scalers[index].name.clone();
                let Some(sockets) = running.group_sockets(&name) else {
                    continue;
                };
                let ready = sockets.len() as u32;
                let youngest = sockets
                    .iter()
                    .filter_map(|socket| running.ready_at.get(socket))
                    .max()
                    .map(|at| now.saturating_duration_since(*at));
                let group = &mut running.scalers[index];
                let capacity = capacities.remove(&(app.clone(), id.clone(), group.name.clone()));

                match group.observe(now, samples, app, id, ready, youngest, capacity) {
                    Step::Ask { from, to, reason } => {
                        let key = (app.clone(), id.clone(), name);
                        reasons.insert(key.clone(), (ready, reason));
                        asks.push(crate::budget::Ask {
                            key,
                            priority: priorities.get(app).copied().unwrap_or(0),
                            from,
                            to,
                            cost,
                        });
                    }
                    Step::Event(kind, level, message) => {
                        emitted.push((kind, subject.clone(), level, message));
                    }
                    Step::Nothing => {}
                }
            }
        }

        for admitted in crate::budget::admit(&budget, used, asks) {
            let (app, id, name) = &admitted.ask.key;
            let Some(group) = self
                .running
                .get_mut(&(app.clone(), id.clone()))
                .and_then(|running| running.scalers.iter_mut().find(|group| &group.name == name))
            else {
                continue;
            };

            let subject = format!("{app}@{id}");
            let (ready, reason) = reasons.remove(&admitted.ask.key).unwrap_or_default();
            let held = admitted.limit.map_or_else(String::new, |limit| {
                format!("the node's {limit} budget ({})", budget.describe(&used))
            });

            for (kind, level, message) in group.admitted(
                now,
                ready,
                admitted.ask.from,
                admitted.ask.to,
                admitted.to,
                admitted.limit,
                &held,
                &reason,
                Some(admitted.ask.priority),
            ) {
                emitted.push((kind, subject.clone(), level, message));
            }
        }

        for ((app, id), running) in &mut self.running {
            if running.retained_until.is_some() || running.control.is_none() {
                continue;
            }

            for index in 0..running.scalers.len() {
                let Some(ready) = running
                    .group_sockets(&running.scalers[index].name)
                    .map(|sockets| sockets.len() as u32)
                else {
                    continue;
                };

                let group = &mut running.scalers[index];

                if let Some(desired) = group.wanted(ready) {
                    requests.push((app.clone(), id.clone(), group.name.clone(), desired));
                }
            }
        }

        for (kind, subject, level, message) in emitted {
            self.events.emit(kind, &subject, level, &message);
        }

        for (app, id, group, processes) in requests {
            self.ask_scale(&app, &id, &group, processes);
        }

        // Section 9.1: over the memory budget, retained releases make way,
        // the oldest first, one a tick: down to the host alone, then
        // stopped.

        if self
            .budget()
            .memory
            .is_some_and(|memory| used.memory > memory)
        {
            self.relieve_memory().await;
        }

        for (app, id, idle) in self.idle_hosts(now) {
            self.sleep_release(&app, &id, idle).await;
        }

        let view = self.budget_view(used);
        self.check_floors_ahead(&view);
        self.info
            .defined
            .write()
            .expect("defined names poisoned")
            .budget = view;
    }

    /// Section 7.5: each window of a schedule or overlay that raises a
    /// floor is checked against the node's budget once ahead of it, from
    /// the start of its warm-up's lead or `BUDGET_LEAD` before it,
    /// whichever is longer, with an event when it does not fit.
    fn check_floors_ahead(&mut self, view: &crate::budget::View) {
        if !self.budget().is_set() {
            return;
        }

        let now = now_unix_ms();
        let overlays = self.store.overlays().unwrap_or_default();

        for entry in &overlays {
            let (name, overlay) = entry;
            let Some((start, _)) = crate::overlays::window(overlay, now) else {
                continue;
            };
            let lead = overlay
                .warm_up
                .as_ref()
                .and_then(|warm_up| muru_config::parse_duration(&warm_up.lead).ok())
                .unwrap_or_default()
                .max(BUDGET_LEAD);

            if now >= start
                || start > now + lead.as_millis() as u64
                || !self.budget_checked.insert((name.clone(), start))
            {
                continue;
            }

            for ((app, group), capacity) in crate::overlays::capacities(std::iter::once(entry)) {
                if capacity.floor == 0 {
                    continue;
                }

                if let Err(why) = view.floor_fits(&app, &group, capacity.floor, None) {
                    self.events.emit(
                        "schedule.over_budget",
                        &format!("overlay {name}"),
                        "warning",
                        &format!(
                            "the window from {}: a floor of {} workers for {app} ({group} group) does not fit the node's budget as it stands: {why}",
                            clock(start),
                            capacity.floor
                        ),
                    );
                }
            }
        }

        self.budget_checked.retain(|(_, start)| *start > now);
    }

    /// Section 9.1: the node is over its memory budget. The retained
    /// release created first that still has workers drops to its host
    /// alone; when none has, the one created first stops. One kept for
    /// inspection goes before any rollback target.
    async fn relieve_memory(&mut self) {
        let mut retained: Vec<(bool, u64, String, String)> = self
            .running
            .iter()
            .filter(|(_, running)| running.retained_until.is_some())
            .map(|((app, id), running)| {
                let created = self
                    .store
                    .release(app, id)
                    .map_or(0, |release| release.created_unix_ms);
                (!running.inspected, created, app.clone(), id.clone())
            })
            .collect();
        retained.sort();

        for (_, _, app, id) in &retained {
            let key = (app.clone(), id.clone());
            let Some(running) = self.running.get_mut(&key) else {
                continue;
            };

            if running.workers() == 0 || running.backend.host().is_none() {
                continue;
            }

            let inspected = running.inspected;
            let left = running.retained_until.map_or(Duration::ZERO, |until| {
                until.saturating_duration_since(Instant::now())
            });
            let groups: Vec<String> = running
                .scalers
                .iter_mut()
                .map(|group| {
                    group.requested = Some(0);
                    group.name.clone()
                })
                .collect();

            for group in &groups {
                self.ask_scale(app, id, group, 0);
            }

            // The note says what a rollback now costs; one kept for
            // inspection keeps its own.

            if !inspected && let Ok(release) = self.store.release(app, id) {
                let until = crate::model::now_unix_ms() + left.as_millis() as u64;
                let note = Kept::Workers(groups.iter().map(|group| (group.clone(), 0)).collect())
                    .note(&clock(until));
                self.set_state(&release, ReleaseState::Retained, &note);
            }

            self.events.emit(
                "release.retained",
                &format!("{app}@{id}"),
                "warning",
                "down to its host alone: the node is over its memory budget",
            );
            return;
        }

        if let Some((_, _, app, id)) = retained.first() {
            self.stop_release(app, id, "the node is over its memory budget");
        }
    }

    /// Section 7.4: the releases whose groups have all been at zero for
    /// their host_idle_timeout, with how long that is; the time each
    /// began is kept as it goes.
    fn idle_hosts(&mut self, now: Instant) -> Vec<(String, String, Duration)> {
        let mut idle = Vec::new();

        for ((app, id), running) in &mut self.running {
            let timeout = running
                .scalers
                .first()
                .and_then(|group| group.scaler.spec().host_idle_timeout_ms)
                .map(Duration::from_millis);

            let at_zero = running.backend.host().is_some()
                && running.control.is_some()
                && running.retained_until.is_none()
                && running.workers() == 0
                && running
                    .scalers
                    .iter()
                    .all(|group| group.scaler.desired() == 0);

            let Some(timeout) = timeout.filter(|_| at_zero) else {
                running.idle_since = None;
                continue;
            };

            let since = *running.idle_since.get_or_insert(now);

            if now.saturating_duration_since(since) >= timeout {
                idle.push((app.clone(), id.clone(), timeout));
            }
        }

        idle
    }

    /// Section 7.4: stop the host of a release whose groups have been at
    /// zero for its host_idle_timeout. The release keeps its place in
    /// the snapshot, so the next request waits at the edge and wakes it.
    async fn sleep_release(&mut self, app: &str, id: &str, idle: Duration) {
        let key = (app.to_owned(), id.to_owned());
        let Some(mut running) = self.running.remove(&key) else {
            return;
        };

        let backend = std::mem::replace(&mut running.backend, Backend::Dormant);
        let run_dir = self.locations.run.path.join("apps").join(app).join(id);
        let record = backend.record_path(&run_dir);
        let grace = self
            .store
            .release(app, id)
            .map_or(Duration::from_secs(5), |release| {
                release.spec.timeouts.stop_budget()
            });

        if tokio::time::timeout(stop_wait(grace), backend.stop(grace))
            .await
            .is_err()
        {
            tracing::warn!(
                app,
                id,
                "the idle host did not stop within the drain timeout"
            );
        }

        // The next request starts the host again, which writes its secret
        // files again.

        let _ = std::fs::remove_file(record);
        let _ = std::fs::remove_dir_all(run_dir.join("secrets"));
        running.control = None;
        running.status.clear();
        running.signals_from = None;
        running.host_pss = None;
        running.idle_since = None;
        self.running.insert(key, running);

        if let Ok(release) = self.store.release(app, id) {
            self.set_state(
                &release,
                ReleaseState::Ready,
                "idle: its host has stopped, and the next request starts it",
            );
        }

        self.events.emit(
            "release.idle",
            &format!("{app}@{id}"),
            "info",
            &format!(
                "every worker group has been at zero for {}: its host stopped, and the next request starts it again",
                muru_config::format_duration(idle)
            ),
        );
    }

    /// Section 7.4: a request is waiting at the edge for a group with no
    /// workers. A release whose host stopped while idle is started
    /// again; a group scaled to zero gets a worker. The node's workers
    /// limit holds back neither, and a wake that takes the node over it
    /// stops retained releases to make room; the memory limit applies to
    /// both, as it does to a deploy.
    async fn wake(&mut self, wake: muru_proto::edge::v1::Wake) {
        let budget = self.budget();

        let key = (wake.app.clone(), wake.release.clone());
        let subject = format!("{}@{}", wake.app, wake.release);
        let now = Instant::now();
        let fallback = self.node_mean_pss();

        let Some(running) = self.running.get_mut(&key) else {
            return;
        };

        if matches!(running.backend, Backend::Dormant) {
            self.wake_host(&wake.app, &wake.release).await;
            return;
        }

        if running.retained_until.is_some() || running.control.is_none() {
            return;
        }

        let Some(index) = running
            .scalers
            .iter()
            .position(|group| group.name == wake.group)
        else {
            return;
        };

        let cost = running.worker_cost(&running.scalers[index], fallback);
        let group = &mut running.scalers[index];

        let Some(Decision::ScaleOut { to, reason }) = group.scaler.wake(now) else {
            return;
        };

        let priority = self
            .store
            .app(&wake.app)
            .map_or(0, |record| record.priority);
        let ask = crate::budget::Ask {
            key: (),
            priority,
            from: 0,
            to,
            cost,
        };

        // The wake is admitted with the workers limit lifted. Retained
        // releases make way for its memory, the oldest first, as they do
        // for a deploy, and it is refused only when it does not fit even
        // without them.

        let lifted = crate::budget::Budget {
            workers: None,
            ..budget
        };
        let mut used = self.budget_usage();
        let mut admitted = crate::budget::admit(&lifted, used, vec![ask.clone()]).remove(0);

        while admitted.limit == Some(crate::budget::Limit::Memory) {
            let Some((other_app, other_id)) = self.oldest_retained(None) else {
                break;
            };

            self.stop_release(
                &other_app,
                &other_id,
                &format!(
                    "making room in the node's memory budget for {subject}, woken by a request"
                ),
            );
            used = self.budget_usage();
            admitted = crate::budget::admit(&lifted, used, vec![ask.clone()]).remove(0);
        }

        let Some(group) = self
            .running
            .get_mut(&key)
            .and_then(|running| running.scalers.get_mut(index))
        else {
            return;
        };

        if let Some(limit) = admitted.limit {
            let held = format!("the node's {limit} budget ({})", budget.describe(&used));
            group.scaler.limit(0, admitted.to, now, &held);
            self.events.emit(
                "scaling.denied_by_budget",
                &subject,
                "warning",
                &format!(
                    "{}: a request is waiting for a group scaled to zero, and no worker fits {held}",
                    wake.group
                ),
            );
            return;
        }

        group.requested = Some(to);
        let mut message = decision_message(&wake.group, 0, to, &reason, false);

        if let Some(limit) = self
            .budget()
            .workers
            .filter(|limit| used.workers + to > *limit)
        {
            message.push_str(&format!(", over the node's budget of {limit} workers"));
        }

        self.events
            .emit("scaling.decision", &subject, "info", &message);
        self.ask_scale(&wake.app, &wake.release, &wake.group, to);
        self.make_worker_room(&subject).await;
    }

    /// Section 7.4: start again the host of a release that stopped while
    /// idle, for a request waiting at the edge. Its memory must fit the
    /// node's budget, retained releases making way as for a deploy; the
    /// workers limit does not hold it back.
    async fn wake_host(&mut self, app: &str, id: &str) {
        let key = (app.to_owned(), id.to_owned());
        let subject = format!("{app}@{id}");
        let Ok(release) = self.store.release(app, id) else {
            return;
        };

        if self.make_room(app, &release.spec).await.is_err() {
            return;
        }

        let starting = release.spec.processes
            + release
                .spec
                .groups
                .iter()
                .map(|group| group.processes)
                .sum::<u32>();
        let used = self.budget_usage();
        let mut message = "a request is waiting: starting its host again".to_owned();

        if let Some(limit) = self
            .budget()
            .workers
            .filter(|limit| used.workers + starting > *limit)
        {
            message.push_str(&format!(", over the node's budget of {limit} workers"));
        }

        self.running.remove(&key);
        self.events
            .emit("release.woken", &subject, "info", &message);

        self.start_then(
            app,
            id,
            AfterStart::Wake {
                app: app.to_owned(),
                id: id.to_owned(),
            },
        )
        .await;
    }

    /// Section 7.4: a wake from zero took the node over its workers
    /// limit. Retained releases that hold workers stop, the oldest first,
    /// until the node is back under it or none is left; until then other
    /// releases' scale-out finds no room.
    async fn make_worker_room(&mut self, subject: &str) {
        let Some(limit) = self.budget().workers else {
            return;
        };

        while self.budget_usage().workers > limit {
            let fallback = self.node_mean_pss();
            let Some((app, id)) =
                self.oldest_retained_where(|_, running| running.usage(fallback).workers > 0)
            else {
                break;
            };

            self.stop_release(
                &app,
                &id,
                &format!(
                    "making room in the node's worker budget for {subject}, woken by a request"
                ),
            );
        }
    }

    /// Section 7.4: the mean proportional set size of every worker that
    /// reported one, standing in for a group's that is not known.
    fn node_mean_pss(&self) -> Option<u64> {
        let sizes: Vec<u64> = self
            .running
            .values()
            .flat_map(|running| running.status.iter().filter_map(|worker| worker.pss))
            .collect();
        (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>() / sizes.len() as u64)
    }

    /// Section 7.4: what the running releases hold against the node's
    /// budget.
    fn budget_usage(&self) -> crate::budget::Usage {
        let fallback = self.node_mean_pss();
        let mut used = crate::budget::Usage::default();

        for running in self.running.values() {
            used.add_usage(&running.usage(fallback));
        }

        // Issue 693: a release being started holds what it is expected to.

        for starting in self.starting.values() {
            used.memory += starting.reserved;
        }

        used
    }

    /// Section 7.4: the node's budget and what its releases hold, for the
    /// checks before a release starts and on schedules.
    fn budget_view(&self, used: crate::budget::Usage) -> crate::budget::View {
        let fallback = self.node_mean_pss();
        let mut view = crate::budget::View {
            budget: self.budget(),
            used,
            fallback,
            sources: self.budget_sources(),
            ..crate::budget::View::default()
        };

        for ((app, id), running) in &self.running {
            if running.retained_until.is_some() {
                view.retained += running.usage(fallback).memory;
                continue;
            }

            if self.serving.get(app) != Some(id) {
                continue;
            }

            if let Some(pss) = running.host_pss {
                view.hosts.insert(app.clone(), pss);
            }

            for group in &running.scalers {
                let held = running
                    .group_sockets(&group.name)
                    .map_or(0, <[PathBuf]>::len) as u32;
                view.groups.insert(
                    (app.clone(), group.name.clone()),
                    (
                        held.max(group.scaler.desired()),
                        running.worker_cost(group, None),
                    ),
                );
            }
        }

        view
    }

    /// Section 13.6: before a release starts, whether its memory fits the
    /// node's budget, retained releases making way, the oldest first,
    /// when that is enough; refused with a `scaling.denied_by_budget`
    /// event when it is not.
    async fn make_room(&mut self, app: &str, spec: &ReleaseSpec) -> Result<(), String> {
        if self.budget().memory.is_none() {
            return Ok(());
        }

        let view = self.budget_view(self.budget_usage());

        if let Err(why) = view.release_fits(app, spec, view.retained) {
            let message = if view.retained > 0 {
                format!("{app}: {why}, even with the retained releases stopped")
            } else {
                format!("{app}: {why}")
            };
            self.events
                .emit("scaling.denied_by_budget", app, "warning", &message);
            return Err(format!("not started: {message}"));
        }

        while self
            .budget_view(self.budget_usage())
            .release_fits(app, spec, 0)
            .is_err()
        {
            let Some((other_app, other_id)) = self.oldest_retained(None) else {
                break;
            };
            self.stop_release(
                &other_app,
                &other_id,
                &format!("making room in the node's memory budget for {app}"),
            );
        }

        Ok(())
    }

    /// `make_room` for a recorded release that is not running.
    async fn make_room_for(&mut self, app: &str, id: &str) -> Result<(), String> {
        if self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return Ok(());
        }

        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        self.make_room(app, &release.spec).await
    }

    /// Section 9.1: the retained release created first, other than one
    /// of `except`'s application, which make way under memory pressure.
    fn oldest_retained(&self, except: Option<&str>) -> Option<(String, String)> {
        self.oldest_retained_where(|app, _| except != Some(app))
    }

    /// The retained release created first of those `wanted` accepts,
    /// given each one's application and state; one kept for inspection
    /// goes before any rollback target.
    fn oldest_retained_where(
        &self,
        wanted: impl Fn(&str, &Running) -> bool,
    ) -> Option<(String, String)> {
        self.running
            .iter()
            .filter(|((app, _), running)| running.retained_until.is_some() && wanted(app, running))
            .map(|((app, id), running)| {
                let created = self
                    .store
                    .release(app, id)
                    .map_or(0, |release| release.created_unix_ms);
                (!running.inspected, created, app.clone(), id.clone())
            })
            .min()
            .map(|(_, _, app, id)| (app, id))
    }

    /// Section 7.5: the capacity active overlays and schedules set, and
    /// the floors of those warming up ahead of a window, for each running
    /// release's groups. A release taking a share of its application's
    /// traffic, as a canary does, takes that share of the floor and of a
    /// cap (issue 712), rounded up.
    fn capacities(&self) -> HashMap<(String, String, String), crate::scaling::Capacity> {
        let overlays = self.store.overlays().unwrap_or_default();
        let states = self.store.overlay_states().unwrap_or_default();
        let mut applied = crate::overlays::ordered(&overlays, &states);

        // A warm-up readies capacity ahead of a window, so an overlay's
        // cap waits for the window itself.

        let warming: Vec<(String, muru_config::Overlay)> = overlays
            .iter()
            .filter(|(name, _)| self.warm_ups.contains_key(name))
            .map(|(name, overlay)| {
                let mut overlay = overlay.clone();

                for change in overlay.apps.values_mut() {
                    change.cap_workers = None;
                }

                (name.clone(), overlay)
            })
            .collect();
        applied.extend(warming.iter());
        let by_app = crate::overlays::capacities(applied);
        let mut found = HashMap::new();

        if by_app.is_empty() {
            return found;
        }

        let routes = self.effective().0;
        let rollouts = self.store.rollouts(None).unwrap_or_default();

        for (app, id) in self.running.keys() {
            let share = self.traffic_share(app, id, &routes, &rollouts);

            if share == 0 {
                continue;
            }

            for ((capacity_app, group), capacity) in &by_app {
                if capacity_app != app {
                    continue;
                }

                let mut capacity = capacity.clone();
                capacity.floor = (capacity.floor * share).div_ceil(100);

                if capacity.cap > 0 {
                    capacity.cap = (capacity.cap * share).div_ceil(100).max(1);
                }

                found.insert((app.clone(), id.clone(), group.clone()), capacity);
            }
        }

        found
    }

    /// The percentage of its application's requests a release takes: its
    /// rollout or policy share on the route that gives it the most, the
    /// rest for the release that serves, and none for another.
    fn traffic_share(
        &self,
        app: &str,
        id: &str,
        routes: &[muru_config::Route],
        rollouts: &[Rollout],
    ) -> u32 {
        let serves = self.serving.get(app).is_some_and(|serving| serving == id);
        let rollout = rollouts
            .iter()
            .find(|rollout| rollout.app == app && rollout.state.is_active());
        let mut best = None;

        for route in routes
            .iter()
            .filter(|route| route_apps(route).first() == Some(&app))
        {
            let mut shares: BTreeMap<&str, u32> = route
                .traffic
                .iter()
                .flat_map(|traffic| traffic.split.iter())
                .filter(|(release, _)| !release.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
                .map(|(release, percent)| (release.as_str(), (*percent).min(100)))
                .collect();

            if let Some(rollout) = rollout {
                shares.insert(rollout.candidate.as_str(), rollout.percent().min(100));
            }

            let named: u32 = shares.values().sum();

            let share = match shares.get(id) {
                Some(share) => *share,
                None if serves => 100u32.saturating_sub(named),
                None => 0,
            };

            best = Some(best.unwrap_or(0).max(share));
        }

        // Without a route, a rollout of the default application shares
        // its requests all the same.

        match (best, rollout) {
            (Some(best), _) => best,
            (None, Some(rollout)) if rollout.candidate == id => rollout.percent().min(100),
            (None, Some(rollout)) if serves => 100u32.saturating_sub(rollout.percent().min(100)),
            (None, _) => {
                if serves {
                    100
                } else {
                    0
                }
            }
        }
    }

    /// Ask a release's host to bring a group to a worker count; the host
    /// announces the sockets as they change.
    /// Section 7.4 and issue 384: the budget in force. The daemon file's
    /// limits win; `memory` and `cpu` it leaves out default to the
    /// daemon's own cgroup's, read now, so an in-place resize applies to
    /// the next decision: `memory.max` less the headroom, and the quota
    /// of `cpu.max`. `workers` has no default.
    fn budget(&self) -> crate::budget::Budget {
        let mut budget = self.budget_table;

        let Some(limits) = self.cgroup.as_ref().map(crate::cgroup::Limits::again) else {
            return budget;
        };

        if budget.memory.is_none() {
            budget.memory = limits
                .memory_max
                .map(|limit| limit.saturating_sub(crate::cgroup::headroom(limit)));
        }

        if budget.cpu.is_none() {
            budget.cpu = limits
                .cpu_max
                .map(|(quota, period)| quota as f64 / period.max(1) as f64);
        }

        budget
    }

    /// Where the budget's limits come from, for status: the cgroup's
    /// files for those the daemon file leaves out.
    fn budget_sources(&self) -> String {
        let Some(limits) = self.cgroup.as_ref().map(crate::cgroup::Limits::again) else {
            return String::new();
        };

        let mut from = Vec::new();

        if self.budget_table.memory.is_none()
            && let Some(limit) = limits.memory_max
        {
            from.push(format!(
                "memory from memory.max {} less {} of headroom",
                muru_config::format_size_approx(limit),
                muru_config::format_size_approx(crate::cgroup::headroom(limit))
            ));
        }

        if self.budget_table.cpu.is_none()
            && let Some((quota, period)) = limits.cpu_max
        {
            from.push(format!("cpu from cpu.max {quota} {period}"));
        }

        if from.is_empty() {
            String::new()
        } else {
            format!(" ({})", from.join(", "))
        }
    }

    /// Sections 7.7 and 13.9: at every memory check, the shortest
    /// `check_interval` of the running releases, read the daemon's
    /// cgroup again and, with less than the headroom left under its
    /// `memory.max`, have the largest worker of every release replaced,
    /// or its group scaled in by it when its policy allows (issue 622).
    fn guard_memory(&mut self) {
        let Some(limits) = &self.cgroup else {
            return;
        };

        let now = Instant::now();

        if self
            .guard_checked
            .is_some_and(|(at, interval)| now.saturating_duration_since(at) < interval)
        {
            return;
        }

        let read = limits.again();
        let changes = read.changes(limits);

        if !changes.is_empty() {
            tracing::info!("cgroup limits changed: {}", changes.join(", "));
        }

        self.cgroup = Some(read.clone());

        // Every release's workers, each measured by its group's policy.

        let mut workers = Vec::new();
        let mut hosts = 0;
        let mut interval = 5_000;

        for ((app, id), running) in &self.running {
            hosts += running.host_pss.unwrap_or(0);
            let spec = self.store.release(app, id).ok().map(|release| release.spec);

            if let Some(spec) = &spec {
                interval = interval.min(spec.memory.check_interval_ms);
            }

            for worker in &running.status {
                let policy = spec
                    .as_ref()
                    .map(|spec| {
                        spec.groups
                            .iter()
                            .find(|group| group.name == worker.group)
                            .and_then(|group| group.memory.clone())
                            .unwrap_or_else(|| spec.memory.clone())
                    })
                    .unwrap_or_default();

                let (size, measure) = match policy.measure {
                    muru_config::Measure::Uss => (worker.uss, "uss"),
                    muru_config::Measure::Pss => (worker.pss, "pss"),
                    muru_config::Measure::Rss => (worker.rss, "rss"),
                };

                let name = muru_common::worker::WorkerName::new(&worker.group, worker.index);

                workers.push(crate::guard::Worker {
                    owner: format!("{app}@{id}"),
                    group: name.group.clone(),
                    index: worker.index,
                    name: name.to_string(),
                    pid: worker.pid,
                    size: size.unwrap_or(0),
                    measure,
                    replacing: false,
                });
            }

            // Issue 662: a worker being replaced for memory is the guard's
            // one at a time.

            for replacement in &running.memory_replacements {
                let name =
                    muru_common::worker::WorkerName::new(&replacement.group, replacement.index);

                workers.push(crate::guard::Worker {
                    owner: format!("{app}@{id}"),
                    group: name.group.clone(),
                    index: replacement.index,
                    name: name.to_string(),
                    pid: replacement.pid,
                    size: 0,
                    measure: "",
                    replacing: true,
                });
            }
        }

        self.guard_checked = Some((now, Duration::from_millis(interval.max(250))));
        let memory = read.memory();

        let action = match memory {
            Some(memory) => self.guard.check(memory, &workers, hosts),
            None => crate::guard::Action::Nothing,
        };

        *self.info.cgroup_limits.lock().expect("limits poisoned") = Some(crate::guard::report(
            &read,
            memory,
            workers.iter().map(|worker| worker.size).sum(),
            hosts,
            Some(&self.guard),
        ));

        match action {
            crate::guard::Action::Nothing => {}
            crate::guard::Action::Hold(warning) => tracing::warn!("{warning}"),
            crate::guard::Action::Replace {
                owner,
                group,
                index,
                pid,
                reason,
            } => self.guard_replace(&owner, &group, index, pid, &reason, &workers),
        }
    }

    /// The guard chose a worker: its group is scaled in by it when the
    /// group has a policy above its fewest workers, else it is replaced.
    fn guard_replace(
        &mut self,
        owner: &str,
        group: &str,
        index: u32,
        pid: u32,
        reason: &str,
        workers: &[crate::guard::Worker],
    ) {
        let Some((app, id)) = owner.split_once('@') else {
            return;
        };

        let key = (app.to_owned(), id.to_owned());
        let current = workers
            .iter()
            .filter(|worker| worker.owner == owner && worker.group == group && !worker.replacing)
            .count() as u32;
        let mut shrunk = None;

        if let Some(running) = self.running.get_mut(&key)
            && running.retained_until.is_none()
            && let Some(scaling) = running
                .scalers
                .iter_mut()
                .find(|scaling| scaling.name == group)
        {
            let spec = scaling.scaler.spec();

            if !spec.is_static()
                && !spec.advisory
                && scaling.scaler.pinned().is_none()
                && current > spec.min_workers
            {
                let to = current - 1;
                let why = format!("for memory: {reason}");
                scaling.scaler.shrink(current, to, Instant::now(), &why);
                scaling.requested = Some(to);
                shrunk = Some(decision_message(group, current, to, &why, false));
            }
        }

        if let Some(message) = &shrunk {
            self.guard.scaled_in();
            self.events.emit("scaling.decision", owner, "info", message);
        }

        let Some(control) = self
            .running
            .get(&key)
            .and_then(|running| running.control.as_ref())
        else {
            return;
        };

        self.last_request += 1;
        control.request(
            self.last_request,
            "recycle",
            Some(&muru_common::worker::WorkerName::new(group, index)),
            &serde_json::json!({
                "pid": pid,
                "reason": reason,
                "cause": "container_memory",
                "retire": shrunk.is_some(),
            }),
        );
    }

    fn ask_scale(&mut self, app: &str, id: &str, group: &str, processes: u32) {
        let Some(control) = self
            .running
            .get(&(app.to_owned(), id.to_owned()))
            .and_then(|running| running.control.as_ref())
        else {
            return;
        };

        self.last_request += 1;
        control.request(
            self.last_request,
            "scale",
            None,
            &serde_json::json!({ "group": group, "processes": processes }),
        );
    }

    /// Section 7.4: pin a group's worker count, so the autoscaler stands
    /// aside, or let it decide again; the pin outlives the daemon.
    fn set_scale(
        &mut self,
        app: &str,
        id: &str,
        group: &str,
        processes: Option<u32>,
        unpin: bool,
    ) -> Result<Vec<(String, ScaleStatus)>, String> {
        let now = Instant::now();
        let key = (app.to_owned(), id.to_owned());

        if self.starting.contains_key(&key) {
            return Err(starting_refusal(app, id));
        }

        let Some(running) = self.running.get_mut(&key) else {
            return Err(format!("release {app}@{id} is not running"));
        };

        if running.backend.servers().is_some() {
            return Err(format!(
                "release {app}@{id} runs a managed HTTP server, whose instance count is fixed"
            ));
        }

        let Some(scaling) = running
            .scalers
            .iter_mut()
            .find(|scaling| scaling.name == group)
        else {
            return Err(format!("release {app}@{id} has no worker group {group}"));
        };

        let subject = format!("{app}@{id}");

        if unpin {
            if scaling.scaler.pinned().is_none() {
                return Err(format!("{group} is not pinned"));
            }

            scaling.scaler.unpin(now);
            self.store
                .remove_scale_pins(app, id, Some(group))
                .map_err(|error| error.to_string())?;
            self.events.emit(
                "scaling.decision",
                &subject,
                "notice",
                &format!(
                    "{group}: unpinned; the autoscaler decides again from {} workers",
                    scaling.scaler.desired()
                ),
            );
        } else {
            let Some(count) = processes else {
                return Err("give --processes COUNT or --unpin".to_owned());
            };
            let spec = scaling.scaler.spec();

            if count == 0 {
                return Err("processes must be at least 1".to_owned());
            }

            if !spec.is_static() && count > scaling.scaler.ceiling() {
                return Err(format!(
                    "{count} is over the group's max_workers of {}",
                    scaling.scaler.ceiling()
                ));
            }

            scaling.scaler.pin(count, now);
            scaling.requested = Some(count);
            self.store
                .put_scale_pin(app, id, group, count)
                .map_err(|error| error.to_string())?;
            self.events.emit(
                "scaling.decision",
                &subject,
                "notice",
                &format!("{group}: pinned to {count} workers by an operator"),
            );
            self.ask_scale(app, id, group, count);
        }

        self.scale_status(app, id)
    }

    /// Section 4.12: put an application into maintenance mode or take it
    /// out, recorded so a daemon restart keeps it, announced as an event,
    /// and pushed to the edge.
    async fn set_maintenance(&mut self, app: &str, on: bool) -> Result<Option<u64>, String> {
        self.store.app(app).map_err(|error| error.to_string())?;
        let was = self.maintenance.get(app).copied();

        match (on, was) {
            (true, Some(since)) => return Ok(Some(since)),
            (false, None) => return Ok(None),
            (true, None) => {
                let since = crate::model::now_unix_ms();
                self.store
                    .set_maintenance(app, Some(since))
                    .map_err(|error| error.to_string())?;
                self.maintenance.insert(app.to_owned(), since);
                self.events.emit(
                    "maintenance.changed",
                    app,
                    "notice",
                    "maintenance on: the edge serves the maintenance page",
                );
            }
            (false, Some(_)) => {
                self.store
                    .set_maintenance(app, None)
                    .map_err(|error| error.to_string())?;
                self.maintenance.remove(app);
                self.events.emit(
                    "maintenance.changed",
                    app,
                    "notice",
                    "maintenance off: the application serves again",
                );
            }
        }

        self.push_snapshot().await?;
        Ok(self.maintenance.get(app).copied())
    }

    /// Section 4.12: put a virtual host into maintenance mode or take it
    /// out, recorded so a daemon restart keeps it, announced as an event,
    /// and pushed to the edge.
    async fn set_host_maintenance(&mut self, host: &str, on: bool) -> Result<Option<u64>, String> {
        let known = self
            .store
            .hosts()
            .map_err(|error| error.to_string())?
            .iter()
            .any(|(name, _)| name == host);

        if !known {
            return Err(format!("no virtual host named {host}"));
        }

        let was = self.host_maintenance.get(host).copied();
        let subject = format!("host {host}");

        match (on, was) {
            (true, Some(since)) => return Ok(Some(since)),
            (false, None) => return Ok(None),
            (true, None) => {
                let since = crate::model::now_unix_ms();
                self.store
                    .set_host_maintenance(host, Some(since))
                    .map_err(|error| error.to_string())?;
                self.host_maintenance.insert(host.to_owned(), since);
                self.events.emit(
                    "maintenance.changed",
                    &subject,
                    "notice",
                    "maintenance on: the edge serves the maintenance page for every route of the host",
                );
            }
            (false, Some(_)) => {
                self.store
                    .set_host_maintenance(host, None)
                    .map_err(|error| error.to_string())?;
                self.host_maintenance.remove(host);
                self.events.emit(
                    "maintenance.changed",
                    &subject,
                    "notice",
                    "maintenance off: the host serves again",
                );
            }
        }

        self.push_snapshot().await?;
        Ok(self.host_maintenance.get(host).copied())
    }

    /// Section 11.2: put a change to the access log in force, or put the
    /// changes back (every one, or those for one application), and send
    /// the edge the result; with neither, only say what is in force.
    async fn override_access_log(
        &mut self,
        change: Option<muru_edge::access::Override>,
        reset: Option<String>,
    ) -> Result<(Vec<muru_edge::access::Override>, usize), String> {
        self.expire_access_overrides().await;
        let now = std::time::SystemTime::now();
        let mut reset_count = 0;

        if let Some(app) = reset {
            let (put_back, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.access_overrides)
                .into_iter()
                .partition(|change| app.is_empty() || change.app == app);
            self.access_overrides = kept;
            reset_count = put_back.len();

            for change in &put_back {
                self.events.emit(
                    "access_log.reverted",
                    access_subject(change),
                    "notice",
                    &format!("the change to write {} was put back", change.describe()),
                );
            }
        }

        if let Some(change) = change {
            let left = change.until.duration_since(now).unwrap_or_default();
            self.events.emit(
                "access_log.changed",
                access_subject(&change),
                "notice",
                &format!(
                    "the access log writes {} for {}",
                    change.describe(),
                    muru_edge::access::format_lifetime(left)
                ),
            );
            self.access_overrides.push(change);
        } else if reset_count == 0 {
            return Ok((self.access_overrides.clone(), 0));
        }

        self.push_snapshot().await?;
        Ok((self.access_overrides.clone(), reset_count))
    }

    /// Section 11.2: drop the changes to the access log whose period has
    /// ended, saying so, and send the edge the rest.
    async fn expire_access_overrides(&mut self) {
        let now = std::time::SystemTime::now();

        if self
            .access_overrides
            .iter()
            .all(|change| change.in_force(now))
        {
            return;
        }

        let (kept, ended): (Vec<_>, Vec<_>) = std::mem::take(&mut self.access_overrides)
            .into_iter()
            .partition(|change| change.in_force(now));
        self.access_overrides = kept;

        for change in &ended {
            self.events.emit(
                "access_log.reverted",
                access_subject(change),
                "notice",
                &format!("the change to write {} ended", change.describe()),
            );
        }

        if let Err(error) = self.push_snapshot().await {
            tracing::warn!(%error, "the edge did not take the access log back");
        }
    }

    /// The autoscaler's state for each group of a running release.
    fn scale_status(&self, app: &str, id: &str) -> Result<Vec<(String, ScaleStatus)>, String> {
        let now = Instant::now();

        if self.starting.contains_key(&(app.to_owned(), id.to_owned())) {
            return Err(starting_refusal(app, id));
        }

        let Some(running) = self.running.get(&(app.to_owned(), id.to_owned())) else {
            return Err(format!("release {app}@{id} is not running"));
        };

        // A retained release is not the autoscaler's while it is warm
        // (section 9.1): it holds the workers it has.

        Ok(running
            .scalers
            .iter()
            .map(|scaling| {
                let mut status = scaling.scaler.status(now);

                if running.retained_until.is_some() || running.control.is_none() {
                    status.ready = running
                        .group_sockets(&scaling.name)
                        .map_or(0, <[PathBuf]>::len) as u32;
                    status.desired = status.ready;
                }

                (scaling.name.clone(), status)
            })
            .collect())
    }

    /// Whether the edge routes to the release at all: it serves, keeps
    /// sticky clients, is held, or a policy or rollout refers to it.
    fn routable(&self, app: &str, id: &str) -> bool {
        let key = (app.to_owned(), id.to_owned());
        let Some(running) = self.running.get(&key) else {
            return false;
        };
        self.serving.get(app) == Some(&id.to_owned())
            || running.sticky_until.is_some()
            || running.held
            || (running.retained_until.is_none() && self.referenced_releases().contains(&key))
    }

    /// The releases the route policies name and the candidates of the
    /// rollouts under way, by application.
    fn referenced_releases(&self) -> BTreeSet<(String, String)> {
        let mut referenced = referenced_in(&self.effective().0);
        referenced.extend(
            self.store
                .rollouts(None)
                .unwrap_or_default()
                .into_iter()
                .filter(|rollout| rollout.state.is_active())
                .map(|rollout| (rollout.app, rollout.candidate)),
        );
        referenced
    }

    /// Section 9.4: the longest sticky grace among the application's
    /// routes with a sticky policy, or none when no route is sticky.
    fn sticky_grace(&self, app: &str) -> Option<Duration> {
        self.store
            .routes()
            .unwrap_or_default()
            .iter()
            .filter(|route| route_apps(route).contains(&app))
            .filter_map(|route| route.traffic.as_ref())
            .filter(|traffic| traffic.sticky == Some(true))
            .map(sticky_grace_of)
            .filter(|grace| !grace.is_zero())
            .max()
    }

    /// A route's traffic policy for the edge, when it has one or its
    /// application has a rollout under way: the rollout's candidate share
    /// first, then the policy's shares of the releases the snapshot
    /// carries, newest first so the newest release's buckets are at the
    /// top (section 9.4), with the key, salt and stickiness.
    fn route_traffic(
        &self,
        route: &muru_config::Route,
        included: &BTreeSet<(String, String)>,
        rollouts: &[Rollout],
    ) -> Option<pb::Traffic> {
        let app = route_apps(route).first().copied().unwrap_or_default();
        let rollout = rollouts.iter().find(|rollout| {
            rollout.app == app && included.contains(&(app.to_owned(), rollout.candidate.clone()))
        });

        if route.traffic.is_none() && rollout.is_none() {
            return None;
        }

        let mut shares: Vec<pb::Share> = rollout
            .map(|rollout| pb::Share {
                release: rollout.candidate.clone(),
                weight: rollout.percent().min(100) * 100,
                upstream: String::new(),
            })
            .into_iter()
            .collect();
        let traffic = route.traffic.clone().unwrap_or_default();
        let mut declared: Vec<(u64, &String, u32)> = traffic
            .split
            .iter()
            .filter(|(id, _)| !id.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
            .filter(|(id, _)| included.contains(&(app.to_owned(), (*id).clone())))
            .filter(|(id, _)| rollout.is_none_or(|rollout| &rollout.candidate != *id))
            .map(|(id, percent)| {
                let created = self
                    .store
                    .release(app, id)
                    .map(|release| release.created_unix_ms)
                    .unwrap_or(0);
                (created, id, *percent)
            })
            .collect();
        declared.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
        shares.extend(declared.into_iter().map(|(_, release, percent)| pb::Share {
            release: release.clone(),
            weight: percent.min(100) * 100,
            upstream: String::new(),
        }));

        // Section 4.13: shares to upstreams come after the releases', in
        // name order.

        shares.extend(traffic.split.iter().filter_map(|(id, percent)| {
            let upstream = id.strip_prefix(muru_edge::routing::UPSTREAM_PREFIX)?;
            Some(pb::Share {
                release: String::new(),
                weight: (*percent).min(100) * 100,
                upstream: upstream.to_owned(),
            })
        }));

        // Section 4.14: a class goes to its release while the snapshot
        // carries it, else as the policy's other requests do.

        let classes = traffic
            .classes
            .iter()
            .filter(|(_, id)| {
                id.starts_with(muru_edge::routing::UPSTREAM_PREFIX)
                    || included.contains(&(app.to_owned(), (*id).clone()))
            })
            .map(|(class, id)| (class.clone(), id.clone()))
            .collect();

        Some(pb::Traffic {
            shares,
            classes,
            hash_key: traffic.hash_key.clone().unwrap_or_default(),
            salt: traffic
                .salt
                .clone()
                .unwrap_or_else(|| format!("{} {}", route.host, route.path)),
            sticky: traffic.sticky.unwrap_or(false),
            sticky_grace_s: sticky_grace_of(&traffic).as_secs(),
        })
    }

    /// Stop the retained releases whose window has passed.
    async fn expire_retained(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String)> = self
            .running
            .iter()
            .filter(|(_, running)| running.retained_until.is_some_and(|until| until <= now))
            .map(|(key, _)| key.clone())
            .collect();

        for (app, id) in expired {
            self.stop_release(&app, &id, "the retention window passed");
        }
    }

    /// Section 9.6: start the previous release again if it is not running,
    /// move traffic to it, and drain the one that served.
    async fn rollback(&mut self, app: &str, done: oneshot::Sender<Result<Release, String>>) {
        let checked = (|| {
            let record = self.store.app(app).map_err(|error| error.to_string())?;
            let Some(previous) = record.previous.clone() else {
                return Err(format!("{app} has no previous release to roll back to"));
            };

            if let Some(refusal) = crate::config::second_app_refusal(&self.store, app) {
                return Err(refusal);
            }

            Ok((previous, record.current.clone()))
        })();

        let (previous, current) = match checked {
            Ok(checked) => checked,
            Err(error) => {
                let _ = done.send(Err(error));
                return;
            }
        };

        self.start_then(
            app,
            &previous.clone(),
            AfterStart::Rollback {
                app: app.to_owned(),
                previous,
                current,
                done,
            },
        )
        .await;
    }

    /// What follows the start of the release a rollback returns to (issue
    /// 693): traffic moves to it, and the release that served drains.
    async fn rolled_back(
        &mut self,
        app: &str,
        previous: &str,
        current: Option<String>,
    ) -> Result<Release, String> {
        let previous = previous.to_owned();
        self.switch_to(app, &previous).await;
        self.store
            .set_app_releases(app, Some(&previous), current.as_deref())
            .map_err(|error| error.to_string())?;

        // ADR 0082: an apply reads this, and does not put back in traffic
        // the release a rollback took out of it.

        if let Some(from) = &current {
            self.store
                .set_serving_origin(
                    app,
                    crate::model::ServingSetBy::Rollback { from: from.clone() },
                )
                .map_err(|error| error.to_string())?;
        }

        self.events.emit(
            "app.rolled-back",
            app,
            "notice",
            &format!("rolled back to {previous}"),
        );

        if let Some(current) = current.filter(|current| current != &previous)
            && !self
                .referenced_releases()
                .contains(&(app.to_owned(), current.clone()))
        {
            self.drain(app, &current, Retention::default()).await;
        }

        self.store
            .release(app, &previous)
            .map_err(|error| error.to_string())
    }

    /// Take a release out of traffic, if it serves, and stop it.
    async fn drain_release(&mut self, app: &str, id: &str) -> Result<Release, String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        self.store
            .release(app, id)
            .map_err(|error| error.to_string())?;

        // Issue 693: a release still starting has its start cancelled,
        // which stops its host if one has started; it is stopping until
        // the start has reported, and stopped then.

        let cancelled = self.cancel_start(app, id, "the release was drained").await;

        if record.current.as_deref() == Some(id) {
            self.clear_traffic(app).await;
            self.store
                .set_app_releases(app, None, Some(id))
                .map_err(|error| error.to_string())?;
            self.events
                .emit("traffic.switched", app, "notice", "no release serves");
        }

        if cancelled {
            let release = self
                .store
                .release(app, id)
                .map_err(|error| error.to_string())?;

            if self.starting.contains_key(&(app.to_owned(), id.to_owned())) {
                self.set_state(
                    &release,
                    ReleaseState::Stopping,
                    "its start is being cancelled: the release was drained",
                );
            } else if !release.state.is_final() {
                // A start waiting for its turn had started nothing.

                self.set_state(
                    &release,
                    ReleaseState::Stopped,
                    "its start was cancelled: the release was drained",
                );
            }

            return self
                .store
                .release(app, id)
                .map_err(|error| error.to_string());
        }

        self.drain(
            app,
            id,
            Retention {
                lasts: Duration::ZERO,
                ..Retention::default()
            },
        )
        .await;

        self.store
            .release(app, id)
            .map_err(|error| error.to_string())
    }

    /// Section 9.1: drop a release's record, unless it is protected now.
    fn forget_release(&self, app: &str, id: &str) -> Result<(), String> {
        let record = self.store.app(app).map_err(|error| error.to_string())?;
        let release = self
            .store
            .release(app, id)
            .map_err(|error| error.to_string())?;
        let candidate = crate::removal::Candidate {
            id: id.to_owned(),
            state: release.state,
            created_unix_ms: release.created_unix_ms,
            declared: false,
            referenced: self
                .referenced_releases()
                .contains(&(app.to_owned(), id.to_owned())),
        };

        if let Some(why) = crate::removal::protection(
            record.current.as_deref(),
            record.previous.as_deref(),
            &candidate,
        ) {
            return Err(format!("{app}@{id} cannot be removed: {}", why.describe()));
        }

        let key = (app.to_owned(), id.to_owned());

        if self.running.contains_key(&key) || self.starting.contains_key(&key) {
            return Err(format!("{app}@{id} cannot be removed: it is still running"));
        }

        if self.stops.contains_key(&key) {
            return Err(format!(
                "{app}@{id} cannot be removed: it is still stopping; try again when it has stopped"
            ));
        }

        self.store
            .delete_release(app, id)
            .map_err(|error| error.to_string())
    }

    /// Section 9.1: why an application cannot be removed now, if so: a
    /// route still sends it traffic, a rollout is under way, or a release
    /// of it is being built outside a start (a start under way is
    /// cancelled by the removal instead).
    fn app_removal_refused(&self, app: &str) -> Option<String> {
        let (routes, _) = self.effective();

        for route in self
            .store
            .routes()
            .unwrap_or_default()
            .iter()
            .chain(&routes)
        {
            if route_apps(route).contains(&app) {
                return Some(format!(
                    "route {} {} sends traffic to {app}; remove the route first",
                    route.host,
                    route.key()
                ));
            }
        }

        if let Ok(Some(rollout)) = self.store.active_rollout(app) {
            return Some(format!(
                "a rollout of {app} to {} is under way; promote or abort it first",
                rollout.candidate
            ));
        }

        if let Some(run) = self
            .store
            .runs(app, None)
            .unwrap_or_default()
            .into_iter()
            .find(crate::model::RunRecord::under_way)
        {
            return Some(format!(
                "run {} of {} is under way; stop it with muructl command stop {} first",
                run.id, run.name, run.id
            ));
        }

        self.store
            .releases(Some(app))
            .unwrap_or_default()
            .iter()
            .find(|release| {
                // Issue 693: a start under way is cancelled by the
                // removal, so only a build outside one refuses it.

                let key = (app.to_owned(), release.id.clone());
                let under_way = self.starting.contains_key(&key)
                    || self
                        .start_queue
                        .get(app)
                        .is_some_and(|queue| queue.iter().any(|(id, _)| *id == release.id));

                !under_way
                    && matches!(
                        release.state,
                        ReleaseState::Pending | ReleaseState::Building
                    )
            })
            .map(|release| {
                format!(
                    "{app}@{} is {}; wait for it to finish",
                    release.id, release.state
                )
            })
    }

    /// Section 9.1: remove an application, answering once its releases'
    /// stops have ended (issue 699).
    async fn remove_app_then(&mut self, app: String, done: oneshot::Sender<Result<(), String>>) {
        if let Err(error) = self.remove_app(&app).await {
            let _ = done.send(Err(error));
            return;
        }

        match self.stops.iter_mut().find(|((name, _), _)| *name == app) {
            Some((_, stop)) => stop.waiters.push(AfterStop::RemoveApp(app, done)),
            None => {
                let _ = done.send(self.forget_app(&app).await);
            }
        }
    }

    /// Section 9.1: drain whatever an application runs, its removal
    /// refused if it cannot go now.
    async fn remove_app(&mut self, app: &str) -> Result<(), String> {
        if let Some(why) = self.app_removal_refused(app) {
            return Err(format!("{app} cannot be removed: {why}"));
        }

        let releases = self
            .store
            .releases(Some(app))
            .map_err(|error| error.to_string())?;

        for release in &releases {
            let key = (app.to_owned(), release.id.clone());

            if self.running.contains_key(&key) || !release.state.is_final() {
                self.drain_release(app, &release.id).await?;
            }
        }

        Ok(())
    }

    /// Section 9.1: drop an application's release records and the
    /// application, once nothing of it runs.
    async fn forget_app(&mut self, app: &str) -> Result<(), String> {
        if let Some(((_, id), _)) = self.running.iter().find(|((name, _), _)| name == app) {
            return Err(format!("{app}@{id} did not stop"));
        }

        self.store
            .delete_app(app)
            .map_err(|error| error.to_string())?;
        self.serving.remove(app);
        self.maintenance.remove(app);

        // The edge forgets the application with its next snapshot.

        if let Err(error) = self.push_snapshot().await {
            tracing::warn!(%error, "pushing the snapshot after removing {app} failed");
        }

        Ok(())
    }

    /// A host that exited on its own fails its release; the release that
    /// serves is started again once.
    async fn check_hosts(&mut self) {
        let mut exited = Vec::new();
        let mut reports = Vec::new();
        let mut marks_changed = false;

        for (key, running) in &mut self.running {
            // Section 9.9: a managed server's instances are started
            // again where they crash; the release stays up, and an
            // instance started again is out of service as starting until
            // it answers (issue 917), which the edge hears in a snapshot.

            if let Backend::Servers(servers) = &mut running.backend {
                let marked = servers.starting();
                let changes = servers.check(&probed(&self.commands, &key.0, &key.1)).await;
                marks_changed |= servers.starting() != marked;

                if !changes.is_empty() {
                    let run_dir = self
                        .locations
                        .run
                        .path
                        .join("apps")
                        .join(&key.0)
                        .join(&key.1);
                    write_record(&run_dir.join("servers.toml"), &servers.record());
                }

                reports.extend(changes.into_iter().map(|report| (key.clone(), report)));
                continue;
            }

            if let Ok(Some(how)) = running.backend.try_exited() {
                exited.push((key.clone(), how));
            }
        }

        for ((app, id), report) in reports {
            self.events.emit(
                report.kind,
                &format!("{app}@{id}"),
                report.level,
                &report.message,
            );
        }

        for ((app, id), how) in exited {
            self.running.remove(&(app.clone(), id.clone()));
            let subject = format!("{app}@{id}");
            let message = format!("the host {how}");

            if let Ok(release) = self.store.release(&app, &id) {
                self.set_state(&release, ReleaseState::Failed, &message);
            }

            self.events
                .emit("release.failed", &subject, "error", &message);

            let serving = self
                .store
                .app(&app)
                .ok()
                .and_then(|record| record.current)
                .as_deref()
                == Some(id.as_str());

            if serving {
                self.clear_traffic(&app).await;
                self.events.emit(
                    "release.restarting",
                    &subject,
                    "warning",
                    "starting the release again",
                );

                self.start_then(
                    &app,
                    &id,
                    AfterStart::Restart {
                        app: app.clone(),
                        id: id.clone(),
                    },
                )
                .await;
            }
        }

        if marks_changed && let Err(error) = self.push_snapshot().await {
            tracing::warn!(%error, "pushing the snapshot with the instances starting failed");
        }

        self.publish_servers();
    }

    /// Issue 922: an instance's relay reports its end, which marks the
    /// instance starting at once and tells the edge, so the requests
    /// that follow are held rather than refused while the relay reads
    /// the last of its output and the next check sees the exit.
    async fn server_ended(&mut self, app: &str, id: &str, index: u32, pid: u32) {
        let key = (app.to_owned(), id.to_owned());

        let Some(Running {
            backend: Backend::Servers(servers),
            ..
        }) = self.running.get_mut(&key)
        else {
            return;
        };

        if servers.ended(index, pid)
            && let Err(error) = self.push_snapshot().await
        {
            tracing::warn!(%error, "pushing the snapshot with an instance ended failed");
        }
    }

    /// Issue 917: an instance started again answered its probe, or did
    /// not within the readiness timeout; either way its starting mark is
    /// cleared, and the edge told, unless the probe was of a process the
    /// instance no longer has.
    async fn server_probed(&mut self, app: &str, id: &str, index: u32, pid: u32, answered: bool) {
        let key = (app.to_owned(), id.to_owned());

        let Some(Running {
            backend: Backend::Servers(servers),
            ..
        }) = self.running.get_mut(&key)
        else {
            return;
        };

        let Some(report) = servers.probed(index, pid, answered) else {
            return;
        };

        if let Err(error) = self.push_snapshot().await {
            tracing::warn!(%error, "pushing the snapshot with an instance ready failed");
        }

        self.events.emit(
            report.kind,
            &format!("{app}@{id}"),
            report.level,
            &report.message,
        );
    }

    /// Section 8.7: what a release's processes start from, from the
    /// daemon's own environment and whom they run as.
    fn daemon_base(
        &self,
        bytecode: Option<muru_config::Bytecode>,
        identity: Option<crate::host::Identity>,
    ) -> Vec<(String, String)> {
        let pass_env = self
            .info
            .pass_env
            .read()
            .expect("pass_env poisoned")
            .clone();

        crate::commands::passed_from_daemon(std::env::vars(), &pass_env, bytecode, identity)
    }

    /// A managed server's whole environment: its release's variables over
    /// the clean base, with the release's interpreter first on PATH.
    /// How a managed server's instances' log relays are started, from
    /// this program, with their events under the release.
    fn server_relays(&self, subject: &str) -> Result<crate::servers::Relays, String> {
        Ok(crate::servers::Relays {
            executable: running_executable()
                .map_err(|error| format!("finding the executable to start relays from: {error}"))?,
            settings: self.relay_settings,
            release: subject.to_owned(),
            watcher: self.relay_watcher(subject),
        })
    }

    fn server_environment(
        &self,
        env: Vec<(String, String)>,
        app: &str,
        id: &str,
        spec: &crate::model::ReleaseSpec,
        identity: Option<crate::host::Identity>,
    ) -> Vec<(String, String)> {
        let base = self.daemon_base(None, identity);

        crate::commands::environment(
            env,
            &crate::commands::Interpreter {
                python: &spec.python,
                runtime: None,
                pycache: None,
                bytecode: None,
                base: Some(&base),
                places: &self.places(app, id, identity).variables(),
            },
            app,
            id,
            &spec.source,
        )
    }

    /// Publish what runs for each release, for the release objects the
    /// API answers with: after everything the daemon does, so a release
    /// that stops leaves the map at once.
    fn publish_live(&self) {
        let live: HashMap<(String, String), LiveRelease> = self
            .running
            .iter()
            .map(|(key, running)| {
                (
                    key.clone(),
                    LiveRelease {
                        host: running.backend.host().is_some(),
                        announced: running.announced,
                        workers: running.workers.clone(),
                        retained_until_unix_ms: running.retained_until_unix_ms,
                    },
                )
            })
            .collect();

        *self.info.live.lock().expect("live poisoned") = live;
    }

    /// Publish each managed server release's instances, for `release
    /// show`: on every check, so a release that stops leaves the list
    /// within one.
    fn publish_servers(&self) {
        let servers = self
            .running
            .iter()
            .filter_map(|(key, running)| match &running.backend {
                Backend::Servers(servers) => Some((key.clone(), servers.record().instances)),
                _ => None,
            })
            .collect();
        *self.info.servers.lock().expect("servers poisoned") = servers;
    }

    /// Push a snapshot without the application's workers: its serving
    /// release has just been unset or is being restarted.
    async fn clear_traffic(&mut self, app: &str) {
        self.serving.remove(app);

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the snapshot failed");
        }
    }

    /// An edge that died is started again on the same listener with the
    /// last snapshot (section 2.4).
    async fn check_edge(&mut self) {
        match self.edge.check().await {
            Ok(None) => {}
            Ok(Some(applied)) => {
                self.info.edge_pid.store(
                    self.edge.pid().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.restarted",
                    "edge",
                    "warning",
                    "the edge process exited and was started again on the same listener",
                );
                self.replace_unreachable(applied);
            }
            Err(error) => {
                self.events
                    .emit("edge.failed", "edge", "error", &format!("{error:#}"));
            }
        }
    }

    /// Section 5.9: connect to a running host's control socket in the
    /// background and feed what it says into the daemon's inbox.
    fn attach_control(&self, app: &str, id: &str) {
        let Some(host) = self
            .running
            .get(&(app.to_owned(), id.to_owned()))
            .and_then(|running| running.backend.host())
        else {
            return;
        };

        let path = host.control_path();
        let commands = self.commands.clone();
        let (app, id) = (app.to_owned(), id.to_owned());

        tokio::spawn(async move {
            let (control, mut messages) = match HostControl::connect(&path).await {
                Ok(connected) => connected,
                Err(error) => {
                    tracing::info!(%error, "release {app}@{id}: no host control connection");
                    return;
                }
            };

            if commands
                .send(Command::HostAttached {
                    app: app.clone(),
                    id: id.clone(),
                    control,
                })
                .await
                .is_err()
            {
                return;
            }

            while let Some(message) = messages.recv().await {
                let sent = commands
                    .send(Command::HostMessage {
                        app: app.clone(),
                        id: id.clone(),
                        message,
                    })
                    .await;

                if sent.is_err() {
                    break;
                }
            }
        });
    }

    /// Act on what a host said: new worker sockets go into the record
    /// and, when the release serves, into the next snapshot, and are
    /// acknowledged; events are recorded under the release.
    async fn host_message(&mut self, app: &str, id: &str, message: HostMessage) {
        let key = (app.to_owned(), id.to_owned());

        match message {
            HostMessage::Workers {
                version,
                sockets,
                groups,
                interfaces,
                workers,
            } => {
                self.info.interfaces.announce(app, id, interfaces);

                let Some(running) = self.running.get_mut(&key) else {
                    return;
                };

                running.workers = workers;
                running.announced = true;

                // Section 7.4: the scalers were sized from the
                // configuration when the release started, which may have
                // left the interface to be detected; they follow what
                // the host found.

                if let Ok(release) = self.store.release(app, id) {
                    let interface =
                        self.info
                            .interfaces
                            .runs_as(app, id, "", release.spec.interface);
                    size_scalers(&mut running.scalers, &release.spec, interface);
                }

                let Some((record, pid)) = running.backend.host().map(|host| {
                    (
                        host.run_dir().join("..").join("host.toml"),
                        host.pid().unwrap_or(0),
                    )
                }) else {
                    return;
                };
                let before: BTreeSet<PathBuf> = running.routed_sockets().into_iter().collect();
                running.sockets = sockets;

                for (name, group_sockets) in groups {
                    if let Some(group) = running.groups.iter_mut().find(|group| group.name == name)
                    {
                        group.sockets = group_sockets;
                    }
                }

                let all = running.routed_sockets();

                // Section 7.4: a socket new to the routing is a worker
                // that just became ready.

                let now = Instant::now();
                running.ready_at.retain(|socket, _| all.contains(socket));

                for socket in &all {
                    running.ready_at.entry(socket.clone()).or_insert(now);
                }

                // The host's first announcement on a control connection
                // repeats what the daemon read from the run directory when
                // the release started, just after a deploy answers. A push
                // for it would cost an edge apply and leave out a worker
                // that happens to be busy (section 5.1), for a routing that
                // is already in place, so one that changes nothing the edge
                // routes by is acknowledged without a push.

                let changed = before != all.iter().cloned().collect::<BTreeSet<PathBuf>>();
                write_record(
                    &record,
                    &HostRecord {
                        pid,
                        sockets: all,
                        groups: running.groups.clone(),
                        runtime: read_record::<HostRecord>(&record)
                            .and_then(|previous| previous.runtime),
                    },
                );

                if changed
                    && self.routable(app, id)
                    && let Err(error) = self.push_snapshot().await
                {
                    tracing::error!(%error, "pushing the snapshot after a worker change failed");
                    return;
                }

                if let Some(control) = self
                    .running
                    .get(&key)
                    .and_then(|running| running.control.as_ref())
                {
                    control.acknowledge(version);
                }
            }
            HostMessage::Status {
                workers,
                memory_replacements,
                ..
            } => {
                if let Some(running) = self.running.get_mut(&key) {
                    // Section 7.3: the workers' own signals, from what
                    // grew since a report at least a few seconds older,
                    // since reports can arrive moments apart.

                    let now = Instant::now();

                    match &running.signals_from {
                        None => running.signals_from = Some((now, workers.clone())),
                        Some((at, _)) if now.saturating_duration_since(*at) < SIGNAL_WINDOW => {}
                        Some((at, before)) => {
                            let elapsed = now.saturating_duration_since(*at);

                            for group in &mut running.scalers {
                                let signals = crate::scaling::WorkerSignals::between(
                                    before,
                                    &workers,
                                    &group.name,
                                    elapsed,
                                    group.threads,
                                );
                                let near = group.memory_limit.is_some_and(|(measure, limit)| {
                                    workers
                                        .iter()
                                        .filter(|worker| worker.group == group.name)
                                        .filter_map(|worker| match measure {
                                            muru_config::Measure::Uss => worker.uss,
                                            muru_config::Measure::Pss => worker.pss,
                                            muru_config::Measure::Rss => worker.rss,
                                        })
                                        .any(|used| {
                                            used.saturating_mul(10) >= limit.saturating_mul(9)
                                        })
                                });
                                group.scaler.set_workers(signals, near);
                            }

                            running.signals_from = Some((now, workers.clone()));
                        }
                    }

                    // Section 7.4: the host is measured when its workers
                    // are, for the node's memory budget.

                    if workers.iter().any(|worker| worker.pss.is_some()) {
                        running.host_pss = running
                            .backend
                            .host()
                            .and_then(HostProcess::pid)
                            .and_then(crate::budget::process_pss);
                    }

                    running.status = workers;
                    running.memory_replacements = memory_replacements;
                }
            }
            HostMessage::Response { id, workers, error } => {
                if let Some(done) = self.asked.remove(&id) {
                    let _ = done.send(if error.is_empty() {
                        Ok(workers)
                    } else {
                        Err(error)
                    });
                }
            }
            // Section 11.7: only a development run keeps request records,
            // so the daemon has nothing to join a capture to.
            HostMessage::Capture(_) => {}
            HostMessage::Loaded {
                outcome,
                ms,
                imports,
                first,
            } => {
                // Issue 753: the host's load, an event the first time it
                // says so and a gauge from then on.

                if let Some(running) = self.running.get_mut(&key) {
                    running.load_ms = Some(ms);
                }

                if first {
                    self.events.emit(
                        "release.loaded",
                        &format!("{app}@{id}"),
                        "info",
                        &crate::host::loaded_message(&outcome, ms, imports.as_ref()),
                    );
                }
            }
            // Section 6.18: the load's import report is kept in the store
            // beside the release, which removing the release removes.
            HostMessage::Imports { part, parts, data } => {
                let whole = self
                    .imports_parts
                    .entry(key.clone())
                    .or_default()
                    .take(part, parts, data);

                if let Some(text) = whole {
                    self.imports_parts.remove(&key);

                    if let Err(error) = self.store.put_import_report(app, id, &text) {
                        tracing::warn!(%error, "release {app}@{id}: the import report cannot be kept");
                    }
                }
            }
            HostMessage::Event {
                kind,
                level,
                message,
                group,
                reason,
                ..
            } => {
                let level = match level.as_str() {
                    "error" => "error",
                    "warning" => "warning",
                    "notice" => "notice",
                    _ => "info",
                };

                // Section 11.1: a replaced worker and an exit are counted
                // by group and reason.

                if let Some(running) = self.running.get_mut(&key) {
                    let counts = match kind.as_str() {
                        "worker.recycled" | "worker.timeout_killed" => Some(&mut running.recycles),
                        "worker.exited" | "worker.oom_killed" => Some(&mut running.exits),
                        _ => None,
                    };

                    if let Some(counts) = counts {
                        let group = if group.is_empty() {
                            muru_edge::routing::DEFAULT_GROUP.to_owned()
                        } else {
                            group
                        };

                        let reason = if reason.is_empty() {
                            "other".to_owned()
                        } else {
                            reason
                        };
                        *counts.entry((group, reason)).or_default() += 1;
                    }
                }

                // Section 9.3: a crash or a timeout kill counts against
                // the release in a rollout's gates.

                if kind == "worker.timeout_killed" || (kind == "worker.exited" && level != "info") {
                    *self
                        .worker_faults
                        .entry((app.to_owned(), id.to_owned()))
                        .or_default() += 1;
                }

                self.events
                    .emit(&kind, &format!("{app}@{id}"), level, &message);
            }
        }
    }

    /// Section 13.4: replace the edge with one from this daemon's image,
    /// on the same listeners or on `listeners` when a reload changed
    /// them (section 13.3); the old one finishes its connections and
    /// exits. `where_` says which in the event. Returns the old edge's
    /// pid; on failure the old edge keeps serving, and the event says so.
    async fn replace_edge(
        &mut self,
        listeners: Option<Vec<crate::edges::Listener>>,
        where_: &str,
    ) -> Result<u32, String> {
        let grace = self.drain_grace();

        match self.edge.replace(grace, listeners).await {
            Ok((old_pid, applied)) => {
                let pid = self.edge.pid().unwrap_or(0);
                self.info
                    .edge_pid
                    .store(pid, std::sync::atomic::Ordering::Relaxed);
                *self.info.listeners.write().expect("listeners poisoned") = self.edge.listeners();
                write_edge_record(&self.locations.run.path, &self.edge);
                self.events.emit(
                    "edge.replaced",
                    "edge",
                    "info",
                    &format!(
                        "edge pid {old_pid} replaced by pid {pid} on {where_}; the old edge finishes its connections and exits"
                    ),
                );
                self.replace_unreachable(applied);
                Ok(old_pid)
            }
            Err(error) => {
                let message = format!(
                    "the edge could not be replaced: {error:#}; the old edge keeps serving"
                );
                self.events.emit("edge.failed", "edge", "error", &message);
                Err(message)
            }
        }
    }

    /// Section 13.3: what a stop signal means to this daemon, said in the
    /// log; whether it shuts everything down.
    fn signalled(&self, signal: &str) -> bool {
        if self.shutdown.shuts_down() {
            tracing::info!("{signal}: shutting down ({})", self.shutdown.describe());
        } else {
            tracing::info!("{signal}: detaching; hosts and the edge keep serving");
        }

        self.shutdown.shuts_down()
    }

    /// The longest exit grace among the releases with workers, for what a
    /// shutdown's drain leaves of its budget (section 13.3).
    fn largest_exit_grace(&self) -> Duration {
        let largest = self
            .running
            .iter()
            .filter(|(_, running)| running.backend.host().is_some())
            .filter_map(|((app, id), _)| self.store.release(app, id).ok())
            .map(|release| release.spec.timeouts.exit_grace_ms)
            .max()
            .unwrap_or(0);

        Duration::from_millis(largest)
    }

    /// Section 13.3: the edge's part of a shutdown. For the policy's
    /// delay it keeps accepting, with every connection closing after its
    /// response, then it stops accepting and the requests in flight
    /// finish, within what the budget leaves for the workers' shutdown
    /// work and a moment for the edge. The edge itself stays up for the WebSockets until it is
    /// stopped last.
    async fn drain_edge(&mut self, policy: &crate::shutdown::Policy, deadline: Instant) {
        let grace = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(policy.delay)
            .saturating_sub(crate::shutdown::drain_reserve(self.largest_exit_grace()));

        match self.edge.drain(policy.delay, grace).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    "requests were still in flight when the drain's {} were up",
                    muru_config::format_duration(grace)
                );
            }
            Err(error) => {
                tracing::warn!(%error, "the edge could not be drained");
            }
        }
    }

    /// Stop every host, for shutdown: all at once, each given its workers'
    /// stop budget or what the shutdown's budget leaves, whichever ends
    /// first, and killed at `deadline` if it is still there.
    async fn stop_all_until(&mut self, deadline: Instant) {
        let running = std::mem::take(&mut self.running);
        self.serving.clear();

        if let Err(error) = self.push_snapshot().await {
            tracing::error!(%error, "pushing the empty snapshot failed");
        }

        let mut stopping = Vec::new();

        for ((app, id), running) in running {
            let release = self.store.release(&app, &id).ok();

            if let Some(release) = &release {
                self.set_state(release, ReleaseState::Stopping, "the daemon is stopping");
            }

            let budget = release
                .as_ref()
                .map(|release| release.spec.timeouts.stop_budget())
                .unwrap_or(DRAIN_TIMEOUT);
            let kill_at = deadline.min(Instant::now() + budget + crate::host::STOP_MARGIN);
            let run_dir = self.locations.run.path.join("apps").join(&app).join(&id);
            let record = running.backend.record_path(&run_dir);
            stopping.push(async move {
                running
                    .backend
                    .stop_until(kill_at, crate::shutdown::workers_deadline(deadline))
                    .await;
                forget_release_dir(&run_dir, &record);
                release
            });
        }

        for release in futures::future::join_all(stopping)
            .await
            .into_iter()
            .flatten()
        {
            self.set_state(&release, ReleaseState::Stopped, "the daemon stopped");
        }

        let _ = std::fs::remove_dir(self.locations.run.path.join("apps"));
    }

    /// Where a release's host writes its output, its directory created.
    fn host_log(&self, app: &str, id: &str) -> PathBuf {
        let path = self.locations.release_log(app, id);

        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        path
    }

    /// Section 10.7: one point of history a second, from the edge's
    /// counters, and the WebSockets open by release for status.
    async fn sample_history(&mut self, samples: Option<&[muru_edge::metrics::Sample]>) {
        let Some(samples) = samples else {
            return;
        };

        let workers = self
            .running
            .values()
            .map(|running| {
                running.sockets.len()
                    + running
                        .groups
                        .iter()
                        .map(|group| group.sockets.len())
                        .sum::<usize>()
            })
            .sum::<usize>() as u64;
        let reading = crate::dashboard::reading(samples, workers);
        let now = crate::model::now_unix_ms() / 1000;
        let mut websockets: HashMap<(String, String), u64> = HashMap::new();

        for sample in samples
            .iter()
            .filter(|sample| sample.name == "muru_websocket_connections")
        {
            let label = |name: &str| {
                sample
                    .labels
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default()
            };
            *websockets
                .entry((label("app"), label("release")))
                .or_default() += sample.value.max(0.0) as u64;
        }

        *self.info.websockets.lock().expect("websockets poisoned") = websockets;

        if let Some(point) = self.sampler.observe(now, reading) {
            self.history.record(point);
        }

        // Section 13.10: the live figures, from the same samples and what
        // each release's workers last reported.

        let reports: Vec<(String, String, Vec<crate::host::WorkerStatus>)> = self
            .running
            .iter()
            .map(|((app, id), running)| (app.clone(), id.clone(), running.status.clone()))
            .collect();
        self.info.process_figures.observe(samples, &reports);
    }

    /// Section 13.10: the processes the daemon supervises, the daemon
    /// first: the edge, each release's host, relay and workers as the
    /// host last announced them, managed server instances, and the
    /// commands it runs.
    fn processes(&self) -> Vec<crate::processes::Known> {
        use crate::processes::Known;

        let daemon = std::process::id();
        let mut known = vec![Known {
            pid: daemon,
            role: "daemon",
            what: crate::processes::daemon_phrase(self.locations.mode_word()),
            ..Known::default()
        }];

        if let Some(pid) = self.edge.pid() {
            let listeners = self.info.listeners.read().expect("listeners poisoned");
            known.push(Known {
                pid,
                parent: daemon,
                role: "edge",
                what: format!("listening on {}", crate::processes::addresses(&listeners)),
                ..Known::default()
            });
        }

        // Section 13.10: a host's row says what its release does with
        // traffic, so an idle instance still shows which one serves.

        let rollouts: Vec<Rollout> = self
            .store
            .rollouts(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|rollout| rollout.state.is_active())
            .collect();

        for ((app, id), running) in &self.running {
            let subject = format!("{app}@{id}");

            match &running.backend {
                Backend::Host(host) => {
                    let Some(host_pid) = host.pid() else {
                        continue;
                    };

                    let (state, phrase) = self.routing_state(app, id, running, &rollouts);

                    let mut groups: Vec<String> = running
                        .groups
                        .iter()
                        .map(|group| group.name.clone())
                        .collect();

                    if !running.sockets.is_empty() || groups.is_empty() {
                        groups.insert(0, "default".to_owned());
                    }

                    // What the default mount runs as chooses the figure a
                    // worker's GIL/LAG shows.

                    let interface = crate::processes::default_interface(
                        &self.info.interfaces.announced(app, id),
                    );

                    known.push(Known {
                        pid: host_pid,
                        parent: daemon,
                        role: "host",
                        what: crate::processes::host_phrase(&subject, &phrase),
                        app: app.clone(),
                        release: id.clone(),
                        groups,
                        interface: interface.clone(),
                        state,
                        ..Known::default()
                    });

                    if let Some(pid) = host.relay_pid() {
                        let log = self.locations.release_log(app, id);
                        let shown = log
                            .strip_prefix(&self.locations.logs.path)
                            .unwrap_or(&log)
                            .display()
                            .to_string();
                        known.push(Known {
                            pid,
                            parent: host_pid,
                            spawned_by: daemon,
                            role: "relay",
                            what: format!("{subject} output to {shown}"),
                            app: app.clone(),
                            release: id.clone(),
                            ..Known::default()
                        });
                    }

                    for worker in &running.workers {
                        known.push(Known {
                            pid: worker.pid,
                            parent: host_pid,
                            role: "worker",
                            what: crate::processes::worker_phrase(
                                &worker.group,
                                worker.index,
                                &worker.state,
                                true,
                            ),
                            app: app.clone(),
                            release: id.clone(),
                            group: worker.group.clone(),
                            index: Some(worker.index),
                            socket: Some(worker.socket.clone()),
                            interface: interface.clone(),
                            ..Known::default()
                        });
                    }
                }
                Backend::Servers(servers) => {
                    let log = self.locations.release_log(app, id);
                    let shown = log
                        .strip_prefix(&self.locations.logs.path)
                        .unwrap_or(&log)
                        .display()
                        .to_string();

                    for instance in servers.record().instances {
                        if instance.pid == 0 {
                            continue;
                        }

                        // Issue 492: each instance's relay is its parent.

                        let parent = if instance.relay_pid == 0 {
                            daemon
                        } else {
                            known.push(Known {
                                pid: instance.relay_pid,
                                parent: daemon,
                                role: "relay",
                                what: format!("{subject} output to {shown}"),
                                app: app.clone(),
                                release: id.clone(),
                                ..Known::default()
                            });
                            instance.relay_pid
                        };

                        known.push(Known {
                            pid: instance.pid,
                            parent,
                            role: "server",
                            what: format!("{subject} instance {}", instance.index),
                            app: app.clone(),
                            release: id.clone(),
                            index: Some(instance.index),
                            program: servers.program().to_owned(),
                            ..Known::default()
                        });
                    }
                }
                Backend::Dormant => {}
            }
        }

        for command in crate::commands::running() {
            known.push(Known {
                pid: command.pid,
                parent: daemon,
                role: "command",
                what: format!("{}@{} {}", command.app, command.release, command.label),
                app: command.app,
                release: command.release,
                ..Known::default()
            });
        }

        known
    }

    /// A running release's part in routing, as a word for the answer and
    /// a phrase for its host's row: a rollout's candidate is `canary`
    /// with its share, the application's current release `serving`, one
    /// draining or retained says so, with when a retained one stops in
    /// local time, and a ready one that takes no traffic is `held`; one
    /// still starting says its state.
    fn routing_state(
        &self,
        app: &str,
        id: &str,
        running: &Running,
        rollouts: &[Rollout],
    ) -> (String, String) {
        if let Some(rollout) = rollouts
            .iter()
            .find(|rollout| rollout.app == app && rollout.candidate == id)
        {
            return (
                "canary".to_owned(),
                format!("canary {}%", rollout.percent().min(100)),
            );
        }

        let current = self
            .store
            .app(app)
            .ok()
            .and_then(|record| record.current)
            .is_some_and(|current| current == id);
        let state = self
            .store
            .release(app, id)
            .map(|release| release.state)
            .ok();

        let word = match state {
            Some(ReleaseState::Draining) => "draining",
            Some(ReleaseState::Retained) => "retained",
            _ if current => "serving",
            Some(ReleaseState::Ready) => "held",
            Some(other) => return (other.to_string(), other.to_string()),
            None => return (String::new(), String::new()),
        };

        // The time is UTC, with the zone, as `release list` and the
        // events give it (issue 618).

        let phrase = if word == "retained" && running.retained_until_unix_ms > 0 {
            format!("retained until {}", clock(running.retained_until_unix_ms))
        } else {
            word.to_owned()
        };

        (word.to_owned(), phrase)
    }

    /// The log files the daemon's writers keep: the access log, the
    /// edge's own log and the files virtual hosts name, which the edge
    /// reopens; the daemon's own, when it was started in the background;
    /// and each running release's log, which its relays reopen.
    fn log_writers(&self) -> Vec<PathBuf> {
        let mut writers = vec![self.locations.access_log(), self.locations.edge_log()];

        if self.owns_log {
            writers.push(self.locations.daemon_log());
        }

        let mut own: Vec<String> = self
            .store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, host)| host.access_log_file)
            .collect();
        own.sort();
        own.dedup();
        writers.extend(
            own.into_iter()
                .map(|file| self.locations.logs.path.join(file)),
        );

        for (app, id) in self.running.keys() {
            writers.push(self.locations.release_log(app, id));
        }

        writers
    }

    /// Section 11.3: reopen_on_rename. Every `rename_check`, see whether
    /// a log file its writers can reopen was renamed away, and if one was,
    /// have every writer open its file again by name, as `muructl logs
    /// reopen` does. A file being cut by a command is left to it.
    fn watch_renames(&mut self) {
        let now = Instant::now();

        if !self.reopen_on_rename || now < self.rename_due {
            return;
        }

        self.rename_due = now + self.rename_check;
        let paths: Vec<PathBuf> = self
            .log_writers()
            .into_iter()
            .filter(|path| !self.claimed_logs.contains(path))
            .collect();
        let renamed = self.renames.check(&paths);

        if !renamed.is_empty() {
            let names: Vec<String> = renamed
                .iter()
                .map(|path| {
                    path.strip_prefix(&self.locations.logs.path)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect();
            self.reopen_logs_because(false, &format!("{} renamed", names.join(", ")));
        }

        if let Some(seen) = self.renames.take_changed()
            && let Err(error) = self.store.set_log_files_seen(&seen)
        {
            tracing::warn!(%error, "recording the log files last seen failed");
        }
    }

    /// Section 11.3: rotate the log files that are due, by size or by the
    /// clock, then have every writer reopen its file by name.
    fn rotate_logs(&mut self) {
        if self.rotation.is_none() {
            return;
        }

        let candidates = self.log_writers();
        let Some(rotator) = self.rotation.as_mut() else {
            return;
        };
        let now = std::time::SystemTime::now();

        // The period each file is now in is recorded before anything is
        // rotated, so a daemon started after a crash does not rotate a
        // file a second time.

        let due: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|path| {
                !self.claimed_logs.contains(path) && path.is_file() && rotator.due(path, now)
            })
            .collect();
        let changed = rotator.take_changed();

        if !changed.is_empty()
            && let Err(error) = self.store.set_log_periods(&changed)
        {
            tracing::warn!(%error, "recording the log files' periods failed");
        }

        let mut rotated = Vec::new();

        for path in due {
            match rotator.rotate(&path, now) {
                Ok(archive) => rotated.push(archive),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "rotating a log failed");
                }
            }
        }

        self.after_rotation(&rotated);
    }

    /// Have the writers reopen what was rotated, and say so.
    fn after_rotation(&mut self, rotated: &[PathBuf]) {
        if rotated.is_empty() {
            return;
        }

        // The writers open new files, which the rename check notes as
        // they are rather than taking the rotation for a rename.

        self.renames.forget();

        self.reopen_own_log();

        self.edge.signal(libc::SIGUSR1);

        for running in self.running.values() {
            running.backend.reopen_log(false);
        }

        self.runs.reopen();

        let names: Vec<String> = rotated
            .iter()
            .map(|archive| {
                archive
                    .strip_prefix(&self.locations.logs.path)
                    .unwrap_or(archive)
                    .display()
                    .to_string()
            })
            .collect();
        self.events.emit(
            "logs.rotated",
            "logs",
            "info",
            &format!("rotated {}", names.join(", ")),
        );
    }

    /// Rotate named logs now, as a command asks.
    fn rotate_named_logs(
        &mut self,
        logs: Vec<(String, PathBuf)>,
    ) -> Result<Vec<RotatedLog>, String> {
        if self.rotation.is_none() {
            return Err(
                "log rotation is external ([logs] rotate = 'external'): whatever \
                 rotates the files does this, followed by muructl logs reopen"
                    .to_owned(),
            );
        }

        let now = std::time::SystemTime::now();
        let mut rotated = Vec::new();
        let mut archives = Vec::new();

        for (name, path) in logs {
            let archive = if self.claimed_logs.contains(&path) {
                Err("it is being cut by another command".to_owned())
            } else {
                let rotator = self.rotation.as_ref().expect("checked above");

                rotator
                    .rotate(&path, now)
                    .map(|archive| {
                        let relative = archive
                            .strip_prefix(&self.locations.logs.path)
                            .unwrap_or(&archive)
                            .display()
                            .to_string();
                        archives.push(archive);
                        relative
                    })
                    .map_err(|error| error.to_string())
            };

            rotated.push(RotatedLog { name, archive });
        }

        self.after_rotation(&archives);
        Ok(rotated)
    }

    /// Hold log files against rotation while a command cuts them.
    fn claim_logs(&mut self, paths: Vec<PathBuf>) -> Result<(), String> {
        if let Some(path) = paths.iter().find(|path| self.claimed_logs.contains(*path)) {
            return Err(format!(
                "{} is being cut by another command",
                path.display()
            ));
        }

        self.claimed_logs.extend(paths);
        Ok(())
    }

    /// What hears a release's relay: its stalls become events under the
    /// release, as its workers' do; counts are read from the host.
    fn relay_watcher(&self, subject: &str) -> crate::host::RelayWatcher {
        let events = self.events.clone();
        let commands = self.commands.clone();
        let subject = subject.to_owned();

        std::sync::Arc::new(move |news| match news {
            crate::host::RelayNews::Event {
                kind,
                level,
                message,
            } => {
                let level = if level == "warning" {
                    "warning"
                } else {
                    "info"
                };
                events.emit(&kind, &subject, level, &message);
            }

            // Issue 922: a managed server's instance ended; the loop
            // marks it starting.
            crate::host::RelayNews::Ended { index, pid } => {
                let Some((app, id)) = subject.split_once('@') else {
                    return;
                };

                let command = Command::ServerEnded {
                    app: app.to_owned(),
                    id: id.to_owned(),
                    index,
                    pid,
                };
                let commands = commands.clone();

                tokio::spawn(async move {
                    let _ = commands.send(command).await;
                });
            }
            crate::host::RelayNews::Dropped(_) => {}
        })
    }

    /// Open the daemon's own log again by name, when it was started in
    /// the background and so writes one.
    fn reopen_own_log(&self) {
        if self.owns_log
            && let Err(error) = crate::logs::reopen_standard_streams(&self.locations.daemon_log())
        {
            tracing::warn!(%error, "reopening the daemon's log failed");
        }
    }

    /// Section 11.3: after something has renamed the files, every writer
    /// opens its file again by name. The daemon reopens its own when it
    /// was started in the background, the edge its access log, the files
    /// virtual hosts name and its own log on SIGUSR1, and each release's
    /// log relay its release log on SIGUSR1; SIGUSR2 makes a host pass
    /// SIGUSR1 on to its workers, whose on_reopen_logs hook handles files
    /// the application opened itself.
    fn reopen_logs(&mut self, workers: bool) -> Reopened {
        self.reopen_logs_because(workers, "")
    }

    /// Reopen the log files as `reopen_logs` does, for `muructl logs
    /// reopen`, and answer once every writer has: the edge, asked over its
    /// control socket rather than signalled, and each relay, by its
    /// report, so a logrotate postrotate that runs the command knows the
    /// renamed files are complete when it returns (issue 609). A writer
    /// that does not answer within `REOPEN_WAIT` does not hold the answer
    /// back longer.
    fn reopen_logs_answering(&mut self, workers: bool, done: oneshot::Sender<Reopened>) {
        let (mut reopened, relays) = self.reopen_writers(workers, "", false);
        let edge = self.edge.pid().map(|_| self.edge.reopen_logs());

        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + REOPEN_WAIT;

            reopened.edge = match edge {
                Some(asked) => match tokio::time::timeout_at(deadline, asked).await {
                    Ok(Ok(failed)) => {
                        if !failed.is_empty() {
                            tracing::warn!(%failed, "the edge could not reopen a log");
                        }

                        true
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(error = %format!("{error:#}"), "the edge could not be asked to reopen its logs");
                        false
                    }
                    Err(_) => {
                        tracing::warn!("the edge did not say it had reopened its logs in time");
                        false
                    }
                },
                None => false,
            };

            for mut relay in relays {
                if tokio::time::timeout_at(deadline, relay.changed())
                    .await
                    .is_err()
                {
                    tracing::warn!("a log relay did not say it had reopened its file in time");
                    break;
                }
            }

            let _ = done.send(reopened);
        });
    }

    /// Reopen the log files as `reopen_logs` does, with why in the
    /// event's message when it is not a command or a signal that asked.
    fn reopen_logs_because(&mut self, workers: bool, why: &str) -> Reopened {
        self.reopen_writers(workers, why, true).0
    }

    /// Have every writer open its file again by name, the edge with
    /// SIGUSR1 when `signal_edge`, and record it; gives what was told, and
    /// for each relay told what changes once it has reopened its file.
    fn reopen_writers(
        &mut self,
        workers: bool,
        why: &str,
        signal_edge: bool,
    ) -> (Reopened, Vec<tokio::sync::watch::Receiver<u64>>) {
        self.reopen_own_log();
        self.renames.forget();

        let edge = if signal_edge {
            self.edge.signal(libc::SIGUSR1)
        } else {
            self.edge.pid().is_some()
        };
        let mut hosts = 0;
        let mut relays = Vec::new();

        // A host's workers are told once its relay has reopened the
        // release log, so what their hooks write goes to the new file.

        for running in self.running.values() {
            let reopened = running.backend.reopen_log(workers);

            if !reopened.is_empty() {
                hosts += 1;
                relays.extend(reopened);
            }
        }

        // Issue 698: a detached command run's relay writes the release's
        // log too.

        relays.extend(self.runs.reopen());

        let message = format!(
            "reopened the log files{}: {}{hosts} host{}{}",
            if why.is_empty() {
                String::new()
            } else {
                format!(" ({why})")
            },
            if edge { "the edge and " } else { "" },
            if hosts == 1 { "" } else { "s" },
            if workers { " and their workers" } else { "" }
        );
        self.events.emit("logs.reopened", "logs", "info", &message);
        (Reopened { edge, hosts }, relays)
    }

    /// Mark a running release ready, once the snapshot that gives it to
    /// the edge has been pushed: the state means a request can reach it.
    /// A release that has meanwhile begun to stop is left as it is.
    fn mark_ready(&self, app: &str, id: &str, message: &str) {
        if !self.running.contains_key(&(app.to_owned(), id.to_owned())) {
            return;
        }

        if let Ok(release) = self.store.release(app, id)
            && matches!(
                release.state,
                ReleaseState::Warming
                    | ReleaseState::Ready
                    | ReleaseState::Draining
                    | ReleaseState::Retained
            )
        {
            self.set_state(&release, ReleaseState::Ready, message);
        }
    }

    fn set_state(&self, release: &Release, state: ReleaseState, message: &str) {
        if let Err(error) = self
            .store
            .set_release_state(&release.app, &release.id, state, message)
        {
            tracing::error!(%error, "recording a release state failed");
        }
    }
}

/// Why something asked of a release that is starting is refused (issue
/// 693).
fn starting_refusal(app: &str, id: &str) -> String {
    format!("release {app}@{id} is starting; try again when it is ready")
}

/// An instant ahead as Unix milliseconds, or 0 for none.
fn unix_ms_of(instant: Option<Instant>) -> u64 {
    instant.map_or(0, |instant| {
        crate::model::now_unix_ms()
            + instant
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
    })
}

/// A time in milliseconds since the epoch in the full form of section
/// 11.8, for the dashboard to show as the viewer's local clock.
pub(crate) fn full_time(unix_ms: u64) -> String {
    muru_common::log::full(std::time::UNIX_EPOCH + Duration::from_millis(unix_ms))
}

/// A Unix time as HH:MM with the zone, UTC: how `release list` and the
/// process view give a retained release's deadline, and a detour's
/// timed traffic its end.
pub(crate) fn clock(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3600, (seconds % 3600) / 60)
}

/// Whether this process's standard output is the file at `path`, which
/// is how a daemon started in the background finds that it owns the
/// daemon log.
fn writes_to(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(current) = std::fs::metadata(path) else {
        return false;
    };

    // SAFETY: fstat on descriptor 1 of this process, into a zeroed struct
    // it fills; the descriptor stays open throughout.

    let (described, stat) = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        (libc::fstat(1, &mut stat) == 0, stat)
    };

    described
        && current.dev() as libc::dev_t == stat.st_dev
        && current.ino() as libc::ino_t == stat.st_ino
}

/// A rotator that carries on from the periods the state store records,
/// less those of log files that are gone (section 11.3).
fn restored_rotator(policy: crate::rotation::Policy, store: &Store) -> crate::rotation::Rotator {
    let (present, gone): (Vec<_>, Vec<_>) = store
        .log_periods()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "reading the log files' periods failed");
            Vec::new()
        })
        .into_iter()
        .partition(|(path, _)| std::path::Path::new(path).is_file());

    let gone: Vec<String> = gone.into_iter().map(|(path, _)| path).collect();

    if !gone.is_empty()
        && let Err(error) = store.forget_log_periods(&gone)
    {
        tracing::warn!(%error, "forgetting the periods of removed log files failed");
    }

    crate::rotation::Rotator::restored(policy, present)
}

/// Size each group's scaler for the interface the release runs as, which
/// its host announces once it has loaded the application: a release left
/// to be detected is sized as WSGI only until then.
fn size_scalers(
    scalers: &mut [GroupScaling],
    spec: &ReleaseSpec,
    interface: muru_config::Interface,
) {
    for scaling in scalers {
        let sizes = if scaling.name == muru_edge::routing::DEFAULT_GROUP {
            Some((spec.threads, spec.sync_threads, spec.concurrency))
        } else {
            spec.groups
                .iter()
                .find(|group| group.name == scaling.name)
                .map(|group| (group.threads, group.sync_threads, group.concurrency))
        };

        if let Some((threads, sync_threads, concurrency)) = sizes {
            (scaling.slots_per_worker, scaling.threads) =
                crate::scaling::worker_size(interface, threads, sync_threads, concurrency);
        }
    }
}

/// Section 7.4: the scaling state of each group of a release that is
/// starting or adopted, from its spec, the interface it runs as, the
/// workers it has and the pins an operator left on it.
fn scalers_for(
    spec: &ReleaseSpec,
    interface: muru_config::Interface,
    sockets: &[PathBuf],
    groups: &[GroupRecord],
    pins: &[(String, u32)],
    now: Instant,
) -> Vec<GroupScaling> {
    let memory_limit = spec
        .memory
        .soft_limit
        .map(|limit| (spec.memory.measure, limit));
    let mut scalers = vec![GroupScaling::new(
        muru_edge::routing::DEFAULT_GROUP,
        spec.scaling.clone(),
        sockets.len() as u32,
        memory_limit,
    )];

    for group in &spec.groups {
        let current = groups
            .iter()
            .find(|record| record.name == group.name)
            .map_or(group.processes as usize, |record| record.sockets.len());
        scalers.push(GroupScaling::new(
            &group.name,
            group.scaling.clone(),
            current as u32,
            memory_limit,
        ));
    }

    size_scalers(&mut scalers, spec, interface);

    // Issue 384: a worker of a group with a hard limit counts at it.

    for scaling in &mut scalers {
        scaling.hard_limit = spec
            .groups
            .iter()
            .find(|group| group.name == scaling.name)
            .and_then(|group| group.memory.as_ref())
            .map_or(spec.memory.hard_limit, |memory| memory.hard_limit);
    }

    for (name, count) in pins {
        if let Some(scaling) = scalers.iter_mut().find(|scaling| &scaling.name == name) {
            scaling.scaler.pin(*count, now);
        }
    }

    scalers
}

/// Section 7.5: how long before a window raising a floor, at least, it
/// is checked against the node's budget.
const BUDGET_LEAD: Duration = Duration::from_secs(15 * 60);

/// How often the clock's synchronisation is checked.
const CLOCK_CHECK: Duration = Duration::from_secs(600);

/// Whether the system clock is synchronised, as the kernel's NTP state
/// says, or none where that cannot be told.
fn clock_synchronised() -> Option<bool> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: timex is plain data for which all zeroes is a valid
        // value, and with modes 0 the call only reads the clock's state
        // into it.

        let state = unsafe {
            let mut timex: libc::timex = std::mem::zeroed();

            #[cfg(target_os = "linux")]
            let state = libc::adjtimex(&mut timex);

            #[cfg(target_os = "macos")]
            let state = libc::ntp_adjtime(&mut timex);

            state
        };

        if state >= 0 {
            return Some(state != libc::TIME_ERROR);
        }
    }

    None
}

/// The event a clock check raises: a warning when the clock is not
/// synchronised while schedules depend on it, once, and a notice when it
/// is synchronised again after one.
fn clock_event(
    synchronised: Option<bool>,
    warned: bool,
    scheduled: bool,
) -> Option<(&'static str, &'static str, &'static str)> {
    match synchronised {
        Some(false) if scheduled && !warned => Some((
            "clock.unsynchronised",
            "warning",
            "the system clock is not synchronised; overlay schedules and scheduled capacity follow it",
        )),
        Some(true) if warned => Some((
            "clock.synchronised",
            "notice",
            "the system clock is synchronised again",
        )),
        _ => None,
    }
}

/// A scaling decision as its event says it.
/// Section 7.3: a diagnosis as words for an event or a status line.
/// The applications a route sends requests to: the application, or a
/// static route's fallback.
fn route_apps(route: &muru_config::Route) -> Vec<&str> {
    let mut apps = Vec::new();

    if !route.app.is_empty() {
        apps.push(route.app.as_str());
    }

    if let Some(app) = route
        .fallback
        .as_ref()
        .and_then(|fallback| fallback.app.as_deref())
    {
        apps.push(app);
    }

    apps
}

/// The releases the routes' traffic policies name, as (application,
/// release) pairs (section 9.4), for a share or for a client class.
fn referenced_in(routes: &[muru_config::Route]) -> BTreeSet<(String, String)> {
    routes
        .iter()
        .filter_map(|route| route.traffic.as_ref().map(|traffic| (route, traffic)))
        .flat_map(|(route, traffic)| {
            route_apps(route).into_iter().flat_map(move |app| {
                traffic
                    .split
                    .keys()
                    .chain(traffic.classes.values())
                    .filter(|id| !id.starts_with(muru_edge::routing::UPSTREAM_PREFIX))
                    .map(move |id| (app.to_owned(), id.clone()))
            })
        })
        .collect()
}

/// Section 4.13: an upstream as the edge takes it, with its CA file read
/// now so a replaced file takes effect with the next snapshot.
fn upstream_message(name: &str, table: &muru_config::Upstream) -> anyhow::Result<pb::Upstream> {
    let millis = |text: Option<&str>| {
        text.and_then(|text| muru_config::parse_duration(text).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    };

    let tls_ca_pem = match table.tls.as_ref().and_then(|tls| tls.ca.as_deref()) {
        Some(ca) => std::fs::read_to_string(ca).with_context(|| format!("reading {ca}"))?,
        None => String::new(),
    };

    let health = table.health.as_ref();

    Ok(pb::Upstream {
        name: name.to_owned(),
        addresses: table.addresses.clone(),
        protocol: table.protocol.clone().unwrap_or_default(),
        tls: table.tls.is_some(),
        tls_ca_pem,
        tls_server_name: table
            .tls
            .as_ref()
            .and_then(|tls| tls.server_name.clone())
            .unwrap_or_default(),
        tls_insecure: table
            .tls
            .as_ref()
            .is_some_and(|tls| tls.verify == Some(false)),
        trust_muru_headers: table.trust_muru_headers,
        sendfile_roots: table.sendfile_roots.clone(),
        sendfile_header: table.sendfile_header.clone().unwrap_or_default(),
        detour: false,
        starting: Vec::new(),
        connect_timeout_ms: millis(table.connect_timeout.as_deref()),
        probe: health.map(|health| pb::UpstreamProbe {
            path: health.path.clone().unwrap_or_default(),
            interval_ms: millis(health.interval.as_deref()),
            timeout_ms: millis(health.timeout.as_deref()),
            unhealthy_after: health.unhealthy_after.unwrap_or(0),
            healthy_after: health.healthy_after.unwrap_or(0),
        }),
        eject_after: health.and_then(|health| health.eject_after).unwrap_or(3),
        eject_for_ms: millis(health.and_then(|health| health.eject_for.as_deref())),
    })
}

/// A policy's sticky grace: what it says, or 10 minutes.
fn sticky_grace_of(traffic: &muru_config::Traffic) -> Duration {
    traffic
        .sticky_grace
        .as_deref()
        .and_then(|text| muru_config::parse_duration(text).ok())
        .unwrap_or(STICKY_GRACE)
}

/// A rollout's steps in words: "10% for 2m, 50% for 2m".
fn describe_steps(steps: &[RolloutStep]) -> String {
    steps
        .iter()
        .map(|step| {
            format!(
                "{}% for {}",
                step.percent,
                muru_config::format_duration(Duration::from_millis(step.hold_ms))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a rollout at this step is doing: "step 2 of 3: 50% for 2m".
fn step_message(steps: &[RolloutStep], step: usize) -> String {
    match steps.get(step) {
        Some(current) => format!(
            "step {} of {}: {}% for {}",
            step + 1,
            steps.len(),
            current.percent,
            muru_config::format_duration(Duration::from_millis(current.hold_ms))
        ),
        None => format!("step {} of {}", step + 1, steps.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_names_a_process_by_its_path_or_its_title() {
        let run_dir = Path::new("/run/muru/apps/shop/r42");

        assert!(super::names(
            "python -m muru.host --socket-dir /run/muru/apps/shop/r42/r1",
            run_dir,
            "muru host shop@r42"
        ));
        assert!(super::names(
            "muru host shop@r42      ",
            run_dir,
            "muru host shop@r42"
        ));
        assert!(super::names(
            "muru host shop@r42 (7f3a9c2e)",
            run_dir,
            "muru host shop@r42"
        ));
        assert!(!super::names(
            "muru host shop@r421",
            run_dir,
            "muru host shop@r42"
        ));
        assert!(!super::names(
            "muru host shop@r42 (web) other",
            run_dir,
            "muru host shop@r42"
        ));
        assert!(!super::names("sleep 300", run_dir, "muru host shop@r42"));
    }

    #[test]
    fn the_adoption_event_names_the_instances_kept_and_those_started_again() {
        use crate::supervisor::Adopted;
        use std::os::unix::process::ExitStatusExt as _;

        let adopted =
            |found: &[Adopted]| -> Vec<(u32, Adopted)> { (0..).zip(found.to_vec()).collect() };

        assert_eq!(
            adoption_message(&adopted(&[Adopted::Kept(41), Adopted::Kept(42)])),
            "re-adopted the server instances (pid 41, 42)"
        );
        assert_eq!(
            adoption_message(&adopted(&[Adopted::Kept(41), Adopted::Gone])),
            "re-adopted the server instances (pid 41); instance 1 was gone and is started again"
        );
        assert_eq!(
            adoption_message(&adopted(&[Adopted::Gone, Adopted::Gone, Adopted::Gone])),
            "none of the server instances was still there; \
             instances 0, 1 and 2 were gone and are started again"
        );

        // Issue 916: one that ended while no daemon ran says how, once.

        let killed = Adopted::Ended {
            exit: crate::supervisor::Exit::Status(std::process::ExitStatus::from_raw(9)),
            wait: Duration::from_secs(1),
        };
        assert_eq!(
            adoption_message(&adopted(&[Adopted::Kept(41), killed])),
            "re-adopted the server instances (pid 41); instance 1 exited with signal: 9 (SIGKILL) \
             while no daemon ran and is started again in 1s"
        );
    }

    #[test]
    fn a_group_that_may_scale_to_zero_retains_no_worker_by_default() {
        let at_zero = crate::model::ScalingSpec {
            mode: muru_config::ScalingMode::Spare,
            min_workers: 0,
            ..crate::model::ScalingSpec::default()
        };
        let floored = crate::model::ScalingSpec {
            min_workers: 2,
            ..at_zero.clone()
        };

        assert_eq!(retained_by_default(&at_zero), 0);
        assert_eq!(retained_by_default(&floored), 1);
        assert_eq!(
            retained_by_default(&crate::model::ScalingSpec::default()),
            1
        );
    }

    /// The process view and `release list` give a retained release's
    /// deadline the same way, in UTC with the zone (issue 618).
    #[test]
    fn a_deadline_is_given_in_utc_with_the_zone() {
        let at = (7 * 3600 + 40 * 60) * 1000 + 59_999;

        assert_eq!(clock(at), "07:40 UTC");
    }

    #[test]
    fn the_retained_note_says_what_a_rollback_costs() {
        let kept = |counts: &[u32]| {
            Kept::Workers(
                counts
                    .iter()
                    .enumerate()
                    .map(|(index, count)| (format!("g{index}"), *count))
                    .collect(),
            )
        };

        assert_eq!(
            kept(&[1, 1]).note("10:00 UTC"),
            "warm until 10:00 UTC; a rollback to it is instant"
        );
        assert_eq!(
            kept(&[0, 0]).note("10:00 UTC"),
            "host kept until 10:00 UTC; a rollback to it forks a worker"
        );
        assert_eq!(
            kept(&[1, 0]).note("10:00 UTC"),
            "warm until 10:00 UTC; a rollback to it forks a worker for a group that kept none"
        );
        assert_eq!(
            Kept::Idle.note("10:00 UTC"),
            "idle until 10:00 UTC; a rollback to it waits for its import"
        );
        assert_eq!(
            Kept::Instances.note("10:00 UTC"),
            "warm until 10:00 UTC; a rollback to it is instant"
        );

        assert_eq!(
            kept(&[1, 1]).event("30m"),
            "kept warm for 30m with 1 worker per group"
        );
        assert_eq!(kept(&[0]).event("30m"), "kept for 30m with its host alone");
        assert_eq!(
            kept(&[1, 0]).event("30m"),
            "kept for 30m with its host and 1 in g0, 0 in g1"
        );
        assert_eq!(
            Kept::Idle.event("30m"),
            "kept idle for 30m with its host stopped"
        );
    }

    #[test]
    fn the_clock_warning_is_raised_once_and_only_for_schedules() {
        assert_eq!(
            clock_event(Some(false), false, true).map(|(kind, ..)| kind),
            Some("clock.unsynchronised")
        );
        assert_eq!(clock_event(Some(false), true, true), None);
        assert_eq!(clock_event(Some(false), false, false), None);
        assert_eq!(
            clock_event(Some(true), true, true).map(|(kind, ..)| kind),
            Some("clock.synchronised")
        );
        assert_eq!(clock_event(Some(true), false, true), None);
        assert_eq!(clock_event(None, false, true), None);
    }

    #[test]
    fn the_clock_state_can_be_read_here() {
        assert!(clock_synchronised().is_some());
    }

    #[test]
    fn an_interface_is_the_announced_one_and_the_configured_one_only_until_then() {
        use muru_config::Interface;

        let interfaces = Interfaces::default();
        assert_eq!(
            interfaces.runs_as("shop", "r1", "", Interface::Auto),
            Interface::Auto
        );

        interfaces.announce(
            "shop",
            "r1",
            vec![
                (String::new(), "asgi".to_owned()),
                ("legacy".to_owned(), "wsgi".to_owned()),
            ],
        );
        assert_eq!(
            interfaces.runs_as("shop", "r1", "", Interface::Auto),
            Interface::Asgi
        );
        assert_eq!(
            interfaces.runs_as("shop", "r1", "legacy", Interface::Auto),
            Interface::Wsgi
        );

        // A mount the host did not name, another release, and an
        // announcement that names nothing, which leaves the last one.

        assert_eq!(
            interfaces.runs_as("shop", "r1", "live", Interface::Auto),
            Interface::Auto
        );
        assert_eq!(
            interfaces.runs_as("shop", "r2", "", Interface::Wsgi),
            Interface::Wsgi
        );
        interfaces.announce("shop", "r1", Vec::new());
        assert_eq!(interfaces.announced("shop", "r1").len(), 2);
    }

    #[test]
    fn a_release_detected_as_asgi_is_sized_by_its_event_loop() {
        use muru_config::Interface;

        // Threads 4, sync threads 8, concurrency 1000: as WSGI until the
        // host has said what it detected, then by the loop.

        assert_eq!(
            crate::scaling::worker_size(Interface::Auto, 4, 8, 1000),
            (4.0, 4)
        );
        assert_eq!(
            crate::scaling::worker_size(Interface::Wsgi, 4, 8, 1000),
            (4.0, 4)
        );
        assert_eq!(
            crate::scaling::worker_size(Interface::Asgi, 4, 8, 1000),
            (1000.0, 9)
        );
        assert_eq!(
            crate::scaling::worker_size(Interface::Grpc, 4, 8, 1000),
            (1000.0, 9)
        );
    }
}
