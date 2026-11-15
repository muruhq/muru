//! The runs of commands the daemon keeps (design specification section
//! 9.12, issue 688): each run's record in the state store, from its start
//! to how it ended, with its output in a file of its own, and the runs
//! started detached, which outlive their caller and the daemon.
//!
//! A run its caller's stream carries is run by `commands.rs`, which tells
//! `Recorded` when it starts, what it writes and how it ends. A detached
//! run is started here, under a log relay that is its parent (ADR 0059 and
//! ADR 0060), as a managed server's instance is: the relay writes its
//! output to the release's log and to the run's own file and records its
//! exit status, so a daemon that starts again takes the run over, or finds
//! out how it ended while no daemon was there. A task here watches each
//! detached run: it stops a run at its timeout or when asked, and records
//! its end.
//!
//! An exclusive command's claim is the run's record: the store refuses a
//! run of the command while another that has not ended holds it, so the
//! claim survives a daemon that dies, and is given up with the end the
//! next daemon records.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::commands::{Prepared, Record, STOP_GRACE};
use crate::events::Events;
use crate::host::{alive, leads_group, signal_group, signal_process, start_time};
use crate::model::{CommandRun, RunRecord, now_unix_ms};
use crate::relay::ProcessRecord;
use crate::store::Store;

/// How many ended runs of each command are kept when nothing says.
pub const DEFAULT_KEEP: usize = 20;

/// How long a relay that was started has to say how the start went.
const RELAY_START_WAIT: Duration = Duration::from_secs(10);

/// How often a relay that is not this daemon's child is looked at.
const POLL: Duration = Duration::from_millis(200);

/// How long a detached run's relay is given to say it has reopened its
/// files.
const REOPEN_WAIT: Duration = Duration::from_secs(5);

/// How long a relay told to kill its run's session gets before it is
/// killed itself.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// What starts a run: someone asking, or the daemon itself.
pub const MANUAL: &str = "manual";

/// The trigger of a run the daemon starts on a command's schedule.
pub const SCHEDULE: &str = "schedule";

/// Who asked for a run the daemon starts by itself.
pub const DAEMON: &str = "daemon";

/// The daemon's runs: their records and output files, and the detached
/// runs under way.
#[derive(Clone)]
pub struct Runs(Arc<Inner>);

struct Inner {
    store: Arc<Store>,
    events: Events,
    /// The daemon's log directory, under which each run's output is.
    logs: PathBuf,
    /// How many ended runs of each command are kept.
    keep: AtomicUsize,
    /// Each detached run being watched, by id: where its stop goes.
    watched: Mutex<HashMap<u64, Watched>>,
}

/// A detached run's watcher.
struct Watched {
    stop: mpsc::UnboundedSender<String>,
    task: tokio::task::JoinHandle<()>,
    /// The relay's pid and start time, which name it, and its control
    /// socket, for telling it to reopen its files (issue 698).
    relay: (u32, u64),
    control: Option<PathBuf>,
}

impl Runs {
    /// The runs of a daemon with this store, event stream and log
    /// directory, keeping `keep` ended runs of each command.
    pub fn new(store: Arc<Store>, events: Events, logs: PathBuf, keep: usize) -> Self {
        Self(Arc::new(Inner {
            store,
            events,
            logs,
            keep: AtomicUsize::new(keep),
            watched: Mutex::new(HashMap::new()),
        }))
    }

    /// Keep this many ended runs of each command from now on.
    pub fn set_keep(&self, keep: usize) {
        self.0.keep.store(keep, Ordering::Relaxed);
    }

    /// The store the runs are kept in.
    pub fn store(&self) -> &Arc<Store> {
        &self.0.store
    }

    /// The file a run's output goes to, relative to the log directory.
    pub fn log_name(app: &str, name: &str, id: u64) -> PathBuf {
        Path::new("apps")
            .join(app)
            .join("commands")
            .join(name)
            .join(format!("{id}.log"))
    }

    /// The file a run's output goes to.
    pub fn log_path(&self, app: &str, name: &str, id: u64) -> PathBuf {
        self.0.logs.join(Self::log_name(app, name, id))
    }

    /// The log of the release a run runs with.
    fn release_log(&self, run: &RunRecord) -> PathBuf {
        self.0
            .logs
            .join("apps")
            .join(&run.app)
            .join(format!("{}.log", run.release))
    }

    /// What records a run its caller's stream carries, started by `trigger`
    /// for `requested_by`.
    pub fn recorded(&self, trigger: &str, requested_by: &str) -> Recorded {
        Recorded {
            runs: self.clone(),
            trigger: trigger.to_owned(),
            requested_by: requested_by.to_owned(),
            file: Mutex::new(None),
        }
    }

    /// Record a run as it starts and open its output file, refusing it
    /// while another run holds its command's claim.
    fn begin(
        &self,
        prepared: &Prepared,
        trigger: &str,
        requested_by: &str,
        detached: bool,
    ) -> Result<(u64, PathBuf), String> {
        let name = prepared.run_name();
        let record = RunRecord {
            app: prepared.app.clone(),
            release: prepared.release.clone(),
            name: name.to_owned(),
            trigger: trigger.to_owned(),
            requested_by: requested_by.to_owned(),
            detached,
            exclusive: prepared.exclusive,
            inputs: prepared.inputs.clone(),
            timeout_ms: prepared
                .timeout
                .map_or(0, |timeout| timeout.as_millis() as u64),
            started_unix_ms: now_unix_ms(),
            secrets: prepared
                .secrets
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            ..RunRecord::default()
        };

        let id = match self.0.store.begin_run(&record) {
            Ok(Ok(id)) => id,
            Ok(Err(holder)) => {
                return Err(format!(
                    "{name} runs one at a time for {}, and run {holder} is under way",
                    prepared.app
                ));
            }
            Err(error) => return Err(error.to_string()),
        };

        // The file is made now, as the user the run runs as, so that a
        // relay running as that user can write it too.

        let path = self.log_path(&prepared.app, name, id);

        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);

        if let Some(identity) = prepared.identity {
            let _ = identity.own(&path);
        }

        Ok((id, path))
    }

    /// Record how a run ended, write it to the run's file and the
    /// release's log, say so, and forget the oldest runs of its command
    /// beyond those kept, with their files.
    fn end(&self, run: &RunRecord, ending: &crate::commands::Ending, logs: &[PathBuf]) {
        let line = format!(
            "{} [command {}] {}\n",
            muru_common::log::full(std::time::SystemTime::now()),
            run.name,
            ending.message
        );

        for path in logs {
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
                let _ = file.write_all(line.as_bytes());
            }
        }

        self.ended(run, ending);
    }

    /// Record a run's end and say so, as `end` does, without writing it
    /// to the files, which the run's own writer has done.
    fn ended(&self, run: &RunRecord, ending: &crate::commands::Ending) {
        if let Err(error) = self.0.store.end_run(
            run.id,
            now_unix_ms(),
            ending.code,
            ending.ok,
            &ending.message,
        ) {
            tracing::warn!(%error, "recording the end of run {} failed", run.id);
        }

        if run.detached {
            let (kind, level) = if ending.ok {
                ("command.finished", "info")
            } else {
                ("command.failed", "warning")
            };
            self.0.events.emit(
                kind,
                &format!("{}@{}", run.app, run.release),
                level,
                &format!("{}: run {} {}", run.name, run.id, ending.message),
            );
        }

        if !run.secrets.is_empty() {
            let _ = std::fs::remove_dir_all(&run.secrets);
        }

        self.prune(&run.app, &run.name);
    }

    /// Forget the ended runs of a command beyond those kept, and remove
    /// their output.
    fn prune(&self, app: &str, name: &str) {
        let keep = self.0.keep.load(Ordering::Relaxed);

        match self.0.store.prune_runs(app, name, keep) {
            Ok(forgotten) => {
                for run in forgotten {
                    let _ = std::fs::remove_file(self.log_path(app, name, run.id));
                }
            }
            Err(error) => tracing::warn!(%error, "forgetting old runs of {name} failed"),
        }
    }

    /// Start a run detached: under a log relay that is its parent, from
    /// `executable`, its output going to the release's log and the run's
    /// own file. Answers with the run's id once it has started, or why it
    /// could not.
    pub async fn start_detached(
        &self,
        prepared: Prepared,
        trigger: &str,
        requested_by: &str,
        executable: &Path,
    ) -> Result<u64, String> {
        let (id, file) = self.begin(&prepared, trigger, requested_by, true)?;
        let run = self.0.store.run(id).map_err(|error| error.to_string())?;
        let release_log = prepared.log.clone().unwrap_or_else(|| file.clone());

        match spawn_relay(&prepared, id, &file, &release_log, executable).await {
            Ok((relay, started)) => {
                if let Err(error) = self.0.store.set_run_process(
                    id,
                    started.pid,
                    started.start_time,
                    started.relay_pid,
                    started.relay_start_time,
                ) {
                    tracing::warn!(%error, "recording run {id}'s process failed");
                }

                let with = if prepared.inputs.is_empty() {
                    String::new()
                } else {
                    format!(", with {}", prepared.inputs)
                };
                self.0.events.emit(
                    "command.started",
                    &prepared.subject(),
                    "info",
                    &format!(
                        "{}: run {id} started detached (pid {}){with}",
                        prepared.label, started.pid
                    ),
                );

                let run = RunRecord {
                    pid: started.pid,
                    start_time: started.start_time,
                    relay_pid: started.relay_pid,
                    relay_start_time: started.relay_start_time,
                    ..run
                };
                self.watch(run, Some(relay), vec![release_log, file]);
                Ok(id)
            }
            Err(error) => {
                let message = format!("could not be started: {error:#}");
                let ending = crate::commands::Ending {
                    code: 127,
                    message: message.clone(),
                    ok: false,
                };
                self.end(&run, &ending, &[release_log, file]);
                Err(format!("{} {message}", prepared.label))
            }
        }
    }

    /// Watch a detached run until it ends: stop it at its timeout or when
    /// asked, and record how it ended. `relay` is the relay when it is
    /// this daemon's child.
    fn watch(&self, run: RunRecord, relay: Option<tokio::process::Child>, logs: Vec<PathBuf>) {
        let (stop, stops) = mpsc::unbounded_channel();
        let runs = self.clone();
        let id = run.id;
        let relay_named = (run.relay_pid, run.relay_start_time);
        let control = run_dir(&run).map(|dir| dir.join(crate::relay::CONTROL_SOCKET));

        let task = tokio::spawn(async move {
            let ending = watch_run(&runs, &run, relay, stops).await;
            runs.end(&run, &ending, &logs);
            runs.0
                .watched
                .lock()
                .expect("runs poisoned")
                .remove(&run.id);
        });

        self.0.watched.lock().expect("runs poisoned").insert(
            id,
            Watched {
                stop,
                task,
                relay: relay_named,
                control,
            },
        );
    }

    /// Have every detached run's relay open the release's log and the
    /// run's own file again by name (issue 698), as a release's relays do
    /// after a rotation or `muructl logs reopen`. Gives, for each relay,
    /// what changes once it reports that it has: the relay is connected
    /// to first and signalled once it answers, so its report is not
    /// missed, and the connection is given up after `REOPEN_WAIT`.
    pub fn reopen(&self) -> Vec<tokio::sync::watch::Receiver<u64>> {
        let watched = self.0.watched.lock().expect("runs poisoned");
        let mut answers = Vec::new();

        for watched in watched.values() {
            let (pid, start) = watched.relay;

            let Some(control) = watched.control.clone() else {
                continue;
            };

            if !still_running(pid, start) {
                continue;
            }

            let (told, answer) = tokio::sync::watch::channel(0);
            answers.push(answer);

            tokio::spawn(async move {
                let mut signalled = false;

                let reports = crate::relay::watch_reports(&control, |report| match report {
                    crate::relay::Report::Status(_) if !signalled => {
                        signalled = true;

                        if still_running(pid, start) {
                            signal_process(pid, libc::SIGUSR1);
                        }
                    }
                    crate::relay::Report::Reopened => told.send_modify(|count| *count += 1),
                    _ => {}
                });

                let _ = tokio::time::timeout(REOPEN_WAIT, reports).await;
            });
        }

        answers
    }

    /// Stop the detached run with this id, as at its timeout, because of
    /// `why`; false when it is not one being watched.
    pub fn stop(&self, id: u64, why: &str) -> bool {
        let watched = self.0.watched.lock().expect("runs poisoned");

        match watched.get(&id) {
            Some(watched) => watched.stop.send(why.to_owned()).is_ok(),
            None => false,
        }
    }

    /// Take over the runs a previous daemon left under way. A detached
    /// run whose relay is still there is watched again; one whose relay is
    /// gone ended while no daemon was there, as its relay recorded. A run
    /// a caller's stream carried lost its caller with the daemon: whatever
    /// is left of it is killed.
    pub fn adopt(&self) {
        let under_way = match self.0.store.runs_under_way() {
            Ok(runs) => runs,
            Err(error) => {
                tracing::warn!(%error, "reading the runs under way failed");
                return;
            }
        };

        for run in under_way {
            if self
                .0
                .watched
                .lock()
                .expect("runs poisoned")
                .contains_key(&run.id)
            {
                continue;
            }

            if run.relay_pid == 0 {
                if still_running(run.pid, run.start_time) {
                    signal_group(run.pid, libc::SIGKILL);
                }

                let ending = crate::commands::Ending {
                    code: 137,
                    message: "was stopped because the daemon that ran it went away".to_owned(),
                    ok: false,
                };
                let logs = [
                    self.release_log(&run),
                    self.log_path(&run.app, &run.name, run.id),
                ];
                self.end(&run, &ending, &logs);
                continue;
            }

            let logs = vec![
                self.release_log(&run),
                self.log_path(&run.app, &run.name, run.id),
            ];
            self.watch(run, None, logs);
        }
    }

    /// Stop every detached run because of `why`, and wait for them until
    /// `deadline`, for a daemon that shuts down. One still there then is
    /// killed, with a moment to go.
    pub async fn stop_all(&self, why: &str, deadline: Instant) {
        let tasks: Vec<(u64, mpsc::UnboundedSender<String>)> = self
            .0
            .watched
            .lock()
            .expect("runs poisoned")
            .iter()
            .map(|(id, watched)| (*id, watched.stop.clone()))
            .collect();

        for (_, stop) in &tasks {
            let _ = stop.send(why.to_owned());
        }

        while Instant::now() < deadline + KILL_WAIT {
            if self.0.watched.lock().expect("runs poisoned").is_empty() {
                return;
            }

            // At the deadline what is left is killed: its relay is told
            // first, and a second stop asked for does that.

            if Instant::now() >= deadline {
                for (_, stop) in &tasks {
                    let _ = stop.send(KILL.to_owned());
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop watching the detached runs, leaving them running for the next
    /// daemon, for a daemon that detaches or upgrades.
    pub fn release(&self) {
        let watched = std::mem::take(&mut *self.0.watched.lock().expect("runs poisoned"));

        for (_, watched) in watched {
            watched.task.abort();
        }
    }
}

/// What a stop asked for says to kill the run at once.
const KILL: &str = "\0kill";

/// Whether the process with this pid and start time is still the one
/// that has the pid.
fn still_running(pid: u32, started: u64) -> bool {
    pid != 0 && alive(pid) && (started == 0 || start_time(pid) == Some(started))
}

/// What `spawn_relay` learned: the relay, and what it recorded of the run.
type Spawned = (tokio::process::Child, ProcessRecord);

/// Start the relay of a detached run, which starts the run's command in
/// a session of its own, and wait for it to say how that went.
async fn spawn_relay(
    prepared: &Prepared,
    id: u64,
    file: &Path,
    release_log: &Path,
    executable: &Path,
) -> anyhow::Result<Spawned> {
    let dir = prepared
        .secrets
        .clone()
        .context("a detached run needs a directory of its own")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    if let Some(identity) = prepared.identity {
        identity
            .own(&dir)
            .with_context(|| format!("giving {} to the application's user", dir.display()))?;
    }

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(release_log)
        .with_context(|| format!("opening {}", release_log.display()))?;
    let tag = format!("[command {}] ", prepared.label);

    let mut command = tokio::process::Command::new(executable);
    command
        .arg("relay")
        .arg("run")
        .arg("--control")
        .arg(dir.join(crate::relay::CONTROL_SOCKET))
        .arg("--record")
        .arg(dir.join(crate::relay::RECORD))
        .arg("--log-file")
        .arg(release_log)
        .arg("--also-log-file")
        .arg(file)
        .arg("--release")
        .arg(prepared.subject())
        .arg("--tag")
        .arg(&tag)
        .arg("--stdout-tag")
        .arg(&tag)
        .arg("--name")
        .arg(format!("run {id}"))
        .arg("--stop-signal")
        .arg(libc::SIGTERM.to_string())
        .arg("--stop-group")
        .arg("--")
        .args(&prepared.argv)
        .current_dir(&prepared.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);

    if prepared.clean {
        command.env_clear();
    }

    for name in &prepared.unset {
        command.env_remove(name);
    }

    command.envs(prepared.env.iter().map(|(name, value)| (name, value)));
    crate::host::own_session(&mut command, prepared.identity);

    let mut relay = command.spawn().context("running muructl relay")?;

    // The relay says, in one line, what it started or why it could not;
    // one that says nothing in time is killed.

    let mut line = String::new();

    let said = match relay.stdout.take() {
        Some(out) => {
            let mut out = tokio::io::BufReader::new(out);
            tokio::time::timeout(RELAY_START_WAIT, out.read_line(&mut line)).await
        }
        None => Ok(Ok(0)),
    };

    let started = match said {
        Ok(Ok(_)) => serde_json::from_str::<ProcessRecord>(line.trim_end()).ok(),
        _ => None,
    };

    match started {
        Some(started) if started.error.is_none() && started.pid != 0 => Ok((relay, started)),
        started => {
            let _ = relay.start_kill();
            let _ = relay.wait().await;
            anyhow::bail!(
                "{}",
                started
                    .and_then(|started| started.error)
                    .unwrap_or_else(|| { "its log relay did not start it".to_owned() })
            )
        }
    }
}

/// Watch a detached run to its end: its relay's exit, which is the run's
/// end, a stop asked for, or its timeout. A stop sends the relay SIGTERM,
/// which it passes on to the run's session, and after the grace period
/// SIGUSR2, on which it kills the session; a relay that does not go is
/// killed, with what is left of the session. Gives how the run ended.
async fn watch_run(
    runs: &Runs,
    run: &RunRecord,
    mut relay: Option<tokio::process::Child>,
    mut stops: mpsc::UnboundedReceiver<String>,
) -> crate::commands::Ending {
    // The timeout and the stop run by the wall clock, so that a run taken
    // over by another daemon keeps its deadline.

    let deadline = (run.timeout_ms > 0).then(|| {
        let ends = run.started_unix_ms + run.timeout_ms;
        Instant::now() + Duration::from_millis(ends.saturating_sub(now_unix_ms()))
    });

    let mut stopping: Option<String> = (!run.stopping.is_empty()).then(|| run.stopping.clone());
    let mut kill_at = stopping.as_ref().map(|_| Instant::now() + STOP_GRACE);
    let mut killed_at: Option<Instant> = None;

    loop {
        tokio::select! {
            () = relay_exited(&mut relay, run) => break,
            Some(why) = stops.recv() => {
                if why == KILL {
                    kill_at = Some(Instant::now());
                } else if stopping.is_none() {
                    stop_relay(runs, &relay, run, &why);
                    stopping = Some(why);
                    kill_at = Some(Instant::now() + STOP_GRACE);
                }
            }
            () = sleep_until(deadline), if stopping.is_none() && deadline.is_some() => {
                let why = format!(
                    "it ran past its timeout of {}",
                    muru_config::format_duration(Duration::from_millis(run.timeout_ms))
                );
                stop_relay(runs, &relay, run, &why);
                stopping = Some(why);
                kill_at = Some(Instant::now() + STOP_GRACE);
            }
            () = sleep_until(kill_at), if kill_at.is_some() && killed_at.is_none() => {
                signal_relay(&relay, run, libc::SIGUSR2);
                killed_at = Some(Instant::now());
            }
            () = sleep_until(killed_at.map(|at| at + KILL_WAIT)), if killed_at.is_some() => {
                // The relay did not go: it is killed, and so is what is
                // left of the run's session.

                signal_relay(&relay, run, libc::SIGKILL);

                if still_running(run.pid, run.start_time) && leads_group(run.pid) {
                    signal_group(run.pid, libc::SIGKILL);
                }

                killed_at = Some(Instant::now() + Duration::from_secs(3600));
            }
        }
    }

    let took = Duration::from_millis(now_unix_ms().saturating_sub(run.started_unix_ms));
    let record = run_dir(run).and_then(|dir| ProcessRecord::read(&dir.join(crate::relay::RECORD)));

    match record
        .filter(|record| record.pid == run.pid)
        .and_then(|record| record.status)
    {
        Some(status) => {
            use std::os::unix::process::ExitStatusExt as _;

            crate::commands::ending(
                Ok(std::process::ExitStatus::from_raw(status)),
                stopping,
                took,
            )
        }
        None => {
            if still_running(run.pid, run.start_time) && leads_group(run.pid) {
                signal_group(run.pid, libc::SIGKILL);
            }

            crate::commands::Ending {
                code: 1,
                message: "lost its log relay and was stopped".to_owned(),
                ok: false,
            }
        }
    }
}

/// The directory a detached run's relay keeps its record in.
fn run_dir(run: &RunRecord) -> Option<PathBuf> {
    (!run.secrets.is_empty()).then(|| PathBuf::from(&run.secrets))
}

/// Ask the relay to stop the run, recording why.
fn stop_relay(runs: &Runs, relay: &Option<tokio::process::Child>, run: &RunRecord, why: &str) {
    if let Err(error) = runs.0.store.set_run_stopping(run.id, why) {
        tracing::warn!(%error, "recording why run {} is stopped failed", run.id);
    }

    signal_relay(relay, run, libc::SIGTERM);
}

/// Send the run's relay a signal, while it is the relay.
fn signal_relay(relay: &Option<tokio::process::Child>, run: &RunRecord, signal: i32) {
    match relay {
        Some(child) => {
            if let Some(pid) = child.id() {
                signal_process(pid, signal);
            }
        }
        None => signal_relay_pid(run, signal),
    }
}

/// Send the relay a signal by its pid, checked against its start time.
fn signal_relay_pid(run: &RunRecord, signal: i32) {
    if still_running(run.relay_pid, run.relay_start_time) {
        signal_process(run.relay_pid, signal);
    }
}

/// Wait for the run's relay to exit: as a child, or by its pid and start
/// time.
async fn relay_exited(relay: &mut Option<tokio::process::Child>, run: &RunRecord) {
    match relay {
        Some(child) => {
            let _ = child.wait().await;
        }
        None => {
            while still_running(run.relay_pid, run.relay_start_time) {
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

/// Sleep until the instant, or forever when there is none.
async fn sleep_until(instant: Option<Instant>) {
    match instant {
        Some(instant) => tokio::time::sleep_until(instant).await,
        None => std::future::pending().await,
    }
}

/// The daemon's record of a run its caller's stream carries: the run's
/// record from its start, its output in the run's own file, and its end.
pub struct Recorded {
    runs: Runs,
    trigger: String,
    requested_by: String,
    /// The run's id and output file, once it has begun.
    file: Mutex<Option<(u64, std::fs::File)>>,
}

impl Record for Recorded {
    fn keeps_runs(&self) -> bool {
        true
    }

    fn begin(&self, prepared: &Prepared) -> Result<Option<u64>, String> {
        let (id, path) = self
            .runs
            .begin(prepared, &self.trigger, &self.requested_by, false)?;

        if let Ok(file) = std::fs::OpenOptions::new().append(true).open(&path) {
            *self.file.lock().expect("run file poisoned") = Some((id, file));
        }

        Ok(Some(id))
    }

    fn started(&self, run: Option<u64>, pid: u32) {
        if let Some(id) = run
            && let Err(error) =
                self.runs
                    .0
                    .store
                    .set_run_process(id, pid, start_time(pid).unwrap_or(0), 0, 0)
        {
            tracing::warn!(%error, "recording run {id}'s process failed");
        }
    }

    fn output(&self, label: &str, line: &str, _stderr: bool) {
        if let Some((_, file)) = self.file.lock().expect("run file poisoned").as_mut() {
            let time = muru_common::log::full(std::time::SystemTime::now());
            let _ = writeln!(file, "{time} [command {label}] {line}");
        }
    }

    fn event(&self, kind: &str, subject: &str, level: &'static str, message: &str) {
        self.runs.0.events.emit(kind, subject, level, message);
    }

    fn ended(&self, prepared: &Prepared, run: Option<u64>, ended: &CommandRun) {
        let Some(id) = run else {
            return;
        };

        if let Err(error) =
            self.runs
                .0
                .store
                .end_run(id, now_unix_ms(), ended.exit_code, ended.ok, &ended.message)
        {
            tracing::warn!(%error, "recording the end of run {id} failed");
        }

        self.runs.prune(&prepared.app, prepared.run_name());
    }
}
