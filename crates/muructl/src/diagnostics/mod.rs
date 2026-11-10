//! The diagnostic commands (design specification sections 11.9 and
//! 18.6): `muructl debug report`, a summary of an installation that is
//! safe to post with a bug report, and `muructl debug bundle`, an archive
//! of a whole system for the people who own it.
//!
//! What the commands collect is shared. This module reaches an
//! installation however it runs (a daemon, running or stopped, a
//! development run or `muructl serve`) and collects the facts from it,
//! as the types of `muru_controller::diagnostics`. A fact is read from
//! the daemon's files where they hold it, and a running daemon is asked
//! only for what it alone knows, each question with a time limit of its
//! own. A part that cannot be collected is noted with the reason, and
//! collection carries on.

pub mod bundle;
pub mod markdown;
pub mod redact;
pub mod report;
pub mod scrub;

use std::future::Future;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::time::Duration;

use muru_controller::client;
use muru_controller::diagnostics::stored::{self, Names};
use muru_controller::diagnostics::{NotCollected, ReleaseFacts, Setting, WorkerFacts, frameworks};
use muru_controller::locations::{Locations, Mode};
use muru_controller::store::Store;
use muru_proto::admin::v1 as pb;
use muru_proto::admin::v1::admin_client::AdminClient;
use tonic::transport::Channel;

use crate::admin::{DaemonFileArgs, SocketArgs};

/// How long one question of a running daemon or run may take. A part
/// that takes longer is noted as not collected.
const ASK: Duration = Duration::from_secs(5);

/// How many events a running daemon is asked for: more than it keeps.
const EVERY_EVENT: u32 = 100_000;

/// How Muru runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// `muructl run`.
    Run,
    /// `muructl serve`.
    Serve,
    /// A user's own daemon.
    UserDaemon,
    /// The system daemon.
    SystemDaemon,
}

impl Runs {
    /// How it is said.
    pub fn describe(self) -> &'static str {
        match self {
            Runs::Run => "muructl run",
            Runs::Serve => "muructl serve",
            Runs::UserDaemon => "a user daemon",
            Runs::SystemDaemon => "a system daemon",
        }
    }

    /// Whether it is a daemon, which has a state store and files of its
    /// own, as a development run and `muructl serve` have not.
    pub fn is_daemon(self) -> bool {
        matches!(self, Runs::UserDaemon | Runs::SystemDaemon)
    }
}

/// What answers on an admin socket.
struct Live {
    client: AdminClient<Channel>,
    status: pb::Status,
}

/// An installation as the diagnostics reach it: what answers on its
/// admin socket, if anything does, and a daemon's files.
pub struct Installation {
    runs: Runs,
    live: Option<Live>,
    /// A daemon's locations; a development run and `muructl serve` keep
    /// no files.
    locations: Option<Locations>,
    /// A daemon's state store, opened read-only, or why it could not be.
    store: Result<Store, String>,
    /// What could not be collected so far.
    notes: Vec<NotCollected>,
}

/// One question of a running daemon within the time a part is given, as
/// its answer or why there is none.
async fn within<T>(
    asked: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<T, String> {
    match tokio::time::timeout(ASK, asked).await {
        Ok(Ok(response)) => Ok(response.into_inner()),
        Ok(Err(status)) => Err(status.message().to_owned()),
        Err(_) => Err(format!("no answer within {} seconds", ASK.as_secs())),
    }
}

impl Installation {
    /// Reach the installation the options name: whatever answers on the
    /// admin socket, else the files of a daemon that is not running.
    pub async fn reach(socket: &SocketArgs, files: &DaemonFileArgs) -> anyhow::Result<Self> {
        let path = socket.socket()?;
        let mut locations = crate::admin::daemon_locations(socket, files);

        let answered = tokio::time::timeout(ASK, async {
            let mut client = client::connect(&path).await?;
            let status = client
                .get_status(pb::GetStatusRequest {})
                .await?
                .into_inner();
            anyhow::Ok(Live { client, status })
        })
        .await;

        // A socket nothing listens on is a daemon that is not running.
        // One that is there and gives no answer is said to be so, and the
        // daemon's files are read as for a stopped one.

        let absent = !path.exists() || client::left_behind(&path);
        let mut notes = Vec::new();

        let live = match answered {
            Ok(Ok(live)) => Some(live),
            Ok(Err(_)) | Err(_) if absent => None,
            Ok(Err(error)) => {
                notes.push(NotCollected::new("live state", format!("{error:#}")));
                None
            }
            Err(_) => {
                notes.push(NotCollected::new(
                    "live state",
                    format!(
                        "what listens at {} gave no answer within {} seconds",
                        path.display(),
                        ASK.as_secs()
                    ),
                ));
                None
            }
        };

        // A development run and muructl serve say what they are; muructl
        // serve leaves the file that names it beside its socket.

        let runs = match &live {
            Some(live) if live.status.kind == "run" => {
                if path.with_file_name("serve.json").is_file() {
                    Runs::Serve
                } else {
                    Runs::Run
                }
            }
            Some(_) if std::fs::metadata(&path).is_ok_and(|socket| socket.uid() == 0) => {
                Runs::SystemDaemon
            }
            Some(_) => Runs::UserDaemon,
            None if locations.mode == Mode::System => Runs::SystemDaemon,
            None => Runs::UserDaemon,
        };

        if !runs.is_daemon() {
            return Ok(Self {
                runs,
                live,
                locations: None,
                store: Err(format!("{} keeps no state store", runs.describe())),
                notes,
            });
        }

        // A running daemon says where its state is, which an option to
        // this command may not match.

        let mut live = live;

        if let Some(live) = &mut live {
            if !live.status.state_dir.is_empty() && files.state_dir().is_none() {
                locations.state.path = PathBuf::from(&live.status.state_dir);
            }

            if files.log_dir().is_none()
                && let Ok(logs) = within(live.client.list_logs(pb::ListLogsRequest {})).await
            {
                locations.logs.path = PathBuf::from(logs.directory);
            }
        }

        let store_path = locations.state_store();

        let store = if store_path.is_file() {
            Store::open_read_only(&store_path)
                .map_err(|error| format!("{} could not be read: {error}", store_path.display()))
        } else {
            Err(format!(
                "there is no state store at {}",
                store_path.display()
            ))
        };

        Ok(Self {
            runs,
            live,
            locations: Some(locations),
            store,
            notes,
        })
    }

    /// How it runs.
    pub fn runs(&self) -> Runs {
        self.runs
    }

    /// Whether the daemon's state store can be read, so that what it
    /// holds need not be asked of the daemon.
    pub fn reads_store(&self) -> bool {
        self.store.is_ok()
    }

    /// Whether something answered on the admin socket.
    pub fn running(&self) -> bool {
        self.live.is_some()
    }

    /// What answered on the admin socket said of itself.
    pub fn status(&self) -> Option<&pb::Status> {
        self.live.as_ref().map(|live| &live.status)
    }

    /// A daemon's locations, or none for a development run and `muructl
    /// serve`.
    pub fn locations(&self) -> Option<&Locations> {
        self.locations.as_ref()
    }

    /// Note a part that could not be collected.
    pub fn note(&mut self, part: &str, why: impl std::fmt::Display) {
        self.notes.push(NotCollected::new(part, why));
    }

    /// What could not be collected, taken from the installation.
    pub fn take_notes(&mut self) -> Vec<NotCollected> {
        std::mem::take(&mut self.notes)
    }

    /// Why nothing can be collected from a daemon that is not running
    /// and has left no state store.
    fn unreachable(&self) -> String {
        match &self.store {
            Err(why) if self.live.is_none() => format!("no daemon is running, and {why}"),
            Err(why) => why.clone(),
            Ok(_) => "no daemon is running".to_owned(),
        }
    }

    /// The releases, with the states of their workers where a host runs
    /// and the frameworks a worker of each serving one has loaded.
    pub async fn releases(&mut self) -> Result<Vec<ReleaseFacts>, String> {
        if !self.runs.is_daemon() {
            return self.release_of_run().await;
        }

        let answered = match &mut self.live {
            Some(live) => Some(
                within(live.client.list_releases(pb::ListReleasesRequest {
                    app: String::new(),
                    disk: false,
                }))
                .await,
            ),
            None => None,
        };

        let mut releases: Vec<ReleaseFacts> = match answered {
            Some(Ok(answer)) => answer
                .releases
                .iter()
                .map(ReleaseFacts::from_message)
                .collect(),
            Some(Err(why)) => {
                self.note(
                    "worker states",
                    format!("the daemon was asked for its releases: {why}"),
                );
                self.stored_releases()?
            }
            None => self.stored_releases()?,
        };

        for release in &mut releases {
            if release.serving && !release.workers.is_empty() {
                let worker = release.workers[0].index;

                match self
                    .inspect(&release.app.clone(), &release.id.clone(), worker)
                    .await
                {
                    Ok(inspected) => release.frameworks = frameworks(&inspected["modules"]),
                    Err(why) => self.note(
                        "frameworks",
                        format!("worker {worker} of {} was asked: {why}", release.name()),
                    ),
                }
            }
        }

        Ok(releases)
    }

    /// The releases the installation has, each as its application and
    /// its id, without anything being asked of a worker.
    pub async fn releases_known(&mut self) -> Result<Vec<(String, String)>, String> {
        let named = |releases: Vec<ReleaseFacts>| {
            releases
                .into_iter()
                .map(|release| (release.app, release.id))
                .collect()
        };

        if !self.runs.is_daemon() {
            return match self.status() {
                Some(status) => Ok(vec![(status.app.clone(), status.release.clone())]),
                None => Err(self.unreachable()),
            };
        }

        if let Ok(releases) = self.stored_releases() {
            return Ok(named(releases));
        }

        let Some(live) = &mut self.live else {
            return Err(self.unreachable());
        };

        within(live.client.list_releases(pb::ListReleasesRequest {
            app: String::new(),
            disk: false,
        }))
        .await
        .map(|answer| {
            answer
                .releases
                .into_iter()
                .map(|release| (release.app, release.id))
                .collect()
        })
    }

    /// The releases as the state store records them.
    fn stored_releases(&self) -> Result<Vec<ReleaseFacts>, String> {
        let store = self.store.as_ref().map_err(|_| self.unreachable())?;
        stored::releases(store).map_err(|error| error.to_string())
    }

    /// The one release of a development run or `muructl serve`, from its
    /// host and one of its workers.
    async fn release_of_run(&mut self) -> Result<Vec<ReleaseFacts>, String> {
        let Some(status) = self.status().cloned() else {
            return Err(self.unreachable());
        };

        let mut release = ReleaseFacts {
            app: status.app.clone(),
            id: status.release.clone(),
            state: "starting".to_owned(),
            message: String::new(),
            serving: true,
            kind: "workers".to_owned(),
            interface: String::new(),
            detected_interface: String::new(),
            frameworks: Vec::new(),
            python_version: String::new(),
            python_build: String::new(),
            workers: Vec::new(),
        };

        match self
            .debug(&status.app, &status.release, "status", None)
            .await
        {
            Ok(answer) => {
                release.workers = answer
                    .workers
                    .iter()
                    .filter_map(|worker| {
                        let result: serde_json::Value =
                            serde_json::from_str(&worker.result_json).ok()?;

                        Some(WorkerFacts {
                            index: worker.index,
                            group: result["group"].as_str().unwrap_or_default().to_owned(),
                            state: result["state"].as_str().unwrap_or_default().to_owned(),
                        })
                    })
                    .collect();
            }
            Err(why) => self.note("worker states", format!("the host was asked: {why}")),
        }

        if release.workers.iter().any(|worker| worker.state == "ready") {
            release.state = "ready".to_owned();
        }

        let Some(worker) = release.workers.first().map(|worker| worker.index) else {
            return Ok(vec![release]);
        };

        match self.inspect(&status.app, &status.release, worker).await {
            Ok(inspected) => {
                let settings = &inspected["settings"];
                release.frameworks = frameworks(&inspected["modules"]);
                release.python_version =
                    inspected["python"].as_str().unwrap_or_default().to_owned();
                release.detected_interface = settings["interface"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();

                release.python_build = match settings["gil_enabled"].as_bool() {
                    Some(true) => "regular".to_owned(),
                    Some(false) => "free-threaded".to_owned(),
                    None => String::new(),
                };
            }
            Err(why) => self.note(
                "interpreter and frameworks",
                format!("worker {worker} was asked: {why}"),
            ),
        }

        Ok(vec![release])
    }

    /// One debug operation of a release's workers, within the time a
    /// part is given.
    async fn debug(
        &mut self,
        app: &str,
        release: &str,
        op: &str,
        worker: Option<u32>,
    ) -> Result<pb::DebugResponse, String> {
        let Some(live) = &mut self.live else {
            return Err("nothing is running".to_owned());
        };

        within(live.client.debug(pb::DebugRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            op: op.to_owned(),
            worker,
            args_json: "{}".to_owned(),
        }))
        .await
    }

    /// What `debug inspect` says of one worker.
    async fn inspect(
        &mut self,
        app: &str,
        release: &str,
        worker: u32,
    ) -> Result<serde_json::Value, String> {
        let answer = self.debug(app, release, "inspect", Some(worker)).await?;
        let Some(answer) = answer.workers.first() else {
            return Err("no worker answered".to_owned());
        };

        if !answer.ok {
            return Err(answer.error.clone());
        }

        serde_json::from_str(&answer.result_json).map_err(|error| error.to_string())
    }

    /// The effective settings of the releases named, each with where it
    /// came from, as `muructl config show --effective` reports them.
    pub async fn settings(&mut self, releases: &[ReleaseFacts]) -> Result<Vec<Setting>, String> {
        let read = self.store.as_ref().ok().map(|store| {
            let mut settings = Vec::new();

            for release in releases {
                settings.extend(stored::settings(store, &release.app, &release.id)?);
            }

            Ok::<_, muru_controller::store::StoreError>(settings)
        });

        match read {
            Some(Ok(settings)) => return Ok(settings),
            Some(Err(error)) if self.live.is_none() => return Err(error.to_string()),
            _ => {}
        }

        // Without the store, what answers on the socket is asked: a
        // daemon whose store this user may not read, a run or a serve.

        let Some(live) = &mut self.live else {
            return Err(self.unreachable());
        };

        let answer = within(live.client.export_config(pb::ExportConfigRequest {
            app: String::new(),
            effective: true,
            resolve_rollouts: false,
        }))
        .await?;
        let wanted: Vec<String> = releases.iter().map(ReleaseFacts::name).collect();

        Ok(answer
            .layers
            .iter()
            .filter_map(|line| Setting::parse(line))
            .filter(|setting| wanted.iter().any(|name| name == setting.release()))
            .collect())
    }

    /// Every event kept, oldest first.
    pub async fn events(&mut self) -> Result<Vec<pb::Event>, String> {
        match self.store.as_ref().ok().map(stored::events) {
            Some(Ok(events)) => return Ok(events),
            Some(Err(error)) if self.live.is_none() => return Err(error.to_string()),
            _ => {}
        }

        let Some(live) = &mut self.live else {
            return Err(self.unreachable());
        };

        within(live.client.list_events(pb::ListEventsRequest {
            after: 0,
            limit: EVERY_EVENT,
        }))
        .await
        .map(|answer| answer.events)
    }

    /// The names that identify the installation: its applications,
    /// virtual hosts and the users its applications run as. Whatever
    /// cannot be asked for is left out.
    pub async fn names(&mut self) -> Names {
        if let Ok(store) = &self.store
            && let Ok(names) = stored::names(store)
        {
            return names;
        }

        let Some(live) = &mut self.live else {
            return Names::default();
        };

        let mut names = Names::default();

        if !live.status.app.is_empty() {
            names.apps.push(live.status.app.clone());
        }

        if let Ok(answer) = within(live.client.list_apps(pb::ListAppsRequest {})).await {
            for app in answer.apps {
                names.apps.push(app.name);
                names.users.extend([app.user, app.group]);
            }
        }

        if let Ok(answer) = within(live.client.list_hosts(pb::ListHostsRequest {})).await {
            for host in answer.hosts {
                names.hosts.push(host.name);
                names.hosts.extend(host.names);
                names.hosts.push(host.canonical);
            }
        }

        names.users.retain(|name| !name.is_empty());
        names.hosts.retain(|name| !name.is_empty());
        names
    }

    /// The daemon's own configuration file as a table, or none when
    /// there is no file: where the options and the environment of this
    /// command put it, which is where the daemon reads it when it was
    /// started with the same.
    pub fn daemon_file(&self) -> Result<Option<(PathBuf, toml::Table)>, String> {
        let Some(locations) = &self.locations else {
            return Ok(None);
        };

        let path = &locations.config.path;

        if !path.is_file() {
            return Ok(None);
        }

        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
        let table = text
            .parse::<toml::Table>()
            .map_err(|error| format!("{} could not be parsed: {error}", path.display()))?;
        Ok(Some((path.clone(), table)))
    }
}

/// The path to a user's home directory, when the environment says.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The name of the user running the command, when the environment says.
pub fn user() -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|name| !name.is_empty())
}
